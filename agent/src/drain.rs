//! Shutting this agent down without wedging the runners it is serving.
//!
//! Deleting an agent pod destroys the node data volume, and the block device detaches
//! ~10s later. Every runner still holding a slot bind mount is then pointing at a dead
//! filesystem: reads return `EIO`, kubelet cannot recreate the container, and nothing
//! recovers. Worse, kubelet never calls `NodeUnpublishVolume` for those pods while this
//! agent is alive, so the final flush never runs and the newest work is lost with the
//! volume.
//!
//! The drain inverts that order. It deletes the pods holding slots *first*, while the
//! agent is still serving, so kubelet unpublishes each one — flushing to S3 and
//! unmounting cleanly — and only then does the agent exit. The runners are recreated by
//! the controller and re-hydrate onto the replacement agent's volume.
//!
//! Best-effort by construction: a node crash, a SIGKILL or `--grace-period=0` skips it
//! entirely. It converts the *common* case — a chart upgrade, i.e. every deploy — from
//! destructive to clean; it is not a substitute for the reactive recovery path.

use std::time::Duration;

use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::kube::api::{DeleteParams, ListParams};

use crate::csi::DRIVER_NAME;
use crate::store::SlotStore;

/// Field manager name. Only used to build the client; the drain issues no applies.
const MANAGER: &str = "kubimo-agent";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub deleted: usize,
    /// Whether every published volume was unpublished before the timeout.
    pub drained: bool,
}

/// Whether this pod's fate depends on this agent.
///
/// Keyed on the CSI driver rather than a label: holding one of our volumes is the exact
/// property that makes a pod's mount die with this agent, it needs no label contract
/// kept in sync, and it catches cache-job pods and anything added later for free.
fn holds_a_slot(pod: &Pod) -> bool {
    pod.spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_some_and(|volumes| {
            volumes.iter().any(|volume| {
                volume
                    .csi
                    .as_ref()
                    .is_some_and(|csi| csi.driver == DRIVER_NAME)
            })
        })
}

pub async fn run(
    store: &SlotStore,
    node_name: &str,
    timeout: Duration,
    poll: Duration,
    runner_grace_secs: u64,
) -> Result<Summary, Box<dyn std::error::Error>> {
    // First, so nothing new publishes onto a volume that is about to disappear.
    store.mark_draining()?;

    let client = kubimo::Client::builder().name(MANAGER).build().await?;
    let pods = client.api_global::<Pod>();

    // Cluster-scoped: runner pods live in tenant namespaces, and a `spec.nodeName`
    // field selector still requires a cluster-wide list.
    let params = ListParams::default().fields(&format!("spec.nodeName={node_name}"));
    let all = pods.kube().list(&params).await?;

    let mut deleted = 0;
    let delete_params = DeleteParams {
        grace_period_seconds: Some(runner_grace_secs.min(u64::from(u32::MAX)) as u32),
        ..Default::default()
    };
    for pod in all.items.iter().filter(|pod| holds_a_slot(pod)) {
        let Some(name) = pod.metadata.name.as_deref() else {
            continue;
        };
        // Already on its way out; kubelet will unpublish it without our help.
        if pod.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let namespace = pod.metadata.namespace.as_deref().unwrap_or("default");
        let scoped = client.api_namespaced::<Pod>(namespace);
        match scoped.kube().delete(name, &delete_params).await {
            Ok(_) => {
                deleted += 1;
                tracing::info!(
                    pod = name,
                    namespace,
                    "draining: deleted pod holding a slot"
                );
            }
            Err(err) => {
                // Keep going: one undeletable pod must not strand the rest, which would
                // lose their flushes too.
                tracing::error!(%err, pod = name, namespace, "draining: could not delete pod");
            }
        }
    }

    // Wait on local state rather than the API. `published_workspaces` is derived from the
    // `vol-` records, which `node_unpublish_volume` removes only after the final flush
    // and unmount — so it empties exactly when the work we are waiting for is done, and
    // it needs no extra RBAC.
    let deadline = std::time::Instant::now() + timeout;
    let mut drained = false;
    loop {
        match store.published_workspaces() {
            Ok(published) if published.is_empty() => {
                drained = true;
                break;
            }
            Ok(published) => {
                if std::time::Instant::now() >= deadline {
                    // `namespace/workspace`, not the bare workspace name: a CR name is only
                    // unique within its namespace, so naming just the workspace here could
                    // point an operator at the wrong tenant's slot.
                    let remaining: Vec<String> = published
                        .iter()
                        .map(|(namespace, workspace)| format!("{namespace}/{workspace}"))
                        .collect();
                    tracing::error!(
                        remaining = remaining.len(),
                        workspaces = ?remaining,
                        "draining: timed out with volumes still published; their slots \
                         will be lost with this node volume"
                    );
                    break;
                }
            }
            Err(err) => {
                tracing::error!(%err, "draining: cannot read publish records");
                break;
            }
        }
        tokio::time::sleep(poll).await;
    }

    tracing::info!(deleted, drained, "drain complete");
    Ok(Summary { deleted, drained })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::core::v1::{
        CSIVolumeSource, PersistentVolumeClaimVolumeSource, PodSpec, Volume,
    };

    fn pod_with(volumes: Vec<Volume>) -> Pod {
        Pod {
            spec: Some(PodSpec {
                volumes: Some(volumes),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn slot_volume() -> Volume {
        Volume {
            name: "workspace".to_string(),
            csi: Some(CSIVolumeSource {
                driver: DRIVER_NAME.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn selects_pods_holding_one_of_our_slots() {
        assert!(holds_a_slot(&pod_with(vec![slot_volume()])));
    }

    /// A pod on an ordinary PVC volume holds no slot; deleting it would be
    /// gratuitous disruption.
    #[test]
    fn ignores_a_pod_on_a_pvc() {
        let pod = pod_with(vec![Volume {
            name: "workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource::default()),
            ..Default::default()
        }]);
        assert!(!holds_a_slot(&pod));
    }

    #[test]
    fn ignores_another_drivers_csi_volume() {
        let pod = pod_with(vec![Volume {
            name: "other".to_string(),
            csi: Some(CSIVolumeSource {
                driver: "csi.scaleway.com".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        assert!(!holds_a_slot(&pod));
    }

    #[test]
    fn ignores_a_pod_with_no_volumes() {
        assert!(!holds_a_slot(&Pod::default()));
        assert!(!holds_a_slot(&pod_with(vec![])));
    }
}
