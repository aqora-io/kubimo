//! Seeding a slot's virtualenv from a node-local template.
//!
//! The marimo image carries a ~920MB venv at `/home/me/venv` that is a near
//! duplicate of its own system site-packages. The dedicated path copies that
//! onto every workspace volume (`cp -a /home/me/.`), which is why a fresh 2Gi
//! workspace reports ~962MB used before the user writes anything, and why
//! `uv sync` on a cold slot is slow.
//!
//! Here the template is materialised **once per node** by an init container on
//! the agent DaemonSet, and each slot gets a reflink copy: a new inode sharing
//! extents copy-on-write. Physical disk stays shared, the slot's own writes are
//! private, and the copy is O(extents) rather than O(bytes).
//!
//! `--reflink=auto` rather than `=always` deliberately: XFS supports reflink
//! (verified on the Scaleway data volume), but a dev cluster on ext4 does not,
//! and falling back to a real copy is better than refusing to start.

use std::path::Path;
use std::process::Stdio;

#[derive(Debug, thiserror::Error)]
pub enum VenvError {
    #[error("running cp: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("copying the venv template failed: {0}")]
    Copy(String),
}

/// Where the DaemonSet's init container leaves the template.
pub const TEMPLATE_SUBDIR: &str = "home-template";

/// Virtualenv directory inside a slot.
///
/// Must stay `venv`: the path `/home/me/venv` is written into every workspace's
/// tracked `pyproject.toml` as `[tool.marimo.venv] path`, so it is user data,
/// not an implementation detail we can relocate.
pub const VENV_SUBDIR: &str = "venv";

/// Seed a slot from the node's `/home/me` template.
///
/// Covers the whole home skeleton, not just the venv: a brand-new workspace
/// also needs `workspace/pyproject.toml`, without which `uv sync` fails with
/// "No `pyproject.toml` found" and the runner never starts. This is the same
/// content the dedicated path's `init-dirs` container copies with
/// `cp -a /home/me/. /mnt`.
///
/// Runs *before* hydration, so a workspace that has an archive overlays its own
/// files on top of the skeleton — matching the dedicated ordering of
/// init-dirs then restore.
///
/// Returns `false` when no template has been staged on this node, which is not
/// an error: the runner falls back to building its own, exactly as today.
pub async fn seed_from_template(data_root: &Path, slot_dir: &Path) -> Result<bool, VenvError> {
    let template = data_root.join(TEMPLATE_SUBDIR);
    if !template.is_dir() {
        return Ok(false);
    }
    if slot_dir.join(VENV_SUBDIR).exists() {
        // A reused slot already has its venv; overwriting would discard
        // packages the tenant installed.
        return Ok(false);
    }
    // Trailing `/.` copies the *contents* into the existing slot directory
    // rather than nesting a `home-template` directory inside it.
    let output = tokio::process::Command::new("cp")
        .arg("--archive")
        .arg("--reflink=auto")
        .arg(format!("{}/.", template.display()))
        .arg(slot_dir)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(VenvError::Copy(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_template_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        assert!(!seed_from_template(dir.path(), &slot).await.unwrap());
    }

    /// The skeleton must include `workspace/pyproject.toml`, not just the
    /// venv: without it `uv sync` fails and the runner never starts. Observed
    /// on staging before the template covered the whole home directory.
    #[tokio::test]
    async fn seeds_both_the_workspace_skeleton_and_the_venv() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join(TEMPLATE_SUBDIR);
        std::fs::create_dir_all(template.join(VENV_SUBDIR).join("lib")).unwrap();
        std::fs::write(template.join(VENV_SUBDIR).join("lib/marimo.py"), b"x").unwrap();
        std::fs::create_dir_all(template.join("workspace")).unwrap();
        std::fs::write(template.join("workspace/pyproject.toml"), b"[project]").unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();

        assert!(seed_from_template(dir.path(), &slot).await.unwrap());
        assert_eq!(
            std::fs::read(slot.join(VENV_SUBDIR).join("lib/marimo.py")).unwrap(),
            b"x"
        );
        assert_eq!(
            std::fs::read(slot.join("workspace/pyproject.toml")).unwrap(),
            b"[project]"
        );
        // Contents, not a nested `home-template` directory.
        assert!(!slot.join(TEMPLATE_SUBDIR).exists());
    }

    /// A reused slot keeps whatever the tenant installed; re-seeding would
    /// silently discard their packages.
    #[tokio::test]
    async fn an_existing_venv_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join(TEMPLATE_SUBDIR);
        std::fs::create_dir_all(template.join(VENV_SUBDIR)).unwrap();
        std::fs::write(template.join("from-template"), b"t").unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir_all(slot.join(VENV_SUBDIR)).unwrap();
        std::fs::write(slot.join(VENV_SUBDIR).join("tenant-installed"), b"m").unwrap();

        assert!(!seed_from_template(dir.path(), &slot).await.unwrap());
        assert!(slot.join(VENV_SUBDIR).join("tenant-installed").exists());
        assert!(!slot.join(VENV_SUBDIR).join("from-template").exists());
    }
}
