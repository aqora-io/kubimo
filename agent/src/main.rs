//! kubimo node agent.
//!
//! Runs as a privileged DaemonSet on every node that carries a kubimo data
//! volume. It owns the slot lifecycle on that volume: create a slot, give it a
//! project quota, hand it to a runner pod as a bind mount, and reclaim it when
//! the workspace is done with it.

mod csi;
mod mount;
mod quota;
mod slot;
mod store;

use std::path::PathBuf;

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
        /// Serve slots even when the data volume has no project-quota
        /// enforcement. For development on ext4 only: such slots have **no**
        /// capacity limit, so one workspace can fill the volume and break every
        /// other tenant on the node.
        #[arg(long, env = "KUBIMO_AGENT_ALLOW_UNQUOTAED_SLOTS")]
        allow_unquotaed_slots: bool,
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
            limit_bytes,
        } => create_slot(&args.data_root, &workspace, limit_bytes),
        Command::Serve {
            socket,
            node_name,
            default_limit_bytes,
            allow_unquotaed_slots,
        } => serve(
            &args.data_root,
            &socket,
            node_name,
            default_limit_bytes,
            allow_unquotaed_slots,
        ),
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
    limit_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
    let resolved = store.resolve_or_create(workspace)?;
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

fn serve(
    data_root: &std::path::Path,
    socket: &std::path::Path,
    node_name: String,
    default_limit_bytes: u64,
    allow_unquotaed_slots: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SlotStore::new(SlotLayout::new(data_root));
    let node = csi::KubimoNode::new(node_name, store, default_limit_bytes, allow_unquotaed_slots);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            csi::serve(socket, node, async {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("shutting down");
            })
            .await
        })
}
