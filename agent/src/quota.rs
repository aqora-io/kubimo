//! XFS project quotas: the per-slot capacity limit.
//!
//! Two kernel interfaces are involved, and both are needed:
//!
//! 1. `FS_IOC_FSSETXATTR` stamps a project id onto the slot directory's inode
//!    and sets `FS_XFLAG_PROJINHERIT`, so everything created underneath
//!    inherits it. This is what `xfs_quota project -s` does.
//! 2. `quotactl_fd(Q_XSETQLIM)` sets the block limit for that project id.
//!
//! We use `quotactl_fd` (syscall 443, Linux 5.14+) rather than `quotactl`
//! because `quotactl` takes a *block device path*, and a container does not
//! have one: the agent sees the volume mounted at `/data`, while
//! `/proc/mounts` names it `/dev/disk/by-id/scsi-0SCW_sbs_volume-…`, a udev
//! symlink that does not exist inside the container's `/dev`. `quotactl_fd`
//! takes an fd on the filesystem instead, which we always have.
//!
//! Enforcement requires the filesystem to be mounted `prjquota` (accounting
//! *and* enforcement). `pqnoenforce` accounts without enforcing and, notably,
//! also disables the `statvfs` override that makes a slot report its quota as
//! its filesystem size.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// `FS_XFLAG_PROJINHERIT` — children inherit the directory's project id.
const FS_XFLAG_PROJINHERIT: u32 = 0x0000_0200;

/// Ioctl direction bits (`_IOC_READ` / `_IOC_WRITE` from `asm-generic/ioctl.h`).
const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

const FS_DQUOT_VERSION: i8 = 1;
/// `FS_PROJ_QUOTA` in `d_flags`.
const FS_PROJ_QUOTA: i8 = 4;
/// `FS_DQ_BHARD | FS_DQ_BSOFT` — the only fields we set.
const FS_DQ_BSOFT: u16 = 0x0004;
const FS_DQ_BHARD: u16 = 0x0008;

/// `PRJQUOTA` quota type.
const PRJQUOTA: u32 = 2;
/// `Q_XSETQLIM` = `XQM_CMD(4)` = `('X' << 8) + 4`.
const Q_XSETQLIM: u32 = (b'X' as u32) << 8 | 4;
const SUBCMDSHIFT: u32 = 8;

/// XFS quota block limits are expressed in 512-byte "basic blocks".
const BASIC_BLOCK_BYTES: u64 = 512;

const SYS_QUOTACTL_FD: libc::c_long = 443;

/// `struct fsxattr` from `linux/fs.h`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct FsXAttr {
    fsx_xflags: u32,
    fsx_extsize: u32,
    fsx_nextents: u32,
    fsx_projid: u32,
    fsx_cowextsize: u32,
    fsx_pad: [u8; 8],
}

/// `struct fs_disk_quota` from `linux/dqblk_xfs.h`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct FsDiskQuota {
    d_version: i8,
    d_flags: i8,
    d_fieldmask: u16,
    d_id: u32,
    d_blk_hardlimit: u64,
    d_blk_softlimit: u64,
    d_ino_hardlimit: u64,
    d_ino_softlimit: u64,
    d_bcount: u64,
    d_icount: u64,
    d_itimer: i32,
    d_btimer: i32,
    d_iwarns: u16,
    d_bwarns: u16,
    d_itimer_hi: i8,
    d_btimer_hi: i8,
    d_rtbtimer_hi: i8,
    d_padding2: i8,
    d_rtb_hardlimit: u64,
    d_rtb_softlimit: u64,
    d_rtbcount: u64,
    d_rtbtimer: i32,
    d_rtbwarns: u16,
    d_padding3: i16,
    d_padding4: [u8; 8],
}

/// Build an ioctl request number the way `_IOR`/`_IOW` do.
const fn ioc(dir: u32, type_: u8, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | ((type_ as u32) << 8) | nr
}

fn fs_ioc_fsgetxattr() -> u32 {
    ioc(IOC_READ, b'X', 31, size_of::<FsXAttr>() as u32)
}

fn fs_ioc_fssetxattr() -> u32 {
    ioc(IOC_WRITE, b'X', 32, size_of::<FsXAttr>() as u32)
}

/// `QCMD(cmd, type)`.
const fn qcmd(cmd: u32, type_: u32) -> u32 {
    (cmd << SUBCMDSHIFT) | (type_ & 0x00ff)
}

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("opening {path:?}: {source}")]
    Open { path: String, source: io::Error },
    #[error("reading project attributes of {path:?}: {source}")]
    GetXAttr { path: String, source: io::Error },
    #[error("assigning project {project_id} to {path:?}: {source}")]
    SetXAttr {
        path: String,
        project_id: u32,
        source: io::Error,
    },
    #[error(
        "setting the limit for project {project_id}: {source} \
         (is the filesystem mounted with `prjquota`?)"
    )]
    SetLimit { project_id: u32, source: io::Error },
    #[error("reading mount options: {0}")]
    MountOptions(String),
}

/// Whether the filesystem holding `fs_root` is mounted with project quota
/// *enforcement*.
///
/// `pquota`/`prjquota` mean accounting **and** enforcement; `pqnoenforce`
/// accounts only, and — importantly — also disables the `statvfs` override that
/// makes a slot report its quota as its filesystem size, which is what keeps
/// the indexer's disk-usage reporting honest. So `pqnoenforce` deliberately
/// does not count as supported.
pub fn project_quota_enforced(fs_root: &Path) -> Result<bool, QuotaError> {
    let options = crate::mount::super_options_for(fs_root)
        .map_err(|err| QuotaError::MountOptions(err.to_string()))?;
    Ok(options.is_some_and(|options| {
        options
            .split(',')
            .any(|option| option == "prjquota" || option == "pquota")
    }))
}

