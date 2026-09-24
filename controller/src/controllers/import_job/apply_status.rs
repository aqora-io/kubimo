use kubimo::conditions::{IMPORT_COMPLETE, IMPORT_FAILED};
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::{ImportJob, ImportJobStatus, prelude::*};

use crate::context::Context;

use super::ImportJobReconciler;

/// Whether the import has reached its one terminal condition.
pub(super) fn is_finished(import_job: &ImportJob) -> bool {
    import_job
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.status == "True"
                    && (condition.type_ == IMPORT_COMPLETE || condition.type_ == IMPORT_FAILED)
            })
        })
}

/// The Job's terminal condition, as the ImportJob reports it. `None` while
/// the Job is still pending or running — including the `SuccessCriteriaMet` /
/// `FailureTarget` conditions a Job sets just before it terminates.
pub(super) fn terminal_condition(job: &Job, generation: Option<i64>) -> Option<Condition> {
    let condition = job
        .status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|condition| {
            condition.status == "True"
                && (condition.type_ == "Complete" || condition.type_ == "Failed")
        })?;
    let type_ = if condition.type_ == "Complete" {
        IMPORT_COMPLETE
    } else {
        IMPORT_FAILED
    };
    Some(Condition {
        type_: type_.into(),
        status: "True".into(),
        // Older apiservers leave `Complete`'s reason empty, which a Condition
        // may not be.
        reason: condition
            .reason
            .clone()
            .filter(|reason| !reason.is_empty())
            .unwrap_or_else(|| condition.type_.clone()),
        message: condition.message.clone().unwrap_or_default(),
        // The Job's own time, so repeating the write changes nothing.
        last_transition_time: condition
            .last_transition_time
            .clone()
            .unwrap_or_else(|| Time(Timestamp::now())),
        observed_generation: generation,
    })
}

impl ImportJobReconciler {
    pub(super) async fn apply_status(
        &self,
        ctx: &Context,
        import_job: &ImportJob,
        job: &Job,
    ) -> Result<(), kubimo::Error> {
        let Some(condition) = terminal_condition(job, import_job.metadata.generation) else {
            return Ok(());
        };
        let namespace = import_job.require_namespace()?;
        let mut import_job = import_job.clone();
        import_job.status = Some(ImportJobStatus {
            conditions: Some(vec![condition]),
        });
        ctx.api_namespaced::<ImportJob>(namespace)
            .patch_status(&import_job)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

    fn time() -> Time {
        Time("2026-09-24T12:00:00Z".parse().unwrap())
    }

    fn job(conditions: &[(&str, &str, Option<&str>)]) -> Job {
        Job {
            status: Some(JobStatus {
                conditions: Some(
                    conditions
                        .iter()
                        .map(|(type_, status, reason)| JobCondition {
                            type_: type_.to_string(),
                            status: status.to_string(),
                            reason: reason.map(str::to_string),
                            message: Some(format!("{type_} message")),
                            last_transition_time: Some(time()),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_complete_job_completes_the_import() {
        let job = job(&[
            ("SuccessCriteriaMet", "True", Some("CompletionsReached")),
            ("Complete", "True", Some("CompletionsReached")),
        ]);
        let condition = terminal_condition(&job, Some(3)).unwrap();
        assert_eq!(condition.type_, IMPORT_COMPLETE);
        assert_eq!(condition.status, "True");
        assert_eq!(condition.reason, "CompletionsReached");
        assert_eq!(condition.message, "Complete message");
        assert_eq!(condition.last_transition_time, time());
        assert_eq!(condition.observed_generation, Some(3));
    }

    #[test]
    fn a_failed_job_fails_the_import() {
        let job = job(&[
            ("FailureTarget", "True", Some("DeadlineExceeded")),
            ("Failed", "True", Some("DeadlineExceeded")),
        ]);
        let condition = terminal_condition(&job, None).unwrap();
        assert_eq!(condition.type_, IMPORT_FAILED);
        assert_eq!(condition.reason, "DeadlineExceeded");
    }

    #[test]
    fn an_empty_reason_falls_back_to_the_type() {
        let condition = terminal_condition(&job(&[("Complete", "True", Some(""))]), None);
        assert_eq!(condition.unwrap().reason, "Complete");
        let condition = terminal_condition(&job(&[("Failed", "True", None)]), None);
        assert_eq!(condition.unwrap().reason, "Failed");
    }

    #[test]
    fn a_job_that_has_not_terminated_reports_nothing() {
        assert!(terminal_condition(&Job::default(), None).is_none());
        assert!(terminal_condition(&job(&[]), None).is_none());
        assert!(terminal_condition(&job(&[("Complete", "False", None)]), None).is_none());
        assert!(terminal_condition(&job(&[("FailureTarget", "True", Some("x"))]), None).is_none());
    }

    #[test]
    fn only_a_terminal_condition_finishes_the_import() {
        let mut import_job = ImportJob::new("x", Default::default());
        assert!(!is_finished(&import_job));
        let condition = terminal_condition(&job(&[("Failed", "True", None)]), None).unwrap();
        import_job.status = Some(ImportJobStatus {
            conditions: Some(vec![condition]),
        });
        assert!(is_finished(&import_job));
    }
}
