use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use kubimo::{WorkspaceManifest, url::Url};
use thiserror::Error;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::DownloadArgs;
use crate::disk;
use crate::s3::{DownloadError, S3Client};

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
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("unsafe path in manifest: {0}")]
    UnsafePath(String),
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
/// rejected. Marimo meta/cache urls are ignored — they are derived artifacts.
pub fn plan_restore(manifest: &WorkspaceManifest) -> Result<RestorePlan, PlanError> {
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
                if let Some(target) = &symlink.path {
                    plan.symlinks.push((entry_path, PathBuf::from(target)));
                } else {
                    plan.skipped.push(entry_path);
                }
            } else if let Some(file) = &entry.file {
                if let Some(content) = &file.content {
                    plan.files.push(RestoreFile {
                        path: entry_path,
                        url: content.url.clone(),
                        crc32: content.crc32,
                        modified: entry.modified.map(Into::into),
                    });
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
pub async fn restore(args: &DownloadArgs, s3: &S3Client) -> Result<(), RestoreError> {
    let manifest_url = kubimo::manifest_url(&args.bucket, args.key_prefix.as_deref())?;
    let bytes = s3.get_bytes(&manifest_url).await?;
    let manifest: WorkspaceManifest = serde_json::from_slice(&bytes)?;
    if !manifest.upload_content {
        return Err(RestoreError::NoContent);
    }
    let plan = plan_restore(&manifest)?;

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
    if failed > 0 && !args.best_effort {
        return Err(RestoreError::Failed(failed, total));
    }
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
        }
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
        let plan = plan_restore(&manifest).unwrap();
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
        let plan = plan_restore(&manifest).unwrap();
        assert!(plan.directories.contains(&PathBuf::from("orphan")));
    }

    #[test]
    fn test_plan_restore_rejects_parent_dir_in_path() {
        let manifest = manifest(vec![ManifestDirectory {
            path: "../evil".to_string(),
            entries: vec![],
        }]);
        assert!(matches!(
            plan_restore(&manifest),
            Err(PlanError::UnsafePath(_))
        ));
    }

    #[test]
    fn test_plan_restore_rejects_absolute_path() {
        let manifest = manifest(vec![ManifestDirectory {
            path: "/etc".to_string(),
            entries: vec![],
        }]);
        assert!(matches!(
            plan_restore(&manifest),
            Err(PlanError::UnsafePath(_))
        ));
    }

    #[test]
    fn test_plan_restore_rejects_unsafe_entry_name() {
        for name in ["..", "a/b"] {
            let manifest = manifest(vec![ManifestDirectory {
                path: "".to_string(),
                entries: vec![file_entry(name, true)],
            }]);
            assert!(
                matches!(plan_restore(&manifest), Err(PlanError::UnsafePath(_))),
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
        let plan = plan_restore(&manifest).unwrap();
        assert_eq!(plan.files[0].modified, Some(modified));
    }
}
