use std::fmt;
use std::marker::PhantomData;

use kube::core::{NamespaceResourceScope, Resource, object::HasSpec};
use serde::{de, ser};

use crate::{config::Config, id::gen_name};

pub use kube::api::ListParams;

pub trait ResourceFactory: HasSpec {
    fn new(name: &str, spec: Self::Spec) -> Self;
}

pub struct Service<'a, T> {
    client: &'a kube::Client,
    config: &'a Config,
    resource: PhantomData<T>,
}

impl<'a, T> Service<'a, T>
where
    T: Resource<Scope = NamespaceResourceScope>
        + ResourceFactory
        + ser::Serialize
        + de::DeserializeOwned
        + fmt::Debug
        + Clone,
    <T as Resource>::DynamicType: Default,
{
    fn api(&self) -> kube::Api<T> {
        kube::Api::<T>::default_namespaced(self.client.clone())
    }

    pub async fn get(&self, name: &str) -> kube::Result<Option<T>> {
        match self.api().get(name).await {
            Ok(item) => Ok(Some(item)),
            Err(kube::Error::Api(e)) => {
                if matches!(e.code, 404) {
                    Ok(None)
                } else {
                    Err(kube::Error::Api(e))
                }
            }
            Err(e) => Err(e),
        }
    }

    pub async fn create(&self, spec: <T as HasSpec>::Spec) -> kube::Result<T> {
        self.api()
            .create(
                &kube::api::PostParams {
                    field_manager: Some(self.config.name.clone()),
                    ..Default::default()
                },
                &T::new(&gen_name(self.config.resource_name_len), spec),
            )
            .await
    }

    pub async fn list(&self, options: &ListParams) -> kube::Result<kube::api::ObjectList<T>> {
        self.api().list(options).await
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
    fn service<T>(&self) -> async_graphql::Result<Service<'a, T>>;
}

impl<'a, T> ContextServiceExt<'a> for T
where
    T: async_graphql::context::DataContext<'a>,
{
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
