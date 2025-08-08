use std::fmt::Debug;

use futures::prelude::*;
use futures::stream::BoxStream;
use kube::{
    Resource,
    api::{Patch, PatchParams},
    runtime::watcher::{Event, watcher},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{ApiListStreamExt, FilterParams, ListStream, ObjectMetaExt, ResourceNameExt, Result};

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
            .patch(
                resource.name()?,
                &self.patch_params(),
                &Patch::Apply(&object),
            )
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
    pub fn list(&self, params: &FilterParams) -> ListStream<T> {
        self.inner.list_stream(params)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub fn watch(&self, params: &FilterParams) -> BoxStream<'static, Result<Event<T>>> {
        watcher(self.inner.clone(), params.into())
            .map_err(Into::into)
            .boxed()
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn delete(&self, name: &str) -> Result<Option<T>> {
        Ok(self.inner.delete(name, &Default::default()).await?.left())
    }

    #[tracing::instrument(level = "debug", skip(self), ret, err)]
    pub async fn patch_status(&self, resource: &T) -> Result<T> {
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
