use kubimo::Workspace;
use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;

pub(super) const PVC_BOUND: &str = "PvcBound";
pub(super) const WORKSPACE_READY: &str = "WorkspaceReady";
pub(super) const POD_SCHEDULED: &str = "PodScheduled";
pub(super) const POD_READY: &str = "PodReady";
pub(super) const STARTUP_CONDITIONS: [&str; 4] =
    [PVC_BOUND, WORKSPACE_READY, POD_SCHEDULED, POD_READY];

fn condition(
    type_: &str,
    status: &str,
    reason: &str,
    message: String,
    observed_generation: Option<i64>,
) -> Condition {
    Condition {
        type_: type_.to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message,
        observed_generation,
        last_transition_time: Time(Timestamp::now()),
    }
}

pub(super) fn pvc_bound_condition(
    pvc_name: &str,
    pvc: Option<&PersistentVolumeClaim>,
    observed_generation: Option<i64>,
) -> Condition {
    let (status, reason, message) = match pvc {
        None => (
            "False",
            "NotFound",
            format!("PersistentVolumeClaim {pvc_name:?} not found"),
        ),
        Some(pvc) => match pvc.status.as_ref().and_then(|s| s.phase.as_deref()) {
            Some("Bound") => (
                "True",
                "Bound",
                "PersistentVolumeClaim is bound".to_string(),
            ),
            Some("Lost") => ("False", "Lost", "PersistentVolumeClaim is lost".to_string()),
            _ => (
                "False",
                "Pending",
                "PersistentVolumeClaim is pending".to_string(),
            ),
        },
    };
    condition(PVC_BOUND, status, reason, message, observed_generation)
}

pub(super) fn workspace_ready_condition(
    workspace_name: &str,
    workspace: Option<&Workspace>,
    observed_generation: Option<i64>,
) -> Condition {
    let ready = workspace.and_then(|workspace| {
        workspace
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|cond| cond.type_ == "Ready"))
    });
    match (workspace, ready) {
        (None, _) => condition(
            WORKSPACE_READY,
            "False",
            "NotFound",
            format!("Workspace {workspace_name:?} not found"),
            observed_generation,
        ),
        (Some(_), None) => condition(
            WORKSPACE_READY,
            "False",
            "Pending",
            "Workspace has no Ready condition yet".to_string(),
            observed_generation,
        ),
        (Some(_), Some(ready)) => condition(
            WORKSPACE_READY,
            &ready.status,
            &ready.reason,
            ready.message.clone(),
            observed_generation,
        ),
    }
}

pub(super) fn pod_scheduled_condition(
    pod: Option<&Pod>,
    observed_generation: Option<i64>,
) -> Condition {
    let scheduled = pod.and_then(|pod| {
        pod.status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|cond| cond.type_ == "PodScheduled"))
    });
    let (status, reason, message) = match (pod, scheduled) {
        (None, _) => (
            "False",
            "NotPresent".to_string(),
            "Pod not created yet".to_string(),
        ),
        (Some(_), None) => (
            "False",
            "Pending".to_string(),
            "Pod has not been scheduled yet".to_string(),
        ),
        (Some(_), Some(scheduled)) if scheduled.status == "True" => (
            "True",
            "Scheduled".to_string(),
            "Pod has been scheduled".to_string(),
        ),
        (Some(_), Some(scheduled)) => (
            "False",
            scheduled
                .reason
                .clone()
                .unwrap_or_else(|| "Pending".to_string()),
            scheduled
                .message
                .clone()
                .unwrap_or_else(|| "Pod has not been scheduled yet".to_string()),
        ),
    };
    condition(POD_SCHEDULED, status, &reason, message, observed_generation)
}

pub(super) fn pod_ready_condition(
    pod: Option<&Pod>,
    observed_generation: Option<i64>,
) -> Condition {
    let (status, reason, message) = match pod {
        None => ("False", "NotPresent".to_string(), "Not present".to_string()),
        Some(pod) => {
            let ready = pod
                .status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .and_then(|conditions| conditions.iter().find(|cond| cond.type_ == "Ready"));
            match ready {
                None => ("False", "NotStarted".to_string(), "Not started".to_string()),
                Some(ready) if ready.status == "True" => {
                    ("True", "Ready".to_string(), "Ready".to_string())
                }
                Some(_) => match container_state_detail(pod) {
                    Some((reason, message)) => ("False", reason, message),
                    None => ("False", "NotReady".to_string(), "Not ready".to_string()),
                },
            }
        }
    };
    condition(POD_READY, status, &reason, message, observed_generation)
}

