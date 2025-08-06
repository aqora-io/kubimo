use std::sync::Arc;

use crate::{config::Config, service::Service};

pub struct ControllerContext {
    pub client: kube::Client,
    pub config: Config,
}

impl ControllerContext {
    pub fn service<T>(&self) -> Service<'_, T> {
        Service::new(&self.client, &self.config)
    }
}

impl ControllerContext {
    pub fn new(client: kube::Client, config: Config) -> Arc<Self> {
        Arc::new(Self { client, config })
    }
}

pub type ControllerError<E> = kube::runtime::controller::Error<E, kube::runtime::watcher::Error>;
pub type ControllerResult<T, E> = Result<
    (
        kube::runtime::reflector::ObjectRef<T>,
        kube::runtime::controller::Action,
    ),
    ControllerError<E>,
>;
