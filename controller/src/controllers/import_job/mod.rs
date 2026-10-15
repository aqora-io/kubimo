mod apply_job;
mod apply_owner_reference;
mod apply_status;

use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::kube::runtime::{Controller, controller::Action, watcher};
use kubimo::{ImportJob, Workspace, prelude::*};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::controllers::lost_node::{LostNodeSweep, recheck_while_running, release_stranded_pods};
use crate::controllers::runner::is_workspace_ready;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

use apply_job::{AppliedJob, IMPORT_JOB_LABEL};

#[derive(Debug, Clone, Copy)]
struct ImportJobReconciler;

#[async_trait::async_trait]
impl Reconciler for ImportJobReconciler {
    type Resource = ImportJob;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, import_job: &ImportJob) -> Result<Action, Self::Error> {
        // A job-only workspace has no runner to release its pods from a lost
        // node, and a stranded pod pins this job's pod to that node too.
        let namespace = import_job.require_namespace()?;
        let sweep = release_stranded_pods(ctx, namespace, &import_job.spec.workspace).await;
        if sweep == LostNodeSweep::Released {
            return Ok(sweep.settle(Action::await_change()));
        }
        let action = self.apply_job_resources(ctx, import_job).await?;
        Ok(sweep.settle(action))
    }
}

impl ImportJobReconciler {
    async fn apply_job_resources(
        &self,
        ctx: &Context,
        import_job: &ImportJob,
    ) -> Result<Action, kubimo::Error> {
        // An import runs once. Finished is final, even if its Job is later
        // deleted: re-running it would overwrite whatever the workspace has
        // done with the files since.
        if apply_status::is_finished(import_job) {
            return Ok(Action::await_change());
        }

        let namespace = import_job.require_namespace()?;
        let workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get_opt(&import_job.spec.workspace)
            .await?;
        let Some(workspace) = workspace else {
            return Err(kubimo::Error::Custom(format!(
                "ImportJob bound to workspace that does not exist: {workspace:?}",
                workspace = import_job.spec.workspace
            )));
        };

        // Before the readiness gate, so the ImportJob is garbage-collected
        // even if the workspace never becomes ready.
        self.apply_owner_reference(ctx, import_job, &workspace)
            .await?;

        if !is_workspace_ready(&workspace) {
            return Ok(Action::requeue(Duration::from_secs(5)));
        }

        let job = match self.apply_job(ctx, import_job, &workspace).await? {
            AppliedJob::Job(job) => job,
            AppliedJob::Waiting(retry) => return Ok(Action::requeue(retry)),
            AppliedJob::Refused { reason, message } => {
                let condition =
                    apply_status::failed(reason, message, import_job.metadata.generation);
                self.patch_condition(ctx, import_job, condition).await?;
                return Ok(Action::await_change());
            }
        };
        // Job status changes come back through `.owns(jobs)`.
        self.apply_status(ctx, import_job, &job).await?;
        Ok(recheck_while_running(&job))
    }
}

#[allow(clippy::result_large_err)]
pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<ImportJob, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let import_jobs = ctx.api_global::<ImportJob>().kube().clone();
    let jobs = ctx.api_global::<Job>().kube().clone();
    Ok(Controller::new(import_jobs, Default::default())
        .owns(jobs, watcher::Config::default().labels(IMPORT_JOB_LABEL))
        .graceful_shutdown_on(shutdown_signal)
        .run(
            ImportJobReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
