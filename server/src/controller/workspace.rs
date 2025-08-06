use std::sync::Arc;

use futures::Stream;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, VolumeResourceRequirements};
use kube::{
    api::Resource,
    runtime::{Controller, controller::Action},
};
use thiserror::Error;

use crate::{
    crd::{KubimoWorkspace, KumimoWorkspaceStatus, StatusError},
    service::Service,
};

use super::context::{ControllerContext, ControllerResult};

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("MissingObjectKey: {0}")]
    MissingObjectKey(&'static str),
}

impl From<&Error> for StatusError {
    fn from(err: &Error) -> Self {
        match err {
            Error::Kube(e) => StatusError::from(e),
            _ => StatusError::new(err),
        }
    }
}

async fn reconcile(
    workspace: Arc<KubimoWorkspace>,
    ctx: Arc<ControllerContext>,
) -> Result<Action, Error> {
    let name = workspace
        .metadata
        .name
        .as_ref()
        .ok_or_else(|| Error::MissingObjectKey(".metadata.name"))?
        .clone();
    let service = ctx.service::<PersistentVolumeClaim>();
    match impl_reconcile(service, name.clone(), workspace.clone()).await {
        Ok(action) => {
            ctx.service::<KubimoWorkspace>()
                .patch_status(
                    &name,
                    &KumimoWorkspaceStatus {
                        reconciliation_error: None,
                    },
                )
                .await?;
            Ok(action)
        }
        Err(err) => {
            let status_error = StatusError::from(&err);
            ctx.service::<KubimoWorkspace>()
                .patch_status(
                    &name,
                    &KumimoWorkspaceStatus {
                        reconciliation_error: Some(status_error),
                    },
                )
                .await?;
            Err(err)
        }
    }
}

async fn impl_reconcile(
    service: Service<'_, PersistentVolumeClaim>,
    name: String,
    workspace: Arc<KubimoWorkspace>,
) -> Result<Action, Error> {
    let oref = workspace.controller_owner_ref(&()).unwrap();
    let pvc = PersistentVolumeClaim {
        metadata: kube::api::ObjectMeta {
            name: Some(name.clone()),
            namespace: workspace.metadata.namespace.clone(),
            owner_references: Some(vec![oref]),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteMany".to_string()]),
            storage_class_name: workspace.spec.storage_class_name.clone(),
            resources: Some(VolumeResourceRequirements {
                requests: Some(
                    std::iter::once(("storage".to_string(), workspace.spec.storage.clone()))
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    service.patch(&name, &pvc).await?;
    Ok(Action::await_change())
}

fn error_policy(
    _object: Arc<KubimoWorkspace>,
    _error: &Error,
    _ctx: Arc<ControllerContext>,
) -> Action {
    Action::await_change()
}

pub fn controller(ctx: &ControllerContext) -> Controller<KubimoWorkspace> {
    let kws = kube::Api::<KubimoWorkspace>::default_namespaced(ctx.client.clone());
    let pvcs = kube::Api::<PersistentVolumeClaim>::default_namespaced(ctx.client.clone());
    Controller::new(kws, Default::default()).owns(pvcs, Default::default())
}

pub fn run(
    ctx: Arc<ControllerContext>,
    controller: Controller<KubimoWorkspace>,
) -> impl Stream<Item = ControllerResult<KubimoWorkspace, Error>> {
    controller.run(reconcile, error_policy, ctx)
}
