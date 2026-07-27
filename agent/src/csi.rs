//! The CSI node plugin.
//!
//! Slots reach runner pods as **inline ephemeral volumes** rather than a PVC
//! plus `subPath`. That choice is what keeps the normal Kubernetes scheduler in
//! charge: an inline ephemeral volume imposes no topology constraint, so
//! cluster-autoscaler scale-up, drain, cordon, taints and preemption all behave
//! normally. Pinning a pod to a node by hostname — which a per-node PVC would
//! force — makes scale-up impossible, because the autoscaler simulates against
//! a template node whose hostname never matches.
//!
//! It also sidesteps two other traps: kubelet's `fsGroup` recursion (this
//! driver advertises `fsGroupPolicy: None` and chowns exactly the slot) and the
//! immutability of `volumes`/`subPath` on an existing pod.
//!
//! Only the Identity and Node services exist. There is no controller service:
//! nothing is provisioned cluster-wide, so kubelet never calls `CreateVolume`.

use std::path::{Path, PathBuf};

use tonic::{Request, Response, Status};

use crate::quota;
use crate::store::SlotStore;

pub mod proto {
    #![allow(clippy::doc_overindented_list_items)]
    tonic::include_proto!("csi.v1");
}

use proto::identity_server::{Identity, IdentityServer};
use proto::node_server::{Node, NodeServer};

/// Must match the `CSIDriver` object's name and the directory the plugin
/// registers under in kubelet's plugin directory.
pub const DRIVER_NAME: &str = "kubimo.aqora.io";
const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Volume attribute naming the workspace whose slot to mount. Set by the
/// controller in the runner pod's inline volume definition.
const ATTR_WORKSPACE: &str = "workspace";
/// Optional per-slot hard capacity limit in bytes, from `spec.storage.max`.
const ATTR_LIMIT_BYTES: &str = "limitBytes";
/// Bucket and key prefix of the workspace's S3 archive, from `spec.indexer`.
/// Absent means "do not hydrate" — used by workspaces with no archive.
const ATTR_BUCKET: &str = "bucket";
const ATTR_KEY_PREFIX: &str = "keyPrefix";

/// Owner of a slot's contents: the `me` user baked into the marimo image.
const SLOT_UID: u32 = 1000;
const SLOT_GID: u32 = 1000;

pub struct KubimoIdentity;

