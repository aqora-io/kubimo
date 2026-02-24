use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, Pod, PodSpec, PodTemplateSpec, Volume,
    VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::indexer;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    async fn delete_indexer_pod_if_exists(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let indexer_pod = indexer::pod_name(workspace.name()?);
        let namespace = workspace.require_namespace()?;
        ctx.api_namespaced::<Pod>(namespace)
            .delete_opt(&indexer_pod)
            .await?;
        Ok(())
    }

    async fn workspace_indexer_cleanup_job_name(
        &self,
        workspace: &Workspace,
    ) -> Result<String, kubimo::Error> {
        Ok(format!("{}-indexer-cleanup", workspace.name()?))
    }

    async fn apply_indexer_cleanup_job(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<Job, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let service_account_name = indexer::service_account_name(workspace_name);
        let job = Job {
            metadata: ObjectMeta {
                name: Some(self.workspace_indexer_cleanup_job_name(workspace).await?),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        restart_policy: Some("Never".to_string()),
                        service_account_name: Some(service_account_name.to_string()),
                        containers: vec![Container {
                            name: "indexer".to_string(),
                            image: Some(ctx.config.marimo_image.clone()),
                            command: Some(cmd!["/app/indexer"]),
                            args: Some(cmd!["clean", workspace_name]),
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
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<Job>(workspace.require_namespace()?)
            .patch(&job)
            .await?;
        Ok(job)
    }

    pub async fn get_or_apply_indexer_cleanup_job(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<Job, kubimo::Error> {
        let job_name = self.workspace_indexer_cleanup_job_name(workspace).await?;
        if let Some(job) = ctx
            .api_namespaced::<Job>(workspace.require_namespace()?)
            .get_opt(&job_name)
            .await?
        {
            Ok(job)
        } else {
            self.apply_indexer_cleanup_job(ctx, workspace).await
        }
    }

    pub async fn cleanup_indexer(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        if workspace.spec.indexer.is_none() {
            return Ok(());
        }
        self.delete_indexer_pod_if_exists(ctx, workspace).await?;
        let job = self
            .get_or_apply_indexer_cleanup_job(ctx, workspace)
            .await?;
        let job_is_complete = job
            .status
            .and_then(|status| status.conditions)
            .unwrap_or_default()
            .iter()
            .any(|cond| cond.type_ == "Complete");
        if job_is_complete {
            Ok(())
        } else {
            Err(kubimo::Error::Custom(
                "Waiting for indexer cleanup job to complete".into(),
            ))
        }
    }
}
