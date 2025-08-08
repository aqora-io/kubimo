use hyper_util::{client::legacy::Client as HttpClient, rt::TokioExecutor};
use k8s_openapi::{
    NamespaceResourceScope,
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
};
use kube::client::ConfigExt as _;
use kube::{CustomResourceExt, Resource};
use tower::ServiceBuilder;
use tower_http::{BoxError, trace::TraceLayer};

use crate::{Api, ClientBuildError, Result};

#[derive(Clone)]
pub struct Client {
    name: String,
    kube: kube::Client,
}

#[derive(Default)]
pub struct ClientBuilder {
    name: Option<String>,
    namespace: Option<String>,
    config: Option<kube::Config>,
}

impl ClientBuilder {
    pub fn name(&mut self, name: impl ToString) -> &mut Self {
        self.name = Some(name.to_string());
        self
    }

    pub fn namespace(&mut self, namespace: impl ToString) -> &mut Self {
        self.namespace = Some(namespace.to_string());
        self
    }

    pub fn config(&mut self, config: kube::Config) -> &mut Self {
        self.config = Some(config);
        self
    }

    pub async fn build(&mut self) -> Result<Client, ClientBuildError> {
        let name = self.name.take().unwrap_or_else(|| "kubimo".into());
        let config = if let Some(config) = self.config.take() {
            config
        } else {
            kube::Config::infer().await?
        };
        let kube_service = ServiceBuilder::new()
            .layer(config.base_uri_layer())
            .option_layer(config.auth_layer()?)
            .layer(TraceLayer::new_for_http())
            .map_err(BoxError::from)
            .service(
                HttpClient::builder(TokioExecutor::new()).build(config.rustls_https_connector()?),
            );
        let kube_client = kube::Client::new(
            kube_service,
            self.namespace.take().unwrap_or(config.default_namespace),
        );
        Ok(Client {
            name,
            kube: kube_client,
        })
    }
}

impl Client {
    pub async fn infer() -> Result<Client, ClientBuildError> {
        Self::builder().build().await
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn kube(&self) -> &kube::Client {
        &self.kube
    }

    #[inline]
    pub fn api<T>(&self) -> Api<T>
    where
        T: Resource<Scope = NamespaceResourceScope>,
        <T as Resource>::DynamicType: Default,
    {
        Api::new(
            self.name.clone(),
            kube::Api::default_namespaced(self.kube.clone()),
        )
    }

    #[inline]
    pub fn api_with_namespace<T>(&self, namespace: &str) -> Api<T>
    where
        T: Resource<Scope = NamespaceResourceScope>,
        <T as Resource>::DynamicType: Default,
    {
        Api::new(
            self.name.clone(),
            kube::Api::namespaced(self.kube.clone(), namespace),
        )
    }

    #[inline]
    pub fn api_all<T>(&self) -> Api<T>
    where
        T: Resource,
        <T as Resource>::DynamicType: Default,
    {
        Api::new(self.name.clone(), kube::Api::all(self.kube.clone()))
    }

    #[tracing::instrument(level = "debug", skip_all, ret, err)]
    async fn patch_crd<T>(
        &self,
        api: kube::Api<CustomResourceDefinition>,
    ) -> Result<CustomResourceDefinition>
    where
        T: CustomResourceExt,
    {
        Ok(api
            .patch(
                T::crd_name(),
                &kube::api::PatchParams::apply(&self.name),
                &kube::api::Patch::Apply(T::crd()),
            )
            .await?)
    }

    pub async fn patch_all_crds(&self) -> Result<()> {
        let api = kube::Api::<CustomResourceDefinition>::all(self.kube.clone());
        self.patch_crd::<crate::KubimoWorkspace>(api.clone())
            .await?;
        self.patch_crd::<crate::KubimoRunner>(api.clone()).await?;
        Ok(())
    }
}
