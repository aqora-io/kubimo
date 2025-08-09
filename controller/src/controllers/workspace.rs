use std::sync::Arc;

use futures::Stream;
use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
use kubimo::kube::{
    api::ObjectMeta,
    runtime::{Controller, controller::Action},
};
use kubimo::{KubimoWorkspace, prelude::*};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};
use crate::resources::{ResourceRequirement, Resources};

#[derive(Debug, Clone, Copy)]
struct WorkspaceReconciler;

#[async_trait::async_trait]
impl Reconciler for WorkspaceReconciler {
    type Resource = KubimoWorkspace;
    type Error = kubimo::Error;

    async fn apply(
        &self,
        ctx: &Context,
        workspace: &KubimoWorkspace,
    ) -> Result<Action, Self::Error> {
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: workspace.metadata.name.clone(),
                namespace: workspace.metadata.namespace.clone(),
                owner_references: Some(vec![workspace.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteMany".to_string()]),
                resources: Resources {
                    requests: ResourceRequirement {
                        storage: workspace.spec.min_storage.clone(),
                        ..Default::default()
                    },
                    limits: ResourceRequirement {
                        storage: workspace.spec.max_storage.clone(),
                        ..Default::default()
                    },
                }
                .into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.client
            .api::<PersistentVolumeClaim>()
            .patch(&pvc)
            .await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<KubimoWorkspace, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let bmows = ctx.api::<KubimoWorkspace>().kube().clone();
    let pvc = ctx.api::<PersistentVolumeClaim>().kube().clone();
    Ok(Controller::new(bmows, Default::default())
        .owns(pvc, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            WorkspaceReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
