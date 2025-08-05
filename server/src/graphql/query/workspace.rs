use async_graphql::*;

use crate::{
    crd::KubimoWorkspace,
    id::Id,
    service::{ContextServiceExt, ListParams, ObjectListConnection, ObjectListExt},
};

#[derive(Clone, Debug)]
pub struct Workspace(KubimoWorkspace);

impl From<KubimoWorkspace> for Workspace {
    fn from(workspace: KubimoWorkspace) -> Self {
        Workspace(workspace)
    }
}

impl From<Workspace> for KubimoWorkspace {
    fn from(workspace: Workspace) -> Self {
        workspace.0
    }
}

#[Object]
impl Workspace {
    pub async fn id(&self) -> Result<Id> {
        Ok(Id::Workspace(
            self.0
                .metadata
                .name
                .clone()
                .ok_or_else(|| "Workspace ID not found".to_string())?,
        ))
    }
}

#[derive(Default)]
pub struct WorkspaceQuery;

#[Object]
impl WorkspaceQuery {
    pub async fn workspaces(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 100)] first: u32,
        after: Option<String>,
    ) -> Result<ObjectListConnection<Workspace>> {
        Ok(ctx
            .service::<KubimoWorkspace>()?
            .list(&ListParams {
                limit: Some(first),
                continue_token: after,
                ..Default::default()
            })
            .await?
            .connection())
    }
}
