use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, Pod, PodSpec, Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{
    Expr, FilterParams, KubimoLabel, Runner, RunnerCommand, RunnerField, Workspace, prelude::*,
};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::runner::is_invalid_request;
use crate::controllers::{indexer, workspace_affinity};

use super::WorkspaceReconciler;

const CONTAINER_NAME: &str = "indexer";

/// What an apply did to the live indexer pod.
///
/// `Replaced` is the one outcome the caller has to act on: the drifted pod has
/// been deleted and nothing has recreated it yet, so the reconcile has to come
/// back rather than wait for a change.
pub(crate) enum IndexerApply {
    Applied,
    Replaced,
}

impl WorkspaceReconciler {
    async fn apply_indexer_pod(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<IndexerApply, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let service_account_name = indexer::service_account_name(workspace_name);
        let pod_name = indexer::pod_name(workspace_name);
        let mut labels: BTreeMap<String, String> = [(
            KubimoLabel::borrow("name").to_string(),
            pod_name.to_string(),
        )]
        .into_iter()
        .collect();
        labels.extend(workspace_affinity::workspace_label_map(workspace_name));
        let affinity = Some(workspace_affinity::workspace_affinity(workspace_name));
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.to_string()),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                service_account_name: Some(service_account_name.to_string()),
                affinity,
                containers: vec![Container {
                    name: CONTAINER_NAME.to_string(),
                    image: Some(ctx.config.marimo_image.clone()),
                    command: Some(cmd!["/app/indexer"]),
                    args: Some(indexer::upload_args(workspace, true)?),
                    env: indexer::env(workspace),
                    env_from: indexer::env_from(workspace),
                    volume_mounts: Some(vec![VolumeMount {
                        mount_path: indexer::MOUNT_DIR.to_string(),
                        name: workspace_name.to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }],
                volumes: Some(vec![Volume {
                    name: workspace_name.to_string(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: workspace_name.to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        match ctx.api_namespaced::<Pod>(namespace).patch(&pod).await {
            Err(err) if is_invalid_request(&err) => {
                // A live pod's spec is almost entirely immutable, so an apply that has to
                // change one of those fields — the archive location, which reaches the
                // indexer as command line args and environment — can only be honoured by
                // replacement. A 422 is also what any other pod validation failure looks
                // like, so fetch the live pod and delete only when the fields this
                // reconciler derives from `spec.indexer` are what actually differ; any
                // other 422, and any failure to fetch, propagates untouched and is spaced
                // out by the caller's backoff. The comparison covers more than the runner's
                // does because an indexer is a watcher holding no user state — the
                // workspace's files live on the volume — so a needless recreation costs a
                // moment of unindexed edits rather than someone's session. Pods carry a
                // termination grace period, so recreation waits for the next reconcile
                // rather than racing the delete.
                let live = ctx
                    .api_namespaced::<Pod>(namespace)
                    .get_opt(&pod_name)
                    .await;
                if matches!(&live, Ok(Some(live)) if indexer_container_drifted(live, &pod)) {
                    ctx.api_namespaced::<Pod>(namespace)
                        .delete_opt(&pod_name)
                        .await?;
                    return Ok(IndexerApply::Replaced);
                }
                Err(err)
            }
            result => result.map(|_| IndexerApply::Applied),
        }
    }

    async fn delete_pod_if_exists(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let pod_name = indexer::pod_name(workspace_name);
        ctx.api_namespaced::<Pod>(namespace)
            .delete_opt(&pod_name)
            .await?;
        Ok(())
    }

    async fn has_active_edit_runner(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<bool, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let runner = ctx
            .api_namespaced::<Runner>(namespace)
            .find(&FilterParams::new().with_fields(vec![
                Expr::new(RunnerField::Workspace).eq(workspace_name),
                Expr::new(RunnerField::Command).eq(RunnerCommand::Edit),
            ]))
            .await?;
        Ok(runner.is_some())
    }

    pub(crate) async fn apply_indexer(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<IndexerApply, kubimo::Error> {
        if workspace.spec.indexer.is_none() {
            self.delete_pod_if_exists(ctx, workspace).await?;
            return Ok(IndexerApply::Applied);
        };
        if !self.has_active_edit_runner(ctx, workspace).await? {
            self.delete_pod_if_exists(ctx, workspace).await?;
            return Ok(IndexerApply::Applied);
        }
        self.apply_indexer_pod(ctx, workspace).await
    }
}

/// Whether the live indexer container differs from the desired one in a field
/// that cannot be updated in place. The image is deliberately left out: kubelet
/// accepts a new one on a running pod, so a rolled marimo image is no reason to
/// throw the pod away.
fn indexer_container_drifted(live: &Pod, desired: &Pod) -> bool {
    fn container(pod: &Pod) -> Option<&Container> {
        pod.spec
            .as_ref()?
            .containers
            .iter()
            .find(|container| container.name == CONTAINER_NAME)
    }
    let (Some(live), Some(desired)) = (container(live), container(desired)) else {
        return false;
    };
    live.args != desired.args || live.env != desired.env || live.env_from != desired.env_from
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::core::v1::{EnvFromSource, EnvVar, SecretEnvSource};

    fn indexer_pod(
        image: &str,
        args: &[&str],
        env: &[(&str, &str)],
        env_from: Option<&str>,
    ) -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: CONTAINER_NAME.to_string(),
                    image: Some(image.to_string()),
                    args: Some(args.iter().map(|arg| arg.to_string()).collect()),
                    env: Some(
                        env.iter()
                            .map(|(name, value)| EnvVar {
                                name: name.to_string(),
                                value: Some(value.to_string()),
                                ..Default::default()
                            })
                            .collect(),
                    ),
                    env_from: env_from.map(|secret| {
                        vec![EnvFromSource {
                            secret_ref: Some(SecretEnvSource {
                                name: secret.to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }]
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn desired() -> Pod {
        indexer_pod(
            "marimo:v1",
            &["upload", "--watch", "--bucket", "bucket", "ws", "/dir"],
            &[("RUST_LOG", "info")],
            Some("archive"),
        )
    }

    /// The archive location arrives on the command line, so a workspace that
    /// gains a bucket can only be honoured by replacement.
    #[test]
    fn changed_args_mark_an_indexer_pod_for_replacement() {
        let live = indexer_pod(
            "marimo:v1",
            &["upload", "--watch", "ws", "/dir"],
            &[("RUST_LOG", "info")],
            Some("archive"),
        );
        assert!(indexer_container_drifted(&live, &desired()));
    }

    /// The credentials the indexer uploads with come from the pod's environment,
    /// which is just as immutable as its args.
    #[test]
    fn changed_env_marks_an_indexer_pod_for_replacement() {
        let live = indexer_pod(
            "marimo:v1",
            &["upload", "--watch", "--bucket", "bucket", "ws", "/dir"],
            &[("RUST_LOG", "debug")],
            Some("archive"),
        );
        assert!(indexer_container_drifted(&live, &desired()));

        let live = indexer_pod(
            "marimo:v1",
            &["upload", "--watch", "--bucket", "bucket", "ws", "/dir"],
            &[("RUST_LOG", "info")],
            Some("other-archive"),
        );
        assert!(indexer_container_drifted(&live, &desired()));
    }

    /// A pod already carrying the desired spec must never be a replacement
    /// candidate, or an unrelated 422 would delete a healthy indexer. The image
    /// is mutable in place, so a bumped image is not drift either.
    #[test]
    fn a_matching_indexer_pod_is_never_replaced() {
        assert!(!indexer_container_drifted(&desired(), &desired()));
        assert!(!indexer_container_drifted(
            &indexer_pod(
                "marimo:v0",
                &["upload", "--watch", "--bucket", "bucket", "ws", "/dir"],
                &[("RUST_LOG", "info")],
                Some("archive"),
            ),
            &desired()
        ));
    }

    /// Without a container to compare against there is nothing to attribute the
    /// rejection to, so the apply error stands rather than a pod being deleted
    /// on a guess.
    #[test]
    fn a_pod_without_the_indexer_container_is_never_replaced() {
        assert!(!indexer_container_drifted(&Pod::default(), &desired()));
    }
}
