use futures::future::BoxFuture;
use futures::prelude::*;
use kubimo::kube::{Resource, runtime::controller};

use crate::ControllerResult;

pub trait ControllerStreamExt<'a> {
    fn wait(self) -> BoxFuture<'a, ()>;
}

impl<'a, S, K, E> ControllerStreamExt<'a> for S
where
    S: Stream<Item = ControllerResult<K, E>> + Send + 'a,
    K: Resource,
    K::DynamicType: Default,
    E: 'static,
{
    fn wait(self) -> BoxFuture<'a, ()> {
        self.for_each_concurrent(None, |result| {
            // Reconcile errors are logged by the reconciler's trace layer, but
            // watcher errors surface nowhere else, and a failing watch means
            // the controller stops seeing changes.
            if let Err(controller::Error::QueueError(err)) = result {
                tracing::warn!(kind = %K::kind(&Default::default()), "watcher error: {err}");
            }
            futures::future::ready(())
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use kubimo::kube::runtime::watcher;
    use tracing_subscriber::prelude::*;

    use super::*;

    struct CountWarnings(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountWarnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// kube retries a failing watch with backoff but reports it nowhere, so a
    /// controller that has stopped seeing changes would leave no trace.
    #[test]
    fn watcher_errors_are_logged() {
        let warnings = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarnings(warnings.clone()));
        let stream =
            futures::stream::iter([ControllerResult::<kubimo::Workspace, std::io::Error>::Err(
                controller::Error::QueueError(watcher::Error::NoResourceVersion),
            )]);

        tracing::subscriber::with_default(subscriber, || {
            futures::executor::block_on(stream.wait())
        });

        assert_eq!(warnings.load(Ordering::Relaxed), 1);
    }
}
