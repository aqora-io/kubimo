//! The upload half of the indexer: walk a workspace, push it to S3, and keep
//! the `WorkspaceDirectory` CRs in step.
//!
//! Lives in the library rather than the binary so the node agent can run the
//! same pipeline for the slots it hosts, instead of a second implementation
//! drifting away from this one.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{
    FutureExt,
    stream::{StreamExt, TryStreamExt, futures_unordered::FuturesUnordered},
};
use kubimo::FilterParams;
use kubimo::{
    ResourceNameExt, Workspace, WorkspaceArchiveStatus, WorkspaceDir, WorkspaceDirContentUrl,
    WorkspaceDirDirectory, WorkspaceDirEntry, WorkspaceDirField, WorkspaceDirFile,
    WorkspaceDirMarimo, WorkspaceDirMarimoCache, WorkspaceDirSpec, WorkspaceDirSymlink,
    WorkspaceStatus, WorkspaceStorageStatus, url::Url,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncSeek},
    process::Command as Cmd,
    sync::{
        Mutex, Semaphore,
        mpsc::{Receiver, Sender, channel},
    },
    task::JoinSet,
};

use crate::disk;
use crate::fingerprint::ContentCache;
use crate::keys::{WorkspaceDirNameSet, WorkspaceFileUrlSet};
use crate::python::{Notebook, get_marimo_notebook};
use crate::s3::{CacheMarkers, DownloadError, S3Client, UploadError};
use crate::watcher::{WaitError, Watcher};

/// Everything the pipeline needs, without the binary's clap types so the node
/// agent can drive it too.
#[derive(Clone, Debug)]
pub struct UploadOptions {
    pub include_gitignored: bool,
    pub exclude_hidden: bool,
    pub max_file_size: u64,
    pub max_upload_concurrency: usize,
    pub bucket: Option<String>,
    pub key_prefix: Option<String>,
    pub watch: bool,
    pub upload_content: bool,
    /// Index a workspace whose directory is genuinely empty, overwriting its
    /// archive. Off by default: see [`empty_walk_is_safe`] for why an empty
    /// walk is otherwise refused.
    pub allow_empty: bool,
    pub watch_debounce_millis: u64,
    /// Ceiling on how long a burst of events may defer a sync.
    pub watch_max_wait_millis: u64,
    pub watch_poll_millis: u64,
    /// Name of the Workspace this directory belongs to.
    pub name: String,
    pub directory: PathBuf,
}

const CACHE_FORMATS: &[&str] = &["md", "html", "ipynb"];

#[derive(Clone)]
pub struct WorkspaceKeys {
    dir_names: Arc<Mutex<WorkspaceDirNameSet>>,
    file_urls: Arc<Mutex<WorkspaceFileUrlSet>>,
}

impl WorkspaceKeys {
    pub fn new(dir_names: WorkspaceDirNameSet, file_urls: WorkspaceFileUrlSet) -> Self {
        Self {
            dir_names: Arc::new(Mutex::new(dir_names)),
            file_urls: Arc::new(Mutex::new(file_urls)),
        }
    }

    pub async fn dir_name(&self, path: PathBuf) -> String {
        self.dir_names.lock().await.get_or_insert(path)
    }

    pub async fn file_url(&self, path: PathBuf) -> Result<Url, kubimo::url::ParseError> {
        self.file_urls.lock().await.get_or_insert(path)
    }
}

fn marimo_cache_path(path: impl AsRef<Path>, format: &str) -> Option<PathBuf> {
    let path = path.as_ref();
    let parent = path.parent()?;
    let file_name = path.file_name()?;
    Some(
        parent
            .join("__marimo__")
            .join(file_name)
            .with_extension(format),
    )
}

fn marimo_meta_path(path: impl AsRef<Path>) -> PathBuf {
    path.as_ref().with_extension("meta.json")
}