#[tonic::async_trait]
impl Identity for KubimoIdentity {
    async fn get_plugin_info(
        &self,
        _request: Request<proto::GetPluginInfoRequest>,
    ) -> Result<Response<proto::GetPluginInfoResponse>, Status> {
        Ok(Response::new(proto::GetPluginInfoResponse {
            name: DRIVER_NAME.to_string(),
            vendor_version: DRIVER_VERSION.to_string(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<proto::GetPluginCapabilitiesRequest>,
    ) -> Result<Response<proto::GetPluginCapabilitiesResponse>, Status> {
        // No CONTROLLER_SERVICE and no VOLUME_ACCESSIBILITY_CONSTRAINTS: slots
        // are node-local and created on demand, and advertising topology would
        // reintroduce the scheduling constraint this design exists to avoid.
        Ok(Response::new(proto::GetPluginCapabilitiesResponse {
            capabilities: vec![],
        }))
    }

    async fn probe(
        &self,
        _request: Request<proto::ProbeRequest>,
    ) -> Result<Response<proto::ProbeResponse>, Status> {
        Ok(Response::new(proto::ProbeResponse { ready: Some(true) }))
    }
}

pub struct KubimoNode {
    node_id: String,
    store: SlotStore,
    default_limit_bytes: u64,
    /// Permit slots on a filesystem without project-quota enforcement.
    ///
    /// Off by default and deliberately explicit: an unquota'd slot has no
    /// capacity limit at all, so one workspace can fill the node volume and
    /// break every other tenant on that node. Silently degrading would be the
    /// easiest way to ship unlimited slots to production by accident.
    allow_unquotaed_slots: bool,
    /// Built once from the process environment (AWS_*), shared across slots so
    /// one connection pool serves every workspace on the node.
    s3: indexer::s3::S3Client,
    /// Kubernetes client used to refresh `WorkspaceDirectory` CRs when flushing.
    /// `None` when the agent runs without cluster access, in which case slots
    /// still hydrate and mount but are never pushed back.
    client: Option<kubimo::Client>,
    /// Continuous sync task per published volume.
    ///
    /// Only bound slots appear here: an idle slot has no runner and cannot
    /// change, so the watcher count on a node tracks running runners rather
    /// than total slots.
    watchers: std::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// Give a restored tree to the runner's uid.
///
/// The agent writes as root, so without this the runner (uid 1000) cannot
/// modify its own files. `fsGroup` cannot do this job: on a shared node volume
/// kubelet would apply it to every slot on the node, not just this one.
fn chown_tree(dir: &Path) -> std::io::Result<()> {
    for entry in walkdir(dir)? {
        // `lchown`, never `chown`: `chown` follows symlinks, and this tree is
        // tenant-controlled. `restore` creates symlinks straight from the
        // manifest without validating their *targets* (only the link path is
        // checked), so a manifest entry pointing at `/etc/shadow` would have
        // this — running as node-root — hand that file to uid 1000.
        //
        // `lchown` also closes the TOCTOU: swapping a file for a symlink
        // between the walk and the chown changes nothing, because the symlink
        // itself is what gets chowned.
        std::os::unix::fs::lchown(&entry, Some(SLOT_UID), Some(SLOT_GID))?;
    }
    Ok(())
}

/// Depth-first listing of `dir` and everything under it.
///
/// Deliberately does not follow symlinks: the tree contains tenant-controlled
/// paths, and following a planted link would chown something outside the slot.
fn walkdir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = vec![dir.to_path_buf()];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    Ok(found)
}

impl KubimoNode {
    pub fn new(
        node_id: String,
        store: SlotStore,
        default_limit_bytes: u64,
        allow_unquotaed_slots: bool,
        client: Option<kubimo::Client>,
    ) -> Self {
        Self {
            node_id,
            store,
            default_limit_bytes,
            allow_unquotaed_slots,
            s3: indexer::s3::S3Client::from_env(),
            client,
            watchers: Default::default(),
        }
    }

    /// Start continuous sync for a freshly published slot.
    fn start_watcher(
        &self,
        volume_id: &str,
        workspace: &str,
        dir: &Path,
        archive: &crate::hydrate::ArchiveLocation,
    ) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let mut watchers = match self.watchers.lock() {
            Ok(watchers) => watchers,
            Err(poisoned) => poisoned.into_inner(),
        };
        if watchers.contains_key(volume_id) {
            return;
        }
        match crate::hydrate::spawn_watcher(
            dir,
            workspace,
            archive,
            client.clone(),
            self.s3.clone(),
        ) {
            Ok(handle) => {
                watchers.insert(volume_id.to_string(), handle);
                tracing::info!(workspace, "watching slot for changes");
            }
            Err(err) => tracing::warn!(%err, workspace, "could not start watcher; \
                 the slot will only be flushed when its runner stops"),
        }
    }

    /// Stop continuous sync for a volume.
    ///
    /// Aborted rather than gracefully stopped: a final flush follows
    /// immediately, so anything the watcher was midway through is redone.
    fn stop_watcher(&self, volume_id: &str) {
        let mut watchers = match self.watchers.lock() {
            Ok(watchers) => watchers,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(handle) = watchers.remove(volume_id) {
            handle.abort();
        }
    }

    /// Push a published slot's tracked files to S3.
    ///
    /// Never fails the unpublish. The slot keeps its data on disk either way, so
    /// a failed flush is recoverable — whereas refusing to unmount would leave
    /// the pod stuck Terminating and block the node from draining.
    async fn flush_published(&self, volume_id: &str) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let published = match self.store.lookup_publish(volume_id) {
            Ok(Some(published)) => published,
            // Unknown volume: the agent restarted, or this workspace was never
            // published by us. Nothing to flush.
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(%err, "could not read publish record; skipping flush");
                return;
            }
        };
        // No bucket recorded means the workspace has no archive configured;
        // there is nowhere to flush to.
        let Some(bucket) = published.bucket.clone() else {
            return;
        };
        let archive = crate::hydrate::ArchiveLocation {
            bucket,
            key_prefix: published.key_prefix.clone(),
        };
        let workspace = published.workspace;
        let dir = self.store.layout().slot_dir(&published.slot);
        tracing::info!(workspace, slot = %published.slot, "flushing slot to S3");
        if let Err(err) =
            crate::hydrate::flush_slot(&dir, &workspace, &archive, client, &self.s3).await
        {
            tracing::error!(%err, workspace, "flush failed; slot data is still on disk");
        }
    }

    /// Prepare the slot backing `workspace` and return its directory.
    ///
    /// Quota and ownership are applied only when the slot is first created:
    /// re-stamping a project id on every publish would be wasted work, and
    /// re-chowning could stomp on files the tenant deliberately made
    /// more restrictive.
    async fn prepare_slot(
        &self,
        workspace: &str,
        limit_bytes: u64,
        archive: Option<&crate::hydrate::ArchiveLocation>,
    ) -> Result<PathBuf, Status> {
        let resolved = self
            .store
            .resolve_or_create(workspace)
            .map_err(|err| Status::internal(format!("resolving slot: {err}")))?;
        let dir = self.store.layout().slot_dir(&resolved.id);
        if resolved.created {
            let quotas_enforced = quota::project_quota_enforced(self.store.layout().root())
                .map_err(|err| Status::internal(format!("checking quota support: {err}")))?;
            match (quotas_enforced, self.allow_unquotaed_slots) {
                (true, _) => {
                    // Stamp the project before anything is written: inodes
                    // created beforehand keep the old project and escape
                    // accounting.
                    quota::assign_project(&dir, resolved.project_id)
                        .map_err(|err| Status::internal(format!("assigning project: {err}")))?;
                    quota::set_project_limit(
                        self.store.layout().root(),
                        resolved.project_id,
                        limit_bytes,
                    )
                    .map_err(|err| Status::internal(format!("setting quota: {err}")))?;
                }
                (false, true) => tracing::warn!(
                    workspace,
                    slot = %resolved.id,
                    "filesystem has no project-quota enforcement and \
                     --allow-unquotaed-slots is set: this slot has NO capacity limit \
                     and can fill the node volume"
                ),
                (false, false) => {
                    return Err(Status::failed_precondition(format!(
                        "{} is not mounted with project-quota enforcement (`prjquota`), so slots \
                         would have no capacity limit. Mount the data volume with `prjquota`, or \
                         pass --allow-unquotaed-slots to accept unlimited slots.",
                        self.store.layout().root().display()
                    )));
                }
            }
            std::fs::set_permissions(
                &dir,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            )
            .map_err(|err| Status::internal(format!("setting slot permissions: {err}")))?;
            std::os::unix::fs::chown(&dir, Some(SLOT_UID), Some(SLOT_GID))
                .map_err(|err| Status::internal(format!("chowning slot: {err}")))?;
            tracing::info!(
                workspace,
                slot = %resolved.id,
                project_id = resolved.project_id,
                limit_bytes,
                "allocated slot"
            );
            // Only a freshly created slot is hydrated. Re-hydrating one that is
            // already populated would overwrite the tenant's newer local edits
            // with whatever was last synced — this is the path that makes
            // reopening a workspace with a warm slot instant.
            if let Some(archive) = archive {
                let hydrated = crate::hydrate::hydrate_slot(&dir, archive, &self.s3)
                    .await
                    .map_err(|err| Status::internal(format!("hydrating slot: {err}")))?;
                tracing::info!(workspace, slot = %resolved.id, hydrated, "slot hydrated");
                // Restored files land as root; the runner is uid 1000.
                chown_tree(&dir)
                    .map_err(|err| Status::internal(format!("chowning hydrated slot: {err}")))?;
            }
        }
        Ok(dir)
    }
}

#[tonic::async_trait]
impl Node for KubimoNode {
    async fn node_get_info(
        &self,
        _request: Request<proto::NodeGetInfoRequest>,
    ) -> Result<Response<proto::NodeGetInfoResponse>, Status> {
        Ok(Response::new(proto::NodeGetInfoResponse {
            node_id: self.node_id.clone(),
            // 0 means "no limit": slots are directories, not attachments, which
            // is the entire point — Scaleway caps block volumes at 15 per node.
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<proto::NodeGetCapabilitiesRequest>,
    ) -> Result<Response<proto::NodeGetCapabilitiesResponse>, Status> {
        // No STAGE_UNSTAGE_VOLUME: kubelet does not call NodeStageVolume for
        // inline ephemeral volumes, and there is no shared stage to set up.
        Ok(Response::new(proto::NodeGetCapabilitiesResponse {
            capabilities: vec![],
        }))
    }

    async fn node_publish_volume(
        &self,
        request: Request<proto::NodePublishVolumeRequest>,
    ) -> Result<Response<proto::NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        if request.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }
        let workspace = request
            .volume_context
            .get(ATTR_WORKSPACE)
            .ok_or_else(|| {
                Status::invalid_argument(format!("volume attribute {ATTR_WORKSPACE:?} is required"))
            })?
            .clone();
        let limit_bytes = match request.volume_context.get(ATTR_LIMIT_BYTES) {
            None => self.default_limit_bytes,
            Some(raw) => raw.parse::<u64>().map_err(|_| {
                Status::invalid_argument(format!(
                    "volume attribute {ATTR_LIMIT_BYTES:?} must be a byte count, got {raw:?}"
                ))
            })?,
        };

        let archive =
            request
                .volume_context
                .get(ATTR_BUCKET)
                .map(|bucket| crate::hydrate::ArchiveLocation {
                    bucket: bucket.clone(),
                    key_prefix: request.volume_context.get(ATTR_KEY_PREFIX).cloned(),
                });
        let slot_dir = self
            .prepare_slot(&workspace, limit_bytes, archive.as_ref())
            .await?;
        if let Some(slot) = self
            .store
            .lookup(&workspace)
            .map_err(|err| Status::internal(format!("looking up slot: {err}")))?
            && let Err(err) = self.store.record_publish(
                &request.volume_id,
                &crate::store::PublishedSlot {
                    workspace: workspace.clone(),
                    slot: slot.id,
                    bucket: archive.as_ref().map(|a| a.bucket.clone()),
                    key_prefix: archive.as_ref().and_then(|a| a.key_prefix.clone()),
                },
            )
        {
            // Not fatal: the mount is what the pod needs. Losing the record only
            // means the flush on unpublish is skipped, and the slot keeps the
            // data on disk either way.
            tracing::warn!(%err, workspace, "could not record publish; flush on stop will be skipped");
        }
        if let Some(archive) = archive.as_ref() {
            self.start_watcher(&request.volume_id, &workspace, &slot_dir, archive);
        }
        let target = Path::new(&request.target_path);
        let mounted = crate::mount::bind(&slot_dir, target, request.readonly)
            .map_err(|err| Status::internal(format!("publishing slot: {err}")))?;
        if mounted {
            tracing::info!(
                workspace,
                target = %target.display(),
                read_only = request.readonly,
                "published slot"
            );
        }
        Ok(Response::new(proto::NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<proto::NodeUnpublishVolumeRequest>,
    ) -> Result<Response<proto::NodeUnpublishVolumeResponse>, Status> {
        let request = request.into_inner();
        if request.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }
        let target = Path::new(&request.target_path);
        // Stop watching first so the final flush is not racing an in-flight
        // upload, then flush: the runner's containers have already stopped by
        // the time kubelet calls this, so the tree is quiescent.
        self.stop_watcher(&request.volume_id);
        self.flush_published(&request.volume_id).await;
        let unmounted = crate::mount::unbind(target)
            .map_err(|err| Status::internal(format!("unpublishing slot: {err}")))?;
        // The slot itself deliberately survives: keeping it lets the next open
        // of this workspace skip hydration entirely. Reclaiming is the reaper's
        // job, not the unpublish path's.
        if unmounted {
            // kubelet creates the target directory, so it is ours to remove.
            let _ = std::fs::remove_dir(target);
            self.store.forget_publish(&request.volume_id);
            tracing::info!(target = %target.display(), "unpublished slot");
        }
        Ok(Response::new(proto::NodeUnpublishVolumeResponse {}))
    }

    async fn node_stage_volume(
        &self,
        _request: Request<proto::NodeStageVolumeRequest>,
    ) -> Result<Response<proto::NodeStageVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "this driver does not advertise STAGE_UNSTAGE_VOLUME",
        ))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<proto::NodeUnstageVolumeRequest>,
    ) -> Result<Response<proto::NodeUnstageVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "this driver does not advertise STAGE_UNSTAGE_VOLUME",
        ))
    }

    async fn node_get_volume_stats(
        &self,
        _request: Request<proto::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<proto::NodeGetVolumeStatsResponse>, Status> {
        Err(Status::unimplemented(
            "this driver does not advertise GET_VOLUME_STATS",
        ))
    }

    async fn node_expand_volume(
        &self,
        _request: Request<proto::NodeExpandVolumeRequest>,
    ) -> Result<Response<proto::NodeExpandVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "slot capacity is changed via the project quota, not NodeExpandVolume",
        ))
    }
}

