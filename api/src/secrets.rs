use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The values object stored as [`crate::SECRETS_FILE_NAME`] under an archive's
/// key prefix — everything the names-only [`crate::ManifestSecrets`] section
/// describes, with its values. Fetched only by a `Values` restore.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum WorkspaceSecretsVersion {
    V1,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSecrets {
    pub version: WorkspaceSecretsVersion,
    /// Name of the workspace the secrets were exported from.
    pub workspace: String,
    /// KEY/VALUE pairs from the workspace root `.env`, in file order,
    /// duplicate keys deduped with the last value winning (dotenv semantics).
    pub env: Vec<SecretEnvEntry>,
    /// Whole files matched by the workspace's `.secrets` patterns.
    pub files: Vec<SecretFileEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretEnvEntry {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretFileEntry {
    /// Workspace-relative path.
    pub path: String,
    pub size: u64,
    /// Standard-base64 content; `None` when the file was over the inline size
    /// cap or unreadable at export time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_secrets_serde_round_trip() {
        let secrets = WorkspaceSecrets {
            version: WorkspaceSecretsVersion::V1,
            workspace: "ws".to_string(),
            env: vec![SecretEnvEntry {
                key: "API_KEY".to_string(),
                value: "hunter2".to_string(),
            }],
            files: vec![
                SecretFileEntry {
                    path: "creds/key.pem".to_string(),
                    size: 4,
                    content_base64: Some("AAAA".to_string()),
                },
                SecretFileEntry {
                    path: "big.bin".to_string(),
                    size: 1 << 30,
                    content_base64: None,
                },
            ],
        };
        let json = serde_json::to_value(&secrets).unwrap();
        assert_eq!(json["version"], "V1");
        assert_eq!(json["env"][0]["key"], "API_KEY");
        assert_eq!(json["files"][0]["contentBase64"], "AAAA");
        assert!(json["files"][1].get("contentBase64").is_none());
        let parsed: WorkspaceSecrets = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.env, secrets.env);
        assert_eq!(parsed.files, secrets.files);
    }
}
