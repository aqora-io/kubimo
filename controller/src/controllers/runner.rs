use std::sync::Arc;

use futures::Stream;
use kubimo::k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaimVolumeSource, Pod, PodSpec, Volume, VolumeMount,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::kube::runtime::{
    Controller,
    controller::Action,
    finalizer::{Error as FinalizerError, Event, finalizer},
};
use kubimo::{KubimoLabel, KubimoRunner, KubimoWorkspace, prelude::*};

use crate::command::Command;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::resources::{ResourceRequirement, Resources};
use crate::status::wrap_reconcile;

type ReconcileError = FinalizerError<kubimo::Error>;

async fn reconcile(runner: Arc<KubimoRunner>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    finalizer(
        ctx.api::<KubimoRunner>().kube(),
        &KubimoLabel::new("controller").to_string(),
        runner,
        |event| async move {
            match event {
                Event::Apply(runner) => wrap_reconcile(runner, ctx, reconcile_apply).await,
                Event::Cleanup(_) => Ok(Action::await_change()),
            }
        },
    )
    .await
}

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
        let bmors = ctx.api::<KubimoRunner>();
        let mut runner = bmors.get(runner.name()?).await?;
        runner
            .meta_mut()
            .owner_references
            .get_or_insert_default()
            .push(workspace.static_controller_owner_ref()?);
        bmors.patch(&runner).await?;
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
                command: Some(Command::fmt(["tail", "-f", "/dev/null"])),
                ..Default::default()
            }],
            init_containers: Some(vec![Container {
                name: format!("{}-init", runner.name()?),
                image: Some(ctx.config.marimo_init_image_name.clone()),
                resources: Some(resources.clone().into()),
                volume_mounts: Some(vec![volume_mount.clone()]),
                command: Some(Command::fmt(["sh", "/setup/init.sh"])),
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

fn error_policy(_object: Arc<KubimoRunner>, _error: &ReconcileError, _ctx: Arc<Context>) -> Action {
    Action::await_change()
}

pub fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> impl Stream<Item = ControllerResult<KubimoRunner, ReconcileError>> {
    let bmors = ctx.api::<KubimoRunner>().kube().clone();
    let pods = ctx.api::<Pod>().kube().clone();
    Controller::new(bmors, Default::default())
        .owns(pods, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(reconcile, error_policy, ctx)
}
