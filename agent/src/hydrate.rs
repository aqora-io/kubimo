//! Filling a fresh slot from the workspace's S3 archive.
//!
//! Hydration runs *inside* `NodePublishVolume`, so it completes before the
//! runner's containers start. That ordering is deliberate rather than
//! incidental: gVisor only delivers `inotify` events that originate inside the
//! sandbox, so files the agent writes from the host are invisible to marimo's
//! `--watch`, and a half-written `uv.lock` could be read by `uv sync`. Letting
//! the mount block until the tree is complete is what makes that safe.
//!
//! The restore itself is the indexer's, not a second implementation: it already
//! handles manifest parsing, `..`-rejection, symlink write-through, CRC
//! verification and partial-file cleanup.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indexer::object_store;
use indexer::restore::{RestoreError, RestoreOptions, restore};
use indexer::s3::{DownloadError, S3Client};
use kubimo::WorkspaceRestoreSecrets;
use tokio::time::Instant;

/// Notebook directory inside a slot.
///
/// The slot is mounted at `/home/me`, and the indexer archives
/// `/home/me/workspace`, so archive paths are relative to this subdirectory.
/// The rest of `/home/me` — the node template's pixi/uv caches — deliberately
/// sits outside it and is never archived or restored.
pub const WORKSPACE_SUBDIR: &str = "workspace";

/// How many files to transfer concurrently.
const DOWNLOAD_CONCURRENCY: usize = 16;

