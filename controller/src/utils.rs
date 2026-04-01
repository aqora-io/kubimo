use futures::future::BoxFuture;
use futures::prelude::*;
use kubimo::k8s_openapi::api::core::v1::{Capabilities, SecurityContext};

pub trait ControllerStreamExt<'a> {
    fn wait(self) -> BoxFuture<'a, ()>;
}

impl<'a, T> ControllerStreamExt<'a> for T
where
    T: Stream + Send + 'a,
{
    fn wait(self) -> BoxFuture<'a, ()> {
        self.for_each_concurrent(None, |_| futures::future::ready(()))
            .boxed()
    }
}

pub fn hardened_security_context() -> SecurityContext {
    SecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(1000),
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}
