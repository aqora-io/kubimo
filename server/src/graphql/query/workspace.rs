use async_graphql::*;

use crate::{
    crd::KubimoWorkspace,
    id::Id,
    service::{ContextServiceExt, ListParams, ObjectListConnection, ObjectListExt},
};

#[derive(Clone, Debug)]
pub struct Workspace(KubimoWorkspace);

impl Workspace {
    #[inline]
    pub fn inner(&self) -> &KubimoWorkspace {
        &self.0
    }

    pub fn name(&self) -> Result<&str> {
        Ok(self
            .inner()
            .metadata
            .name
            .as_ref()
            .ok_or_else(|| "Workspace ID not found".to_string())?)
    }
}

impl From<KubimoWorkspace> for Workspace {
    fn from(workspace: KubimoWorkspace) -> Self {
        Workspace(workspace)
    }
}

#[Object]
impl Workspace {
    pub async fn id(&self) -> Result<Id> {
        Ok(Id::Workspace(self.name()?.into()))
    }

    pub async fn storage(&self) -> &str {
        &self.inner().spec.storage.0
    }

    pub async fn reconciliation_error(&self) -> Option<&str> {
        self.inner()
            .status
            .as_ref()
            .and_then(|status| status.reconciliation_error.as_ref())
            .map(|err| err.message.as_str())
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
