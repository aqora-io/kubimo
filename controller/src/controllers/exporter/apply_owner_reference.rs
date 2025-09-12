use kubimo::{KubimoExporter, KubimoWorkspace, json_patch_macros::*, prelude::*};

use crate::context::Context;

use super::ExporterReconciler;

impl ExporterReconciler {
    pub(crate) async fn apply_owner_reference(
        &self,
        ctx: &Context,
        exporter: &KubimoExporter,
    ) -> Result<(), kubimo::Error> {
        if !exporter
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|orefs| {
                orefs.iter().any(|oref| {
                    oref.controller.is_some_and(|yes| yes)
                        && oref.kind == KubimoWorkspace::kind(&())
                        && oref.name == exporter.spec.workspace
                })
            })
        {
            let workspace = ctx
                .api::<KubimoWorkspace>()
                .get(exporter.spec.workspace.as_ref())
                .await?;
            let mut owner_refs = exporter
                .metadata
                .owner_references
                .clone()
                .unwrap_or_default();
            owner_refs.push(workspace.static_controller_owner_ref()?);
            ctx.api::<KubimoExporter>()
                .patch_json(
                    exporter.name()?,
                    patch![add!(["metadata", "ownerReferences"] => owner_refs)],
                )
                .await?;
        }
        Ok(())
    }
}
