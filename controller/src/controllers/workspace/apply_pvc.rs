use kubimo::k8s_crd_snapshot_storage::{VolumeSnapshot, VolumeSnapshotSource, VolumeSnapshotSpec};
use kubimo::k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, TypedLocalObjectReference,
};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kubimo::kube::api::{AttachParams, ObjectMeta};
use kubimo::{
    Expr, FilterParams, Requirement, Runner, RunnerCommand, RunnerField, StorageQuantity,
    StorageRequirement, StorageUnit, Workspace, WorkspaceStorageStatus, prelude::*,
};

use crate::context::Context;
use crate::resources::Resources;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    pub(crate) async fn apply_pvc(
        &self,
        ctx: &Context,
        workspace: &Workspace,
        storage: Option<Requirement<StorageQuantity>>,
        current_limit: Option<StorageQuantity>,
    ) -> Result<PersistentVolumeClaim, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let owner_ref = workspace.static_controller_owner_ref()?;

        // A bound claim's storage `limit` is immutable (it cannot be added,
        // changed, or removed), so `current_limit` (read once in `plan_storage`)
        // is echoed back unchanged on every apply rather than fighting that
        // immutability.
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(workspace_name.to_owned()),
                namespace: Some(namespace.to_owned()),
                owner_references: Some(vec![owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(
                get_pvc_spec(
                    ctx,
                    workspace_name,
                    namespace,
                    owner_ref,
                    workspace.spec.clone_workspace_name.as_deref(),
                    storage,
                    current_limit,
                )
                .await?,
            ),
            ..Default::default()
        };
        ctx.api_namespaced::<PersistentVolumeClaim>(namespace)
            .patch(&pvc)
            .await
    }

    /// Resolve the storage requirement to provision: the auto-scaled requirement
    /// clamped to whatever budget governs this workspace. Reads the existing PVC
    /// (PVCs can only grow, so its request is a floor). Also returns that PVC's
    /// immutable storage limit so `apply_pvc` can echo it back without re-reading.
    pub(crate) async fn plan_storage(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(StoragePlan, Option<StorageQuantity>), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;

        let pvc = ctx
            .api_namespaced::<PersistentVolumeClaim>(namespace)
            .get_opt(workspace_name)
            .await?;
        let current_request = pvc.as_ref().and_then(pvc_storage_request);
        let current_limit = pvc.as_ref().and_then(pvc_storage_limit);

        let mut desired = effective_storage(
            workspace.spec.storage.as_ref(),
            workspace.status.as_ref().and_then(|s| s.storage.as_ref()),
            current_request.as_ref(),
            current_limit.as_ref(),
        );

        // A new clone restores a snapshot of the source volume, so its PVC must be
        // at least as large as the source's current request (a snapshot cannot be
        // restored into a smaller volume). Folding that floor in here — before the
        // budget check — both sizes the clone correctly and lets a budget refuse a
        // clone that cannot fit, instead of silently provisioning over the limit.
        if pvc.is_none()
            && let Some(clone_name) = workspace.spec.clone_workspace_name.as_deref()
            && let Some(floor) = clone_storage_floor(ctx, namespace, clone_name).await?
        {
            desired = pick_larger(
                desired,
                Some(Requirement {
                    min: Some(floor),
                    max: None,
                }),
            );
        }

        let allowance =
            crate::controllers::budget::workspace_storage_allowance(ctx, workspace).await?;

        let plan = apply_budget(desired, current_request.as_ref(), pvc.is_some(), allowance);
        Ok((plan, current_limit))
    }
}

