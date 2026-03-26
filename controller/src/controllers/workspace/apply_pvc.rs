use kubimo::k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, TypedLocalObjectReference,
};
use kubimo::kcr_snapshot_storage_k8s_io::v1::volumesnapshots::{
    VolumeSnapshot, VolumeSnapshotSource, VolumeSnapshotSpec,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Workspace, prelude::*};

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

        if let Some(pvc) = ctx
            .api_namespaced::<PersistentVolumeClaim>(namespace)
            .get_opt(workspace_name)
            .await?
        {
            return Ok(pvc);
        }

        let data_source =
            if let Some(clone_workspace_name) = workspace.spec.clone_workspace_name.as_ref() {
                let snapshot = VolumeSnapshot {
                    metadata: ObjectMeta {
                        name: Some(workspace_name.to_owned()),
                        namespace: Some(namespace.to_owned()),
                        ..Default::default()
                    },
                    spec: VolumeSnapshotSpec {
                        source: VolumeSnapshotSource {
                            persistent_volume_claim_name: Some(clone_workspace_name.clone()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                };

                ctx.api_namespaced::<VolumeSnapshot>(namespace)
                    .patch(&snapshot)
                    .await?;

                Some(TypedLocalObjectReference {
                    api_group: Some("snapshot.storage.k8s.io".to_string()),
                    kind: "VolumeSnapshot".to_string(),
                    name: workspace_name.to_owned(),
                })
            } else {
                None
            };

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(workspace_name.to_owned()),
                namespace: Some(namespace.to_owned()),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                resources: Resources::default()
                    .storage(workspace.spec.storage.clone())
                    .into(),
                data_source,
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<PersistentVolumeClaim>(namespace)
            .patch(&pvc)
            .await
    }
}
