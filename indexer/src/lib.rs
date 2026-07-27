//! Reusable half of the indexer.
//!
//! The `indexer` binary walks a workspace and uploads it; the node agent needs
//! the *other* direction — pulling a workspace archive back down onto a slot.
//! Rather than reimplement manifest parsing, path safety and CRC verification a
//! second time, both share the modules here.

pub mod disk;
pub mod keys;
pub mod manifest;
pub mod python;
pub mod restore;
pub mod s3;
pub mod upload;
pub mod watcher;

/// Re-exported so consumers can match on the error variants that come back
/// through [`restore`] without pinning their own `object_store` version.
pub use object_store;
