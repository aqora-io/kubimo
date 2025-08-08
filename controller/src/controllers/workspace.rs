use std::sync::Arc;

use futures::Stream;
use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
use kubimo::kube::{
    api::ObjectMeta,
    runtime::{
        Controller,
        controller::Action,
        finalizer::{Error as FinalizerError, Event, finalizer},
    },
};
use kubimo::{KubimoLabel, KubimoWorkspace, prelude::*};

use crate::context::Context;
use crate::error::ControllerResult;
use crate::resources::{ResourceRequirement, Resources};
use crate::status::wrap_reconcile;

type ReconcileError = FinalizerError<kubimo::Error>;

async fn reconcile(
    workspace: Arc<KubimoWorkspace>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    finalizer(
        ctx.api::<KubimoWorkspace>().kube(),
        &KubimoLabel::new("controller").to_string(),
        workspace,
        |event| async move {
            match event {
                Event::Apply(workspace) => wrap_reconcile(workspace, ctx, reconcile_apply).await,
                Event::Cleanup(_) => Ok(Action::await_change()),
            }
        },
    )
    .await
}

#[tracing::instrument(skip(ctx), ret, err)]
async fn reconcile_apply(
    workspace: Arc<KubimoWorkspace>,
    ctx: Arc<Context>,
) -> kubimo::Result<Action> {
    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: workspace.metadata.name.clone(),
            namespace: workspace.metadata.namespace.clone(),
            owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteMany".to_string()]),
            resources: Resources {
                requests: ResourceRequirement {
                    storage: workspace.spec.min_storage.clone(),
                    ..Default::default()
                },
                limits: ResourceRequirement {
                    storage: workspace.spec.max_storage.clone(),
                    ..Default::default()
                },
            }
            .into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    ctx.client
        .api::<PersistentVolumeClaim>()
        .patch(&pvc)
        .await?;
    Ok(Action::await_change())
}

fn error_policy(
    _object: Arc<KubimoWorkspace>,
    _error: &ReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    Action::await_change()
}

pub fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> impl Stream<Item = ControllerResult<KubimoWorkspace, ReconcileError>> {
    let bmows = ctx.api::<KubimoWorkspace>().kube().clone();
    let pvc = ctx.api::<PersistentVolumeClaim>().kube().clone();
    Controller::new(bmows, Default::default())
        .owns(pvc, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(reconcile, error_policy, ctx)
}
