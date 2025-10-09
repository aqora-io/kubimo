use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
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
        let namespace = workspace.require_namespace()?;
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: workspace.metadata.name.clone(),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                resources: Resources::default()
                    .storage(workspace.spec.storage.clone())
                    .into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<PersistentVolumeClaim>(namespace)
            .patch(&pvc)
            .await
    }
}
