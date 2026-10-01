use std::collections::{BTreeMap, BTreeSet};

use kubimo::k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, IngressTLS, ServiceBackendPort,
};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kubimo::kube::api::ObjectMeta;
use kubimo::{Runner, RunnerIngress, prelude::*};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::Config;

#[inline]
pub(crate) fn ingress_path_from_name(name: &str) -> String {
    const ASCII_SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!("/{}", utf8_percent_encode(name, ASCII_SET))
}

pub fn ingress_path(runner: &Runner) -> kubimo::Result<String> {
    if let Some(path) = runner
        .spec
        .ingress
        .as_ref()
        .and_then(|ingress| ingress.path.as_ref())
    {
        Ok(path.clone())
    } else {
        Ok(ingress_path_from_name(runner.name()?))
    }
}

/// The path this runner is actually served under.
///
/// A claimed warm pod serves the base-url minted at its birth — marimo cannot
/// change it once booted — so the claim recorded in status overrides whatever
/// the spec asked for. Everything that routes or polls a live runner (the
/// Ingress, the status check) must use this, not [`ingress_path`].
pub fn effective_ingress_path(runner: &Runner) -> kubimo::Result<String> {
    if let Some(claim) = runner.status.as_ref().and_then(|s| s.claim.as_ref()) {
        return Ok(claim.ingress_path.clone());
    }
    ingress_path(runner)
}

pub(crate) struct IngressParams<'a> {
    pub name: &'a str,
    pub namespace: Option<String>,
    pub owner_reference: OwnerReference,
    pub path: String,
    pub service_name: &'a str,
    pub port: i32,
    /// A runner's own ingress settings; `None` takes the configured defaults.
    /// Its `path` is ignored — the caller resolves the path.
    pub ingress: Option<&'a RunnerIngress>,
}

