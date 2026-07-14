use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::crd::WorkspaceDirEntry;

/// Name of the manifest object under the indexer key prefix. Cannot collide
/// with content keys, which are always exactly 13 base32 characters.
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

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
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDirectory {
    /// Path relative to the workspace root; "" is the root (same convention
    /// as `WorkspaceDirSpec.path`).
    pub path: String,
    pub entries: Vec<WorkspaceDirEntry>,
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
        };

        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["version"], "V1");
        assert_eq!(json["uploadContent"], true);
        assert_eq!(json["totalContentBytes"], 42);
        assert_eq!(json["directories"][0]["path"], "");
        assert_eq!(json["directories"][0]["entries"][0]["name"], "notebook.py");

        let parsed: WorkspaceManifest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.workspace, manifest.workspace);
        assert_eq!(parsed.upload_content, manifest.upload_content);
        assert_eq!(parsed.total_content_bytes, manifest.total_content_bytes);
        assert_eq!(parsed.directories.len(), 1);
        assert_eq!(parsed.directories[0].entries[0].name, "notebook.py");
    }
}
