//! Condition types a `Runner` reports as it starts up.
//!
//! These strings are a public contract, not an implementation detail. Consumers
//! match on them byte-exactly and treat a *missing* condition as unsatisfied, so
//! renaming one does not surface as an error anywhere — it silently pins every
//! runner at the phase before it, forever. They live here rather than in the
//! controller so that both the writer and its readers compile against the same
//! constant.

/// The workspace's storage is attached and usable.
///
/// Named for the `Dedicated` mechanism (a bound PVC), and deliberately kept
/// under that name for `Pooled`, where it instead reflects the runner's slot
/// mount. The mechanism differs; the meaning — "storage is ready" — does not,
/// and consumers key off the name.
pub const PVC_BOUND: &str = "PvcBound";
/// The runner's `Workspace` reports `Ready`.
pub const WORKSPACE_READY: &str = "WorkspaceReady";
/// The runner's pod has been assigned to a node.
pub const POD_SCHEDULED: &str = "PodScheduled";
/// The runner's pod is passing its readiness probe.
pub const POD_READY: &str = "PodReady";

/// Every startup condition, in the order they are fulfilled.
pub const STARTUP_CONDITIONS: [&str; 4] = [PVC_BOUND, WORKSPACE_READY, POD_SCHEDULED, POD_READY];
