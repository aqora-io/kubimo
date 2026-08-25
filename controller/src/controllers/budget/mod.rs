use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::kube::runtime::{Controller, controller::Action};
use kubimo::{
    Budget, BudgetResourceStatus, BudgetStatus, FilterParams, Selector, StorageQuantity,
    StorageUnit, Workspace, prelude::*,
};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

/// How often a Budget refreshes its usage status. Sibling Workspace changes do
/// not trigger the Budget directly, so we poll (cf. `runner_status`).
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

const EXCEEDED: &str = "Exceeded";

#[derive(Debug, Clone, Copy)]
struct BudgetReconciler;

#[async_trait::async_trait]
impl Reconciler for BudgetReconciler {
    type Resource = Budget;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, budget: &Budget) -> Result<Action, Self::Error> {
        let namespace = budget.require_namespace()?;
        let used = sum_workspace_storage(ctx, namespace, &budget.spec.label_selector()).await?;
        let limit = budget.spec.storage.clone();
        let exceeded = limit
            .as_ref()
            .and_then(StorageQuantity::to_bytes)
            .is_some_and(|limit| used > limit);

        let mut patched = budget.clone();
        patched.status = Some(BudgetStatus {
            conditions: Some(vec![exceeded_condition(budget, exceeded)]),
            storage: Some(BudgetResourceStatus {
                used: Some(StorageQuantity::new(used as f64, StorageUnit::B)),
                limit,
            }),
        });
        ctx.api_namespaced::<Budget>(namespace)
            .patch_status(&patched)
            .await?;
        Ok(Action::requeue(REFRESH_INTERVAL))
    }
}

/// Committed storage (bytes) a Workspace contributes to its budget.
///
/// A workspace commits nothing up front: its slot is carved out of a shared,
/// overcommitted node volume, and the quota is a ceiling rather than a
/// reservation. What it actually occupies is the honest number, and it is
/// trustworthy — under an XFS project quota `statvfs` reports the slot's own
/// usage, which is what the agent writes here.
///
/// Deliberately not the slot quota: that is sized so a workspace does not hit
/// a wall, so billing it would charge every workspace its ceiling.
fn workspace_committed_bytes(workspace: &Workspace) -> u64 {
    workspace
        .status
        .as_ref()
        .and_then(|status| status.storage.as_ref())
        .and_then(|storage| storage.used.as_ref())
        .and_then(StorageQuantity::to_bytes)
        .unwrap_or(0)
}

/// Sum the committed storage of every Workspace matching `selector` (see
/// [`workspace_committed_bytes`]).
pub(crate) async fn sum_workspace_storage(
    ctx: &Context,
    namespace: &str,
    selector: &Selector,
) -> Result<u64, kubimo::Error> {
    let workspaces: Vec<Workspace> = ctx
        .api_namespaced::<Workspace>(namespace)
        .list(&FilterParams::new().with_labels(selector.clone()))
        .map_ok(|item| item.item)
        .try_collect()
        .await?;
    Ok(workspaces.iter().fold(0u64, |total, workspace| {
        total.saturating_add(workspace_committed_bytes(workspace))
    }))
}

/// `Exceeded` condition, preserving the previous transition time when the status
/// is unchanged.
fn exceeded_condition(budget: &Budget, exceeded: bool) -> Condition {
    let (status, reason, message) = if exceeded {
        (
            "True",
            "Exceeded",
            "Budget usage exceeds the configured limit",
        )
    } else {
        (
            "False",
            "WithinBudget",
            "Budget usage is within the configured limit",
        )
    };
    let previous = budget
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conditions| conditions.iter().find(|cond| cond.type_ == EXCEEDED));
    let last_transition_time = match previous {
        Some(previous) if previous.status == status => previous.last_transition_time.clone(),
        _ => Time(Timestamp::now()),
    };
    Condition {
        last_transition_time,
        observed_generation: budget.metadata.generation,
        message: message.into(),
        reason: reason.into(),
        status: status.into(),
        type_: EXCEEDED.into(),
    }
}

#[allow(clippy::result_large_err)]
pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Budget, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let budgets = ctx.api_global::<Budget>().kube().clone();
    Ok(Controller::new(budgets, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            BudgetReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::WorkspaceSpec;

    fn gib(n: u64) -> u64 {
        n * 1024 * 1024 * 1024
    }

    /// Build a Workspace with an optional `spec.storage.max`.
    fn workspace_with(max: Option<&str>) -> Workspace {
        let spec = WorkspaceSpec {
            storage: max.map(|max| kubimo::StorageRequirement {
                max: Some(max.parse().unwrap()),
            }),
            ..Default::default()
        };
        Workspace::new("ws", spec)
    }

    /// A workspace commits nothing up front — its slot comes out of a shared,
    /// overcommitted volume — so it is billed for what it actually occupies,
    /// not the `spec.storage.max` quota ceiling.
    #[test]
    fn committed_bytes_come_from_actual_usage_not_the_spec() {
        let mut ws = workspace_with(Some("2Gi"));
        ws.status.get_or_insert_default().storage = Some(kubimo::WorkspaceStorageStatus {
            used: Some(StorageQuantity::new(gib(7) as f64, kubimo::StorageUnit::B)),
            capacity: Some(StorageQuantity::new(gib(64) as f64, kubimo::StorageUnit::B)),
            available: None,
        });
        // Not 2Gi (spec.max), and not 64Gi (the reported capacity).
        assert_eq!(workspace_committed_bytes(&ws), gib(7));
    }

    /// Before the agent has reported anything, a workspace contributes nothing
    /// rather than a made-up figure.
    #[test]
    fn committed_bytes_are_zero_until_usage_is_reported() {
        let ws = workspace_with(Some("2Gi"));
        assert_eq!(workspace_committed_bytes(&ws), 0);
    }
}
