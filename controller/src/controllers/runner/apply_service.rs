use kubimo::k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use kubimo::kube::api::ObjectMeta;
use kubimo::{KubimoRunner, prelude::*};

use crate::context::Context;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_service(
        &self,
        ctx: &Context,
        runner: &KubimoRunner,
    ) -> Result<Service, kubimo::Error> {
        let svc = Service {
            metadata: ObjectMeta {
                name: runner.metadata.name.clone(),
                namespace: runner.metadata.namespace.clone(),
                owner_references: Some(vec![runner.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(self.pod_labels(runner)?),
                ports: Some(vec![ServicePort {
                    name: Some("marimo".to_string()),
                    port: Self::PORT,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api::<Service>().patch(&svc).await
    }
}
