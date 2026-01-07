use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{CacheJob, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::resources::Resources;

use super::CacheJobReconciler;

impl CacheJobReconciler {
    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        cache_job: &CacheJob,
    ) -> Result<Job, kubimo::Error> {
        let cache_job_name = cache_job.name()?;
        let namespace = cache_job.require_namespace()?;
        let workspace_name = &cache_job.spec.workspace;

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
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            name: "cache".into(),
                            image: Some(ctx.config.marimo_image.clone()),
                            resources: Resources::default()
                                .cpu(cache_job.spec.cpu.clone())
                                .memory(cache_job.spec.memory.clone())
                                .into(),
                            volume_mounts: Some(vec![VolumeMount {
                                mount_path: "/home/me".into(),
                                name: workspace_name.clone(),
                                ..Default::default()
                            }]),
                            env: cache_job.spec.env.clone(),
                            env_from: cache_job.spec.env_from.clone(),
                            command: Some(cmd!["bash", "/setup/start.sh", "cache"]),
                            ..Default::default()
                        }],
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
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<Job>(namespace).patch(&job).await
    }
}
