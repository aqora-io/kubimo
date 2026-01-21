mod keys;
mod python;
mod upload;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use keys::{WorkspaceDirNameSet, WorkspaceFileUrlSet};
use kubimo::FilterParams;
use upload::{UploadError, upload};

use clap::Parser;
use futures::stream::{StreamExt, TryStreamExt, futures_unordered::FuturesUnordered};
use kubimo::{
    WorkspaceDir, WorkspaceDirContentUrl, WorkspaceDirDirectory, WorkspaceDirEntry,
    WorkspaceDirField, WorkspaceDirFile, WorkspaceDirMarimo, WorkspaceDirMarimoCache,
    WorkspaceDirSpec, WorkspaceDirSymlink, prelude::*, url::Url,
};
use object_store::{
    ObjectStoreExt,
    aws::{AmazonS3, AmazonS3Builder},
};
use python::is_marimo_notebook;
use thiserror::Error;
use tokio::sync::{
    Mutex, RwLock, Semaphore,
    mpsc::{Receiver, Sender, channel},
};
use tokio::task::JoinSet;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, prelude::*};

const CACHE_FORMATS: &[&str] = &["md", "html", "ipynb"];
const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long)]
    include_gitignored: bool,
    #[arg(long)]
    exclude_hidden: bool,
    #[arg(long, default_value_t = 100 * 1024 * 1024)] // 100 MB
    max_file_size: u64,
    #[arg(long, default_value_t = 10)]
    max_upload_concurrency: usize,
    #[arg(long, short)]
    name: String,
    #[arg(long, short)]
    bucket: String,
    #[arg(long, short = 'p')]
    key_prefix: Option<String>,
    #[arg(default_value = ".")]
    directory: PathBuf,
}

#[derive(Clone)]
pub struct DirectoryVisitor {
    tx: Sender<PathBuf>,
    directory: Arc<PathBuf>,
}

impl ignore::ParallelVisitor for DirectoryVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::error!("Error reading entry: {}", err);
                return ignore::WalkState::Continue;
            }
        };
        let path = match entry.path().strip_prefix(self.directory.as_ref()) {
            Ok(path) => path.to_path_buf(),
            Err(err) => {
                tracing::error!(
                    "Error stripping prefix for {}: {}",
                    entry.path().display(),
                    err
                );
                return ignore::WalkState::Continue;
            }
        };
        if path.as_os_str().is_empty() {
            return ignore::WalkState::Continue;
        }
        if let Err(err) = self.tx.blocking_send(path) {
            tracing::error!("Error sending entry: {}", err);
        }
        ignore::WalkState::Continue
    }
}

impl ignore::ParallelVisitorBuilder<'static> for DirectoryVisitor {
    fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 'static> {
        Box::new(self.clone())
    }
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

#[derive(Clone)]
pub struct S3Client {
    builder: Arc<AmazonS3Builder>,
    clients: Arc<RwLock<BTreeMap<String, AmazonS3>>>,
}

