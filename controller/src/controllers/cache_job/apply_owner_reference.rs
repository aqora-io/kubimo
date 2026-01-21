use kubimo::{CacheJob, Workspace, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::CacheJobReconciler;

impl CacheJobReconciler {
    pub(crate) async fn apply_owner_reference(
        &self,
        ctx: &Context,
        cache_job: &CacheJob,
    ) -> Result<(), kubimo::Error> {
        let namespace = cache_job.require_namespace()?;
        if !cache_job
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == Workspace::kind(&())
                        && oref.name == cache_job.spec.workspace
                })
            })
        {
            let workspace = ctx
                .api_namespaced::<Workspace>(namespace)
                .get(cache_job.spec.workspace.as_ref())
                .await?;
            let mut owner_refs = cache_job
                .metadata
                .owner_references
                .clone()
                .unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api_namespaced::<CacheJob>(namespace)
                .patch_json(
                    cache_job.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