/// Why the runner pod is not ready, derived from container state. Prefers the
/// "runner" container; falls back to the first non-ready container (sidecar).
fn container_state_detail(pod: &Pod) -> Option<(String, String)> {
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    let container = statuses
        .iter()
        .find(|container| container.name == "runner" && !container.ready)
        .or_else(|| statuses.iter().find(|container| !container.ready))?;
    let state = container.state.as_ref()?;
    if let Some(waiting) = state.waiting.as_ref() {
        let reason = waiting.reason.clone()?;
        let message = waiting.message.clone().unwrap_or_else(|| reason.clone());
        return Some((reason, message));
    }
    if let Some(terminated) = state.terminated.as_ref() {
        let reason = terminated
            .reason
            .clone()
            .unwrap_or_else(|| "Terminated".to_string());
        let message = format!(
            "Container terminated with exit code {exit_code}",
            exit_code = terminated.exit_code
        );
        return Some((reason, message));
    }
    if state.running.is_some() {
        return Some((
            "Starting".to_string(),
            "Container running, waiting for marimo health check".to_string(),
        ));
    }
    None
}

/// Upserts by `type_`. Bumps `last_transition_time` only when `status`
/// changes; a reason change updates reason/message/observed_generation but
/// keeps the timestamp; message-only changes are ignored to avoid status
/// churn from fluctuating messages (e.g. back-off countdowns).
pub(super) fn upsert_condition(conditions: &mut Vec<Condition>, new: Condition) {
    let Some(current) = conditions.iter_mut().find(|cond| cond.type_ == new.type_) else {
        conditions.push(new);
        return;
    };
    if current.status != new.status {
        *current = new;
    } else if current.reason != new.reason {
        current.reason = new.reason;
        current.message = new.message;
        current.observed_generation = new.observed_generation;
    }
}

