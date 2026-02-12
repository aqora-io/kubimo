mod apply_owner_reference;

use std::sync::Arc;

use futures::prelude::*;
use kubimo::WorkspaceDir;
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct WorkspaceDirectoryReconciler;

#[async_trait::async_trait]
impl Reconciler for WorkspaceDirectoryReconciler {
    type Resource = WorkspaceDir;
    type Error = kubimo::Error;

    async fn apply(
        &self,
        ctx: &Context,
        workspace_dir: &WorkspaceDir,
    ) -> Result<Action, Self::Error> {
        self.apply_owner_reference(ctx, workspace_dir).await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<WorkspaceDir, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let workspace_dirs = ctx.api_global::<WorkspaceDir>().kube().clone();
    Ok(Controller::new(workspace_dirs, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            WorkspaceDirectoryReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
