//! The warm-pod-pool protocol between the controller and the node agent.
//!
//! Deliberately not behind `client`, for the same reason as [`crate::conditions`]:
//! these strings are matched byte-exactly by two separately pinned binaries.
//! The controller writes the claim onto a warm pod; the agent, watching its
//! node's pool pods, executes it and acks. Everything here travels as pod
//! labels, annotations or volume attributes — never as CLI flags, which older
//! pinned images reject where unknown metadata is simply ignored.

use k8s_openapi::api::core::v1::Pod;
use serde::{Deserialize, Serialize};

use crate::crd::WorkspaceRestoreSecrets;

/// Permanent label naming the pool a pod was minted from. Survives the claim,
/// so the pool controller keeps seeing lifecycle events for pods it no longer
/// owns (the claim swaps the controller ownerReference to the Runner).
pub const POOL_LABEL: &str = "kubimo.aqora.io/pool";

/// Which side of the claim a pool pod is on. Transitions are guarded with a
/// JSON-Patch `test` op on the previous value, which is what makes a claim and
/// a retire mutually exclusive.
pub const POOL_STATE_LABEL: &str = "kubimo.aqora.io/pool-state";
/// Unclaimed and claimable.
pub const POOL_STATE_WARM: &str = "warm";
/// Bound (or binding) to a Runner.
pub const POOL_STATE_CLAIMED: &str = "claimed";
/// Withdrawn from the claimable set by the pool controller, about to be
/// deleted (template drift or excess replicas).
pub const POOL_STATE_RETIRING: &str = "retiring";

/// The one label a warm pod's own Service selects, set to the route's name
/// ([`WARM_ROUTE_ANNOTATION`]) once the agent acks the claim. A warm pod is
/// minted without it, so its Service has no endpoints and its Ingress reaches
/// nothing until the slot is hydrated.
pub const ROUTE_LABEL: &str = "kubimo.aqora.io/route";

/// Controller-written annotation carrying a JSON [`PoolClaim`].
pub const CLAIM_ANNOTATION: &str = "kubimo.aqora.io/claim";

/// Agent-written ack: [`CLAIM_STATE_BOUND`] once the slot is hydrated and
/// linked, [`CLAIM_STATE_FAILED`] if the claim cannot be honoured (the
/// controller then deletes the pod and falls back to a cold start).
pub const CLAIM_STATE_ANNOTATION: &str = "kubimo.aqora.io/claim-state";
pub const CLAIM_STATE_BOUND: &str = "bound";
pub const CLAIM_STATE_FAILED: &str = "failed";
/// Human-readable reason accompanying a failed ack.
pub const CLAIM_ERROR_ANNOTATION: &str = "kubimo.aqora.io/claim-error";

/// Identity minted at warm-pod creation, persisted on the pod itself so a
/// restarted controller can rebuild `status.claim` without guessing. The same
/// values are baked into the pod command; the annotations are the read-back
/// contract.
pub const WARM_BASE_URL_ANNOTATION: &str = "kubimo.aqora.io/warm-base-url";
pub const WARM_TOKEN_ANNOTATION: &str = "kubimo.aqora.io/warm-token";
/// Names the Service and Ingress minted with a warm pod, both named after the
/// pod and owned by it. Absent on a pod minted by an older controller, whose
/// runner routes it through the runner's own Service and Ingress instead.
pub const WARM_ROUTE_ANNOTATION: &str = "kubimo.aqora.io/warm-route";
/// Hash of the pool template a warm pod was built from; a mismatch is what
/// retires it.
pub const POOL_TEMPLATE_HASH_ANNOTATION: &str = "kubimo.aqora.io/pool-template-hash";

/// Volume attribute marking an anonymous, template-seeded slot that belongs to
/// no workspace yet. Mutually exclusive with the `workspace`/`bucket`/`seed*`
/// attributes.
pub const POOLED_VOLUME_ATTRIBUTE: &str = "pooled";