/// The minimum storage a fresh clone of `clone_name` must request: the source's
/// current PVC request, or its configured `spec.storage.min` when the source PVC
/// does not exist yet. A snapshot cannot be restored into a smaller volume.
async fn clone_storage_floor(
    ctx: &Context,
    namespace: &str,
    clone_name: &str,
) -> Result<Option<StorageQuantity>, kubimo::Error> {
    if let Some(pvc) = ctx
        .api_namespaced::<PersistentVolumeClaim>(namespace)
        .get_opt(clone_name)
        .await?
        && let Some(request) = pvc_storage_request(&pvc)
    {
        return Ok(Some(request));
    }
    Ok(ctx
        .api_namespaced::<Workspace>(namespace)
        .get_opt(clone_name)
        .await?
        .and_then(|workspace| workspace.spec.storage)
        .and_then(|storage| storage.min))
}

/// Outcome of [`WorkspaceReconciler::plan_storage`].
pub(crate) struct StoragePlan {
    /// The requirement to apply to the PVC (None leaves the storage-class default).
    pub(crate) request: Option<Requirement<StorageQuantity>>,
    /// When set, provisioning is refused with this reason (minimum can't fit budget).
    pub(crate) refuse: Option<String>,
}

/// Clamp the desired storage request to the budget `allowance` (max total bytes
/// this workspace may hold). Never shrinks below the committed PVC request; a
/// brand-new workspace whose minimum can't fit is refused.
fn apply_budget(
    desired: Option<Requirement<StorageQuantity>>,
    current_request: Option<&StorageQuantity>,
    has_pvc: bool,
    allowance: Option<u64>,
) -> StoragePlan {
    let Some(desired) = desired else {
        return StoragePlan {
            request: None,
            refuse: None,
        };
    };
    let Some(cap) = allowance else {
        return StoragePlan {
            request: Some(desired),
            refuse: None,
        };
    };
    let Some(desired_bytes) = desired.min.as_ref().and_then(StorageQuantity::to_bytes) else {
        // No explicit request to size (storage-class default) — pass through.
        return StoragePlan {
            request: Some(desired),
            refuse: None,
        };
    };

    if !has_pvc && desired_bytes > cap {
        return StoragePlan {
            request: None,
            refuse: Some(format!(
                "requested storage {desired_bytes} bytes exceeds remaining budget {cap} bytes"
            )),
        };
    }

    let floor = current_request
        .and_then(StorageQuantity::to_bytes)
        .unwrap_or(0);
    let clamped = desired_bytes.min(cap).max(floor);
    StoragePlan {
        request: Some(Requirement {
            min: Some(StorageQuantity::new(clamped as f64, StorageUnit::B)),
            max: desired.max,
        }),
        refuse: None,
    }
}

async fn get_pvc_spec(
    ctx: &Context,
    workspace_name: &str,
    namespace: &str,
    owner_ref: OwnerReference,
    clone_workspace_name: Option<&str>,
    storage: Option<Requirement<StorageQuantity>>,
    preserve_limit: Option<StorageQuantity>,
) -> Result<PersistentVolumeClaimSpec, kubimo::Error> {
    // Echo back an existing PVC's storage limit unchanged; never introduce one.
    // A bound claim's limit is immutable, so dropping or changing it 422s.
    let resources = |storage| {
        let mut resources = Resources::default().storage(storage);
        resources.limits.storage = preserve_limit.clone();
        resources
    };

    let spec = PersistentVolumeClaimSpec {
        access_modes: Some(vec!["ReadWriteOnce".to_string()]),
        ..Default::default()
    };

    if let Some(clone_workspace_name) = clone_workspace_name {
        get_or_create_snapshot(
            ctx,
            namespace,
            workspace_name,
            owner_ref,
            clone_workspace_name,
        )
        .await?;

        // `storage` already carries the clone floor (see `plan_storage`), so the
        // restored volume is sized to at least the source.
        Ok(PersistentVolumeClaimSpec {
            resources: resources(storage).into(),
            data_source: Some(TypedLocalObjectReference {
                api_group: Some("snapshot.storage.k8s.io".to_string()),
                kind: "VolumeSnapshot".to_string(),
                name: workspace_name.to_owned(),
            }),
            ..spec
        })
    } else {
        Ok(PersistentVolumeClaimSpec {
            resources: resources(storage).into(),
            ..spec
        })
    }
}

