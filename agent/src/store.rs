//! Persistent slot bookkeeping on the data volume.
//!
//! Two things must survive an agent restart: which slot belongs to which
//! workspace, and which XFS project ids are taken. Both live under
//! `<root>/.index`, which is never bind-mounted into a pod.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

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

/// A slot currently published to a runner pod, and where its archive lives.
///
/// Recorded at publish time because `NodeUnpublishVolume` receives only the
/// volume id — not the volume attributes that carried the archive location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedSlot {
    pub workspace: String,
    /// Namespace the Workspace CR lives in. Not the agent's own: it runs beside
    /// the controller, while workspaces belong to whoever created them.
    pub namespace: String,
    pub slot: SlotId,
    pub bucket: Option<String>,
    pub key_prefix: Option<String>,
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

    /// Marker recording that this slot's contents reached S3.
    ///
    /// Deliberately node-local rather than read from `status.archive`: that
    /// field is per-*workspace*, and the case this exists for is a workspace
    /// holding slots on two nodes at once. Whichever node flushed last would own
    /// the timestamp, so trusting it could evict the other node's slot on the
    /// strength of a flush that never covered it.
    fn flushed_path(&self, id: &SlotId) -> PathBuf {
        self.index_dir().join(format!("flushed-{id}"))
    }

    fn counter_path(&self) -> PathBuf {
        self.index_dir().join("next-project-id")
    }

    /// Record that `workspace`'s slot has been flushed to S3.
    ///
    /// The file's mtime is the timestamp; nothing reads its contents. Only
    /// called after a flush actually succeeded, which is what lets the reaper
    /// treat the slot as a cache it may drop.
    pub fn mark_flushed(&self, workspace: &str) -> Result<(), StoreError> {
        validate_workspace_name(workspace)?;
        let Some(id) = self.lookup_slot_id(workspace)? else {
            return Ok(());
        };
        let path = self.flushed_path(&id);
        std::fs::write(&path, b"").map_err(io_err(format!("writing {}", path.display())))?;
        Ok(())
    }

    /// Forget that `workspace`'s slot was flushed, because it is about to
    /// change.
    ///
    /// Called on every publish, so the marker means "flushed *and* untouched
    /// since" rather than merely "flushed at some point". Without it a slot
    /// mounted twice — a cache job beside a runner — keeps the marker written
    /// when the first mount ended, and if the second one's final flush then
    /// fails, the reaper would read a stale marker as permission to evict work
    /// that never reached S3.
    pub fn clear_flushed(&self, workspace: &str) -> Result<(), StoreError> {
        validate_workspace_name(workspace)?;
        let Some(id) = self.lookup_slot_id(workspace)? else {
            return Ok(());
        };
        match std::fs::remove_file(self.flushed_path(&id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(format!("clearing flush marker for {id}"))(err)),
        }
    }

    /// How long ago `workspace`'s slot was last flushed.
    ///
    /// `None` means it has never been flushed successfully, which callers must
    /// read as "not safe to drop": the slot may hold the only copy of the
    /// tenant's newest work. A marker whose mtime is in the future — a clock
    /// step — also yields `None` rather than a bogus age.
    pub fn flushed_ago(&self, workspace: &str) -> Result<Option<Duration>, StoreError> {
        validate_workspace_name(workspace)?;
        let Some(id) = self.lookup_slot_id(workspace)? else {
            return Ok(None);
        };
        let Ok(meta) = std::fs::metadata(self.flushed_path(&id)) else {
            return Ok(None);
        };
        Ok(meta
            .modified()
            .ok()
            .and_then(|at| std::time::SystemTime::now().duration_since(at).ok()))
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
        // First line only: the namespace follows on the second.
        let id =
            SlotId::parse(raw.lines().next().unwrap_or_default().trim()).map_err(|source| {
                StoreError::CorruptIndex {
                    workspace: workspace.to_string(),
                    source,
                }
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
    fn record(
        &self,
        workspace: &str,
        namespace: &str,
        id: &SlotId,
        project_id: u32,
    ) -> Result<(), StoreError> {
        std::fs::create_dir_all(self.index_dir()).map_err(io_err("creating index dir"))?;
        std::fs::write(self.project_id_path(id), project_id.to_string())
            .map_err(io_err("writing project id"))?;
        // Slot id then namespace, one per line. The namespace is recorded
        // because a Workspace CR can only be looked up in the namespace it
        // lives in, and the agent's own namespace is not it — the agent runs
        // beside the controller while workspaces belong to whichever namespace
        // created them.
        std::fs::write(
            self.workspace_link(workspace),
            format!("{}\n{namespace}", id.as_str()),
        )
        .map_err(io_err("writing workspace index"))?;
        Ok(())
    }

    fn publish_path(&self, volume_id: &str) -> PathBuf {
        // The volume id is kubelet-generated (`csi-<hex>`), but it still reaches
        // us from outside, so hash it rather than trusting it as a filename.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in volume_id.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.index_dir().join(format!("vol-{hash:016x}"))
    }

    /// Remember what a published volume maps to.
    ///
    /// `NodeUnpublishVolume` receives only the volume id and target path — not
    /// the volume attributes — so without this the agent could not tell which
    /// workspace to flush when a runner goes away.
    pub fn record_publish(
        &self,
        volume_id: &str,
        published: &PublishedSlot,
    ) -> Result<(), StoreError> {
        validate_workspace_name(&published.workspace)?;
        std::fs::create_dir_all(self.index_dir()).map_err(io_err("creating index dir"))?;
        // Line-oriented rather than JSON to keep the agent's on-disk state
        // trivially inspectable during an incident.
        let body = format!(
            "{}\n{}\n{}\n{}\n{}",
            published.workspace,
            published.slot,
            published.bucket.as_deref().unwrap_or(""),
            published.key_prefix.as_deref().unwrap_or(""),
            published.namespace,
        );
        std::fs::write(self.publish_path(volume_id), body).map_err(io_err("recording publish"))?;
        Ok(())
    }

    /// What a published volume maps to, if it is still recorded.
    pub fn lookup_publish(&self, volume_id: &str) -> Result<Option<PublishedSlot>, StoreError> {
        let raw = match std::fs::read_to_string(self.publish_path(volume_id)) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_err("reading publish record")(err)),
        };
        let mut lines = raw.lines();
        let (Some(workspace), Some(slot)) = (lines.next(), lines.next()) else {
            return Ok(None);
        };
        let slot = SlotId::parse(slot).map_err(|source| StoreError::CorruptIndex {
            workspace: workspace.to_string(),
            source,
        })?;
        let non_empty = |value: Option<&str>| {
            value
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        };
        let bucket = non_empty(lines.next());
        let key_prefix = non_empty(lines.next());
        Ok(Some(PublishedSlot {
            workspace: workspace.to_string(),
            slot,
            bucket,
            key_prefix,
            namespace: non_empty(lines.next()).unwrap_or_default(),
        }))
    }

    pub fn forget_publish(&self, volume_id: &str) {
        let _ = std::fs::remove_file(self.publish_path(volume_id));
    }

    /// Drop every publish record naming `workspace`.
    ///
    /// A record is keyed by volume id, so a workspace that was published, unpublished
    /// and republished can have left more than one behind. Any that survives makes
    /// [`Self::published_workspaces`] report the workspace as mounted forever, which
    /// stops the reaper reclaiming its slot for as long as this data volume lives.
    fn forget_publishes_for(&self, workspace: &str) {
        let Ok(entries) = std::fs::read_dir(self.index_dir()) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("vol-") {
                continue;
            }
            if std::fs::read_to_string(entry.path())
                .is_ok_and(|raw| raw.lines().next() == Some(workspace))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Every workspace this node holds a slot for.
    ///
    /// Read from the index rather than by listing slot directories, because a
    /// slot's directory name is opaque — which workspace it belongs to is only
    /// recorded in the `ws-` link.
    pub fn workspaces(&self) -> Result<Vec<(String, Option<String>)>, StoreError> {
        let entries = match std::fs::read_dir(self.index_dir()) {
            Ok(entries) => entries,
            // No index yet means no slots yet.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_err("listing index dir")(err)),
        };
        Ok(entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let workspace = name.to_str()?.strip_prefix("ws-")?.to_string();
                // Anything failing validation was not written by us.
                validate_workspace_name(&workspace).ok()?;
                let namespace = self.recorded_namespace(&workspace);
                Some((workspace, namespace))
            })
            .collect())
    }

    /// The workspaces that currently have at least one published volume.
    ///
    /// A slot in this set is mounted into a live pod, so it must never be
    /// reclaimed however its workspace's CR looks.
    pub fn published_workspaces(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let entries = match std::fs::read_dir(self.index_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(err) => return Err(io_err("listing index dir")(err)),
        };
        let mut published = std::collections::HashSet::new();
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Must match `publish_path`. Getting this prefix wrong makes every
            // slot look unpublished, and the sweep then deletes slots out from
            // under running pods.
            if !name.starts_with("vol-") {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(entry.path())
                && let Some(workspace) = raw.lines().next()
            {
                published.insert(workspace.to_string());
            }
        }
        Ok(published)
    }

    /// Drop a workspace's slot and everything indexing it.
    ///
    /// The project id file goes too, releasing that id: the quota limit set
    /// against it is meaningless once no inodes carry it.
    pub fn remove_slot(&self, workspace: &str) -> Result<bool, StoreError> {
        validate_workspace_name(workspace)?;
        // Before anything else, and on both exits: a leaked publish record outlives the
        // slot it names and would make this workspace look permanently mounted.
        self.forget_publishes_for(workspace);
        let Some(id) = self.lookup_slot_id(workspace)? else {
            // Nothing recorded, though the link may be a dangling leftover.
            let _ = std::fs::remove_file(self.workspace_link(workspace));
            return Ok(false);
        };
        let dir = self.layout.slot_dir(&id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_err(format!("removing {}", dir.display()))(err)),
        }
        let _ = std::fs::remove_file(self.workspace_link(workspace));
        let _ = std::fs::remove_file(self.project_id_path(&id));
        let _ = std::fs::remove_file(self.flushed_path(&id));
        Ok(true)
    }

    /// The namespace recorded for `workspace`, if the index carries one.
    ///
    /// Absent for slots written before the namespace was recorded. Callers must
    /// treat that as "unknown" rather than guessing: guessing wrong means
    /// looking a Workspace up in a namespace it does not live in, which reads
    /// as "deleted".
    fn recorded_namespace(&self, workspace: &str) -> Option<String> {
        let raw = std::fs::read_to_string(self.workspace_link(workspace)).ok()?;
        raw.lines()
            .nth(1)
            .map(str::trim)
            .filter(|namespace| !namespace.is_empty())
            .map(ToString::to_string)
    }

    /// The slot id recorded for `workspace`, whether or not its directory still
    /// exists. Unlike [`Self::lookup`] a missing directory is not treated as
    /// "no slot", because reclaiming has to clean up the index either way.
    fn lookup_slot_id(&self, workspace: &str) -> Result<Option<SlotId>, StoreError> {
        let link = self.workspace_link(workspace);
        let raw = match std::fs::read_to_string(&link) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_err(format!("reading {}", link.display()))(err)),
        };
        SlotId::parse(raw.lines().next().unwrap_or_default().trim())
            .map(Some)
            .map_err(|source| StoreError::CorruptIndex {
                workspace: workspace.to_string(),
                source,
            })
    }

    /// Resolve the slot for `workspace`, allocating one if it has none.
    pub fn resolve_or_create(
        &self,
        workspace: &str,
        namespace: &str,
    ) -> Result<ResolvedSlot, StoreError> {
        validate_workspace_name(workspace)?;
        if let Some(existing) = self.lookup(workspace)? {
            return Ok(existing);
        }
        let id = SlotId::generate();
        let project_id = self.allocate_project_id()?;
        std::fs::create_dir_all(self.layout.slot_dir(&id)).map_err(io_err("creating slot dir"))?;
        self.record(workspace, namespace, &id, project_id)?;
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
        let first = store.resolve_or_create("bmow-abc", "platform").unwrap();
        assert!(first.created);
        let second = store.resolve_or_create("bmow-abc", "platform").unwrap();
        assert!(!second.created, "second call should reuse the slot");
        assert_eq!(first.id, second.id);
        assert_eq!(first.project_id, second.project_id);
    }

    #[test]
    fn distinct_workspaces_get_distinct_slots_and_project_ids() {
        let (_dir, store) = store();
        let a = store.resolve_or_create("bmow-a", "platform").unwrap();
        let b = store.resolve_or_create("bmow-b", "platform").unwrap();
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
            let slot = store
                .resolve_or_create(&format!("bmow-{i}"), "platform")
                .unwrap();
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
        let first = store.resolve_or_create("bmow-abc", "platform").unwrap();
        std::fs::remove_dir_all(store.layout().slot_dir(&first.id)).unwrap();
        let second = store.resolve_or_create("bmow-abc", "platform").unwrap();
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
                store().1.resolve_or_create(name, "platform").is_err(),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn publish_records_round_trip() {
        let (_dir, store) = store();
        let slot = store.resolve_or_create("bmow-abc", "platform").unwrap();
        let published = PublishedSlot {
            workspace: "bmow-abc".to_string(),
            // The unpublish path has no volume attributes to re-read, so the
            // namespace has to survive the round trip or the final flush goes
            // looking for the workspace in the wrong one.
            namespace: "platform".to_string(),
            slot: slot.id.clone(),
            bucket: Some("bucket".to_string()),
            key_prefix: Some("workspace/abc/".to_string()),
        };
        store.record_publish("csi-deadbeef", &published).unwrap();
        assert_eq!(
            store.lookup_publish("csi-deadbeef").unwrap().unwrap(),
            published
        );
        store.forget_publish("csi-deadbeef");
        assert!(store.lookup_publish("csi-deadbeef").unwrap().is_none());
    }

    /// Unpublishing a volume the agent never published (it restarted, or the
    /// record was reaped) must not error — kubelet retries until it succeeds.
    #[test]
    fn lookup_publish_is_none_for_an_unknown_volume() {
        let (_dir, store) = store();
        assert!(store.lookup_publish("csi-never-seen").unwrap().is_none());
    }

    /// A volume id reaches us from outside; it must never be used as a path
    /// component directly.
    #[test]
    fn publish_record_path_stays_inside_the_index() {
        let (_dir, store) = store();
        let path = store.publish_path("../../etc/passwd");
        assert_eq!(path.parent().unwrap(), store.index_dir());
    }

    #[test]
    fn accepts_real_workspace_names() {
        let (_dir, store) = store();
        assert!(
            store
                .resolve_or_create("bmow-019e82b3-1234-7abc-8def-0123456789ab", "platform")
                .is_ok()
        );
    }

    fn publish(store: &SlotStore, volume_id: &str, workspace: &str, slot: SlotId) {
        store
            .record_publish(
                volume_id,
                &PublishedSlot {
                    workspace: workspace.to_string(),
                    namespace: "platform".to_string(),
                    slot,
                    bucket: None,
                    key_prefix: None,
                },
            )
            .unwrap();
    }

    /// The reaper keeps any workspace named in `published_workspaces`, unconditionally.
    /// So a publish record that outlives its slot does not merely leak a small file — it
    /// disables reclamation for that workspace for as long as the data volume lives.
    #[test]
    fn removing_a_slot_drops_its_publish_records() {
        let (_dir, store) = store();
        let mine = store.resolve_or_create("bmow-abc", "platform").unwrap();
        let theirs = store.resolve_or_create("bmow-xyz", "platform").unwrap();
        // Two records for one workspace: republishing under a new volume id is normal.
        publish(&store, "vol-one", "bmow-abc", mine.id.clone());
        publish(&store, "vol-two", "bmow-abc", mine.id.clone());
        publish(&store, "vol-other", "bmow-xyz", theirs.id.clone());

        store.remove_slot("bmow-abc").unwrap();

        let published = store.published_workspaces().unwrap();
        assert!(
            !published.contains("bmow-abc"),
            "a surviving record would pin this workspace as published forever"
        );
        assert!(
            published.contains("bmow-xyz"),
            "another workspace's record must be left alone"
        );
    }
}
