use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use base64::Engine as _;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use kubimo::{ManifestSecrets, WorkspaceManifest, WorkspaceRestoreSecrets, url::Url};
use thiserror::Error;
use tokio::{io::AsyncWriteExt, sync::Semaphore, task::JoinSet};

use crate::disk;
use crate::s3::{DownloadError, S3Client};
use crate::secrets;

#[derive(Debug, PartialEq)]
pub struct RestoreFile {
    pub path: PathBuf,
    pub url: Url,
    pub crc32: Option<u32>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Default)]
pub struct RestorePlan {
    pub directories: Vec<PathBuf>,
    /// (link path, target); targets are restored verbatim and never followed.
    pub symlinks: Vec<(PathBuf, PathBuf)>,
    pub files: Vec<RestoreFile>,
    /// File entries without a content url (e.g. over the indexer's max file
    /// size at upload time).
    pub skipped: Vec<PathBuf>,
    /// File entries the secret matcher diverted. Empty by construction for
    /// archives written by a secrets-aware indexer — their secret paths never
    /// reach the manifest — so anything here is a legacy archive's, and only a
    /// `Values` restore may write it verbatim.
    pub secret_files: Vec<RestoreFile>,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("unsafe path in manifest: {0}")]
    UnsafePath(String),
    #[error("manifest entry points outside the archive: {url} is not under {expected}")]
    ForeignContent { url: String, expected: String },
}

/// Where an archive's objects are allowed to live.
///
/// A manifest's content urls are followed verbatim, and whoever restores holds
/// credentials for the whole bucket — the node agent serves every workspace on
/// its node from one client. So a manifest naming another prefix would have its
/// objects copied into this slot, and a manifest naming another bucket would
/// reach whatever else those credentials can see.
///
/// No archive legitimately does this. Both writers key their objects under the
/// same prefix as the manifest: the indexer as `{prefix}{base32}.{ext}`
/// (`keys.rs`), and the platform's seed as `{prefix}{relative path}`. The one
/// case that looks like an exception isn't — a seeded workspace's own manifest
/// sits at `workspace/{uuid}/` and points into `workspace/{uuid}/seed/`, which
/// is still underneath it.
///
/// Tenants cannot write a manifest today, so this is defence in depth rather
/// than a live hole. It is cheap here and expensive to add after something can.
#[derive(Debug, Clone)]
pub struct ArchiveOrigin {
    pub bucket: String,
    pub key_prefix: Option<String>,
}

impl ArchiveOrigin {
    fn base(&self) -> String {
        let mut prefix = self.key_prefix.as_deref().unwrap_or("").to_string();
        // Anchor at a path boundary: without the trailing slash, `mine` would
        // also admit `mine-other/…` as its own content.
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        format!("s3://{}/{}", self.bucket, prefix)
    }

    fn check(&self, url: &Url) -> Result<(), PlanError> {
        let base = self.base();
        if url.as_str().starts_with(&base) {
            return Ok(());
        }
        Err(PlanError::ForeignContent {
            url: url.to_string(),
            expected: base,
        })
    }
}

fn safe_relative_path(path: &str) -> Result<PathBuf, PlanError> {
    let path = Path::new(path);
    if !path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(PlanError::UnsafePath(path.display().to_string()));
    }
    Ok(path.to_path_buf())
}

fn safe_entry_path(dir: &Path, name: &str) -> Result<PathBuf, PlanError> {
    let mut components = Path::new(name).components();
    if !(matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()) {
        return Err(PlanError::UnsafePath(name.to_string()));
    }
    Ok(dir.join(name))
}

