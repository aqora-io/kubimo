use std::collections::BTreeMap;

use kubimo::KubimoLabel;
use kubimo::k8s_openapi::api::core::v1::{Affinity, PodAffinity, PodAffinityTerm};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

pub(crate) fn workspace_label(workspace_name: &str) -> (String, String) {
    (
        KubimoLabel::borrow("workspace").to_string(),
        workspace_name.to_string(),
    )
}

pub(crate) fn workspace_label_map(workspace_name: &str) -> BTreeMap<String, String> {
    let (key, value) = workspace_label(workspace_name);
    [(key, value)].into_iter().collect()
}

/// Returns a required pod affinity on the workspace label so workspace pods
/// are always scheduled onto the same node.
pub(crate) fn workspace_affinity(workspace_name: &str) -> Affinity {
    let (key, value) = workspace_label(workspace_name);
    Affinity {
        pod_affinity: Some(PodAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![PodAffinityTerm {
                label_selector: Some(LabelSelector {
                    match_labels: Some([(key, value)].into_iter().collect()),
                    ..Default::default()
                }),
                topology_key: "kubernetes.io/hostname".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}
