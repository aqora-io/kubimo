use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    Affinity, NodeAffinity, NodeSelectorRequirement, NodeSelectorTerm, PodAffinity,
    PodAffinityTerm, PreferredSchedulingTerm,
};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kubimo::prelude::ResourceExt;
use kubimo::{KubimoLabel, Workspace};

pub(crate) fn workspace_label(workspace_name: &str) -> (String, String) {
    (
        KubimoLabel::borrow("workspace").to_string(),
        workspace_name.to_string(),
    )
}

pub fn workspace_label_map(workspace_name: &str) -> BTreeMap<String, String> {
    let (key, value) = workspace_label(workspace_name);
    [(key, value)].into_iter().collect()
}

/// Returns a required pod affinity on the workspace label so workspace pods
/// are always scheduled onto the same node.
pub fn workspace_affinity(workspace_name: &str) -> Affinity {
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

/// The affinity for a new pod of `workspace`: the required pod affinity, plus
/// a preference for the node its slot is on, if any.
///
/// Every kind of pod can be a workspace's first (a runner, a cache job, an
/// import), and the first one decides where the rest go. Only for pods being
/// created: affinity is immutable, and the slot's node changes after the first
/// pod publishes its volume.
pub fn new_pod_affinity(workspace: &Workspace) -> Affinity {
    let mut affinity = workspace_affinity(&workspace.name_any());
    if let Some(node) = workspace
        .status
        .as_ref()
        .and_then(|status| status.slot.as_ref())
        .and_then(|slot| slot.node.as_deref())
    {
        prefer_node(&mut affinity, node);
    }
    affinity
}

/// Adds a soft preference for `node`: the node whose data volume already holds
/// the workspace's slot (`status.slot.node`). Reusing that copy keeps what S3
/// does not have (marimo caches, exports, anything not yet synced), and a
/// preference rather than a requirement means a lost or full node is simply
/// passed over. Only matters for a workspace's first pod; later ones follow it
/// through the required pod affinity.
pub fn prefer_node(affinity: &mut Affinity, node: &str) {
    affinity
        .node_affinity
        .get_or_insert_with(NodeAffinity::default)
        .preferred_during_scheduling_ignored_during_execution
        .get_or_insert_with(Vec::new)
        .push(PreferredSchedulingTerm {
            weight: 100,
            preference: NodeSelectorTerm {
                // The agent records the node's name, which the hostname
                // label need not equal.
                match_fields: Some(vec![NodeSelectorRequirement {
                    key: "metadata.name".to_string(),
                    operator: "In".to_string(),
                    values: Some(vec![node.to_string()]),
                }]),
                ..Default::default()
            },
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefer_node_adds_a_soft_node_name_preference_and_keeps_the_pod_affinity() {
        let mut affinity = workspace_affinity("bmow-x");
        prefer_node(&mut affinity, "node-a");
        assert!(affinity.pod_affinity.is_some());
        let preferred = affinity
            .node_affinity
            .unwrap()
            .preferred_during_scheduling_ignored_during_execution
            .unwrap();
        assert_eq!(preferred.len(), 1);
        let requirement = &preferred[0].preference.match_fields.as_ref().unwrap()[0];
        assert_eq!(requirement.key, "metadata.name");
        assert_eq!(
            requirement.values.as_deref(),
            Some(&["node-a".to_string()][..])
        );
    }

    #[test]
    fn a_new_pod_prefers_the_slot_node_only_once_there_is_one() {
        let mut workspace = Workspace::new("bmow-x", Default::default());
        assert_eq!(new_pod_affinity(&workspace), workspace_affinity("bmow-x"));

        workspace.status = Some(kubimo::WorkspaceStatus {
            slot: Some(kubimo::WorkspaceSlotStatus {
                node: Some("node-a".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut expected = workspace_affinity("bmow-x");
        prefer_node(&mut expected, "node-a");
        assert_eq!(new_pod_affinity(&workspace), expected);
    }
}
