//! Persistent slot bookkeeping on the data volume.
//!
//! Two things must survive an agent restart: which slot belongs to which
//! workspace, and which XFS project ids are taken. Both live under
//! `<root>/.index`, which is never bind-mounted into a pod.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::slot::{SlotId, SlotIdError, SlotLayout};

/// First project id handed out. 0 is the filesystem default (unaccounted), and
/// leaving a low range free keeps room for future agent-owned projects such as
/// a shared package cache.
const FIRST_PROJECT_ID: u32 = 1000;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("workspace name {0:?} is not a valid Kubernetes object name")]
    InvalidWorkspaceName(String),
    #[error("slot index for {workspace:?} is corrupt: {source}")]
    CorruptIndex {
        workspace: String,
        source: SlotIdError,
    },
    #[error("project id counter exhausted")]
    ProjectIdExhausted,
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Io { context, source }
}

/// Reject anything that is not a plain Kubernetes object name.
///
/// The workspace name becomes a filename under `.index`, so `..`, `/` and NUL
/// must never get through even though the caller is the kubelet.
fn validate_workspace_name(name: &str) -> Result<(), StoreError> {
    let invalid = name.is_empty()
        || name.len() > 253
        || !name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '.')
        || name.starts_with(['-', '.'])
        || name.ends_with(['-', '.'])
        || name.contains("..");
    if invalid {
        return Err(StoreError::InvalidWorkspaceName(name.to_string()));
    }
    Ok(())
}

pub struct SlotStore {
    layout: SlotLayout,
}

/// A slot resolved for a workspace, and whether this call created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSlot {
    pub id: SlotId,
    pub project_id: u32,
    pub created: bool,
}

impl SlotStore {
    pub fn new(layout: SlotLayout) -> Self {
        Self { layout }
    }

    pub fn layout(&self) -> &SlotLayout {
        &self.layout
    }

    fn index_dir(&self) -> PathBuf {
        self.layout.root().join(".index")
    }

    fn workspace_link(&self, workspace: &str) -> PathBuf {
        self.index_dir().join(format!("ws-{workspace}"))
    }

    fn project_id_path(&self, id: &SlotId) -> PathBuf {
        self.index_dir().join(format!("projid-{id}"))
    }

    fn counter_path(&self) -> PathBuf {
        self.index_dir().join("next-project-id")
    }

