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
    let mut source = template.into_os_string();
    source.push("/.");
    let output = tokio::process::Command::new("cp")
        .arg("--archive")
        .arg("--reflink=auto")
        .arg(source)
        .arg(slot_dir)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        // Whatever landed is an arbitrary prefix of the skeleton, and the
        // skip-if-present check above would keep it forever. The quota is
        // applied before this runs, so a workspace whose `spec.storage.max` is
        // below the ~920MB template reliably gets EDQUOT partway.
        //
        // The whole slot goes, not just the venv: `cp` copies the entire home
        // skeleton, so it can equally die on a truncated
        // `workspace/pyproject.toml` — the one file whose absence or corruption
        // makes `uv sync` fail and the runner never start. Seeding only ever
        // runs on a freshly created slot, so there is nothing here but our own
        // partial copy. Clearing it puts the slot back to "no template staged",
        // where the runner builds its own venv — slow, but working.
        if let Err(err) = clear_dir_contents(slot_dir) {
            tracing::error!(
                %err,
                slot = %slot_dir.display(),
                "could not clear a partially copied venv template"
            );
        }
        return Err(VenvError::Copy(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(true)
}

/// Remove everything inside `dir`, keeping `dir` itself.
///
/// The directory has to stay: it is the slot, already stamped with its XFS
/// project id and chowned to the runner, and recreating it would drop both.
fn clear_dir_contents(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // `file_type` on the entry, not `metadata`: it does not follow symlinks,
        // so a link to a directory is unlinked rather than recursed into.
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
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

    /// A copy that dies partway — the slot's quota is applied before it runs,
    /// so any workspace smaller than the ~920MB template hits `EDQUOT` mid-tree
    /// — must not leave a half skeleton behind. The caller only warns, so the
    /// slot would be published with a venv missing an arbitrary subset of its
    /// packages, and the skip-if-present check would never let it be repaired.
    ///
    /// The whole slot is cleared, not just the venv: `cp` can equally die on a
    /// truncated `workspace/pyproject.toml`, and that one makes `uv sync` fail
    /// outright. Which files landed before the failure is up to readdir order,
    /// so the assertion is that *nothing* survives.
    #[tokio::test]
    async fn a_failed_copy_leaves_no_venv_and_the_next_call_re_seeds() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join(TEMPLATE_SUBDIR);
        std::fs::create_dir_all(template.join(VENV_SUBDIR).join("lib")).unwrap();
        std::fs::write(template.join(VENV_SUBDIR).join("lib/marimo.py"), b"x").unwrap();
        std::fs::create_dir_all(template.join("workspace")).unwrap();
        std::fs::write(template.join("workspace/pyproject.toml"), b"[project]").unwrap();
        // Unreadable to this (non-root) process, so `cp` copies part of the
        // tree and then fails — standing in for the `EDQUOT` partway through.
        let locked = template.join(VENV_SUBDIR).join("locked.py");
        std::fs::write(&locked, b"y").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();

        let err = seed_from_template(dir.path(), &slot).await.unwrap_err();
        assert!(matches!(err, VenvError::Copy(_)), "{err:?}");
        let leftovers: Vec<_> = std::fs::read_dir(&slot)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a partial skeleton would be published as if it were complete: {leftovers:?}"
        );
        assert!(slot.is_dir(), "the slot itself must survive");

        // With the obstacle gone the next publish re-seeds, rather than
        // skipping because a venv is already there.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(seed_from_template(dir.path(), &slot).await.unwrap());
        assert_eq!(
            std::fs::read(slot.join(VENV_SUBDIR).join("lib/marimo.py")).unwrap(),
            b"x"
        );
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