/// Read the requested storage quantity from an existing PVC's spec.
pub(crate) fn pvc_storage_request(pvc: &PersistentVolumeClaim) -> Option<StorageQuantity> {
    pvc.spec
        .as_ref()?
        .resources
        .as_ref()?
        .requests
        .as_ref()?
        .get("storage")
        .cloned()
        .map(StorageQuantity::from)
}

/// Read the storage limit from an existing PVC's spec, if one is set.
fn pvc_storage_limit(pvc: &PersistentVolumeClaim) -> Option<StorageQuantity> {
    pvc.spec
        .as_ref()?
        .resources
        .as_ref()?
        .limits
        .as_ref()?
        .get("storage")
        .cloned()
        .map(StorageQuantity::from)
}

/// Pick the requirement with the larger configured minimum.
fn pick_larger(
    a: Option<Requirement<StorageQuantity>>,
    b: Option<Requirement<StorageQuantity>>,
) -> Option<Requirement<StorageQuantity>> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let key = |req: &Requirement<StorageQuantity>| {
                req.min
                    .as_ref()
                    .and_then(StorageQuantity::to_bytes)
                    .unwrap_or(0)
            };
            Some(if key(&a) >= key(&b) { a } else { b })
        }
        (a, b) => a.or(b),
    }
}

/// Resolve the storage requirement to apply to the PVC, accounting for
/// auto-scaling against the reported usage and never shrinking below the
/// current request.
fn effective_storage(
    spec: Option<&StorageRequirement>,
    status: Option<&WorkspaceStorageStatus>,
    current_request: Option<&StorageQuantity>,
    current_limit: Option<&StorageQuantity>,
) -> Option<Requirement<StorageQuantity>> {
    let spec = spec?;
    let max_bytes = spec.max.as_ref().and_then(StorageQuantity::to_bytes);
    let mut request = spec.min.as_ref().and_then(StorageQuantity::to_bytes);

    // auto-scale: grow when usage exceeds `from * capacity`
    if let (Some(auto), Some(status)) = (spec.auto.as_ref(), status)
        && let (Some(used), Some(capacity)) = (
            status.used.as_ref().and_then(StorageQuantity::to_bytes),
            status.capacity.as_ref().and_then(StorageQuantity::to_bytes),
        )
        && used as f64 > auto.from * capacity as f64
    {
        let target = (capacity as f64 * auto.to).ceil() as u64;
        request = Some(request.map_or(target, |b| b.max(target)));
    }

    // clamp up-bound to max, then to an existing PVC's immutable limit (the
    // request may never exceed a limit already on the claim), then floor to the
    // PVC's current request (never shrink)
    if let Some(max) = max_bytes {
        request = request.map(|b| b.min(max));
    }
    if let Some(limit) = current_limit.and_then(StorageQuantity::to_bytes) {
        request = request.map(|b| b.min(limit));
    }
    if let Some(cur) = current_request.and_then(StorageQuantity::to_bytes) {
        request = Some(request.map_or(cur, |b| b.max(cur)));
    }

    // `max` is only a clamp on the request above; it is deliberately not carried
    // into the PVC spec, since a bound claim's storage limit is immutable.
    Some(Requirement {
        min: request.map(|b| StorageQuantity::new(b as f64, StorageUnit::B)),
        max: None,
    })
}

