mod keys;
mod python;
mod s3;
mod watcher;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use keys::{WorkspaceDirNameSet, WorkspaceFileUrlSet};
use kubimo::FilterParams;
use s3::{CacheMarkers, S3Client, UploadError};
use watcher::{WaitError, Watcher};

use clap::Parser;
use futures::stream::{StreamExt, TryStreamExt, futures_unordered::FuturesUnordered};
use kubimo::{
    WorkspaceDir, WorkspaceDirContentUrl, WorkspaceDirDirectory, WorkspaceDirEntry,
    WorkspaceDirField, WorkspaceDirFile, WorkspaceDirMarimo, WorkspaceDirMarimoCache,
    WorkspaceDirSpec, WorkspaceDirSymlink, prelude::*, url::Url,
};
use python::{Notebook, get_marimo_notebook};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncSeek},
    process::Command,
    sync::{
        Mutex, Semaphore,
        mpsc::{Receiver, Sender, channel},
    },
    task::JoinSet,
};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, prelude::*};

const CACHE_FORMATS: &[&str] = &["md", "html", "ipynb"];

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, short = 'i')]
    include_gitignored: bool,
    #[arg(long, short = 'k')]
    exclude_hidden: bool,
    #[arg(long, default_value_t = 100 * 1024 * 1024)] // 100 MB
    max_file_size: u64,
    #[arg(long, default_value_t = 10)]
    max_upload_concurrency: usize,
    #[arg(long, short, env = "AWS_BUCKET")]
    bucket: Option<String>,
    #[arg(long, short = 'p', env = "AWS_KEY_PREFIX")]
    key_prefix: Option<String>,
    #[arg(long, short)]
    watch: bool,
    #[arg(long, short)]
    upload_content: bool,
    #[arg(long, default_value_t = 500)]
    watch_debounce_millis: u64,
    #[arg(long, default_value_t = 60 * 1000)]
    watch_poll_millis: u64,
    name: String,
    #[arg(default_value = ".")]
    directory: PathBuf,
}

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
        self.upload(path, size, tokio::fs::File::open(full_path).await?)
            .await
            .map(Some)
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
        let marimo = marimo
            .inspect_err(|err| {
                tracing::error!("Error reading marimo for {}: {}", path.display(), err)
            })
            .ok()
            .flatten();
        let content = content
            .inspect_err(|err| {
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

async fn process_existing_dirs(
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
    let git_dir = match Command::new("git")
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

#[derive(Debug, Default)]
struct RunResult {
    names: BTreeSet<String>,
    urls: BTreeSet<Url>,
    paths: BTreeSet<PathBuf>,
}

async fn run(
    args: &Args,
    client: &kubimo::Client,
    s3: &S3Client,
    keys: &WorkspaceKeys,
    previous_names: &BTreeSet<String>,
    previous_urls: &BTreeSet<Url>,
) -> RunResult {
    let git_dir = match get_relative_git_dir(&args.directory).await {
        Ok(git_dir) => Some(git_dir),
        Err(err) => {
            tracing::warn!("Could not get git dir: {}", err);
            None
        }
    };

    let mut join_set = JoinSet::new();
    let rx = walk(
        &mut join_set,
        WalkOptions {
            directory: args.directory.clone(),
            include_gitignored: args.include_gitignored,
            exclude_hidden: args.exclude_hidden,
            git_dir,
        },
        1000,
    );
    let (rx, paths) = edit_paths(&mut join_set, rx, args.directory.clone(), 1000);
    let mut rx = process(
        &mut join_set,
        rx,
        WorkerOptions {
            s3: s3.clone(),
            directory: Arc::new(args.directory.clone()),
            max_file_size: args.max_file_size,
            upload_content: args.upload_content,
            upload_permits: Arc::new(Semaphore::new(args.max_upload_concurrency)),
            keys: keys.clone(),
        },
        1000,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2),
    );

    let mut urls = BTreeSet::new();
    let mut workspace_dirs = BTreeMap::new();
    while let Some((path, entry)) = rx.recv().await {
        let name = keys.dir_name(path.clone()).await;
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
    let names_to_delete = previous_names
        .difference(&names)
        .cloned()
        .collect::<BTreeSet<_>>();
    let urls_to_delete = previous_urls
        .difference(&urls)
        .cloned()
        .collect::<BTreeSet<_>>();

    let futs = FuturesUnordered::new();
    for mut dir in workspace_dirs.into_values() {
        let bmowds = client.api::<WorkspaceDir>();
        futs.push(tokio::spawn(async move {
            if let Some(entries) = dir.spec.entries.as_mut() {
                entries.sort_by_key(|entry| entry.name.clone())
            }
            let path = &dir.spec.path;
            let name = dir.name().unwrap_or_default();
            match bmowds.patch(&dir).await {
                Ok(_) => tracing::info!("Patched workspace dir {name} [{path}]"),
                Err(err) => tracing::error!("Error creating workspace dir {name} [{path}]: {err}"),
            }
        }));
    }
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
    let paths = match Arc::try_unwrap(paths) {
        Ok(paths) => paths.into_inner(),
        Err(paths) => {
            tracing::warn!("Error getting paths ownership: {:?}", paths);
            paths.lock().await.clone()
        }
    };
    RunResult { names, urls, paths }
}

async fn watch(
    args: &Args,
    client: &kubimo::Client,
    s3: &S3Client,
    keys: &WorkspaceKeys,
    mut previous_names: BTreeSet<String>,
    mut previous_urls: BTreeSet<Url>,
) {
    let mut watcher = Watcher::new(
        Duration::from_millis(args.watch_debounce_millis),
        Duration::from_millis(args.watch_poll_millis),
    )
    .expect("Could not create watcher");
    loop {
        let res = run(args, client, s3, keys, &previous_names, &previous_urls).await;
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

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let client = kubimo::Client::infer()
        .await
        .expect("Could not create client");
    let s3 = S3Client::from_env();

    let mut previous_names = BTreeSet::new();
    let mut previous_urls = BTreeSet::new();
    let mut names = WorkspaceDirNameSet::new(args.name.clone());
    let mut urls = WorkspaceFileUrlSet::new(
        args.bucket.clone().unwrap_or_default(),
        args.key_prefix.clone(),
    )
    .expect("Could not create WorkspaceFileUrlSet");
    let mut cache_markers = CacheMarkers::new();
    process_existing_dirs(
        &client,
        &args.name,
        &mut names,
        &mut urls,
        &mut cache_markers,
        &mut previous_names,
        &mut previous_urls,
    )
    .await;
    let keys = WorkspaceKeys::new(names, urls);
    s3.set_cache(cache_markers).await;

    if args.watch {
        watch(&args, &client, &s3, &keys, previous_names, previous_urls).await;
    } else {
        let _ = run(&args, &client, &s3, &keys, &previous_names, &previous_urls).await;
    }
}