pub(super) fn startup_complete(conditions: &[Condition]) -> bool {
    STARTUP_CONDITIONS.iter().all(|type_| {
        conditions
            .iter()
            .any(|cond| cond.type_ == *type_ && cond.status == "True")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::WorkspaceStatus;
    use kubimo::k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
        ContainerStatus, PersistentVolumeClaimStatus, PodCondition, PodStatus,
    };
    use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kubimo::k8s_openapi::jiff::Timestamp;

    fn pvc_with_phase(phase: Option<&str>) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            status: phase.map(|phase| PersistentVolumeClaimStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn workspace_with_ready(status: &str, reason: &str, message: &str) -> Workspace {
        let mut workspace = Workspace::new("test", Default::default());
        workspace.status = Some(WorkspaceStatus {
            conditions: Some(vec![Condition {
                type_: "Ready".to_string(),
                status: status.to_string(),
                reason: reason.to_string(),
                message: message.to_string(),
                observed_generation: None,
                last_transition_time: Time(Timestamp::UNIX_EPOCH),
            }]),
        });
        workspace
    }

    fn pod_with_status(status: PodStatus) -> Pod {
        Pod {
            status: Some(status),
            ..Default::default()
        }
    }

    fn pod_condition(type_: &str, status: &str) -> PodCondition {
        PodCondition {
            type_: type_.to_string(),
            status: status.to_string(),
            ..Default::default()
        }
    }

    fn container_status(name: &str, ready: bool, state: Option<ContainerState>) -> ContainerStatus {
        ContainerStatus {
            name: name.to_string(),
            ready,
            state,
            ..Default::default()
        }
    }

    fn waiting(reason: &str, message: Option<&str>) -> ContainerState {
        ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.to_string()),
                message: message.map(ToString::to_string),
            }),
            ..Default::default()
        }
    }

    fn assert_condition(condition: &Condition, type_: &str, status: &str, reason: &str) {
        assert_eq!(condition.type_, type_);
        assert_eq!(condition.status, status);
        assert_eq!(condition.reason, reason);
    }

    #[test]
    fn pvc_bound_missing_pvc_is_not_found() {
        let condition = pvc_bound_condition("ws", None, Some(1));
        assert_condition(&condition, PVC_BOUND, "False", "NotFound");
        assert!(condition.message.contains("ws"));
        assert_eq!(condition.observed_generation, Some(1));
    }

    #[test]
    fn pvc_bound_no_status_is_pending() {
        let pvc = pvc_with_phase(None);
        let condition = pvc_bound_condition("ws", Some(&pvc), None);
        assert_condition(&condition, PVC_BOUND, "False", "Pending");
    }

    #[test]
    fn pvc_bound_pending_phase_is_pending() {
        let pvc = pvc_with_phase(Some("Pending"));
        let condition = pvc_bound_condition("ws", Some(&pvc), None);
        assert_condition(&condition, PVC_BOUND, "False", "Pending");
    }

    #[test]
    fn pvc_bound_bound_phase_is_true() {
        let pvc = pvc_with_phase(Some("Bound"));
        let condition = pvc_bound_condition("ws", Some(&pvc), None);
        assert_condition(&condition, PVC_BOUND, "True", "Bound");
    }

    #[test]
    fn pvc_bound_lost_phase_is_lost() {
        let pvc = pvc_with_phase(Some("Lost"));
        let condition = pvc_bound_condition("ws", Some(&pvc), None);
        assert_condition(&condition, PVC_BOUND, "False", "Lost");
    }

    #[test]
    fn workspace_ready_missing_workspace_is_not_found() {
        let condition = workspace_ready_condition("ws", None, None);
        assert_condition(&condition, WORKSPACE_READY, "False", "NotFound");
        assert!(condition.message.contains("ws"));
    }

    #[test]
    fn workspace_ready_no_status_is_pending() {
        let workspace = Workspace::new("test", Default::default());
        let condition = workspace_ready_condition("ws", Some(&workspace), None);
        assert_condition(&condition, WORKSPACE_READY, "False", "Pending");
    }

    #[test]
    fn workspace_ready_mirrors_ready_condition() {
        let workspace = workspace_with_ready("True", "JobComplete", "Job complete");
        let condition = workspace_ready_condition("ws", Some(&workspace), None);
        assert_condition(&condition, WORKSPACE_READY, "True", "JobComplete");
        assert_eq!(condition.message, "Job complete");
    }

    #[test]
    fn workspace_ready_mirrors_failed_job() {
        let workspace = workspace_with_ready("False", "JobFailed", "Job failed");
        let condition = workspace_ready_condition("ws", Some(&workspace), None);
        assert_condition(&condition, WORKSPACE_READY, "False", "JobFailed");
        assert_eq!(condition.message, "Job failed");
    }

    #[test]
    fn pod_scheduled_missing_pod_is_not_present() {
        let condition = pod_scheduled_condition(None, None);
        assert_condition(&condition, POD_SCHEDULED, "False", "NotPresent");
    }

    #[test]
    fn pod_scheduled_no_conditions_is_pending() {
        let pod = pod_with_status(PodStatus::default());
        let condition = pod_scheduled_condition(Some(&pod), None);
        assert_condition(&condition, POD_SCHEDULED, "False", "Pending");
    }

    #[test]
    fn pod_scheduled_true_is_scheduled() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("PodScheduled", "True")]),
            ..Default::default()
        });
        let condition = pod_scheduled_condition(Some(&pod), None);
        assert_condition(&condition, POD_SCHEDULED, "True", "Scheduled");
    }

    #[test]
    fn pod_scheduled_unschedulable_passes_through_reason_and_message() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![PodCondition {
                reason: Some("Unschedulable".to_string()),
                message: Some("0/3 nodes are available".to_string()),
                ..pod_condition("PodScheduled", "False")
            }]),
            ..Default::default()
        });
        let condition = pod_scheduled_condition(Some(&pod), None);
        assert_condition(&condition, POD_SCHEDULED, "False", "Unschedulable");
        assert_eq!(condition.message, "0/3 nodes are available");
    }

    #[test]
    fn pod_ready_missing_pod_is_not_present() {
        let condition = pod_ready_condition(None, None);
        assert_condition(&condition, POD_READY, "False", "NotPresent");
    }

    #[test]
    fn pod_ready_no_ready_condition_is_not_started() {
        let pod = pod_with_status(PodStatus::default());
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "NotStarted");
    }

    #[test]
    fn pod_ready_true_is_ready() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "True")]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "True", "Ready");
    }

    #[test]
    fn pod_ready_no_container_statuses_is_not_ready() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "False")]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "NotReady");
    }

    #[test]
    fn pod_ready_waiting_container_surfaces_reason() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "False")]),
            container_statuses: Some(vec![container_status(
                "runner",
                false,
                Some(waiting("ContainerCreating", None)),
            )]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "ContainerCreating");
    }

    #[test]
    fn pod_ready_waiting_container_surfaces_message() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "False")]),
            container_statuses: Some(vec![container_status(
                "runner",
                false,
                Some(waiting("ImagePullBackOff", Some("Back-off pulling image"))),
            )]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "ImagePullBackOff");
        assert_eq!(condition.message, "Back-off pulling image");
    }

    #[test]
    fn pod_ready_running_unready_container_is_starting() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "False")]),
            container_statuses: Some(vec![container_status(
                "runner",
                false,
                Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
            )]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "Starting");
    }

    #[test]
    fn pod_ready_terminated_container_includes_exit_code() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "False")]),
            container_statuses: Some(vec![container_status(
                "runner",
                false,
                Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 1,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            )]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "Terminated");
        assert!(condition.message.contains('1'));
    }

    #[test]
    fn pod_ready_falls_back_to_unready_sidecar() {
        let pod = pod_with_status(PodStatus {
            conditions: Some(vec![pod_condition("Ready", "False")]),
            container_statuses: Some(vec![
                container_status("runner", true, None),
                container_status("sidecar", false, Some(waiting("CrashLoopBackOff", None))),
            ]),
            ..Default::default()
        });
        let condition = pod_ready_condition(Some(&pod), None);
        assert_condition(&condition, POD_READY, "False", "CrashLoopBackOff");
    }

    fn existing(status: &str, reason: &str, message: &str) -> Condition {
        Condition {
            type_: POD_READY.to_string(),
            status: status.to_string(),
            reason: reason.to_string(),
            message: message.to_string(),
            observed_generation: Some(1),
            last_transition_time: Time(Timestamp::UNIX_EPOCH),
        }
    }

    fn incoming(status: &str, reason: &str, message: &str) -> Condition {
        Condition {
            observed_generation: Some(2),
            last_transition_time: Time(Timestamp::now()),
            ..existing(status, reason, message)
        }
    }

    #[test]
    fn upsert_inserts_when_absent() {
        let mut conditions = vec![];
        upsert_condition(&mut conditions, incoming("False", "Pending", "Pending"));
        assert_eq!(conditions.len(), 1);
        assert_condition(&conditions[0], POD_READY, "False", "Pending");
    }

    #[test]
    fn upsert_status_change_bumps_transition_time() {
        let mut conditions = vec![existing("False", "Pending", "Pending")];
        upsert_condition(&mut conditions, incoming("True", "Ready", "Ready"));
        assert_condition(&conditions[0], POD_READY, "True", "Ready");
        assert_ne!(
            conditions[0].last_transition_time,
            Time(Timestamp::UNIX_EPOCH)
        );
        assert_eq!(conditions[0].observed_generation, Some(2));
    }

    #[test]
    fn upsert_reason_change_keeps_transition_time() {
        let mut conditions = vec![existing("False", "ContainerCreating", "Creating")];
        upsert_condition(&mut conditions, incoming("False", "Starting", "Starting"));
        assert_condition(&conditions[0], POD_READY, "False", "Starting");
        assert_eq!(conditions[0].message, "Starting");
        assert_eq!(conditions[0].observed_generation, Some(2));
        assert_eq!(
            conditions[0].last_transition_time,
            Time(Timestamp::UNIX_EPOCH)
        );
    }

    #[test]
    fn upsert_ignores_message_only_change() {
        let mut conditions = vec![existing("False", "CrashLoopBackOff", "back-off 10s")];
        upsert_condition(
            &mut conditions,
            incoming("False", "CrashLoopBackOff", "back-off 20s"),
        );
        assert_eq!(conditions[0].message, "back-off 10s");
        assert_eq!(conditions[0].observed_generation, Some(1));
        assert_eq!(
            conditions[0].last_transition_time,
            Time(Timestamp::UNIX_EPOCH)
        );
    }

    fn true_condition(type_: &str) -> Condition {
        Condition {
            type_: type_.to_string(),
            status: "True".to_string(),
            reason: "Ready".to_string(),
            message: "Ready".to_string(),
            observed_generation: None,
            last_transition_time: Time(Timestamp::UNIX_EPOCH),
        }
    }

    #[test]
    fn startup_complete_when_all_true() {
        let conditions: Vec<_> = STARTUP_CONDITIONS
            .iter()
            .map(|t| true_condition(t))
            .collect();
        assert!(startup_complete(&conditions));
    }

    #[test]
    fn startup_not_complete_when_one_false() {
        let mut conditions: Vec<_> = STARTUP_CONDITIONS
            .iter()
            .map(|t| true_condition(t))
            .collect();
        conditions[0].status = "False".to_string();
        assert!(!startup_complete(&conditions));
    }

    #[test]
    fn startup_not_complete_when_one_missing() {
        let conditions: Vec<_> = STARTUP_CONDITIONS[1..]
            .iter()
            .map(|t| true_condition(t))
            .collect();
        assert!(!startup_complete(&conditions));
    }

    #[test]
    fn startup_complete_ignores_extra_conditions() {
        let mut conditions: Vec<_> = STARTUP_CONDITIONS
            .iter()
            .map(|t| true_condition(t))
            .collect();
        let mut extra = true_condition("SomethingElse");
        extra.status = "False".to_string();
        conditions.push(extra);
        assert!(startup_complete(&conditions));
    }
}
