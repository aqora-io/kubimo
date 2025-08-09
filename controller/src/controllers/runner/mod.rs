mod apply_owner_reference;
mod apply_pod;

use std::sync::Arc;

use futures::Stream;
use kubimo::KubimoRunner;
use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct RunnerReconciler;

#[async_trait::async_trait]
impl Reconciler for RunnerReconciler {
    type Resource = KubimoRunner;
    type Error = kubimo::Error;

    #[tracing::instrument(skip(ctx), ret, err)]
    async fn apply(&self, ctx: &Context, runner: &KubimoRunner) -> Result<Action, Self::Error> {
        self.apply_owner_references(ctx, runner).await?;
        self.apply_pod(ctx, runner).await?;
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
    Ok(Controller::new(bmors, Default::default())
        .owns(pods, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            RunnerReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
