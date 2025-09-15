use kubimo::k8s_openapi::api::core::v1::{
    Container, ContainerPort, PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec,
    Probe, TCPSocketAction, Volume, VolumeMount,
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
                containers: vec![Container {
                    name: format!("{}-runner", runner.name()?),
                    image: Some(ctx.config.marimo_image_name.clone()),
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
                    liveness_probe: Some(Probe {
                        tcp_socket: Some(TCPSocketAction {
                            port: IntOrString::Int(80),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    command: Some(cmd![
                        "bash",
                        "/setup/start.sh",
                        "--base-url",
                        self.ingress_path(runner)?,
                        match runner.spec.command {
                            RunnerCommand::Edit => "edit",
                            RunnerCommand::Run => "run",
                        }
                    ]),
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
        ctx.api::<Pod>().patch(&pod).await
    }
}
