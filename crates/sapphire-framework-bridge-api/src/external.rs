//! External devices on the control plane (#199): clients that reach the workgroup's
//! applications with a key instead of syncing.
//!
//! The bridge owns the ledger (the workgroup root's `external_devices/`). An application's
//! server asks it to [`authenticate`](crate::EXTERNAL_DEVICE_AUTHENTICATE) a presented token
//! for its own app name; the CLIs and the GUI manage the records.

use serde::{Deserialize, Serialize};

use crate::{ApiKey, GrainId};

/// One external device, as the ledger has it. Never carries the token or its hash.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExternalDeviceInfo {
    /// Its id.
    pub id: GrainId,
    /// Its name, unique among external devices.
    pub name: String,
    /// A note for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The applications it may use.
    #[serde(default)]
    pub apps: Vec<String>,
    /// When it was added (RFC 3339).
    pub created_at: String,
    /// When its token was last replaced (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<String>,
    /// When it was retired (RFC 3339), if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<String>,
}

/// Result of [`EXTERNAL_DEVICE_LIST`](crate::EXTERNAL_DEVICE_LIST).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExternalDeviceListResult {
    /// Every record, by name.
    pub external_devices: Vec<ExternalDeviceInfo>,
}

/// Parameters of [`EXTERNAL_DEVICE_ADD`](crate::EXTERNAL_DEVICE_ADD).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalDeviceAddParams {
    /// Its name.
    pub name: String,
    /// A note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The applications it may use.
    pub apps: Vec<String>,
}

/// Parameters naming one record: retire, restore, rotate.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalDeviceSelectParams {
    /// Its name or id.
    pub selector: String,
}

/// Parameters of [`EXTERNAL_DEVICE_SET_APPS`](crate::EXTERNAL_DEVICE_SET_APPS).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalDeviceSetAppsParams {
    /// Its name or id.
    pub selector: String,
    /// The applications it may use from now on.
    pub apps: Vec<String>,
}

/// Result of [`EXTERNAL_DEVICE_ADD`](crate::EXTERNAL_DEVICE_ADD) and
/// [`EXTERNAL_DEVICE_ROTATE`](crate::EXTERNAL_DEVICE_ROTATE): the record and its new token,
/// which is never available again. `Debug` hides the token.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalDeviceTokenResult {
    /// The record.
    pub external_device: ExternalDeviceInfo,
    /// The token to give the external device.
    pub token: ApiKey,
}

/// Parameters of [`EXTERNAL_DEVICE_AUTHENTICATE`](crate::EXTERNAL_DEVICE_AUTHENTICATE).
/// `Debug` hides the token.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalDeviceAuthenticateParams {
    /// The token the client presented.
    pub token: ApiKey,
    /// The application asking, which the external device must be allowed to use.
    pub app: String,
}

/// Result of [`EXTERNAL_DEVICE_AUTHENTICATE`](crate::EXTERNAL_DEVICE_AUTHENTICATE): who it
/// is. A refusal is an error, never a result.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExternalDeviceAuthenticateResult {
    /// Its id.
    pub id: GrainId,
    /// Its name.
    pub name: String,
}

/// One management request, as either CLI builds it from its arguments.
#[derive(Clone, Debug)]
pub enum ExternalDeviceRequest {
    /// List every record.
    List,
    /// Add one.
    Add(ExternalDeviceAddParams),
    /// Retire one.
    Retire(String),
    /// Restore one.
    Restore(String),
    /// Replace one's token.
    Rotate(String),
    /// Set one's applications.
    SetApps(ExternalDeviceSetAppsParams),
}

/// What a management request answers.
#[derive(Clone, Debug)]
pub enum ExternalDeviceOutcome {
    /// The records.
    List(Vec<ExternalDeviceInfo>),
    /// One record, changed.
    One(ExternalDeviceInfo),
    /// One record and the token to hand over, once.
    WithToken(ExternalDeviceTokenResult),
}

/// One line describing a record: name, id, applications, state.
pub fn describe_external_device(d: &ExternalDeviceInfo) -> String {
    let apps = if d.apps.is_empty() {
        "no apps".to_owned()
    } else {
        d.apps.join(", ")
    };
    let state = if d.retired_at.is_some() {
        " (retired)"
    } else {
        ""
    };
    format!("{} {} [{apps}]{state}", d.name, d.id)
}

/// The lines either CLI prints for an outcome. A token is printed here, and only here.
pub fn describe_external_outcome(outcome: &ExternalDeviceOutcome) -> Vec<String> {
    match outcome {
        ExternalDeviceOutcome::List(list) if list.is_empty() => {
            vec!["no external devices".to_owned()]
        }
        ExternalDeviceOutcome::List(list) => list.iter().map(describe_external_device).collect(),
        ExternalDeviceOutcome::One(d) => vec![describe_external_device(d)],
        ExternalDeviceOutcome::WithToken(r) => vec![
            describe_external_device(&r.external_device),
            format!("token: {}", r.token.expose()),
            "This token is shown once. Give it to the external device now.".to_owned(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ExternalDeviceInfo {
        ExternalDeviceInfo {
            id: GrainId::random(),
            name: "pendant".into(),
            description: None,
            apps: vec!["sapphire-agent".into()],
            created_at: "2026-10-11T00:00:00Z".into(),
            rotated_at: None,
            retired_at: None,
        }
    }

    #[test]
    fn tokens_never_print_in_debug() {
        let r = ExternalDeviceTokenResult {
            external_device: info(),
            token: ApiKey::new("sapphire-ed-secret"),
        };
        assert!(!format!("{r:?}").contains("secret"));
        let p = ExternalDeviceAuthenticateParams {
            token: ApiKey::new("sapphire-ed-secret"),
            app: "a".into(),
        };
        assert!(!format!("{p:?}").contains("secret"));
    }

    #[test]
    fn the_token_is_printed_once_with_a_warning() {
        let lines = describe_external_outcome(&ExternalDeviceOutcome::WithToken(
            ExternalDeviceTokenResult {
                external_device: info(),
                token: ApiKey::new("sapphire-ed-xyz"),
            },
        ));
        assert_eq!(lines[1], "token: sapphire-ed-xyz");
        let listed =
            describe_external_outcome(&ExternalDeviceOutcome::List(vec![ExternalDeviceInfo {
                retired_at: Some("2026-10-12T00:00:00Z".into()),
                ..info()
            }]));
        assert!(
            listed[0].ends_with("[sapphire-agent] (retired)"),
            "{}",
            listed[0]
        );
    }
}
