//! kubimo node agent.
//!
//! Runs as a privileged DaemonSet on every node that carries a kubimo data
//! volume. It owns the slot lifecycle on that volume: create a slot, give it a
//! project quota, hand it to a runner pod as a bind mount, and reclaim it when
//! the workspace is done with it.

mod clients;
mod csi;
mod hydrate;
mod kernel;
mod mount;
mod quota;
mod reaper;
mod slot;
mod store;
mod venv;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use slot::SlotLayout;
use store::SlotStore;

/// Fallback slot quota when the volume definition carries no `limitBytes`.
const DEFAULT_LIMIT_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "kubimo-agent")]
struct Args {
    /// Mount point of the node data volume.
    #[arg(long, env = "KUBIMO_AGENT_DATA_ROOT", default_value = "/data")]
    data_root: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Report what the agent can see about the data volume. Read-only; used to
    /// validate a node before enabling slot allocation on it.
    Inspect,
    /// Allocate (or look up) the slot for a workspace and print its id.
    /// Mostly a debugging aid — kubelet drives this through `serve`.
    CreateSlot {
        #[arg(long)]
        workspace: String,
        /// Namespace the Workspace CR lives in.
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long, default_value_t = DEFAULT_LIMIT_BYTES)]
        limit_bytes: u64,
    },
    /// Serve the CSI node plugin.
    Serve {
        /// Unix socket kubelet connects to. The node-driver-registrar sidecar
        /// advertises this path.
        #[arg(long, env = "CSI_ENDPOINT", default_value = "/csi/csi.sock")]
        socket: PathBuf,
        /// Name of the node this agent runs on, reported by `NodeGetInfo`.
        #[arg(long, env = "KUBE_NODE_NAME")]
        node_name: String,
        #[arg(long, env = "KUBIMO_AGENT_DEFAULT_LIMIT_BYTES", default_value_t = DEFAULT_LIMIT_BYTES)]
        default_limit_bytes: u64,
        /// Minimum kernel release required to serve slots, e.g. "6.8.0-125".
        ///
        /// A shared node volume makes kernel filesystem bugs cross-tenant; see
        /// CVE-2026-64600. The patched version is distro-specific, so the
        /// operator supplies it rather than the agent guessing.
        #[arg(long, env = "KUBIMO_AGENT_MIN_KERNEL_VERSION")]
        min_kernel_version: Option<String>,
        /// Serve slots on a kernel below `--min-kernel-version`, or with no
        /// minimum configured at all.
        #[arg(long, env = "KUBIMO_AGENT_ALLOW_UNPATCHED_KERNEL")]
        allow_unpatched_kernel: bool,
        /// Serve slots even when the data volume has no project-quota
        /// enforcement. For development on ext4 only: such slots have **no**
        /// capacity limit, so one workspace can fill the volume and break every
        /// other tenant on the node.
        #[arg(long, env = "KUBIMO_AGENT_ALLOW_UNQUOTAED_SLOTS")]
        allow_unquotaed_slots: bool,
        /// How long an idle, flushed slot is kept before it is dropped.
        ///
        /// A slot outlives its runner so reopening a workspace is instant, but a
        /// workspace that goes idle and is then scheduled onto another node
        /// leaves this one behind forever — nothing else collects a slot whose
        /// workspace still exists. Eviction treats it as what it is: a cache
        /// whose contents are already in S3, costing one re-hydrate on next use.
        ///
        /// Only ever applied to slots this node has actually flushed, so a
        /// failed or skipped flush keeps the only copy on disk regardless of
        /// age. Set to 0 to disable eviction entirely.
        #[arg(
            long,
            env = "KUBIMO_AGENT_IDLE_SLOT_TTL_SECS",
            default_value_t = reaper::DEFAULT_IDLE_TTL.as_secs(),
        )]
        idle_slot_ttl_secs: u64,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let result = match args.command {
        Command::Inspect => {
            inspect(&args.data_root);
            Ok(())
        }
        Command::CreateSlot {
            workspace,
            namespace,
            limit_bytes,
        } => create_slot(&args.data_root, &workspace, &namespace, limit_bytes),
        Command::Serve {
            socket,
            node_name,
            default_limit_bytes,
            min_kernel_version,
            allow_unpatched_kernel,
            allow_unquotaed_slots,
            idle_slot_ttl_secs,
        } => check_kernel(min_kernel_version.as_deref(), allow_unpatched_kernel).and_then(|()| {
            serve(
                &args.data_root,
                &socket,
                node_name,
                default_limit_bytes,
                allow_unquotaed_slots,
                Duration::from_secs(idle_slot_ttl_secs),
            )
        }),
    };
    if let Err(err) = result {
        tracing::error!("{err}");
        std::process::exit(1);
    }
}

