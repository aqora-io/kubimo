use kubimo::{Workspace, WorkspaceDir, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::WorkspaceDirectoryReconciler;

impl WorkspaceDirectoryReconciler {
    pub(crate) async fn apply_owner_reference(
        &self,
        ctx: &Context,
        workspace_dir: &WorkspaceDir,
    ) -> Result<(), kubimo::Error> {
        let namespace = workspace_dir.require_namespace()?;
        if !workspace_dir
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == Workspace::kind(&())
                        && oref.name == workspace_dir.spec.workspace
                })
            })
        {
            let workspace = ctx
                .api_namespaced::<Workspace>(namespace)
                .get(workspace_dir.spec.workspace.as_ref())
                .await?;
            let mut owner_refs = workspace_dir
                .metadata
                .owner_references
                .clone()
                .unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api_namespaced::<WorkspaceDir>(namespace)
                .patch_json(
                    workspace_dir.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
