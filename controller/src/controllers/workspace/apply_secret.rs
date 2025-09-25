use kubimo::k8s_openapi::api::core::v1::Secret;
use kubimo::kube::api::ObjectMeta;
use kubimo::{Workspace, prelude::*};

use crate::context::Context;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    pub(crate) async fn apply_secret(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<Option<Secret>, kubimo::Error> {
        let namespace = workspace.require_namespace()?;
        let Some(data) = workspace.spec.secret_data.clone() else {
            return Ok(None);
        };
        let secret = Secret {
            metadata: ObjectMeta {
                name: workspace.metadata.name.clone(),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };
        Ok(Some(
            ctx.api_namespaced::<Secret>(namespace)
                .patch(&secret)
                .await?,
        ))
    }
}
