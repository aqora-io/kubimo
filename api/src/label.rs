use std::fmt;

use kube::Resource;

pub struct KubimoLabel(pub String);

impl KubimoLabel {
    pub fn new(name: impl ToString) -> Self {
        Self(name.to_string())
    }
}

impl fmt::Display for KubimoLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kubimo.aqora.io/{}", self.0)
    }
}

pub trait ResourceLabelExt: Resource {
    fn insert_label(&mut self, label: impl ToString, value: impl ToString) -> Option<String> {
        self.meta_mut()
            .labels
            .get_or_insert_default()
            .insert(label.to_string(), value.to_string())
    }

    fn remove_label(&mut self, label: impl ToString) -> Option<String> {
        self.meta_mut()
            .labels
            .as_mut()
            .and_then(|labels| labels.remove(&label.to_string()))
    }

    fn insert_annotation(
        &mut self,
        annotation: impl ToString,
        value: impl ToString,
    ) -> Option<String> {
        self.meta_mut()
            .annotations
            .get_or_insert_default()
            .insert(annotation.to_string(), value.to_string())
    }

    fn remove_annotation(&mut self, annotation: impl ToString) -> Option<String> {
        self.meta_mut()
            .annotations
            .as_mut()
            .and_then(|annotations| annotations.remove(&annotation.to_string()))
    }
}

impl<T> ResourceLabelExt for T where T: Resource {}
