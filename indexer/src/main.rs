use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use indexer::keys::{WorkspaceDirNameSet, WorkspaceFileUrlSet};
use indexer::restore;
use indexer::s3::{CacheMarkers, S3Client};
use indexer::upload::{self, WorkspaceKeys};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, prelude::*};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Upload(UploadArgs),
    Clean(CleanArgs),
    Download(DownloadArgs),
}

#[derive(Args, Debug)]
struct UploadArgs {
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
    /// Index a workspace whose directory is genuinely empty. Without this, a
    /// walk that finds nothing while an archive exists is refused rather than
    /// deleting that archive — an unmounted volume looks exactly like an empty
    /// one from in here.
    #[arg(long)]
    allow_empty: bool,
    #[arg(long, default_value_t = 500)]
    watch_debounce_millis: u64,
    /// Ceiling on how long a burst of events may defer a sync. Without one a
    /// directory that never goes quiet is never indexed.
    #[arg(long, default_value_t = 10 * 1000)]
    watch_max_wait_millis: u64,
    #[arg(long, default_value_t = 60 * 1000)]
    watch_poll_millis: u64,
    name: String,
    #[arg(default_value = ".")]
    directory: PathBuf,
}

impl UploadArgs {
    fn to_options(&self) -> upload::UploadOptions {
        upload::UploadOptions {
            include_gitignored: self.include_gitignored,
            exclude_hidden: self.exclude_hidden,
            max_file_size: self.max_file_size,
            max_upload_concurrency: self.max_upload_concurrency,
            bucket: self.bucket.clone(),
            key_prefix: self.key_prefix.clone(),
            watch: self.watch,
            upload_content: self.upload_content,
            allow_empty: self.allow_empty,
            watch_debounce_millis: self.watch_debounce_millis,
            watch_max_wait_millis: self.watch_max_wait_millis,
            watch_poll_millis: self.watch_poll_millis,
            name: self.name.clone(),
            directory: self.directory.clone(),
        }
    }
}

#[derive(Args, Debug)]
struct CleanArgs {
    name: String,
}

#[derive(Args, Debug)]
pub(crate) struct DownloadArgs {
    #[arg(long, short, env = "AWS_BUCKET")]
    pub(crate) bucket: String,
    #[arg(long, short = 'p', env = "AWS_KEY_PREFIX")]
    pub(crate) key_prefix: Option<String>,
    #[arg(long, default_value_t = 10)]
    pub(crate) max_download_concurrency: usize,
    /// Continue on per-file download errors instead of failing.
    #[arg(long)]
    pub(crate) best_effort: bool,
    #[arg(default_value = ".")]
    pub(crate) directory: PathBuf,
}

impl DownloadArgs {
    fn to_options(&self) -> restore::RestoreOptions {
        restore::RestoreOptions {
            bucket: self.bucket.clone(),
            key_prefix: self.key_prefix.clone(),
            directory: self.directory.clone(),
            max_download_concurrency: self.max_download_concurrency,
            best_effort: self.best_effort,
        }
    }
}
#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let s3 = S3Client::from_env();

    match cli.command {
        // Download needs no Kubernetes API access; only the other commands
        // build a client.
        Command::Download(args) => {
            if let Err(err) = restore::restore(&args.to_options(), &s3).await {
                tracing::error!("Error restoring workspace: {err}");
                std::process::exit(1);
            }
        }
        Command::Upload(args) => {
            let client = kube_client().await;
            let mut previous_names = BTreeSet::new();
            let mut previous_urls = BTreeSet::new();
            let mut names = WorkspaceDirNameSet::new(args.name.clone());
            let mut urls = WorkspaceFileUrlSet::new(
                args.bucket.clone().unwrap_or_default(),
                args.key_prefix.clone(),
            )
            .expect("Could not create WorkspaceFileUrlSet");
            let mut cache_markers = CacheMarkers::new();
            upload::process_existing_dirs(
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
                upload::watch(
                    &args.to_options(),
                    &client,
                    &s3,
                    &keys,
                    previous_names,
                    previous_urls,
                )
                .await;
            } else {
                let result = upload::run(
                    &args.to_options(),
                    // One-shot: nothing to carry over between runs.
                    &indexer::fingerprint::ContentCache::new(),
                    &client,
                    &s3,
                    &keys,
                    &previous_names,
                    &previous_urls,
                )
                .await;
                // A refusal is not a crash, but it is not a successful index
                // either. Exiting non-zero is what lets a caller driving many
                // of these tell "indexed" from "declined to destroy an
                // archive" — the watcher, by contrast, keeps running and
                // retries on the next event.
                if result.refused {
                    std::process::exit(2);
                }
                // Same reasoning, one step further in: the archive was
                // rewritten, but not from a complete picture of the tree. A
                // caller must not read that as "this workspace is now safely
                // in S3". Its own exit code, since the two need different
                // remedies — this one is a retry.
                if result.failures > 0 {
                    tracing::error!(
                        failures = result.failures,
                        "Indexed with failures; the archive does not fully represent {}",
                        args.directory.display()
                    );
                    std::process::exit(3);
                }
            }
        }
        Command::Clean(args) => {
            let client = kube_client().await;
            upload::clean(&client, &s3, &args.name).await;
        }
    }
}

async fn kube_client() -> kubimo::Client {
    kubimo::Client::builder()
        .name("kubimo-indexer")
        .build()
        .await
        .expect("Could not create client")
}