/// Owner of every slot's contents. Matches the `me` user baked into the marimo
/// image, so the runner can write without kubelet's `fsGroup` recursion — which
/// on a shared volume would chown every slot on the node at every pod start.
const SLOT_UID: u32 = 1000;
const SLOT_GID: u32 = 1000;

fn inspect(data_root: &std::path::Path) {
    let layout = SlotLayout::new(data_root);
    tracing::info!(root = %layout.root().display(), "data volume");
    match std::fs::read_dir(layout.slots_dir()) {
        Ok(entries) => {
            let slots: Vec<_> = entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    slot::SlotId::parse(entry.file_name().to_string_lossy().as_ref()).ok()
                })
                .collect();
            tracing::info!(count = slots.len(), "slots");
            for id in slots {
                tracing::info!(slot = %id, "found");
            }
        }
        Err(err) => tracing::warn!(%err, "no slots directory yet"),
    }
}

fn create_slot(
    data_root: &std::path::Path,
    workspace: &str,
    namespace: &str,
    limit_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
    let resolved = store.resolve_or_create(workspace, namespace)?;
    let dir = store.layout().slot_dir(&resolved.id);
    if resolved.created {
        // Order matters: stamp the project id before anything is written,
        // because inodes created beforehand keep the old project and escape
        // accounting.
        quota::assign_project(&dir, resolved.project_id)?;
        quota::set_project_limit(store.layout().root(), resolved.project_id, limit_bytes)?;
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        std::os::unix::fs::chown(&dir, Some(SLOT_UID), Some(SLOT_GID))?;
    }
    tracing::info!(
        slot = %resolved.id,
        project_id = resolved.project_id,
        created = resolved.created,
        path = %dir.display(),
        "slot ready"
    );
    println!("{}", resolved.id);
    Ok(())
}

/// Enforce the kernel floor before serving any slot.
fn check_kernel(
    minimum: Option<&str>,
    allow_unpatched: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match (minimum, allow_unpatched) {
        (Some(minimum), _) => match kernel::require_at_least(minimum) {
            Ok(current) => {
                tracing::info!(?current, minimum, "kernel meets the required minimum");
                Ok(())
            }
            Err(err) if allow_unpatched => {
                tracing::warn!("{err}");
                Ok(())
            }
            Err(err) => Err(err.into()),
        },
        // No floor configured. Warn rather than refuse — the right value is
        // distro-specific and we cannot know it — but make the gap visible
        // instead of letting it pass silently.
        (None, false) => {
            tracing::warn!(
                "no --min-kernel-version set: this node is not gated against kernel filesystem \
                 bugs, which are cross-tenant on a shared node volume (see CVE-2026-64600)"
            );
            Ok(())
        }
        (None, true) => Ok(()),
    }
}

fn serve(
    data_root: &std::path::Path,
    socket: &std::path::Path,
    node_name: String,
    default_limit_bytes: u64,
    allow_unquotaed_slots: bool,
    idle_slot_ttl: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
    let reaper_root = data_root.to_path_buf();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            // Optional: without cluster access the agent still hydrates and
            // mounts slots, it just cannot refresh WorkspaceDirectory CRs when
            // flushing. Degrading here rather than refusing to start keeps the
            // storage path working if RBAC is misconfigured.
            //
            // `kubimo-indexer`, not the inferred default: the agent writes the
            // same `status.storage` and `WorkspaceDirectory` fields the indexer
            // does, and in a pooled workspace the cache job still runs an
            // indexer container. Two managers apply-patching the same fields
            // conflict, and server-side apply reports that as a 409 we only
            // log. Sharing the indexer's identity makes the writes idempotent
            // instead. It must stay distinct from the controller's
            // `kubimo-controller`, which owns `status.mode` on the same object.
            let client = match kubimo::Client::builder()
                .name("kubimo-indexer")
                .build()
                .await
            {
                Ok(client) => Some(client),
                Err(err) => {
                    tracing::warn!(%err, "no Kubernetes access; slots will not be flushed to S3");
                    None
                }
            };
            // Slots outlive the runners that used them — that is what makes
            // reopening a workspace instant — so nothing on the unpublish path
            // deletes one. Without this sweep they would accumulate on the node
            // volume for every workspace ever opened here, including deleted
            // ones. Needs cluster access to tell a deleted workspace from a
            // merely idle one, so it only runs when we have a client.
            if client.is_some() {
                tokio::spawn(reaper::run(
                    SlotStore::new(SlotLayout::new(&reaper_root)),
                    clients::NamespacedClients::new(true),
                    idle_slot_ttl,
                ));
            } else {
                tracing::warn!(
                    "no Kubernetes access; slots for deleted workspaces will not be reclaimed"
                );
            }
            let node = csi::KubimoNode::new(
                node_name,
                store,
                default_limit_bytes,
                allow_unquotaed_slots,
                client,
            );
            csi::serve(socket, node, async {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("shutting down");
            })
            .await
        })
}
