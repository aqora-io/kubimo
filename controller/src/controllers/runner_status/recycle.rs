//! Detecting a runner whose slot bind mount is dead, so its pod can be recreated.
//!
//! Replacing the agent pod destroys the node data volume; the Scaleway block device
//! detaches ~10s later while the runner's bind mount survives, so every I/O through it
//! returns `EIO` and kubelet cannot recreate the container. Nothing recovers from that
//! on its own — the runner reconciler re-applies the same pod spec and considers itself
//! settled, while `runner_status` reports the failure forever.
//!
//! The remedy is to delete the pod: kubelet then calls `NodeUnpublishVolume`, whose
//! `unbind` clears the stale mount with `MNT_DETACH`, and the runner controller recreates
//! the pod against the new agent. This module decides *when* that is the right thing to
//! do. The deletion itself is deliberately not here.
//!
//! This is remediation, not repair: everything written since the agent died is on a
//! filesystem that no longer exists. The slot re-hydrates from S3 to the last watcher
//! sync. That trade — a restart losing seconds of work, against a session wedged
//! indefinitely — is the whole justification for the module.

// Landed ahead of its call site: this predicate deletes user-visible pods, so it is
// reviewed and tested on its own before anything acts on it. The reconciler wiring, and
// the removal of this allow, is the immediate follow-up.
#![allow(dead_code)]

use std::collections::BTreeMap;

use kubimo::WorkspaceMode;
use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::k8s_openapi::jiff::Timestamp;

use super::conditions::container_state_detail;

/// Annotations recorded on the **Runner**, not the pod: the pod is recreated under the
/// same name, so a pod annotation would vanish with the thing it is meant to remember.
pub(super) const RECYCLED_POD_UID: &str = "kubimo.aqora.io/recycled-pod-uid";
pub(super) const RECYCLED_AT: &str = "kubimo.aqora.io/recycled-at";
pub(super) const RECYCLED_COUNT: &str = "kubimo.aqora.io/recycled-count";

/// Container states in which a dead bind mount surfaces. `CreateContainerError` is
/// kubelet failing to build the container spec because it cannot stat the mount;
/// `RunContainerError` is the runtime failing on the same path.
const WEDGE_REASONS: &[&str] = &["CreateContainerError", "RunContainerError"];

/// Substrings kubelet and the runtime use when the bind source's backing device is gone.
///
/// Deliberately excludes "no such file or directory": a *missing* slot directory is a
/// different bug, and recreating the pod would neither fix it nor make it diagnosable.
const DEAD_MOUNT_MARKERS: &[&str] = &[
    "input/output error",
    "transport endpoint is not connected",
    "stale file handle",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Wedge {
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub(super) struct RecyclePolicy {
    pub enabled: bool,
    /// How long the wedge must persist before acting.
    ///
    /// This is the guard against the case that is *invisible* in container status: a
    /// genuinely new pod on a node whose agent is not up yet fails as a `FailedMount`
    /// event, with the container stuck `ContainerCreating` and no message at all. That
    /// is the normal state during every agent rollout. Only a message-bearing signature
    /// can trigger a recycle, and only after it has outlasted a plausible rollout.
    pub dwell_secs: i64,
    pub cooldown_secs: i64,
    /// After this many recycles the runner is left broken on purpose, so a permanently
    /// bad node surfaces as an error the user can see rather than an endless restart.
    pub max_recycles: u32,
}

impl Default for RecyclePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            dwell_secs: 120,
            cooldown_secs: 600,
            max_recycles: 3,
        }
    }
}

/// The wedge signature, read straight from container state.
pub(super) fn wedged(pod: &Pod) -> Option<Wedge> {
    let (reason, message) = container_state_detail(pod)?;
    if !WEDGE_REASONS.iter().any(|known| *known == reason) {
        return None;
    }
    let haystack = message.to_ascii_lowercase();
    DEAD_MOUNT_MARKERS
        .iter()
        .any(|marker| haystack.contains(marker))
        .then_some(Wedge { reason, message })
}

/// When the pod's containers last stopped being ready.
///
/// Uses the pod's own `ContainersReady` transition rather than our `PvcBound`
/// condition: `upsert_condition` keeps its timestamp across reason-only changes, and for
/// a pod that wedged *after* running, `PvcBound` stays `True` and never transitions at
/// all. Falls back to pod creation, which is correct for a pod that never started.
fn wedged_since(pod: &Pod) -> Option<Timestamp> {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conditions| {
            conditions
                .iter()
                .find(|cond| cond.type_ == "ContainersReady")
        })
        .and_then(|cond| cond.last_transition_time.as_ref().map(|time| time.0))
        .or_else(|| pod.metadata.creation_timestamp.as_ref().map(|time| time.0))
}