    /// Look up the slot recorded for `workspace`, if any.
    pub fn lookup(&self, workspace: &str) -> Result<Option<ResolvedSlot>, StoreError> {
        validate_workspace_name(workspace)?;
        let link = self.workspace_link(workspace);
        let raw = match std::fs::read_to_string(&link) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_err(format!("reading {}", link.display()))(err)),
        };
        let id = SlotId::parse(raw.trim()).map_err(|source| StoreError::CorruptIndex {
            workspace: workspace.to_string(),
            source,
        })?;
        // A recorded slot whose directory is gone was wiped out from under us;
        // treat it as absent so the caller allocates a fresh one rather than
        // bind-mounting a path that does not exist.
        if !self.layout.slot_dir(&id).is_dir() {
            return Ok(None);
        }
        let project_id = std::fs::read_to_string(self.project_id_path(&id))
            .map_err(io_err(format!("reading project id for {id}")))?
            .trim()
            .parse::<u32>()
            .map_err(|_| StoreError::CorruptIndex {
                workspace: workspace.to_string(),
                source: SlotIdError::MissingPrefix,
            })?;
        Ok(Some(ResolvedSlot {
            id,
            project_id,
            created: false,
        }))
    }

    /// Allocate the next free project id.
    ///
    /// Ids are never reused: the counter only moves forward. Reusing an id
    /// while a previous tenant's tree still held blocks would make the new
    /// slot start out already partly consumed.
    fn allocate_project_id(&self) -> Result<u32, StoreError> {
        std::fs::create_dir_all(self.index_dir()).map_err(io_err("creating index dir"))?;
        let path = self.counter_path();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err(format!("opening {}", path.display())))?;
        // Serialise concurrent NodePublishVolume calls; kubelet issues them in
        // parallel for different pods on the same node.
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).map_err(|err| {
            StoreError::Io {
                context: "locking project id counter".into(),
                source: err.into(),
            }
        })?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(io_err("reading project id counter"))?;
        let next = raw.trim().parse::<u32>().unwrap_or(FIRST_PROJECT_ID);
        let following = next.checked_add(1).ok_or(StoreError::ProjectIdExhausted)?;
        file.seek(SeekFrom::Start(0))
            .map_err(io_err("rewinding project id counter"))?;
        file.set_len(0).map_err(io_err("truncating counter"))?;
        file.write_all(following.to_string().as_bytes())
            .map_err(io_err("writing project id counter"))?;
        file.sync_all().map_err(io_err("syncing counter"))?;
        Ok(next)
    }

    /// Record that `workspace` owns `id` with `project_id`.
    fn record(&self, workspace: &str, id: &SlotId, project_id: u32) -> Result<(), StoreError> {
        std::fs::create_dir_all(self.index_dir()).map_err(io_err("creating index dir"))?;
        std::fs::write(self.project_id_path(id), project_id.to_string())
            .map_err(io_err("writing project id"))?;
        std::fs::write(self.workspace_link(workspace), id.as_str())
            .map_err(io_err("writing workspace index"))?;
        Ok(())
    }

    /// Resolve the slot for `workspace`, allocating one if it has none.
    pub fn resolve_or_create(&self, workspace: &str) -> Result<ResolvedSlot, StoreError> {
        validate_workspace_name(workspace)?;
        if let Some(existing) = self.lookup(workspace)? {
            return Ok(existing);
        }
        let id = SlotId::generate();
        let project_id = self.allocate_project_id()?;
        std::fs::create_dir_all(self.layout.slot_dir(&id)).map_err(io_err("creating slot dir"))?;
        self.record(workspace, &id, project_id)?;
        Ok(ResolvedSlot {
            id,
            project_id,
            created: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SlotStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SlotStore::new(SlotLayout::new(dir.path()));
        (dir, store)
    }

    #[test]
    fn resolve_is_stable_for_the_same_workspace() {
        let (_dir, store) = store();
        let first = store.resolve_or_create("bmow-abc").unwrap();
        assert!(first.created);
        let second = store.resolve_or_create("bmow-abc").unwrap();
        assert!(!second.created, "second call should reuse the slot");
        assert_eq!(first.id, second.id);
        assert_eq!(first.project_id, second.project_id);
    }

    #[test]
    fn distinct_workspaces_get_distinct_slots_and_project_ids() {
        let (_dir, store) = store();
        let a = store.resolve_or_create("bmow-a").unwrap();
        let b = store.resolve_or_create("bmow-b").unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(a.project_id, b.project_id);
    }

    /// Project ids must only move forward, even across "restarts" (new store
    /// over the same root), or a fresh slot could inherit a previous tenant's
    /// accounted blocks.
    #[test]
    fn project_ids_are_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = std::collections::HashSet::new();
        for i in 0..25 {
            let store = SlotStore::new(SlotLayout::new(dir.path()));
            let slot = store.resolve_or_create(&format!("bmow-{i}")).unwrap();
            assert!(seen.insert(slot.project_id), "reused {}", slot.project_id);
        }
        assert_eq!(seen.len(), 25);
        assert!(seen.iter().all(|id| *id >= FIRST_PROJECT_ID));
    }

    /// A slot directory wiped by the LRU reaper must not leave the index
    /// pointing at a path that no longer exists.
    #[test]
    fn wiped_slot_directory_is_treated_as_absent() {
        let (_dir, store) = store();
        let first = store.resolve_or_create("bmow-abc").unwrap();
        std::fs::remove_dir_all(store.layout().slot_dir(&first.id)).unwrap();
        let second = store.resolve_or_create("bmow-abc").unwrap();
        assert!(second.created);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn rejects_workspace_names_that_could_escape_the_index() {
        for name in [
            "../etc/passwd",
            "..",
            "a/b",
            "",
            "-leading",
            "trailing-",
            "has..dots",
            "Uppercase",
            "nul\0byte",
        ] {
            assert!(
                store().1.resolve_or_create(name).is_err(),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn accepts_real_workspace_names() {
        let (_dir, store) = store();
        assert!(
            store
                .resolve_or_create("bmow-019e82b3-1234-7abc-8def-0123456789ab")
                .is_ok()
        );
    }
}
