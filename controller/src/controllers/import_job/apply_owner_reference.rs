use kubimo::{ImportJob, Workspace, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::ImportJobReconciler;

impl ImportJobReconciler {
    pub(crate) async fn apply_owner_reference(
        &self,
        ctx: &Context,
        import_job: &ImportJob,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let namespace = import_job.require_namespace()?;
        if !import_job
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == Workspace::kind(&())
                        && oref.name == import_job.spec.workspace
                })
            })
        {
            let mut owner_refs = import_job
                .metadata
                .owner_references
                .clone()
                .unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api_namespaced::<ImportJob>(namespace)
                .patch_json(
                    import_job.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
