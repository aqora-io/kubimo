use kubimo::k8s_openapi::api::core::v1::{
    Container, ContainerPort, HTTPGetAction, PersistentVolumeClaimVolumeSource, Pod,
    PodSecurityContext, PodSpec, Probe, Volume, VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kubimo::kube::api::ObjectMeta;
use kubimo::{Runner, RunnerCommand, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::resources::Resources;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_pod(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<Pod, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let ingress_path = self.ingress_path(runner)?;
        let path_prefix = ingress_path.strip_suffix('/').unwrap_or(&ingress_path);
        let probe_action = HTTPGetAction {
            path: Some(match runner.spec.command {
                RunnerCommand::Edit => format!("{path_prefix}/health"),
                RunnerCommand::Run => format!("{path_prefix}/_health"),
            }),
            port: IntOrString::Int(80),
            ..Default::default()
        };
        let mut command = cmd!["bash", "/setup/start.sh", "--base-url", ingress_path,];
        if let Some(token) = runner
            .spec
            .token
            .as_ref()
            .and_then(|token| token.value.as_ref())
        {
            command.extend(cmd!["--token", token]);
        }
        command.push(
            match runner.spec.command {
                RunnerCommand::Edit => "edit",
                RunnerCommand::Run => "run",
            }
            .into(),
        );
        let pod = Pod {
            metadata: ObjectMeta {
                name: runner.metadata.name.clone(),
                namespace: runner.metadata.namespace.clone(),
                owner_references: Some(vec![runner.static_controller_owner_ref()?]),
                labels: Some(self.pod_labels(runner)?),
                ..Default::default()
            },
            spec: Some(PodSpec {
                runtime_class_name: Some("gvisor".to_string()),
                automount_service_account_token: Some(false),
                enable_service_links: Some(false),
                security_context: Some(PodSecurityContext {
                    fs_group: Some(1000),
                    ..Default::default()
                }),
                hostname: Some("kubimo".into()),
                containers: vec![Container {
                    name: "runner".into(),
                    image: Some(ctx.config.marimo_image.clone()),
                    resources: Resources::default()
                        .cpu(runner.spec.cpu.clone())
                        .memory(runner.spec.memory.clone())
                        .into(),
                    volume_mounts: Some(vec![VolumeMount {
                        mount_path: "/home/me".to_string(),
                        name: runner.spec.workspace.clone(),
                        ..Default::default()
                    }]),
                    ports: Some(vec![ContainerPort {
                        container_port: 80,
                        name: Some("marimo".to_string()),
                        ..Default::default()
                    }]),
                    env: runner.spec.env.clone(),
                    env_from: runner.spec.env_from.clone(),
                    startup_probe: Some(Probe {
                        http_get: Some(probe_action.clone()),
                        failure_threshold: Some(90),
                        period_seconds: Some(1),
                        ..Default::default()
                    }),
                    liveness_probe: Some(Probe {
                        http_get: Some(probe_action.clone()),
                        period_seconds: Some(10),
                        ..Default::default()
                    }),
                    command: Some(command),
                    ..Default::default()
                }],
                volumes: Some(vec![Volume {
                    name: runner.spec.workspace.clone(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: runner.spec.workspace.clone(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<Pod>(namespace).patch(&pod).await
    }
}
