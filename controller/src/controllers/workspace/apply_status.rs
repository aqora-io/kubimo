use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::{Workspace, WorkspaceStatus, prelude::*};

use crate::context::Context;

use super::WorkspaceReconciler;

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
                update_workspace_status(workspace.clone(), None, StatusKind::JobComplete)
            };
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
    workspace.status = Some(WorkspaceStatus {
        conditions: Some(conditions),
    });
    workspace
}
