use async_graphql::*;

use crate::{crd::KubimoWorkspace, id::Id, service::ContextServiceExt};

use super::workspace::Workspace;

#[derive(Clone, Debug, Interface)]
#[graphql(field(name = "id", ty = "Id"))]
pub enum Node {
    Workspace(Workspace),
}

impl From<KubimoWorkspace> for Node {
    fn from(workspace: KubimoWorkspace) -> Self {
        Node::Workspace(workspace.into())
    }
}

#[derive(Default)]
pub struct NodeQuery;

#[Object]
impl NodeQuery {
    pub async fn node(&self, ctx: &Context<'_>, id: Id) -> Result<Option<Node>> {
        Ok(match id {
            Id::Workspace(name) => ctx
                .service::<KubimoWorkspace>()?
                .get_opt(&name)
                .await?
                .map(|workspace| Node::Workspace(workspace.into())),
        })
    }
}
