use async_graphql::*;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use crate::{
    crd::{KubimoWorkspace, KubimoWorkspaceSpec},
    graphql::query::workspace::Workspace,
    id::{Id, ResourceFactoryExt},
    service::ContextServiceExt,
};

#[derive(Clone, Debug, InputObject)]
pub struct CreateWorkspaceInput {
    storage: String,
    storage_class_name: Option<String>,
}

impl From<CreateWorkspaceInput> for KubimoWorkspace {
    fn from(input: CreateWorkspaceInput) -> Self {
        KubimoWorkspace::create(KubimoWorkspaceSpec {
            storage: Quantity(input.storage),
            storage_class_name: input.storage_class_name,
        })
    }
}

#[derive(Clone, Debug, Default, InputObject)]
pub struct UpdateWorkspaceInput {
    storage: Option<String>,
    storage_class_name: MaybeUndefined<String>,
}

impl UpdateWorkspaceInput {
    fn update(self, workspace: &mut KubimoWorkspace) {
        if let Some(storage) = self.storage {
            workspace.spec.storage = Quantity(storage);
        }
        match self.storage_class_name {
            MaybeUndefined::Value(storage_class) => {
                workspace.spec.storage_class_name = Some(storage_class);
            }
            MaybeUndefined::Undefined => {}
            MaybeUndefined::Null => {
                workspace.spec.storage_class_name = None;
            }
        }
    }
}

#[derive(Default)]
pub struct WorkspaceMutation;

#[Object]
impl WorkspaceMutation {
    pub async fn create_workspace(
        &self,
        ctx: &Context<'_>,
        input: CreateWorkspaceInput,
    ) -> Result<Workspace> {
        Ok(ctx
            .service::<KubimoWorkspace>()?
            .create(&input.into())
            .await?
            .into())
    }

    pub async fn update_workspace(
        &self,
        ctx: &Context<'_>,
        id: Id,
        input: UpdateWorkspaceInput,
    ) -> Result<Workspace> {
        let name = id.name();
        let service = ctx.service::<KubimoWorkspace>()?;
        let mut workspace = service.get(name).await?;
        input.update(&mut workspace);
        Ok(service.update(name, &workspace).await?.into())
    }

    pub async fn delete_workspace(&self, ctx: &Context<'_>, id: Id) -> Result<Option<Workspace>> {
        Ok(ctx
            .service::<KubimoWorkspace>()?
            .delete(id.name())
            .await?
            .map(Workspace::from))
    }
}
