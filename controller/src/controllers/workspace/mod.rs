mod apply_indexer;
mod apply_indexer_rbac;
mod apply_job;
mod apply_pvc;
mod apply_status;
mod cleanup_indexer;

pub(crate) use apply_pvc::pvc_storage_request;
pub(crate) use apply_status::BUDGET_EXCEEDED_REASON;

use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::k8s_crd_snapshot_storage::VolumeSnapshot;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, ServiceAccount};
use kubimo::k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kubimo::kube::runtime::{Controller, controller::Action, reflector::ObjectRef, watcher};
use kubimo::prelude::*;
use kubimo::{Runner, Workspace};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

/// Sibling Workspace deletions that free up budget do not trigger a refused
/// Workspace, so recheck periodically.
const BUDGET_REQUEUE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
struct WorkspaceReconciler;

#[async_trait::async_trait]
impl Reconciler for WorkspaceReconciler {
    type Resource = Workspace;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, workspace: &Workspace) -> Result<Action, Self::Error> {
        let (plan, current_limit) = self.plan_storage(ctx, workspace).await?;
        if let Some(reason) = plan.refuse {
            self.apply_budget_status(ctx, workspace, &reason).await?;
            return Ok(Action::requeue(BUDGET_REQUEUE_INTERVAL));
        }
        futures::future::try_join_all([
            self.apply_pvc(ctx, workspace, plan.request, current_limit)
                .map_ok(|_| ())
                .boxed(),
            self.apply_job(ctx, workspace).map_ok(|_| ()).boxed(),
            self.apply_indexer_rbac(ctx, workspace).boxed(),
            self.apply_indexer(ctx, workspace).boxed(),
            self.apply_status(ctx, workspace).boxed(),
        ])
        .await?;
        Ok(Action::await_change())
    }

    async fn cleanup(&self, ctx: &Context, workspace: &Workspace) -> Result<Action, Self::Error> {
        self.cleanup_indexer(ctx, workspace).await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Workspace, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let bmows = ctx.api_global::<Workspace>().kube().clone();
    let pvc = ctx.api_global::<PersistentVolumeClaim>().kube().clone();
    let jobs = ctx.api_global::<Job>().kube().clone();
    let pods = ctx.api_global::<Pod>().kube().clone();
    let service_accounts = ctx.api_global::<ServiceAccount>().kube().clone();
    let roles = ctx.api_global::<Role>().kube().clone();
    let role_bindings = ctx.api_global::<RoleBinding>().kube().clone();
    let runners = ctx.api_global::<Runner>().kube().clone();
    let snaps = ctx.api_global::<VolumeSnapshot>().kube().clone();
    Ok(Controller::new(bmows, Default::default())
        .owns(pvc, Default::default())
        .owns(jobs, Default::default())
        .owns(pods, Default::default())
        .owns(service_accounts, Default::default())
        .owns(roles, Default::default())
        .owns(role_bindings, Default::default())
        .owns(snaps, Default::default())
        .watches(runners, watcher::Config::default(), |runner| {
            runner
                .namespace()
                .map(|namespace| ObjectRef::new(&runner.spec.workspace).within(&namespace))
        })
        .graceful_shutdown_on(shutdown_signal)
        .run(
            WorkspaceReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