/// Environment variable telling `start.sh` it is pre-booting for a pool: defer
/// workspace preparation until the claim, which it learns of by polling for
/// [`CLAIM_MARKER_RELATIVE_PATH`].
/// An env var rather than a flag so an older image starts normally instead of
/// crashing on an unknown argument.
pub const CLAIM_MARKER_ENV: &str = "KUBIMO_CLAIM_MARKER";

/// Where, relative to the slot root (`/home/me` in the pod), the agent writes
/// the claim marker once hydration is complete. Outside the `workspace/`
/// subtree, so the indexer never uploads it.
pub const CLAIM_MARKER_RELATIVE_PATH: &str = ".kubimo/claimed";

/// Environment variable naming the file a warm pod reports its claim-time
/// migration through: [`MIGRATION_MARKER_RELATIVE_PATH`] under the slot root.
/// Unset (an older controller), the pod migrates without reporting.
pub const MIGRATION_MARKER_ENV: &str = "KUBIMO_MIGRATION_MARKER";

/// Where, relative to the slot root, a warm pod reports on the legacy-workspace
/// migration the claim marker starts: `pending` from boot, then the claim
/// marker's content once that migration is over, whatever its outcome. The
/// agent holds the ack until then, so no session can plan a notebook's
/// environment before the migration has written its header. A pod with no
/// report (an older image) is acked at once. Written by the pod's user, so
/// outside the root-owned directory of the claim marker.
pub const MIGRATION_MARKER_RELATIVE_PATH: &str = ".kubimo-migration";

/// The payload of [`CLAIM_ANNOTATION`]: everything the agent needs to turn a
/// pod's anonymous slot into the workspace's slot. Carries no credentials —
/// the agent holds the S3 secret kubelet delivered at NodePublishVolume, which
/// is why a claiming workspace's indexer secret must match the pool's.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PoolClaim {
    pub workspace: String,
    /// The workspace's own archive, hydrated with `Values` secrets like any
    /// warm reopen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// Seed fallback, used only when the archive holds no manifest yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_key_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_secrets: Option<WorkspaceRestoreSecrets>,
    /// The workspace's `storage.max`, re-applied to the slot's XFS project
    /// before hydration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

/// The one claimed pod of a workspace every observer agrees should survive a
/// double-claim, out of the workspace's `pods`: the oldest contender, by name
/// on a timestamp tie. Racers may see different subsets for a moment, but any
/// pod whose owner observes a rival and loses this comparison is deleted, so
/// the set only ever shrinks toward a single survivor.
///
/// Contenders are the pods carrying [`POOL_LABEL`] that are neither being
/// deleted nor finished. The filter is part of the rule: two observers that
/// counted contenders differently could elect different survivors.
///
/// Shared with the agent, which applies it before recording a claimed slot
/// as the workspace's: only the survivor may name its node in
/// `status.slot`, or a loser that bound last would leave the workspace's
/// ownership pointing at a node about to lose its pod.
pub fn claim_winner(pods: &[Pod]) -> Option<&str> {
    pods.iter()
        .filter(|pod| is_claim_contender(pod))
        .min_by_key(|pod| {
            (
                pod.metadata.creation_timestamp.clone(),
                pod.metadata.name.clone(),
            )
        })
        .and_then(|pod| pod.metadata.name.as_deref())
}

