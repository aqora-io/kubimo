use futures::prelude::*;
use kubimo::k8s_openapi::api::{
    core::v1::ServiceAccount,
    rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject},
};
use kubimo::kube::{CustomResourceExt, api::ObjectMeta};
use kubimo::{Workspace, WorkspaceDir, prelude::*};

use crate::context::Context;
use crate::controllers::indexer;

use super::WorkspaceReconciler;

impl WorkspaceReconciler {
    async fn apply_indexer_service_account(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let owner_references = Some(vec![workspace.static_controller_owner_ref()?]);
        let service_account_name = indexer::service_account_name(workspace_name);
        let service_account = ServiceAccount {
            metadata: ObjectMeta {
                name: Some(service_account_name.to_string()),
                namespace: workspace.metadata.namespace.clone(),
                owner_references,
                ..Default::default()
            },
            ..Default::default()
        };
        ctx.api_namespaced::<ServiceAccount>(namespace)
            .patch(&service_account)
            .await?;
        Ok(())
    }

    async fn apply_indexer_role(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let owner_references = Some(vec![workspace.static_controller_owner_ref()?]);
        let role_name = indexer::role_name(workspace_name);
        let crd = WorkspaceDir::crd();
        let role = Role {
            metadata: ObjectMeta {
                name: Some(role_name.to_string()),
                namespace: workspace.metadata.namespace.clone(),
                owner_references,
                ..Default::default()
            },
            rules: Some(vec![PolicyRule {
                api_groups: Some(vec![crd.spec.group]),
                resources: Some(vec![crd.spec.names.plural]),
                verbs: vec![
                    "get".to_string(),
                    "list".to_string(),
                    "watch".to_string(),
                    "create".to_string(),
                    "update".to_string(),
                    "patch".to_string(),
                    "delete".to_string(),
                ],
                ..Default::default()
            }]),
        };
        ctx.api_namespaced::<Role>(namespace).patch(&role).await?;
        Ok(())
    }

    async fn apply_indexer_role_binding(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        let workspace_name = workspace.name()?;
        let namespace = workspace.require_namespace()?;
        let owner_references = Some(vec![workspace.static_controller_owner_ref()?]);
        let role_name = indexer::role_name(workspace_name);
        let role_binding_name = indexer::role_binding_name(workspace_name);
        let service_account_name = indexer::service_account_name(workspace_name);
        let role_binding = RoleBinding {
            metadata: ObjectMeta {
                name: Some(role_binding_name.to_string()),
                namespace: workspace.metadata.namespace.clone(),
                owner_references,
                ..Default::default()
            },
            role_ref: RoleRef {
                api_group: "rbac.authorization.k8s.io".to_string(),
                kind: "Role".to_string(),
                name: role_name.to_string(),
            },
            subjects: Some(vec![Subject {
                api_group: None,
                kind: "ServiceAccount".to_string(),
                name: service_account_name.to_string(),
                namespace: Some(namespace.to_string()),
            }]),
        };
        ctx.api_namespaced::<RoleBinding>(namespace)
            .patch(&role_binding)
            .await?;
        Ok(())
    }

    pub(crate) async fn apply_indexer_rbac(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        if workspace.metadata.deletion_timestamp.is_some() {
            return Ok(());
        }
        futures::future::try_join_all(vec![
            self.apply_indexer_service_account(ctx, workspace).boxed(),
            self.apply_indexer_role(ctx, workspace).boxed(),
            self.apply_indexer_role_binding(ctx, workspace).boxed(),
        ])
        .await?;
        Ok(())
    }
}
