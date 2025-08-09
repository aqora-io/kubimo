use std::sync::Arc;

use futures::{
    Stream,
    future::{BoxFuture, FutureExt},
};
use kubimo::k8s_openapi::api::core::v1::{
    Container, ContainerPort, PersistentVolumeClaimVolumeSource, Pod, PodSpec, Probe,
    TCPSocketAction, Volume, VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kubimo::kube::api::ObjectMeta;
use kubimo::kube::runtime::{Controller, controller::Action};
use kubimo::{KubimoRunner, KubimoWorkspace, json_patch_macros::*, prelude::*};

use crate::backoff::default_error_policy;
use crate::command::cmd;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};
use crate::resources::{ResourceRequirement, Resources};

#[tracing::instrument(skip(ctx), ret, err)]
async fn reconcile_apply(runner: Arc<KubimoRunner>, ctx: Arc<Context>) -> kubimo::Result<Action> {
    if !runner
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|orefs| {
            orefs.iter().any(|oref| {
                oref.controller.is_some_and(|yes| yes)
                    && oref.kind == KubimoWorkspace::kind(&())
                    && oref.name == runner.spec.workspace
            })
        })
    {
        let workspace = ctx
            .api::<KubimoWorkspace>()
            .get(runner.spec.workspace.as_ref())
            .await?;
        let mut owner_refs = runner.metadata.owner_references.clone().unwrap_or_default();
        owner_refs.push(workspace.static_controller_owner_ref()?);
        ctx.api::<KubimoRunner>()
            .patch_json(
                runner.name()?,
                patch![add!(["metadata", "ownerReferences"] => owner_refs)],
            )
            .await?;
    }
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
    const PORT: i32 = 3000;
    const PORT_NAME: &str = "marimo";
    let pod = Pod {
        metadata: ObjectMeta {
            name: runner.metadata.name.clone(),
            namespace: runner.metadata.namespace.clone(),
            owner_references: Some(vec![runner.static_controller_owner_ref()?]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: format!("{}-runner", runner.name()?),
                image: Some(ctx.config.marimo_base_image_name.clone()),
                resources: resources.clone().into(),
                volume_mounts: Some(vec![volume_mount.clone()]),
                ports: Some(vec![ContainerPort {
                    container_port: PORT,
                    name: Some(PORT_NAME.to_string()),
                    ..Default::default()
                }]),
                liveness_probe: Some(Probe {
                    tcp_socket: Some(TCPSocketAction {
                        port: IntOrString::Int(PORT),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                command: Some(cmd![
                    "uv",
                    "run",
                    "marimo",
                    "edit",
                    "--headless",
                    "--host=0.0.0.0",
                    "--port={PORT}",
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
    ctx.api::<Pod>().patch(&pod).await?;
    Ok(Action::await_change())
}

struct RunnerReconciler;

impl Reconciler for RunnerReconciler {
    type Resource = KubimoRunner;
    type Error = kubimo::Error;
    fn apply(
        &self,
        runner: Arc<KubimoRunner>,
        ctx: Arc<Context>,
    ) -> BoxFuture<'static, Result<Action, Self::Error>> {
        reconcile_apply(runner, ctx).boxed()
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<KubimoRunner, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let bmors = ctx.api::<KubimoRunner>().kube().clone();
    let pods = ctx.api::<Pod>().kube().clone();
    Ok(Controller::new(bmors, Default::default())
        .owns(pods, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            RunnerReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
