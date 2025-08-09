use kubimo::k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{KubimoRunner, prelude::*};

use crate::context::Context;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_ingress(
        &self,
        ctx: &Context,
        runner: &KubimoRunner,
    ) -> Result<Ingress, kubimo::Error> {
        let svc = Ingress {
            metadata: ObjectMeta {
                name: runner.metadata.name.clone(),
                namespace: runner.metadata.namespace.clone(),
                owner_references: Some(vec![runner.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                ingress_class_name: Some("nginx".to_string()),
                rules: Some(vec![IngressRule {
                    http: Some(HTTPIngressRuleValue {
                        paths: vec![HTTPIngressPath {
                            path: Some(self.ingress_path(runner)?),
                            path_type: "Prefix".to_string(),
                            backend: IngressBackend {
                                service: Some(IngressServiceBackend {
                                    name: runner.name()?.to_string(),
                                    port: Some(ServiceBackendPort {
                                        number: Some(80),
                                        ..Default::default()
                                    }),
                                }),
                                ..Default::default()
                            },
                        }],
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api::<Ingress>().patch(&svc).await
    }
}