/// The Ingress routing `path` to a runner's Service, shaped by the controller
/// config and, for a runner, its `spec.ingress`.
pub(crate) fn build_ingress(config: &Config, params: IngressParams) -> Ingress {
    let ingress_class_name = params
        .ingress
        .and_then(|ingress| ingress.class_name.clone())
        .unwrap_or_else(|| config.ingress_class_name.clone());
    let spec_tls = params.ingress.and_then(|ingress| ingress.tls.as_ref());
    let mut annotations = BTreeMap::new();
    annotations.insert(
        "kubernetes.io/ingress.class".to_string(),
        ingress_class_name.clone(),
    );
    // Keeps the kernel websocket and the code-mode SSE stream alive while a
    // cell runs; see `runner_proxy_timeout_secs` in the config.
    let proxy_timeout_secs = config.runner_proxy_timeout_secs.to_string();
    annotations.insert(
        "nginx.ingress.kubernetes.io/proxy-read-timeout".to_string(),
        proxy_timeout_secs.clone(),
    );
    annotations.insert(
        "nginx.ingress.kubernetes.io/proxy-send-timeout".to_string(),
        proxy_timeout_secs,
    );
    if let Some(cluster_issuer) = spec_tls
        .and_then(|tls| tls.cluster_issuer.as_ref())
        .or(config.cluster_issuer.as_ref())
    {
        annotations.insert(
            "cert-manager.io/cluster-issuer".to_string(),
            cluster_issuer.clone(),
        );
    }
    let mut hosts = config.runner_hosts.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(spec_hosts) = spec_tls.and_then(|tls| tls.hosts.as_ref()) {
        for host in spec_hosts {
            hosts.insert(host.clone());
        }
    }
    let (tls, hosts) = if hosts.is_empty() {
        (None, vec![None])
    } else {
        (
            Some(vec![IngressTLS {
                hosts: Some(hosts.iter().cloned().collect()),
                secret_name: Some(
                    spec_tls
                        .and_then(|tls| tls.secret_name.clone())
                        .unwrap_or_else(|| {
                            let mut tls_secret_name = String::new();
                            for hostname in &hosts {
                                tls_secret_name.push_str(
                                    hostname
                                        .to_lowercase()
                                        .replace(|ch: char| !ch.is_ascii_alphanumeric(), "-")
                                        .trim_start_matches('-'),
                                );
                                tls_secret_name.push('-');
                            }
                            tls_secret_name.push_str("tls");
                            tls_secret_name
                        }),
                ),
            }]),
            hosts.into_iter().map(Some).collect(),
        )
    };
    let rules = hosts
        .into_iter()
        .map(|host| IngressRule {
            host,
            http: Some(HTTPIngressRuleValue {
                paths: vec![HTTPIngressPath {
                    path: Some(params.path.clone()),
                    path_type: "Prefix".to_string(),
                    backend: IngressBackend {
                        service: Some(IngressServiceBackend {
                            name: params.service_name.to_string(),
                            port: Some(ServiceBackendPort {
                                number: Some(params.port),
                                ..Default::default()
                            }),
                        }),
                        ..Default::default()
                    },
                }],
            }),
        })
        .collect();
    Ingress {
        metadata: ObjectMeta {
            name: Some(params.name.to_string()),
            namespace: params.namespace,
            owner_references: Some(vec![params.owner_reference]),
            annotations: Some(annotations),
            ..Default::default()
        },
        spec: Some(IngressSpec {
            ingress_class_name: Some(ingress_class_name),
            tls,
            rules: Some(rules),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::RunnerTls;

    fn params<'a>(ingress: Option<&'a RunnerIngress>) -> IngressParams<'a> {
        IngressParams {
            name: "bmor-x",
            namespace: Some("platform".into()),
            owner_reference: OwnerReference {
                kind: "Runner".into(),
                name: "bmor-x".into(),
                ..Default::default()
            },
            path: "/runner/abc".into(),
            service_name: "bmor-x",
            port: 80,
            ingress,
        }
    }

    /// Pins the shape every runner Ingress has had: class and timeout
    /// annotations, the issuer, one rule per host (configured hosts plus the
    /// spec's), and a TLS secret named after the sorted hosts.
    #[test]
    fn a_runner_ingress_routes_every_host_to_its_service() {
        let mut config = Config::test_default();
        config.runner_hosts = vec!["kubimo.org".into()];
        config.cluster_issuer = Some("letsencrypt".into());
        let spec = RunnerIngress {
            path: Some("/ignored".into()),
            tls: Some(RunnerTls {
                hosts: Some(vec!["extra.kubimo.org".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ingress = build_ingress(&config, params(Some(&spec)));

        let annotations = ingress.metadata.annotations.as_ref().unwrap();
        assert_eq!(annotations["kubernetes.io/ingress.class"], "nginx");
        assert_eq!(
            annotations["nginx.ingress.kubernetes.io/proxy-read-timeout"],
            "3600"
        );
        assert_eq!(
            annotations["nginx.ingress.kubernetes.io/proxy-send-timeout"],
            "3600"
        );
        assert_eq!(annotations["cert-manager.io/cluster-issuer"], "letsencrypt");
        assert_eq!(
            ingress.metadata.owner_references.as_ref().unwrap()[0].kind,
            "Runner"
        );

        let spec = ingress.spec.as_ref().unwrap();
        assert_eq!(spec.ingress_class_name.as_deref(), Some("nginx"));
        let tls = &spec.tls.as_ref().unwrap()[0];
        assert_eq!(
            tls.hosts.as_deref().unwrap(),
            ["extra.kubimo.org", "kubimo.org"]
        );
        assert_eq!(
            tls.secret_name.as_deref(),
            Some("extra-kubimo-org-kubimo-org-tls")
        );
        let rules = spec.rules.as_ref().unwrap();
        assert_eq!(
            rules.iter().map(|r| r.host.as_deref()).collect::<Vec<_>>(),
            [Some("extra.kubimo.org"), Some("kubimo.org")]
        );
        for rule in rules {
            let path = &rule.http.as_ref().unwrap().paths[0];
            assert_eq!(path.path.as_deref(), Some("/runner/abc"));
            assert_eq!(path.path_type, "Prefix");
            let backend = path.backend.service.as_ref().unwrap();
            assert_eq!(backend.name, "bmor-x");
            assert_eq!(backend.port.as_ref().unwrap().number, Some(80));
        }
    }

    /// Without hosts the Ingress has one host-less rule and no TLS; a spec
    /// secret name wins over the derived one.
    #[test]
    fn hostless_and_named_tls_ingresses() {
        let config = Config::test_default();
        let ingress = build_ingress(&config, params(None));
        let spec = ingress.spec.as_ref().unwrap();
        assert!(spec.tls.is_none());
        assert_eq!(spec.rules.as_ref().unwrap()[0].host, None);

        let named = RunnerIngress {
            tls: Some(RunnerTls {
                hosts: Some(vec!["kubimo.org".into()]),
                secret_name: Some("wildcard-tls".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ingress = build_ingress(&config, params(Some(&named)));
        assert_eq!(
            ingress.spec.unwrap().tls.unwrap()[0].secret_name.as_deref(),
            Some("wildcard-tls")
        );
    }
}
