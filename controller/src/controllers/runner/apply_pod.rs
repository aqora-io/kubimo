use kubimo::k8s_openapi::api::core::v1::{
    Container, ContainerPort, PersistentVolumeClaimVolumeSource, Pod, PodSpec, Probe,
    TCPSocketAction, Volume, VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kubimo::kube::api::ObjectMeta;
use kubimo::{KubimoRunner, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::resources::{ResourceRequirement, Resources};

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_pod(
        &self,
        ctx: &Context,
        runner: &KubimoRunner,
    ) -> Result<Pod, kubimo::Error> {
        let volume_mount = VolumeMount {
            mount_path: "/workspace".to_string(),
            name: runner.spec.workspace.clone(),
            ..Default::default()
        };
        let resources = Resources {
            requests: ResourceRequirement {
                cpu: runner.spec.min_cpu.clone(),
                memory: runner.spec.min_memory.clone(),
                ..Default::default()
            },
            limits: ResourceRequirement {
                cpu: runner.spec.max_cpu.clone(),
                memory: runner.spec.max_memory.clone(),
                ..Default::default()
            },
        };
        let ingress_path = self.ingress_path(runner)?;
        let pod = Pod {
            metadata: ObjectMeta {
                name: runner.metadata.name.clone(),
                namespace: runner.metadata.namespace.clone(),
                owner_references: Some(vec![runner.static_controller_owner_ref()?]),
                labels: Some(self.pod_labels(runner)?),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: format!("{}-runner", runner.name()?),
                    image: Some(ctx.config.marimo_base_image_name.clone()),
                    resources: resources.clone().into(),
                    volume_mounts: Some(vec![volume_mount.clone()]),
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
                        "uv",
                        "run",
                        "marimo",
                        "--log-level=info",
                        "--yes",
                        "edit",
                        "--headless",
                        "--watch",
                        "--host=0.0.0.0",
                        "--port=80",
                        "--base-url={ingress_path}",
                        "--allow-origins='*'",
                        "--no-token",
                        "--skip-update-check",
                    ]),
                    ..Default::default()
                }],
                init_containers: Some(vec![Container {
                    name: format!("{}-init", runner.name()?),
                    image: Some(ctx.config.marimo_init_image_name.clone()),
                    resources: Some(resources.clone().into()),
                    volume_mounts: Some(vec![volume_mount.clone()]),
                    command: Some(cmd!["sh", "/setup/init.sh"]),
                    ..Default::default()
                }]),
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