fn is_claim_contender(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .is_some_and(|labels| labels.contains_key(POOL_LABEL))
        && pod.metadata.deletion_timestamp.is_none()
        && !matches!(
            pod.status
                .as_ref()
                .and_then(|status| status.phase.as_deref()),
            Some("Succeeded" | "Failed")
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KubimoLabel;

    /// The label constants must stay in the kubimo label namespace the rest of
    /// the operator uses; they are spelled out as consts only so the agent can
    /// select on them without a formatter.
    #[test]
    fn pool_labels_are_kubimo_labels() {
        assert_eq!(POOL_LABEL, KubimoLabel::borrow("pool").to_string());
        assert_eq!(
            POOL_STATE_LABEL,
            KubimoLabel::borrow("pool-state").to_string()
        );
        assert_eq!(ROUTE_LABEL, KubimoLabel::borrow("route").to_string());
    }

    /// The claim annotation payload is a controller↔agent wire format between
    /// separately pinned binaries: unknown fields must be ignored and knowns
    /// must round-trip.
    #[test]
    fn pool_claim_round_trips_and_ignores_unknown_fields() {
        let claim = PoolClaim {
            workspace: "bmow-test".into(),
            bucket: Some("archives".into()),
            key_prefix: Some("workspace/abc/".into()),
            seed_bucket: Some("seeds".into()),
            seed_key_prefix: Some("template/".into()),
            seed_secrets: Some(WorkspaceRestoreSecrets::NamesOnly),
            limit_bytes: Some(64 << 30),
        };
        let json = serde_json::to_string(&claim).unwrap();
        assert_eq!(serde_json::from_str::<PoolClaim>(&json).unwrap(), claim);

        let with_unknown: PoolClaim =
            serde_json::from_str(r#"{"workspace":"bmow-test","futureField":true}"#).unwrap();
        assert_eq!(with_unknown.workspace, "bmow-test");
    }

    /// A minimal claim (workspace with no archive yet) must not serialize
    /// nulls: the annotation is read by an agent that may be older than the
    /// controller, and absent means absent.
    #[test]
    fn a_partial_claim_serializes_no_nulls() {
        let claim = PoolClaim {
            workspace: "bmow-test".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&claim).unwrap();
        assert!(!json.contains("null"), "claim had a null: {json}");
    }

    /// Every observer of a double-claim must elect the same survivor, whatever
    /// subset it sees: oldest first, name as the tie-break.
    #[test]
    fn double_claims_agree_on_one_winner() {
        let pod = |name: &str, seconds: i64| claimed_pod(name, seconds);
        let older = pod("editors-bbbb", 100);
        let newer = pod("editors-aaaa", 200);
        let tied = pod("editors-cccc", 100);

        assert_eq!(claim_winner(&[]), None);
        assert_eq!(
            claim_winner(std::slice::from_ref(&newer)),
            Some("editors-aaaa")
        );
        // Age beats name…
        assert_eq!(
            claim_winner(&[newer.clone(), older.clone()]),
            Some("editors-bbbb")
        );
        // …and a timestamp tie falls back to the name, in either order.
        assert_eq!(
            claim_winner(&[tied.clone(), older.clone()]),
            claim_winner(&[older, tied]),
        );
    }

    /// Only pods still holding a claim contend: the oldest pod of the
    /// workspace loses to a younger claimed one if it is unlabelled, being
    /// deleted, or finished.
    #[test]
    fn only_live_claimed_pods_contend() {
        use k8s_openapi::api::core::v1::PodStatus;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let survivor = claimed_pod("editors-young", 200);

        let mut unlabelled = claimed_pod("runner", 100);
        unlabelled.metadata.labels = None;
        let mut deleting = claimed_pod("editors-deleting", 100);
        deleting.metadata.deletion_timestamp = Some(Time(Timestamp::new(300, 0).unwrap()));
        let mut finished = claimed_pod("editors-finished", 100);
        finished.status = Some(PodStatus {
            phase: Some("Failed".to_string()),
            ..Default::default()
        });

        for older in [unlabelled, deleting, finished] {
            assert_eq!(
                claim_winner(&[older, survivor.clone()]),
                Some("editors-young")
            );
        }
    }

    fn claimed_pod(name: &str, created: i64) -> Pod {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
        use k8s_openapi::jiff::Timestamp;
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                creation_timestamp: Some(Time(Timestamp::new(created, 0).unwrap())),
                labels: Some([(POOL_LABEL.to_string(), "editors".to_string())].into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}