#[derive(Clone)]
pub struct WorkerOptions {
    s3: S3Client,
    directory: Arc<PathBuf>,
    max_file_size: u64,
    upload_content: bool,
    upload_permits: Arc<Semaphore>,
    keys: WorkspaceKeys,
    /// Lets an unchanged file skip being read and HEADed entirely.
    content_cache: ContentCache,
    /// Shared with the run that spawned these workers: what they could not
    /// upload is what the archive is missing, and only the run can report it.
    failures: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub struct EntryWorker {
    rx: Arc<Mutex<Receiver<PathBuf>>>,
    tx: Sender<(PathBuf, WorkspaceDirEntry)>,
    opts: WorkerOptions,
}

#[derive(Error, Debug)]
pub enum WorkerError {
    #[error(transparent)]
    Entry(#[from] ignore::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Upload(#[from] UploadError),
    #[error(transparent)]
    S3(#[from] object_store::Error),
    #[error(transparent)]
    S3Key(#[from] object_store::path::Error),
    #[error(transparent)]
    Url(#[from] kubimo::url::ParseError),
}

impl EntryWorker {
    async fn run(&self) {
        while let Some(path) = self.rx.lock().await.recv().await {
            let Some(directory) = path.parent() else {
                tracing::error!("Entry has no parent directory: {}", path.display());
                continue;
            };
            let entry = match self.process(&path).await {
                Ok(entry) => entry,
                Err(err) => {
                    // Dropping the entry does not merely leave it stale in the
                    // archive: its urls never join the live set, so the sweep
                    // deletes the objects a previous cycle uploaded for it and
                    // the manifest is rewritten as if the file did not exist.
                    self.opts.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("Error processing entry {}: {}", path.display(), err);
                    continue;
                }
            };
            if let Err(err) = self.tx.send((directory.to_path_buf(), entry)).await {
                tracing::error!("Error sending entry {}: {}", path.display(), err);
            }
        }
    }

    async fn upload(
        &self,
        path: impl AsRef<Path>,
        size: u64,
        input: impl AsyncRead + AsyncSeek + Unpin,
    ) -> Result<WorkspaceDirContentUrl, WorkerError> {
        let url = self.opts.keys.file_url(path.as_ref().to_path_buf()).await?;
        let result = self
            .opts
            .s3
            .upload(&url, input, size, &self.opts.upload_permits)
            .await?;
        Ok(WorkspaceDirContentUrl {
            url,
            crc32: Some(result.crc32),
            e_tag: result.e_tag,
        })
    }

    async fn upload_cache(
        &self,
        path: impl AsRef<Path>,
        size: u64,
    ) -> Result<WorkspaceDirContentUrl, WorkerError> {
        let path = path.as_ref();
        let full_path = self.opts.directory.join(path);
        let input = tokio::fs::File::open(&full_path).await?;
        self.upload(path, size, input).await
    }

    async fn upload_meta_json(
        &self,
        path: impl AsRef<Path>,
        notebook: Notebook,
    ) -> Result<WorkspaceDirContentUrl, WorkerError> {
        let path = path.as_ref();
        let meta_path = marimo_meta_path(path);
        let bytes = serde_json::to_vec(&notebook.meta())?;
        let size = bytes.len() as u64;
        let input = std::io::Cursor::new(bytes);
        self.upload(&meta_path, size, input).await
    }

    async fn process_marimo_cache(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Option<WorkspaceDirMarimoCache>, WorkerError> {
        let path = path.as_ref();
        let Some(format) = path.extension().and_then(OsStr::to_str) else {
            return Ok(None);
        };
        let full_path = self.opts.directory.join(path);
        if !CACHE_FORMATS.contains(&format) {
            return Ok(None);
        }
        if !tokio::fs::try_exists(&full_path).await? {
            return Ok(None);
        }
        let metadata = tokio::fs::metadata(&full_path).await?;
        if !metadata.is_file() {
            return Ok(None);
        }
        let size = metadata.len();
        if size == 0 {
            return Ok(None);
        }
        let mut out = WorkspaceDirMarimoCache {
            format: format.to_string(),
            size: Some(size),
            created: metadata.created().ok().map(Into::into),
            modified: metadata.modified().ok().map(Into::into),
            ..Default::default()
        };
        if size > self.opts.max_file_size {
            return Ok(Some(out));
        }
        match self.upload_cache(path, size).await {
            Ok(url) => {
                out.url = Some(url);
            }
            Err(err) => {
                tracing::error!("Error uploading cache {}: {}", path.display(), err);
            }
        }
        Ok(Some(out))
    }

    async fn process_marimo(
        &self,
        path: impl AsRef<Path>,
        size: u64,
    ) -> Result<Option<WorkspaceDirMarimo>, WorkerError> {
        let path = path.as_ref();
        if size > self.opts.max_file_size {
            return Ok(None);
        }
        if path.extension() != Some(OsStr::new("py")) {
            return Ok(None);
        }
        let full_path = self.opts.directory.join(path);
        let source = tokio::fs::read(&full_path).await?;
        let Some(meta) = get_marimo_notebook(source.into()) else {
            return Ok(None);
        };
        let meta_upload = {
            let worker = self.clone();
            let path = path.to_path_buf();
            tokio::spawn(async move { worker.upload_meta_json(&path, meta).await })
        };
        let mut futs = FuturesUnordered::new();
        for format in CACHE_FORMATS {
            if let Some(path) = marimo_cache_path(path, format) {
                futs.push(self.process_marimo_cache(path));
            }
        }
        let mut caches = vec![];
        while let Some(fut) = futs.next().await {
            match fut {
                Ok(Some(cache)) => {
                    caches.push(cache);
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::error!("Error processing marimo cache: {}", err);
                }
            }
        }
        caches.sort_by_key(|cache| cache.format.clone());
        let meta_json = match meta_upload.await {
            Ok(Ok(url)) => Some(url),
            Ok(Err(err)) => {
                tracing::error!(
                    "Error uploading marimo meta json for {}: {}",
                    path.display(),
                    err
                );
                None
            }
            Err(err) => {
                tracing::error!(
                    "Error joining marimo meta json upload for {}: {}",
                    path.display(),
                    err
                );
                None
            }
        };
        Ok(Some(WorkspaceDirMarimo {
            meta_json,
            caches: if caches.is_empty() {
                None
            } else {
                Some(caches)
            },
        }))
    }

    async fn process_content(
        &self,
        path: impl AsRef<Path>,
        size: u64,
    ) -> Result<Option<WorkspaceDirContentUrl>, WorkerError> {
        if !self.opts.upload_content {
            return Ok(None);
        }
        let path = path.as_ref();
        if size > self.opts.max_file_size {
            return Ok(None);
        }
        let full_path = self.opts.directory.join(path);
        if !tokio::fs::try_exists(&full_path).await? {
            return Ok(None);
        }
        let metadata = tokio::fs::metadata(&full_path).await?;
        if !metadata.is_file() {
            return Ok(None);
        }
        // The whole point of the fingerprint: without this, every filesystem
        // event re-reads every file in the workspace to compute its crc32 and
        // issues a HEAD per file, because the cache marker is only consulted
        // *after* the read.
        let modified = metadata.modified().ok();
        if let Some(cached) = self.opts.content_cache.get(path, modified, size).await {
            return Ok(Some(cached));
        }
        let uploaded = self
            .upload(path, size, tokio::fs::File::open(full_path).await?)
            .await?;
        self.opts
            .content_cache
            .insert(path.to_path_buf(), modified, size, &uploaded)
            .await;
        Ok(Some(uploaded))
    }

    async fn process_file(
        &self,
        path: impl AsRef<Path>,
        size: u64,
    ) -> Result<WorkspaceDirFile, WorkerError> {
        let path = path.as_ref();
        let (marimo, content) = futures::future::join(
            self.process_marimo(&path, size),
            self.process_content(&path, size),
        )
        .await;
        // Deliberately not counted as a failure: the file's own content is
        // uploaded either way, so the archive still restores this file. All
        // that is lost is the notebook metadata a later cycle recomputes.
        let marimo = marimo
            .inspect_err(|err| {
                tracing::error!("Error reading marimo for {}: {}", path.display(), err)
            })
            .ok()
            .flatten();
        // This one is counted. A file with no content url is absent from the
        // live url set, so the sweep deletes whatever was uploaded for it last
        // time and the manifest is rewritten without it: the archive stops
        // being a complete copy of the tree.
        let content = content
            .inspect_err(|err| {
                self.opts.failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!("Error uploading content for {}: {}", path.display(), err)
            })
            .ok()
            .flatten();
        Ok(WorkspaceDirFile {
            marimo,
            content,
            size: Some(size),
        })
    }

    async fn process(&self, path: impl AsRef<Path>) -> Result<WorkspaceDirEntry, WorkerError> {
        let path = path.as_ref();
        let file_name = if let Some(name) = path.file_name() {
            name
        } else {
            return Err(WorkerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Entry has no file name: {}", path.display()),
            )));
        };
        let full_path = self.opts.directory.join(path);
        let metadata = tokio::fs::metadata(&full_path).await?;
        let mut out = WorkspaceDirEntry {
            name: file_name.to_string_lossy().to_string(),
            created: metadata.created().ok().map(Into::into),
            modified: metadata.modified().ok().map(Into::into),
            ..Default::default()
        };
        if metadata.is_dir() {
            let name = self.opts.keys.dir_name(path.to_path_buf()).await;
            out.directory = Some(WorkspaceDirDirectory { name: Some(name) });
        }
        if metadata.is_symlink() {
            let path = tokio::fs::read_link(&full_path)
                .await
                .inspect_err(|err| {
                    tracing::error!("Error reading symlink for {}: {}", path.display(), err)
                })
                .ok()
                .map(|path| path.to_string_lossy().to_string());
            out.symlink = Some(WorkspaceDirSymlink { path });
        }
        if metadata.is_file() {
            let size = metadata.len();
            out.file = Some(self.process_file(path, size).await?);
        }
        Ok(out)
    }
}

pub struct WalkOptions {
    directory: PathBuf,
    include_gitignored: bool,
    exclude_hidden: bool,
    git_dir: Option<PathBuf>,
    /// Shared with the run that started the walk: a subtree the walk could not
    /// read is a subtree missing from the archive.
    failures: Arc<AtomicUsize>,
}

pub fn walk(join_set: &mut JoinSet<()>, options: WalkOptions, buffer: usize) -> Receiver<PathBuf> {
    let (tx, rx) = channel(buffer);
    let walker = ignore::WalkBuilder::new(&options.directory)
        .require_git(false)
        .git_ignore(!options.include_gitignored)
        .hidden(options.exclude_hidden)
        .build_parallel();
    join_set.spawn_blocking(move || {
        walker.run(|| {
            Box::new(|entry: Result<ignore::DirEntry, ignore::Error>| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        // Whatever this entry was — a file or a whole subtree —
                        // never reaches the workers, so the manifest is written
                        // as if it did not exist.
                        options.failures.fetch_add(1, Ordering::Relaxed);
                        tracing::error!("Error reading entry: {}", err);
                        return ignore::WalkState::Continue;
                    }
                };
                if let Some(git_dir) = &options.git_dir
                    && entry.path() == git_dir
                {
                    return ignore::WalkState::Skip;
                }
                if let Err(err) = tx.blocking_send(entry.into_path()) {
                    tracing::error!("Error sending entry: {}", err);
                }
                ignore::WalkState::Continue
            })
        });
    });
    rx
}

pub fn edit_paths(
    join_set: &mut JoinSet<()>,
    mut rx: Receiver<PathBuf>,
    directory: PathBuf,
    buffer: usize,
) -> (Receiver<PathBuf>, Arc<Mutex<BTreeSet<PathBuf>>>) {
    let scanned = Arc::new(Mutex::new(BTreeSet::new()));
    let (tx, new_rx) = channel(buffer);
    let out = (new_rx, scanned.clone());
    join_set.spawn(async move {
        while let Some(path) = rx.recv().await {
            scanned.lock().await.insert(path.clone());
            let path = match path.strip_prefix(&directory) {
                Ok(path) => path.to_path_buf(),
                Err(err) => {
                    tracing::error!("Error stripping prefix for {}: {}", path.display(), err);
                    continue;
                }
            };
            if path.as_os_str().is_empty() {
                continue;
            }
            if let Err(err) = tx.send(path.clone()).await {
                tracing::error!("Error sending path {}: {}", path.display(), err);
            }
        }
    });
    out
}

pub fn process(
    join_set: &mut JoinSet<()>,
    rx: Receiver<PathBuf>,
    opts: WorkerOptions,
    buffer: usize,
    workers: usize,
) -> Receiver<(PathBuf, WorkspaceDirEntry)> {
    let shared_rx = Arc::new(Mutex::new(rx));
    let (tx, rx) = channel(buffer);
    let worker = EntryWorker {
        rx: shared_rx,
        tx,
        opts,
    };
    for _ in 0..workers {
        let cloned_worker = worker.clone();
        join_set.spawn(async move {
            cloned_worker.run().await;
        });
    }
    rx
}

pub async fn process_existing_dirs(
    client: &kubimo::Client,
    name: &str,
    names: &mut WorkspaceDirNameSet,
    urls: &mut WorkspaceFileUrlSet,
    cache_markers: &mut CacheMarkers,
    previous_names: &mut BTreeSet<String>,
    previous_urls: &mut BTreeSet<Url>,
) {
    let mut workspace_dirs = client
        .api::<WorkspaceDir>()
        .list(&FilterParams::new().with_fields((WorkspaceDirField::Workspace, name)));
    while let Some(workspace_dir) = workspace_dirs.next().await {
        let workspace_dir = match workspace_dir {
            Ok(dir) => dir.item,
            Err(err) => {
                tracing::error!("Error listing workspace dirs: {}", err);
                continue;
            }
        };
        let name = match workspace_dir.name() {
            Ok(name) => name,
            Err(err) => {
                tracing::error!("Error getting workspace dir name: {}", err);
                continue;
            }
        };
        previous_names.insert(name.to_owned());
        let dir_path = PathBuf::from(&workspace_dir.spec.path);
        if let Err(err) = names.insert(dir_path.clone(), name) {
            tracing::warn!("Error inserting workspace dir name: {}", err);
            continue;
        }
        for entry in workspace_dir.spec.entries.unwrap_or_default().as_slice() {
            let path = dir_path.join(&entry.name);
            let Some(file) = &entry.file else {
                continue;
            };
            // Re-seed the *content* url first, and before the marimo check —
            // a plain file has no marimo block and would otherwise never be
            // re-seeded at all. Without this every restart mints a fresh random
            // key for the same path, re-uploads the content under it, and
            // orphans the old object forever: nothing else ever deletes it.
            if let Some(content) = &file.content {
                previous_urls.insert(content.url.clone());
                if let Err(err) = urls.insert(path.clone(), &content.url) {
                    tracing::warn!(
                        "Error inserting workspace content url for {}: {}",
                        path.display(),
                        err
                    );
                }
                if let Some(e_tag) = &content.e_tag
                    && let Some(crc32) = &content.crc32
                {
                    cache_markers.insert(content.url.clone(), *crc32, e_tag.clone());
                }
            }
            let Some(marimo) = &file.marimo else {
                continue;
            };
            if let Some(url) = &marimo.meta_json {
                let meta_path = marimo_meta_path(&path);
                previous_urls.insert(url.url.clone());
                if let Err(err) = urls.insert(meta_path.clone(), &url.url) {
                    tracing::warn!(
                        "Error inserting workspace file url for {}: {}",
                        meta_path.display(),
                        err
                    );
                }
                if let Some(e_tag) = &url.e_tag
                    && let Some(crc32) = &url.crc32
                {
                    cache_markers.insert(url.url.clone(), *crc32, e_tag.clone());
                }
            }
            let Some(caches) = &marimo.caches else {
                continue;
            };
            for cache in caches {
                let cache_path = match marimo_cache_path(&path, &cache.format) {
                    Some(path) => path,
                    None => {
                        tracing::error!(
                            "Error getting marimo cache path for {}: {}",
                            path.display(),
                            cache.format
                        );
                        continue;
                    }
                };
                if let Some(url) = &cache.url {
                    previous_urls.insert(url.url.clone());
                    if let Err(err) = urls.insert(cache_path.clone(), &url.url) {
                        tracing::error!(
                            "Error inserting workspace file url for {}: {}",
                            cache_path.display(),
                            err
                        );
                    }
                    if let Some(e_tag) = &url.e_tag
                        && let Some(crc32) = &url.crc32
                    {
                        cache_markers.insert(url.url.clone(), *crc32, e_tag.clone());
                    }
                }
            }
        }
    }
}

async fn clean_url(client: &S3Client, url: Url) {
    if let Err(err) = client.delete(&url).await {
        tracing::error!("Error deleting object at {}: {}", url, err);
    } else {
        tracing::info!("Deleted object at {}", url);
    }
}

async fn clean_workspace_dir(client: &kubimo::Client, name: String) {
    if let Err(err) = client.api::<WorkspaceDir>().delete(&name).await {
        tracing::error!("Error deleting workspace dir {}: {}", name, err);
    } else {
        tracing::info!("Deleted workspace dir {}", name);
    }
}

/// Purge everything a deleted workspace left behind: its `WorkspaceDirectory`
/// CRs and every object they name, plus the archive manifest.
///
/// The manifest is passed in rather than discovered because its key is built
/// from the bucket and prefix alone, never from the CRs. Leaving it behind is
/// not merely litter: a workspace recreated at the same fixed `keyPrefix` finds
/// that manifest, reads it as proof that an archive exists, and refuses to
/// index itself until a file appears.
///
/// Known limitation: the sweep is driven by the CRs, so an archive whose CRs
/// are already gone is missed entirely. Deleting by prefix instead would need a
/// list API that `S3Client` does not have.
pub async fn clean(
    client: &kubimo::Client,
    s3: &S3Client,
    name: &str,
    bucket: Option<&str>,
    key_prefix: Option<&str>,
) {
    let mut workspace_dirs = client
        .api::<WorkspaceDir>()
        .list(&FilterParams::new().with_fields((WorkspaceDirField::Workspace, name)));
    let futs = FuturesUnordered::new();
    if let Some(bucket) = bucket {
        match kubimo::manifest_url(bucket, key_prefix) {
            Ok(url) => futs.push(clean_url(s3, url).boxed()),
            Err(err) => tracing::error!("Error building manifest url: {err}"),
        }
    }
    while let Some(workspace_dir) = workspace_dirs.next().await {
        let workspace_dir = match workspace_dir {
            Ok(dir) => dir.item,
            Err(err) => {
                tracing::error!("Error listing workspace dirs: {}", err);
                continue;
            }
        };
        match workspace_dir.name() {
            Ok(name) => {
                futs.push(clean_workspace_dir(client, name.to_owned()).boxed());
            }
            Err(err) => {
                tracing::error!("Error getting workspace dir name: {}", err);
            }
        }
        for entry in workspace_dir.spec.entries.unwrap_or_default().as_slice() {
            let Some(file) = &entry.file else {
                continue;
            };
            // Delete the file's content too. `clean` used to remove only the
            // marimo meta/cache objects, so deleting a workspace left every
            // uploaded file behind in the bucket forever.
            if let Some(content) = &file.content {
                futs.push(clean_url(s3, content.url.clone()).boxed());
            }
            let Some(marimo) = &file.marimo else {
                continue;
            };
            if let Some(url) = &marimo.meta_json {
                futs.push(clean_url(s3, url.url.clone()).boxed());
            }
            let Some(caches) = &marimo.caches else {
                continue;
            };
            for cache in caches {
                if let Some(url) = &cache.url {
                    futs.push(clean_url(s3, url.url.clone()).boxed());
                }
            }
        }
    }
    futs.collect::<()>().await;
}

#[derive(Debug, Error)]
enum GitDirError {
    #[error("git command could not run: {0}")]
    Command(std::io::Error),
    #[error("git command failed {0}: {1}")]
    Status(std::process::ExitStatus, String),
    #[error("could not canonicalize path {0}: {1}")]
    Canonicalize(PathBuf, std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("git dir is empty")]
    EmptyOutput,
    #[error("git dir is not relative: {0}")]
    NotRelative(PathBuf),
}

async fn get_relative_git_dir(dir: impl AsRef<Path>) -> Result<PathBuf, GitDirError> {
    let abs_dir = dir
        .as_ref()
        .canonicalize()
        .map_err(|err| GitDirError::Canonicalize(dir.as_ref().to_path_buf(), err))?;
    let git_dir = match Cmd::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(&abs_dir)
        .output()
        .await
    {
        Ok(output) => {
            if output.status.success() {
                let Some(path) = String::from_utf8(output.stdout)?
                    .lines()
                    .next()
                    .map(PathBuf::from)
                else {
                    return Err(GitDirError::EmptyOutput);
                };
                path.canonicalize()
                    .map_err(|err| GitDirError::Canonicalize(path.to_path_buf(), err))?
            } else {
                return Err(GitDirError::Status(
                    output.status,
                    String::from_utf8_lossy(&output.stderr).into(),
                ));
            }
        }
        Err(err) => {
            return Err(GitDirError::Command(err));
        }
    };
    let relative = git_dir
        .strip_prefix(&abs_dir)
        .map(|path| path.to_path_buf())
        .map_err(|_| GitDirError::NotRelative(git_dir))?;
    Ok(dir.as_ref().join(relative))
}

/// `#[must_use]` because dropping this is how a caller silently promotes a
/// half-written archive to a successful one.
#[must_use]
#[derive(Debug, Default)]
pub struct RunResult {
    names: BTreeSet<String>,
    urls: BTreeSet<Url>,
    paths: BTreeSet<PathBuf>,
    /// The cycle refused to touch the archive because the walk came back empty.
    /// One-shot runs turn this into a non-zero exit; the watcher keeps going.
    pub refused: bool,
    /// Operations that failed in this cycle, so the archive no longer fully
    /// represents the tree. Callers that treat a successful upload as a
    /// durability boundary must not do so when this is non-zero.
    pub failures: usize,
    /// Total size of the files this cycle tracked — the archive's contents as
    /// the walk saw them, not the bytes transferred, since unchanged files are
    /// served from the fingerprint cache. This is what a pooled workspace
    /// reports as `status.archive.totalContentBytes`, and it is deliberately
    /// far smaller than `status.storage.used`: the venv, gitignored files and
    /// everything outside the workspace directory are never walked.
    pub content_bytes: u64,
}

/// What the archive looked like before this cycle, as far as S3 can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestProbe {
    /// No manifest under this prefix — the workspace has never been indexed.
    Absent,
    /// A manifest that records no directories.
    Empty,
    /// A manifest with directories in it: there is an archive here.
    NonEmpty,
    /// The manifest could not be fetched or parsed. Treated as "there might be
    /// an archive here", because we cannot prove otherwise.
    Unreadable,
}

