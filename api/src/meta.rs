use crate::{Error, Result};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Resource;

pub trait ResourceNameExt: Resource {
    fn name(&self) -> Result<&str> {
        self.meta()
            .name
            .as_deref()
            .ok_or(Error::ObjectMetaMissing("name"))
    }
}

impl<T> ResourceNameExt for T where T: Resource {}

pub trait ResourceNamespaceExt: Resource {
    fn require_namespace(&self) -> Result<&str> {
        self.meta()
            .namespace
            .as_deref()
            .ok_or(Error::ObjectMetaMissing("namespace"))
    }
}

impl<T> ResourceNamespaceExt for T where T: Resource {}

pub trait ResourceOwnerRefExt: Resource<DynamicType = ()> {
    fn static_controller_owner_ref(&self) -> Result<OwnerReference> {
        self.controller_owner_ref(&())
            .ok_or(Error::ObjectMetaMissing("controller_owner_ref"))
    }
}

impl<T> ResourceOwnerRefExt for T where T: Resource<DynamicType = ()> {}

pub trait ObjectMetaExt {
    fn strip_system(&self) -> Self;
}

impl ObjectMetaExt for ObjectMeta {
    fn strip_system(&self) -> Self {
        ObjectMeta {
            name: self.name.clone(),
            generate_name: self.generate_name.clone(),
            annotations: self.annotations.clone(),
            labels: self.labels.clone(),
            owner_references: self.owner_references.clone(),
            ..Default::default()
        }
    }
}
