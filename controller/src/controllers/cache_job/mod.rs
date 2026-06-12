mod apply_job;
mod apply_owner_reference;

use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::kube::runtime::{Controller, controller::Action};
use kubimo::{CacheJob, Workspace, prelude::*};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::controllers::runner::is_workspace_ready;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct CacheJobReconciler;

#[async_trait::async_trait]
impl Reconciler for CacheJobReconciler {
    type Resource = CacheJob;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, cache_job: &CacheJob) -> Result<Action, Self::Error> {
        let namespace = cache_job.require_namespace()?;
        let workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get_opt(&cache_job.spec.workspace)
            .await?;
        // Workspace does not exist
        let Some(workspace) = workspace else {
            return Err(kubimo::Error::Custom(format!(
                "CacheJob bound to workspace that does not exist: {workspace:?}",
                workspace = cache_job.spec.workspace
            )));
        };

        // Set the owner reference before gating on readiness so the CacheJob
        // is garbage-collected even if the workspace never becomes ready.
        self.apply_owner_reference(ctx, cache_job, &workspace)
            .await?;

        // The cache job mounts the workspace volume; running it before the
        // workspace's init job has populated the volume fails `uv sync`.
        if !is_workspace_ready(&workspace) {
            return Ok(Action::requeue(Duration::from_secs(5)));
        }

        self.apply_job(ctx, cache_job).await?;
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
