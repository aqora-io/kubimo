//! Seeding a slot from a node-local template.
//!
//! The marimo image carries pre-seeded pixi/uv package caches under
//! `/home/me` — plus a pre-built environment for the canonical `readme.py` —
//! instead of a venv. Every kernel builds its own per-notebook environment
//! lazily, so nothing here is required for a runner to start; seeding just
//! saves the first build from a cold cache.
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
pub enum TemplateError {
    #[error("running cp: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("copying the node template failed: {0}")]
    Copy(String),
}

/// Where the DaemonSet's init container stages the node template.
///
/// Must equal the chart's `/data/template` mount.
pub const TEMPLATE_SUBDIR: &str = "template";

/// Seed a slot from the node's `/data/template`.
///
/// Runs *before* hydration, so a workspace that has an archive overlays its
/// own files on top of whatever the template seeded.
///
/// Only ever seeds a slot this publish just created: if `slot_dir` already has
/// any entry — an earlier seed, a recycled slot id, tenant files — this
/// returns `Ok(false)` and leaves it untouched rather than overlaying it.
///
/// Returns `false` when no template has been staged on this node, which is not
/// an error: the kernel builds its own environment lazily, exactly as if
/// seeding had never run.
pub async fn seed_from_template(data_root: &Path, slot_dir: &Path) -> Result<bool, TemplateError> {
    let template = data_root.join(TEMPLATE_SUBDIR);
    if !template.is_dir() {
        return Ok(false);
    }
    if std::fs::read_dir(slot_dir)?.next().is_some() {
        return Ok(false);
    }
    // Trailing `/.` copies the *contents* into the existing slot directory
    // rather than nesting a `template` directory inside it.
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
        // A partial copy is an arbitrary prefix of the template — which files
        // landed before `cp` failed is up to readdir order — and the
        // skip-if-present check above would keep it forever. Clearing it puts
        // the slot back to "no template", where kernels build their
        // environments from scratch: slow, but working.
        if let Err(err) = clear_dir_contents(slot_dir) {
            tracing::error!(
                %err,
                slot = %slot_dir.display(),
                "could not clear a partially copied node template"
            );
        }
        return Err(TemplateError::Copy(
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

    /// The template is a whole home skeleton, not a single well-known
    /// directory: it includes dotfiles (`.cache/rattler/...` is where pixi
    /// keeps its package cache) and arbitrarily nested directories, and
    /// `cp --archive` must carry all of it.
    #[tokio::test]
    async fn seeds_dotfiles_and_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join(TEMPLATE_SUBDIR);
        std::fs::create_dir_all(template.join(".cache/rattler/pkgs")).unwrap();
        std::fs::write(template.join(".cache/rattler/pkgs/numpy.conda"), b"x").unwrap();
        std::fs::create_dir_all(template.join("workspace")).unwrap();
        std::fs::write(template.join("workspace/readme.py"), b"import marimo").unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();

        assert!(seed_from_template(dir.path(), &slot).await.unwrap());
        assert_eq!(
            std::fs::read(slot.join(".cache/rattler/pkgs/numpy.conda")).unwrap(),
            b"x"
        );
        assert_eq!(
            std::fs::read(slot.join("workspace/readme.py")).unwrap(),
            b"import marimo"
        );
        // Contents, not a nested `template` directory.
        assert!(!slot.join(TEMPLATE_SUBDIR).exists());
    }

    /// A copy that dies partway must not leave a half-seeded slot behind: the
    /// skip-if-present check above would keep it forever, so a slot could get
    /// stuck with an arbitrary subset of the template and never be repaired.
    #[tokio::test]
    async fn a_failed_copy_clears_the_slot_and_the_next_call_re_seeds() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join(TEMPLATE_SUBDIR);
        std::fs::create_dir_all(template.join("pkgs")).unwrap();
        std::fs::write(template.join("pkgs/numpy.conda"), b"x").unwrap();
        // Unreadable to this (non-root) process, so `cp` copies part of the
        // tree and then fails.
        let locked = template.join("locked");
        std::fs::write(&locked, b"y").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();

        let err = seed_from_template(dir.path(), &slot).await.unwrap_err();
        assert!(matches!(err, TemplateError::Copy(_)), "{err:?}");
        let leftovers: Vec<_> = std::fs::read_dir(&slot)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a partial template would be published as if it were complete: {leftovers:?}"
        );
        assert!(slot.is_dir(), "the slot itself must survive");

        // With the obstacle gone the next publish re-seeds, rather than
        // skipping because the slot already has an entry.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(seed_from_template(dir.path(), &slot).await.unwrap());
        assert_eq!(std::fs::read(slot.join("pkgs/numpy.conda")).unwrap(), b"x");
    }

    /// A slot that already has content — an earlier seed, a recycled slot id,
    /// or tenant files — must never be overlaid; whichever of those it is, it
    /// wins over the template.
    #[tokio::test]
    async fn a_non_empty_slot_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join(TEMPLATE_SUBDIR);
        std::fs::create_dir_all(&template).unwrap();
        std::fs::write(template.join("from-template"), b"t").unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("tenant-file"), b"m").unwrap();

        assert!(!seed_from_template(dir.path(), &slot).await.unwrap());
        assert_eq!(std::fs::read(slot.join("tenant-file")).unwrap(), b"m");
        assert!(!slot.join("from-template").exists());
    }
}
