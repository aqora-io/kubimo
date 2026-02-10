use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec, Volume,
    VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Expr, FilterParams, Runner, RunnerCommand, RunnerField, Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::indexer;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    async fn apply_indexer_pod(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let service_account_name = indexer::service_account_name(workspace_name);
        let pod_name = indexer::pod_name(workspace_name);
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.to_string()),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                runtime_class_name: Some("gvisor".to_string()),
                service_account_name: Some(service_account_name.to_string()),
                enable_service_links: Some(false),
                security_context: Some(PodSecurityContext {
                    fs_group: Some(1000),
                    ..Default::default()
                }),
                containers: vec![Container {
                    name: "indexer".to_string(),
                    image: Some(ctx.config.marimo_image.clone()),
                    command: Some(cmd!["/app/indexer"]),
                    args: Some(indexer::args(workspace, true)?),
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
        ctx.api_namespaced::<Pod>(namespace).patch(&pod).await?;
        Ok(())
    }

    async fn delete_pod_if_exists(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let pod_name = indexer::pod_name(workspace_name);
        match ctx.api_namespaced::<Pod>(namespace).delete(&pod_name).await {
            Ok(_) => Ok(()),
            Err(err) if indexer::is_not_found_error(&err) => Ok(()),
            Err(err) => Err(err),
        }
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
    ) -> Result<(), kubimo::Error> {
        if workspace.metadata.deletion_timestamp.is_some() {
            return Ok(());
        }
        if workspace.spec.indexer.is_none() {
            self.delete_pod_if_exists(ctx, workspace).await?;
            return Ok(());
        };
        if !self.has_active_edit_runner(ctx, workspace).await? {
            self.delete_pod_if_exists(ctx, workspace).await?;
            return Ok(());
        }
        self.apply_indexer_pod(ctx, workspace).await?;
        Ok(())
    }
}
