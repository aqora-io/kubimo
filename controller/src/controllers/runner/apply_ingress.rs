use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, IngressTLS, ServiceBackendPort,
};
use kubimo::kube::api::ObjectMeta;
use kubimo::{Runner, prelude::*};

use crate::context::Context;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_ingress(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<Ingress, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let ingress_class_name = runner
            .spec
            .ingress
            .as_ref()
            .and_then(|ingress| ingress.class_name.clone())
            .unwrap_or_else(|| ctx.config.ingress_class_name.clone());
        let tls = runner
            .spec
            .ingress
            .as_ref()
            .and_then(|ingress| ingress.tls.as_ref());
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "kubernetes.io/ingress.class".to_string(),
            ingress_class_name.clone(),
        );
        if let Some(tls) = tls {
            annotations.insert(
                "cert-manager.io/cluster-issuer".to_string(),
                tls.cluster_issuer.clone(),
            );
        }
        let svc = Ingress {
            metadata: ObjectMeta {
                name: runner.metadata.name.clone(),
                namespace: runner.metadata.namespace.clone(),
                owner_references: Some(vec![runner.static_controller_owner_ref()?]),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                ingress_class_name: Some(ingress_class_name),
                tls: tls.map(|tls| {
                    vec![IngressTLS {
                        hosts: Some(vec![tls.host.clone()]),
                        secret_name: Some(tls.secret_name()),
                    }]
                }),
                rules: Some(vec![IngressRule {
                    host: tls.map(|tls| tls.host.clone()),
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
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<Ingress>(namespace).patch(&svc).await
    }
}