/// Split a manifest into the filesystem operations needed to restore it. The
/// manifest is only semi-trusted: paths escaping the target directory are
/// rejected, and so are content urls escaping `origin` (see [`ArchiveOrigin`]).
/// Marimo meta/cache urls are ignored — they are derived artifacts.
///
/// Entries `matcher` marks secret are diverted: files with content to
/// [`RestorePlan::secret_files`], everything else dropped — a legacy archive's
/// `.env` (always, by file name) and whatever its own `.secrets` patterns
/// matched must not be restored as ordinary files.
pub fn plan_restore(
    manifest: &WorkspaceManifest,
    origin: &ArchiveOrigin,
    matcher: &Gitignore,
) -> Result<RestorePlan, PlanError> {
    let mut plan = RestorePlan::default();
    for directory in &manifest.directories {
        let dir_path = safe_relative_path(&directory.path)?;
        // Also create the directory itself: its `directory` entry in the
        // parent may be missing from a partially indexed batch.
        if !dir_path.as_os_str().is_empty() {
            plan.directories.push(dir_path.clone());
        }
        for entry in &directory.entries {
            let entry_path = safe_entry_path(&dir_path, &entry.name)?;
            if entry.directory.is_some() {
                plan.directories.push(entry_path);
            } else if let Some(symlink) = &entry.symlink {
                if secrets::is_secret(matcher, &entry_path, false) {
                    // Restoring a matched symlink would materialize a pointer
                    // to whatever it names; drop it.
                } else if let Some(target) = &symlink.path {
                    plan.symlinks.push((entry_path, PathBuf::from(target)));
                } else {
                    plan.skipped.push(entry_path);
                }
            } else if let Some(file) = &entry.file {
                if let Some(content) = &file.content {
                    origin.check(&content.url)?;
                    let file = RestoreFile {
                        path: entry_path,
                        url: content.url.clone(),
                        crc32: content.crc32,
                        modified: entry.modified.map(Into::into),
                    };
                    if secrets::is_secret(matcher, &file.path, false) {
                        plan.secret_files.push(file);
                    } else {
                        plan.files.push(file);
                    }
                } else if secrets::is_secret(matcher, &entry_path, false) {
                    // Unrestorable in every mode; already warned at upload.
                } else {
                    plan.skipped.push(entry_path);
                }
            } else {
                plan.skipped.push(entry_path);
            }
        }
    }
    Ok(plan)
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error("error parsing manifest: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Url(#[from] kubimo::url::ParseError),
    #[error("archive was written without --upload-content and cannot be restored")]
    NoContent,
    #[error("not enough space: archive needs {needed} bytes, {available} available")]
    InsufficientSpace { needed: u64, available: u64 },
    #[error("could not determine disk usage: {0}")]
    Disk(#[from] rustix::io::Errno),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0} of {1} files failed to download")]
    Failed(usize, usize),
}

/// Restore an archive into `args.directory` from its manifest in S3.
/// Everything [`restore`] needs, without depending on the binary's clap types
/// so the node agent can call it too.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// Bucket holding the archive.
    pub bucket: String,
    pub key_prefix: Option<String>,
    /// Where to write the restored tree.
    pub directory: PathBuf,
    pub max_download_concurrency: usize,
    /// Continue past per-file download errors instead of failing the restore.
    pub best_effort: bool,
    /// How to treat the archive's secrets. `NamesOnly` is the fail-safe
    /// default; only a caller that decided the restorer may see the source's
    /// values passes `Values`.
    pub secrets: WorkspaceRestoreSecrets,
}

pub async fn restore(args: &RestoreOptions, s3: &S3Client) -> Result<(), RestoreError> {
    let manifest_url = kubimo::manifest_url(&args.bucket, args.key_prefix.as_deref())?;
    let bytes = s3.get_bytes(&manifest_url).await?;
    let manifest: WorkspaceManifest = serde_json::from_slice(&bytes)?;
    if !manifest.upload_content {
        return Err(RestoreError::NoContent);
    }
    let origin = ArchiveOrigin {
        bucket: args.bucket.clone(),
        key_prefix: args.key_prefix.clone(),
    };
    let matcher = legacy_matcher(args, s3, &manifest, &origin).await?;
    let plan = plan_restore(&manifest, &origin, &matcher)?;

    tokio::fs::create_dir_all(&args.directory).await?;
    let usage = disk::disk_usage(&args.directory)?;
    if usage.available < manifest.total_content_bytes {
        return Err(RestoreError::InsufficientSpace {
            needed: manifest.total_content_bytes,
            available: usage.available,
        });
    }

    for dir in &plan.directories {
        tokio::fs::create_dir_all(args.directory.join(dir)).await?;
    }
    for (link, target) in &plan.symlinks {
        let link = args.directory.join(link);
        remove_if_exists(&link).await?;
        tokio::fs::symlink(target, &link).await?;
    }
    for path in &plan.skipped {
        tracing::warn!("Skipping {}: no content in archive", path.display());
    }

    let total = plan.files.len();
    let permits = Arc::new(Semaphore::new(args.max_download_concurrency));
    let mut join_set = JoinSet::new();
    for file in plan.files {
        let s3 = s3.clone();
        let permits = permits.clone();
        let directory = args.directory.clone();
        join_set.spawn(async move {
            let _permit = match permits.acquire().await {
                Ok(permit) => permit,
                Err(err) => {
                    tracing::error!("Error acquiring permit: {err}");
                    return 1;
                }
            };
            match download_file(&s3, &directory, &file).await {
                Ok(()) => {
                    tracing::info!("Restored {}", file.path.display());
                    0
                }
                Err(err) => {
                    tracing::error!("Error restoring {}: {err}", file.path.display());
                    1
                }
            }
        });
    }
    let mut failed = 0;
    while let Some(res) = join_set.join_next().await {
        failed += res.unwrap_or(1);
    }
    tracing::info!(
        "Restored {} of {total} files ({} skipped)",
        total - failed,
        plan.skipped.len()
    );
    let outcome = restore_secrets(args, s3, &manifest, plan.secret_files).await?;
    failed += outcome.failed;
    let total = total + outcome.total;
    if failed > 0 && !args.best_effort {
        return Err(RestoreError::Failed(failed, total));
    }
    Ok(())
}

/// The matcher [`plan_restore`] diverts with. For an archive written by a
/// secrets-aware indexer nothing needs matching — its secret paths never
/// reached the manifest, and the always-secret `.env` file name is caught by
/// [`secrets::is_secret`] regardless — so this is empty. For a legacy archive
/// (`manifest.secrets` is `None`), the workspace's `.secrets` pattern file
/// exists only as a normal entry *inside* the archive; honour it, closing the
/// window where a user created `.secrets` while an old indexer was still
/// uploading those matched files as ordinary entries.
async fn legacy_matcher(
    args: &RestoreOptions,
    s3: &S3Client,
    manifest: &WorkspaceManifest,
    origin: &ArchiveOrigin,
) -> Result<Gitignore, RestoreError> {
    if manifest.secrets.is_some() {
        return Ok(Gitignore::empty());
    }
    let Some(url) = manifest
        .directories
        .iter()
        .filter(|dir| dir.path.is_empty())
        .flat_map(|dir| dir.entries.iter())
        .filter(|entry| entry.name == secrets::SECRETS_PATTERN_FILE)
        .find_map(|entry| entry.file.as_ref()?.content.as_ref())
        .map(|content| content.url.clone())
    else {
        return Ok(Gitignore::empty());
    };
    origin.check(&url)?;
    let bytes = match s3.get_bytes(&url).await {
        Ok(bytes) => bytes,
        // Swept between the manifest read and now; the next manifest no
        // longer names it.
        Err(DownloadError::S3(object_store::Error::NotFound { .. })) => {
            return Ok(Gitignore::empty());
        }
        Err(err) => match args.secrets {
            // Without the patterns a NamesOnly restore would write the files
            // they mark as ordinary entries — the one thing it must not do.
            WorkspaceRestoreSecrets::NamesOnly => return Err(err.into()),
            // A Values restore writes them verbatim either way.
            WorkspaceRestoreSecrets::Values => {
                tracing::warn!("Could not fetch {}: {err}", secrets::SECRETS_PATTERN_FILE);
                return Ok(Gitignore::empty());
            }
        },
    };
    let mut builder = GitignoreBuilder::new("");
    for line in String::from_utf8_lossy(&bytes).lines() {
        if let Err(err) = builder.add_line(None, line) {
            tracing::warn!(
                "Malformed line in archived {}: {err}",
                secrets::SECRETS_PATTERN_FILE
            );
        }
    }
    match builder.build() {
        Ok(matcher) => Ok(matcher),
        Err(err) => {
            tracing::warn!(
                "Could not build a matcher from the archived {}: {err}",
                secrets::SECRETS_PATTERN_FILE
            );
            Ok(Gitignore::empty())
        }
    }
}

#[derive(Debug, Default)]
struct SecretRestoreOutcome {
    total: usize,
    failed: usize,
}

/// The secrets phase, after the ordinary files are down. Four-way on
/// (mode × archive generation):
///
/// - `Values` × secrets-aware: fetch the secrets object, write `.env` and the
///   secret files with owner-only permissions.
/// - `Values` × legacy: download the diverted entries verbatim — exactly the
///   pre-secrets behavior, which keeps warm pooled reopens of un-reindexed
///   archives intact.
/// - `NamesOnly` × secrets-aware: write `.env` placeholders from the
///   manifest's key names; secret files are only mentioned.
/// - `NamesOnly` × legacy: read the archived `.env` *into memory only* to
///   derive its key names. The mode is a policy boundary, not a permission
///   one — the restorer holds the archive's credentials either way — so
///   deriving names gives legacy public clones the same placeholder UX
///   without ever writing the values to disk.
async fn restore_secrets(
    args: &RestoreOptions,
    s3: &S3Client,
    manifest: &WorkspaceManifest,
    secret_files: Vec<RestoreFile>,
) -> Result<SecretRestoreOutcome, RestoreError> {
    match (args.secrets, manifest.secrets.as_ref()) {
        (WorkspaceRestoreSecrets::Values, Some(names)) => {
            restore_secret_values(args, s3, names).await
        }
        (WorkspaceRestoreSecrets::Values, None) => {
            let mut outcome = SecretRestoreOutcome {
                total: secret_files.len(),
                ..Default::default()
            };
            for file in &secret_files {
                match download_file(s3, &args.directory, file).await {
                    Ok(()) => tracing::info!("Restored secret file {}", file.path.display()),
                    Err(err) => {
                        outcome.failed += 1;
                        tracing::error!("Error restoring {}: {err}", file.path.display());
                    }
                }
            }
            Ok(outcome)
        }
        (WorkspaceRestoreSecrets::NamesOnly, Some(names)) => {
            for path in &names.file_paths {
                tracing::info!("Withholding secret file {path} (names-only restore)");
            }
            write_env_placeholders(args, &names.env_keys).await
        }
        (WorkspaceRestoreSecrets::NamesOnly, None) => {
            let mut env_keys = Vec::new();
            for file in &secret_files {
                if file.path.as_os_str() == secrets::DOTENV_FILE_NAME {
                    match s3.get_bytes(&file.url).await {
                        Ok(bytes) => {
                            env_keys = secrets::parse_dotenv(&String::from_utf8_lossy(&bytes))
                                .into_iter()
                                .map(|(key, _)| key)
                                .collect();
                        }
                        Err(DownloadError::S3(object_store::Error::NotFound { .. })) => {
                            tracing::warn!("Archived .env vanished before its keys were read");
                        }
                        Err(err) => return Err(err.into()),
                    }
                } else {
                    tracing::info!(
                        "Withholding secret file {} (names-only restore)",
                        file.path.display()
                    );
                }
            }
            write_env_placeholders(args, &env_keys).await
        }
    }
}

async fn write_env_placeholders(
    args: &RestoreOptions,
    env_keys: &[String],
) -> Result<SecretRestoreOutcome, RestoreError> {
    if env_keys.is_empty() {
        return Ok(SecretRestoreOutcome::default());
    }
    let rendered = secrets::render_placeholders(env_keys.iter().map(String::as_str));
    let path = args.directory.join(secrets::DOTENV_FILE_NAME);
    let mut outcome = SecretRestoreOutcome {
        total: 1,
        ..Default::default()
    };
    match write_secret_file(&path, rendered.as_bytes()).await {
        Ok(()) => tracing::info!("Wrote .env placeholders for {} keys", env_keys.len()),
        Err(err) => {
            outcome.failed += 1;
            tracing::error!("Error writing .env placeholders: {err}");
        }
    }
    Ok(outcome)
}

async fn restore_secret_values(
    args: &RestoreOptions,
    s3: &S3Client,
    names: &ManifestSecrets,
) -> Result<SecretRestoreOutcome, RestoreError> {
    let url = kubimo::secrets_url(&args.bucket, args.key_prefix.as_deref())?;
    let bytes = match s3.get_bytes(&url).await {
        Ok(bytes) => bytes,
        Err(DownloadError::S3(object_store::Error::NotFound { .. })) => {
            if names.env_keys.is_empty() && names.file_paths.is_empty() {
                return Ok(SecretRestoreOutcome::default());
            }
            // The upload side writes the secrets object before the manifest,
            // but a cycle whose secrets upload failed still writes its
            // manifest (holding back `lastSyncedAt` instead), so the names can
            // outlive the object. Degrade to placeholders and count the
            // missing values as a failure: the default strict restore still
            // fails through the count, while `--best-effort` gets the same
            // panel-visible keys a names-only restore would produce.
            tracing::error!(
                "Manifest names secrets but the archive has no secrets object at {url}"
            );
            let mut outcome = write_env_placeholders(args, &names.env_keys).await?;
            outcome.total += 1;
            outcome.failed += 1;
            return Ok(outcome);
        }
        Err(err) => return Err(err.into()),
    };
    let workspace_secrets: kubimo::WorkspaceSecrets = serde_json::from_slice(&bytes)?;
    write_secret_values(args, &workspace_secrets).await
}

async fn write_secret_values(
    args: &RestoreOptions,
    workspace_secrets: &kubimo::WorkspaceSecrets,
) -> Result<SecretRestoreOutcome, RestoreError> {
    let mut outcome = SecretRestoreOutcome::default();
    if !workspace_secrets.env.is_empty() {
        outcome.total += 1;
        let rendered = secrets::render_dotenv(
            workspace_secrets
                .env
                .iter()
                .map(|entry| (entry.key.as_str(), entry.value.as_str())),
        );
        let path = args.directory.join(secrets::DOTENV_FILE_NAME);
        match write_secret_file(&path, rendered.as_bytes()).await {
            Ok(()) => tracing::info!("Restored .env ({} keys)", workspace_secrets.env.len()),
            Err(err) => {
                outcome.failed += 1;
                tracing::error!("Error restoring .env: {err}");
            }
        }
    }
    for file in &workspace_secrets.files {
        // The secrets object is only semi-trusted, like the manifest: a path
        // escaping the target directory must not be written.
        let path = safe_relative_path(&file.path)?;
        let Some(content) = &file.content_base64 else {
            tracing::warn!(
                "Secret file {} has no inline content (over the cap at export); skipping",
                path.display()
            );
            continue;
        };
        outcome.total += 1;
        let bytes = match base64::engine::general_purpose::STANDARD.decode(content) {
            Ok(bytes) => bytes,
            Err(err) => {
                outcome.failed += 1;
                tracing::error!("Error decoding secret file {}: {err}", path.display());
                continue;
            }
        };
        let full_path = args.directory.join(&path);
        if let Some(parent) = full_path.parent()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            outcome.failed += 1;
            tracing::error!("Error creating {}: {err}", parent.display());
            continue;
        }
        match write_secret_file(&full_path, &bytes).await {
            Ok(()) => tracing::info!("Restored secret file {}", path.display()),
            Err(err) => {
                outcome.failed += 1;
                tracing::error!("Error restoring secret file {}: {err}", path.display());
            }
        }
    }
    Ok(outcome)
}

