use kubimo::{KubimoRunner, KubimoWorkspace, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_owner_references(
        &self,
        ctx: &Context,
        runner: &KubimoRunner,
    ) -> Result<(), kubimo::Error> {
        if !runner
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == KubimoWorkspace::kind(&())
                        && oref.name == runner.spec.workspace
                })
            })
        {
            let workspace = ctx
                .api::<KubimoWorkspace>()
                .get(runner.spec.workspace.as_ref())
                .await?;
            let mut owner_refs = runner.metadata.owner_references.clone().unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api::<KubimoRunner>()
                .patch_json(
                    runner.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
