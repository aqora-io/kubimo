use std::ops::Deref;
use std::sync::Arc;

use crate::Config;

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
    pub fn new(client: kubimo::Client, config: Config) -> Arc<Self> {
        Arc::new(Self { client, config })
    }
}