/// Write a secret with owner-only permissions, never following a pre-existing
/// symlink (the [`create_output_file`] hazard).
async fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    remove_if_exists(path).await?;
    let mut file = tokio::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    // A tokio `File` runs its writes on a background task; dropping the handle
    // without flushing can drop the write with it.
    file.flush().await?;
    Ok(())
}

async fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Open `path` for writing without ever following a pre-existing symlink
/// (e.g. one planted by a malicious manifest entry of the same name): remove
/// whatever is there and create a fresh regular file.
async fn create_output_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    remove_if_exists(path).await?;
    tokio::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .await
}

async fn download_file(
    s3: &S3Client,
    directory: &Path,
    file: &RestoreFile,
) -> Result<(), RestoreError> {
    let full_path = directory.join(&file.path);
    let output = create_output_file(&full_path).await?;
    // `download` takes the handle by value, so it is closed by the time an
    // error returns here.
    if let Err(err) = s3.download(&file.url, output, file.crc32).await {
        // Don't leave a partial or corrupt file behind — with --best-effort
        // the restore continues and the file would otherwise look restored.
        if let Err(remove_err) = remove_if_exists(&full_path).await {
            tracing::warn!(
                "Could not remove partial file {}: {remove_err}",
                file.path.display()
            );
        }
        return Err(err.into());
    }
    if let Some(modified) = file.modified
        && let Err(err) = set_modified(&full_path, modified)
    {
        tracing::warn!("Could not restore mtime for {}: {err}", file.path.display());
    }
    Ok(())
}

