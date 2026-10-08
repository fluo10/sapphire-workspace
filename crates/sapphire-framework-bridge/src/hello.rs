// Used from Task 5 (the bridge wiring); unused outside tests until then.
#![cfg_attr(not(test), allow(dead_code))]

use grain_id::GrainId;
use serde::{Deserialize, Serialize};

/// What a bridge tells each peer about itself, every interval and on change.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Hello {
    pub device_id: GrainId,
    pub priority: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<u8>,
    #[serde(default)]
    pub hosting: Vec<GrainId>,
    #[serde(default)]
    pub designated: Vec<GrainId>,
    #[serde(default)]
    pub backup: Vec<GrainId>,
}
