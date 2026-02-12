use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{CacheJob, Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::indexer;
use crate::resources::Resources;

use super::CacheJobReconciler;

impl CacheJobReconciler {
    fn cache_container(&self, ctx: &Context, cache_job: &CacheJob) -> Container {
        let workspace_name = cache_job.spec.workspace.clone();
        Container {
            name: "cache".into(),
            image: Some(ctx.config.marimo_image.clone()),
            resources: Resources::default()
                .cpu(cache_job.spec.cpu.clone())
                .memory(cache_job.spec.memory.clone())
                .into(),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: indexer::MOUNT_DIR.into(),
                name: workspace_name,
                ..Default::default()
            }]),
            env: cache_job.spec.env.clone(),
            env_from: cache_job.spec.env_from.clone(),
            command: Some({
                let mut command = cmd!["bash", "/setup/start.sh"];
                if let Some(log_level) = cache_job.spec.log_level.as_ref() {
                    command.extend(cmd!["--log-level", log_level]);
                }
                command.push("cache".into());
                command
            }),
            ..Default::default()
        }
    }

    fn indexer_container(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<Container, kubimo::Error> {
        Ok(Container {
            name: "indexer".to_string(),
            image: Some(ctx.config.marimo_image.clone()),
            command: Some(cmd!["/app/indexer"]),
            args: Some(indexer::args(workspace, false)?),
            env: indexer::env(workspace),
            env_from: indexer::env_from(workspace),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: indexer::MOUNT_DIR.to_string(),
                name: workspace.name()?.to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        })
    }

    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        cache_job: &CacheJob,
    ) -> Result<Job, kubimo::Error> {
        let cache_job_name = cache_job.name()?;
        let namespace = cache_job.require_namespace()?;
        let workspace_name = &cache_job.spec.workspace;
        let workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get(workspace_name)
            .await?;
        let should_run_indexer =
            workspace.spec.indexer.is_some() && !indexer::is_pod_running(ctx, &workspace).await?;

        let mut pod_spec = PodSpec {
            containers: vec![],
            security_context: Some(PodSecurityContext {
                fs_group: Some(1000),
                ..Default::default()
            }),
            volumes: Some(vec![Volume {
                name: workspace_name.clone(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: workspace_name.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            restart_policy: Some("Never".into()),
            ..Default::default()
        };

        let cache_container = self.cache_container(ctx, cache_job);
        if should_run_indexer {
            pod_spec
                .containers
                .push(self.indexer_container(ctx, &workspace)?);
            pod_spec.init_containers = Some(vec![cache_container]);
            pod_spec.service_account_name = Some(indexer::service_account_name(workspace_name));
        } else {
            pod_spec.containers.push(cache_container);
        }

        let job = Job {
            metadata: ObjectMeta {
                name: Some(cache_job_name.to_string()),
                namespace: Some(namespace.to_string()),
                owner_references: Some(vec![cache_job.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                backoff_limit: cache_job.spec.backoff_limit,
                template: PodTemplateSpec {
                    spec: Some(pod_spec),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        ctx.api_namespaced::<Job>(namespace).patch(&job).await
    }
}
