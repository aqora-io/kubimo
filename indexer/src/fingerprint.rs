//! Skipping work for files that have not changed.
//!
//! The upload path reads a file end-to-end to compute its crc32 *before* it can
//! consult the S3 cache marker, and then issues a HEAD to compare ETags. That
//! is fine when a walk happens once per workspace, but the watcher re-walks the
//! whole tree on every filesystem event: touching one notebook costs a full
//! read of every file in the workspace plus a HEAD per file.
//!
//! A (mtime, size) fingerprint short-circuits that. It is the same signal `make`
//! and rsync use by default: cheap to obtain from the `stat` the walk already
//! performs, and wrong only if a file is modified within the timestamp's
//! resolution *without* changing length. The 60s poll re-walk bounds how long
//! such a change could go unnoticed, and the trade is very much worth it —
//! without it an idle-but-watched workspace re-reads itself continuously.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use kubimo::WorkspaceDirContentUrl;
use tokio::sync::RwLock;

/// What was true about a file the last time it was successfully uploaded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    modified: Option<SystemTime>,
    size: u64,
    content: WorkspaceDirContentUrlKey,
}

/// `WorkspaceDirContentUrl` is not `Eq`, so keep the parts needed to rebuild it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceDirContentUrlKey {
    url: String,
    crc32: Option<u32>,
    e_tag: Option<String>,
}

/// Remembers which files are already in S3 unchanged.
#[derive(Clone, Default)]
pub struct ContentCache {
    entries: Arc<RwLock<HashMap<PathBuf, Fingerprint>>>,
}

impl ContentCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The previously uploaded content for `path`, if it still looks identical.
    ///
    /// A missing mtime (filesystems that do not report one) never matches, so
    /// the file is re-read rather than assumed unchanged.
    pub async fn get(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        size: u64,
    ) -> Option<WorkspaceDirContentUrl> {
        let entries = self.entries.read().await;
        let found = entries.get(path)?;
        if found.size != size || found.modified.is_none() || found.modified != modified {
            return None;
        }
        Some(WorkspaceDirContentUrl {
            url: found.content.url.parse().ok()?,
            crc32: found.content.crc32,
            e_tag: found.content.e_tag.clone(),
        })
    }

    pub async fn insert(
        &self,
        path: PathBuf,
        modified: Option<SystemTime>,
        size: u64,
        content: &WorkspaceDirContentUrl,
    ) {
        self.entries.write().await.insert(
            path,
            Fingerprint {
                modified,
                size,
                content: WorkspaceDirContentUrlKey {
                    url: content.url.to_string(),
                    crc32: content.crc32,
                    e_tag: content.e_tag.clone(),
                },
            },
        );
    }

    /// Drop paths that no longer exist, so a long-lived watcher does not grow
    /// without bound as files come and go.
    pub async fn retain_paths(&self, live: &std::collections::BTreeSet<PathBuf>) {
        self.entries
            .write()
            .await
            .retain(|path, _| live.contains(path));
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.entries.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn content(url: &str) -> WorkspaceDirContentUrl {
        WorkspaceDirContentUrl {
            url: url.parse().unwrap(),
            crc32: Some(7),
            e_tag: Some("etag".into()),
        }
    }

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[tokio::test]
    async fn an_unchanged_file_hits() {
        let cache = ContentCache::new();
        let path = PathBuf::from("a.py");
        cache
            .insert(path.clone(), at(100), 42, &content("s3://b/k"))
            .await;
        let hit = cache.get(&path, at(100), 42).await.unwrap();
        assert_eq!(hit.url.to_string(), "s3://b/k");
        assert_eq!(hit.crc32, Some(7));
    }

    #[tokio::test]
    async fn a_changed_mtime_or_size_misses() {
        let cache = ContentCache::new();
        let path = PathBuf::from("a.py");
        cache
            .insert(path.clone(), at(100), 42, &content("s3://b/k"))
            .await;
        assert!(cache.get(&path, at(101), 42).await.is_none(), "mtime");
        assert!(cache.get(&path, at(100), 43).await.is_none(), "size");
    }

    /// A rewrite that keeps the length must still be caught when the timestamp
    /// moves — this is the common case for editing a notebook in place.
    #[tokio::test]
    async fn a_same_size_rewrite_with_a_new_mtime_misses() {
        let cache = ContentCache::new();
        let path = PathBuf::from("a.py");
        cache
            .insert(path.clone(), at(100), 42, &content("s3://b/k"))
            .await;
        assert!(cache.get(&path, at(200), 42).await.is_none());
    }

    /// Without a usable timestamp we cannot claim the file is unchanged.
    #[tokio::test]
    async fn a_missing_mtime_never_hits() {
        let cache = ContentCache::new();
        let path = PathBuf::from("a.py");
        cache
            .insert(path.clone(), None, 42, &content("s3://b/k"))
            .await;
        assert!(cache.get(&path, None, 42).await.is_none());
    }

    #[tokio::test]
    async fn an_unknown_path_misses() {
        let cache = ContentCache::new();
        assert!(cache.get(Path::new("nope.py"), at(1), 1).await.is_none());
    }

    #[tokio::test]
    async fn deleted_paths_are_pruned() {
        let cache = ContentCache::new();
        cache
            .insert(PathBuf::from("keep.py"), at(1), 1, &content("s3://b/1"))
            .await;
        cache
            .insert(PathBuf::from("gone.py"), at(1), 1, &content("s3://b/2"))
            .await;
        assert_eq!(cache.len().await, 2);
        cache
            .retain_paths(&[PathBuf::from("keep.py")].into_iter().collect())
            .await;
        assert_eq!(cache.len().await, 1);
        assert!(cache.get(Path::new("gone.py"), at(1), 1).await.is_none());
    }
}
