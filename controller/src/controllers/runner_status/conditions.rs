use std::time::Duration;

use kubimo::Workspace;
use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;

// Re-exported from the api crate rather than defined here. These strings are
// the contract consumers match on, and they treat a *missing* condition as
// unsatisfied — so a rename that only touched the controller would pin every
// runner at the previous phase, with no error raised anywhere. Sharing the
// definition means a consumer can assert against it.
pub(super) use kubimo::conditions::{
    POD_READY, POD_SCHEDULED, PVC_BOUND, STARTUP_CONDITIONS, WORKSPACE_READY,
};

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

/// The `Pooled`-mode counterpart of [`pvc_bound_condition`]: reports whether
/// the workspace's slot on the node data volume is assigned and mountable.
///
/// Deliberately keeps the `PvcBound` condition *type*. The platform matches on
/// that exact string and treats a **missing** condition as unsatisfied, so
/// emitting a differently-named condition here would leave every runner stuck
/// at "Binding volume…" (20%) in the UI forever, with no error and no log.
/// Only the meaning changes: a slot, not a PVC.
pub(super) fn slot_bound_condition(
    pod: Option<&Pod>,
    observed_generation: Option<i64>,
) -> Condition {
    // Derived from the pod rather than from `Workspace.status.slot`, because
    // the slot is created *by* the CSI driver during NodePublishVolume — that
    // is, partway through pod startup. Nothing writes `status.slot` before the
    // pod exists, so keying off it would leave this condition False forever,
    // which the platform renders as "Binding volume…" at 20% with no error
    // anywhere.
    //
    // A container cannot start until its volumes are mounted, so "some
    // container started" is a sound proxy for "the slot is bound".
    let (status, reason, message) = match pod {
        None => (
            "False",
            "Pending".to_string(),
            "Waiting for the runner pod to be created".to_string(),
        ),
        Some(pod) => {
            let statuses = pod
                .status
                .as_ref()
                .and_then(|status| status.container_statuses.as_deref())
                .unwrap_or_default();
            // "Has a container *ever* started", not "is one running now".
            //
            // A crashlooping runner is currently Waiting, but it only got to
            // crash because its volumes mounted. Reporting False here would
            // render as "Binding volume…" at 20% in the platform and hide an
            // application error behind a storage message. `restartCount` and a
            // previous termination are the evidence that the mount succeeded.
            let any_started = statuses.iter().any(|container| {
                container.started.unwrap_or(false)
                    || container.restart_count > 0
                    || container
                        .last_state
                        .as_ref()
                        .is_some_and(|state| state.terminated.is_some())
                    || container
                        .state
                        .as_ref()
                        .is_some_and(|state| state.running.is_some() || state.terminated.is_some())
            });
            if any_started {
                (
                    "True",
                    "Bound".to_string(),
                    "Workspace slot is mounted".to_string(),
                )
            } else {
                // Surface why, so a slot that cannot be mounted (no quota
                // support on the node, data volume full) is diagnosable instead
                // of just being slow.
                match container_state_detail(pod) {
                    Some((reason, message)) => ("False", reason, message),
                    None => (
                        "False",
                        "Pending".to_string(),
                        "Waiting for the workspace slot to be mounted".to_string(),
                    ),
                }
            }
        }
    };
    condition(PVC_BOUND, status, &reason, message, observed_generation)
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

/// Whether the runner's pod is passing its readiness probe, as of the
/// conditions computed earlier in this same reconcile.
///
/// This is a second, independent observer of the runner's health: kubelet's
/// probe travels over the pod network and is reported through the apiserver,
/// not over the ingress path the status poll uses. The two can only agree by
/// accident, which is what makes this worth consulting when the poll fails.
pub(super) fn pod_is_ready(conditions: &[Condition]) -> bool {
    conditions
        .iter()
        .any(|cond| cond.type_ == POD_READY && cond.status == "True")
}

/// How long `PodReady` has held its current status, in seconds.
///
/// [`upsert_condition`] bumps `last_transition_time` only when `status`
/// changes — a reason or message change leaves it alone — so this is exactly
/// "how long has the pod been (un)ready", stable across a crashloop's
/// fluctuating back-off messages.
pub(super) fn pod_ready_age_secs(conditions: &[Condition], now_secs: i64) -> Option<i64> {
    conditions
        .iter()
        .find(|cond| cond.type_ == POD_READY)
        .map(|cond| now_secs - cond.last_transition_time.0.as_second())
}

/// Requeue interval while the startup conditions are not all True.
///
/// Startup is genuinely slow, so the first window polls fast. But a pod that
/// never becomes ready — a crashloop, or a `Render` runner that is never even
/// polled — stays in this branch forever, and at 3s that is ~28,800 reconciles
/// a day each, every one of them re-GETting the pod, PVC and Workspace. Decay
/// once it is clear this is not a startup any more.
pub(super) fn startup_requeue_interval(
    conditions: &[Condition],
    now_secs: i64,
    poll_interval: Duration,
) -> Duration {
    const STARTING_WINDOW_SECS: i64 = 2 * 60;
    const SETTLED_WINDOW_SECS: i64 = 10 * 60;
    const WEDGED_REQUEUE: Duration = Duration::from_secs(60);

    match pod_ready_age_secs(conditions, now_secs) {
        // No PodReady condition yet: this reconcile is the first, so treat it
        // as a fresh startup.
        None => super::STARTUP_REQUEUE_INTERVAL,
        Some(age) if age < STARTING_WINDOW_SECS => super::STARTUP_REQUEUE_INTERVAL,
        Some(age) if age < SETTLED_WINDOW_SECS => poll_interval,
        Some(_) => WEDGED_REQUEUE,
    }
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

    fn pod_with_container(state: ContainerState, started: Option<bool>) -> Pod {
        Pod {
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "runner".to_string(),
                    state: Some(state),
                    started,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A running container proves its volumes mounted — kubelet will not start
    /// one otherwise.
    #[test]
    fn slot_bound_true_once_a_container_has_started() {
        let pod = pod_with_container(
            ContainerState {
                running: Some(ContainerStateRunning::default()),
                ..Default::default()
            },
            Some(true),
        );
        let cond = slot_bound_condition(Some(&pod), None);
        assert_eq!(cond.status, "True");
        assert_eq!(cond.reason, "Bound");
    }

    /// The whole point of this condition: it must keep the `PvcBound` type
    /// string, because the platform treats a missing condition as unsatisfied
    /// and would pin the runner at "Binding volume…" (20%) forever.
    #[test]
    fn slot_bound_keeps_the_pvc_bound_condition_type() {
        assert_eq!(slot_bound_condition(None, None).type_, PVC_BOUND);
        let pod = pod_with_container(ContainerState::default(), Some(true));
        assert_eq!(slot_bound_condition(Some(&pod), None).type_, PVC_BOUND);
    }

    #[test]
    fn slot_bound_pending_before_the_pod_exists() {
        let cond = slot_bound_condition(None, None);
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "Pending");
    }

    /// A container still waiting on its mount must not read as bound.
    #[test]
    fn slot_bound_false_while_the_container_is_waiting() {
        let pod = pod_with_container(
            ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("ContainerCreating".to_string()),
                    message: None,
                }),
                ..Default::default()
            },
            Some(false),
        );
        let cond = slot_bound_condition(Some(&pod), None);
        assert_eq!(cond.status, "False");
    }

    /// A crashlooping runner has already proved its volume mounted. Reporting
    /// the slot as unbound would show "Binding volume…" at 20% and hide the
    /// real application error. Observed on minikube with an unhydrated slot:
    /// `uv sync` exits 2, restartCount climbs, state is Waiting.
    #[test]
    fn slot_stays_bound_once_a_container_has_crashlooped() {
        let mut pod = pod_with_container(
            ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".to_string()),
                    message: Some("back-off 40s restarting failed container".to_string()),
                }),
                ..Default::default()
            },
            Some(false),
        );
        let status = &mut pod
            .status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()[0];
        status.restart_count = 3;
        status.last_state = Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 2,
                ..Default::default()
            }),
            ..Default::default()
        });
        let cond = slot_bound_condition(Some(&pod), None);
        assert_eq!(cond.status, "True", "a crashloop is not a storage failure");
        assert_eq!(cond.reason, "Bound");
    }

    /// A slot that cannot be mounted — no quota support, data volume full —
    /// must surface *why*, not just look slow.
    #[test]
    fn slot_bound_surfaces_the_mount_failure_reason() {
        let pod = pod_with_container(
            ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CreateContainerError".to_string()),
                    message: Some("failed to publish volume: no project quota".to_string()),
                }),
                ..Default::default()
            },
            Some(false),
        );
        let cond = slot_bound_condition(Some(&pod), None);
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "CreateContainerError");
        assert!(cond.message.contains("project quota"));
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
            ..Default::default()
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

    /// `PodReady` at a chosen age, which is what both the GC guard and the
    /// requeue decay key on.
    fn pod_ready_at(status: &str, transitioned_secs: i64) -> Vec<Condition> {
        vec![Condition {
            type_: POD_READY.to_string(),
            status: status.to_string(),
            reason: "Test".to_string(),
            message: String::new(),
            last_transition_time: Time(Timestamp::from_second(transitioned_secs).unwrap()),
            observed_generation: None,
        }]
    }

    #[test]
    fn pod_is_ready_reads_the_condition_just_upserted() {
        assert!(pod_is_ready(&pod_ready_at("True", 0)));
        assert!(!pod_is_ready(&pod_ready_at("False", 0)));
        // A missing condition is not readiness.
        assert!(!pod_is_ready(&[]));
    }

    #[test]
    fn pod_ready_age_is_measured_from_the_transition() {
        let conditions = pod_ready_at("False", 1_000);
        assert_eq!(pod_ready_age_secs(&conditions, 1_600), Some(600));
        assert_eq!(pod_ready_age_secs(&[], 1_600), None);
    }

    /// A pod that is genuinely starting must keep the fast poll — that is the
    /// whole reason the 3s interval exists.
    #[test]
    fn startup_requeue_stays_fast_for_a_real_startup() {
        let poll = Duration::from_secs(10);
        assert_eq!(
            startup_requeue_interval(&pod_ready_at("False", 1_000), 1_030, poll),
            super::super::STARTUP_REQUEUE_INTERVAL
        );
        // No condition yet: this is the first reconcile, so treat it as startup.
        assert_eq!(
            startup_requeue_interval(&[], 1_030, poll),
            super::super::STARTUP_REQUEUE_INTERVAL
        );
    }

    /// A pod that never becomes ready — a crashloop, or a `Render` runner that
    /// is never polled at all — would otherwise sit at 3s forever, ~28,800
    /// reconciles a day each.
    #[test]
    fn startup_requeue_decays_for_a_pod_that_never_becomes_ready() {
        let poll = Duration::from_secs(10);
        let start = 1_000;
        assert_eq!(
            startup_requeue_interval(&pod_ready_at("False", start), start + 5 * 60, poll),
            poll,
            "past the startup window it should fall back to the poll interval"
        );
        assert_eq!(
            startup_requeue_interval(&pod_ready_at("False", start), start + 60 * 60, poll),
            Duration::from_secs(60),
            "a pod un-ready for an hour is wedged, not starting"
        );
    }
}
