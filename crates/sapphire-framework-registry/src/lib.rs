//! The device ledger for a sapphire workgroup.
//!
//! One TOML file per device, named by the device's grain-id. One file per record is what
//! lets two hosts pair at the same moment without colliding: each writes its own file, and
//! the sync layer sees two independent additions rather than one contested file.
//!
//! Path conventions belong to the caller. This crate takes a directory and works inside it.

mod devices;
mod error;
mod migrate;
mod store;

pub use devices::{Device, Devices, DEFAULT_PRIORITY};
pub use error::{Error, Result};
pub use migrate::{MigrationReport, migrate_single_file};
// Re-exported so an application can name `Device::id` without depending on grain-id itself.
pub use grain_id::GrainId;
