use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    SecurityContext, Volume, VolumeMount,
};

use kubimo::kube::api::ObjectMeta;
use kubimo::{Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::hardened_security_context;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<Option<Job>, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;

        if let Some(job) = ctx
            .api_namespaced::<Job>(namespace)
            .get_opt(workspace_name)
            .await?
        {
            return Ok(Some(job));
        }

        if workspace.spec.clone_workspace_name.is_some() {
            return Ok(None);
        }

        let mut volumes = workspace.spec.volumes.clone().unwrap_or_default();
        volumes.push(Volume {
            name: workspace_name.into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_name.into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut init_containers = vec![Container {
            name: "init-dirs".into(),
            image: Some(ctx.config.marimo_image.clone()),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: "/mnt".into(),
                name: workspace_name.into(),
                ..Default::default()
            }]),
            command: Some(cmd![
                "sh",
                "-c",
                r#"
set -ex
chown me:me /mnt
cp -a /home/me/. /mnt
"#,
            ]),
            security_context: Some(SecurityContext {
                run_as_user: Some(0),
                run_as_group: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        }];
        if let Some(spec_init_containers) = workspace.spec.init_containers.clone() {
            init_containers.extend(spec_init_containers)
        }
        let job = Job {
            metadata: ObjectMeta {
                name: workspace.metadata.name.clone(),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        init_containers: Some(init_containers),
                        containers: vec![Container {
                            name: "init".to_string(),
                            image: Some(ctx.config.busybox_image.clone()),
                            command: Some(cmd!["/bin/true"]),
                            ..Default::default()
                        }],
                        security_context: Some(PodSecurityContext {
                            fs_group: Some(1000),
                            ..Default::default()
                        }),
                        volumes: Some(volumes),
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
