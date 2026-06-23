use kubimo::k8s_crd_snapshot_storage::VolumeSnapshot;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::{Workspace, WorkspaceStatus, prelude::*};

use crate::context::Context;

use super::WorkspaceReconciler;

/// `reason` of the `Ready=False` condition written when a Workspace is refused
/// provisioning because it does not fit its budget.
pub(crate) const BUDGET_EXCEEDED_REASON: &str = "BudgetExceeded";

impl WorkspaceReconciler {
    pub(crate) async fn apply_status(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        if workspace.metadata.deletion_timestamp.is_some() {
            return Ok(());
        }
        let namespace = workspace.require_namespace()?;
        let name = workspace.name()?;
        let workspace =
            if let Some(job) = ctx.api_namespaced::<Job>(namespace).get_opt(name).await? {
                update_workspace_status(
                    workspace.clone(),
                    job_last_transition_time(&job),
                    StatusKind::from_job(&job),
                )
            } else {
                let status = if workspace.spec.clone_workspace_name.is_some() {
                    let snap_is_ready = ctx
                        .api_namespaced::<VolumeSnapshot>(namespace)
                        .get_opt(name)
                        .await?
                        .and_then(|snap| snap.status)
                        .and_then(|st| st.ready_to_use)
                        .unwrap_or_default();
                    if snap_is_ready {
                        StatusKind::JobComplete
                    } else {
                        StatusKind::JobNotComplete
                    }
                } else {
                    // Not Complete unless its job was created
                    StatusKind::JobNotComplete
                };
                update_workspace_status(workspace.clone(), None, status)
            };
        ctx.api_namespaced::<Workspace>(namespace)
            .patch_status(&workspace)
            .await?;
        Ok(())
    }

    /// Mark the workspace not-Ready because provisioning was refused by a budget.
    pub(crate) async fn apply_budget_status(
        &self,
        ctx: &Context,
        workspace: &Workspace,
        reason: &str,
    ) -> Result<(), kubimo::Error> {
        if workspace.metadata.deletion_timestamp.is_some() {
            return Ok(());
        }
        let namespace = workspace.require_namespace()?;
        let workspace = update_workspace_status(
            workspace.clone(),
            None,
            StatusKind::BudgetExceeded(reason.to_owned()),
        );
        ctx.api_namespaced::<Workspace>(namespace)
            .patch_status(&workspace)
            .await?;
        Ok(())
    }
}

#[allow(clippy::enum_variant_names)] // redudant variants but useful if we add non-job related
enum StatusKind {
    JobFailed,
    JobNotComplete,
    JobComplete,
    BudgetExceeded(String),
}

impl StatusKind {
    fn from_job(job: &Job) -> Self {
        let job_conditions = job
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_deref())
            .unwrap_or_default();
        let job_complete = job_conditions.iter().find(|cond| cond.type_ == "Complete");
        let job_failed = job_conditions.iter().find(|cond| cond.type_ == "Failed");
        if job_failed.is_some_and(|cond| cond.status == "True") {
            Self::JobFailed
        } else if job_complete.is_none_or(|cond| cond.status == "False") {
            Self::JobNotComplete
        } else {
            Self::JobComplete
        }
    }
}

fn job_last_transition_time(job: &Job) -> Option<Time> {
    job.status
        .as_ref()
        .and_then(|status| status.conditions.as_deref())
        .unwrap_or_default()
        .iter()
        .filter_map(|cond| cond.last_transition_time.as_ref())
        .max()
        .cloned()
}

fn update_workspace_status(
    mut workspace: Workspace,
    last_transition_time: Option<Time>,
    kind: StatusKind,
) -> Workspace {
    let last_transition_time = last_transition_time
        .or(workspace.metadata.creation_timestamp.clone())
        .unwrap_or_else(|| Time(Timestamp::now()));
    let observed_generation = workspace.metadata.generation;
    let ready = match kind {
        StatusKind::JobFailed => Condition {
            last_transition_time,
            observed_generation,
            message: "Job failed".into(),
            reason: "JobFailed".into(),
            status: "False".into(),
            type_: "Ready".into(),
        },
        StatusKind::JobNotComplete => Condition {
            last_transition_time,
            observed_generation,
            message: "Job not complete".into(),
            reason: "JobNotComplete".into(),
            status: "False".into(),
            type_: "Ready".into(),
        },
        StatusKind::JobComplete => Condition {
            last_transition_time,
            observed_generation,
            message: "Job complete".into(),
            reason: "JobComplete".into(),
            status: "True".into(),
            type_: "Ready".into(),
        },
        StatusKind::BudgetExceeded(message) => Condition {
            last_transition_time,
            observed_generation,
            message,
            reason: BUDGET_EXCEEDED_REASON.into(),
            status: "False".into(),
            type_: "Ready".into(),
        },
    };
    let mut conditions = workspace
        .status
        .as_ref()
        .and_then(|status| status.conditions.clone())
        .unwrap_or_default();
    if let Some(current_ready) = conditions.iter_mut().find(|cond| cond.type_ == "Ready") {
        *current_ready = ready;
    } else {
        conditions.push(ready);
    }
    // Send only `conditions`. `storage` is owned by the indexer's field manager
    // ("kubimo-indexer") under server-side apply; copying it into this patch would
    // make "kubimo-controller" co-claim those fields and 409-conflict with the
    // indexer's writes. Omitting it (storage stays `None`, skipped on serialize)
    // leaves the indexer's value untouched on the server.
    workspace.status = Some(WorkspaceStatus {
        conditions: Some(conditions),
        ..Default::default()
    });
    workspace
}
