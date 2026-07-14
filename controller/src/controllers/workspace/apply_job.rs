use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    SecurityContext, Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::indexer;
use crate::controllers::workspace_affinity;

use super::WorkspaceReconciler;

fn build_init_containers(
    marimo_image: &str,
    workspace_name: &str,
    workspace: &Workspace,
) -> Vec<Container> {
    let mut init_containers = vec![Container {
        name: "init-dirs".into(),
        image: Some(marimo_image.to_string()),
        volume_mounts: Some(vec![VolumeMount {
            mount_path: indexer::INIT_MOUNT_DIR.into(),
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
    if let Some(restore) = workspace.spec.restore_from.as_ref() {
        init_containers.push(Container {
            name: "restore".into(),
            image: Some(marimo_image.to_string()),
            command: Some(cmd!["/app/indexer"]),
            args: Some(indexer::download_args(restore)),
            env: indexer::pod_env(restore.pod.as_ref()),
            env_from: indexer::pod_env_from(restore.pod.as_ref()),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: indexer::INIT_MOUNT_DIR.into(),
                name: workspace_name.into(),
                ..Default::default()
            }]),
            // `init-dirs` has already chowned the volume to me:me (1000).
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                run_as_group: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(spec_init_containers) = workspace.spec.init_containers.clone() {
        init_containers.extend(spec_init_containers)
    }
    init_containers
}

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
        let init_containers =
            build_init_containers(&ctx.config.marimo_image, workspace_name, workspace);
        let pod_labels = workspace_affinity::workspace_label_map(workspace_name);
        let affinity = Some(workspace_affinity::workspace_affinity(workspace_name));
        let job = Job {
            metadata: ObjectMeta {
                name: workspace.metadata.name.clone(),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(pod_labels),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            name: "init".to_string(),
                            image: Some(ctx.config.busybox_image.clone()),
                            command: Some(cmd!["/bin/true"]),
                            ..Default::default()
                        }],
                        init_containers: Some(init_containers),
                        affinity,
                        security_context: Some(PodSecurityContext {
                            fs_group: Some(1000),
                            ..Default::default()
                        }),
                        volumes: Some(volumes),
                        restart_policy: Some("Never".into()),
                        ..Default::default()
                    }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::core::v1::EnvVar;
    use kubimo::{WorkspaceIndexerPod, WorkspaceRestoreFrom, WorkspaceSpec};

    fn workspace(spec: WorkspaceSpec) -> Workspace {
        Workspace::new("ws", spec)
    }

    #[test]
    fn test_init_containers_without_restore() {
        let workspace = workspace(WorkspaceSpec {
            init_containers: Some(vec![Container {
                name: "user".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let containers = build_init_containers("marimo:test", "ws", &workspace);
        assert_eq!(
            containers
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["init-dirs", "user"]
        );
    }

    #[test]
    fn test_init_containers_with_restore() {
        let workspace = workspace(WorkspaceSpec {
            restore_from: Some(WorkspaceRestoreFrom {
                bucket: "bucket".to_string(),
                key_prefix: Some("workspace/".to_string()),
                pod: Some(WorkspaceIndexerPod {
                    env: Some(vec![EnvVar {
                        name: "AWS_ACCESS_KEY_ID".to_string(),
                        value: Some("id".to_string()),
                        ..Default::default()
                    }]),
                    env_from: None,
                }),
            }),
            init_containers: Some(vec![Container {
                name: "user".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let containers = build_init_containers("marimo:test", "ws", &workspace);
        assert_eq!(
            containers
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["init-dirs", "restore", "user"]
        );
        let restore = &containers[1];
        assert_eq!(restore.image.as_deref(), Some("marimo:test"));
        assert_eq!(
            restore.command.as_deref(),
            Some(["/app/indexer".to_string()].as_slice())
        );
        assert_eq!(
            restore.args.as_deref(),
            Some(
                [
                    "download".to_string(),
                    "--bucket".to_string(),
                    "bucket".to_string(),
                    "--key-prefix".to_string(),
                    "workspace/".to_string(),
                    indexer::INIT_WORKSPACE_DIR.to_string(),
                ]
                .as_slice()
            )
        );
        let env = restore.env.as_deref().unwrap();
        assert!(env.iter().any(|e| e.name == "AWS_ACCESS_KEY_ID"));
        assert!(env.iter().any(|e| e.name == "RUST_LOG"));
        let mounts = restore.volume_mounts.as_deref().unwrap();
        assert_eq!(mounts[0].mount_path, indexer::INIT_MOUNT_DIR);
        assert_eq!(mounts[0].name, "ws");
        let security = restore.security_context.as_ref().unwrap();
        assert_eq!(security.run_as_user, Some(1000));
        assert_eq!(security.run_as_group, Some(1000));
    }
}
