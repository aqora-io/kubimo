mod apply_owner_reference;
mod apply_pod;
mod apply_service;

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::prelude::*;
use kubimo::k8s_openapi::api::core::v1::{Pod, Service};
use kubimo::kube::runtime::{Controller, controller::Action};
use kubimo::{KubimoLabel, KubimoRunner, prelude::*};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct RunnerReconciler;

impl RunnerReconciler {
    const PORT: i32 = 3000;
    fn pod_labels(&self, runner: &KubimoRunner) -> kubimo::Result<BTreeMap<String, String>> {
        Ok([(
            KubimoLabel::new("name").to_string(),
            runner.name()?.to_string(),
        )]
        .into_iter()
        .collect())
    }
}

#[async_trait::async_trait]
impl Reconciler for RunnerReconciler {
    type Resource = KubimoRunner;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, runner: &KubimoRunner) -> Result<Action, Self::Error> {
        futures::future::try_join_all([
            self.apply_owner_references(ctx, runner).boxed(),
            self.apply_pod(ctx, runner).map_ok(|_| ()).boxed(),
            self.apply_service(ctx, runner).map_ok(|_| ()).boxed(),
        ])
        .await?;
        Ok(Action::await_change())
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
    let svcs = ctx.api::<Service>().kube().clone();
    Ok(Controller::new(bmors, Default::default())
        .owns(pods, Default::default())
        .owns(svcs, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            RunnerReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
