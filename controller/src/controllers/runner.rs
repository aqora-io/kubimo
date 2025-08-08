use std::collections::BTreeMap;
use std::sync::Arc;

use futures::Stream;
use kubimo::k8s_openapi::api::core::v1::{Container, Pod, PodSpec, ResourceRequirements};
use kubimo::kube::api::ObjectMeta;
use kubimo::kube::runtime::{
    Controller,
    controller::Action,
    finalizer::{Error as FinalizerError, Event, finalizer},
};
use kubimo::{KubimoLabel, KubimoRunner, KubimoWorkspace, prelude::*};

use crate::context::Context;
use crate::error::ControllerResult;
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
    let workspace = ctx
        .api::<KubimoWorkspace>()
        .get(runner.spec.workspace.as_ref())
        .await?;

    let bmors = ctx.api::<KubimoRunner>();
    let mut runner = bmors.get(runner.name()?).await?;
    let oref = workspace.static_controller_owner_ref()?;
    if !runner
        .meta()
        .owner_references
        .as_ref()
        .map(|orefs| orefs.contains(&oref))
        .unwrap_or(false)
    {
        runner
            .meta_mut()
            .owner_references
            .get_or_insert_default()
            .push(oref);
        runner = bmors.patch(&runner).await?;
    }

    let name = runner.name()?;

    let requests = if runner.spec.min_memory.is_some() || runner.spec.min_cpu.is_some() {
        let mut requests = BTreeMap::default();
        if let Some(min_memory) = runner.spec.min_memory.clone() {
            requests.insert("memory".to_string(), min_memory.into());
        }
        if let Some(min_cpu) = runner.spec.min_cpu.clone() {
            requests.insert("cpu".to_string(), min_cpu.into());
        }
        Some(requests)
    } else {
        None
    };
    let limits = if runner.spec.max_memory.is_some() || runner.spec.max_cpu.is_some() {
        let mut limits = BTreeMap::default();
        if let Some(max_memory) = runner.spec.max_memory.clone() {
            limits.insert("memory".to_string(), max_memory.into());
        }
        if let Some(max_cpu) = runner.spec.max_cpu.clone() {
            limits.insert("cpu".to_string(), max_cpu.into());
        }
        Some(limits)
    } else {
        None
    };
    let resources = if requests.is_some() || limits.is_some() {
        Some(ResourceRequirements {
            requests,
            limits,
            ..Default::default()
        })
    } else {
        None
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
                name: format!("{name}-runner"),
                image: Some(ctx.config.marimo_base_image_name.clone()),
                resources,
                command: Some(
                    ["tail", "-f", "/dev/null"]
                        .into_iter()
                        .map(ToString::to_string)
                        .collect(),
                ),
                ..Default::default()
            }],
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
