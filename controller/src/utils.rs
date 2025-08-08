use futures::future::BoxFuture;
use futures::prelude::*;

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
