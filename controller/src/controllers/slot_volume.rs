//! The volume a workspace's files are mounted from, in either storage mode.
//!
//! Shared by the runner pod and the cache job. They mount the same workspace at
//! the same path, so a difference between them is never a design choice — it is
//! a bug. Under `Pooled` in particular, a cache job that still asked for the
//! workspace's PVC would sit Pending forever, because no such PVC exists.

use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    CSIVolumeSource, LocalObjectReference, PersistentVolumeClaimVolumeSource, PodSecurityContext,
    Volume,
};
use kubimo::{Workspace, WorkspaceMode, WorkspacePythonRuntime, WorkspaceRestoreSecrets};

/// Must match the `CSIDriver` object the agent registers under.
pub(crate) const SLOT_CSI_DRIVER: &str = "kubimo.aqora.io";

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
    /// Carries the secrets mode from `spec.restoreFrom.secrets` alongside the
    /// location.
    pub seed: Option<(String, Option<String>, WorkspaceRestoreSecrets)>,
    /// Secret holding the workspace's S3 credentials, from
    /// `spec.indexer.pod.envFrom` — the same one the dedicated indexer pod
    /// mounts.
    ///
    /// Passed to the agent as the volume's `nodePublishSecretRef` rather than
    /// configured on the agent itself, because a node serves workspaces from
    /// more than one S3 account: on a shared cluster each environment has its
    /// own bucket *and* endpoint, so a single node-level credential could only
    /// ever serve one of them. kubelet resolves this in the *pod's* namespace,
    /// which is where the platform already keeps the secret, and hands the
    /// contents to `NodePublishVolume` — so the agent needs no access to
    /// Secrets of its own.
    pub credentials_secret: Option<String>,
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
                .map(|restore| {
                    (
                        restore.bucket.clone(),
                        restore.key_prefix.clone(),
                        restore.secrets.unwrap_or_default(),
                    )
                }),
            credentials_secret: workspace
                .and_then(|workspace| workspace.spec.indexer.as_ref())
                .and_then(credentials_secret_name),
        }
    }
}

/// The Secret an indexer spec pulls its S3 credentials from.
///
/// Takes the first `envFrom` secret reference, which is how the platform
/// expresses this and how the dedicated indexer container consumes it. `env`
/// entries are not considered: a `secretKeyRef` there names a single key, not
/// the whole credential set.
fn credentials_secret_name(indexer: &kubimo::WorkspaceIndexer) -> Option<String> {
    indexer
        .pod
        .as_ref()?
        .env_from
        .as_ref()?
        .iter()
        .find_map(|source| source.secret_ref.as_ref()?.name.clone().into())
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
    python_runtime: WorkspacePythonRuntime,
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
                // Resolved by kubelet in this pod's namespace and delivered to
                // the agent as `NodePublishVolumeRequest.secrets`. Never put
                // credentials in `volume_attributes`: those are stored on the
                // Pod object and readable by anything that can read Pods.
                node_publish_secret_ref: sources
                    .credentials_secret
                    .clone()
                    .map(|name| LocalObjectReference { name }),
                volume_attributes: Some(slot_attributes(workspace_name, sources, python_runtime)),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn slot_attributes(
    workspace_name: &str,
    sources: SlotSources,
    python_runtime: WorkspacePythonRuntime,
) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::from([
        ("workspace".to_string(), workspace_name.to_string()),
        ("python_runtime".to_string(), python_runtime.to_string()),
    ]);
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
    if let Some((bucket, key_prefix, secrets)) = sources.seed {
        attributes.insert("seedBucket".to_string(), bucket);
        if let Some(key_prefix) = key_prefix {
            attributes.insert("seedKeyPrefix".to_string(), key_prefix);
        }
        // Set explicitly even for the default, so the behavior is pinned by
        // this controller rather than by the agent's default. An older agent
        // ignores the attribute, which lands on the same safe default. A mode
        // name, never a credential, so it is safe on the Pod object.
        attributes.insert("seedSecrets".to_string(), secrets.to_string());
    }
    attributes
}

