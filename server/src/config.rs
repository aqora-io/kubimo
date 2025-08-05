use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};

fn default_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
}

fn default_port() -> u16 {
    3000
}

fn default_manager_name() -> String {
    "kubimo".to_string()
}

fn default_resource_name_len() -> usize {
    let mut target = u32::MAX / names::NOUNS.len() as u32;
    let mut len = 1;
    while target > 0 {
        target /= names::ADJECTIVES.len() as u32;
        len += 1;
    }
    len
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    pub namespace: Option<String>,
    #[serde(default = "default_manager_name")]
    pub name: String,
    #[serde(default = "default_resource_name_len")]
    pub resource_name_len: usize,
}

impl Config {
    pub fn load() -> Result<Config, config::ConfigError> {
        config::Config::builder()
            .add_source(config::Environment::with_prefix("KUBIMO"))
            .build()?
            .try_deserialize()
    }
}
