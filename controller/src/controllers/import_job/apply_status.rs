use futures::TryStreamExt;
use kubimo::conditions::{
    IMPORT_COMPLETE, IMPORT_FAILED, IMPORT_FETCH_FAILED, IMPORT_WRITE_FAILED,
};
use kubimo::k8s_openapi::api::batch::v1::{Job, JobCondition};
use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::{FilterParams, ImportJob, ImportJobStatus, prelude::*};

use crate::context::Context;

use super::ImportJobReconciler;
use super::apply_job::IMPORT_CONTAINER;

/// How much of a failure's message the condition carries: a termination
/// message can run to 4 KiB, and a condition is read by people.
const MAX_MESSAGE_LEN: usize = 1024;

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

/// The Job's `Complete` or `Failed` condition. `None` while the Job is still
/// pending or running — including the `SuccessCriteriaMet` / `FailureTarget`
/// conditions a Job sets just before it terminates.
fn job_outcome(job: &Job) -> Option<&JobCondition> {
    job.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|condition| {
            condition.status == "True"
                && (condition.type_ == "Complete" || condition.type_ == "Failed")
        })
}

fn job_failed(job: &Job) -> bool {
    job_outcome(job).is_some_and(|condition| condition.type_ == "Failed")
}

/// Why the Job's pod failed: the termination message of its first container
/// to fail with one — `s3-get` and the importer each write one — or else the
/// pod's own status, as when it was evicted.
pub(super) fn failure_cause(pod: &Pod) -> Option<(String, String)> {
    let status = pod.status.as_ref()?;
    let containers = status.init_container_statuses.iter().flatten();
    for container in containers.chain(status.container_statuses.iter().flatten()) {
        let Some(terminated) = container
            .state
            .as_ref()
            .and_then(|state| state.terminated.as_ref())
        else {
            continue;
        };
        let message = terminated.message.as_deref().unwrap_or_default().trim();
        if terminated.exit_code == 0 || message.is_empty() {
            continue;
        }
        // Every init container is a fetch.
        let reason = if container.name == IMPORT_CONTAINER {
            IMPORT_WRITE_FAILED
        } else {
            IMPORT_FETCH_FAILED
        };
        return Some((reason.to_string(), message.to_string()));
    }
    let message = status
        .message
        .as_deref()
        .filter(|message| !message.is_empty())?;
    let reason = status
        .reason
        .as_deref()
        .filter(|reason| !reason.is_empty())?;
    Some((reason.to_string(), message.to_string()))
}

/// The Job's terminal condition, as the ImportJob reports it: a failure with
/// its cause from `pod` when there is one, else from the Job. `None` while
/// the Job is still pending or running.
pub(super) fn terminal_condition(
    job: &Job,
    pod: Option<&Pod>,
    generation: Option<i64>,
) -> Option<Condition> {
    let condition = job_outcome(job)?;
    // Older apiservers leave `Complete`'s reason empty, which a Condition may
    // not be.
    let job_cause = || {
        (
            condition
                .reason
                .clone()
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| condition.type_.clone()),
            condition.message.clone().unwrap_or_default(),
        )
    };
    let (type_, (reason, message)) = if condition.type_ == "Complete" {
        (IMPORT_COMPLETE, job_cause())
    } else {
        (
            IMPORT_FAILED,
            pod.and_then(failure_cause).unwrap_or_else(job_cause),
        )
    };
    Some(Condition {
        type_: type_.into(),
        status: "True".into(),
        reason,
        message: truncated(message),
        // The Job's own time, so repeating the write changes nothing.
        last_transition_time: condition
            .last_transition_time
            .clone()
            .unwrap_or_else(|| Time(Timestamp::now())),
        observed_generation: generation,
    })
}

/// A failure that never got as far as a Job.
pub(super) fn failed(reason: &str, message: String, generation: Option<i64>) -> Condition {
    Condition {
        type_: IMPORT_FAILED.into(),
        status: "True".into(),
        reason: reason.into(),
        message: truncated(message),
        last_transition_time: Time(Timestamp::now()),
        observed_generation: generation,
    }
}

