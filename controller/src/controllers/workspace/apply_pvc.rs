use std::cmp::max_by_key;

use kubimo::k8s_crd_snapshot_storage::{VolumeSnapshot, VolumeSnapshotSource, VolumeSnapshotSpec};
use kubimo::k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, TypedLocalObjectReference,
};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kubimo::kube::api::{AttachParams, ObjectMeta};
use kubimo::{
    Expr, FilterParams, Requirement, Runner, RunnerCommand, RunnerField, StorageQuantity,
    Workspace, prelude::*,
};

use crate::context::Context;
use crate::resources::Resources;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    pub(crate) async fn apply_pvc(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<PersistentVolumeClaim, kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let owner_ref = workspace.static_controller_owner_ref()?;

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
                    workspace.spec.storage.as_ref(),
                )
                .await?,
            ),
            ..Default::default()
        };
        ctx.api_namespaced::<PersistentVolumeClaim>(namespace)
            .patch(&pvc)
            .await
    }
}

async fn get_pvc_spec(
    ctx: &Context,
    workspace_name: &str,
    namespace: &str,
    owner_ref: OwnerReference,
    clone_workspace_name: Option<&str>,
    storage: Option<&Requirement<StorageQuantity>>,
) -> Result<PersistentVolumeClaimSpec, kubimo::Error> {
    let spec = PersistentVolumeClaimSpec {
        access_modes: Some(vec!["ReadWriteOnce".to_string()]),
        ..Default::default()
    };

    if let Some(clone_workspace_name) = clone_workspace_name {
        let clone_workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get(clone_workspace_name)
            .await?;

        get_or_create_snapshot(
            ctx,
            namespace,
            workspace_name,
            owner_ref,
            clone_workspace_name,
        )
        .await?;

        let storage = if let Some((storage, clone_storage)) =
            storage.zip(clone_workspace.spec.storage.as_ref())
        {
            Some(max_by_key(storage, clone_storage, |req| {
                req.min
                    .as_ref()
                    .and_then(StorageQuantity::as_unit::<i64>)
                    .map(|(amount, unit)| amount * unit)
            }))
        } else {
            storage.or(clone_workspace.spec.storage.as_ref())
        };

        Ok(PersistentVolumeClaimSpec {
            resources: Resources::default().storage(storage.cloned()).into(),
            data_source: Some(TypedLocalObjectReference {
                api_group: Some("snapshot.storage.k8s.io".to_string()),
                kind: "VolumeSnapshot".to_string(),
                name: workspace_name.to_owned(),
            }),
            ..spec
        })
    } else {
        Ok(PersistentVolumeClaimSpec {
            resources: Resources::default().storage(storage.cloned()).into(),
            ..spec
        })
    }
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
