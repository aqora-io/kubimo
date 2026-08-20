//! The warm-pod-pool protocol between the controller and the node agent.
//!
//! Deliberately not behind `client`, for the same reason as [`crate::conditions`]:
//! these strings are matched byte-exactly by two separately pinned binaries.
//! The controller writes the claim onto a warm pod; the agent, watching its
//! node's pool pods, executes it and acks. Everything here travels as pod
//! labels, annotations or volume attributes — never as CLI flags, which older
//! pinned images reject where unknown metadata is simply ignored.

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
/// Hash of the pool template a warm pod was built from; a mismatch is what
/// retires it.
pub const POOL_TEMPLATE_HASH_ANNOTATION: &str = "kubimo.aqora.io/pool-template-hash";

/// Volume attribute marking an anonymous, template-seeded slot that belongs to
/// no workspace yet. Mutually exclusive with the `workspace`/`bucket`/`seed*`
/// attributes.
pub const POOLED_VOLUME_ATTRIBUTE: &str = "pooled";

/// Environment variable telling `start.sh` it is pre-booting for a pool: skip
/// the dependency sync now and poll for [`CLAIM_MARKER_RELATIVE_PATH`] instead.
/// An env var rather than a flag so an older image starts normally instead of
/// crashing on an unknown argument.
pub const CLAIM_MARKER_ENV: &str = "KUBIMO_CLAIM_MARKER";

/// Where, relative to the slot root (`/home/me` in the pod), the agent writes
/// the claim marker once hydration is complete. Outside the `workspace/`
/// subtree, so the indexer never uploads it.
pub const CLAIM_MARKER_RELATIVE_PATH: &str = ".kubimo/claimed";

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
}