/// Convert a byte limit to XFS basic blocks, rounding **up** so a slot never
/// gets silently less than it was promised.
fn bytes_to_basic_blocks(bytes: u64) -> u64 {
    bytes.div_ceil(BASIC_BLOCK_BYTES)
}

/// Stamp `project_id` onto `dir` and mark it project-inheriting.
///
/// Must run before anything is written into the directory: inodes created
/// beforehand keep the old project and would not be accounted.
pub fn assign_project(dir: &Path, project_id: u32) -> Result<(), QuotaError> {
    let file = File::open(dir).map_err(|source| QuotaError::Open {
        path: dir.display().to_string(),
        source,
    })?;
    let mut attr = FsXAttr::default();
    // SAFETY: `attr` is a correctly sized `struct fsxattr` and the fd is open.
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            fs_ioc_fsgetxattr() as libc::Ioctl,
            &raw mut attr,
        )
    };
    if rc < 0 {
        return Err(QuotaError::GetXAttr {
            path: dir.display().to_string(),
            source: io::Error::last_os_error(),
        });
    }
    attr.fsx_projid = project_id;
    attr.fsx_xflags |= FS_XFLAG_PROJINHERIT;
    // SAFETY: as above; the kernel only reads from `attr` here.
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            fs_ioc_fssetxattr() as libc::Ioctl,
            &raw const attr,
        )
    };
    if rc < 0 {
        return Err(QuotaError::SetXAttr {
            path: dir.display().to_string(),
            project_id,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Apply a hard block limit to `project_id`.
///
/// Soft and hard are set to the same value: a soft limit only starts a grace
/// timer, and we want the write to fail at the boundary rather than days later.
pub fn set_project_limit(
    fs_root: &Path,
    project_id: u32,
    limit_bytes: u64,
) -> Result<(), QuotaError> {
    let file = File::open(fs_root).map_err(|source| QuotaError::Open {
        path: fs_root.display().to_string(),
        source,
    })?;
    let blocks = bytes_to_basic_blocks(limit_bytes);
    let quota = FsDiskQuota {
        d_version: FS_DQUOT_VERSION,
        d_flags: FS_PROJ_QUOTA,
        d_fieldmask: FS_DQ_BHARD | FS_DQ_BSOFT,
        d_id: project_id,
        d_blk_hardlimit: blocks,
        d_blk_softlimit: blocks,
        ..Default::default()
    };
    // SAFETY: syscall 443 is `quotactl_fd(fd, cmd, id, addr)`; `quota` is a
    // correctly laid out `struct fs_disk_quota` that the kernel only reads.
    let rc = unsafe {
        libc::syscall(
            SYS_QUOTACTL_FD,
            file.as_raw_fd() as libc::c_int,
            qcmd(Q_XSETQLIM, PRJQUOTA) as libc::c_uint,
            project_id as libc::c_uint,
            &raw const quota as *const libc::c_void,
        )
    };
    if rc < 0 {
        return Err(QuotaError::SetLimit {
            project_id,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Struct layout is an ABI contract with the kernel; a wrong size makes the
    /// ioctl number wrong and the call fails with EINVAL at best, or corrupts
    /// memory at worst.
    #[test]
    fn kernel_struct_sizes_match_the_abi() {
        // 5 * u32 + 8 bytes of padding.
        assert_eq!(size_of::<FsXAttr>(), 28);
        // Per linux/dqblk_xfs.h. `d_id` sits at offset 4, so `d_blk_hardlimit`
        // is already 8-aligned at offset 8 and the struct carries no interior
        // padding: 8 + 6*8 (blk/ino/count limits) + 2*4 (timers) + 2*2 (warns)
        // + 4*1 (hi timers) + 3*8 (rtb) + 4 + 2 + 2 + 8 = 112.
        assert_eq!(size_of::<FsDiskQuota>(), 112);
    }

    /// Compare against the constants the kernel headers expand to, so a change
    /// to `FsXAttr` cannot silently shift the ioctl number.
    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(fs_ioc_fsgetxattr(), 0x801c_581f, "FS_IOC_FSGETXATTR");
        assert_eq!(fs_ioc_fssetxattr(), 0x401c_5820, "FS_IOC_FSSETXATTR");
    }

    #[test]
    fn qcmd_matches_xfs_quota_encoding() {
        // QCMD(Q_XSETQLIM, PRJQUOTA)
        assert_eq!(qcmd(Q_XSETQLIM, PRJQUOTA), 0x0058_0402);
    }

    /// Rounding down would hand out less than promised; a 1-byte limit must
    /// still reserve a whole basic block.
    #[test]
    fn byte_limits_round_up_to_whole_basic_blocks() {
        assert_eq!(bytes_to_basic_blocks(0), 0);
        assert_eq!(bytes_to_basic_blocks(1), 1);
        assert_eq!(bytes_to_basic_blocks(512), 1);
        assert_eq!(bytes_to_basic_blocks(513), 2);
        // 64 GiB, the platform's spec.storage.max
        assert_eq!(bytes_to_basic_blocks(64 * 1024 * 1024 * 1024), 134_217_728);
    }
}
