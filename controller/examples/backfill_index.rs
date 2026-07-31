//! Index workspaces that have no S3 archive, one at a time, under supervision.
//!
//! An indexer pod normally exists only while a workspace has a live `Edit` runner
//! (`apply_indexer`), so a workspace nobody has opened since archives became load-bearing
//! has nothing in S3 — no content, no `manifest.json`, and no `status.storage`, which is
//! also what the controller needs to autoscale its disk. Production has ~99 of those.
//!
//! The alternative is a `CacheJob`, and it is the wrong tool here: it runs the cache
//! container as an *init* step, which `uv sync`s and then **imports every module and
//! executes every notebook** as arbitrary user code, with `backoffLimit` defaulting to 6.
//! This runs the indexer alone — one walk of the tree, uploads, done.
//!
//! Deliberately an operator tool rather than controller behaviour: it is a one-time
//! migration, it wants a human watching it, and making it a controller feature would mean
//! editing `apply_indexer` — the very function whose "delete the pod when there is no Edit
//! runner" branch is the reason this is needed — during an upgrade.
//!
//! ```sh
//! cargo run -p kubimo-controller --example backfill_index -- \
//!     --namespace platform --image ghcr.io/aqora-io/kubimo-marimo:0.2.0 --dry-run
//! ```
//!
//! **The image must contain the empty-walk guard.** Without it, pointing this at a
//! workspace whose PVC failed to mount deletes that workspace's entire archive. With it,
//! the pod exits 2 and this run aborts.

use std::collections::BTreeMap;
use std::time::Duration;

use clap::Parser;
use futures::TryStreamExt;
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, Pod, PodSpec, Volume,
    VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{FilterParams, Runner, RunnerCommand, Workspace, WorkspaceDir, prelude::*};
use kubimo_controller::controllers::{indexer, workspace_affinity};

/// How often to check a running backfill pod.
const POLL: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(about = "Index workspaces that have no S3 archive")]
struct Cli {
    /// Namespace holding the Workspace CRs.
    #[arg(long, short)]
    namespace: String,
    /// Marimo image to run the indexer from. No default on purpose: it must be one
    /// containing the empty-walk guard, and that is not a decision to make implicitly.
    #[arg(long)]
    image: String,
    /// Print each pod as JSON and exit without creating anything. `kubectl apply -f`
    /// accepts it directly, so a reviewed pod can be applied by hand.
    #[arg(long)]
    dry_run: bool,
    /// Index at most this many workspaces. Useful for a first supervised run.
    #[arg(long)]
    limit: Option<usize>,
    /// Only this workspace, by CR name.
    #[arg(long)]
    workspace: Option<String>,
    /// Give up on a pod that has not reached a terminal phase within this many seconds.
    /// A pod stuck in `ContainerCreating` usually means a leaked attachment elsewhere.
    #[arg(long, default_value_t = 600)]
    timeout_secs: u64,
}

/// Why a workspace is not eligible for a backfill pod.
enum Skip {
    /// No `spec.indexer`, so the indexer has nowhere to write. The platform's sweep
    /// patches this; it is not this tool's job to invent a bucket.
    NoIndexer,
    /// A metadata-only archive looks backed up and restores empty, which is worse than
    /// having none at all.
    NoUploadContent,
    /// No credentials and no `AWS_BUCKET`.
    NoCredentials,
    /// Already indexed.
    HasArchive,
    /// An Edit runner (and so a live indexer pod) is already watching this workspace.
    /// Two indexers on one workspace mint duplicate keys and thrash each other's CRs.
    LiveIndexer,
    /// A previous run left a pod behind. Resolve by hand rather than guess.
    StalePod,
    /// Pooled workspaces are synced by the agent, not by an indexer pod.
    NotDedicated,
}

