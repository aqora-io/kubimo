//! Bind mounts, the mechanism that hands a slot to a runner pod.
//!
//! kubelet may retry `NodePublishVolume`/`NodeUnpublishVolume` after a partial
//! failure or a restart, and the CSI spec requires both to be idempotent, so
//! every operation here checks the current state first.

use std::io;
use std::path::Path;

use rustix::mount::{MountFlags, UnmountFlags, mount_bind, unmount};

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("creating mount point {path:?}: {source}")]
    CreateTarget { path: String, source: io::Error },
    #[error("reading mount table: {0}")]
    MountTable(io::Error),
    #[error("bind mounting {source_path:?} onto {target:?}: {source}")]
    Bind {
        source_path: String,
        target: String,
        source: rustix::io::Errno,
    },
    #[error("making {target:?} read-only: {source}")]
    Remount {
        target: String,
        source: rustix::io::Errno,
    },
    #[error("unmounting {target:?}: {source}")]
    Unmount {
        target: String,
        source: rustix::io::Errno,
    },
}

/// What is actually mounted at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    /// Nothing mounted here.
    Absent,
    /// Mounted and answering I/O.
    Live,
    /// A mount entry exists but the filesystem behind it is gone.
    ///
    /// Observed on Scaleway: when the pod holding a block volume is deleted,
    /// the volume detaches ~10s later while the mount entry survives. The path
    /// still looks mounted in `/proc/self/mountinfo`, but every operation
    /// returns `EIO`, and a fresh `mount` over it fails. Treating this as
    /// "already mounted" would hand a runner a dead filesystem.
    Stale,
}

/// Whether `path` appears in the mount table.
///
/// Compares against `/proc/self/mountinfo` rather than the classic
/// "st_dev differs from parent" trick, which gives a false negative for a bind
/// mount of a directory onto a target on the same filesystem — exactly the case
/// here, since slot and target both live on the node data volume.
fn in_mount_table(path: &Path) -> Result<bool, MountError> {
    let target = path.to_string_lossy();
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").map_err(MountError::MountTable)?;
    Ok(mountinfo.lines().any(|line| {
        // mountinfo field 5 (1-indexed) is the mount point, and octal-escapes
        // spaces and tabs, which a slot id can never contain.
        line.split(' ').nth(4).is_some_and(|point| point == target)
    }))
}

/// Whether the filesystem at `path` still answers I/O.
///
/// Uses `read_dir` rather than `stat`, because `stat` on the mount point can be
/// served from cache after the backing device disappears, while `opendir` on a
/// detached filesystem reliably fails.
fn is_responsive(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Err(_) => false,
        // An empty but healthy directory yields `None`, which is fine; only an
        // actual error from the first read means the filesystem is gone.
        Ok(mut entries) => !matches!(entries.next(), Some(Err(_))),
    }
}

pub fn mount_state(path: &Path) -> Result<MountState, MountError> {
    if !in_mount_table(path)? {
        return Ok(MountState::Absent);
    }
    Ok(if is_responsive(path) {
        MountState::Live
    } else {
        MountState::Stale
    })
}

/// Superblock mount options of the mount that contains `path`.
///
/// Returns the options of the *longest* matching mount point, which is the one
/// actually backing the path when mounts are nested.
pub fn super_options_for(path: &Path) -> Result<Option<String>, MountError> {
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").map_err(MountError::MountTable)?;
    Ok(parse_super_options(&mountinfo, &path.to_string_lossy()))
}

/// Pure half of [`super_options_for`], split out so the format handling is
/// testable without a live `/proc`.
fn parse_super_options(mountinfo: &str, path: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // Format: id parent major:minor root mountpoint opts [tags...] - fstype source superopts
        // The optional tag list before the "-" separator is variable length, so
        // the trailing fields have to be found relative to that separator.
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let (Some(mount_point), Some(super_options)) = (fields.get(4), fields.get(separator + 3))
        else {
            continue;
        };
        let contains = *mount_point == path
            || (path.starts_with(mount_point)
                && (mount_point.ends_with('/')
                    || path.as_bytes().get(mount_point.len()) == Some(&b'/')));
        if contains
            && best
                .as_ref()
                .is_none_or(|(len, _)| mount_point.len() > *len)
        {
            best = Some((mount_point.len(), (*super_options).to_string()));
        }
    }
    best.map(|(_, options)| options)
}

