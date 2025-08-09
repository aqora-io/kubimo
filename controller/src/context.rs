use std::ops::Deref;

use crate::Config;

#[derive(Clone)]
pub struct Context {
    pub client: kubimo::Client,
    pub config: Config,
}

impl Deref for Context {
    type Target = kubimo::Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Context {
    pub fn new(client: kubimo::Client, config: Config) -> Self {
        Self { client, config }
    }
}