/// May a cycle whose walk produced nothing go on to rewrite the archive?
///
/// The clean sweep is `previous − current`, so a walk that yields nothing marks
/// *everything* stale: every `WorkspaceDirectory` CR is deleted, every object
/// under the prefix is deleted, and `upload_manifest` writes an empty manifest
/// first. An unmounted volume, a hydration that silently produced nothing, and
/// a user who genuinely deleted every file are indistinguishable from here —
/// and only one of the three is recoverable.
///
/// So an empty walk may only proceed when there is demonstrably nothing to
/// lose. Checking the `WorkspaceDirectory` CRs alone is not enough: a workspace
/// can have an S3 archive and no CRs (production has several), and for those
/// `previous` is empty while the manifest is not. Hence the probe.
fn empty_walk_is_safe(
    previous_names: &BTreeSet<String>,
    previous_urls: &BTreeSet<Url>,
    manifest: ManifestProbe,
) -> bool {
    if !previous_names.is_empty() || !previous_urls.is_empty() {
        return false;
    }
    // `Absent` is the never-indexed workspace writing its first manifest, which
    // must keep working. `Unreadable` fails closed: an archive we cannot read is
    // exactly the one we least want to overwrite.
    matches!(manifest, ManifestProbe::Absent | ManifestProbe::Empty)
}

