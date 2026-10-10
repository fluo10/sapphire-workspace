//! External devices (#199): the ledger in the workgroup root, and the requests on it.
//!
//! Shared by the control plane and by the CLI when no bridge runs, so both carry a request
//! out the same way.

use sapphire_bridge_api::{
    ApiKey, ExternalDeviceAuthenticateResult, ExternalDeviceInfo, ExternalDeviceOutcome,
    ExternalDeviceRequest, ExternalDeviceTokenResult,
};
use sapphire_registry::{ExternalDevice, ExternalDevices};

use crate::error::{Error, Result};
use crate::workgroup::Workgroup;

/// What the wire carries of a record: never the token's hash.
fn info(d: &ExternalDevice) -> ExternalDeviceInfo {
    ExternalDeviceInfo {
        id: d.id,
        name: d.name.clone(),
        description: d.description.clone(),
        apps: d.apps.clone(),
        created_at: d.created_at.to_rfc3339(),
        rotated_at: d.rotated_at.map(|t| t.to_rfc3339()),
        retired_at: d.retired_at.map(|t| t.to_rfc3339()),
    }
}

/// Carry out one management request on `workgroup`'s ledger.
pub(crate) fn carry_out(
    workgroup: &Workgroup,
    request: ExternalDeviceRequest,
) -> Result<ExternalDeviceOutcome> {
    // Opened afresh for every request: the ledger is synced, and a change from another
    // device must be seen before this one is applied on top of it.
    let mut ledger = workgroup.external_devices()?;
    let with_token = |(d, token): (ExternalDevice, sapphire_registry::Token)| {
        ExternalDeviceOutcome::WithToken(ExternalDeviceTokenResult {
            external_device: info(&d),
            token: ApiKey::new(token.expose()),
        })
    };
    Ok(match request {
        ExternalDeviceRequest::List => {
            ExternalDeviceOutcome::List(ledger.entries().iter().map(info).collect())
        }
        ExternalDeviceRequest::Add(p) => with_token(ledger.add(&p.name, p.description, p.apps)?),
        ExternalDeviceRequest::Retire(s) => ExternalDeviceOutcome::One(info(&ledger.retire(&s)?)),
        ExternalDeviceRequest::Restore(s) => ExternalDeviceOutcome::One(info(&ledger.restore(&s)?)),
        ExternalDeviceRequest::Rotate(s) => with_token(ledger.rotate(&s)?),
        ExternalDeviceRequest::SetApps(p) => {
            ExternalDeviceOutcome::One(info(&ledger.set_apps(&p.selector, p.apps)?))
        }
    })
}

/// Who presented `token` to `app`. The refusal says nothing about why: a client probing
/// tokens learns no more than "no".
pub(crate) fn authenticate(
    workgroup: &Workgroup,
    token: &ApiKey,
    app: &str,
) -> Result<ExternalDeviceAuthenticateResult> {
    let ledger: ExternalDevices = workgroup.external_devices()?;
    match ledger.authenticate(token.expose(), app) {
        Some(d) => Ok(ExternalDeviceAuthenticateResult {
            id: d.id,
            name: d.name.clone(),
        }),
        None => Err(Error::Config("the token was refused".to_owned())),
    }
}
