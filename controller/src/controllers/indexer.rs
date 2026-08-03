use kubimo::k8s_openapi::api::core::v1::{EnvFromSource, EnvVar, Pod};
use kubimo::{Workspace, WorkspaceIndexerPod, WorkspaceRestoreFrom, prelude::*};

use crate::command::cmd;
use crate::context::Context;

pub(crate) const WORKSPACE_DIR: &str = "/home/me/workspace";
pub const MOUNT_DIR: &str = "/home/me";
/// Where the init job mounts the workspace volume.
pub(crate) const INIT_MOUNT_DIR: &str = "/mnt";
pub(crate) const INIT_WORKSPACE_DIR: &str = "/mnt/workspace";

#[inline]
pub fn pod_name(workspace_name: &str) -> String {
    format!("{workspace_name}-indexer")
}

#[inline]
pub fn service_account_name(workspace_name: &str) -> String {
    format!("{workspace_name}-indexer")
}

#[inline]
pub(crate) fn role_name(workspace_name: &str) -> String {
    format!("{workspace_name}-indexer")
}

#[inline]
pub(crate) fn role_binding_name(workspace_name: &str) -> String {
    format!("{workspace_name}-indexer")
}

pub fn upload_args(workspace: &Workspace, watch: bool) -> Result<Vec<String>, kubimo::Error> {
    let workspace_name = workspace.name()?;
    let mut args = vec!["upload".to_string()];
    if watch {
        args.extend(cmd!["--watch"]);
    }
    if let Some(indexer) = workspace.spec.indexer.as_ref() {
        if let Some(bucket) = indexer.bucket.as_ref() {
            args.extend(cmd!["--bucket", bucket]);
        }
        if let Some(key_prefix) = indexer.key_prefix.as_ref() {
            args.extend(cmd!["--key-prefix", key_prefix]);
        }
        if let Some(upload_content) = indexer.upload_content
            && upload_content
        {
            args.extend(cmd!["--upload-content"]);
        }
    }
    args.push(workspace_name.to_string());
    args.push(WORKSPACE_DIR.to_string());
    Ok(args)
}

pub(crate) fn clean_args(workspace: &Workspace) -> Result<Vec<String>, kubimo::Error> {
    let workspace_name = workspace.name()?;
    Ok(cmd!["clean", workspace_name])
}

/// Pod env for the purge, carrying the archive's location.
///
/// The archive's location is not recorded anywhere the indexer can look it up
/// on its own, so the purge only reaches the manifest if we hand it over. It
/// travels as environment rather than as `--bucket`/`--key-prefix` because the
/// image running it is pinned separately from this controller
/// (`controller.marimoImage`): an indexer older than those flags rejects the
/// whole invocation, and since the deletion finalizer blocks until the Job
/// succeeds, that leaves every workspace wedged in `Terminating`. An older
/// indexer ignores an environment variable it does not know instead.
///
/// Set explicitly rather than left to the `envFrom` secret, which carries an
/// `AWS_BUCKET` of its own: the spec is what the rest of the archive layout is
/// derived from, so it has to win where the two disagree.
pub(crate) fn clean_env(workspace: &Workspace) -> Option<Vec<EnvVar>> {
    let mut env = env(workspace).unwrap_or_default();
    let Some(indexer) = workspace.spec.indexer.as_ref() else {
        return Some(env);
    };
    let mut set = |name: &str, value: &str| {
        env.retain(|existing| existing.name != name);
        env.push(EnvVar {
            name: name.to_string(),
            value: Some(value.to_string()),
            ..Default::default()
        });
    };
    if let Some(bucket) = indexer.bucket.as_ref() {
        set("AWS_BUCKET", bucket);
    }
    if let Some(key_prefix) = indexer.key_prefix.as_ref() {
        set("AWS_KEY_PREFIX", key_prefix);
    }
    Some(env)
}

pub(crate) fn download_args(restore: &WorkspaceRestoreFrom) -> Vec<String> {
    let mut args = cmd!["download", "--bucket", restore.bucket];
    if let Some(key_prefix) = restore.key_prefix.as_ref() {
        args.extend(cmd!["--key-prefix", key_prefix]);
    }
    args.push(INIT_WORKSPACE_DIR.to_string());
    args
}

pub(crate) fn pod_env(pod: Option<&WorkspaceIndexerPod>) -> Option<Vec<EnvVar>> {
    let mut env = pod
        .and_then(|pod| pod.env.as_ref())
        .cloned()
        .unwrap_or_default();
    if !env.iter().any(|env_var| env_var.name == "RUST_LOG") {
        env.push(EnvVar {
            name: "RUST_LOG".to_string(),
            value: Some("info".to_string()),
            ..Default::default()
        })
    }
    Some(env)
}