/// The volume name warm pods mount at `/home/me`.
///
/// A workspace pod names its volume after the workspace; a warm pod has none
/// yet, and the name is immutable, so it gets a fixed one. Pool sidecar
/// templates may reference it.
pub(crate) const WARM_SLOT_VOLUME_NAME: &str = "slot";

/// An anonymous, template-seeded slot for a warm pool pod: no workspace, no
/// archive, nothing to hydrate or flush. The agent links it to a workspace at
/// claim time, which is also when it learns the archive location — so the only
/// attributes here are the ones needed to *provision*: the runtime picking the
/// venv template and the interim quota.
pub(crate) fn warm_slot_volume(
    limit_bytes: Option<u64>,
    python_runtime: WorkspacePythonRuntime,
    credentials_secret: Option<String>,
) -> Volume {
    let mut attributes = BTreeMap::from([
        (
            kubimo::pool::POOLED_VOLUME_ATTRIBUTE.to_string(),
            "true".to_string(),
        ),
        ("python_runtime".to_string(), python_runtime.to_string()),
    ]);
    if let Some(limit) = limit_bytes {
        attributes.insert("limitBytes".to_string(), limit.to_string());
    }
    Volume {
        name: WARM_SLOT_VOLUME_NAME.to_string(),
        csi: Some(CSIVolumeSource {
            driver: SLOT_CSI_DRIVER.to_string(),
            read_only: Some(false),
            // The pool's S3 secret, delivered to the agent now because kubelet
            // only hands secrets over at NodePublishVolume — the claim, which
            // is when they are first needed, carries none. Same rule as the
            // workspace volume: never in the attributes.
            node_publish_secret_ref: credentials_secret.map(|name| LocalObjectReference { name }),
            volume_attributes: Some(attributes),
            ..Default::default()
        }),
        ..Default::default()
    }
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
            seed: Some((
                "bucket".into(),
                Some("workspace-template/v1/".into()),
                WorkspaceRestoreSecrets::Values,
            )),
            credentials_secret: Some("s3-credentials".into()),
        }
    }

    #[test]
    fn dedicated_mode_uses_the_workspace_pvc() {
        let volume = workspace_volume(
            "bmow-test",
            WorkspaceMode::Dedicated,
            false,
            sources(),
            Default::default(),
        );
        assert_eq!(
            volume.persistent_volume_claim.unwrap().claim_name,
            "bmow-test"
        );
        assert!(volume.csi.is_none());
    }

    #[test]
    fn pooled_mode_passes_the_slot_sources_through() {
        let volume = workspace_volume(
            "bmow-test",
            WorkspaceMode::Pooled,
            false,
            sources(),
            Default::default(),
        );
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
        assert_eq!(attrs.get("seedSecrets").unwrap(), "values");
    }

    /// The safe default travels explicitly, and only alongside a seed — a
    /// workspace without `restoreFrom` has nothing to gate.
    #[test]
    fn pooled_mode_pins_the_seed_secrets_default() {
        let volume = workspace_volume(
            "bmow-test",
            WorkspaceMode::Pooled,
            false,
            SlotSources {
                seed: Some(("bucket".into(), None, WorkspaceRestoreSecrets::default())),
                ..Default::default()
            },
            Default::default(),
        );
        let attrs = volume.csi.unwrap().volume_attributes.unwrap();
        assert_eq!(attrs.get("seedSecrets").unwrap(), "names-only");
    }

    /// The agent has no S3 credentials of its own — a node serves workspaces
    /// from several S3 accounts at once — so kubelet has to hand it each
    /// workspace's own, resolved from this ref in the *pod's* namespace.
    #[test]
    fn pooled_mode_names_the_workspace_credentials_secret() {
        let volume = workspace_volume(
            "bmow-test",
            WorkspaceMode::Pooled,
            false,
            sources(),
            Default::default(),
        );
        assert_eq!(
            volume.csi.unwrap().node_publish_secret_ref.unwrap().name,
            "s3-credentials"
        );
    }

    /// Volume attributes live on the Pod object, readable by anything that can
    /// read Pods. Credentials must only ever travel via the secret ref.
    #[test]
    fn credentials_never_appear_in_volume_attributes() {
        let volume = workspace_volume(
            "bmow-test",
            WorkspaceMode::Pooled,
            false,
            sources(),
            Default::default(),
        );
        let attrs = volume.csi.unwrap().volume_attributes.unwrap();
        for (key, value) in &attrs {
            if key == "seedSecrets" {
                // The one deliberate exception: a restore *mode* about
                // secrets, never a secret. Pin its value to the enum's two
                // spellings so it can never grow into a carrier.
                assert!(value == "values" || value == "names-only", "{value}");
                continue;
            }
            assert!(!key.to_lowercase().contains("secret"), "{key}");
            assert_ne!(value, "s3-credentials", "{key} leaked the secret name");
        }
    }

    /// Derived from the same `envFrom` secret the dedicated indexer container
    /// mounts, so the two modes read the same credentials.
    #[test]
    fn the_credentials_secret_comes_from_the_indexer_env_from() {
        use kubimo::k8s_openapi::api::core::v1::{EnvFromSource, SecretEnvSource};

        let indexer = kubimo::WorkspaceIndexer {
            pod: Some(kubimo::WorkspaceIndexerPod {
                env_from: Some(vec![EnvFromSource {
                    secret_ref: Some(SecretEnvSource {
                        name: "pr-1035-s3-credentials".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            credentials_secret_name(&indexer).as_deref(),
            Some("pr-1035-s3-credentials")
        );
        // No pod section at all is the standalone case: nothing to reference.
        assert!(credentials_secret_name(&kubimo::WorkspaceIndexer::default()).is_none());
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
            Default::default(),
        );
        let attrs = volume.csi.unwrap().volume_attributes.unwrap();
        assert!(!attrs.contains_key("bucket"));
        assert!(!attrs.contains_key("keyPrefix"));
        assert!(!attrs.contains_key("limitBytes"));
        assert!(!attrs.contains_key("seedBucket"));
        assert!(!attrs.contains_key("seedSecrets"));
    }

    /// A warm slot names no workspace and no archive — the mirror image of
    /// `pooled_mode_passes_the_slot_sources_through`. An agent seeing both
    /// `pooled` and a workspace would be looking at a controller bug and must
    /// refuse, so the builder can never produce that combination.
    #[test]
    fn warm_slot_is_anonymous() {
        let volume = warm_slot_volume(
            Some(2_147_483_648),
            WorkspacePythonRuntime::Uv,
            Some("s3-credentials".into()),
        );
        assert_eq!(volume.name, WARM_SLOT_VOLUME_NAME);
        let csi = volume.csi.unwrap();
        assert_eq!(csi.driver, SLOT_CSI_DRIVER);
        assert_eq!(csi.read_only, Some(false));
        let attrs = csi.volume_attributes.unwrap();
        assert_eq!(attrs.get("pooled").unwrap(), "true");
        assert_eq!(attrs.get("python_runtime").unwrap(), "Uv");
        assert_eq!(attrs.get("limitBytes").unwrap(), "2147483648");
        assert!(!attrs.contains_key("workspace"));
        assert!(!attrs.contains_key("bucket"));
        assert!(!attrs.contains_key("keyPrefix"));
        assert!(!attrs.contains_key("seedBucket"));
        assert!(!attrs.contains_key("seedSecrets"));
    }

    /// Same rule as the workspace volume: credentials only ever travel via the
    /// secret ref, never in attributes readable off the Pod object.
    #[test]
    fn warm_slot_credentials_only_in_the_secret_ref() {
        let volume = warm_slot_volume(None, Default::default(), Some("s3-credentials".into()));
        let csi = volume.csi.unwrap();
        assert_eq!(csi.node_publish_secret_ref.unwrap().name, "s3-credentials");
        for (key, value) in csi.volume_attributes.unwrap().iter() {
            assert!(!key.to_lowercase().contains("secret"), "{key}");
            assert_ne!(value, "s3-credentials", "{key} leaked the secret name");
        }
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
