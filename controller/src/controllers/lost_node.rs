//! Pods stranded on a node that is gone.
//!
//! Only a pod's kubelet can confirm its deletion, so a pod on a node that has
//! stopped reporting stays Terminating until the node comes back, however long
//! that takes. For a workspace that is fatal: its pods' required pod affinity
//! (see [`super::workspace_affinity`]) matches the stranded pods, so every
//! replacement is pinned to the dead node too. Force-deleting is what an
//! operator would do by hand; this module decides when it is safe to.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::TryStreamExt;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::api::core::v1::{Node, Pod};
use kubimo::k8s_openapi::jiff::{SignedDuration, Timestamp};
use kubimo::kube::api::{DeleteParams, Preconditions};
use kubimo::kube::runtime::controller::Action;
use kubimo::{FilterParams, prelude::*};

use crate::context::Context;
use crate::controllers::workspace_affinity;

/// How long past its deletion deadline a pod must still exist before it counts
/// as stranded. The node lifecycle controller has already waited out the
/// unreachable toleration (300 s by default) before evicting it; this only
/// covers a kubelet that comes back right at the deadline.
pub(crate) const STRANDED_AFTER: Duration = Duration::from_secs(60);

/// Taints that say the node's kubelet is not there to confirm anything:
/// the node lifecycle controller's own, and the one an operator sets for a
/// non-graceful node shutdown.
const LOST_NODE_TAINTS: &[&str] = &[
    "node.kubernetes.io/unreachable",
    "node.kubernetes.io/out-of-service",
];

/// Whether `pod` was deleted, has outlived its deletion deadline by
/// [`STRANDED_AFTER`], and sits on a node that cannot finish the deletion:
/// one that no longer exists (`node` is `None`), or one that is not Ready and
/// carries a [`LOST_NODE_TAINTS`] taint.
///
/// A NotReady node without such a taint still has a kubelet talking to the API
/// server (it is reporting NotReady), so it will finish the deletion itself.
pub(crate) fn stranded_on_lost_node(pod: &Pod, node: Option<&Node>, now: Timestamp) -> bool {
    let Some(deadline) = pod.metadata.deletion_timestamp.as_ref() else {
        return false;
    };
    let stranded_at = deadline.0 + SignedDuration::try_from(STRANDED_AFTER).unwrap_or_default();
    if now < stranded_at {
        return false;
    }
    let Some(node) = node else {
        return true;
    };
    !is_ready(node) && has_lost_node_taint(node)
}

/// What [`release_stranded_pods`] found among a workspace's pods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LostNodeSweep {
    /// No pod of the workspace is terminating.
    Clear,
    /// At least one stranded pod was force-deleted by this sweep.
    Released,
    /// Some pod is terminating but not (yet) stranded, or the sweep could not
    /// tell. Its node may still finish the deletion, or it may turn out to be
    /// lost: no watched object changes when a node is lost, so the caller has
    /// to come back.
    Waiting,
}

impl LostNodeSweep {
    /// The action a reconcile that ran this sweep should return, given what
    /// the rest of it asked for.
    ///
    /// `Released` comes back shortly, so the replacement is created against a
    /// workspace with the stranded pods gone. `Waiting` only turns an
    /// `await_change` into a timer: a reconcile that already set one keeps it.
    /// A runner's never exceeds a few seconds; a cache job held back by
    /// `startAfter` comes back when due, and sweeps then.
    pub(crate) fn settle(self, action: Action) -> Action {
        match self {
            Self::Released => Action::requeue(Duration::from_secs(2)),
            Self::Waiting if action == Action::await_change() => Action::requeue(WAITING_REQUEUE),
            _ => action,
        }
    }
}

/// How often a workspace with a terminating pod is re-checked for a lost node.
const WAITING_REQUEUE: Duration = Duration::from_secs(30);

/// The action for a job reconcile whose Job is `job`.
///
/// The job controllers watch their Jobs, not the Jobs' pods, so a pod evicted
/// off a lost node is not guaranteed to wake them, and no other controller
/// sweeps a workspace that has only jobs. While the Job runs, come back on a
/// timer so the sweep gets its chance; once it has finished, its pods hold no
/// slot and there is nothing left to release.
pub(crate) fn recheck_while_running(job: &Job) -> Action {
    let finished = job
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.status == "True"
                    && (condition.type_ == "Complete" || condition.type_ == "Failed")
            })
        });
    if finished {
        Action::await_change()
    } else {
        Action::requeue(WAITING_REQUEUE)
    }
}

