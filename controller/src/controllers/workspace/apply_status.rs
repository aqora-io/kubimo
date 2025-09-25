use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
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
        let Some(job) = ctx.api_namespaced::<Job>(namespace).get_opt(name).await? else {
            return Ok(());
        };
        let job_conditions = job
            .status
            .and_then(|status| status.conditions)
            .unwrap_or_default();
        let job_complete = job_conditions.iter().find(|cond| cond.type_ == "Complete");
        let job_failed = job_conditions.iter().find(|cond| cond.type_ == "Failed");
        let last_transition_time = job_conditions
            .iter()
            .filter_map(|cond| cond.last_transition_time.as_ref())
            .max()
            .or(workspace.metadata.creation_timestamp.as_ref())
            .cloned()
            .unwrap_or_else(|| Time(chrono::Utc::now()));
        let observed_generation = workspace.metadata.generation;
        let ready = if job_failed.is_some_and(|cond| cond.status == "True") {
            Condition {
                last_transition_time,
                observed_generation,
                message: "Job failed".into(),
                reason: "JobFailed".into(),
                status: "False".into(),
                type_: "Ready".into(),
            }
        } else if job_complete.is_none_or(|cond| cond.status == "False") {
            Condition {
                last_transition_time,
                observed_generation,
                message: "Job not complete".into(),
                reason: "JobNotComplete".into(),
                status: "False".into(),
                type_: "Ready".into(),
            }
        } else {
            Condition {
                last_transition_time,
                observed_generation,
                message: "Job complete".into(),
                reason: "JobComplete".into(),
                status: "True".into(),
                type_: "Ready".into(),
            }
        };
        let mut workspace = workspace.clone();
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
        ctx.api_namespaced::<Workspace>(namespace)
            .patch_status(&workspace)
            .await?;
        Ok(())
    }
}
