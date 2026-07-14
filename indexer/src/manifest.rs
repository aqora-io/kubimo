use std::collections::BTreeMap;

use kubimo::{ManifestDirectory, ManifestVersion, WorkspaceDir, WorkspaceManifest};

/// Project the freshly indexed batch of workspace dirs into a manifest that
/// fully describes the archive without the `WorkspaceDirectory` CRs.
pub fn build_manifest(
    workspace: &str,
    upload_content: bool,
    dirs: &BTreeMap<String, WorkspaceDir>,
) -> WorkspaceManifest {
    let mut directories = dirs
        .values()
        .map(|dir| {
            let mut entries = dir.spec.entries.clone().unwrap_or_default();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            ManifestDirectory {
                path: dir.spec.path.clone(),
                entries,
            }
        })
        .collect::<Vec<_>>();
    directories.sort_by(|a, b| a.path.cmp(&b.path));
    let total_content_bytes = directories
        .iter()
        .flat_map(|dir| dir.entries.iter())
        .filter_map(|entry| entry.file.as_ref())
        .filter(|file| file.content.is_some())
        .filter_map(|file| file.size)
        .sum();
    WorkspaceManifest {
        version: ManifestVersion::V1,
        workspace: workspace.to_string(),
        upload_content,
        total_content_bytes,
        directories,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::{
        WorkspaceDirContentUrl, WorkspaceDirDirectory, WorkspaceDirEntry, WorkspaceDirFile,
        WorkspaceDirSpec,
    };

    fn file_entry(name: &str, size: u64, with_content: bool) -> WorkspaceDirEntry {
        WorkspaceDirEntry {
            name: name.to_string(),
            file: Some(WorkspaceDirFile {
                size: Some(size),
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

    fn dirs(items: Vec<(&str, &str, Vec<WorkspaceDirEntry>)>) -> BTreeMap<String, WorkspaceDir> {
        items
            .into_iter()
            .map(|(name, path, entries)| {
                (
                    name.to_string(),
                    WorkspaceDir::new(
                        name,
                        WorkspaceDirSpec {
                            workspace: "ws".to_string(),
                            path: path.to_string(),
                            entries: Some(entries),
                        },
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn test_build_manifest_sorts_dirs_by_path_and_entries_by_name() {
        // BTreeMap orders by (random) dir name; the manifest must order by path.
        let dirs = dirs(vec![
            ("aaa", "sub", vec![file_entry("b.txt", 1, true)]),
            (
                "zzz",
                "",
                vec![
                    file_entry("z.txt", 1, true),
                    WorkspaceDirEntry {
                        name: "sub".to_string(),
                        directory: Some(WorkspaceDirDirectory {
                            name: Some("aaa".to_string()),
                        }),
                        ..Default::default()
                    },
                ],
            ),
        ]);
        let manifest = build_manifest("ws", true, &dirs);
        assert_eq!(manifest.directories[0].path, "");
        assert_eq!(manifest.directories[1].path, "sub");
        assert_eq!(manifest.directories[0].entries[0].name, "sub");
        assert_eq!(manifest.directories[0].entries[1].name, "z.txt");
    }

    #[test]
    fn test_build_manifest_totals_only_entries_with_content() {
        let dirs = dirs(vec![(
            "root",
            "",
            vec![
                file_entry("a.txt", 10, true),
                file_entry("too-big.bin", 5, false),
            ],
        )]);
        let manifest = build_manifest("ws", true, &dirs);
        assert_eq!(manifest.total_content_bytes, 10);
    }

    #[test]
    fn test_build_manifest_header() {
        let manifest = build_manifest("ws", false, &BTreeMap::new());
        assert!(matches!(manifest.version, ManifestVersion::V1));
        assert_eq!(manifest.workspace, "ws");
        assert!(!manifest.upload_content);
        assert_eq!(manifest.total_content_bytes, 0);
        assert!(manifest.directories.is_empty());
    }
}
