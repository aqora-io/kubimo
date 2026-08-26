use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use url::Url;

use std::collections::BTreeMap;

use crate::crd::{WorkspaceDir, WorkspaceDirEntry};

/// Name of the manifest object under the indexer key prefix. Cannot collide
/// with content keys, which are always exactly 13 base32 characters.
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

/// Name of the secrets object under the indexer key prefix — the values the
/// names-only [`WorkspaceManifest::secrets`] section describes. Same collision
/// argument as [`MANIFEST_FILE_NAME`].
pub const SECRETS_FILE_NAME: &str = "secrets.json";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ManifestVersion {
    V1,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceManifest {
    pub version: ManifestVersion,
    /// Name of the workspace the manifest was generated from.
    pub workspace: String,
    /// Whether raw file contents were uploaded (`--upload-content`).
    pub upload_content: bool,
    /// Sum of `file.size` over entries that have a content url.
    pub total_content_bytes: u64,
    pub directories: Vec<ManifestDirectory>,
    /// Names-only view of the archive's secrets; the values live in a separate
    /// [`SECRETS_FILE_NAME`] object so a restore can withhold them.
    ///
    /// `None` marks a manifest written before secrets existed — a legacy
    /// archive whose `.env` may still sit in `directories` as a normal entry.
    /// Secrets-aware indexers always write `Some`, even when empty, so a
    /// restore can tell the two apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<ManifestSecrets>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_objects: Option<ManifestGitObjects>,
}

/// Names-only view of the archive's secrets.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestSecrets {
    /// Key names parsed from the workspace root `.env`, in file order.
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Workspace-relative paths of whole files marked secret (sorted).
    #[serde(default)]
    pub file_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDirectory {
    /// Path relative to the workspace root; "" is the root (same convention
    /// as `WorkspaceDirSpec.path`).
    pub path: String,
    pub entries: Vec<WorkspaceDirEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ManifestGitObjects {
    /// Associates git oids to path
    pub sha1: BTreeMap<String, String>,
}

/// The url of the manifest object for an archive, matching the indexer's raw
/// `{prefix}{name}` key concatenation.
pub fn manifest_url(bucket: &str, key_prefix: Option<&str>) -> Result<Url, url::ParseError> {
    Url::parse(&format!("s3://{bucket}/"))?.join(&format!(
        "{}{}",
        key_prefix.unwrap_or(""),
        MANIFEST_FILE_NAME
    ))
}

/// The url of the secrets object for an archive, same layout as
/// [`manifest_url`].
pub fn secrets_url(bucket: &str, key_prefix: Option<&str>) -> Result<Url, url::ParseError> {
    Url::parse(&format!("s3://{bucket}/"))?.join(&format!(
        "{}{}",
        key_prefix.unwrap_or(""),
        SECRETS_FILE_NAME
    ))
}

/// Project the freshly indexed batch of workspace dirs into a manifest that
/// fully describes the archive without the `WorkspaceDirectory` CRs.
pub fn build_manifest(
    workspace: &str,
    upload_content: bool,
    dirs: &BTreeMap<String, WorkspaceDir>,
    secrets: ManifestSecrets,
    git_objects: ManifestGitObjects,
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
        // Always `Some`: this is what distinguishes a filtered archive from a
        // legacy one on restore.
        secrets: Some(secrets),
        git_objects: Some(git_objects),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{WorkspaceDirContentUrl, WorkspaceDirEntry, WorkspaceDirFile};

    #[test]
    fn test_manifest_url_with_prefix() {
        let url = manifest_url("bucket", Some("workspace/")).unwrap();
        assert_eq!(url.as_str(), "s3://bucket/workspace/manifest.json");
    }

    #[test]
    fn test_manifest_url_without_prefix() {
        let url = manifest_url("bucket", None).unwrap();
        assert_eq!(url.as_str(), "s3://bucket/manifest.json");
    }

    #[test]
    fn test_manifest_url_prefix_without_trailing_slash() {
        // Matches the indexer's raw `{prefix}{name}` key concatenation.
        let url = manifest_url("bucket", Some("ws1-")).unwrap();
        assert_eq!(url.as_str(), "s3://bucket/ws1-manifest.json");
    }

    #[test]
    fn test_secrets_url_sits_next_to_the_manifest() {
        let url = secrets_url("bucket", Some("workspace/")).unwrap();
        assert_eq!(url.as_str(), "s3://bucket/workspace/secrets.json");
        let url = secrets_url("bucket", None).unwrap();
        assert_eq!(url.as_str(), "s3://bucket/secrets.json");
    }

    #[test]
    fn test_manifest_serde_round_trip() {
        let manifest = WorkspaceManifest {
            version: ManifestVersion::V1,
            workspace: "workspace".to_string(),
            upload_content: true,
            total_content_bytes: 42,
            directories: vec![ManifestDirectory {
                path: "".to_string(),
                entries: vec![WorkspaceDirEntry {
                    name: "notebook.py".to_string(),
                    file: Some(WorkspaceDirFile {
                        size: Some(42),
                        content: Some(WorkspaceDirContentUrl {
                            url: "s3://bucket/workspace/0123456789abc.py".parse().unwrap(),
                            crc32: Some(7),
                            e_tag: None,
                        }),
                        marimo: None,
                    }),
                    ..Default::default()
                }],
            }],
            secrets: Some(ManifestSecrets {
                env_keys: vec!["API_KEY".to_string()],
                file_paths: vec!["creds/key.pem".to_string()],
            }),
            git_objects: None,
        };

        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["version"], "V1");
        assert_eq!(json["uploadContent"], true);
        assert_eq!(json["totalContentBytes"], 42);
        assert_eq!(json["directories"][0]["path"], "");
        assert_eq!(json["directories"][0]["entries"][0]["name"], "notebook.py");
        assert_eq!(json["secrets"]["envKeys"][0], "API_KEY");
        assert_eq!(json["secrets"]["filePaths"][0], "creds/key.pem");

        let parsed: WorkspaceManifest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.workspace, manifest.workspace);
        assert_eq!(parsed.upload_content, manifest.upload_content);
        assert_eq!(parsed.total_content_bytes, manifest.total_content_bytes);
        assert_eq!(parsed.directories.len(), 1);
        assert_eq!(parsed.directories[0].entries[0].name, "notebook.py");
        assert_eq!(parsed.secrets, manifest.secrets);
    }

    /// A manifest written before secrets existed parses to `secrets: None` —
    /// the marker restore uses to treat the archive as unfiltered.
    #[test]
    fn test_pre_secrets_manifest_parses_to_none() {
        let parsed: WorkspaceManifest = serde_json::from_value(serde_json::json!({
            "version": "V1",
            "workspace": "ws",
            "uploadContent": true,
            "totalContentBytes": 0,
            "directories": [],
        }))
        .unwrap();
        assert!(parsed.secrets.is_none());
    }

    /// Older binaries parse newer manifests because unknown fields are
    /// ignored; this pins the property the compat story leans on.
    #[test]
    fn test_manifest_tolerates_unknown_fields() {
        let parsed: WorkspaceManifest = serde_json::from_value(serde_json::json!({
            "version": "V1",
            "workspace": "ws",
            "uploadContent": true,
            "totalContentBytes": 0,
            "directories": [],
            "someFutureField": {"nested": true},
        }))
        .unwrap();
        assert_eq!(parsed.workspace, "ws");
    }
}

