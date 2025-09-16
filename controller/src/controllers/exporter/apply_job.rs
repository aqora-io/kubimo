use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, EnvFromSource, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec,
    PodTemplateSpec, SecretEnvSource, Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Exporter, prelude::*};

use crate::command::cmd;
use crate::context::Context;

use super::ExporterReconciler;

impl ExporterReconciler {
    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        exporter: &Exporter,
    ) -> Result<Option<Job>, kubimo::Error> {
        let Some(s3_url) = exporter
            .spec
            .s3_request
            .as_ref()
            .and_then(|s3_req| s3_req.url.as_ref())
        else {
            return Ok(None);
        };
        let namespace = exporter.require_namespace()?;
        let job = Job {
            metadata: ObjectMeta {
                name: exporter.metadata.name.clone(),
                namespace: exporter.metadata.namespace.clone(),
                owner_references: Some(vec![exporter.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            name: format!("{}-export", exporter.name()?),
                            image: Some(ctx.config.marimo_image_name.clone()),
                            volume_mounts: Some(vec![VolumeMount {
                                mount_path: "/home/me".to_string(),
                                name: exporter.spec.workspace.clone(),
                                ..Default::default()
                            }]),
                            env_from: Some(vec![EnvFromSource {
                                secret_ref: Some(
                                    exporter
                                        .spec
                                        .s3_request
                                        .as_ref()
                                        .and_then(|s3_req| s3_req.secret.as_ref())
                                        .map(|secret| SecretEnvSource {
                                            name: secret.clone(),
                                            optional: Some(false),
                                        })
                                        .unwrap_or_else(|| SecretEnvSource {
                                            name: ctx.config.s3_creds_secret.clone(),
                                            optional: Some(true),
                                        }),
                                ),
                                ..Default::default()
                            }]),
                            command: Some(cmd!["s3-tar", "upload", ".", s3_url]),
                            ..Default::default()
                        }],
                        security_context: Some(PodSecurityContext {
                            fs_group: Some(1000),
                            ..Default::default()
                        }),
                        volumes: Some(vec![Volume {
                            name: exporter.spec.workspace.clone(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: exporter.spec.workspace.clone(),
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
        Ok(Some(
            ctx.api_namespaced::<Job>(namespace).patch(&job).await?,
        ))
    }
}