/// Fetch the current manifest to find out whether an archive exists. Only
/// called when the walk produced nothing, so it costs nothing on the hot path.
async fn probe_manifest(args: &UploadOptions, s3: &S3Client) -> ManifestProbe {
    let Some(bucket) = args.bucket.as_deref() else {
        // No bucket configured means no archive to destroy; the CR check alone
        // decides.
        return ManifestProbe::Absent;
    };
    let url = match kubimo::manifest_url(bucket, args.key_prefix.as_deref()) {
        Ok(url) => url,
        Err(err) => {
            tracing::error!("Error building manifest url: {err}");
            return ManifestProbe::Unreadable;
        }
    };
    match s3.get_bytes(&url).await {
        Ok(bytes) => match serde_json::from_slice::<kubimo::WorkspaceManifest>(&bytes) {
            Ok(manifest) if manifest.directories.is_empty() => ManifestProbe::Empty,
            Ok(_) => ManifestProbe::NonEmpty,
            Err(err) => {
                tracing::error!("Could not parse manifest at {url}: {err}");
                ManifestProbe::Unreadable
            }
        },
        Err(DownloadError::S3(object_store::Error::NotFound { .. })) => ManifestProbe::Absent,
        Err(err) => {
            tracing::error!("Could not read manifest at {url}: {err}");
            ManifestProbe::Unreadable
        }
    }
}

