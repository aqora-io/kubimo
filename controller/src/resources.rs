use kubimo::k8s_openapi::api::core::v1::{ResourceRequirements, VolumeResourceRequirements};
use kubimo::k8s_openapi::apimachinery::pkg::api::resource::Quantity as KubeQuantity;
use kubimo::{CpuUnit, Quantity, StorageUnit};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct ResourceRequirement {
    pub storage: Option<Quantity<StorageUnit>>,
    pub memory: Option<Quantity<StorageUnit>>,
    pub cpu: Option<Quantity<CpuUnit>>,
}

impl ResourceRequirement {
    fn is_empty(&self) -> bool {
        self.storage.is_none() && self.memory.is_none() && self.cpu.is_none()
    }
}

impl From<ResourceRequirement> for BTreeMap<String, KubeQuantity> {
    fn from(value: ResourceRequirement) -> Self {
        let mut map = BTreeMap::default();
        if let Some(storage) = value.storage {
            map.insert("storage".to_string(), storage.into());
        }
        if let Some(memory) = value.memory {
            map.insert("memory".to_string(), memory.into());
        }
        if let Some(cpu) = value.cpu {
            map.insert("cpu".to_string(), cpu.into());
        }
        map
    }
}

impl From<ResourceRequirement> for Option<BTreeMap<String, KubeQuantity>> {
    fn from(value: ResourceRequirement) -> Self {
        if value.is_empty() {
            None
        } else {
            Some(value.into())
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Resources {
    pub requests: ResourceRequirement,
    pub limits: ResourceRequirement,
}

impl From<Resources> for VolumeResourceRequirements {
    fn from(value: Resources) -> Self {
        VolumeResourceRequirements {
            requests: value.requests.into(),
            limits: value.limits.into(),
        }
    }
}

impl From<Resources> for Option<VolumeResourceRequirements> {
    fn from(value: Resources) -> Self {
        if value.requests.is_empty() && value.limits.is_empty() {
            None
        } else {
            Some(value.into())
        }
    }
}

impl From<Resources> for ResourceRequirements {
    fn from(value: Resources) -> Self {
        ResourceRequirements {
            requests: value.requests.into(),
            limits: value.limits.into(),
            ..Default::default()
        }
    }
}

impl From<Resources> for Option<ResourceRequirements> {
    fn from(value: Resources) -> Self {
        if value.requests.is_empty() && value.limits.is_empty() {
            None
        } else {
            Some(value.into())
        }
    }
}
