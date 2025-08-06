use std::fmt;
use std::marker::PhantomData;

use futures::Stream;
use kube::core::{NamespaceResourceScope, Resource, object::HasStatus};
use serde::{de, ser};

use crate::config::Config;

pub use kube::api::ListParams;

pub struct Service<'a, T> {
    client: &'a kube::Client,
    config: &'a Config,
    resource: PhantomData<T>,
}

impl<'a, T> Service<'a, T> {
    pub fn new(client: &'a kube::Client, config: &'a Config) -> Self {
        Self {
            client,
            config,
            resource: PhantomData,
        }
    }
}

impl<'a, T> Service<'a, T>
where
    T: Resource<Scope = NamespaceResourceScope>
        + ser::Serialize
        + de::DeserializeOwned
        + fmt::Debug
        + Send
        + Clone
        + 'static,
    <T as Resource>::DynamicType: Default,
{
    fn api(&self) -> kube::Api<T> {
        kube::Api::<T>::default_namespaced(self.client.clone())
    }

    pub async fn get(&self, name: &str) -> kube::Result<T> {
        self.api().get(name).await
    }

    pub async fn get_opt(&self, name: &str) -> kube::Result<Option<T>> {
        self.api().get_opt(name).await
    }

    pub async fn create(&self, item: &T) -> kube::Result<T> {
        self.api()
            .create(
                &kube::api::PostParams {
                    field_manager: Some(self.config.name.clone()),
                    ..Default::default()
                },
                item,
            )
            .await
    }

    pub async fn update(&self, name: &str, item: &T) -> kube::Result<T> {
        self.api()
            .patch(
                name,
                &kube::api::PatchParams {
                    field_manager: Some(self.config.name.clone()),
                    ..Default::default()
                },
                &kube::api::Patch::Merge(item),
            )
            .await
    }

    pub async fn patch(&self, name: &str, item: &T) -> kube::Result<T> {
        self.api()
            .patch(
                name,
                &kube::api::PatchParams {
                    field_manager: Some(self.config.name.clone()),
                    ..Default::default()
                },
                &kube::api::Patch::Apply(item),
            )
            .await
    }

    pub async fn patch_status(&self, name: &str, item: &T::Status) -> kube::Result<T>
    where
        T: HasStatus,
        <T as HasStatus>::Status: ser::Serialize + de::DeserializeOwned + fmt::Debug + Clone,
    {
        let status = serde_json::json!({
            "status": item
        });
        self.api()
            .patch_status(
                name,
                &kube::api::PatchParams {
                    field_manager: Some(self.config.name.clone()),
                    ..Default::default()
                },
                &kube::api::Patch::Merge(&status),
            )
            .await
    }

    pub async fn list(&self, options: &ListParams) -> kube::Result<kube::api::ObjectList<T>> {
        self.api().list(options).await
    }

    pub fn watch_one(
        &self,
        name: &str,
    ) -> impl Stream<Item = kube::runtime::watcher::Result<kube::runtime::watcher::Event<T>>>
    + 'static
    + use<T> {
        kube::runtime::watcher(
            self.api(),
            kube::runtime::watcher::Config::default().fields(&format!("metadata.name={name}")),
        )
    }

    pub async fn delete(&self, name: &str) -> kube::Result<Option<T>> {
        Ok(self
            .api()
            .delete(name, &kube::api::DeleteParams::default())
            .await?
            .left())
    }
}

pub type ObjectListConnection<T> = async_graphql::connection::Connection<String, T>;

pub trait ObjectListExt<T>
where
    T: async_graphql::OutputType,
{
    fn connection(self) -> ObjectListConnection<T>;
}

impl<T, U> ObjectListExt<T> for kube::api::ObjectList<U>
where
    T: async_graphql::OutputType + From<U>,
    U: Clone,
{
    fn connection(mut self) -> ObjectListConnection<T> {
        let last = self.items.pop();
        let has_next_page = self
            .metadata
            .continue_
            .as_ref()
            .is_some_and(|token| !token.is_empty())
            && last.is_some();
        let mut connection = async_graphql::connection::Connection::new(false, has_next_page);
        connection.edges = self
            .items
            .into_iter()
            .map(|item| async_graphql::connection::Edge::new(String::new(), T::from(item)))
            .collect();
        if let Some(last) = last {
            if let Some(token) = self.metadata.continue_ {
                connection
                    .edges
                    .push(async_graphql::connection::Edge::new(token, T::from(last)));
            }
        }
        connection
    }
}

pub trait ContextServiceExt<'a> {
    type Error;
    fn service<T>(&self) -> Result<Service<'a, T>, Self::Error>;
}

impl<'a, T> ContextServiceExt<'a> for T
where
    T: async_graphql::context::DataContext<'a>,
{
    type Error = async_graphql::Error;
    fn service<U>(&self) -> async_graphql::Result<Service<'a, U>> {
        let client = self.data::<kube::Client>()?;
        let config = self.data::<Config>()?;
        Ok(Service {
            client,
            config,
            resource: PhantomData,
        })
    }
}
