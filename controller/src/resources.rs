use kubimo::k8s_openapi::api::core::v1::{ResourceRequirements, VolumeResourceRequirements};
use kubimo::k8s_openapi::apimachinery::pkg::api::resource::Quantity as KubeQuantity;
use kubimo::{CpuQuantity, Requirement, StorageQuantity};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct ResourceRequirement {
    pub storage: Option<StorageQuantity>,
    pub memory: Option<StorageQuantity>,
    pub cpu: Option<CpuQuantity>,
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

impl Resources {
    pub fn storage(mut self, requirement: Option<Requirement<StorageQuantity>>) -> Self {
        // Only the request is applied to the PVC. A storage `limit` is immutable
        // once the claim is bound, so writing one would make every later
        // reconcile (e.g. auto-scaling a `requests` bump) fail with a 422. The
        // `max` ceiling is enforced in software when computing the request.
        self.requests.storage = requirement.and_then(|requirement| requirement.min);
        self
    }

    pub fn memory(mut self, requirement: Option<Requirement<StorageQuantity>>) -> Self {
        if let Some(requirement) = requirement {
            self.requests.memory = requirement.min;
            self.limits.memory = requirement.max;
        } else {
            self.requests.memory = None;
            self.limits.memory = None;
        }
        self
    }

    pub fn cpu(mut self, requirement: Option<Requirement<CpuQuantity>>) -> Self {
        if let Some(requirement) = requirement {
            self.requests.cpu = requirement.min;
            self.limits.cpu = requirement.max;
        } else {
            self.requests.cpu = None;
            self.limits.cpu = None;
        }
        self
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::StorageUnit;

    #[test]
    fn storage_sets_request_but_never_a_limit() {
        // A storage limit is immutable on a bound PVC, so `max` must not reach
        // the claim even when configured.
        let resources = Resources::default().storage(Some(Requirement {
            min: Some(StorageQuantity::new(10, StorageUnit::Gi)),
            max: Some(StorageQuantity::new(20, StorageUnit::Gi)),
        }));
        let volume: VolumeResourceRequirements = resources.into();
        assert!(volume.requests.unwrap().contains_key("storage"));
        assert!(volume.limits.is_none());
    }
}
