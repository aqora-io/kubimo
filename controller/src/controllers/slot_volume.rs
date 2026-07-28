//! The volume a workspace's files are mounted from, in either storage mode.
//!
//! Shared by the runner pod and the cache job. They mount the same workspace at
//! the same path, so a difference between them is never a design choice — it is
//! a bug. Under `Pooled` in particular, a cache job that still asked for the
//! workspace's PVC would sit Pending forever, because no such PVC exists.

use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    CSIVolumeSource, PersistentVolumeClaimVolumeSource, PodSecurityContext, Volume,
};
use kubimo::{Workspace, WorkspaceMode};

/// Must match the `CSIDriver` object the agent registers under.
const SLOT_CSI_DRIVER: &str = "kubimo.aqora.io";

/// Where the agent sources a slot's contents.
#[derive(Debug, Default, Clone)]
pub(crate) struct SlotSources {
    /// Hard per-slot capacity, from `spec.storage.max`.
    pub limit_bytes: Option<u64>,
    /// The workspace's own archive, from `spec.indexer`: hydrated on mount and
    /// written back on flush. A missing bucket means "no archive": the slot
    /// starts empty and is never persisted.
    pub archive: Option<(Option<String>, Option<String>)>,
    /// Fallback source for a workspace that has never been indexed, from
    /// `spec.restoreFrom`. Consumed by a restore init container under
    /// `Dedicated`; under `Pooled` there is no init Job, so it travels to the
    /// agent and is applied only when `archive` turns out to have no manifest.
    pub seed: Option<(String, Option<String>)>,
}

impl SlotSources {
    pub(crate) fn from_workspace(workspace: Option<&Workspace>) -> Self {
        Self {
            limit_bytes: workspace
                .and_then(|workspace| workspace.spec.storage.as_ref())
                .and_then(|storage| storage.max.as_ref())
                .and_then(|max| max.to_bytes()),
            archive: workspace
                .and_then(|workspace| workspace.spec.indexer.as_ref())
                .map(|indexer| (indexer.bucket.clone(), indexer.key_prefix.clone())),
            seed: workspace
                .and_then(|workspace| workspace.spec.restore_from.as_ref())
                .map(|restore| (restore.bucket.clone(), restore.key_prefix.clone())),
        }
    }
}

/// The volume mounted at `/home/me`.
///
/// `Dedicated` uses the workspace's own PVC. `Pooled` uses an **inline
/// ephemeral** CSI volume served by the node agent, which resolves the
/// workspace to a slot on the node's shared data volume. Inline means no
/// PersistentVolume and therefore no topology constraint, so the scheduler
/// stays in charge — a per-node PVC would force hostname pinning, which
/// cluster-autoscaler can never satisfy against its template node.
pub(crate) fn workspace_volume(
    workspace_name: &str,
    mode: WorkspaceMode,
    read_only: bool,
    sources: SlotSources,
) -> Volume {
    match mode {
        WorkspaceMode::Dedicated => Volume {
            name: workspace_name.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_name.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        },
        WorkspaceMode::Pooled => Volume {
            name: workspace_name.to_string(),
            csi: Some(CSIVolumeSource {
                driver: SLOT_CSI_DRIVER.to_string(),
                read_only: Some(read_only),
                volume_attributes: Some(slot_attributes(workspace_name, sources)),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn slot_attributes(workspace_name: &str, sources: SlotSources) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::from([("workspace".to_string(), workspace_name.to_string())]);
    if let Some(limit) = sources.limit_bytes {
        attributes.insert("limitBytes".to_string(), limit.to_string());
    }
    // Only pass the bucket when it is actually set: the agent treats a missing
    // bucket as "no archive, start empty" rather than guessing one.
    if let Some((bucket, key_prefix)) = sources.archive {
        if let Some(bucket) = bucket {
            attributes.insert("bucket".to_string(), bucket);
        }
        if let Some(key_prefix) = key_prefix {
            attributes.insert("keyPrefix".to_string(), key_prefix);
        }
    }
    if let Some((bucket, key_prefix)) = sources.seed {
        attributes.insert("seedBucket".to_string(), bucket);
        if let Some(key_prefix) = key_prefix {
            attributes.insert("seedKeyPrefix".to_string(), key_prefix);
        }
    }
    attributes
}

/// `fsGroup` is only safe on a volume the workspace owns outright.
///
/// On the shared node volume kubelet would recursively chown the **entire**
/// volume — every slot on the node — at every pod start, which blows past the
/// runner's 90s startup probe once a node is full. The agent chowns exactly the
/// slot it creates instead, and the runner image already runs as uid 1000.
pub(crate) fn pod_security_context(mode: WorkspaceMode) -> Option<PodSecurityContext> {
    match mode {
        WorkspaceMode::Dedicated => Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        WorkspaceMode::Pooled => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> SlotSources {
        SlotSources {
            limit_bytes: Some(2_147_483_648),
            archive: Some((Some("bucket".into()), Some("workspace/abc/".into()))),
            seed: Some(("bucket".into(), Some("workspace-template/v1/".into()))),
        }
    }

    #[test]
    fn dedicated_mode_uses_the_workspace_pvc() {
        let volume = workspace_volume("bmow-test", WorkspaceMode::Dedicated, false, sources());
        assert_eq!(
            volume.persistent_volume_claim.unwrap().claim_name,
            "bmow-test"
        );
        assert!(volume.csi.is_none());
    }

    #[test]
    fn pooled_mode_passes_the_slot_sources_through() {
        let volume = workspace_volume("bmow-test", WorkspaceMode::Pooled, false, sources());
        assert!(volume.persistent_volume_claim.is_none());
        let csi = volume.csi.unwrap();
        assert_eq!(csi.driver, SLOT_CSI_DRIVER);
        let attrs = csi.volume_attributes.unwrap();
        assert_eq!(attrs.get("workspace").unwrap(), "bmow-test");
        assert_eq!(attrs.get("limitBytes").unwrap(), "2147483648");
        assert_eq!(attrs.get("bucket").unwrap(), "bucket");
        assert_eq!(attrs.get("keyPrefix").unwrap(), "workspace/abc/");
        assert_eq!(attrs.get("seedBucket").unwrap(), "bucket");
        // The workspace's own archive stays distinct from its seed: conflating
        // them would let a warm workspace be overwritten by the template it was
        // created from.
        assert_eq!(
            attrs.get("seedKeyPrefix").unwrap(),
            "workspace-template/v1/"
        );
    }

    /// A workspace with no indexer config must not get a half-specified
    /// archive: the agent keys off `bucket` being absent to start empty.
    #[test]
    fn pooled_mode_omits_archive_attributes_when_unconfigured() {
        let volume = workspace_volume(
            "bmow-test",
            WorkspaceMode::Pooled,
            false,
            SlotSources::default(),
        );
        let attrs = volume.csi.unwrap().volume_attributes.unwrap();
        assert!(!attrs.contains_key("bucket"));
        assert!(!attrs.contains_key("keyPrefix"));
        assert!(!attrs.contains_key("limitBytes"));
        assert!(!attrs.contains_key("seedBucket"));
    }

    /// Kubelet applies `fsGroup` to the whole volume, which on a shared node
    /// volume is every tenant's slot.
    #[test]
    fn pooled_mode_drops_fs_group() {
        assert!(pod_security_context(WorkspaceMode::Pooled).is_none());
        assert_eq!(
            pod_security_context(WorkspaceMode::Dedicated)
                .unwrap()
                .fs_group,
            Some(1000)
        );
    }
}
