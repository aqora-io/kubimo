//! Gateway API routing for runners and warm pods.
//!
//! The `HTTPRoute` type here is the subset of `gateway.networking.k8s.io/v1`
//! the controller writes, rather than the `gateway-api` crate's: that one
//! would bump `Cargo.lock`, which the agent image is tagged by, and roll every
//! node's agent for a controller-only change. It is never installed as a CRD
//! (it is not in `all_crds`); the Gateway API CRDs come with the gateway.

use kubimo::kube::CustomResource;
use kubimo::kube::api::ObjectMeta;
use serde::{Deserialize, Serialize};

use crate::config::{Config, GatewayRef};
use crate::context::Context;
use crate::controllers::ingress::{RouteParams, route_hosts};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[kube(
    group = "gateway.networking.k8s.io",
    version = "v1",
    kind = "HTTPRoute",
    plural = "httproutes",
    namespaced,
    schema = "disabled",
    derive = "PartialEq",
    crates(
        kube = "::kubimo::kube",
        kube_core = "::kubimo::kube::core",
        k8s_openapi = "::kubimo::k8s_openapi"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_refs: Vec<ParentReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hostnames: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<HttpRouteRule>,
}

/// A Gateway the route attaches to. Group and kind are left to their
/// defaults (a Gateway), so server-side apply never claims them.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteRule {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<HttpRouteMatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backend_refs: Vec<HttpBackendRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<HttpRouteTimeouts>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<HttpPathMatch>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpPathMatch {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// A Service backend. Group, kind and weight are left to their defaults (a
/// core Service, weight 1).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpBackendRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteTimeouts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
}

/// The HTTPRoute twin of [`crate::controllers::ingress::build_ingress`]: the
/// same name, owner, hosts and path prefix, to the same Service, attached to
/// the configured Gateway. Ingress-only settings (class, issuer, TLS secret)
/// don't apply; the Gateway's listeners own TLS.
pub(crate) fn build_http_route(
    config: &Config,
    gateway: &GatewayRef,
    params: &RouteParams,
) -> HTTPRoute {
    HTTPRoute {
        metadata: ObjectMeta {
            name: Some(params.name.to_string()),
            namespace: params.namespace.clone(),
            owner_references: Some(vec![params.owner_reference.clone()]),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            parent_refs: vec![ParentReference {
                name: gateway.name.clone(),
                namespace: Some(gateway.namespace.clone()),
                section_name: gateway.section_name.clone(),
            }],
            hostnames: route_hosts(config, params.ingress).into_iter().collect(),
            rules: vec![HttpRouteRule {
                matches: vec![HttpRouteMatch {
                    path: Some(HttpPathMatch {
                        type_: Some("PathPrefix".to_string()),
                        value: Some(params.path.clone()),
                    }),
                }],
                backend_refs: vec![HttpBackendRef {
                    name: params.service_name.to_string(),
                    port: Some(params.port),
                }],
                // "0s" disables the request timeout. Envoy's default of 15 s
                // would cut the code-mode SSE stream and any long request;
                // idle connections are bounded by the Gateway's traffic
                // policies instead.
                timeouts: Some(HttpRouteTimeouts {
                    request: Some("0s".to_string()),
                }),
            }],
        },
    }
}

/// Apply a route's HTTPRoute when a Gateway is configured; without one, do
/// nothing.
pub(crate) async fn apply_http_route(
    ctx: &Context,
    namespace: &str,
    params: &RouteParams<'_>,
) -> kubimo::Result<()> {
    let Some(gateway) = &ctx.config.gateway else {
        return Ok(());
    };
    ctx.api_namespaced::<HTTPRoute>(namespace)
        .patch(&build_http_route(&ctx.config, gateway, params))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    use kubimo::{RunnerIngress, RunnerTls};

    fn gateway(section_name: Option<&str>) -> GatewayRef {
        GatewayRef {
            name: "kubimo".into(),
            namespace: "envoy-gateway-system".into(),
            section_name: section_name.map(Into::into),
        }
    }

    fn params<'a>(ingress: Option<&'a RunnerIngress>) -> RouteParams<'a> {
        RouteParams {
            name: "bmor-x",
            namespace: Some("platform".into()),
            owner_reference: OwnerReference {
                api_version: "kubimo.aqora.io/v1".into(),
                kind: "Runner".into(),
                name: "bmor-x".into(),
                uid: "11111111-2222-3333-4444-555555555555".into(),
                controller: Some(true),
                ..Default::default()
            },
            path: "/runner/abc".into(),
            service_name: "bmor-x",
            port: 80,
            ingress,
        }
    }

    /// Every host the Ingress would answer on, configured and the spec's,
    /// routes the path prefix to the runner's Service.
    #[test]
    fn a_runner_route_routes_every_host_to_its_service() {
        let mut config = Config::test_default();
        config.runner_hosts = vec!["kubimo.org".into()];
        let spec = RunnerIngress {
            path: Some("/ignored".into()),
            tls: Some(RunnerTls {
                hosts: Some(vec!["extra.kubimo.org".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let route = build_http_route(&config, &gateway(Some("https")), &params(Some(&spec)));

        assert_eq!(route.metadata.name.as_deref(), Some("bmor-x"));
        assert_eq!(route.metadata.namespace.as_deref(), Some("platform"));
        assert_eq!(
            route.metadata.owner_references.as_ref().unwrap()[0].kind,
            "Runner"
        );
        assert_eq!(route.spec.hostnames, ["extra.kubimo.org", "kubimo.org"]);
        assert_eq!(
            route.spec.parent_refs,
            [ParentReference {
                name: "kubimo".into(),
                namespace: Some("envoy-gateway-system".into()),
                section_name: Some("https".into()),
            }]
        );
        let rule = &route.spec.rules[0];
        let path = rule.matches[0].path.as_ref().unwrap();
        assert_eq!(path.type_.as_deref(), Some("PathPrefix"));
        assert_eq!(path.value.as_deref(), Some("/runner/abc"));
        assert_eq!(
            rule.backend_refs,
            [HttpBackendRef {
                name: "bmor-x".into(),
                port: Some(80),
            }]
        );
    }

    /// The exact server-side apply body: no nulls, no status, no defaulted
    /// group/kind/weight, and nothing for unset hosts or section.
    #[test]
    fn the_apply_body_sets_only_what_the_controller_owns() {
        let config = Config::test_default();
        let route = build_http_route(&config, &gateway(None), &params(None));
        assert_eq!(
            serde_json::to_value(&route).unwrap(),
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "HTTPRoute",
                "metadata": {
                    "name": "bmor-x",
                    "namespace": "platform",
                    "ownerReferences": [{
                        "apiVersion": "kubimo.aqora.io/v1",
                        "kind": "Runner",
                        "name": "bmor-x",
                        "uid": "11111111-2222-3333-4444-555555555555",
                        "controller": true,
                    }],
                },
                "spec": {
                    "parentRefs": [{
                        "name": "kubimo",
                        "namespace": "envoy-gateway-system",
                    }],
                    "rules": [{
                        "matches": [{
                            "path": { "type": "PathPrefix", "value": "/runner/abc" },
                        }],
                        "backendRefs": [{ "name": "bmor-x", "port": 80 }],
                        "timeouts": { "request": "0s" },
                    }],
                },
            })
        );
    }

    /// Class, issuer and TLS secret shape only the Ingress.
    #[test]
    fn ingress_only_settings_do_not_change_the_route() {
        let mut config = Config::test_default();
        config.runner_hosts = vec!["kubimo.org".into()];
        let plain = build_http_route(&config, &gateway(None), &params(None));
        let spec = RunnerIngress {
            class_name: Some("traefik".into()),
            tls: Some(RunnerTls {
                cluster_issuer: Some("letsencrypt".into()),
                secret_name: Some("wildcard-tls".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        config.cluster_issuer = Some("letsencrypt".into());
        config.ingress_class_name = "traefik".into();
        config.runner_proxy_timeout_secs = 60;
        assert_eq!(
            build_http_route(&config, &gateway(None), &params(Some(&spec))),
            plain
        );
    }

    /// The API server answers an apply with defaulted fields and a status
    /// this type doesn't model; they must not fail the read.
    #[test]
    fn a_server_response_with_extra_fields_deserializes() {
        let response = serde_json::json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "bmor-x", "namespace": "platform", "generation": 1 },
            "spec": {
                "parentRefs": [{
                    "group": "gateway.networking.k8s.io",
                    "kind": "Gateway",
                    "name": "kubimo",
                    "namespace": "envoy-gateway-system",
                }],
                "rules": [{
                    "matches": [{ "path": { "type": "PathPrefix", "value": "/runner/abc" } }],
                    "backendRefs": [{
                        "group": "",
                        "kind": "Service",
                        "name": "bmor-x",
                        "port": 80,
                        "weight": 1,
                    }],
                    "timeouts": { "request": "0s" },
                }],
            },
            "status": { "parents": [{ "conditions": [] }] },
        });
        let route: HTTPRoute = serde_json::from_value(response).unwrap();
        assert_eq!(route.spec.rules[0].backend_refs[0].name, "bmor-x");
        assert_eq!(route.spec.parent_refs[0].name, "kubimo");
    }
}
