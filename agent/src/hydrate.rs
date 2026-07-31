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

use indexer::object_store;
use indexer::restore::{RestoreError, RestoreOptions, restore};
use indexer::s3::{DownloadError, S3Client};

/// Notebook directory inside a slot.
///
/// The slot is mounted at `/home/me`, and the indexer archives
/// `/home/me/workspace`, so archive paths are relative to this subdirectory.
/// `/home/me/venv` deliberately sits outside it — the venv is rebuilt, never
/// restored.
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
}

/// Where a workspace's archive lives.
#[derive(Debug, Clone)]
pub struct ArchiveLocation {
    pub bucket: String,
    pub key_prefix: Option<String>,
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
    .await;
    // Extend rather than replace: this client is shared by every slot on the
    // node, so replacing would drop every other slot's markers.
    s3.extend_cache(cache_markers).await;

    let keys = indexer::upload::WorkspaceKeys::new(names, urls);
    Ok((options, keys, previous))
}

/// Continuously sync a bound slot to S3 until the returned task is aborted.
///
/// Only *bound* slots get a watcher — an idle slot has no runner and cannot
/// change — so the number of watchers on a node equals the number of running
/// runners, which is what one indexer pod per active workspace already costs
/// today.
pub async fn spawn_watcher(
    slot_dir: &Path,
    workspace: &str,
    archive: &ArchiveLocation,
    client: kubimo::Client,
    s3: indexer::s3::S3Client,
) -> Result<tokio::task::JoinHandle<()>, HydrateError> {
    let (options, keys, previous) =
        upload_inputs(slot_dir, workspace, archive, true, &client, &s3).await?;
    let name = workspace.to_string();
    Ok(tokio::spawn(async move {
        // Racing the watcher against the workspace's disappearance, rather than
        // relying on the unpublish to stop it. Deleting a workspace purges its
        // S3 prefix, but the runner pod lingers for its termination grace
        // period — and a shutting-down marimo still writes files. Any upload in
        // that window recreates the prefix the platform just emptied, with no
        // CR left to find it by. The final flush is guarded separately.
        tokio::select! {
            () = indexer::upload::watch(
                &options, &client, &s3, &keys, previous.names, previous.urls,
            ) => {}
            () = wait_until_deleted(&client, &name) => {
                tracing::info!(workspace = %name, "workspace deleted; stopping watcher");
            }
        }
    }))
}

/// How often to re-check that a watched workspace still exists.
///
/// Bounds how long an upload can keep recreating a purged archive. Short enough
/// to land inside a pod's termination grace period, long enough that a node's
/// worth of slots is a negligible request rate.
const DELETION_POLL: std::time::Duration = std::time::Duration::from_secs(10);

/// Resolve once `workspace` is gone or has been marked for deletion.
///
/// Never resolves on API errors: losing the API server briefly must not look
/// like a deletion and silently stop syncing a live workspace.
async fn wait_until_deleted(client: &kubimo::Client, workspace: &str) {
    loop {
        tokio::time::sleep(DELETION_POLL).await;
        match client.api::<kubimo::Workspace>().get_opt(workspace).await {
            Ok(Some(found)) if found.metadata.deletion_timestamp.is_none() => {}
            Ok(_) => return,
            Err(err) => {
                tracing::warn!(%err, workspace, "could not check whether the workspace still exists")
            }
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
/// files are durable; the venv and other scratch are not, and are rebuilt.
pub async fn flush_slot(
    slot_dir: &Path,
    workspace: &str,
    archive: &ArchiveLocation,
    client: &kubimo::Client,
    s3: &indexer::s3::S3Client,
) -> Result<(), HydrateError> {
    if !slot_dir.join(WORKSPACE_SUBDIR).is_dir() {
        // Nothing was ever hydrated here; there is nothing to push back.
        return Ok(());
    }
    let (options, keys, previous) =
        upload_inputs(slot_dir, workspace, archive, false, client, s3).await?;
    indexer::upload::run(
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
    Ok(())
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