/// Materialize the watch set, which is shared with the walk workers.
async fn take_paths(paths: Arc<Mutex<BTreeSet<PathBuf>>>) -> BTreeSet<PathBuf> {
    match Arc::try_unwrap(paths) {
        Ok(paths) => paths.into_inner(),
        Err(paths) => {
            tracing::warn!("Error getting paths ownership: {:?}", paths);
            paths.lock().await.clone()
        }
    }
}

pub async fn run(
    args: &UploadOptions,
    content_cache: &ContentCache,
    client: &kubimo::Client,
    s3: &S3Client,
    keys: &WorkspaceKeys,
    previous_names: &BTreeSet<String>,
    previous_urls: &BTreeSet<Url>,
) -> RunResult {
    let git_dir = match get_relative_git_dir(&args.directory).await {
        Ok(git_dir) => Some(git_dir),
        // `git` is simply absent from the marimo image, so this fires on every
        // upload cycle — once or twice a second while a workspace is active.
        // That is a property of the image, not a problem to report, and at warn
        // level it drowns out the events that are. A workspace with no git dir
        // is likewise ordinary. Anything else is still worth seeing.
        Err(GitDirError::Command(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("No git available; skipping git metadata");
            None
        }
        Err(err @ GitDirError::Status(..)) => {
            tracing::debug!("Not a git repository; skipping git metadata: {err}");
            None
        }
        Err(err) => {
            tracing::warn!("Could not get git dir: {}", err);
            None
        }
    };

    // One counter for the whole cycle, shared with the walk and the workers:
    // every path that increments it is a path where the archive ends up
    // narrower than the tree on disk.
    let failures = Arc::new(AtomicUsize::new(0));

    let mut join_set = JoinSet::new();
    let rx = walk(
        &mut join_set,
        WalkOptions {
            directory: args.directory.clone(),
            include_gitignored: args.include_gitignored,
            exclude_hidden: args.exclude_hidden,
            git_dir,
            failures: failures.clone(),
        },
        1000,
    );
    let (rx, paths) = edit_paths(&mut join_set, rx, args.directory.clone(), 1000);
    let upload_permits = Arc::new(Semaphore::new(args.max_upload_concurrency));
    let mut rx = process(
        &mut join_set,
        rx,
        WorkerOptions {
            content_cache: content_cache.clone(),
            s3: s3.clone(),
            directory: Arc::new(args.directory.clone()),
            max_file_size: args.max_file_size,
            upload_content: args.upload_content,
            upload_permits: upload_permits.clone(),
            keys: keys.clone(),
            failures: failures.clone(),
        },
        1000,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2),
    );

    let mut urls = BTreeSet::new();
    let mut workspace_dirs = BTreeMap::new();
    let mut content_bytes = 0u64;
    while let Some((path, entry)) = rx.recv().await {
        let name = keys.dir_name(path.clone()).await;
        content_bytes = content_bytes
            .saturating_add(entry.file.as_ref().and_then(|file| file.size).unwrap_or(0));
        // Content urls belong in the live set too: `previous_urls` minus this
        // is what gets deleted from S3, so leaving content out meant a removed
        // or renamed file's object was never swept.
        if let Some(content) = entry.file.as_ref().and_then(|file| file.content.as_ref()) {
            urls.insert(content.url.clone());
        }
        if let Some(marimo) = entry.file.as_ref().and_then(|file| file.marimo.as_ref()) {
            if let Some(url) = marimo.meta_json.as_ref() {
                urls.insert(url.url.clone());
            }
            if let Some(caches) = marimo.caches.as_ref() {
                for url in caches {
                    if let Some(url) = &url.url {
                        urls.insert(url.url.clone());
                    }
                }
            }
        }
        workspace_dirs
            .entry(name.clone())
            .or_insert_with(|| {
                WorkspaceDir::new(
                    &name,
                    WorkspaceDirSpec {
                        workspace: args.name.clone(),
                        path: path.to_string_lossy().to_string(),
                        ..Default::default()
                    },
                )
            })
            .spec
            .entries
            .get_or_insert_default()
            .push(entry);
    }
    let names = workspace_dirs.keys().cloned().collect::<BTreeSet<_>>();
    if names.is_empty()
        && !args.allow_empty
        && !empty_walk_is_safe(
            previous_names,
            previous_urls,
            probe_manifest(args, s3).await,
        )
    {
        tracing::error!(
            workspace = %args.name,
            directory = %args.directory.display(),
            previous_dirs = previous_names.len(),
            previous_objects = previous_urls.len(),
            "Walk produced no entries but this workspace has an archive; refusing to \
             delete it. Pass --allow-empty to index a workspace that is genuinely empty."
        );
        return RunResult {
            // Hand the *previous* sets straight back. Returning the empty walked
            // sets would disarm the guard: with nothing recorded as previous, the
            // next cycle sails through and overwrites the archive after all.
            names: previous_names.clone(),
            urls: previous_urls.clone(),
            paths: take_paths(paths).await,
            refused: true,
            failures: failures.load(Ordering::Relaxed),
            content_bytes,
        };
    }
    let names_to_delete = previous_names
        .difference(&names)
        .cloned()
        .collect::<BTreeSet<_>>();
    let urls_to_delete = previous_urls
        .difference(&urls)
        .cloned()
        .collect::<BTreeSet<_>>();

    // Upload the manifest before deleting stale objects so a concurrent
    // reader never holds a manifest referencing objects deleted by this
    // batch. The manifest url is never part of the stale-url bookkeeping.
    let mut manifest_uploaded = false;
    if let Some(bucket) = args.bucket.as_deref() {
        manifest_uploaded =
            upload_manifest(args, s3, bucket, &workspace_dirs, &upload_permits).await;
        if !manifest_uploaded {
            // Without a manifest the archive cannot be restored at all, however
            // many objects reached the bucket.
            failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    let futs = FuturesUnordered::new();
    for mut dir in workspace_dirs.into_values() {
        let bmowds = client.api::<WorkspaceDir>();
        let failures = failures.clone();
        futs.push(tokio::spawn(async move {
            if let Some(entries) = dir.spec.entries.as_mut() {
                entries.sort_by_key(|entry| entry.name.clone())
            }
            let path = &dir.spec.path;
            let name = dir.name().unwrap_or_default();
            match bmowds.patch(&dir).await {
                Ok(_) => tracing::info!("Patched workspace dir {name} [{path}]"),
                Err(err) => {
                    // The CRs are how the next cycle recovers this directory's
                    // key layout, so one that never lands leaves the archive
                    // and the cluster's view of it out of step.
                    failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("Error creating workspace dir {name} [{path}]: {err}")
                }
            }
        }));
    }
    // Neither sweep is counted as a failure: a stale CR or a stale object that
    // survives is a leak, and the next cycle tries again. Nothing the tenant
    // wrote is lost by it.
    for name in names_to_delete {
        let bmowds = client.api::<WorkspaceDir>();
        futs.push(tokio::spawn(async move {
            match bmowds.delete(&name).await {
                Ok(_) => tracing::info!("Deleted workspace dir {name}"),
                Err(err) => tracing::error!("Error deleting workspace dir {name}: {err}"),
            }
        }));
    }
    for url in urls_to_delete {
        let cloned_s3 = s3.clone();
        futs.push(tokio::spawn(async move {
            match cloned_s3.delete(&url).await {
                Ok(_) => tracing::info!("Deleted object at {}", url),
                Err(err) => tracing::error!("Error deleting object at {}: {err}", url),
            }
        }));
    }
    if let Err(err) = futs.try_collect::<()>().await {
        tracing::error!("Error waiting for tasks: {}", err);
    }
    // A sync is only claimed when the manifest landed *and* nothing in this batch
    // was left behind. A manifest naming an object whose upload failed describes
    // an archive that cannot be restored, and saying it synced would make an
    // incomplete archive look durable — the one claim this field exists to make
    // honestly.
    let synced_content_bytes =
        (manifest_uploaded && failures.load(Ordering::Relaxed) == 0).then_some(content_bytes);
    update_workspace_status(args, client, synced_content_bytes).await;
    let paths = take_paths(paths).await;
    // Drop fingerprints for files that no longer exist, so a watcher running
    // for days does not accumulate an entry per file ever seen. `paths` holds
    // absolute paths (it doubles as the watch set) while the cache is keyed
    // workspace-relative, hence the strip.
    let live: BTreeSet<PathBuf> = paths
        .iter()
        .filter_map(|path| path.strip_prefix(args.directory.as_path()).ok())
        .map(Path::to_path_buf)
        .collect();
    content_cache.retain_paths(&live).await;
    RunResult {
        names,
        urls,
        paths,
        refused: false,
        failures: failures.load(Ordering::Relaxed),
        content_bytes,
    }
}

/// Build and upload the archive manifest for the current batch. Best-effort:
/// failures are logged and never abort indexing — the next batch rewrites it.
/// Note the manifest reflects whatever the walk produced: a partially failed
/// walk shrinks the manifest until a later batch repairs it (same semantics
/// as the `WorkspaceDirectory` CR sweep).
///
/// Returns whether the manifest reached the bucket, because "the next batch
/// rewrites it" is only true while there is a next batch — a one-shot flush
/// has none, and restoring needs this object.
async fn upload_manifest(
    args: &UploadOptions,
    s3: &S3Client,
    bucket: &str,
    workspace_dirs: &BTreeMap<String, WorkspaceDir>,
    upload_permits: &Semaphore,
) -> bool {
    let manifest = kubimo::build_manifest(&args.name, args.upload_content, workspace_dirs);
    let url = match kubimo::manifest_url(bucket, args.key_prefix.as_deref()) {
        Ok(url) => url,
        Err(err) => {
            tracing::error!("Error building manifest url: {err}");
            return false;
        }
    };
    let bytes = match serde_json::to_vec(&manifest) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!("Error serializing manifest: {err}");
            return false;
        }
    };
    let size = bytes.len() as u64;
    let input = std::io::Cursor::new(bytes);
    match s3.upload(&url, input, size, upload_permits).await {
        Ok(_) => {
            tracing::info!("Uploaded manifest to {url}");
            true
        }
        Err(err) => {
            tracing::error!("Error uploading manifest to {url}: {err}");
            false
        }
    }
}

