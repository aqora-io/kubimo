//! kubimo node agent.
//!
//! Runs as a privileged DaemonSet on every node that carries a kubimo data
//! volume. It owns the slot lifecycle on that volume: create a slot, give it a
//! project quota, hand it to a runner pod, and wipe it when the workspace is
//! done with it.

mod quota;
mod slot;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Allocate a slot: create the directory, give it a project quota, and hand
    /// it to uid/gid 1000. Prints the slot id.
    CreateSlot {
        /// Hard capacity limit, in bytes.
        #[arg(long)]
        limit_bytes: u64,
        /// XFS project id to account this slot under.
        #[arg(long)]
        project_id: u32,
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
            limit_bytes,
            project_id,
        } => create_slot(&args.data_root, project_id, limit_bytes),
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

fn create_slot(
    data_root: &std::path::Path,
    project_id: u32,
    limit_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let layout = slot::SlotLayout::new(data_root);
    let id = slot::SlotId::generate();
    let dir = layout.slot_dir(&id);
    std::fs::create_dir_all(&dir)?;

    // Order matters: stamp the project id before anything is written, because
    // inodes created beforehand keep the old project and escape accounting.
    quota::assign_project(&dir, project_id)?;
    quota::set_project_limit(layout.root(), project_id, limit_bytes)?;

    // 0700: a slot is readable only by its own runner. The bind mount is the
    // real boundary, but defence in depth costs nothing here.
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    std::os::unix::fs::chown(&dir, Some(SLOT_UID), Some(SLOT_GID))?;

    tracing::info!(slot = %id, project_id, limit_bytes, path = %dir.display(), "created slot");
    println!("{id}");
    Ok(())
}

fn inspect(data_root: &std::path::Path) {
    let layout = slot::SlotLayout::new(data_root);
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
