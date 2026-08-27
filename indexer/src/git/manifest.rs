use std::{
    borrow::Cow,
    collections::{BTreeMap, LinkedList},
    path::Path,
};

use kubimo::{ManifestGitObjects, WorkspaceDir, WorkspaceDirEntry};
use thiserror::Error;

use crate::git;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Blob Not Found")]
    BlobNotFound,
}

pub fn build(
    dirs: &BTreeMap<String, WorkspaceDir>,
    uploaded: BTreeMap<String, git::Oid>,
) -> Result<ManifestGitObjects, Error> {
    let mut objects = uploaded;

    build_symlinks(&mut objects, dirs);

    // SAFETY: symlinks' oid have been computed
    build_trees(&mut objects, dirs)?;

    let sha1 = objects
        .into_iter()
        .map(|(path, oid)| (oid.into_hex_string(), path))
        .collect();

    Ok(ManifestGitObjects { sha1 })
}

fn entry_path(dir_name: &str, entry_name: &str) -> String {
    Path::new(dir_name)
        .join(entry_name)
        .into_os_string()
        .into_string()
        // SATEFY: both `dir_name` and `entry_name` are UTF-8
        .expect("pathbuf into_string")
}

fn build_symlinks(objects: &mut BTreeMap<String, git::Oid>, dirs: &BTreeMap<String, WorkspaceDir>) {
    for (dir_name, dir) in dirs {
        if let Some(entries) = &dir.spec.entries {
            for entry in entries {
                if let Some(symlink) = &entry.symlink {
                    let file_path = entry_path(dir_name, &entry.name);

                    let oid = git::OidKind::Sha1.hash(
                        git::ObjectKind::Blob,
                        symlink.path.as_ref().map_or(&b""[..], String::as_bytes),
                    );

                    objects.insert(file_path, oid);
                }
            }
        }
    }
}

/// Will sort order all directories based on folder hierarchy in top-down order.
fn order_dirs(dirs: &BTreeMap<String, WorkspaceDir>) -> Vec<String> {
    let mut ordered_dirs = Vec::new();

    let mut scanning = LinkedList::new();
    scanning.push_back(String::from(""));

    while let Some(dir_name) = scanning.pop_front() {
        let Some(dir) = dirs.get(&dir_name) else {
            continue;
        };
        let Some(entries) = dir.spec.entries.as_deref() else {
            continue;
        };
        for entry in entries {
            if entry.directory.is_some() {
                scanning.push_back(entry_path(&dir_name, &entry.name));
            }
        }
        ordered_dirs.push(dir_name);
    }

    ordered_dirs
}

/// Will panic if symlink oid cannot be found
fn build_trees(
    objects: &mut BTreeMap<String, git::Oid>,
    dirs: &BTreeMap<String, WorkspaceDir>,
) -> Result<(), Error> {
    let mut ordered_dirs = order_dirs(dirs);

    /*
     * NOTE: we iterate directories in bottom-up order
     * in order to guarantee that sub-tree oid will be found
     */

    while let Some(dir_name) = ordered_dirs.pop() {
        let dir = dirs
            .get(&dir_name)
            // SATEFY: `ordered_dirs` only ever contains paths that came from `dirs`
            .expect("dirs get dir_name");

        let dir_entries = dir
            .spec
            .entries
            .as_deref()
            // SATEFY: `ordered_dirs` only ever contains dirs that contain entries
            .expect("dir spec entries");

        let mut tree_entries = dir_entries
            .iter()
            .map(|entry| build_tree_entry(&objects, &dir_name, entry))
            .collect::<Result<Vec<_>, _>>()?;
        tree_entries.sort();
        let git_sha1 = git::OidKind::Sha1.tree(tree_entries);
        objects.insert(dir_name, git_sha1);
    }

    Ok(())
}

/// Will panic if sub-tree or symlink oid cannot be found
fn build_tree_entry<'a>(
    objects: &'a BTreeMap<String, git::Oid>,
    dir_name: &str,
    entry: &'a WorkspaceDirEntry,
) -> Result<git::TreeEntry<'a>, Error> {
    let path = entry_path(dir_name, &entry.name);

    if entry.directory.is_some() {
        let oid = objects
            .get(&path)
            // SAFETY: `ordered_dirs` is ordered bottom-up, which guarantees a sub-tree has its oid computed already
            .expect("objects get path");

        Ok(git::TreeEntry {
            kind: git::TreeEntryKind::Tree,
            filename: Cow::Borrowed(&entry.name),
            oid: oid.clone(),
        })
    } else if entry.symlink.is_some() {
        let oid = objects
            .get(&path)
            // SAFETY: all symlinks have their oid computed already
            .expect("objects get path");

        Ok(git::TreeEntry {
            kind: git::TreeEntryKind::Link,
            filename: Cow::Borrowed(&entry.name),
            oid: oid.clone(),
        })
    } else if entry.file.is_some() {
        let oid = objects.get(&path).ok_or(Error::BlobNotFound)?;

        Ok(git::TreeEntry {
            kind: git::TreeEntryKind::Blob,
            filename: Cow::Borrowed(&entry.name),
            oid: oid.clone(),
        })
    } else {
        panic!("unexpected dir entry type")
    }
}