/// Bind `source_path` onto `target`, creating `target` if needed.
///
/// Returns `false` if the target was already a mount point, so the caller can
/// distinguish a fresh publish from a retry.
pub fn bind(source_path: &Path, target: &Path, read_only: bool) -> Result<bool, MountError> {
    match mount_state(target)? {
        MountState::Live => return Ok(false),
        // Clear the corpse before mounting over it: a stale mount both hides
        // the real directory and makes a fresh mount fail with EIO.
        MountState::Stale => {
            tracing::warn!(
                target = %target.display(),
                "clearing a stale mount whose backing device disappeared"
            );
            detach(target)?;
        }
        MountState::Absent => {}
    }
    std::fs::create_dir_all(target).map_err(|source| MountError::CreateTarget {
        path: target.display().to_string(),
        source,
    })?;
    mount_bind(source_path, target).map_err(|source| MountError::Bind {
        source_path: source_path.display().to_string(),
        target: target.display().to_string(),
        source,
    })?;
    if read_only {
        // A read-only bind needs a second remount call: the `ro` flag is
        // ignored on the initial bind, which would silently give a Render
        // runner write access to the workspace.
        rustix::mount::mount_remount(target, MountFlags::BIND | MountFlags::RDONLY, "").map_err(
            |source| MountError::Remount {
                target: target.display().to_string(),
                source,
            },
        )?;
    }
    Ok(true)
}

/// Unmount `target` if it is mounted.
///
/// Returns `false` if there was nothing mounted, which the CSI spec treats as
/// success for a repeated `NodeUnpublishVolume`.
pub fn unbind(target: &Path) -> Result<bool, MountError> {
    if mount_state(target)? == MountState::Absent {
        return Ok(false);
    }
    detach(target)?;
    Ok(true)
}

/// Lazily unmount `target`.
///
/// `MNT_DETACH` rather than a plain unmount: it is the only thing that clears a
/// stale mount whose device is gone, and for a live mount it detaches
/// immediately while letting any straggling references drain.
fn detach(target: &Path) -> Result<(), MountError> {
    unmount(target, UnmountFlags::DETACH).map_err(|source| MountError::Unmount {
        target: target.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/` is a mount point in any environment this runs in; a path that does
    /// not exist is not.
    #[test]
    fn detects_real_mount_points() {
        assert!(in_mount_table(Path::new("/")).unwrap());
        assert!(!in_mount_table(Path::new("/definitely/not/mounted/xyzzy")).unwrap());
    }

    #[test]
    fn root_is_a_live_mount_and_a_missing_path_is_absent() {
        assert_eq!(mount_state(Path::new("/")).unwrap(), MountState::Live);
        assert_eq!(
            mount_state(Path::new("/definitely/not/mounted/xyzzy")).unwrap(),
            MountState::Absent
        );
    }

    /// A healthy empty directory must read as responsive — `read_dir` yields
    /// `None`, not an error, and treating that as dead would make the agent
    /// unmount live slots.
    #[test]
    fn an_empty_directory_is_responsive() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_responsive(dir.path()));
    }

    #[test]
    fn a_missing_directory_is_not_responsive() {
        assert!(!is_responsive(Path::new("/definitely/not/here/xyzzy")));
    }

    /// Guards the parse: field 5 is the mount point. Picking the wrong field
    /// would make `bind` think every target is already mounted and silently
    /// skip the mount, handing the pod an empty directory.
    #[test]
    fn mountinfo_field_five_is_the_mount_point() {
        let line = "23 28 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw";
        assert_eq!(line.split(' ').nth(4), Some("/proc"));
    }

    const SAMPLE: &str = "\
23 28 0:22 / /proc rw,relatime shared:12 - proc proc rw
30 25 8:1 / /data rw,relatime - xfs /dev/sda1 rw,attr2,inode64,prjquota
31 30 8:1 /nested /data/nested rw,relatime - xfs /dev/sda1 rw,attr2,inode64
32 25 8:2 / /plain rw,relatime shared:4 master:2 - ext4 /dev/sdb rw";

    /// The optional tag list before "-" is variable length, so super options
    /// must be located relative to the separator, not at a fixed index.
    #[test]
    fn finds_super_options_across_variable_optional_fields() {
        assert_eq!(
            parse_super_options(SAMPLE, "/data").as_deref(),
            Some("rw,attr2,inode64,prjquota")
        );
        // This line carries two optional tags before the separator.
        assert_eq!(parse_super_options(SAMPLE, "/plain").as_deref(), Some("rw"));
    }

    /// A file inside the mount resolves to that mount's options.
    #[test]
    fn resolves_a_path_to_its_containing_mount() {
        assert_eq!(
            parse_super_options(SAMPLE, "/data/slots/slot-x").as_deref(),
            Some("rw,attr2,inode64,prjquota")
        );
    }

    /// Nested mounts must resolve to the longest match, not the first one.
    #[test]
    fn prefers_the_longest_matching_mount_point() {
        assert_eq!(
            parse_super_options(SAMPLE, "/data/nested/file").as_deref(),
            Some("rw,attr2,inode64"),
            "should pick /data/nested, not /data"
        );
    }

    /// `/database` must not match the `/data` mount.
    #[test]
    fn does_not_treat_a_name_prefix_as_a_path_prefix() {
        assert_eq!(parse_super_options(SAMPLE, "/database"), None);
    }
}