#[cfg(test)]
mod build_manifest_tests {
    use super::*;
    use crate::crd::{
        WorkspaceDirContentUrl, WorkspaceDirDirectory, WorkspaceDirFile, WorkspaceDirSpec,
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
        let manifest = build_manifest(
            "ws",
            true,
            &dirs,
            ManifestSecrets::default(),
            ManifestGitObjects::default(),
        );
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
        let manifest = build_manifest(
            "ws",
            true,
            &dirs,
            ManifestSecrets::default(),
            ManifestGitObjects::default(),
        );
        assert_eq!(manifest.total_content_bytes, 10);
    }

    #[test]
    fn test_build_manifest_header() {
        let manifest = build_manifest(
            "ws",
            false,
            &BTreeMap::new(),
            ManifestSecrets::default(),
            ManifestGitObjects::default(),
        );
        assert!(matches!(manifest.version, ManifestVersion::V1));
        assert_eq!(manifest.workspace, "ws");
        assert!(!manifest.upload_content);
        assert_eq!(manifest.total_content_bytes, 0);
        assert!(manifest.directories.is_empty());
        // `Some` even when empty: the presence of the section is what marks
        // the archive as written by a secrets-aware indexer.
        assert_eq!(manifest.secrets, Some(ManifestSecrets::default()));
    }
}