fn truncated(mut message: String) -> String {
    if message.len() > MAX_MESSAGE_LEN {
        let mut end = MAX_MESSAGE_LEN;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    message
}

impl ImportJobReconciler {
    pub(super) async fn apply_status(
        &self,
        ctx: &Context,
        import_job: &ImportJob,
        job: &Job,
    ) -> Result<(), kubimo::Error> {
        // Only a failure needs the pod, for its cause. It is gone if the Job
        // ran out of time, and then the Job's own reason says so.
        let pod = if job_failed(job) {
            self.job_pod(ctx, job).await?
        } else {
            None
        };
        let Some(condition) = terminal_condition(job, pod.as_ref(), import_job.metadata.generation)
        else {
            return Ok(());
        };
        self.patch_condition(ctx, import_job, condition).await
    }

    pub(super) async fn patch_condition(
        &self,
        ctx: &Context,
        import_job: &ImportJob,
        condition: Condition,
    ) -> Result<(), kubimo::Error> {
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

    /// The Job's one pod (it runs once, with no retries).
    async fn job_pod(&self, ctx: &Context, job: &Job) -> Result<Option<Pod>, kubimo::Error> {
        let (Some(namespace), Some(uid)) = (
            job.metadata.namespace.as_deref(),
            job.metadata.uid.as_deref(),
        ) else {
            return Ok(None);
        };
        let params = FilterParams::new().with_labels(("batch.kubernetes.io/controller-uid", uid));
        let pods: Vec<Pod> = ctx
            .api_namespaced::<Pod>(namespace)
            .list(&params)
            .map_ok(|item| item.item)
            .try_collect()
            .await?;
        Ok(pods.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
    use kubimo::k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
    };

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
        let condition = terminal_condition(&job, None, Some(3)).unwrap();
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
        let condition = terminal_condition(&job, None, None).unwrap();
        assert_eq!(condition.type_, IMPORT_FAILED);
        assert_eq!(condition.reason, "DeadlineExceeded");
    }

    #[test]
    fn an_empty_reason_falls_back_to_the_type() {
        let condition = terminal_condition(&job(&[("Complete", "True", Some(""))]), None, None);
        assert_eq!(condition.unwrap().reason, "Complete");
        let condition = terminal_condition(&job(&[("Failed", "True", None)]), None, None);
        assert_eq!(condition.unwrap().reason, "Failed");
    }

    #[test]
    fn a_job_that_has_not_terminated_reports_nothing() {
        assert!(terminal_condition(&Job::default(), None, None).is_none());
        assert!(terminal_condition(&job(&[]), None, None).is_none());
        assert!(terminal_condition(&job(&[("Complete", "False", None)]), None, None).is_none());
        assert!(
            terminal_condition(&job(&[("FailureTarget", "True", Some("x"))]), None, None).is_none()
        );
    }

    #[test]
    fn only_a_terminal_condition_finishes_the_import() {
        let mut import_job = ImportJob::new("x", Default::default());
        assert!(!is_finished(&import_job));
        let condition = terminal_condition(&job(&[("Failed", "True", None)]), None, None).unwrap();
        import_job.status = Some(ImportJobStatus {
            conditions: Some(vec![condition]),
        });
        assert!(is_finished(&import_job));
    }

    fn terminated(name: &str, exit_code: i32, message: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            name: name.to_string(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code,
                    message: message.map(str::to_string),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pod(init: Vec<ContainerStatus>, containers: Vec<ContainerStatus>) -> Pod {
        Pod {
            status: Some(PodStatus {
                init_container_statuses: Some(init),
                container_statuses: Some(containers),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_failed_fetch_is_a_fetch_failure() {
        let pod = pod(
            vec![
                terminated("fetch-0", 0, None),
                terminated(
                    "fetch-1",
                    1,
                    Some("fetching s3://imports/a.csv: not found\n"),
                ),
            ],
            vec![],
        );
        let job = job(&[("Failed", "True", Some("BackoffLimitExceeded"))]);
        let condition = terminal_condition(&job, Some(&pod), None).unwrap();
        assert_eq!(condition.type_, IMPORT_FAILED);
        assert_eq!(condition.reason, IMPORT_FETCH_FAILED);
        assert_eq!(condition.message, "fetching s3://imports/a.csv: not found");
    }

    #[test]
    fn a_failed_import_is_an_import_failure() {
        let pod = pod(
            vec![terminated("fetch-0", 0, None)],
            vec![terminated(
                IMPORT_CONTAINER,
                1,
                Some("nb/a.py: cannot convert: JSONDecodeError: Expecting value"),
            )],
        );
        let cause = failure_cause(&pod).unwrap();
        assert_eq!(cause.0, IMPORT_WRITE_FAILED);
        assert_eq!(
            cause.1,
            "nb/a.py: cannot convert: JSONDecodeError: Expecting value"
        );
    }

    /// An evicted pod's containers say nothing; the pod does.
    #[test]
    fn an_evicted_pod_explains_itself() {
        let mut pod = pod(vec![terminated("fetch-0", 137, None)], vec![]);
        let status = pod.status.as_mut().unwrap();
        status.reason = Some("Evicted".to_string());
        status.message = Some("ephemeral local storage usage exceeds 1Gi".to_string());
        assert_eq!(
            failure_cause(&pod),
            Some((
                "Evicted".to_string(),
                "ephemeral local storage usage exceeds 1Gi".to_string()
            ))
        );
    }

    /// No pod (it ran out of time and was deleted) or a pod with nothing to
    /// say: the Job's reason stands.
    #[test]
    fn without_a_cause_the_jobs_reason_stands() {
        let job = job(&[("Failed", "True", Some("DeadlineExceeded"))]);
        let silent = pod(vec![terminated("fetch-0", 1, Some("  "))], vec![]);
        for pod in [None, Some(&silent)] {
            let condition = terminal_condition(&job, pod, None).unwrap();
            assert_eq!(condition.reason, "DeadlineExceeded");
            assert_eq!(condition.message, "Failed message");
        }
    }

    /// A pod's cause never turns a completed import into a failure.
    #[test]
    fn a_complete_job_ignores_the_pod() {
        let pod = pod(vec![], vec![terminated(IMPORT_CONTAINER, 1, Some("x"))]);
        let job = job(&[("Complete", "True", Some("CompletionsReached"))]);
        let condition = terminal_condition(&job, Some(&pod), None).unwrap();
        assert_eq!(condition.type_, IMPORT_COMPLETE);
        assert_eq!(condition.reason, "CompletionsReached");
    }

    #[test]
    fn a_long_message_is_truncated_on_a_char_boundary() {
        let condition = failed("JobRejected", "é".repeat(MAX_MESSAGE_LEN), None);
        assert!(condition.message.len() <= MAX_MESSAGE_LEN + '…'.len_utf8());
        assert!(condition.message.ends_with('…'));
        assert_eq!(condition.type_, IMPORT_FAILED);
        assert_eq!(condition.reason, "JobRejected");
    }
}
