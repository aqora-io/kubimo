mod apply_status;

use std::sync::Arc;

use futures::prelude::*;
use kubimo::Workspace;
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct WorkspaceReconciler;

#[async_trait::async_trait]
impl Reconciler for WorkspaceReconciler {
    type Resource = Workspace;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, workspace: &Workspace) -> Result<Action, Self::Error> {
        // A workspace owns no cluster resources of its own. Its files live in
        // S3 and its slot is carved out of the node data volume by the agent
        // when kubelet publishes the runner's inline volume — so there is
        // nothing to provision here beyond the status.
        self.apply_status(ctx, workspace).await?;
        Ok(Action::await_change())
    }

    async fn cleanup(&self, _ctx: &Context, _workspace: &Workspace) -> Result<Action, Self::Error> {
        // Nothing to tear down: the slot is dropped by the agent when the last
        // pod unpublishes, and the platform purges the S3 archive. The
        // finalizer wrapper stays because every existing Workspace already
        // carries the "controller" finalizer string — removing the wrapper
        // would leave those objects undeletable.
        Ok(Action::await_change())
    }
}

#[allow(clippy::result_large_err)]
pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Workspace, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let bmows = ctx.api_global::<Workspace>().kube().clone();
    Ok(Controller::new(bmows, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            WorkspaceReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
