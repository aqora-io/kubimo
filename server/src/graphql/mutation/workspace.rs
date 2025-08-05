use async_graphql::*;

use crate::{
    crd::KubimoWorkspace, graphql::query::workspace::Workspace, id::Id, service::ContextServiceExt,
};

#[derive(Default)]
pub struct WorkspaceMutation;

#[Object]
impl WorkspaceMutation {
    pub async fn create_workspace(&self, ctx: &Context<'_>) -> Result<Workspace> {
        Ok(ctx
            .service::<KubimoWorkspace>()?
            .create(Default::default())
            .await?
            .into())
    }

    pub async fn delete_workspace(&self, ctx: &Context<'_>, id: Id) -> Result<Option<Workspace>> {
        Ok(ctx
            .service::<KubimoWorkspace>()?
            .delete(id.name())
            .await?
            .map(Workspace::from))
    }
}
