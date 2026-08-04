use std::fmt::Debug;

use futures::prelude::*;
use kube::api::ObjectList;
use kube::core::object::HasStatus;
use kube::{
    Resource,
    api::{Patch, PatchParams},
};

#[cfg(feature = "runtime")]
use kube::runtime::watcher::{Event, watcher};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    ApiListStreamExt, Error, FilterParams, ListStream, ObjectMetaExt, ResourceNameExt, Result,
};

pub type ApiListStream<T> = futures::stream::MapErr<ListStream<T>, fn(kube::Error) -> Error>;

#[derive(Clone, Debug)]
pub struct Api<T> {
    name: String,
    inner: kube::api::Api<T>,
}

impl<T> Api<T> {
    pub fn new(name: String, inner: kube::api::Api<T>) -> Self {
        Self { name, inner }
    }
}

impl<T> Api<T>
where
    T: Resource + Serialize + DeserializeOwned + Clone + Debug + Send + 'static,
{
    #[inline]
    pub fn kube(&self) -> &kube::Api<T> {
        &self.inner
    }

    #[inline]
    pub fn patch_params(&self) -> PatchParams {
        PatchParams::apply(&self.name)
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn patch(&self, resource: &T) -> Result<T> {
        self.apply(resource, self.patch_params()).await
    }

    /// Apply `resource`, taking over every field it sets even where another
    /// field manager already owns them.
    ///
    /// Server-side apply lets a manager change only the fields it owns; a write
    /// to anyone else's comes back `409 Conflict`. That is what [`Self::patch`]
    /// does, and it is load-bearing — it is the reason two writers with
    /// different identities, such as the node agent and the indexer, cannot
    /// clobber each other's halves of a status.
    ///
    /// A client that *renames* its field manager has no way through that on its
    /// own: every field of every object it has ever written still belongs to the
    /// old name, so its first write to each conflicts, and keeps conflicting
    /// until something takes the fields over. Forcing is that takeover.
    ///
    /// Deliberately opt-in and per call rather than a property of the client:
    /// forcing is right for a migration that knows the fields it is claiming
    /// were its own, and wrong everywhere else, where a conflict is the API
    /// server reporting that two components disagree about who owns what.
    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn patch_force(&self, resource: &T) -> Result<T> {
        self.apply(resource, self.patch_params().force()).await
    }

    async fn apply(&self, resource: &T, params: PatchParams) -> Result<T> {
        let mut json = serde_json::to_value(resource)?;
        let Some(object) = json.as_object_mut() else {
            return Err(crate::Error::expected_json_type("object", &json));
        };
        object.remove("status");
        object.insert(
            "metadata".to_string(),
            serde_json::to_value(resource.meta().strip_system())?,
        );
        Ok(self
            .inner
            .patch(resource.name()?, &params, &Patch::Apply(&object))
            .await?)
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn patch_json(&self, name: &str, patch: json_patch::Patch) -> Result<T> {
        Ok(self
            .inner
            .patch(name, &self.patch_params(), &Patch::<T>::Json(patch))
            .await?)
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn get(&self, name: &str) -> Result<T> {
        Ok(self.inner.get(name).await?)
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn get_opt(&self, name: &str) -> Result<Option<T>> {
        Ok(self.inner.get_opt(name).await?)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub fn list(&self, params: &FilterParams) -> ApiListStream<T>
    where
        T: Unpin,
    {
        self.inner.list_stream(params).map_err(Error::from)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn find(&self, params: &FilterParams) -> Result<Option<T>> {
        Ok(self
            .inner
            .list(&kube::api::ListParams::from(params).limit(1))
            .await?
            .items
            .into_iter()
            .next())
    }

    #[cfg(feature = "runtime")]
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn watch(
        &self,
        params: &FilterParams,
    ) -> futures::stream::BoxStream<'static, Result<Event<T>>> {
        watcher(self.inner.clone(), params.into())
            .map_err(Into::into)
            .boxed()
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn delete(&self, name: &str) -> Result<Option<T>> {
        Ok(self.inner.delete(name, &Default::default()).await?.left())
    }

    /// Delete `name`, treating an object that is already gone as success.
    ///
    /// Goes to the inner client directly rather than through [`Self::delete`],
    /// the way [`Self::get_opt`] does, because `err` on a `tracing::instrument`
    /// emits at ERROR regardless of the span's level and does so on the way out
    /// of the instrumented function — before this one can look at the status
    /// code. Delegating therefore logged every *expected* not-found as an error,
    /// and expecting one is the normal case here: the reconcilers converge by
    /// deleting objects that mostly do not exist. In production that was around
    /// one and a half ERROR lines a second, which is enough to hide the errors
    /// that mean something.
    ///
    /// A delete that fails for any other reason still takes this function's own
    /// `err` and stays as loud as it was.
    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn delete_opt(&self, name: &str) -> Result<Option<T>> {
        match self.inner.delete(name, &Default::default()).await {
            Ok(deleted) => Ok(deleted.left()),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn delete_collection(&self, params: &FilterParams) -> Result<Option<ObjectList<T>>> {
        Ok(self
            .inner
            .delete_collection(&Default::default(), &params.into())
            .await?
            .left())
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn patch_status(&self, resource: &T) -> Result<T>
    where
        T: HasStatus,
    {
        let mut json = serde_json::to_value(resource)?;
        let Some(object) = json.as_object_mut() else {
            return Err(crate::Error::expected_json_type("object", &json));
        };
        object.remove("spec");
        object.remove("metadata");
        Ok(self
            .inner
            .patch_status(
                resource.name()?,
                &self.patch_params(),
                &Patch::Apply(&object),
            )
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::layer::SubscriberExt;

    use crate::Workspace;

    /// Counts ERROR events, which is what `err` on a `tracing::instrument`
    /// produces — at that level whatever the span's own level is.
    struct CountErrors(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountErrors {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::ERROR {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A client whose every request comes back as the API server's own 404.
    fn client_that_always_404s() -> crate::Client {
        let service = tower::service_fn(|_req: http::Request<kube::client::Body>| async {
            http::Response::builder()
                .status(404)
                .header("content-type", "application/json")
                .body(kube::client::Body::from(
                    br#"{"kind":"Status","apiVersion":"v1","status":"Failure",
                        "message":"workspaces.kubimo.aqora.io \"missing\" not found",
                        "reason":"NotFound","code":404}"#
                        .to_vec(),
                ))
                .map_err(std::io::Error::other)
        });
        crate::Client::new(kube::Client::new(service, "default"), "kubimo")
    }

    async fn errors_logged_by<F, Fut, T>(f: F) -> (T, usize)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(CountErrors(count.clone()));
        let out = f().with_subscriber(subscriber).await;
        (out, count.load(Ordering::Relaxed))
    }

    /// Deleting something that is already gone is not an error, and must not be
    /// logged as one.
    ///
    /// The reconcilers converge by deleting objects that mostly do not exist —
    /// an indexer pod for a workspace that has none, a snapshot that was never
    /// taken — so this path runs constantly. While it delegated to `delete`,
    /// whose `err` fires before the status code is looked at, that was around
    /// one and a half ERROR lines a second in production.
    #[tokio::test]
    async fn an_expected_not_found_is_not_logged_as_an_error() {
        let client = client_that_always_404s();
        let (deleted, errors) =
            errors_logged_by(|| async { client.api::<Workspace>().delete_opt("missing").await })
                .await;
        assert!(deleted.expect("a 404 is not a failure here").is_none());
        assert_eq!(errors, 0, "an expected not-found logged {errors} error(s)");
    }

    /// ...and the loudness it used to borrow from `delete` is still there for a
    /// caller that did not ask for the 404 to be swallowed.
    #[tokio::test]
    async fn an_unexpected_not_found_is_still_logged_as_an_error() {
        let client = client_that_always_404s();
        let (deleted, errors) =
            errors_logged_by(|| async { client.api::<Workspace>().delete("missing").await }).await;
        assert!(deleted.is_err());
        assert!(errors > 0, "a failed delete logged nothing");
    }
}