/// Publish what this batch established to the Workspace's status: how much space
/// the volume is using, and — when the batch completed cleanly — that its content
/// reached S3.
///
/// `status.archive.lastSyncedAt` is written here, on every batch that lands, rather
/// than only when the last runner unmounts. The unmount flush is a real durability
/// boundary but a rare one: the platform gives every workspace both an Edit and a
/// Run runner, and the flush is deferred while any sibling volume still mounts the
/// slot, so the field sat at the value it was given minutes after creation while the
/// watcher archived the workspace continuously for hours. A staleness signal that
/// lags reality by days is worse than none, because it reads as "never persisted".
///
/// Best-effort: failures are logged and never abort indexing.
async fn update_workspace_status(
    args: &UploadOptions,
    client: &kubimo::Client,
    synced_content_bytes: Option<u64>,
) {
    // Losing disk usage must not cost the sync record: they describe different
    // things and only one of them is a durability claim.
    let usage = match disk::disk_usage(&args.directory) {
        Ok(usage) => Some(usage),
        Err(err) => {
            tracing::error!(
                "Could not determine disk usage for {:?}: {err}",
                args.directory
            );
            None
        }
    };
    if usage.is_none() && synced_content_bytes.is_none() {
        return;
    }
    let mut workspace = Workspace::new(&args.name, Default::default());
    workspace.status = Some(WorkspaceStatus {
        storage: usage.map(|usage| WorkspaceStorageStatus {
            used: Some(disk::storage_quantity(usage.used)),
            capacity: Some(disk::storage_quantity(usage.capacity)),
            available: Some(disk::storage_quantity(usage.available)),
        }),
        // `keyPrefix` is deliberately absent: the agent owns it, and an apply owns
        // exactly the fields it carries, so naming it here would take it over and
        // then drop it on the first batch that has nothing to say.
        archive: synced_content_bytes.map(|content_bytes| WorkspaceArchiveStatus {
            last_synced_at: Some(kubimo::chrono::Utc::now()),
            total_content_bytes: Some(content_bytes),
            ..Default::default()
        }),
        ..Default::default()
    });
    if let Err(err) = client.api::<Workspace>().patch_status(&workspace).await {
        tracing::error!("Failed to update workspace status: {err}");
    } else {
        tracing::info!(
            "Updated workspace {} status: {:?} bytes used, synced {:?}",
            args.name,
            usage.map(|usage| usage.used),
            synced_content_bytes
        );
    }
}