impl Skip {
    fn reason(&self) -> &'static str {
        match self {
            Skip::NoIndexer => "no spec.indexer (needs the platform sweep first)",
            Skip::NoUploadContent => "spec.indexer.uploadContent is not true",
            Skip::NoCredentials => "spec.indexer.pod.envFrom is empty",
            Skip::HasArchive => "already has WorkspaceDirectory CRs",
            Skip::LiveIndexer => "an Edit runner is already indexing it",
            Skip::StalePod => "a backfill pod from a previous run still exists",
            Skip::NotDedicated => "not a Dedicated workspace",
        }
    }
}

fn backfill_pod_name(workspace_name: &str) -> String {
    // Deliberately not `indexer::pod_name`, which yields `<ws>-indexer`. The workspace
    // reconciler deletes exactly that name whenever a workspace has no Edit runner —
    // which is true of every workspace being backfilled — so a pod under that name would
    // race the controller for its own life.
    format!("{workspace_name}-backfill")
}

fn build_pod(workspace: &Workspace, image: &str) -> Result<Pod, kubimo::Error> {
    let workspace_name = workspace.name()?;
    let pod_name = backfill_pod_name(workspace_name);
    let mut labels: BTreeMap<String, String> =
        workspace_affinity::workspace_label_map(workspace_name);
    // One label to find every pod this tool created, so an interrupted run is
    // `kubectl delete pod -l kubimo.aqora.io/backfill=true` away from clean.
    labels.insert("kubimo.aqora.io/backfill".to_string(), "true".to_string());
    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name),
            namespace: workspace.metadata.namespace.clone(),
            labels: Some(labels),
            // No owner reference on purpose: the workspace controller `.owns(pods)`, so
            // an owned pod would trigger a full Workspace reconcile on every phase
            // change — ~99 workspaces' worth of churn during an upgrade. This tool owns
            // the pod's lifetime, and the label covers the case where it dies first.
            ..Default::default()
        },
        spec: Some(PodSpec {
            service_account_name: Some(indexer::service_account_name(workspace_name)),
            // Pins onto the node already holding the volume when a runner has it, and
            // schedules freely when none does (a pod satisfying its own required
            // affinity is exempt) — the same path the workspace init Job takes.
            affinity: Some(workspace_affinity::workspace_affinity(workspace_name)),
            restart_policy: Some("Never".to_string()),
            containers: vec![Container {
                name: "indexer".to_string(),
                image: Some(image.to_string()),
                command: Some(vec!["/app/indexer".to_string()]),
                // The controller's own argument builder, so bucket, key prefix and
                // `--upload-content` cannot drift from what the live indexer would use.
                // `watch = false`: one pass, then exit.
                args: Some(indexer::upload_args(workspace, false)?),
                env: indexer::env(workspace),
                env_from: indexer::env_from(workspace),
                volume_mounts: Some(vec![VolumeMount {
                    mount_path: indexer::MOUNT_DIR.to_string(),
                    name: workspace_name.to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: workspace_name.to_string(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: workspace_name.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// What happened to one workspace.
enum Outcome {
    Indexed {
        dirs: usize,
    },
    /// The indexer refused to overwrite an archive because the walk found nothing. This
    /// means the PVC is empty or unmounted while an archive exists, which invalidates the
    /// assumption behind the whole run.
    Refused,
    Failed {
        exit_code: i32,
    },
    TimedOut,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    // The kube client speaks TLS, and rustls needs a provider chosen explicitly when more
    // than one backend is available. `main.rs` does the same.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Could not install default crypto provider");
    let client = kubimo::Client::builder()
        .name("kubimo-backfill")
        .namespace(&cli.namespace)
        .build()
        .await?;

    let workspaces: Vec<Workspace> = client
        .api::<Workspace>()
        .list(&FilterParams::default())
        .map_ok(|item| item.item)
        .try_collect()
        .await?;
    let runners: Vec<Runner> = client
        .api::<Runner>()
        .list(&FilterParams::default())
        .map_ok(|item| item.item)
        .try_collect()
        .await?;
    let pods = client.api_namespaced::<Pod>(&cli.namespace);

    let mut eligible = Vec::new();
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    for workspace in &workspaces {
        let Ok(name) = workspace.name() else { continue };
        if let Some(only) = cli.workspace.as_deref()
            && only != name
        {
            continue;
        }
        match classify(&client, workspace, name, &runners, &pods).await? {
            Some(skip) => *skipped.entry(skip.reason()).or_default() += 1,
            None => eligible.push(workspace),
        }
    }

    println!("{} workspace CRs in {}", workspaces.len(), cli.namespace);
    for (reason, count) in &skipped {
        println!("  skipped {count:>4}  {reason}");
    }
    println!("  eligible {:>3}", eligible.len());

    if let Some(limit) = cli.limit
        && eligible.len() > limit
    {
        // Say what was dropped. A truncated run that prints only its successes reads as
        // "everything is done".
        println!(
            "  limiting to {limit} of {} eligible; {} left for a later run",
            eligible.len(),
            eligible.len() - limit
        );
        eligible.truncate(limit);
    }

    if cli.dry_run {
        for workspace in &eligible {
            println!(
                "{}",
                serde_json::to_string_pretty(&build_pod(workspace, &cli.image)?)?
            );
        }
        println!("\ndry run: nothing was created");
        return Ok(());
    }

    // Strictly one at a time. Each pod attaches a volume, Scaleway caps attachments per
    // node, and the point of this tool is to be watchable — a wall of concurrent pods
    // makes a single refusal easy to miss.
    let mut indexed = 0usize;
    let mut failed = 0usize;
    for (position, workspace) in eligible.iter().enumerate() {
        let name = workspace.name()?;
        println!("[{}/{}] {name} …", position + 1, eligible.len());
        let outcome = run_one(&client, &pods, workspace, &cli).await?;
        match outcome {
            Outcome::Indexed { dirs } => {
                indexed += 1;
                println!("         indexed, {dirs} directory CRs");
            }
            Outcome::Refused => {
                // Stop the whole run. The guard firing means a PVC is empty while an
                // archive exists, so the assumption about this population is wrong and
                // the next workspace must not be touched.
                eprintln!(
                    "         REFUSED: the walk found nothing but an archive exists.\n\
                     \n\
                     Its volume is probably empty or failed to mount. Nothing was deleted —\n\
                     that is the guard working. Investigate this workspace before re-running;\n\
                     aborting so the rest are left alone."
                );
                return Err(format!("backfill refused for {name}").into());
            }
            Outcome::Failed { exit_code } => {
                failed += 1;
                eprintln!("         FAILED with exit code {exit_code}");
            }
            Outcome::TimedOut => {
                failed += 1;
                eprintln!(
                    "         TIMED OUT — a pod stuck pending usually means the volume is \
                     attached on another node"
                );
            }
        }
    }

    println!(
        "\nindexed {indexed}, failed {failed}, of {} attempted",
        eligible.len()
    );
    if failed > 0 {
        return Err(format!("{failed} workspace(s) failed").into());
    }
    Ok(())
}

/// Whether this workspace should get a backfill pod, or why not.
async fn classify(
    client: &kubimo::Client,
    workspace: &Workspace,
    name: &str,
    runners: &[Runner],
    pods: &kubimo::Api<Pod>,
) -> Result<Option<Skip>, Box<dyn std::error::Error>> {
    // status.mode wins over spec.mode (effective_mode): a workspace created after the
    // operator's default flipped to Pooled has spec.mode == None but status.mode ==
    // Some(Pooled), and spec-only would misclassify it as Dedicated. Dedicated is the
    // right default here because a workspace with neither spec nor status mode predates
    // pooled mode entirely.
    if workspace.effective_mode(kubimo::WorkspaceMode::Dedicated)
        != kubimo::WorkspaceMode::Dedicated
    {
        return Ok(Some(Skip::NotDedicated));
    }
    let Some(spec) = workspace.spec.indexer.as_ref() else {
        return Ok(Some(Skip::NoIndexer));
    };
    if spec.upload_content != Some(true) {
        return Ok(Some(Skip::NoUploadContent));
    }
    if spec
        .pod
        .as_ref()
        .and_then(|pod| pod.env_from.as_ref())
        .is_none_or(|env_from| env_from.is_empty())
    {
        return Ok(Some(Skip::NoCredentials));
    }
    let has_edit_runner = runners.iter().any(|runner| {
        runner.spec.workspace == name && matches!(runner.spec.command, RunnerCommand::Edit)
    });
    if has_edit_runner || pods.get_opt(&indexer::pod_name(name)).await?.is_some() {
        return Ok(Some(Skip::LiveIndexer));
    }
    if pods.get_opt(&backfill_pod_name(name)).await?.is_some() {
        return Ok(Some(Skip::StalePod));
    }
    if count_dirs(client, name).await? > 0 {
        return Ok(Some(Skip::HasArchive));
    }
    Ok(None)
}

async fn count_dirs(
    client: &kubimo::Client,
    workspace_name: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    let dirs: Vec<WorkspaceDir> = client
        .api::<WorkspaceDir>()
        .list(&FilterParams::default().with_fields(vec![
            kubimo::Expr::new(kubimo::WorkspaceDirField::Workspace).eq(workspace_name),
        ]))
        .map_ok(|item| item.item)
        .try_collect()
        .await?;
    Ok(dirs.len())
}

/// Create one backfill pod, wait for it, read its verdict, then delete it.
async fn run_one(
    client: &kubimo::Client,
    pods: &kubimo::Api<Pod>,
    workspace: &Workspace,
    cli: &Cli,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let name = workspace.name()?;
    let pod_name = backfill_pod_name(name);

    // Refuse rather than proceed if the claim is not bound: a pod that cannot mount is
    // the exact situation the empty-walk guard exists for, and it is cheaper to catch
    // here than to wait out the timeout.
    let pvc = client
        .api_namespaced::<PersistentVolumeClaim>(&cli.namespace)
        .get_opt(name)
        .await?;
    let bound = pvc
        .as_ref()
        .and_then(|pvc| pvc.status.as_ref())
        .and_then(|status| status.phase.as_deref())
        == Some("Bound");
    if !bound {
        eprintln!("         skipping: PVC {name} is not Bound");
        return Ok(Outcome::Failed { exit_code: -1 });
    }

    pods.patch(&build_pod(workspace, &cli.image)?).await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(cli.timeout_secs);
    let outcome = loop {
        if std::time::Instant::now() > deadline {
            break Outcome::TimedOut;
        }
        tokio::time::sleep(POLL).await;
        let Some(pod) = pods.get_opt(&pod_name).await? else {
            break Outcome::Failed { exit_code: -1 };
        };
        let phase = pod
            .status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            .unwrap_or("Unknown");
        match phase {
            "Succeeded" => {
                break Outcome::Indexed {
                    dirs: count_dirs(client, name).await?,
                };
            }
            "Failed" => {
                let exit_code = pod
                    .status
                    .as_ref()
                    .and_then(|status| status.container_statuses.as_ref())
                    .and_then(|statuses| statuses.first())
                    .and_then(|status| status.state.as_ref())
                    .and_then(|state| state.terminated.as_ref())
                    .map(|terminated| terminated.exit_code)
                    .unwrap_or(-1);
                // 2 is the indexer's "I refused to destroy an archive" exit.
                break if exit_code == 2 {
                    Outcome::Refused
                } else {
                    Outcome::Failed { exit_code }
                };
            }
            _ => {}
        }
    };

    // Delete promptly rather than leaving a Succeeded pod to hold its attachment until
    // the attach-detach controller notices.
    if let Err(err) = pods.delete(&pod_name).await {
        eprintln!("         warning: could not delete pod {pod_name}: {err}");
    }
    Ok(outcome)
}
