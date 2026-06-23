use std::path::Path;

use kubimo::{StorageQuantity, StorageUnit};

/// Byte counts describing filesystem usage of a mounted volume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    pub used: u64,
    pub capacity: u64,
    pub available: u64,
}

impl DiskUsage {
    /// Compute usage from raw `statvfs` block counts.
    ///
    /// `frsize` is the fragment (block) size in bytes; `blocks`/`bfree`/`bavail`
    /// are the total / free / available-to-unprivileged block counts.
    fn from_blocks(frsize: u64, blocks: u64, bfree: u64, bavail: u64) -> Self {
        Self {
            used: blocks.saturating_sub(bfree).saturating_mul(frsize),
            capacity: blocks.saturating_mul(frsize),
            available: bavail.saturating_mul(frsize),
        }
    }
}

/// Run `statvfs` on `path` and return the byte usage of the filesystem it lives on.
pub fn disk_usage(path: impl AsRef<Path>) -> rustix::io::Result<DiskUsage> {
    let stat = rustix::fs::statvfs(path.as_ref())?;
    Ok(DiskUsage::from_blocks(
        stat.f_frsize,
        stat.f_blocks,
        stat.f_bfree,
        stat.f_bavail,
    ))
}

/// Represent a raw byte count as a Kubernetes storage quantity (bare bytes).
pub fn storage_quantity(bytes: u64) -> StorageQuantity {
    StorageQuantity::new(bytes as f64, StorageUnit::B)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_used_capacity_available_from_blocks() {
        // 4 KiB blocks, 1000 total, 250 free, 200 available to us.
        let usage = DiskUsage::from_blocks(4096, 1000, 250, 200);
        assert_eq!(usage.capacity, 1000 * 4096);
        assert_eq!(usage.used, 750 * 4096);
        assert_eq!(usage.available, 200 * 4096);
    }

    #[test]
    fn saturates_instead_of_underflowing() {
        // free > total should never panic; used clamps to zero.
        let usage = DiskUsage::from_blocks(4096, 100, 200, 0);
        assert_eq!(usage.used, 0);
    }
}
