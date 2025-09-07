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

use super::{Error, WorkspaceReconciler};

fn construct_command(workspace: &KubimoWorkspace) -> Result<Vec<String>, shlex::QuoteError> {
    let mut script: Vec<String> = cmd!["set -ex"];
    if let Some(git) = workspace.spec.git.as_ref() {
        if let Some(secret) = &git.ssh_key {
            script.extend(cmd![
                format!(
                    "{} > /home/me/.ssh/id_kubimo",
                    shlex::try_join(["echo", secret.as_str()])?
                ),
                "chmod 600 /home/me/.ssh/id_kubimo",
                "echo 'IdentityFile /home/me/.ssh/id_kubimo' >> /home/me/.ssh/config",
                "chmod 600 /home/me/.ssh/config",
            ]);
        }
        if let Some(name) = git.config_name.as_deref() {
            script.push(shlex::try_join([
                "git",
                "config",
                "--global",
                "user.name",
                name,
            ])?);
        }
        if let Some(name) = git.config_email.as_deref() {
            script.push(shlex::try_join([
                "git",
                "config",
                "--global",
                "user.email",
                name,
            ])?);
        }
    }
    if let Some(repo) = workspace.spec.repo.as_ref() {
        if let Ok(url) = GitUrl::parse(&repo.url)
            && url.host.is_some()
            && matches!(url.scheme, Scheme::Ssh | Scheme::GitSsh)
        {
            let mut ssh_keyscan = cmd!["ssh-keyscan"];
            if let Some(port) = url.port {
                ssh_keyscan.extend(cmd!["-p", port]);
            }
            ssh_keyscan.push(url.host.unwrap());
            script.push(format!(
                "{} >> /home/me/.ssh/known_hosts",
                shlex::try_join(ssh_keyscan.iter().map(|s| s.as_str()))?
            ));
        }
        let mut clone = cmd!["git", "clone", "--depth", "1", "--recurse-submodules"];
        if let Some(branch) = repo.branch.as_ref() {
            clone.extend(cmd!["--branch", branch]);
        }
        if let Some(revision) = repo.revision.as_ref() {
            clone.extend(cmd!["--revision", revision]);
        }
        clone.extend(cmd![repo.url, "/home/me/workspace"]);
        script.push(shlex::try_join(clone.iter().map(|s| s.as_str()))?);
    }
    Ok(cmd!["sh", "-c", script.join("\n")])
}

impl WorkspaceReconciler {
    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        workspace: &KubimoWorkspace,
    ) -> Result<Option<Job>, Error> {
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
                            command: Some(construct_command(workspace)?),
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
