//! Finding runners left on a dead slot mount, and recreating them.
//!
//! The shutdown drain covers planned agent replacement. Nothing covers the rest: a node
//! crash, a SIGKILL, a drain that timed out. In those cases the runner keeps its bind
//! mount while the data volume behind it goes away, and the result is remarkably quiet:
//!
//!   - kubelet does **not** report a problem. A container restarted against such a mount
//!     starts successfully and reports `Ready`; there is no `CreateContainerError` and no
//!     waiting reason, because the mount is structurally present and only its I/O fails.
//!   - marimo's liveness probe does not notice. `/health` returns a constant and makes no
//!     syscall, so it answers 200 from a workspace that cannot read a single file.
//!
//! So the controller-side recycle, which keys on a container error, cannot see this shape
//! at all. The agent can: it is the only thing that can ask the mount itself whether it
//! still works. That is what this does — and the answer is unambiguous, because
//! [`mount::MountState::Stale`] means precisely "a mount entry exists and the filesystem
//! behind it is gone".
//!
//! The remedy is to delete the pod, not to unmount. Clearing the mount without rebinding
//! would hand the runner an empty directory, which is a quieter failure than the one it
//! replaces.

use std::path::{Path, PathBuf};

use kubimo::Client;
use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::kube::api::{DeleteParams, ListParams};

use crate::csi::DRIVER_NAME;
use crate::mount::{self, MountState};

/// Where kubelet's per-pod volume directories are, *as this container sees them*.
///
/// The DaemonSet mounts the host's kubelet pods directory here, and kubelet's own
/// `target_path` uses the same prefix, which is why a path can be handed straight to
/// [`mount::mount_state`].
pub const DEFAULT_KUBELET_PODS_DIR: &str = "/var/lib/kubelet/pods";

/// Where kubelet publishes `volume_name` for `pod_uid`.
///
/// Mirrors the `target_path` kubelet passes to `NodePublishVolume`; reconstructing it is
/// what lets the sweep check a mount without having any record of having created it —
/// which is the situation a *replacement* agent is always in.
fn target_path(pods_dir: &Path, pod_uid: &str, volume_name: &str) -> PathBuf {
    pods_dir
        .join(pod_uid)
        .join("volumes/kubernetes.io~csi")
        .join(volume_name)
        .join("mount")
}

/// Names of the pod's volumes that this driver serves.
fn slot_volume_names(pod: &Pod) -> Vec<String> {
    pod.spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .map(|volumes| {
            volumes
                .iter()
                .filter(|volume| {
                    volume
                        .csi
                        .as_ref()
                        .is_some_and(|csi| csi.driver == DRIVER_NAME)
                })
                .map(|volume| volume.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Delete pods on this node whose slot mount is dead. Returns how many were deleted.
pub async fn run(
    client: &Client,
    node_name: &str,
    pods_dir: &Path,
) -> Result<usize, Box<dyn std::error::Error>> {
    let pods = client.api_global::<Pod>();
    let params = ListParams::default().fields(&format!("spec.nodeName={node_name}"));
    let all = pods.kube().list(&params).await?;

    let mut deleted = 0;
    for pod in &all.items {
        // Already on its way out; deleting again achieves nothing.
        if pod.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let (Some(uid), Some(name)) = (pod.metadata.uid.as_deref(), pod.metadata.name.as_deref())
        else {
            continue;
        };
        let namespace = pod.metadata.namespace.as_deref().unwrap_or("default");

        for volume_name in slot_volume_names(pod) {
            let target = target_path(pods_dir, uid, &volume_name);
            match mount::mount_state(&target) {
                Ok(MountState::Stale) => {}
                // Live is the normal case; Absent means kubelet has not published it yet,
                // or already cleaned up. Neither is a reason to delete a running pod.
                Ok(_) => continue,
                Err(err) => {
                    tracing::warn!(%err, target = %target.display(), "cannot classify mount");
                    continue;
                }
            }
            tracing::warn!(
                pod = name,
                namespace,
                target = %target.display(),
                "slot mount is dead; deleting the pod so it is recreated against this agent"
            );
            match client
                .api_namespaced::<Pod>(namespace)
                .kube()
                .delete(name, &DeleteParams::default())
                .await
            {
                Ok(_) => deleted += 1,
                // One undeletable pod must not stop the sweep reaching the others.
                Err(err) => tracing::error!(%err, pod = name, namespace, "could not delete pod"),
            }
            break;
        }
    }
    if deleted > 0 {
        tracing::info!(deleted, "swept pods off dead slot mounts");
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::core::v1::{CSIVolumeSource, PodSpec, Volume};

    fn pod_with(volumes: Vec<Volume>) -> Pod {
        Pod {
            spec: Some(PodSpec {
                volumes: Some(volumes),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn csi_volume(name: &str, driver: &str) -> Volume {
        Volume {
            name: name.to_string(),
            csi: Some(CSIVolumeSource {
                driver: driver.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Must match the `target_path` kubelet passes to `NodePublishVolume` exactly; if it
    /// drifts, the sweep silently checks paths that do not exist and finds nothing wrong
    /// with a node full of dead mounts.
    #[test]
    fn target_path_matches_kubelets_layout() {
        let path = target_path(Path::new("/var/lib/kubelet/pods"), "abc-123", "workspace");
        assert_eq!(
            path,
            Path::new("/var/lib/kubelet/pods/abc-123/volumes/kubernetes.io~csi/workspace/mount")
        );
    }

    #[test]
    fn selects_only_our_driver() {
        let pod = pod_with(vec![
            csi_volume("workspace", DRIVER_NAME),
            csi_volume("other", "csi.scaleway.com"),
        ]);
        assert_eq!(slot_volume_names(&pod), vec!["workspace".to_string()]);
    }

    #[test]
    fn ignores_pods_without_our_volumes() {
        assert!(slot_volume_names(&Pod::default()).is_empty());
        assert!(slot_volume_names(&pod_with(vec![])).is_empty());
        let pod = pod_with(vec![csi_volume("other", "csi.scaleway.com")]);
        assert!(slot_volume_names(&pod).is_empty());
    }
}
