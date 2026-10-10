//! Transport-agnostic replication core for sapphire-framework workspaces.
//!
//! See `docs/superpowers/specs/2026-09-15-p2p-sync-iroh-design.md`, section 2.

mod entry;
mod error;
mod filter;
mod hash;
mod hlc;
mod id;
mod merge;
pub mod paths;
mod replica;
mod report;
mod state;
mod store;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
mod vv;

pub use entry::{Content, Entry, PathUpdate};
pub use error::{Error, Result};
pub use filter::{EMBEDDED_DIR, IGNORE_FILE, SyncFilter};
pub use hash::{ContentHash, ParseHashError};
pub use hlc::{Clock, Hlc, MAX_DRIFT_MS, SystemClock};
pub use id::ReplicaId;
pub use merge::{conflict_path, join, needs_copy, winner};
pub use replica::{ContentSource, DEFAULT_MAX_FILE_SIZE, Replica, ReplicaConfig};
pub use report::{Conflict, PauseReason, Report, ScanOutcome, SkipReason, Skipped};
pub use state::{DiskState, PathState};
pub use store::{FORMAT_VERSION, Meta, ReplicaStore};
pub use vv::{Dot, VersionVector};
