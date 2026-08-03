//! kubimo node agent.
//!
//! Runs as a privileged DaemonSet on every node that carries a kubimo data
//! volume. It owns the slot lifecycle on that volume: create a slot, give it a
//! project quota, hand it to a runner pod as a bind mount, and reclaim it when
//! the workspace is done with it.

mod clients;
mod csi;
mod drain;
mod hydrate;
mod kernel;
mod mount;
mod quota;
mod reaper;
mod slot;
mod store;
mod sweep;
mod venv;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use csi::{SLOT_GID, SLOT_UID};
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
        /// Where kubelet publishes per-pod volumes, as this container sees it.
        ///
        /// Must be the same path it has on the host: `NodePublishVolume` hands us
        /// kubelet's `target_path`, which is a host path, and we operate on it
        /// directly. The stale-mount sweep reconstructs those same paths.
        #[arg(
            long,
            env = "KUBIMO_AGENT_KUBELET_PODS_DIR",
            default_value = sweep::DEFAULT_KUBELET_PODS_DIR,
        )]
        kubelet_pods_dir: PathBuf,
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
    /// Delete the pods holding this node's slots, then wait for kubelet to unpublish
    /// them. Invoked from the DaemonSet's `preStop` hook.
    ///
    /// Runs *before* the agent stops serving, which is the whole point: kubelet's
    /// `NodeUnpublishVolume` is what flushes a slot to S3 and unmounts it cleanly, and
    /// it can only reach an agent that is still alive. Without this, replacing the agent
    /// leaves every runner on a dead mount with its newest work unflushed.
    Drain {
        /// Name of the node this agent runs on.
        #[arg(long, env = "KUBE_NODE_NAME")]
        node_name: String,
        /// Give up after this long. Must stay below the pod's
        /// `terminationGracePeriodSeconds`, or the drain is cut off mid-flush — leaving
        /// some slots flushed and some not, which is worse than not draining at all.
        #[arg(long, env = "KUBIMO_AGENT_DRAIN_TIMEOUT_SECS", default_value_t = 120)]
        timeout_secs: u64,
        /// How often to re-check whether every volume has been unpublished.
        #[arg(long, default_value_t = 2)]
        poll_interval_secs: u64,
        /// Grace period given to each deleted pod. Nested inside our own budget, so it
        /// must be comfortably smaller. marimo has nothing to persist on SIGTERM — the
        /// flush is the agent's job — so this only needs to cover process teardown.
        #[arg(
            long,
            env = "KUBIMO_AGENT_DRAIN_RUNNER_GRACE_SECS",
            default_value_t = 10
        )]
        runner_grace_secs: u64,
    },
}

fn main() {
    let args = Args::parse();
    init_tracing(matches!(args.command, Command::Drain { .. }));
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
            kubelet_pods_dir,
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
                kubelet_pods_dir,
                node_name,
                default_limit_bytes,
                allow_unquotaed_slots,
                Duration::from_secs(idle_slot_ttl_secs),
            )
        }),
        Command::Drain {
            node_name,
            timeout_secs,
            poll_interval_secs,
            runner_grace_secs,
        } => drain_node(
            &args.data_root,
            &node_name,
            Duration::from_secs(timeout_secs),
            Duration::from_secs(poll_interval_secs),
            runner_grace_secs,
        ),
    };
    if let Err(err) = result {
        tracing::error!("{err}");
        std::process::exit(1);
    }
}

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

/// Container stdout, as seen from a process that is not PID 1.
const CONTAINER_STDOUT: &str = "/proc/1/fd/1";

/// Set up logging, optionally redirecting to the container's own stdout.
///
/// kubelet does **not** capture a `preStop` hook's output, so the drain — which runs as
/// a second process in this container — would otherwise do all of its work invisibly:
/// nothing in `kubectl logs` would say which pods it deleted or whether it finished.
/// That is exactly the wrong thing to be missing while diagnosing an upgrade.
///
/// The image's ENTRYPOINT is the agent, so PID 1 is the serving process and
/// `/proc/1/fd/1` is the container's log stream. Writing there puts the drain's lines
/// where an operator already looks, interleaved with the unpublish and flush lines it
/// is causing. Falls back to stderr when the path is unavailable — running outside a
/// container, or PID 1 not being ours.
fn init_tracing(to_container_stdout: bool) {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if to_container_stdout
        && std::fs::OpenOptions::new()
            .write(true)
            .open(CONTAINER_STDOUT)
            .is_ok()
    {
        builder
            .with_writer(|| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(CONTAINER_STDOUT)
                    .map(|file| Box::new(file) as Box<dyn std::io::Write>)
                    .unwrap_or_else(|_| Box::new(std::io::stderr()))
            })
            .init();
    } else {
        builder.init();
    }
}

/// Resolves when the process is asked to stop.
///
/// SIGTERM as well as SIGINT: kubelet sends **SIGTERM**, so waiting only on `ctrl_c()`
/// means the agent never shuts down gracefully and is always SIGKILLed at the end of the
/// termination grace period. That was survivable while the grace period was the 30s
/// default; with the drain's longer budget it would stall every agent replacement for
/// the full period, long after the drain had finished its work.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(err) => {
                tracing::error!(%err, "cannot listen for SIGTERM; falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `preStop` entry point; see [`drain`].
///
/// Exits 0 on a clean drain *and* on timeout — both are expected outcomes that the logs
/// distinguish. A non-zero exit is reserved for genuine misconfiguration (no cluster
/// access, RBAC denied), so a `FailedPreStopHook` event means something an operator has
/// to fix rather than something that merely took too long.
fn drain_node(
    data_root: &std::path::Path,
    node_name: &str,
    timeout: Duration,
    poll: Duration,
    runner_grace_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(drain::run(
            &store,
            node_name,
            timeout,
            poll,
            runner_grace_secs,
        ))?;
    Ok(())
}

fn create_slot(
    data_root: &std::path::Path,
    workspace: &str,
    namespace: &str,
    limit_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
    let resolved = store.resolve_or_create(namespace, workspace)?;
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
    kubelet_pods_dir: PathBuf,
    node_name: String,
    default_limit_bytes: u64,
    allow_unquotaed_slots: bool,
    idle_slot_ttl: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
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
            let sweep_node_name = node_name.clone();
            // Slots outlive the runners that used them — that is what makes
            // reopening a workspace instant — so nothing on the unpublish path
            // deletes one. Without this sweep they would accumulate on the node
            // volume for every workspace ever opened here, including deleted
            // ones. Needs cluster access to tell a deleted workspace from a
            // merely idle one, so it only runs when we have a client.
            if client.is_some() {
                tokio::spawn(reaper::run(
                    // A clone, not a fresh store: the reaper and the CSI node
                    // must share one per-workspace lock map, or the reaper could
                    // reclaim a slot in the middle of a publish creating it.
                    store.clone(),
                    clients::NamespacedClients::new(true),
                    idle_slot_ttl,
                    client.clone().map(|client| reaper::StaleMountSweep {
                        client,
                        node_name: sweep_node_name,
                        pods_dir: kubelet_pods_dir,
                    }),
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
                shutdown_signal().await;
                tracing::info!("shutting down");
            })
            .await
        })
}