pub(crate) fn pod_env_from(pod: Option<&WorkspaceIndexerPod>) -> Option<Vec<EnvFromSource>> {
    pod.and_then(|pod| pod.env_from.clone())
}

pub fn env(workspace: &Workspace) -> Option<Vec<EnvVar>> {
    pod_env(
        workspace
            .spec
            .indexer
            .as_ref()
            .and_then(|indexer| indexer.pod.as_ref()),
    )
}

pub fn env_from(workspace: &Workspace) -> Option<Vec<EnvFromSource>> {
    pod_env_from(
        workspace
            .spec
            .indexer
            .as_ref()
            .and_then(|indexer| indexer.pod.as_ref()),
    )
}

pub(crate) async fn is_pod_running(
    ctx: &Context,
    workspace: &Workspace,
) -> Result<bool, kubimo::Error> {
    let workspace_name = workspace.name()?;
    let namespace = workspace.require_namespace()?;
    let Some(pod) = ctx
        .api_namespaced::<Pod>(namespace)
        .get_opt(pod_name(workspace_name).as_ref())
        .await?
    else {
        return Ok(false);
    };
    Ok(matches!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Running")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::WorkspaceRestoreFrom;

    #[test]
    fn test_download_args_with_key_prefix() {
        let args = download_args(&WorkspaceRestoreFrom {
            bucket: "bucket".to_string(),
            key_prefix: Some("workspace/".to_string()),
            pod: None,
        });
        assert_eq!(
            args,
            vec![
                "download",
                "--bucket",
                "bucket",
                "--key-prefix",
                "workspace/",
                INIT_WORKSPACE_DIR,
            ]
        );
    }

    #[test]
    fn test_download_args_without_key_prefix() {
        let args = download_args(&WorkspaceRestoreFrom {
            bucket: "bucket".to_string(),
            key_prefix: None,
            pod: None,
        });
        assert_eq!(
            args,
            vec!["download", "--bucket", "bucket", INIT_WORKSPACE_DIR]
        );
    }

    /// The purge job is the only thing that ever deletes the manifest, and it
    /// can only do so with the archive's location on its command line.
    #[test]
    fn test_clean_env_carries_the_archive_location() {
        let workspace = kubimo::Workspace::new(
            "ws",
            kubimo::WorkspaceSpec {
                indexer: Some(kubimo::WorkspaceIndexer {
                    bucket: Some("bucket".to_string()),
                    key_prefix: Some("workspace/".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let env = clean_env(&workspace).unwrap();
        let value = |name: &str| {
            env.iter()
                .find(|var| var.name == name)
                .and_then(|var| var.value.clone())
        };
        assert_eq!(value("AWS_BUCKET").as_deref(), Some("bucket"));
        assert_eq!(value("AWS_KEY_PREFIX").as_deref(), Some("workspace/"));
    }

    /// The purge runs in an image pinned separately from this controller, so an
    /// indexer that predates the archive-location flags must still be able to
    /// parse the invocation.
    #[test]
    fn test_clean_args_stay_parseable_by_an_older_indexer() {
        let workspace = kubimo::Workspace::new(
            "ws",
            kubimo::WorkspaceSpec {
                indexer: Some(kubimo::WorkspaceIndexer {
                    bucket: Some("bucket".to_string()),
                    key_prefix: Some("workspace/".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(clean_args(&workspace).unwrap(), vec!["clean", "ws"]);
    }

    /// A workspace with no bucket has no archive; the CR-driven purge is the
    /// whole job and must still run.
    #[test]
    fn test_clean_args_without_an_archive() {
        let workspace = kubimo::Workspace::new("ws", kubimo::WorkspaceSpec::default());
        assert_eq!(clean_args(&workspace).unwrap(), vec!["clean", "ws"]);
    }

    #[test]
    fn test_pod_env_injects_rust_log() {
        let env = pod_env(None).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "RUST_LOG");
        assert_eq!(env[0].value.as_deref(), Some("info"));
    }

    #[test]
    fn test_pod_env_keeps_user_rust_log() {
        let pod = kubimo::WorkspaceIndexerPod {
            env: Some(vec![EnvVar {
                name: "RUST_LOG".to_string(),
                value: Some("debug".to_string()),
                ..Default::default()
            }]),
            env_from: None,
        };
        let env = pod_env(Some(&pod)).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].value.as_deref(), Some("debug"));
    }
}
