//! Condition types a `Runner` reports as it starts up.
//!
//! These strings are a public contract, not an implementation detail. Consumers
//! match on them byte-exactly and treat a *missing* condition as unsatisfied, so
//! renaming one does not surface as an error anywhere — it silently pins every
//! runner at the phase before it, forever. They live here rather than in the
//! controller so that both the writer and its readers compile against the same
//! constant.

/// The workspace's storage is attached and usable: the runner's slot on the
/// node data volume is mounted (or, for a claimed warm pod, the claim is
/// acked).
///
/// Named for the retired `Dedicated` mechanism (a bound PVC) and deliberately
/// kept under that name: consumers match the string byte-exactly, and renaming
/// it would silently pin every runner at the phase before it.
pub const PVC_BOUND: &str = "PvcBound";
/// The runner's `Workspace` reports `Ready`.
pub const WORKSPACE_READY: &str = "WorkspaceReady";
/// The runner's pod has been assigned to a node.
pub const POD_SCHEDULED: &str = "PodScheduled";
/// The runner's pod is passing its readiness probe.
pub const POD_READY: &str = "PodReady";

/// Every startup condition, in the order they are fulfilled.
pub const STARTUP_CONDITIONS: [&str; 4] = [PVC_BOUND, WORKSPACE_READY, POD_SCHEDULED, POD_READY];
