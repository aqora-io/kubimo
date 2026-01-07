mod apply_job;
mod apply_owner_reference;

use std::sync::Arc;

use futures::prelude::*;
use kubimo::CacheJob;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct CacheJobReconciler;

#[async_trait::async_trait]
impl Reconciler for CacheJobReconciler {
    type Resource = CacheJob;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, cache_job: &CacheJob) -> Result<Action, Self::Error> {
        futures::future::try_join_all([
            self.apply_owner_reference(ctx, cache_job).boxed(),
            self.apply_job(ctx, cache_job).map_ok(|_| ()).boxed(),
        ])
        .await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<CacheJob, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let cache_jobs = ctx.api_global::<CacheJob>().kube().clone();
    let jobs = ctx.api_global::<Job>().kube().clone();
    Ok(Controller::new(cache_jobs, Default::default())
        .owns(jobs, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            CacheJobReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
