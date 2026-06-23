use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::kube::runtime::{Controller, controller::Action};
use kubimo::{
    Budget, BudgetResourceStatus, BudgetStatus, FilterParams, Selector, StorageQuantity,
    StorageUnit, Workspace, prelude::*,
};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::controllers::workspace::{BUDGET_EXCEEDED_REASON, pvc_storage_request};
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
        let used =
            sum_workspace_storage(ctx, namespace, &budget.spec.label_selector(), None).await?;
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

/// Committed storage (bytes) a Workspace contributes to its budget: its bound
/// PVC request if it has one, otherwise its configured `spec.storage.min` —
/// except a refused Workspace reserves nothing (it has no PVC and will not
/// provision until budget frees, so counting its minimum would inflate usage
/// and headroom for storage that is never allocated).
fn workspace_committed_bytes(workspace: &Workspace, pvc_request: Option<u64>) -> u64 {
    pvc_request
        .or_else(|| {
            if is_budget_refused(workspace) {
                return None;
            }
            workspace
                .spec
                .storage
                .as_ref()
                .and_then(|storage| storage.min.as_ref())
                .and_then(StorageQuantity::to_bytes)
        })
        .unwrap_or(0)
}

/// Whether a Workspace is currently refused provisioning because it does not fit
/// its budget — identified by the `Ready=False` condition with the
/// [`BUDGET_EXCEEDED_REASON`] reason written by the Workspace reconciler.
fn is_budget_refused(workspace: &Workspace) -> bool {
    workspace
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions.iter().any(|cond| {
                cond.type_ == "Ready"
                    && cond.reason == BUDGET_EXCEEDED_REASON
                    && cond.status == "False"
            })
        })
}

/// Sum the committed storage of every Workspace matching `selector` (see
/// [`workspace_committed_bytes`]). Optionally excludes one workspace by name
/// (used when computing headroom for that workspace).
pub(crate) async fn sum_workspace_storage(
    ctx: &Context,
    namespace: &str,
    selector: &Selector,
    exclude: Option<&str>,
) -> Result<u64, kubimo::Error> {
    let workspaces: Vec<Workspace> = ctx
        .api_namespaced::<Workspace>(namespace)
        .list(&FilterParams::new().with_labels(selector.clone()))
        .map_ok(|item| item.item)
        .try_collect()
        .await?;

    // PVCs are not labelled with the user label, but share the Workspace name.
    let pvc_requests: std::collections::BTreeMap<String, u64> = ctx
        .api_namespaced::<PersistentVolumeClaim>(namespace)
        .list(&FilterParams::new())
        .map_ok(|item| item.item)
        .try_filter_map(|pvc| async move {
            let bytes = pvc_storage_request(&pvc).and_then(|q| q.to_bytes());
            Ok(pvc.metadata.name.zip(bytes))
        })
        .try_collect()
        .await?;

    let mut total = 0u64;
    for workspace in &workspaces {
        let Ok(name) = workspace.name() else { continue };
        if Some(name) == exclude {
            continue;
        }
        let bytes = workspace_committed_bytes(workspace, pvc_requests.get(name).copied());
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// Maximum total storage (bytes) the given Workspace may hold without breaching
/// any Budget that governs it. `None` means unbounded (no matching budget).
pub(crate) async fn workspace_storage_allowance(
    ctx: &Context,
    workspace: &Workspace,
) -> Result<Option<u64>, kubimo::Error> {
    let namespace = workspace.require_namespace()?;
    let name = workspace.name()?;
    let labels = workspace.metadata.labels.as_ref();

    let budgets: Vec<Budget> = ctx
        .api_namespaced::<Budget>(namespace)
        .list(&FilterParams::new())
        .map_ok(|item| item.item)
        .try_collect()
        .await?;

    let mut allowance: Option<u64> = None;
    for budget in &budgets {
        if !budget.spec.matches(labels) {
            continue;
        }
        let Some(cap) = budget
            .spec
            .storage
            .as_ref()
            .and_then(StorageQuantity::to_bytes)
        else {
            continue;
        };
        let others =
            sum_workspace_storage(ctx, namespace, &budget.spec.label_selector(), Some(name))
                .await?;
        let headroom = cap.saturating_sub(others);
        allowance = Some(allowance.map_or(headroom, |current| current.min(headroom)));
    }
    Ok(allowance)
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
    use kubimo::{StorageRequirement, WorkspaceSpec, WorkspaceStatus};

    fn gib(n: u64) -> u64 {
        n * 1024 * 1024 * 1024
    }

    /// Build a Workspace with an optional `spec.storage.min` and an optional
    /// `Ready` condition (`status`, `reason`).
    fn workspace_with(min: Option<&str>, ready: Option<(&str, &str)>) -> Workspace {
        let spec = WorkspaceSpec {
            storage: min.map(|min| StorageRequirement {
                min: Some(min.parse().unwrap()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut workspace = Workspace::new("ws", spec);
        if let Some((status, reason)) = ready {
            workspace.status = Some(WorkspaceStatus {
                conditions: Some(vec![Condition {
                    type_: "Ready".to_string(),
                    status: status.to_string(),
                    reason: reason.to_string(),
                    message: String::new(),
                    observed_generation: None,
                    last_transition_time: Time(Timestamp::UNIX_EPOCH),
                }]),
                ..Default::default()
            });
        }
        workspace
    }

    #[test]
    fn refused_detected_from_ready_condition() {
        let ws = workspace_with(Some("5Gi"), Some(("False", BUDGET_EXCEEDED_REASON)));
        assert!(is_budget_refused(&ws));
    }

    #[test]
    fn not_refused_for_ready_other_reasons_or_no_status() {
        assert!(!is_budget_refused(&workspace_with(
            Some("5Gi"),
            Some(("True", "JobComplete"))
        )));
        assert!(!is_budget_refused(&workspace_with(
            Some("5Gi"),
            Some(("False", "JobNotComplete"))
        )));
        assert!(!is_budget_refused(&workspace_with(Some("5Gi"), None)));
    }

    #[test]
    fn committed_bytes_prefers_pvc_request() {
        // A bound PVC always counts, even if the workspace is somehow also refused.
        let ws = workspace_with(Some("5Gi"), Some(("False", BUDGET_EXCEEDED_REASON)));
        assert_eq!(workspace_committed_bytes(&ws, Some(gib(2))), gib(2));
    }

    #[test]
    fn committed_bytes_pending_reserves_min() {
        let ws = workspace_with(Some("3Gi"), None);
        assert_eq!(workspace_committed_bytes(&ws, None), gib(3));
    }

    #[test]
    fn committed_bytes_refused_reserves_nothing() {
        let ws = workspace_with(Some("3Gi"), Some(("False", BUDGET_EXCEEDED_REASON)));
        assert_eq!(workspace_committed_bytes(&ws, None), 0);
    }

    #[test]
    fn committed_bytes_no_min_is_zero() {
        let ws = workspace_with(None, None);
        assert_eq!(workspace_committed_bytes(&ws, None), 0);
    }
}
