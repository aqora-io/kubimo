use serde::{Deserialize, Serialize};
use url::Url;

#[inline]
fn default_manager_name() -> String {
    "kubimo-controller".to_string()
}

#[inline]
fn default_marimo_image() -> String {
    concat!("ghcr.io/aqora-io/kubimo-marimo:", env!("CARGO_PKG_VERSION")).to_string()
}

#[inline]
fn default_busybox_image() -> String {
    "busybox:1.36.1".to_string()
}

#[inline]
fn default_ingress_class_name() -> String {
    "nginx".to_string()
}

#[inline]
fn default_runner_status_check_interval_secs() -> u64 {
    10
}

/// Both of a runner's long-lived streams — the kernel websocket and the
/// code-mode SSE endpoint (`/api/kernel/execute`, what `marimo pair` drives) —
/// carry no traffic for as long as a cell takes to run. ingress-nginx defaults
/// its proxy timeouts to 60s, which cuts the connection mid-cell; marimo's
/// editor papers over it by reconnecting, but an agent just loses its result.
#[inline]
fn default_runner_proxy_timeout_secs() -> u32 {
    3600
}

#[cfg(feature = "metrics")]
#[inline]
fn default_metrics_enabled() -> bool {
    true
}

#[cfg(feature = "metrics")]
#[inline]
fn default_metrics_bind_addr() -> std::net::SocketAddr {
    ([0, 0, 0, 0], 9090).into()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "method")]
pub enum StatusCheckResolution {
    #[default]
    ServiceDns,
    Ingress {
        host: Url,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCheck {
    #[serde(default)]
    pub resolution: StatusCheckResolution,
    #[serde(default = "default_runner_status_check_interval_secs")]
    pub interval_secs: u64,
}

impl Default for StatusCheck {
    fn default() -> Self {
        Self {
            resolution: Default::default(),
            interval_secs: default_runner_status_check_interval_secs(),
        }
    }
}

#[cfg(feature = "metrics")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
    #[serde(default = "default_metrics_bind_addr")]
    pub bind_addr: std::net::SocketAddr,
}

#[cfg(feature = "metrics")]
impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            bind_addr: default_metrics_bind_addr(),
        }
    }
}

fn deserialize_hosts<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let hosts: Vec<String> = Deserialize::deserialize(deserializer)?;
    for host in hosts.iter() {
        match url::Host::parse(host).map_err(serde::de::Error::custom)? {
            url::Host::Domain(_) => {}
            url::Host::Ipv4(_) | url::Host::Ipv6(_) => {
                return Err(serde::de::Error::custom(
                    "runner_hosts must contain domain names, not IP addresses",
                ));
            }
        }
    }
    Ok(hosts)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_manager_name")]
    pub manager_name: String,
    #[serde(default = "default_marimo_image")]
    pub marimo_image: String,
    #[serde(default = "default_busybox_image")]
    pub busybox_image: String,
    #[serde(default = "default_ingress_class_name")]
    pub ingress_class_name: String,
    #[serde(default, deserialize_with = "deserialize_hosts")]
    pub runner_hosts: Vec<String>,
    #[serde(default)]
    pub cluster_issuer: Option<String>,
    #[serde(default = "default_runner_proxy_timeout_secs")]
    pub runner_proxy_timeout_secs: u32,
    #[serde(default)]
    pub runner_status: StatusCheck,
    #[cfg(feature = "metrics")]
    #[serde(default)]
    pub metrics: MetricsConfig,
}

impl Config {
    pub fn load() -> Result<Config, config::ConfigError> {
        config::Config::builder()
            .add_source(
                config::Environment::with_prefix("KUBIMO")
                    .separator("__")
                    .try_parsing(true)
                    .list_separator(",")
                    .with_list_parse_key("runner_hosts"),
            )
            .build()?
            .try_deserialize()
    }
}