/// Whether this pod should be deleted so the runner controller recreates it.
///
/// Pure so the guards can be tested without a cluster; the caller performs the delete.
pub(super) fn should_recycle(
    pod: &Pod,
    mode: WorkspaceMode,
    runner_annotations: Option<&BTreeMap<String, String>>,
    policy: &RecyclePolicy,
    now: Timestamp,
) -> Option<Wedge> {
    if !policy.enabled {
        return None;
    }
    // A Dedicated workspace's PVC returning EIO is a storage-layer fault that recreating
    // the pod cannot repair. Recycling it would mask a real incident.
    if mode != WorkspaceMode::Pooled {
        return None;
    }
    if pod.metadata.deletion_timestamp.is_some() {
        return None;
    }

    let wedge = wedged(pod)?;

    let since = wedged_since(pod)?;
    if now.as_second().saturating_sub(since.as_second()) < policy.dwell_secs {
        return None;
    }

    let annotations = runner_annotations;
    let get = |key: &str| annotations.and_then(|map| map.get(key));

    // Already issued a delete for this exact pod; it just has not landed yet.
    if let (Some(recorded), Some(uid)) = (get(RECYCLED_POD_UID), pod.metadata.uid.as_deref())
        && recorded == uid
    {
        return None;
    }

    if let Some(count) = get(RECYCLED_COUNT).and_then(|raw| raw.parse::<u32>().ok())
        && count >= policy.max_recycles
    {
        return None;
    }

    if let Some(last) = get(RECYCLED_AT).and_then(|raw| raw.parse::<Timestamp>().ok())
        && now.as_second().saturating_sub(last.as_second()) < policy.cooldown_secs
    {
        return None;
    }

    Some(wedge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateWaiting, ContainerStatus,
        PodCondition, PodStatus,
    };
    use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    const EIO: &str = "failed to create containerd task: mount /home/me: input/output error";

    fn enabled() -> RecyclePolicy {
        RecyclePolicy {
            enabled: true,
            ..Default::default()
        }
    }

    /// `wedged_since` is 1000s ago, so the default 120s dwell is satisfied.
    fn now() -> Timestamp {
        Timestamp::from_second(1_000_000).unwrap()
    }

    fn ready_at(seconds: i64) -> PodCondition {
        PodCondition {
            type_: "ContainersReady".to_string(),
            status: "False".to_string(),
            last_transition_time: Some(Time(Timestamp::from_second(seconds).unwrap())),
            ..Default::default()
        }
    }

    fn waiting_pod(reason: &str, message: &str, restart_count: i32) -> Pod {
        Pod {
            metadata: ObjectMeta {
                uid: Some("pod-uid-1".to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                conditions: Some(vec![ready_at(999_000)]),
                container_statuses: Some(vec![ContainerStatus {
                    name: "runner".to_string(),
                    restart_count,
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some(reason.to_string()),
                            message: Some(message.to_string()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn annotations(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn decide(pod: &Pod, annotations: Option<&BTreeMap<String, String>>) -> Option<Wedge> {
        should_recycle(pod, WorkspaceMode::Pooled, annotations, &enabled(), now())
    }

    #[test]
    fn recycles_a_dead_mount() {
        let pod = waiting_pod("CreateContainerError", EIO, 0);
        assert!(decide(&pod, None).is_some());
    }

    /// The case `PvcBound` cannot see: `slot_bound_condition` treats
    /// `restart_count > 0` as proof the mount succeeded, so a pod that ran, lost its
    /// filesystem, and now cannot recreate its container reports `PvcBound=True`.
    /// Reading container state directly is what makes this reachable.
    #[test]
    fn recycles_a_pod_that_wedged_after_running() {
        let pod = waiting_pod("CreateContainerError", EIO, 1);
        assert!(decide(&pod, None).is_some());
    }

    #[test]
    fn recycles_every_dead_mount_marker() {
        for marker in DEAD_MOUNT_MARKERS {
            let pod = waiting_pod("RunContainerError", &format!("mounting slot: {marker}"), 0);
            assert!(decide(&pod, None).is_some(), "{marker} should recycle");
        }
    }

    /// A missing slot directory is a different bug; recreating the pod fixes nothing.
    #[test]
    fn ignores_a_missing_path() {
        let pod = waiting_pod(
            "CreateContainerError",
            "stat /data/slots/x: no such file",
            0,
        );
        assert!(decide(&pod, None).is_none());
    }

    /// The state every runner passes through while a replacement agent starts. Recycling
    /// here would delete healthy pods on every agent rollout.
    #[test]
    fn ignores_container_creating_with_no_message() {
        let mut pod = waiting_pod("ContainerCreating", "", 0);
        if let Some(status) = pod.status.as_mut()
            && let Some(containers) = status.container_statuses.as_mut()
            && let Some(state) = containers[0].state.as_mut()
            && let Some(waiting) = state.waiting.as_mut()
        {
            waiting.message = None;
        }
        assert!(decide(&pod, None).is_none());
    }

    #[test]
    fn ignores_an_ordinary_crash() {
        let pod = waiting_pod(
            "CrashLoopBackOff",
            "back-off 5m0s restarting failed container",
            0,
        );
        assert!(decide(&pod, None).is_none());
    }

    #[test]
    fn ignores_a_running_container() {
        let mut pod = waiting_pod("CreateContainerError", EIO, 0);
        if let Some(status) = pod.status.as_mut()
            && let Some(containers) = status.container_statuses.as_mut()
        {
            containers[0].state = Some(ContainerState {
                running: Some(ContainerStateRunning::default()),
                ..Default::default()
            });
        }
        assert!(decide(&pod, None).is_none());
    }

    /// A Dedicated PVC returning EIO is a storage incident to surface, not to mask.
    #[test]
    fn never_recycles_a_dedicated_workspace() {
        let pod = waiting_pod("CreateContainerError", EIO, 0);
        let decision = should_recycle(&pod, WorkspaceMode::Dedicated, None, &enabled(), now());
        assert!(decision.is_none());
    }

    #[test]
    fn disabled_by_default() {
        let pod = waiting_pod("CreateContainerError", EIO, 0);
        let decision = should_recycle(
            &pod,
            WorkspaceMode::Pooled,
            None,
            &RecyclePolicy::default(),
            now(),
        );
        assert!(decision.is_none());
    }

    #[test]
    fn waits_out_the_dwell() {
        let mut pod = waiting_pod("CreateContainerError", EIO, 0);
        // Wedged one second ago: below the 120s dwell.
        if let Some(status) = pod.status.as_mut() {
            status.conditions = Some(vec![ready_at(999_999)]);
        }
        assert!(decide(&pod, None).is_none());

        if let Some(status) = pod.status.as_mut() {
            status.conditions = Some(vec![ready_at(999_880)]);
        }
        assert!(decide(&pod, None).is_some(), "exactly at the dwell");
    }

    #[test]
    fn skips_a_pod_already_recycled() {
        let pod = waiting_pod("CreateContainerError", EIO, 0);
        let seen = annotations(&[(RECYCLED_POD_UID, "pod-uid-1")]);
        assert!(decide(&pod, Some(&seen)).is_none());

        let other = annotations(&[(RECYCLED_POD_UID, "pod-uid-0")]);
        assert!(decide(&pod, Some(&other)).is_some());
    }

    #[test]
    fn stops_at_the_cap() {
        let pod = waiting_pod("CreateContainerError", EIO, 0);
        let at_cap = annotations(&[(RECYCLED_COUNT, "3")]);
        assert!(decide(&pod, Some(&at_cap)).is_none());

        let below = annotations(&[(RECYCLED_COUNT, "2")]);
        assert!(decide(&pod, Some(&below)).is_some());
    }

    #[test]
    fn respects_the_cooldown() {
        let pod = waiting_pod("CreateContainerError", EIO, 0);
        let recent = Timestamp::from_second(999_900).unwrap();
        let inside = annotations(&[(RECYCLED_AT, &recent.to_string())]);
        assert!(decide(&pod, Some(&inside)).is_none());

        let old = Timestamp::from_second(999_000).unwrap();
        let outside = annotations(&[(RECYCLED_AT, &old.to_string())]);
        assert!(decide(&pod, Some(&outside)).is_some());
    }

    #[test]
    fn skips_a_pod_already_terminating() {
        let mut pod = waiting_pod("CreateContainerError", EIO, 0);
        pod.metadata.deletion_timestamp = Some(Time(now()));
        assert!(decide(&pod, None).is_none());
    }
}