/// Force-delete every pod of `workspace` that is stranded on a lost node
/// (see [`stranded_on_lost_node`]).
///
/// All of the workspace's pods, not just the caller's own: a stranded
/// sibling — a runner, a claimed warm pod, a cache or import job's pod —
/// matches the workspace's required pod affinity just the same and pins every
/// replacement to the dead node.
///
/// Never fails: an API error here (a missing `nodes` permission, a blip) is
/// logged and reported as `Waiting`, so it retries without holding up the rest
/// of the caller's reconcile.
pub(crate) async fn release_stranded_pods(
    ctx: &Context,
    namespace: &str,
    workspace: &str,
) -> LostNodeSweep {
    match try_release_stranded_pods(ctx, namespace, workspace).await {
        Ok(sweep) => sweep,
        Err(err) => {
            tracing::warn!(%err, workspace, "could not check the workspace's pods for a lost node");
            LostNodeSweep::Waiting
        }
    }
}

async fn try_release_stranded_pods(
    ctx: &Context,
    namespace: &str,
    workspace: &str,
) -> Result<LostNodeSweep, kubimo::Error> {
    let pods = ctx.api_namespaced::<Pod>(namespace);
    let terminating: Vec<Pod> = pods
        .list(&FilterParams::new().with_labels(workspace_affinity::workspace_label(workspace)))
        .map_ok(|item| item.item)
        .try_filter(|pod| futures::future::ready(pod.metadata.deletion_timestamp.is_some()))
        .try_collect()
        .await?;
    if terminating.is_empty() {
        return Ok(LostNodeSweep::Clear);
    }

    let nodes = ctx.api_global::<Node>();
    let mut node_cache: BTreeMap<String, Option<Node>> = BTreeMap::new();
    let now = Timestamp::now();
    let mut sweep = LostNodeSweep::Waiting;
    for pod in terminating {
        // An unscheduled pod has no kubelet to wait on; the API server
        // removes it on its own.
        let Some(node_name) = pod.spec.as_ref().and_then(|spec| spec.node_name.clone()) else {
            continue;
        };
        let node = match node_cache.get(&node_name) {
            Some(node) => node.clone(),
            None => match nodes.get_opt(&node_name).await {
                Ok(node) => {
                    node_cache.insert(node_name.clone(), node.clone());
                    node
                }
                // One unreadable node must not stop the sweep of the others.
                Err(err) => {
                    tracing::warn!(%err, workspace, node = node_name, "could not read the pod's node");
                    continue;
                }
            },
        };
        if !stranded_on_lost_node(&pod, node.as_ref(), now) {
            continue;
        }
        let name = pod.name()?;
        if held_by_finalizers(&pod) {
            tracing::warn!(
                workspace,
                pod = name,
                finalizers = ?pod.metadata.finalizers,
                "a pod force-deleted off a lost node is still held by its finalizers"
            );
            continue;
        }
        // Pinned to this pod's uid: a pod recreated under the same name in
        // the meantime is a different pod, on a node that may be fine.
        let params = DeleteParams {
            grace_period_seconds: Some(0),
            preconditions: pod.metadata.uid.clone().map(|uid| Preconditions {
                uid: Some(uid),
                resource_version: None,
            }),
            ..Default::default()
        };
        match pods.kube().delete(name, &params).await {
            Ok(_) => {}
            // Already gone, or already replaced (the uid precondition):
            // another reconcile got there first.
            Err(kubimo::kube::Error::Api(status)) if status.code == 404 || status.code == 409 => {
                continue;
            }
            // Nor one failed delete: what was already released stays
            // `Released`, and this pod is retried on the next pass.
            Err(err) => {
                tracing::warn!(%err, workspace, pod = name, "could not force-delete a stranded pod");
                continue;
            }
        }
        tracing::warn!(
            workspace,
            pod = name,
            node = node_name,
            "force-deleted a pod stranded on a lost node"
        );
        sweep = LostNodeSweep::Released;
    }
    Ok(sweep)
}

/// Whether `pod` has been force-deleted already and only its finalizers keep
/// it. Deleting it again changes nothing, and reporting it released would only
/// have the caller come straight back for it. Without finalizers it is not
/// held: a force-delete whose removal from storage failed is retried by
/// deleting again.
fn held_by_finalizers(pod: &Pod) -> bool {
    pod.metadata.deletion_grace_period_seconds == Some(0)
        && pod
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|finalizers| !finalizers.is_empty())
}

pub(crate) fn is_ready(node: &Node) -> bool {
    node.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
}