#[cfg(test)]
mod tests {
    use kubimo::{
        WorkspaceDirContentUrl, WorkspaceDirDirectory, WorkspaceDirEntry, WorkspaceDirFile,
        WorkspaceDirSpec, url::Url,
    };

    use super::*;

    #[test]
    fn test_update_dirs_git_hashes() {
        let readme_oid = git::Oid::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let titanic_oid = git::Oid::from_hex("79f3c6cf7c35a70883db2da518935eeebd0cd091").unwrap();
        let utils_polars_oid =
            git::Oid::from_hex("94912be8b3fb47d4161ea50e5948c6296af6ca05").unwrap();
        let utils_secrets_oid =
            git::Oid::from_hex("8843d7f92416211de9ebb963ff4ce28125932878").unwrap();

        let dirs = BTreeMap::from([
            (
                "".to_string(),
                WorkspaceDir::new(
                    "",
                    WorkspaceDirSpec {
                        entries: Some(Vec::from([
                            WorkspaceDirEntry {
                                name: "readme.py".to_string(),
                                file: Some(WorkspaceDirFile {
                                    size: Some(5),
                                    content: Some(WorkspaceDirContentUrl {
                                        url: Url::parse("s3://bucket/file").unwrap(),
                                        crc32: None,
                                        e_tag: None,
                                    }),
                                    marimo: None,
                                }),
                                ..Default::default()
                            },
                            WorkspaceDirEntry {
                                name: "data".to_string(),
                                directory: Some(WorkspaceDirDirectory::default()),
                                ..Default::default()
                            },
                            WorkspaceDirEntry {
                                name: "utils".to_string(),
                                directory: Some(WorkspaceDirDirectory::default()),
                                ..Default::default()
                            },
                        ])),
                        ..Default::default()
                    },
                ),
            ),
            (
                "data".to_string(),
                WorkspaceDir::new(
                    "",
                    WorkspaceDirSpec {
                        entries: Some(Vec::from([WorkspaceDirEntry {
                            name: "titanic.parquet".to_string(),
                            file: Some(WorkspaceDirFile {
                                size: Some(27011),
                                content: Some(WorkspaceDirContentUrl {
                                    url: Url::parse("s3://bucket/file2").unwrap(),
                                    crc32: None,
                                    e_tag: None,
                                }),
                                marimo: None,
                            }),
                            ..Default::default()
                        }])),
                        ..Default::default()
                    },
                ),
            ),
            (
                "utils".to_string(),
                WorkspaceDir::new(
                    "",
                    WorkspaceDirSpec {
                        entries: Some(Vec::from([
                            WorkspaceDirEntry {
                                name: "polars.py".to_string(),
                                file: Some(WorkspaceDirFile {
                                    size: Some(11),
                                    content: Some(WorkspaceDirContentUrl {
                                        url: Url::parse("s3://bucket/file3").unwrap(),
                                        crc32: None,
                                        e_tag: None,
                                    }),
                                    marimo: None,
                                }),
                                ..Default::default()
                            },
                            WorkspaceDirEntry {
                                name: ".hidden".to_string(),
                                directory: Some(WorkspaceDirDirectory::default()),
                                ..Default::default()
                            },
                        ])),
                        ..Default::default()
                    },
                ),
            ),
            (
                "utils/.hidden".to_string(),
                WorkspaceDir::new(
                    "",
                    WorkspaceDirSpec {
                        entries: Some(Vec::from([WorkspaceDirEntry {
                            name: "secrets.txt".to_string(),
                            file: Some(WorkspaceDirFile {
                                size: Some(6),
                                content: Some(WorkspaceDirContentUrl {
                                    url: Url::parse("s3://bucket/file4").unwrap(),
                                    crc32: None,
                                    e_tag: None,
                                }),
                                marimo: None,
                            }),
                            ..Default::default()
                        }])),
                        ..Default::default()
                    },
                ),
            ),
        ]);

        let uploaded = BTreeMap::from([
            ("readme.py".to_string(), readme_oid.clone()),
            ("data/titanic.parquet".to_string(), titanic_oid.clone()),
            ("utils/polars.py".to_string(), utils_polars_oid.clone()),
            (
                "utils/.hidden/secrets.txt".to_string(),
                utils_secrets_oid.clone(),
            ),
        ]);

        let objects = build(&dirs, uploaded).unwrap();
        assert_eq!(
            objects.sha1,
            BTreeMap::from([
                (
                    "7c2fdd5226df715588f63b828e94401d40e4e1f3".to_string(),
                    "".to_string()
                ),
                (
                    "31c68d7f966e6e953b4f3e1c85486988f8c0f451".to_string(),
                    "data".to_string()
                ),
                (
                    titanic_oid.into_hex_string(),
                    "data/titanic.parquet".to_string()
                ),
                (
                    "3713198352c856851e27c6c8868df0a78a6cf28a".to_string(),
                    "utils".to_string()
                ),
                (
                    "12d6f35806a300953f1a33bf1ad5dd5c0d799a95".to_string(),
                    "utils/.hidden".to_string()
                ),
                (
                    utils_secrets_oid.into_hex_string(),
                    "utils/.hidden/secrets.txt".to_string()
                ),
                (
                    utils_polars_oid.into_hex_string(),
                    "utils/polars.py".to_string()
                ),
                (readme_oid.into_hex_string(), "readme.py".to_string()),
            ])
        );
    }
}