pub async fn watch(
    args: &UploadOptions,
    client: &kubimo::Client,
    s3: &S3Client,
    keys: &WorkspaceKeys,
    mut previous_names: BTreeSet<String>,
    mut previous_urls: BTreeSet<Url>,
) {
    // Lives for the whole watch, which is what makes the fingerprint useful:
    // a per-run cache would be empty on every event and skip nothing.
    let content_cache = ContentCache::new();
    let mut watcher = Watcher::new(
        Duration::from_millis(args.watch_debounce_millis),
        Duration::from_millis(args.watch_max_wait_millis),
        Duration::from_millis(args.watch_poll_millis),
    )
    .expect("Could not create watcher");
    loop {
        let res = run(
            args,
            &content_cache,
            client,
            s3,
            keys,
            &previous_names,
            &previous_urls,
        )
        .await;
        if let Err(err) = watcher.watch(res.paths) {
            tracing::error!("Error watching paths: {err}");
        }
        previous_names = res.names;
        previous_urls = res.urls;
        match watcher.wait().await {
            Ok(()) => {}
            Err(WaitError::Closed) => {
                watcher = Watcher::new(
                    Duration::from_millis(args.watch_debounce_millis),
                    Duration::from_millis(args.watch_max_wait_millis),
                    Duration::from_millis(args.watch_poll_millis),
                )
                .expect("Could not create watcher");
            }
            Err(WaitError::CtrlC) => {
                break;
            }
            Err(WaitError::CtrlCError(err)) => {
                tracing::error!("Error setting Ctrl-C handler: {err}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The optimisation only pays off if the fingerprint survives *between*
    /// runs. `watch` owns one cache for its whole lifetime; a per-run cache
    /// would be empty on every event and skip nothing, which is the bug this
    /// guards against.
    #[tokio::test]
    async fn a_cache_hit_avoids_re_reading_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notebook.py");
        std::fs::write(&file, b"import marimo").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let modified = meta.modified().ok();
        let size = meta.len();

        let cache = ContentCache::new();
        let relative = std::path::PathBuf::from("notebook.py");
        let content = WorkspaceDirContentUrl {
            url: "s3://bucket/abc".parse().unwrap(),
            crc32: Some(1),
            e_tag: Some("e".into()),
        };

        // First pass: nothing cached, so the caller would read and upload.
        assert!(cache.get(&relative, modified, size).await.is_none());
        cache
            .insert(relative.clone(), modified, size, &content)
            .await;

        // Second pass over an untouched file: served from the fingerprint, so
        // no read and no HEAD.
        let hit = cache.get(&relative, modified, size).await;
        assert!(hit.is_some(), "unchanged file should hit the cache");
        assert_eq!(hit.unwrap().url.to_string(), "s3://bucket/abc");

        // Touching it invalidates. The timestamp is the caller's to supply —
        // `process_content` passes the one the walk already stat'd — so a
        // synthetic bump stands in for waiting out the filesystem's mtime
        // resolution.
        std::fs::write(&file, b"import marimo  # edited").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let touched = meta.modified().ok().map(|at| at + Duration::from_secs(1));
        assert!(
            cache.get(&relative, touched, meta.len()).await.is_none(),
            "an edited file must miss"
        );
    }

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn urls(items: &[&str]) -> BTreeSet<Url> {
        items.iter().map(|s| s.parse().unwrap()).collect()
    }

    /// The ordinary shape of the disaster: a workspace that has been indexed
    /// before, whose volume is now unmounted. The walk returns nothing, and
    /// without this check every CR and every object would be swept as stale.
    #[test]
    fn an_empty_walk_is_refused_when_directory_crs_exist() {
        assert!(!empty_walk_is_safe(
            &names(&["bmowd-abc"]),
            &BTreeSet::new(),
            ManifestProbe::Absent,
        ));
    }

    /// Same disaster reached from the other side: content objects are known
    /// even though no directory CR is.
    #[test]
    fn an_empty_walk_is_refused_when_content_urls_exist() {
        assert!(!empty_walk_is_safe(
            &BTreeSet::new(),
            &urls(&["s3://bucket/prefix/abc"]),
            ManifestProbe::Absent,
        ));
    }

    /// The case a CR-only check misses. A workspace can have an S3 archive and
    /// no `WorkspaceDirectory` CRs at all — production has several, and they
    /// are precisely the ones a mass re-index touches. `previous` is empty for
    /// them, so only the manifest reveals that there is something to lose.
    #[test]
    fn an_empty_walk_is_refused_when_the_manifest_has_directories() {
        assert!(!empty_walk_is_safe(
            &BTreeSet::new(),
            &BTreeSet::new(),
            ManifestProbe::NonEmpty,
        ));
    }

    /// The guard must not break the never-indexed workspace writing its first,
    /// legitimately empty manifest — that is how a freshly created workspace
    /// gets an archive at all.
    #[test]
    fn a_never_indexed_workspace_may_still_write_its_first_empty_manifest() {
        assert!(empty_walk_is_safe(
            &BTreeSet::new(),
            &BTreeSet::new(),
            ManifestProbe::Absent,
        ));
        // Re-indexing a workspace that is still empty is likewise a no-op, not
        // a refusal.
        assert!(empty_walk_is_safe(
            &BTreeSet::new(),
            &BTreeSet::new(),
            ManifestProbe::Empty,
        ));
    }

    /// Credentials expiring or the network faltering must not read as "there
    /// is no archive here". Failing open would make a transient S3 error
    /// delete a workspace.
    #[test]
    fn an_unreadable_manifest_fails_closed() {
        assert!(!empty_walk_is_safe(
            &BTreeSet::new(),
            &BTreeSet::new(),
            ManifestProbe::Unreadable,
        ));
    }

    /// The whole hazard rests on `edit_paths` dropping the root: `walk` does
    /// emit the directory itself, and the strip-to-empty check at the top of
    /// `edit_paths` is what turns an empty workspace into *zero* downstream
    /// entries rather than one. That is what makes `names` empty, which is what
    /// makes the clean sweep delete everything. Pin it, or a future change to
    /// that skip silently re-arms the destruction.
    #[tokio::test]
    async fn an_empty_directory_yields_no_indexable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut join_set = JoinSet::new();
        let rx = walk(
            &mut join_set,
            WalkOptions {
                directory: dir.path().to_path_buf(),
                include_gitignored: false,
                exclude_hidden: false,
                git_dir: None,
                failures: Arc::new(AtomicUsize::new(0)),
            },
            16,
        );
        let (mut rx, _scanned) = edit_paths(&mut join_set, rx, dir.path().to_path_buf(), 16);
        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(
            count, 0,
            "an empty directory must yield no indexable paths, only the root that edit_paths drops"
        );
    }

    /// A client whose every request fails, without touching the network.
    ///
    /// Building one from a `Config` is not an option here: that constructs a
    /// TLS stack eagerly, which panics in a workspace-wide test run where more
    /// than one rustls crypto provider is compiled in.
    fn offline_client() -> kubimo::Client {
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

    /// No bucket and no content upload: nothing in such a run touches S3, so
    /// the only things that can fail are local and the writes to the cluster.
    fn offline_options(directory: &Path) -> UploadOptions {
        UploadOptions {
            include_gitignored: false,
            exclude_hidden: false,
            max_file_size: 1024,
            max_upload_concurrency: 1,
            bucket: None,
            key_prefix: None,
            watch: false,
            upload_content: false,
            allow_empty: false,
            watch_debounce_millis: 0,
            watch_max_wait_millis: 0,
            watch_poll_millis: 0,
            name: "bmow-abc".to_string(),
            directory: directory.to_path_buf(),
        }
    }

    /// A cycle that could not write everything it walked must say so. The node
    /// agent's flush treats a clean `run` as permission to evict the slot —
    /// the only remaining copy of the tenant's newest work — so a cycle that
    /// reported nothing would trade that copy for a partial archive.
    ///
    /// Driven through the `WorkspaceDirectory` patch: every request this client
    /// makes fails, so the patch fails without needing a cluster. The storage
    /// status patch fails on the same client and is deliberately *not*
    /// counted, which is what pins the count at one.
    #[tokio::test]
    async fn a_cycle_that_could_not_write_a_directory_reports_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notebook.py"), b"import marimo").unwrap();
        let client = offline_client();
        let options = offline_options(dir.path());
        let keys = WorkspaceKeys::new(
            WorkspaceDirNameSet::new("bmow-abc".to_string()),
            WorkspaceFileUrlSet::new("bucket".to_string(), None).unwrap(),
        );
        let result = run(
            &options,
            &ContentCache::new(),
            &client,
            &S3Client::from_env(),
            &keys,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .await;
        assert!(
            !result.refused,
            "the walk found a file, so nothing to refuse"
        );
        assert_eq!(
            result.failures, 1,
            "the unwritable directory CR must be reported"
        );
    }

    /// An entry the workers could not process is not a gap in the archive that
    /// heals itself: it is absent from the live url set, which is exactly what
    /// marks its previously uploaded objects stale.
    ///
    /// A dangling symlink is the cheap way to reach that branch — the walk
    /// happily yields it, and the `metadata` call in `process` follows it and
    /// fails. It is the only entry, so nothing else in the cycle can fail: no
    /// bucket, and no directory CR to write.
    #[tokio::test]
    async fn a_dropped_entry_reports_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone.py"), dir.path().join("dangling.py"))
            .unwrap();
        let keys = WorkspaceKeys::new(
            WorkspaceDirNameSet::new("bmow-abc".to_string()),
            WorkspaceFileUrlSet::new("bucket".to_string(), None).unwrap(),
        );
        let result = run(
            &offline_options(dir.path()),
            &ContentCache::new(),
            &offline_client(),
            &S3Client::from_env(),
            &keys,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .await;
        assert_eq!(
            result.failures, 1,
            "the entry that never reached a WorkspaceDirectory must be reported"
        );
    }
}
