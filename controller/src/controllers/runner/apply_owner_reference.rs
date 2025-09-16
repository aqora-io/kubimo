use kubimo::{Runner, Workspace, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_owner_reference(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<(), kubimo::Error> {
        let namespace = runner.require_namespace()?;
        if !runner
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == Workspace::kind(&())
                        && oref.name == runner.spec.workspace
                })
            })
        {
            let workspace = ctx
                .api_namespaced::<Workspace>(namespace)
                .get(runner.spec.workspace.as_ref())
                .await?;
            let mut owner_refs = runner.metadata.owner_references.clone().unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api_namespaced::<Runner>(namespace)
                .patch_json(
                    runner.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
