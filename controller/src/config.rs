use kubimo::WorkspaceMode;
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
    #[serde(default)]
    pub recycle: RecyclePolicy,
}

/// When to delete a runner pod whose slot bind mount is dead, so it gets recreated.
///
/// Off by default: this deletes user-visible pods, so it stays opt-in until a cluster
/// has produced a real wedged pod to check the signature against.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecyclePolicy {
    pub enabled: bool,
    /// How long the wedge must persist before acting.
    ///
    /// Guards the case that is *invisible* in container status: a new pod on a node
    /// whose agent is not up yet fails as a `FailedMount` event, staying
    /// `ContainerCreating` with no message at all. That is the normal state during every
    /// agent rollout, so the dwell must comfortably outlast one.
    pub dwell_secs: i64,
    pub cooldown_secs: i64,
    /// After this many recycles the runner is left broken on purpose, so a permanently
    /// bad node surfaces as an error someone can see rather than an endless restart.
    pub max_recycles: u32,
}

impl Default for RecyclePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            dwell_secs: 120,
            cooldown_secs: 600,
            max_recycles: 3,
        }
    }
}

impl Default for StatusCheck {
    fn default() -> Self {
        Self {
            resolution: Default::default(),
            interval_secs: default_runner_status_check_interval_secs(),
            recycle: Default::default(),
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
    #[serde(default)]
    pub runner_status: StatusCheck,
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
    fn default_workspace_mode_is_dedicated_when_unset() {
        let config = load_from(&[]).unwrap();
        assert_eq!(config.default_workspace_mode, WorkspaceMode::Dedicated);
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

    /// Recycling deletes user-visible pods, so it must stay off unless asked for.
    #[test]
    fn recycle_is_disabled_unless_enabled() {
        let config = load_from(&[]).unwrap();
        assert!(!config.runner_status.recycle.enabled);
        assert_eq!(config.runner_status.recycle.max_recycles, 3);
    }

    /// The switch that turns remediation on. If this does not deserialize, enabling it
    /// in a chart silently does nothing and runners stay wedged.
    #[test]
    fn recycle_parses_from_env() {
        let config = load_from(&[
            ("KUBIMO__RUNNER_STATUS__RECYCLE__ENABLED", "true"),
            ("KUBIMO__RUNNER_STATUS__RECYCLE__DWELL_SECS", "45"),
            ("KUBIMO__RUNNER_STATUS__RECYCLE__MAX_RECYCLES", "1"),
        ])
        .unwrap();
        assert!(config.runner_status.recycle.enabled);
        assert_eq!(config.runner_status.recycle.dwell_secs, 45);
        assert_eq!(config.runner_status.recycle.max_recycles, 1);
        // Untouched keys keep their defaults rather than resetting to zero.
        assert_eq!(config.runner_status.recycle.cooldown_secs, 600);
        assert_eq!(config.runner_status.interval_secs, 10);
    }
}
