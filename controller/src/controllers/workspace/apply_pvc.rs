use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
use kubimo::kube::api::ObjectMeta;
use kubimo::{KubimoWorkspace, prelude::*};

use crate::context::Context;
use crate::resources::{ResourceRequirement, Resources};

use super::{Error, WorkspaceReconciler};

impl WorkspaceReconciler {
    pub(crate) async fn apply_pvc(
        &self,
        ctx: &Context,
        workspace: &KubimoWorkspace,
    ) -> Result<PersistentVolumeClaim, Error> {
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: workspace.metadata.name.clone(),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteMany".to_string()]),
                resources: Resources {
                    requests: ResourceRequirement {
                        storage: workspace.spec.min_storage.clone(),
                        ..Default::default()
                    },
                    limits: ResourceRequirement {
                        storage: workspace.spec.max_storage.clone(),
                        ..Default::default()
                    },
                }
                .into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(ctx
            .client
            .api::<PersistentVolumeClaim>()
            .patch(&pvc)
            .await?)
    }
}
