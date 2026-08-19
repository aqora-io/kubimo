use kubimo::{WorkspaceMode, WorkspacePythonRuntime};
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
fn default_marimo_conda_image() -> String {
    concat!(
        "ghcr.io/aqora-io/kubimo-marimo-conda:",
        env!("CARGO_PKG_VERSION")
    )
    .to_string()
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
    #[serde(default = "default_marimo_conda_image")]
    pub marimo_conda_image: String,
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
    /// Base path of the shared static-asset origin (the chart's
    /// `staticAssets`), e.g. `/marimo-assets`. When set, every runner pod gets
    /// `KUBIMO_ASSET_URL={base}/{image tag}` and marimo serves its frontend
    /// from there instead of the runner's own (per-claim, cache-busting) path
    /// prefix. Unset, nothing changes. Only enable once the path is routed to
    /// the static-assets Service in every environment fronting the runners.
    #[serde(default)]
    pub runner_asset_base_path: Option<String>,
    #[serde(default)]
    pub runner_status: StatusCheck,
    #[cfg(feature = "metrics")]
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// Mode given to workspaces that have not yet materialized `status.mode`.
    /// Changing this only affects new workspaces: existing ones pin their mode
    /// in status on first reconcile, so flipping this is reversible.
    #[serde(default)]
    pub default_workspace_mode: WorkspaceMode,
}

impl Config {
    fn environment_source() -> config::Environment {
        config::Environment::with_prefix("KUBIMO")
            .separator("__")
            .try_parsing(true)
            .list_separator(",")
            .with_list_parse_key("runner_hosts")
    }

    pub fn load() -> Result<Config, config::ConfigError> {
        config::Config::builder()
            .add_source(Self::environment_source())
            .build()?
            .try_deserialize()
    }

    pub fn marimo_image(&self, python_runtime: WorkspacePythonRuntime) -> &str {
        match python_runtime {
            WorkspacePythonRuntime::Uv => &self.marimo_image,
            WorkspacePythonRuntime::Conda => &self.marimo_conda_image,
        }
    }

    /// The shared asset URL for pods of `image`, when
    /// `runner_asset_base_path` is set: `{base}/{tag}`. The image tag is the
    /// cache key — all pods of one image serve byte-identical assets, and the
    /// static-assets server publishes them under the same tag (the chart
    /// derives it with the same text-after-last-colon rule; keep the two in
    /// agreement).
    pub fn runner_asset_url(&self, image: &str) -> Option<String> {
        let base = self
            .runner_asset_base_path
            .as_deref()?
            .trim_end_matches('/');
        Some(format!("{base}/{tag}", tag = image_tag(image)))
    }
}

/// The tag of an image reference: text after the last `:`, ignoring any
/// `@sha256:...` digest. An untagged reference gets `latest`, matching what
/// the container runtime pulls.
fn image_tag(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    match image.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') => tag,
        _ => "latest",
    }
}

#[cfg(test)]
impl Config {
    /// The all-defaults configuration, hermetically — `Config::load()` would
    /// read the developer's real `KUBIMO__*` environment into a unit test.
    pub(crate) fn test_default() -> Self {
        config::Config::builder()
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_from(vars: &[(&str, &str)]) -> Result<Config, config::ConfigError> {
        let source = Config::environment_source().source(Some(
            vars.iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        ));
        config::Config::builder()
            .add_source(source)
            .build()?
            .try_deserialize()
    }

    #[test]
    fn default_workspace_mode_is_pooled_when_unset() {
        let config = load_from(&[]).unwrap();
        assert_eq!(config.default_workspace_mode, WorkspaceMode::Pooled);
    }

    /// The cutover switch. If this does not deserialize, flipping the default
    /// silently does nothing.
    #[test]
    fn default_workspace_mode_parses_from_env() {
        let config = load_from(&[("KUBIMO__DEFAULT_WORKSPACE_MODE", "Pooled")]).unwrap();
        assert_eq!(config.default_workspace_mode, WorkspaceMode::Pooled);
    }

    #[test]
    fn default_workspace_mode_rejects_unknown_value() {
        assert!(load_from(&[("KUBIMO__DEFAULT_WORKSPACE_MODE", "Nonsense")]).is_err());
    }

    /// Unset means off: no asset URL is minted and pods stay unchanged.
    #[test]
    fn runner_asset_url_is_off_by_default() {
        let config = load_from(&[]).unwrap();
        assert_eq!(config.runner_asset_base_path, None);
        assert_eq!(
            config.runner_asset_url("ghcr.io/aqora-io/kubimo-marimo:src-abc"),
            None
        );
    }

    /// The URL is `{base}/{image tag}` — the tag is the cache key the chart's
    /// static-assets server publishes under, so the derivation here must match
    /// the chart's text-after-last-colon rule.
    #[test]
    fn runner_asset_url_joins_base_and_image_tag() {
        let config = load_from(&[("KUBIMO__RUNNER_ASSET_BASE_PATH", "/marimo-assets/")]).unwrap();
        assert_eq!(
            config
                .runner_asset_url("ghcr.io/aqora-io/kubimo-marimo:src-abc123")
                .as_deref(),
            Some("/marimo-assets/src-abc123")
        );
        assert_eq!(
            config
                .runner_asset_url("ghcr.io/aqora-io/kubimo-marimo:0.2.9@sha256:deadbeef")
                .as_deref(),
            Some("/marimo-assets/0.2.9")
        );
        assert_eq!(
            config
                .runner_asset_url("localhost:5000/kubimo-marimo")
                .as_deref(),
            Some("/marimo-assets/latest"),
            "a registry port is not a tag"
        );
    }
}
