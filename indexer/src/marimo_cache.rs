//! The marimo artifacts archived alongside a notebook.
//!
//! marimo writes them under a `__marimo__` directory next to the notebook, and
//! the workspace template lists that directory in `.ignore`, so they never
//! enter the walk: the indexer looks each one up by path instead, records it
//! in the notebook's `marimo.caches` block, and the restore puts it back at
//! the same path. This is the one place that mapping lives.
//!
//! The `format` string is what the `WorkspaceDirectory` CRs and the archive
//! manifest carry, so the values here are a wire format: never rename one.

use std::path::{Path, PathBuf};

const MARIMO_DIR: &str = "__marimo__";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarimoCacheKind {
    /// `marimo export md`, at `__marimo__/<stem>.md`.
    Md,
    /// `marimo export html`, at `__marimo__/<stem>.html`.
    Html,
    /// `marimo export ipynb`, at `__marimo__/<stem>.ipynb`.
    Ipynb,
    /// The session snapshot (cell outputs) marimo persists when a notebook
    /// runs, at `__marimo__/session/<file name>.json`. Together with
    /// [`Self::Notebook`] it is what marimo-ssr renders a published notebook
    /// from, and it costs a full notebook execution to rebuild.
    Session,
    /// The notebook snapshot (cell code) marimo-ssr renders from, at
    /// `__marimo__/notebook/<file name>.json`.
    Notebook,
}

impl MarimoCacheKind {
    pub const ALL: [Self; 5] = [
        Self::Md,
        Self::Html,
        Self::Ipynb,
        Self::Session,
        Self::Notebook,
    ];

    /// The value stored in `WorkspaceDirMarimoCache::format`.
    pub fn format(self) -> &'static str {
        match self {
            Self::Md => "md",
            Self::Html => "html",
            Self::Ipynb => "ipynb",
            Self::Session => "session",
            Self::Notebook => "notebook",
        }
    }

    /// `None` for a format this build does not know, so a newer writer's
    /// caches are skipped rather than mapped to a wrong path.
    pub fn from_format(format: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.format() == format)
    }

    /// Where the cache for `notebook` lives, relative to the same root as
    /// `notebook`; `None` when `notebook` has no parent or file name.
    pub fn path(self, notebook: &Path) -> Option<PathBuf> {
        let parent = notebook.parent()?;
        let file_name = notebook.file_name()?;
        let marimo_dir = parent.join(MARIMO_DIR);
        Some(match self {
            Self::Md | Self::Html | Self::Ipynb => {
                marimo_dir.join(file_name).with_extension(self.format())
            }
            // marimo keeps the notebook's full file name here (`nb.py.json`).
            Self::Session | Self::Notebook => {
                let mut file_name = file_name.to_os_string();
                file_name.push(".json");
                marimo_dir.join(self.format()).join(file_name)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip() {
        for kind in MarimoCacheKind::ALL {
            assert_eq!(MarimoCacheKind::from_format(kind.format()), Some(kind));
        }
        assert_eq!(
            MarimoCacheKind::ALL.map(MarimoCacheKind::format),
            ["md", "html", "ipynb", "session", "notebook"]
        );
    }

    #[test]
    fn unknown_formats_are_none() {
        // `json` is the file extension of two kinds, not a format.
        assert_eq!(MarimoCacheKind::from_format("json"), None);
        assert_eq!(MarimoCacheKind::from_format(""), None);
        assert_eq!(MarimoCacheKind::from_format("MD"), None);
    }

    #[test]
    fn export_caches_replace_the_extension() {
        let notebook = Path::new("readme.py");
        assert_eq!(
            MarimoCacheKind::Md.path(notebook),
            Some(PathBuf::from("__marimo__/readme.md"))
        );
        assert_eq!(
            MarimoCacheKind::Html.path(Path::new("sub/dir/nb.py")),
            Some(PathBuf::from("sub/dir/__marimo__/nb.html"))
        );
        assert_eq!(
            MarimoCacheKind::Ipynb.path(notebook),
            Some(PathBuf::from("__marimo__/readme.ipynb"))
        );
    }

    #[test]
    fn snapshots_keep_the_full_file_name_under_their_own_directory() {
        assert_eq!(
            MarimoCacheKind::Session.path(Path::new("readme.py")),
            Some(PathBuf::from("__marimo__/session/readme.py.json"))
        );
        assert_eq!(
            MarimoCacheKind::Notebook.path(Path::new("sub/nb.py")),
            Some(PathBuf::from("sub/__marimo__/notebook/nb.py.json"))
        );
    }

    #[test]
    fn a_path_without_a_file_name_has_no_cache() {
        assert_eq!(MarimoCacheKind::Session.path(Path::new("")), None);
        assert_eq!(MarimoCacheKind::Md.path(Path::new("/")), None);
    }
}
