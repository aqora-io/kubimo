use async_graphql::*;

use futures::prelude::*;
use kube::runtime::watcher::Event as KubeEvent;

use crate::crd::KubimoWorkspace;
use crate::graphql::query::node::Node;
use crate::id::Id;
use crate::service::ContextServiceExt;

#[derive(Default)]
pub struct Subscription;

#[derive(Enum, Copy, Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Apply,
    Delete,
}

#[derive(Clone, Debug, SimpleObject)]
pub struct NodeEvent {
    event: Event,
    node: Node,
}

fn map_event<T, U>(event: KubeEvent<T>, f: impl Fn(T) -> U) -> KubeEvent<U> {
    match event {
        KubeEvent::Apply(item) => KubeEvent::Apply(f(item)),
        KubeEvent::Delete(item) => KubeEvent::Delete(f(item)),
        KubeEvent::Init => KubeEvent::Init,
        KubeEvent::InitApply(item) => KubeEvent::InitApply(f(item)),
        KubeEvent::InitDone => KubeEvent::InitDone,
    }
}

#[Subscription]
impl Subscription {
    async fn node(
        &self,
        ctx: &Context<'_>,
        id: Id,
    ) -> Result<impl Stream<Item = Result<NodeEvent>>> {
        Ok(match id {
            Id::Workspace(name) => ctx
                .service::<KubimoWorkspace>()?
                .watch_one(&name)
                .map_ok(|event| map_event(event, Node::from)),
        }
        .try_filter_map(|event| {
            futures::future::ok(match event {
                KubeEvent::Apply(node) => Some(NodeEvent {
                    event: Event::Apply,
                    node,
                }),
                KubeEvent::Delete(node) => Some(NodeEvent {
                    event: Event::Delete,
                    node,
                }),
                _ => None,
            })
        })
        .map_err(async_graphql::Error::from))
    }
}
