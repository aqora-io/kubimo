use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::kube::runtime::controller::Action;
use kubimo::{Runner, prelude::*};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct RunnerStatusReconciler;

#[async_trait::async_trait]
impl Reconciler for RunnerStatusReconciler {
    type Resource = Runner;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, runner: &Runner) -> Result<Action, Self::Error> {
        tracing::warn!(
            "Runner {} {:?}",
            runner.name()?,
            runner.metadata.deletion_timestamp
        );
        Ok(Action::requeue(Duration::from_secs(
            ctx.config.runner_status.interval_secs,
        )))
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Runner, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    Ok(crate::controllers::runner::controller(&ctx)
        .graceful_shutdown_on(shutdown_signal)
        .run(
            RunnerStatusReconciler.reconcile("runner_status").await?,
            default_error_policy,
            ctx,
        ))
}
