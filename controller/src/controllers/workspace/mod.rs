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
use kubimo::{Runner, Workspace, WorkspaceMode};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::controllers::runner::is_already_exists;
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
        // A pooled workspace owns no cluster resources of its own. Its files
        // live in S3 and its slot is carved out of the node data volume by the
        // agent when kubelet publishes the runner's inline volume — so there is
        // no PVC to size, no init Job to seed a volume, and no per-workspace
        // indexer pod. Falling through to the dedicated path would provision
        // exactly the per-workspace volume pooled mode exists to avoid.
        if workspace.effective_mode(ctx.config.default_workspace_mode) == WorkspaceMode::Pooled {
            self.apply_status(ctx, workspace).await?;
            return Ok(Action::await_change());
        }
        let (plan, current_limit) = self.plan_storage(ctx, workspace).await?;
        if let Some(reason) = plan.refuse {
            self.apply_budget_status(ctx, workspace, &reason).await?;
            return Ok(Action::requeue(BUDGET_REQUEUE_INTERVAL));
        }
        let applied = futures::future::try_join_all([
            self.apply_pvc(ctx, workspace, plan.request, current_limit)
                .map_ok(|_| false)
                .boxed(),
            self.apply_job(ctx, workspace).map_ok(|_| false).boxed(),
            self.apply_indexer_rbac(ctx, workspace)
                .map_ok(|_| false)
                .boxed(),
            self.apply_indexer(ctx, workspace)
                .map_ok(|applied| matches!(applied, apply_indexer::IndexerApply::Replaced))
                .boxed(),
            self.apply_status(ctx, workspace).map_ok(|_| false).boxed(),
        ])
        .await;
        // Recreating the indexer pod while the previous one is still Terminating is a
        // 409, and a transient one: the name frees up as soon as it finishes. Left to
        // propagate, it would log a reconcile error on every reconcile that lands in a
        // replaced pod's termination window.
        //
        // Everything else propagates, 422s included. A 422 that apply_indexer did *not*
        // confirm as the known spec drift is an ordinary validation failure and never
        // converges; requeuing those on a fixed 2s timer would spin forever instead of
        // letting the controller's backoff space them out.
        let replaced = match applied {
            Ok(outcomes) => outcomes.into_iter().any(|replaced| replaced),
            Err(err) if is_already_exists(&err) => {
                return Ok(Action::requeue(Duration::from_secs(2)));
            }
            Err(err) => return Err(err),
        };
        if replaced {
            // apply_indexer deleted a pod whose immutable spec had drifted. Nothing has
            // recreated it, and its deletion is not a change this controller can wait on,
            // so come back once the old one has finished terminating and the name is free.
            tracing::info!(
                workspace = workspace.name()?,
                "replaced a drifted indexer pod; requeuing to recreate it"
            );
            return Ok(Action::requeue(Duration::from_secs(2)));
        }
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
