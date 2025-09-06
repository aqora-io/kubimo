use serde::{Deserialize, Serialize};

fn default_manager_name() -> String {
    "kubimo-controller".to_string()
}

fn default_marimo_image_name() -> String {
    "local/kubimo-marimo:dev".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub namespace: Option<String>,
    #[serde(default = "default_manager_name")]
    pub name: String,
    #[serde(default = "default_marimo_image_name")]
    pub marimo_image_name: String,
}

impl Config {
    pub fn load() -> Result<Config, config::ConfigError> {
        config::Config::builder()
            .add_source(config::Environment::with_prefix("KUBIMO"))
            .build()?
            .try_deserialize()
    }
}
