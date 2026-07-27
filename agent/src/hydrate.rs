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
    let directory = slot_dir.join(WORKSPACE_SUBDIR);
    if !directory.is_dir() {
        // Nothing was ever hydrated here; there is nothing to push back.
        return Ok(());
    }
    let options = indexer::upload::UploadOptions {
        include_gitignored: false,
        exclude_hidden: false,
        max_file_size: MAX_FILE_SIZE,
        max_upload_concurrency: DOWNLOAD_CONCURRENCY,
        bucket: Some(archive.bucket.clone()),
        key_prefix: archive.key_prefix.clone(),
        watch: false,
        // Without this the manifest records metadata only and the archive
        // cannot be restored — the workspace would look backed up but hydrate
        // empty.
        upload_content: true,
        watch_debounce_millis: 500,
        watch_poll_millis: 60_000,
        name: workspace.to_string(),
        directory,
    };
    let keys = indexer::upload::WorkspaceKeys::new(
        indexer::keys::WorkspaceDirNameSet::new(workspace.to_string()),
        indexer::keys::WorkspaceFileUrlSet::new(
            archive.bucket.clone(),
            archive.key_prefix.clone(),
        )?,
    );
    let (names, urls) = (Default::default(), Default::default());
    indexer::upload::run(&options, client, s3, &keys, &names, &urls).await;
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