async fn get_or_create_snapshot(
    ctx: &Context,
    namespace: &str,
    workspace_name: &str,
    owner_ref: OwnerReference,
    clone_workspace_name: &str,
) -> Result<VolumeSnapshot, kubimo::Error> {
    if let Some(snapshot) = ctx
        .api_namespaced::<VolumeSnapshot>(namespace)
        .get_opt(workspace_name)
        .await?
    {
        let owned_by_workspace = snapshot
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|refs| {
                refs.iter()
                    .any(|r| r.controller == Some(true) && r.uid == owner_ref.uid.as_str())
            });
        let matches_source = snapshot.spec.source.persistent_volume_claim_name.as_deref()
            == Some(clone_workspace_name);
        if owned_by_workspace && matches_source {
            return Ok(snapshot);
        }
        return Err(kubimo::Error::Custom(format!(
            "refusing to reuse unexpected VolumeSnapshot {workspace_name}"
        )));
    }

    // Flush kernel buffers before taking snapshot
    sync_workspace_pvc(ctx, namespace, clone_workspace_name).await?;

    let snapshot = VolumeSnapshot {
        metadata: ObjectMeta {
            name: Some(workspace_name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: VolumeSnapshotSpec {
            source: VolumeSnapshotSource {
                persistent_volume_claim_name: Some(clone_workspace_name.to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    ctx.api_namespaced::<VolumeSnapshot>(namespace)
        .patch(&snapshot)
        .await
}

async fn sync_workspace_pvc(
    ctx: &Context,
    namespace: &str,
    workspace_name: &str,
) -> Result<(), kubimo::Error> {
    // Do not need to sync when no editor is alive
    let Some(editor) = ctx
        .api_namespaced::<Runner>(namespace)
        .find(&FilterParams::new().with_fields(vec![
            Expr::new(RunnerField::Workspace).eq(workspace_name.to_string()),
            Expr::new(RunnerField::Command).eq(RunnerCommand::Edit),
        ]))
        .await?
    else {
        return Ok(());
    };

    let mut proc = ctx
        .api_namespaced::<Pod>(namespace)
        .kube()
        .exec(
            editor.name()?,
            ["/usr/bin/sync"],
            &AttachParams::default()
                .container("runner")
                .stdout(true)
                .stderr(true),
        )
        .await?;

    // SAFETY: `take_status` is only ever called once
    let status = proc.take_status().expect("cannot take status");

    if let Err(error) = proc.join().await {
        return Err(kubimo::Error::Custom(format!(
            "Cannot sync runner volume: {error:?}"
        )));
    }

    match status.await {
        Some(status) if matches!(status.status.as_deref(), Some("Success")) => Ok(()),
        status => Err(kubimo::Error::Custom(format!(
            "Cannot sync runner volume: code={code:?} reason={reason:?} message={message:?}",
            code = status.as_ref().and_then(|x| x.code),
            reason = status.as_ref().and_then(|x| x.reason.as_deref()),
            message = status.as_ref().and_then(|x| x.message.as_deref()),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::{AutoScale, StorageUnit};

    fn req(min: u64) -> Requirement<StorageQuantity> {
        Requirement {
            min: Some(StorageQuantity::new(min as f64, StorageUnit::B)),
            max: None,
        }
    }

    fn gi(n: u64) -> StorageQuantity {
        StorageQuantity::new(n as f64, StorageUnit::Gi)
    }

    fn request_bytes(plan: &StoragePlan) -> Option<u64> {
        plan.request
            .as_ref()?
            .min
            .as_ref()
            .and_then(StorageQuantity::to_bytes)
    }

    fn min_bytes(req: &Requirement<StorageQuantity>) -> Option<u64> {
        req.min.as_ref().and_then(StorageQuantity::to_bytes)
    }

    #[test]
    fn unbounded_passes_through() {
        let plan = apply_budget(Some(req(100)), None, false, None);
        assert_eq!(request_bytes(&plan), Some(100));
        assert!(plan.refuse.is_none());
    }

    #[test]
    fn under_cap_unchanged() {
        let plan = apply_budget(Some(req(60)), None, false, Some(100));
        assert_eq!(request_bytes(&plan), Some(60));
        assert!(plan.refuse.is_none());
    }

    #[test]
    fn new_workspace_over_cap_refused() {
        let plan = apply_budget(Some(req(120)), None, false, Some(100));
        assert!(plan.request.is_none());
        assert!(plan.refuse.is_some());
    }

    #[test]
    fn existing_pvc_clamped_to_cap() {
        let current = StorageQuantity::new(80.0, StorageUnit::B);
        let plan = apply_budget(Some(req(200)), Some(&current), true, Some(100));
        assert_eq!(request_bytes(&plan), Some(100));
        assert!(plan.refuse.is_none());
    }

    #[test]
    fn clone_floor_raises_min() {
        // `plan_storage` folds the clone floor in with `pick_larger`.
        let floor = req(50);
        // no configured request — the floor becomes the request
        assert_eq!(
            min_bytes(&pick_larger(None, Some(floor.clone())).unwrap()),
            Some(50)
        );
        // a smaller desired min is raised to the floor
        assert_eq!(
            min_bytes(&pick_larger(Some(req(10)), Some(floor.clone())).unwrap()),
            Some(50)
        );
        // a larger desired min wins over the floor
        assert_eq!(
            min_bytes(&pick_larger(Some(req(80)), Some(floor)).unwrap()),
            Some(80)
        );
    }

    #[test]
    fn clone_over_budget_refused() {
        // A clone whose source floor (120) exceeds the cap (100) is refused, even
        // though the workspace itself declares no storage request.
        let desired = pick_larger(None, Some(req(120)));
        let plan = apply_budget(desired, None, false, Some(100));
        assert!(plan.request.is_none());
        assert!(plan.refuse.is_some());
    }

    #[test]
    fn never_shrinks_below_current() {
        // Budget lowered below the committed size: keep current, never shrink.
        let current = StorageQuantity::new(150.0, StorageUnit::B);
        let plan = apply_budget(Some(req(150)), Some(&current), true, Some(100));
        assert_eq!(request_bytes(&plan), Some(150));
        assert!(plan.refuse.is_none());
    }

    #[test]
    fn no_spec_yields_no_requirement() {
        assert!(effective_storage(None, None, None, None).is_none());
    }

    #[test]
    fn max_is_a_clamp_and_never_a_pvc_field() {
        let spec = StorageRequirement {
            min: Some(gi(10)),
            max: Some(gi(20)),
            auto: None,
        };
        let result = effective_storage(Some(&spec), None, None, None).unwrap();
        assert_eq!(min_bytes(&result), gi(10).to_bytes());
        assert!(result.max.is_none());
    }

    #[test]
    fn auto_scale_grows_request_clamped_to_max() {
        let spec = StorageRequirement {
            min: Some(gi(1)),
            max: Some(gi(3)),
            auto: Some(AutoScale { from: 0.5, to: 1.5 }),
        };
        // used (60Gi) > 0.5 * capacity (100Gi): target ceil(100*1.5)=150Gi, clamped to max 3Gi
        let status = WorkspaceStorageStatus {
            used: Some(gi(60)),
            capacity: Some(gi(100)),
            available: Some(gi(40)),
        };
        let result = effective_storage(Some(&spec), Some(&status), None, None).unwrap();
        assert_eq!(min_bytes(&result), gi(3).to_bytes());
    }

    #[test]
    fn never_shrinks_below_current_request() {
        let spec = StorageRequirement {
            min: Some(gi(1)),
            max: None,
            auto: None,
        };
        let result = effective_storage(Some(&spec), None, Some(&gi(5)), None).unwrap();
        assert_eq!(min_bytes(&result), gi(5).to_bytes());
    }

    #[test]
    fn request_never_exceeds_existing_immutable_limit() {
        // A claim already carrying a 2Gi limit can never request more, even when
        // min/max would ask for more — exceeding the immutable limit is invalid.
        let spec = StorageRequirement {
            min: Some(gi(10)),
            max: Some(gi(20)),
            auto: None,
        };
        let result = effective_storage(Some(&spec), None, None, Some(&gi(2))).unwrap();
        assert_eq!(min_bytes(&result), gi(2).to_bytes());
    }
}