fn has_lost_node_taint(node: &Node) -> bool {
    node.spec
        .as_ref()
        .and_then(|spec| spec.taints.as_ref())
        .is_some_and(|taints| {
            taints
                .iter()
                .any(|taint| LOST_NODE_TAINTS.contains(&taint.key.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use kubimo::k8s_openapi::api::core::v1::{NodeCondition, NodeSpec, NodeStatus, Taint};
    use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    use super::*;

    fn now() -> Timestamp {
        "2026-10-15T01:00:00Z".parse().unwrap()
    }

    fn pod_deleted_at(deadline: Option<&str>) -> Pod {
        Pod {
            metadata: ObjectMeta {
                deletion_timestamp: deadline.map(|ts| Time(ts.parse().unwrap())),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn node(ready: &str, taints: &[&str]) -> Node {
        Node {
            spec: Some(NodeSpec {
                taints: Some(
                    taints
                        .iter()
                        .map(|key| Taint {
                            key: key.to_string(),
                            effect: "NoExecute".to_string(),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            status: Some(NodeStatus {
                conditions: Some(vec![NodeCondition {
                    type_: "Ready".to_string(),
                    status: ready.to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const LONG_AGO: &str = "2026-10-15T00:42:00Z";

    #[test]
    fn a_pod_past_its_deadline_on_an_unreachable_node_is_stranded() {
        let pod = pod_deleted_at(Some(LONG_AGO));
        let unreachable = node("Unknown", &["node.kubernetes.io/unreachable"]);
        assert!(stranded_on_lost_node(&pod, Some(&unreachable), now()));
        let out_of_service = node("False", &["node.kubernetes.io/out-of-service"]);
        assert!(stranded_on_lost_node(&pod, Some(&out_of_service), now()));
    }

    #[test]
    fn a_pod_whose_node_is_gone_is_stranded() {
        assert!(stranded_on_lost_node(
            &pod_deleted_at(Some(LONG_AGO)),
            None,
            now()
        ));
    }

    #[test]
    fn a_pod_not_being_deleted_is_never_stranded() {
        let unreachable = node("Unknown", &["node.kubernetes.io/unreachable"]);
        assert!(!stranded_on_lost_node(
            &pod_deleted_at(None),
            Some(&unreachable),
            now()
        ));
        assert!(!stranded_on_lost_node(&pod_deleted_at(None), None, now()));
    }

    #[test]
    fn a_pod_within_its_deadline_margin_is_not_stranded() {
        let unreachable = node("Unknown", &["node.kubernetes.io/unreachable"]);
        let recent = pod_deleted_at(Some("2026-10-15T00:59:30Z"));
        assert!(!stranded_on_lost_node(&recent, Some(&unreachable), now()));
        assert!(!stranded_on_lost_node(&recent, None, now()));
    }

    #[test]
    fn waiting_only_turns_an_idle_reconcile_into_a_timer() {
        let soon = Action::requeue(Duration::from_secs(5));
        assert_eq!(
            LostNodeSweep::Clear.settle(Action::await_change()),
            Action::await_change()
        );
        assert_eq!(
            LostNodeSweep::Waiting.settle(Action::await_change()),
            Action::requeue(WAITING_REQUEUE)
        );
        assert_eq!(LostNodeSweep::Waiting.settle(soon.clone()), soon);
        assert_eq!(
            LostNodeSweep::Released.settle(Action::await_change()),
            Action::requeue(Duration::from_secs(2))
        );
    }

    #[test]
    fn only_a_force_deleted_pod_with_finalizers_is_held() {
        let pod = |grace: Option<i64>, finalizers: &[&str]| Pod {
            metadata: ObjectMeta {
                deletion_grace_period_seconds: grace,
                finalizers: Some(finalizers.iter().map(|f| f.to_string()).collect()),
                ..Default::default()
            },
            ..Default::default()
        };
        let tracking = "batch.kubernetes.io/job-tracking";
        assert!(held_by_finalizers(&pod(Some(0), &[tracking])));
        // Not force-deleted yet: the first force-delete is still worth making.
        assert!(!held_by_finalizers(&pod(Some(30), &[tracking])));
        // Nothing holds it; deleting again finishes a removal that failed.
        assert!(!held_by_finalizers(&pod(Some(0), &[])));
    }

    #[test]
    fn a_node_that_can_still_confirm_strands_nothing() {
        let pod = pod_deleted_at(Some(LONG_AGO));
        // Ready, even if a stale taint lingers.
        let ready = node("True", &["node.kubernetes.io/unreachable"]);
        assert!(!stranded_on_lost_node(&pod, Some(&ready), now()));
        // NotReady but its kubelet is reporting: it finishes the deletion.
        let not_ready = node("False", &["node.kubernetes.io/not-ready"]);
        assert!(!stranded_on_lost_node(&pod, Some(&not_ready), now()));
    }
}