/// Build the gRPC server with both services registered.
///
/// Split out from [`serve`] so tests can drive it over a socket without
/// duplicating the registration, which is exactly where a proto or service-name
/// mismatch would hide.
fn router(node: KubimoNode) -> tonic::service::Routes {
    tonic::service::Routes::default()
        .add_service(IdentityServer::new(KubimoIdentity))
        .add_service(NodeServer::new(node))
}

/// Serve the plugin on a unix socket until `shutdown` resolves.
pub async fn serve(
    socket_path: &Path,
    node: KubimoNode,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // A stale socket from a previous run would make bind() fail with EADDRINUSE.
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path.display(), node_id = node.node_id, "serving CSI plugin");
    tonic::transport::Server::builder()
        .add_routes(router(node))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::UnixListenerStream::new(listener),
            shutdown,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::identity_client::IdentityClient;
    use proto::node_client::NodeClient;
    use tokio::net::UnixStream;

    /// Start the plugin on a temp socket and hand back a connected channel.
    async fn connected() -> (tempfile::TempDir, tonic::transport::Channel) {
        connected_with(true).await
    }

    async fn connected_with(
        allow_unquotaed_slots: bool,
    ) -> (tempfile::TempDir, tonic::transport::Channel) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("csi.sock");
        let store = SlotStore::new(crate::slot::SlotLayout::new(dir.path()));
        let node = KubimoNode::new("test-node".into(), store, 1024, allow_unquotaed_slots, None);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_routes(router(node))
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener))
                .await
        });
        // The URI is ignored for a unix socket, but tonic requires a valid one.
        let connect_to = socket.clone();
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(tower::service_fn(move |_| {
                let path = connect_to.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        UnixStream::connect(path).await?,
                    ))
                }
            }))
            .await
            .unwrap();
        (dir, channel)
    }

    /// kubelet reads the driver name from here and matches it against the
    /// `CSIDriver` object; a mismatch makes every mount fail with "driver not
    /// found".
    #[tokio::test]
    async fn identity_reports_the_registered_driver_name() {
        let (_dir, channel) = connected().await;
        let info = IdentityClient::new(channel)
            .get_plugin_info(proto::GetPluginInfoRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(info.name, DRIVER_NAME);
        assert!(!info.vendor_version.is_empty());
    }

    #[tokio::test]
    async fn node_reports_its_id_and_no_volume_limit() {
        let (_dir, channel) = connected().await;
        let info = NodeClient::new(channel)
            .node_get_info(proto::NodeGetInfoRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(info.node_id, "test-node");
        // Slots are directories, not attachments — the whole point.
        assert_eq!(info.max_volumes_per_node, 0);
    }

    /// Without the `workspace` attribute the driver cannot know whose slot to
    /// mount, and must refuse rather than invent one.
    #[tokio::test]
    async fn publish_requires_the_workspace_attribute() {
        let (_dir, channel) = connected().await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("workspace"));
    }

    #[tokio::test]
    async fn publish_rejects_a_non_numeric_limit() {
        let (_dir, channel) = connected().await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target".into(),
                volume_context: [
                    (ATTR_WORKSPACE.to_string(), "bmow-abc".to_string()),
                    (ATTR_LIMIT_BYTES.to_string(), "not-a-number".to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Unpublishing a path that was never mounted is a normal retry, not an
    /// error — kubelet repeats the call until it succeeds.
    #[tokio::test]
    async fn unpublish_is_idempotent_for_an_unmounted_target() {
        let (dir, channel) = connected().await;
        let target = dir.path().join("never-mounted");
        NodeClient::new(channel)
            .node_unpublish_volume(proto::NodeUnpublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: target.display().to_string(),
            })
            .await
            .expect("unpublish of an unmounted target should succeed");
    }

    /// The safety default: on a filesystem without project-quota enforcement
    /// (a temp dir is ext4/tmpfs here), publishing must refuse rather than
    /// silently hand out a slot with no capacity limit.
    #[tokio::test]
    async fn publish_refuses_unquotaed_slots_by_default() {
        let (_dir, channel) = connected_with(false).await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target-quota".into(),
                volume_context: [(ATTR_WORKSPACE.to_string(), "bmow-abc".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("prjquota"),
            "the error should say how to fix it, got: {}",
            status.message()
        );
    }

    /// The traversal half of the symlink defence: a symlinked *directory* must
    /// not be descended into, or the agent would walk — and chown — a tree
    /// outside the slot entirely.
    #[tokio::test]
    async fn walkdir_does_not_descend_symlinked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"host file").unwrap();

        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("own-file"), b"mine").unwrap();
        std::os::unix::fs::symlink(&outside, slot.join("escape")).unwrap();

        let walked = walkdir(&slot).unwrap();
        // The symlink itself is visited (so it gets lchown'd) ...
        assert!(walked.iter().any(|p| p.ends_with("escape")));
        assert!(walked.iter().any(|p| p.ends_with("own-file")));
        // ... but nothing behind it is.
        assert!(
            !walked.iter().any(|p| p.ends_with("secret")),
            "walked outside the slot through a symlink: {walked:?}"
        );
    }

    /// Advertising STAGE_UNSTAGE would make kubelet call NodeStageVolume, which
    /// this driver does not implement.
    #[tokio::test]
    async fn node_advertises_no_staging_capability() {
        let (_dir, channel) = connected().await;
        let caps = NodeClient::new(channel)
            .node_get_capabilities(proto::NodeGetCapabilitiesRequest {})
            .await
            .unwrap()
            .into_inner();
        assert!(caps.capabilities.is_empty());
    }
}
