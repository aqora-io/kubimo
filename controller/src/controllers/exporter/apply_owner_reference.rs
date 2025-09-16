use kubimo::{Exporter, Workspace, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::ExporterReconciler;

impl ExporterReconciler {
    pub(crate) async fn apply_owner_reference(
        &self,
        ctx: &Context,
        exporter: &Exporter,
    ) -> Result<(), kubimo::Error> {
        let namespace = exporter.require_namespace()?;
        if !exporter
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == Workspace::kind(&())
                        && oref.name == exporter.spec.workspace
                })
            })
        {
            let workspace = ctx
                .api_namespaced::<Workspace>(namespace)
                .get(exporter.spec.workspace.as_ref())
                .await?;
            let mut owner_refs = exporter
                .metadata
                .owner_references
                .clone()
                .unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api_namespaced::<Exporter>(namespace)
                .patch_json(
                    exporter.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