fn set_modified(path: &Path, modified: SystemTime) -> std::io::Result<()> {
    std::fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::{
        ManifestDirectory, ManifestVersion, WorkspaceDirContentUrl, WorkspaceDirDirectory,
        WorkspaceDirEntry, WorkspaceDirFile, WorkspaceDirSymlink, WorkspaceManifest,
    };

    fn manifest(directories: Vec<ManifestDirectory>) -> WorkspaceManifest {
        WorkspaceManifest {
            version: ManifestVersion::V1,
            workspace: "ws".to_string(),
            upload_content: true,
            total_content_bytes: 0,
            directories,
            // Legacy shape: most of these tests predate secrets, and `None`
            // exercises the diversion path a filtered archive never takes.
            secrets: None,
            git_objects: None,
        }
    }

    /// Matches the bucket and (absent) prefix of `file_entry`'s content url.
    fn origin() -> ArchiveOrigin {
        ArchiveOrigin {
            bucket: "bucket".to_string(),
            key_prefix: None,
        }
    }

    fn plan(manifest: &WorkspaceManifest) -> Result<RestorePlan, PlanError> {
        plan_restore(manifest, &origin(), &Gitignore::empty())
    }

    fn file_entry(name: &str, with_content: bool) -> WorkspaceDirEntry {
        WorkspaceDirEntry {
            name: name.to_string(),
            file: Some(WorkspaceDirFile {
                size: Some(1),
                content: with_content.then(|| WorkspaceDirContentUrl {
                    url: "s3://bucket/0123456789abc".parse().unwrap(),
                    crc32: Some(7),
                    e_tag: None,
                }),
                marimo: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_plan_restore_splits_dirs_symlinks_files_and_skips() {
        let manifest = manifest(vec![
            ManifestDirectory {
                path: "".to_string(),
                entries: vec![
                    file_entry("a.txt", true),
                    file_entry("big.bin", false),
                    WorkspaceDirEntry {
                        name: "sub".to_string(),
                        directory: Some(WorkspaceDirDirectory {
                            name: Some("x".to_string()),
                        }),
                        ..Default::default()
                    },
                    WorkspaceDirEntry {
                        name: "link".to_string(),
                        symlink: Some(WorkspaceDirSymlink {
                            path: Some("a.txt".to_string()),
                        }),
                        ..Default::default()
                    },
                ],
            },
            ManifestDirectory {
                path: "sub".to_string(),
                entries: vec![file_entry("b.txt", true)],
            },
        ]);
        let plan = plan(&manifest).unwrap();
        assert!(plan.directories.contains(&PathBuf::from("sub")));
        assert_eq!(
            plan.symlinks,
            vec![(PathBuf::from("link"), PathBuf::from("a.txt"))]
        );
        assert_eq!(
            plan.files.iter().map(|f| &f.path).collect::<Vec<_>>(),
            vec![&PathBuf::from("a.txt"), &PathBuf::from("sub/b.txt")]
        );
        assert_eq!(plan.skipped, vec![PathBuf::from("big.bin")]);
    }

    #[test]
    fn test_plan_restore_creates_dirs_without_parent_entry() {
        // A directory that appears only as a ManifestDirectory path (no
        // `directory` entry in its parent) must still be created, or its
        // files fail to download.
        let manifest = manifest(vec![ManifestDirectory {
            path: "orphan".to_string(),
            entries: vec![file_entry("a.txt", true)],
        }]);
        let plan = plan(&manifest).unwrap();
        assert!(plan.directories.contains(&PathBuf::from("orphan")));
    }

    #[test]
    fn test_plan_restore_rejects_parent_dir_in_path() {
        let manifest = manifest(vec![ManifestDirectory {
            path: "../evil".to_string(),
            entries: vec![],
        }]);
        assert!(matches!(plan(&manifest), Err(PlanError::UnsafePath(_))));
    }

    /// Build a one-file manifest whose content url is `url`.
    fn manifest_pointing_at(url: &str) -> WorkspaceManifest {
        let mut entry = file_entry("a.txt", true);
        entry.file.as_mut().unwrap().content.as_mut().unwrap().url = url.parse().unwrap();
        manifest(vec![ManifestDirectory {
            path: String::new(),
            entries: vec![entry],
        }])
    }

    /// Whoever restores holds credentials for the whole bucket, so a manifest
    /// naming someone else's prefix would pull their objects into this slot.
    #[test]
    fn plan_restore_rejects_content_outside_the_archive() {
        let origin = ArchiveOrigin {
            bucket: "bucket".to_string(),
            key_prefix: Some("workspace/mine/".to_string()),
        };
        for url in [
            // Another tenant's prefix in the same bucket.
            "s3://bucket/workspace/theirs/secret.py",
            // A sibling prefix that shares a textual ancestor.
            "s3://bucket/workspace/mine-other/secret.py",
            // Another bucket entirely.
            "s3://other-bucket/workspace/mine/secret.py",
            // No prefix at all.
            "s3://bucket/secret.py",
        ] {
            assert!(
                matches!(
                    plan_restore(&manifest_pointing_at(url), &origin, &Gitignore::empty()),
                    Err(PlanError::ForeignContent { .. })
                ),
                "expected {url} to be rejected"
            );
        }
    }

    /// The shape a seeded workspace's *own* manifest has: it sits at
    /// `workspace/{uuid}/` and points into `workspace/{uuid}/seed/`. This is
    /// what makes a never-opened pooled workspace cloneable, so the check must
    /// not reject it.
    #[test]
    fn plan_restore_allows_a_seed_nested_under_the_archive() {
        let origin = ArchiveOrigin {
            bucket: "bucket".to_string(),
            key_prefix: Some("workspace/mine/".to_string()),
        };
        let manifest = manifest_pointing_at("s3://bucket/workspace/mine/seed/readme.py");
        let plan = plan_restore(&manifest, &origin, &Gitignore::empty())
            .expect("a nested seed is legitimate");
        assert_eq!(plan.files.len(), 1);
    }

    #[test]
    fn test_plan_restore_rejects_absolute_path() {
        let manifest = manifest(vec![ManifestDirectory {
            path: "/etc".to_string(),
            entries: vec![],
        }]);
        assert!(matches!(plan(&manifest), Err(PlanError::UnsafePath(_))));
    }

    #[test]
    fn test_plan_restore_rejects_unsafe_entry_name() {
        for name in ["..", "a/b"] {
            let manifest = manifest(vec![ManifestDirectory {
                path: "".to_string(),
                entries: vec![file_entry(name, true)],
            }]);
            assert!(
                matches!(plan(&manifest), Err(PlanError::UnsafePath(_))),
                "expected {name:?} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn test_create_output_file_replaces_symlink_instead_of_following() {
        let dir = std::env::temp_dir().join("kubimo-restore-test-symlink");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let target = dir.join("target.txt");
        tokio::fs::write(&target, b"original").await.unwrap();
        let link = dir.join("link.txt");
        tokio::fs::symlink(&target, &link).await.unwrap();

        let mut file = create_output_file(&link).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut file, b"downloaded")
            .await
            .unwrap();
        drop(file);

        // The symlink target must be untouched and the link path must now be
        // a regular file with the downloaded bytes.
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"original");
        let meta = tokio::fs::symlink_metadata(&link).await.unwrap();
        assert!(meta.is_file());
        assert_eq!(tokio::fs::read(&link).await.unwrap(), b"downloaded");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn test_plan_restore_converts_modified_to_system_time() {
        let modified = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let mut entry = file_entry("a.txt", true);
        entry.modified = Some(modified.into());
        let manifest = manifest(vec![ManifestDirectory {
            path: "".to_string(),
            entries: vec![entry],
        }]);
        let plan = plan(&manifest).unwrap();
        assert_eq!(plan.files[0].modified, Some(modified));
    }

    /// A legacy archive's `.env` is a normal manifest entry; the diversion by
    /// file name — at any depth, matcher or no matcher — is what keeps a
    /// restore from writing it as an ordinary file.
    #[test]
    fn plan_restore_diverts_dotenv_by_file_name() {
        let manifest = manifest(vec![
            ManifestDirectory {
                path: "".to_string(),
                entries: vec![file_entry(".env", true), file_entry("notebook.py", true)],
            },
            ManifestDirectory {
                path: "sub".to_string(),
                entries: vec![file_entry(".env", true)],
            },
        ]);
        let plan = plan(&manifest).unwrap();
        assert_eq!(
            plan.files.iter().map(|f| &f.path).collect::<Vec<_>>(),
            vec![&PathBuf::from("notebook.py")]
        );
        assert_eq!(
            plan.secret_files
                .iter()
                .map(|f| &f.path)
                .collect::<Vec<_>>(),
            vec![&PathBuf::from(".env"), &PathBuf::from("sub/.env")]
        );
    }

    /// The archived `.secrets` patterns of a legacy archive divert what they
    /// match, and a matched symlink is dropped rather than materialized.
    #[test]
    fn plan_restore_diverts_matcher_hits_and_drops_matched_symlinks() {
        let mut builder = GitignoreBuilder::new("");
        builder.add_line(None, "*.pem").unwrap();
        let matcher = builder.build().unwrap();
        let manifest = manifest(vec![ManifestDirectory {
            path: "".to_string(),
            entries: vec![
                file_entry("key.pem", true),
                file_entry("notebook.py", true),
                WorkspaceDirEntry {
                    name: "link.pem".to_string(),
                    symlink: Some(WorkspaceDirSymlink {
                        path: Some("key.pem".to_string()),
                    }),
                    ..Default::default()
                },
            ],
        }]);
        let plan = plan_restore(&manifest, &origin(), &matcher).unwrap();
        assert_eq!(
            plan.files.iter().map(|f| &f.path).collect::<Vec<_>>(),
            vec![&PathBuf::from("notebook.py")]
        );
        assert_eq!(
            plan.secret_files
                .iter()
                .map(|f| &f.path)
                .collect::<Vec<_>>(),
            vec![&PathBuf::from("key.pem")]
        );
        assert!(plan.symlinks.is_empty());
    }

    fn restore_options(directory: &Path, secrets: WorkspaceRestoreSecrets) -> RestoreOptions {
        RestoreOptions {
            bucket: "bucket".to_string(),
            key_prefix: None,
            directory: directory.to_path_buf(),
            max_download_concurrency: 1,
            best_effort: false,
            secrets,
        }
    }

    /// The names-only `.env`: keys visible in marimo's panel, values gone,
    /// and nobody but the owner can read even that.
    #[tokio::test]
    async fn a_names_only_restore_writes_placeholders_from_the_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let args = restore_options(dir.path(), WorkspaceRestoreSecrets::NamesOnly);
        let mut manifest = manifest(vec![]);
        manifest.secrets = Some(kubimo::ManifestSecrets {
            env_keys: vec!["API_KEY".to_string(), "TOKEN".to_string()],
            file_paths: vec!["creds/key.pem".to_string()],
        });
        // The S3 client is never touched on this path: names come from the
        // manifest alone.
        let outcome = restore_secrets(&args, &S3Client::from_env(), &manifest, Vec::new())
            .await
            .unwrap();
        assert_eq!(outcome.failed, 0);
        let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert_eq!(env, "API_KEY=\"\"\nTOKEN=\"\"\n");
        let mode = std::fs::metadata(dir.path().join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // The secret file was only mentioned, never created.
        assert!(!dir.path().join("creds").exists());
    }

    /// An empty names section writes nothing — a fresh clone of a workspace
    /// without secrets must not grow an empty `.env`.
    #[tokio::test]
    async fn a_names_only_restore_without_keys_writes_no_env() {
        let dir = tempfile::tempdir().unwrap();
        let args = restore_options(dir.path(), WorkspaceRestoreSecrets::NamesOnly);
        let mut manifest = manifest(vec![]);
        manifest.secrets = Some(kubimo::ManifestSecrets::default());
        restore_secrets(&args, &S3Client::from_env(), &manifest, Vec::new())
            .await
            .unwrap();
        assert!(!dir.path().join(".env").exists());
    }

    #[tokio::test]
    async fn written_secret_values_round_trip_and_reject_traversal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let args = restore_options(dir.path(), WorkspaceRestoreSecrets::Values);
        let secrets = kubimo::WorkspaceSecrets {
            version: kubimo::WorkspaceSecretsVersion::V1,
            workspace: "ws".to_string(),
            env: vec![kubimo::SecretEnvEntry {
                key: "API_KEY".to_string(),
                value: "hunter2".to_string(),
            }],
            files: vec![kubimo::SecretFileEntry {
                path: "creds/key.pem".to_string(),
                size: 6,
                content_base64: Some(base64::engine::general_purpose::STANDARD.encode(b"SECRET")),
            }],
        };
        let outcome = write_secret_values(&args, &secrets).await.unwrap();
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.total, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".env")).unwrap(),
            "API_KEY=\"hunter2\"\n"
        );
        let key = dir.path().join("creds/key.pem");
        assert_eq!(std::fs::read(&key).unwrap(), b"SECRET");
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A malicious secrets object must not write outside the target.
        let evil = kubimo::WorkspaceSecrets {
            files: vec![kubimo::SecretFileEntry {
                path: "../evil.pem".to_string(),
                size: 1,
                content_base64: Some("QQ==".to_string()),
            }],
            ..secrets
        };
        assert!(matches!(
            write_secret_values(&args, &evil).await,
            Err(RestoreError::Plan(PlanError::UnsafePath(_)))
        ));
    }

    #[test]
    fn a_prefix_without_a_trailing_slash_still_rejects_sibling_prefixes() {
        let origin = ArchiveOrigin {
            bucket: "bucket".to_string(),
            key_prefix: Some("workspace/mine".to_string()),
        };
        assert!(
            plan_restore(
                &manifest_pointing_at("s3://bucket/workspace/mine/notebook.py"),
                &origin,
                &Gitignore::empty(),
            )
            .is_ok()
        );
        assert!(matches!(
            plan_restore(
                &manifest_pointing_at("s3://bucket/workspace/mine-other/secret.py"),
                &origin,
                &Gitignore::empty(),
            ),
            Err(PlanError::ForeignContent { .. })
        ));
    }
}
