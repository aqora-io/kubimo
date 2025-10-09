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
}

impl Default for StatusCheck {
    fn default() -> Self {
        Self {
            resolution: Default::default(),
            interval_secs: default_runner_status_check_interval_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_manager_name")]
    pub manager_name: String,
    #[serde(default = "default_marimo_image")]
    pub marimo_image: String,
    #[serde(default = "default_busybox_image")]
    pub busybox_image: String,
    #[serde(default)]
    pub runner_status: StatusCheck,
}

impl Config {
    pub fn load() -> Result<Config, config::ConfigError> {
        config::Config::builder()
            .add_source(config::Environment::with_prefix("KUBIMO").separator("__"))
            .build()?
            .try_deserialize()
    }
}
