use git_url_parse::{GitUrl, Scheme};
use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{KubimoWorkspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        workspace: &KubimoWorkspace,
    ) -> Result<Option<Job>, kubimo::Error> {
        let mut script: Vec<String> = cmd!["set -ex"];
        if let Some(secret) = &workspace.spec.ssh_key {
            script.extend(cmd![
                format!("cat > /home/me/.ssh/id_kubimo << EOM\n{secret}\nEOM"),
                "chmod 600 /home/me/.ssh/id_kubimo",
                "echo 'IdentityFile /home/me/.ssh/id_kubimo' >> /home/me/.ssh/config",
                "chmod 600 /home/me/.ssh/config",
            ]);
        }
        // TODO: make this configurable
        script.push("git config --global user.name 'Kubimo'".into());
        script.push("git config --global user.email 'kubimo@local.domain'".into());
        if let Some(repo) = workspace.spec.repo.as_ref() {
            if let Ok(url) = GitUrl::parse(repo)
                && url.host.is_some()
                && matches!(url.scheme, Scheme::Ssh | Scheme::GitSsh)
            {
                let mut ssh_keyscan = cmd!["ssh-keyscan"];
                if let Some(port) = url.port {
                    ssh_keyscan.extend(cmd!["-p", port]);
                }
                ssh_keyscan.extend(cmd![url.host.unwrap(), ">>", "/home/me/.ssh/known_hosts"]);
                script.push(ssh_keyscan.join(" "));
            }
            script.push(format!("git clone {repo} /home/me/workspace"));
        }
        let command = cmd!["sh", "-c", script.join("\n")];
        let workspace_name = workspace.name()?;
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
                        runtime_class_name: Some("gvisor".to_string()),
                        containers: vec![Container {
                            name: format!("{}-init", workspace_name),
                            image: Some(ctx.config.marimo_image_name.clone()),
                            volume_mounts: Some(vec![VolumeMount {
                                mount_path: "/home/me".to_string(),
                                name: workspace_name.into(),
                                ..Default::default()
                            }]),
                            // command: Some(cmd!["git", "clone", repo, "/workspace"]),
                            command: Some(command),
                            ..Default::default()
                        }],
                        init_containers: Some(vec![Container {
                            name: format!("{}-init-dirs", workspace_name),
                            image: Some("busybox".into()),
                            volume_mounts: Some(vec![VolumeMount {
                                mount_path: "/home/me".to_string(),
                                name: workspace_name.into(),
                                ..Default::default()
                            }]),
                            command: Some(cmd![
                                "sh",
                                "-c",
                                r#"
set -ex
mkdir -p /home/me/.ssh
mkdir -p /home/me/workspace
chown -R 1000:1000 /home/me
"#,
                            ]),
                            ..Default::default()
                        }]),
                        security_context: Some(PodSecurityContext {
                            fs_group: Some(1000),
                            ..Default::default()
                        }),
                        volumes: Some(vec![Volume {
                            name: workspace_name.into(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: workspace_name.into(),
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
        Ok(Some(ctx.client.api::<Job>().patch(&job).await?))
    }
}
