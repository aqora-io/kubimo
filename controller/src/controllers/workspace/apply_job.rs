use git_url_parse::GitUrl;
use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, EnvFromSource, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec,
    PodTemplateSpec, SecretEnvSource, Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Workspace, prelude::*};

use crate::command::cmd;
use crate::context::Context;

use super::WorkspaceReconciler;

fn construct_command(workspace: &Workspace) -> Vec<String> {
    let mut command = cmd!["bash", "/setup/init.sh"];
    if let Some(git) = workspace.spec.git_config.as_ref() {
        if let Some(name) = git.name.as_deref() {
            command.extend(cmd!["--git-name", name,]);
        }
        if let Some(name) = git.email.as_deref() {
            command.extend(cmd!["--git-email", name,]);
        }
    }
    if let Some(repo) = workspace.spec.repo.as_ref() {
        if let Ok(url) = GitUrl::parse(&repo.url)
            && url.host().is_some()
            && url.scheme().is_some_and(|s| s.ends_with("ssh"))
        {
            command.extend(cmd!["--ssh-host", url.host().unwrap()]);
            if let Some(port) = url.port() {
                command.extend(cmd!["--ssh-port", port]);
            }
        }
        command.extend(cmd!["--repo", repo.url]);
        if let Some(branch) = repo.branch.as_ref() {
            command.extend(cmd!["--branch", branch]);
        }
        if let Some(revision) = repo.revision.as_ref() {
            command.extend(cmd!["--revision", revision]);
        }
    }
    if let Some(secret) = &workspace.spec.ssh_key {
        command.extend(cmd!["--ssh-key", secret]);
    }
    if let Some(s3_url) = &workspace
        .spec
        .s3_request
        .as_ref()
        .and_then(|s3_req| s3_req.url.as_ref())
    {
        command.extend(cmd!["--s3-url", s3_url]);
    }
    command
}

impl WorkspaceReconciler {
    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<Job, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
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
                        containers: vec![Container {
                            name: format!("{}-init", workspace_name),
                            image: Some(ctx.config.marimo_image_name.clone()),
                            volume_mounts: Some(vec![VolumeMount {
                                mount_path: "/home/me".to_string(),
                                name: workspace_name.into(),
                                ..Default::default()
                            }]),
                            env_from: Some(vec![EnvFromSource {
                                secret_ref: Some(
                                    workspace
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
                            command: Some(construct_command(workspace)),
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
        ctx.api_with_namespace::<Job>(namespace).patch(&job).await
    }
}