impl S3Client {
    pub fn from_env() -> Self {
        Self {
            builder: Arc::new(AmazonS3Builder::from_env()),
            clients: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub async fn bucket(&self, bucket: &str) -> object_store::Result<AmazonS3> {
        if let Some(client) = self.clients.read().await.get(bucket) {
            return Ok(client.clone());
        }
        let client = self
            .builder
            .as_ref()
            .clone()
            .with_bucket_name(bucket.to_string())
            .build()?;
        self.clients
            .write()
            .await
            .insert(bucket.to_string(), client.clone());
        Ok(client)
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

#[derive(Clone)]
pub struct WorkerOptions {
    s3: S3Client,
    directory: Arc<PathBuf>,
    bucket: Arc<String>,
    max_file_size: u64,
    upload_permits: Arc<Semaphore>,
    keys: WorkspaceKeys,
    e_tags: Arc<BTreeMap<PathBuf, String>>,
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

    async fn matched_etag(
        &self,
        s3: &AmazonS3,
        key: object_store::path::Path,
        path: impl AsRef<Path>,
    ) -> Option<String> {
        let existing_e_tag = self.opts.e_tags.get(path.as_ref())?;
        let e_tag = s3.head(&key).await.ok()?.e_tag?;
        if existing_e_tag == &e_tag {
            Some(e_tag)
        } else {
            None
        }
    }

    async fn upload_cache(
        &self,
        path: impl AsRef<Path>,
        size: u64,
    ) -> Result<WorkspaceDirContentUrl, WorkerError> {
        let path = path.as_ref();
        let url = self.opts.keys.file_url(path.to_path_buf()).await?;
        let key = object_store::path::Path::parse(url.path())?;
        let s3 = self.opts.s3.bucket(&self.opts.bucket).await?;
        if let Some(e_tag) = self.matched_etag(&s3, key.clone(), path).await {
            return Ok(WorkspaceDirContentUrl {
                url,
                e_tag: Some(e_tag),
            });
        }
        let full_path = self.opts.directory.join(path);
        let result = upload(&s3, key, &full_path, size, &self.opts.upload_permits).await?;
        Ok(WorkspaceDirContentUrl {
            url,
            e_tag: result.e_tag,
        })
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
        if !is_marimo_notebook(&source) {
            return Ok(None);
        }
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
        Ok(Some(WorkspaceDirMarimo {
            caches: if caches.is_empty() {
                None
            } else {
                Some(caches)
            },
        }))
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
            let marimo = self
                .process_marimo(&path, size)
                .await
                .inspect_err(|err| {
                    tracing::error!("Error reading marimo for {}: {}", path.display(), err)
                })
                .ok()
                .flatten();
            out.file = Some(WorkspaceDirFile {
                marimo,
                size: Some(size),
            });
        }
        Ok(out)
    }
}

pub struct WalkOptions {
    directory: PathBuf,
    include_gitignored: bool,
    exclude_hidden: bool,
}

pub fn walk(join_set: &mut JoinSet<()>, options: WalkOptions, buffer: usize) -> Receiver<PathBuf> {
    let (tx, rx) = channel(buffer);
    let walker = ignore::WalkBuilder::new(&options.directory)
        .git_ignore(!options.include_gitignored)
        .hidden(options.exclude_hidden)
        .build_parallel();
    let directory = Arc::new(options.directory);
    join_set.spawn_blocking(|| {
        walker.visit(&mut DirectoryVisitor { tx, directory });
    });
    rx
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

pub struct DirManager {
    name: String,
    map: BTreeMap<PathBuf, WorkspaceDir>,
    keys: BTreeSet<u32>,
}

impl DirManager {
    pub fn new(name: String) -> Self {
        let mut map = BTreeMap::new();
        map.insert(
            PathBuf::from(""),
            WorkspaceDir::new(
                &name,
                WorkspaceDirSpec {
                    workspace: name.clone(),
                    ..Default::default()
                },
            ),
        );
        Self {
            name,
            map,
            keys: BTreeSet::new(),
        }
    }

    pub fn get_mut(&mut self, path: PathBuf) -> &mut WorkspaceDir {
        self.map.entry(path.clone()).or_insert_with(|| {
            let mut key = CRC32.checksum(path.as_os_str().as_encoded_bytes());
            while self.keys.contains(&key) {
                key = key.wrapping_add(1);
            }
            self.keys.insert(key);
            let name = format!("{}-{:08x}", self.name, key);
            WorkspaceDir::new(
                &name,
                WorkspaceDirSpec {
                    workspace: self.name.clone(),
                    path: path.to_string_lossy().to_string(),
                    ..Default::default()
                },
            )
        })
    }

    pub fn insert(&mut self, path: PathBuf, mut entry: WorkspaceDirEntry) {
        if let Some(dir) = entry.directory.as_mut() {
            let dir_path = path.join(&entry.name);
            dir.name = self.get_mut(dir_path).metadata.name.clone();
        }
        self.get_mut(path)
            .spec
            .entries
            .get_or_insert_default()
            .push(entry);
    }
}

async fn process_existing_dirs(
    client: &kubimo::Client,
    name: &str,
    names: &mut WorkspaceDirNameSet,
    urls: &mut WorkspaceFileUrlSet,
    e_tags: &mut BTreeMap<PathBuf, String>,
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
            tracing::error!("Error inserting workspace dir name: {}", err);
            continue;
        }
        for entry in workspace_dir.spec.entries.unwrap_or_default().as_slice() {
            let Some(file) = &entry.file else {
                continue;
            };
            let Some(marimo) = &file.marimo else {
                continue;
            };
            let Some(caches) = &marimo.caches else {
                continue;
            };
            let path = dir_path.join(&entry.name);
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
                    if let Some(e_tag) = &url.e_tag {
                        e_tags.insert(cache_path, e_tag.clone());
                    }
                }
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
    let mut urls = WorkspaceFileUrlSet::new(args.bucket.clone(), args.key_prefix)
        .expect("Could not create WorkspaceFileUrlSet");
    let mut e_tags = BTreeMap::new();
    process_existing_dirs(
        &client,
        &args.name,
        &mut names,
        &mut urls,
        &mut e_tags,
        &mut previous_names,
        &mut previous_urls,
    )
    .await;
    let keys = WorkspaceKeys::new(names, urls);

    let mut join_set = JoinSet::new();
    let rx = walk(
        &mut join_set,
        WalkOptions {
            directory: args.directory.clone(),
            include_gitignored: args.include_gitignored,
            exclude_hidden: args.exclude_hidden,
        },
        1000,
    );
    let mut rx = process(
        &mut join_set,
        rx,
        WorkerOptions {
            s3: s3.clone(),
            directory: Arc::new(args.directory),
            bucket: Arc::new(args.bucket),
            max_file_size: args.max_file_size,

            upload_permits: Arc::new(Semaphore::new(args.max_upload_concurrency)),
            keys: keys.clone(),
            e_tags: Arc::new(e_tags.clone()),
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
        if let Some(caches) = entry
            .file
            .as_ref()
            .and_then(|file| file.marimo.as_ref())
            .and_then(|marimo| marimo.caches.as_ref())
        {
            for url in caches {
                if let Some(url) = &url.url {
                    urls.insert(url.url.clone());
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
    let names_to_delete = previous_names
        .difference(&workspace_dirs.keys().cloned().collect::<BTreeSet<_>>())
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
            match bmowds.patch(&dir).await {
                Ok(_) => {
                    tracing::info!("Created workspace dir '{path}'");
                }
                Err(err) => {
                    tracing::error!("Error creating workspace dir '{path}': {err}");
                }
            }
        }));
    }
    for name in names_to_delete {
        let bmowds = client.api::<WorkspaceDir>();
        futs.push(tokio::spawn(async move {
            match bmowds.delete(&name).await {
                Ok(_) => {
                    tracing::info!("Deleted workspace dir '{name}'");
                }
                Err(err) => {
                    tracing::error!("Error deleting workspace dir '{name}': {err}");
                }
            }
        }));
    }
    for url in urls_to_delete {
        let cloned_s3 = s3.clone();
        futs.push(tokio::spawn(async move {
            if url.scheme() != "s3" {
                tracing::warn!("Skipping non-S3 URL deletion: {url}");
                return;
            }
            let bucket = url.authority();
            let s3 = match cloned_s3.bucket(bucket).await {
                Ok(s3) => s3,
                Err(err) => {
                    tracing::error!("Error getting S3 client for bucket '{bucket}': {err}");
                    return;
                }
            };
            let key = match object_store::path::Path::parse(url.path()) {
                Ok(key) => key,
                Err(err) => {
                    tracing::error!("Error parsing S3 key from URL '{url}': {err}");
                    return;
                }
            };
            match s3.delete(&key).await {
                Ok(_) => {
                    tracing::info!("Deleted S3 object '{url}'");
                }
                Err(err) => {
                    tracing::error!("Error deleting S3 object '{url}': {err}");
                }
            }
        }));
    }
    futs.try_collect::<()>()
        .await
        .expect("Error waiting for tasks");
}
