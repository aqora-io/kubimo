use kubimo::k8s_openapi::api::core::v1::{EnvFromSource, EnvVar, Pod};
use kubimo::{Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;

pub(crate) const WORKSPACE_DIR: &str = "/home/me/workspace";
pub(crate) const MOUNT_DIR: &str = "/home/me";

#[inline]
pub(crate) fn pod_name(workspace_name: &str) -> String {
    format!("{workspace_name}-indexer")
}

#[inline]
pub(crate) fn service_account_name(workspace_name: &str) -> String {
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

pub(crate) fn upload_args(
    workspace: &Workspace,
    watch: bool,
) -> Result<Vec<String>, kubimo::Error> {
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

pub(crate) fn env(workspace: &Workspace) -> Option<Vec<EnvVar>> {
    let mut env = workspace
        .spec
        .indexer
        .as_ref()
        .and_then(|indexer| indexer.pod.as_ref())
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

pub(crate) fn env_from(workspace: &Workspace) -> Option<Vec<EnvFromSource>> {
    workspace
        .spec
        .indexer
        .as_ref()
        .and_then(|indexer| indexer.pod.as_ref())
        .and_then(|pod| pod.env_from.clone())
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