/// Matches the standalone indexer's default, so an archive written by either
/// path has the same contents.
const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum HydrateError {
    #[error("creating {path:?}: {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },
    #[error("restoring the workspace archive: {0}")]
    Restore(#[from] RestoreError),
    #[error("building archive urls: {0}")]
    ArchiveUrl(#[from] kubimo::url::ParseError),
    #[error("reading the workspace's directory listings: {0}")]
    ExistingDirs(#[from] kubimo::Error),
}

/// Where a workspace's archive lives.
#[derive(Debug, Clone)]
pub struct ArchiveLocation {
    pub bucket: String,
    pub key_prefix: Option<String>,
}

/// `spec.restoreFrom` as it reaches the agent: where the seed archive lives
/// and how to treat its secrets. A workspace's *own* archive never carries a
/// mode — rehydrating it always restores its own secret values.
#[derive(Debug, Clone)]
pub struct SeedArchive {
    pub location: ArchiveLocation,
    pub secrets: WorkspaceRestoreSecrets,
}

/// What the previous upload of this workspace left behind.
///
/// The upload pipeline computes the set of objects and `WorkspaceDirectory` CRs
/// to delete as "what was there before, minus what is there now", so an empty
/// `previous` means nothing is ever swept.
#[derive(Debug, Default)]
pub struct PreviousUpload {
    pub names: BTreeSet<String>,
    pub urls: BTreeSet<kubimo::url::Url>,
}

/// Build the upload pipeline's inputs for a slot.
///
/// Shared by the one-shot flush and the continuous watcher so the two can never
/// disagree about scope, key layout or file-size limits — a divergence there
/// would mean the flush wrote an archive the watcher would immediately rewrite.
///
/// The key sets are seeded from the workspace's existing `WorkspaceDirectory`
/// CRs rather than started empty. Content keys and directory CR names are
/// random (`KeySet::get_or_insert`), so a fresh set means every path gets a new
/// key on every mount: the whole workspace is re-uploaded under new keys, the
/// previous objects are orphaned with nothing left referencing them, and a
/// second set of directory CRs appears for the same paths. The standalone
/// indexer avoids this by calling `process_existing_dirs` at startup; the agent
/// has to do the same, once per publish rather than once per process.
async fn upload_inputs(
    slot_dir: &Path,
    workspace: &str,
    archive: &ArchiveLocation,
    watch: bool,
    client: &kubimo::Client,
    s3: &S3Client,
) -> Result<
    (
        indexer::upload::UploadOptions,
        indexer::upload::WorkspaceKeys,
        PreviousUpload,
    ),
    HydrateError,
> {
    let options = indexer::upload::UploadOptions {
        include_gitignored: false,
        exclude_hidden: false,
        max_file_size: MAX_FILE_SIZE,
        max_upload_concurrency: DOWNLOAD_CONCURRENCY,
        bucket: Some(archive.bucket.clone()),
        key_prefix: archive.key_prefix.clone(),
        watch,
        // Without this the manifest records metadata only and the archive
        // cannot be restored — the workspace would look backed up but hydrate
        // empty.
        upload_content: true,
        // A slot whose mount vanished, or whose hydration silently produced
        // nothing, walks empty and looks identical to a workspace the user
        // emptied. The agent must never resolve that ambiguity by deleting the
        // archive — S3 is the only copy in Pooled mode.
        allow_empty: false,
        watch_debounce_millis: 500,
        // A busy workspace still syncs at least every 10s.
        watch_max_wait_millis: 10_000,
        watch_poll_millis: 60_000,
        name: workspace.to_string(),
        directory: slot_dir.join(WORKSPACE_SUBDIR),
    };
    let mut names = indexer::keys::WorkspaceDirNameSet::new(workspace.to_string());
    let mut urls = indexer::keys::WorkspaceFileUrlSet::new(
        archive.bucket.clone(),
        archive.key_prefix.clone(),
    )?;
    let mut previous = PreviousUpload::default();
    let mut cache_markers = indexer::s3::CacheMarkers::new();
    indexer::upload::process_existing_dirs(
        client,
        workspace,
        &mut names,
        &mut urls,
        &mut cache_markers,
        &mut previous.names,
        &mut previous.urls,
    )
    .await?;
    // Extend rather than replace: this client is shared by every slot on the
    // node, so replacing would drop every other slot's markers.
    s3.extend_cache(cache_markers).await;

    let keys = indexer::upload::WorkspaceKeys::new(names, urls);
    Ok((options, keys, previous))
}

/// Continuously sync a bound slot to S3 until the returned task is aborted,
/// or the workspace is deleted or moves to another node.
///
/// Only *bound* slots get a watcher — an idle slot has no runner and cannot
/// change — so the number of watchers on a node equals the number of running
/// runners, which is what one indexer pod per active workspace already costs
/// today.
///
/// The returned clock is when the task last confirmed that this node holds the
/// workspace; the final flush goes by it when it cannot confirm that itself.
pub fn spawn_watcher(
    slot_dir: &Path,
    workspace: &str,
    archive: &ArchiveLocation,
    client: kubimo::Client,
    s3: indexer::s3::S3Client,
    node: &str,
) -> (tokio::task::JoinHandle<()>, OwnershipClock) {
    // The publish starting this watcher has just written this node into
    // `status.slot`, which confirms it as well as any read.
    let clock = OwnershipClock::confirmed_now();
    let slot_dir = slot_dir.to_path_buf();
    let archive = archive.clone();
    let name = workspace.to_string();
    let node = node.to_string();
    let task_clock = clock.clone();
    let handle = tokio::spawn(async move {
        let clock = task_clock;
        loop {
            // Read afresh at the start and after every pause: a paused watch's
            // view of what it last uploaded is gone with it. A failed read is
            // retried rather than worked around, since a watch started from a
            // partial one re-uploads everything it missed under new keys.
            let (options, keys, previous) = loop {
                match upload_inputs(&slot_dir, &name, &archive, true, &client, &s3).await {
                    Ok(inputs) => break inputs,
                    Err(err) => {
                        tracing::warn!(%err, workspace = %name, "could not prepare to sync the slot; retrying");
                    }
                }
                tokio::time::sleep(OWNERSHIP_POLL).await;
            };
            // Racing the watcher against the workspace's disappearance, rather
            // than relying on the unpublish to stop it. Deleting a workspace
            // purges its S3 prefix, but the runner pod lingers for its
            // termination grace period — and a shutting-down marimo still
            // writes files. Any upload in that window recreates the prefix the
            // platform just emptied, with no CR left to find it by. The final
            // flush is guarded separately.
            //
            // Likewise its move to another node: once this node's pods were
            // force-deleted off it, the workspace is hydrated and written to
            // elsewhere, and this watcher — which re-uploads the manifest
            // every cycle, changed or not — would overwrite that node's archive
            // with this stale copy for as long as it ran. Every pass checks
            // first; the poll below only catches a move or an unreachable API
            // server between passes. A failed check lets the pass go ahead:
            // only ownership unconfirmed for the whole grace pauses sync,
            // through the poll.
            let may_upload = || async {
                match ownership(&client, &name, &node).await {
                    Some(Ownership::Held) => {
                        clock.confirm();
                        true
                    }
                    Some(Ownership::Lost(lost)) => {
                        clock.lose();
                        stopping(&name, &lost);
                        false
                    }
                    None => true,
                }
            };
            let lost = tokio::select! {
                () = indexer::upload::watch(
                    &options, &client, &s3, &keys, previous.names, previous.urls, may_upload,
                ) => return,
                lost = watch_ownership(&client, &name, &node, &clock) => lost,
            };
            if let Some(lost) = lost {
                stopping(&name, &lost);
                return;
            }
            tracing::warn!(
                workspace = %name,
                "cannot confirm with the API server that this node still holds the \
                 workspace; pausing sync until it can"
            );
            loop {
                tokio::time::sleep(OWNERSHIP_POLL).await;
                match ownership(&client, &name, &node).await {
                    Some(Ownership::Held) => break,
                    Some(Ownership::Lost(lost)) => {
                        clock.lose();
                        stopping(&name, &lost);
                        return;
                    }
                    None => {}
                }
            }
            clock.confirm();
            tracing::info!(workspace = %name, "ownership confirmed; resuming sync");
        }
    });
    (handle, clock)
}

fn stopping(workspace: &str, lost: &Lost) {
    match lost {
        Lost::Deleted => tracing::info!(workspace, "workspace deleted; stopping watcher"),
        Lost::Moved(current) => tracing::warn!(
            workspace,
            current_node = %current,
            "workspace has moved to another node; stopping watcher"
        ),
    }
}

/// What one read of the workspace says about this node's hold on it.
pub(crate) enum Ownership {
    Held,
    Lost(Lost),
}

/// Why this node no longer holds a workspace.
pub(crate) enum Lost {
    /// The workspace is deleted or being deleted.
    Deleted,
    /// `status.slot.node` names this other node.
    Moved(String),
}

/// When this node last confirmed, from the API server, that it holds a
/// workspace.
///
/// Kept current by the slot's watcher, and cleared once it sees the workspace
/// deleted or moved. The final flush reads it when its own check fails: a
/// confirmation within [`OWNERSHIP_GRACE`] is what lets a watcher pass go
/// ahead on a failed check, and the final flush is one more pass.
#[derive(Clone)]
pub struct OwnershipClock(Arc<Mutex<Option<Instant>>>);

impl OwnershipClock {
    pub(crate) fn confirmed_now() -> Self {
        Self(Arc::new(Mutex::new(Some(Instant::now()))))
    }

    fn confirm(&self) {
        *self.lock() = Some(Instant::now());
    }

    fn lose(&self) {
        *self.lock() = None;
    }

    /// Whether ownership was confirmed within [`OWNERSHIP_GRACE`].
    pub fn is_recent(&self) -> bool {
        self.lock()
            .is_some_and(|confirmed| confirmed.elapsed() < OWNERSHIP_GRACE)
    }

    fn lock(&self) -> MutexGuard<'_, Option<Instant>> {
        match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// How often to re-check that a watched workspace still exists and still
/// lives on this node.
///
/// Bounds how long an upload can keep recreating a purged archive, or
/// overwriting the archive of the node the workspace moved to. Short enough
/// to land inside a pod's termination grace period, long enough that a node's
/// worth of slots is a negligible request rate.
const OWNERSHIP_POLL: Duration = Duration::from_secs(10);

/// How long a read of the workspace's ownership may take before it counts as
/// failed.
///
/// The client's own read timeout is kube's, close to five minutes: a read
/// that hangs that long, on a node cut off from the API server without a
/// reset, would run out the whole of [`OWNERSHIP_GRACE`] on its own.
const OWNERSHIP_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long ownership may go unconfirmed before uploads pause, counted from
/// the last read that confirmed it.
///
/// A node the API server cannot reach may have had its pods force-deleted,
/// and its workspace hydrated and written to elsewhere — but never sooner
/// than the unreachable taint's eviction (300 s) plus the pod's grace period
/// plus the controller's margin (60 s). Pausing well inside that window means
/// a node cut off from the API but not from S3 has stopped uploading before
/// any replacement exists, while a short API blip on a healthy node does not
/// interrupt its sync at all.
const OWNERSHIP_GRACE: Duration = Duration::from_secs(60);

/// Read this node's hold on `workspace`. `None` when the API server could not
/// say within [`OWNERSHIP_READ_TIMEOUT`].
pub(crate) async fn ownership(
    client: &kubimo::Client,
    workspace: &str,
    node: &str,
) -> Option<Ownership> {
    let read = tokio::time::timeout(
        OWNERSHIP_READ_TIMEOUT,
        client.api::<kubimo::Workspace>().get_opt(workspace),
    )
    .await;
    let found = match read {
        Ok(Ok(found)) => found,
        Ok(Err(err)) => {
            tracing::warn!(%err, workspace, "could not check whether this node still holds the workspace");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                workspace,
                "timed out checking whether this node still holds the workspace"
            );
            return None;
        }
    };
    Some(match found {
        Some(found) if found.metadata.deletion_timestamp.is_none() => match found
            .status
            .and_then(|status| status.slot)
            .and_then(|slot| slot.node)
            .filter(|current| current != node)
        {
            Some(current) => Ownership::Lost(Lost::Moved(current)),
            None => Ownership::Held,
        },
        _ => Ownership::Lost(Lost::Deleted),
    })
}

/// Resolve once `workspace` is gone, has been marked for deletion, or its
/// `status.slot` names a node other than `node` (`Some`), or once `clock` has
/// gone unconfirmed for [`OWNERSHIP_GRACE`] (`None`).
async fn watch_ownership(
    client: &kubimo::Client,
    workspace: &str,
    node: &str,
    clock: &OwnershipClock,
) -> Option<Lost> {
    loop {
        tokio::time::sleep(OWNERSHIP_POLL).await;
        match ownership(client, workspace, node).await {
            Some(Ownership::Held) => clock.confirm(),
            Some(Ownership::Lost(lost)) => {
                clock.lose();
                return Some(lost);
            }
            None if !clock.is_recent() => return None,
            None => {}
        }
    }
}

/// Push a slot's tracked files up to S3 and refresh its `WorkspaceDirectory`
/// CRs.
///
/// One pass, no watch: this is the flush that runs when a runner goes away, so
/// the durability boundary is "the last time a runner stopped" rather than
/// "whenever the watcher last fired".
///
/// The archive scope is deliberately unchanged from the standalone indexer —
/// `.gitignore` is honoured and only `<slot>/workspace` is walked. Tracked
/// files are durable; the node template's caches and other scratch are not,
/// and are rebuilt.
///
/// `Some` means the archive now fully represents the slot, and only then may
/// the caller record a flush. `None` means part of the tree — or all of it —
/// is still only on this node: the upload pipeline logs its own failures and
/// carries on, because in the watcher's world the next cycle repairs them, but
/// a one-shot flush has no next cycle. Anything that reads the flush marker as
/// permission to discard the slot would be discarding the only copy.
pub async fn flush_slot(
    slot_dir: &Path,
    workspace: &str,
    archive: &ArchiveLocation,
    client: &kubimo::Client,
    s3: &indexer::s3::S3Client,
) -> Result<Option<Flushed>, HydrateError> {
    if !slot_dir.join(WORKSPACE_SUBDIR).is_dir() {
        // Nothing was ever hydrated here, so nothing was pushed back either.
        // Not an error, but not a flush: the archive has not been shown to
        // match a slot whose contents were never walked.
        return Ok(None);
    }
    let (options, keys, previous) =
        upload_inputs(slot_dir, workspace, archive, false, client, s3).await?;
    let result = indexer::upload::run(
        &options,
        // One-shot flush; the watcher keeps its own long-lived cache.
        &indexer::fingerprint::ContentCache::new(),
        client,
        s3,
        &keys,
        &previous.names,
        &previous.urls,
    )
    .await;
    // A refusal means the walk came back empty while an archive exists, so the
    // archive was deliberately left alone — it describes some older state of
    // this slot, not this one.
    if result.refused || result.failures > 0 {
        return Ok(None);
    }
    Ok(Some(Flushed {
        content_bytes: result.content_bytes,
    }))
}

/// What a completed flush put in the archive.
#[derive(Debug, Clone, Copy)]
pub struct Flushed {
    /// Total size of the workspace's tracked files, as the flush walked them.
    /// Small next to the slot's disk usage — the node template's caches are
    /// the bulk of that and are never archived — which is why it is reported
    /// separately rather than folded into `status.storage`.
    pub content_bytes: u64,
}

/// Restore `archive` into `slot_dir/workspace`.
///
/// Returns `false` when the workspace has no archive yet — a brand-new
/// workspace that has never been indexed — which is not an error: the runner
/// starts on an empty slot.
pub async fn hydrate_slot(
    slot_dir: &Path,
    archive: &ArchiveLocation,
    s3: &S3Client,
    secrets: WorkspaceRestoreSecrets,
) -> Result<bool, HydrateError> {
    let directory: PathBuf = slot_dir.join(WORKSPACE_SUBDIR);
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|source| HydrateError::CreateDir {
            path: directory.display().to_string(),
            source,
        })?;
    let options = RestoreOptions {
        bucket: archive.bucket.clone(),
        key_prefix: archive.key_prefix.clone(),
        directory,
        max_download_concurrency: DOWNLOAD_CONCURRENCY,
        // Not best-effort: a slot that is silently missing files looks like the
        // user lost data. Fail the mount instead, so the runner never starts on
        // a partial workspace.
        best_effort: false,
        secrets,
    };
    match restore(&options, s3).await {
        Ok(()) => Ok(true),
        // A workspace that has never been indexed has no manifest. That is the
        // normal state for a freshly created workspace, not a failure.
        //
        // Only the manifest fetch can surface `NotFound` here: per-file
        // download errors are counted inside `restore` and come back as
        // `Failed`, so this cannot quietly swallow missing file content.
        Err(RestoreError::Download(DownloadError::S3(object_store::Error::NotFound {
            ..
        }))) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A client whose every request is answered by `respond`.
    pub(crate) fn client_answering(
        respond: impl Fn() -> Option<kubimo::Workspace> + Clone + Send + 'static,
    ) -> kubimo::Client {
        let service = tower::service_fn(move |_req: http::Request<kubimo::kube::client::Body>| {
            let respond = respond.clone();
            async move {
                let response = match respond() {
                    Some(workspace) => http::Response::builder().status(200).body(
                        kubimo::kube::client::Body::from(serde_json::to_vec(&workspace).unwrap()),
                    ),
                    None => http::Response::builder().status(404).body(
                        kubimo::kube::client::Body::from(
                            serde_json::to_vec(&serde_json::json!({
                                "kind": "Status",
                                "apiVersion": "v1",
                                "status": "Failure",
                                "reason": "NotFound",
                                "code": 404,
                            }))
                            .unwrap(),
                        ),
                    ),
                };
                Ok::<_, std::io::Error>(response.unwrap())
            }
        });
        kubimo::Client::new(
            kubimo::kube::Client::new(service, "default"),
            "kubimo-indexer",
        )
    }

    /// A client whose every request fails outright.
    pub(crate) fn failing_client() -> kubimo::Client {
        let service = tower::service_fn(|_req: http::Request<kubimo::kube::client::Body>| async {
            Err::<http::Response<kubimo::kube::client::Body>, std::io::Error>(
                std::io::Error::other("no cluster in tests"),
            )
        });
        kubimo::Client::new(
            kubimo::kube::Client::new(service, "default"),
            "kubimo-indexer",
        )
    }

    /// A client whose every request hangs, as across a partition that drops
    /// packets rather than resetting connections.
    pub(crate) fn hanging_client() -> kubimo::Client {
        let service = tower::service_fn(|_req: http::Request<kubimo::kube::client::Body>| {
            std::future::pending::<Result<http::Response<kubimo::kube::client::Body>, std::io::Error>>(
            )
        });
        kubimo::Client::new(
            kubimo::kube::Client::new(service, "default"),
            "kubimo-indexer",
        )
    }

    pub(crate) fn workspace_on(node: Option<&str>) -> kubimo::Workspace {
        let mut workspace = kubimo::Workspace::new("bmow-x", Default::default());
        workspace.status = Some(kubimo::WorkspaceStatus {
            slot: Some(kubimo::WorkspaceSlotStatus {
                node: node.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        });
        workspace
    }

    #[tokio::test]
    async fn ownership_reads_held_moved_and_deleted() {
        let on = |node: Option<&'static str>| client_answering(move || Some(workspace_on(node)));
        assert!(matches!(
            ownership(&on(Some("node-a")), "bmow-x", "node-a").await,
            Some(Ownership::Held)
        ));
        // Nothing recorded yet is no rival either.
        assert!(matches!(
            ownership(&on(None), "bmow-x", "node-a").await,
            Some(Ownership::Held)
        ));
        assert!(matches!(
            ownership(&on(Some("node-b")), "bmow-x", "node-a").await,
            Some(Ownership::Lost(Lost::Moved(node))) if node == "node-b"
        ));
        assert!(matches!(
            ownership(&client_answering(|| None), "bmow-x", "node-a").await,
            Some(Ownership::Lost(Lost::Deleted))
        ));
    }

    /// kube's own read timeout is close to five minutes, longer than the
    /// whole grace; a read that hangs must count as failed long before that.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_ownership_read_fails_within_its_timeout() {
        let started = Instant::now();
        assert!(
            ownership(&hanging_client(), "bmow-x", "node-a")
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), OWNERSHIP_READ_TIMEOUT);
    }

    /// The grace runs from the last confirmation, not from the first failed
    /// read: reads that hang from the start pause sync once the grace is out,
    /// not a read timeout or more later.
    #[tokio::test(start_paused = true)]
    async fn unconfirmed_ownership_pauses_sync_once_the_grace_runs_out() {
        let clock = OwnershipClock::confirmed_now();
        let started = Instant::now();
        let lost = watch_ownership(&hanging_client(), "bmow-x", "node-a", &clock).await;
        assert!(lost.is_none(), "unverified, not lost");
        assert!(started.elapsed() >= OWNERSHIP_GRACE);
        assert!(started.elapsed() <= OWNERSHIP_GRACE + OWNERSHIP_READ_TIMEOUT);
        assert!(!clock.is_recent());
    }

    /// A move is final: the clock is cleared, so a final flush whose own check
    /// fails cannot go ahead on a confirmation from before the move.
    #[tokio::test(start_paused = true)]
    async fn a_move_clears_the_ownership_clock() {
        let clock = OwnershipClock::confirmed_now();
        let moved = client_answering(|| Some(workspace_on(Some("node-b"))));
        let lost = watch_ownership(&moved, "bmow-x", "node-a", &clock).await;
        assert!(matches!(lost, Some(Lost::Moved(node)) if node == "node-b"));
        assert!(!clock.is_recent());
    }

    /// A slot with no `workspace` subdirectory has had nothing pushed to S3,
    /// and saying otherwise is what lets the reaper evict it. Reporting `Ok` on
    /// this path once meant the caller marked the slot flushed without a single
    /// byte having been uploaded.
    ///
    /// Nothing here reaches the cluster or S3: the check runs before either
    /// client is touched, which is the point — an empty slot must be decided
    /// locally.
    #[tokio::test]
    async fn a_slot_without_a_workspace_directory_is_not_a_flush() {
        let dir = tempfile::tempdir().unwrap();
        // Every request this client makes fails, so reaching the cluster at all
        // would be visible as an error rather than passing quietly. Building
        // one from a `Config` is not an option: that constructs a TLS stack
        // eagerly, which panics in a workspace-wide test run where more than
        // one rustls crypto provider is compiled in.
        let service = tower::service_fn(|_req: http::Request<kubimo::kube::client::Body>| async {
            Err::<http::Response<kubimo::kube::client::Body>, std::io::Error>(
                std::io::Error::other("no cluster in tests"),
            )
        });
        let client = kubimo::Client::new(
            kubimo::kube::Client::new(service, "default"),
            "kubimo-indexer",
        );
        let archive = ArchiveLocation {
            bucket: "bucket".to_string(),
            key_prefix: None,
        };
        let flushed = flush_slot(
            dir.path(),
            "bmow-abc",
            &archive,
            &client,
            &indexer::s3::S3Client::from_env(),
        )
        .await
        .expect("an unhydrated slot is not an error");
        assert!(
            flushed.is_none(),
            "nothing was uploaded, so this must not be recorded as a flush"
        );
    }
}
