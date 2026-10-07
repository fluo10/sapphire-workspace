//! The `pair/1` protocol: how a new device joins a workgroup.
//!
//! Pairing is the one exchange that happens **before** workgroup membership exists, so it
//! cannot ride the ordinary connection path — that path is authorized with the device
//! ledger, and the joiner is not in it yet. It has its own ALPN ([`PAIR_ALPN`]) and its own
//! gate: the single-use secret an invite carries, which is handed over out of band.
//!
//! The flow is one request, one response:
//!
//! 1. the joiner sends a [`JoinRequest`] — the secret, the name it wants and its node id;
//! 2. the inviter redeems the secret, writes the device record into the workgroup ledger as
//!    a **local write**, and replies [`JoinResponse::Admitted`];
//! 3. everything after that — the record reaching the other devices, the workspaces the
//!    joiner may map — goes over the ordinary sync path, which is what makes a retry of a
//!    lost reply safe.
//!
//! See `docs/superpowers/specs/2026-09-15-p2p-sync-iroh-design.md` §3.6.

use grain_id::GrainId;
use sapphire_framework_session::{read_framed, write_framed};
use sapphire_registry::Device;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{Error, Result};
use crate::invite::Invites;
use crate::workgroup::Workgroup;

/// The ALPN the pairing exchange runs on.
///
/// Deliberately separate from [`ALPN`](sapphire_bridge_api::ALPN): the data plane is gated
/// by workgroup membership, and a joiner has none yet. A listener that saw only the one
/// ALPN would have to decide from the payload whether to apply the membership check, and
/// the one connection that must skip it would look exactly like an intruder's.
pub const PAIR_ALPN: &[u8] = b"sapphire/pair/1";

/// The tag marking a pairing request, on the framing the replication session uses.
const TAG_REQUEST: u8 = 0;

/// The tag marking a pairing response, on the framing the replication session uses.
const TAG_RESPONSE: u8 = 1;

/// What a joiner sends when it wants in.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JoinRequest {
    /// The invite's secret, from the ticket.
    pub secret: [u8; 32],
    /// The name the joining device wants in the ledger.
    pub device_name: String,
    /// The joining device's node id.
    pub node_id: String,
}

/// What the inviter answers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum JoinResponse {
    /// The device is in. The joiner now knows which workgroup it belongs to, what it is
    /// called there and which record is its own — the record itself, id included, so the
    /// joiner holds byte-identical content to the record the inviter's ledger holds. The
    /// ledger is replicated, so a record that differed in anything (even a timestamp) would
    /// fork the device into two conflicting siblings.
    Admitted {
        /// The workgroup that was joined.
        workgroup_id: GrainId,
        /// Its name, as the founding device chose it.
        workgroup_name: String,
        /// This device's own record, as the inviter's ledger now holds it.
        device: Device,
        /// The inviting device's record, as the inviter's ledger holds it.
        ///
        /// The joiner's ledger starts from this record rather than nothing: a device that
        /// joined under an id that sorts before the inviter's would otherwise know no peer
        /// it may dial, and the inviter — knowing the joiner but dialing only smaller ids —
        /// would never dial it either. Membership, and the first sync it needs, arrives
        /// with the pair itself.
        inviter: Box<Device>,
    },
    /// The invite was refused, and why — a message a user can act on.
    Rejected(String),
}

/// Ask to join a workgroup, and hear the answer.
///
/// `request` travels as one frame; the [`JoinResponse`] comes back as another. Every
/// failure to speak the protocol at all is an `Err` — a refusal of the request itself is a
/// normal `JoinResponse::Rejected`, which the caller is meant to show.
pub async fn join<S>(mut stream: S, request: JoinRequest) -> Result<JoinResponse>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let payload = postcard::to_stdvec(&request)
        .map_err(|e| Error::Protocol(format!("could not encode the join request: {e}")))?;
    write_framed(&mut stream, TAG_REQUEST, &payload)
        .await
        .map_err(|e| Error::Protocol(format!("could not send the join request: {e}")))?;
    let (tag, payload) = read_framed(&mut stream)
        .await
        .map_err(|e| Error::Protocol(format!("could not read the pairing reply: {e}")))?
        .ok_or_else(|| {
            Error::Protocol("the other device closed the connection without a reply".to_owned())
        })?;
    if tag != TAG_RESPONSE {
        return Err(Error::Protocol(format!(
            "the pairing reply arrived with tag {tag}, not {TAG_RESPONSE}"
        )));
    }
    postcard::from_bytes(&payload)
        .map_err(|e| Error::Protocol(format!("could not decode the pairing reply: {e}")))
}

/// Answer one join request, and get the device this host just admitted plus its own.
///
/// The inviter's own record rides the reply as `JoinResponse::Admitted::inviter`: the
/// joiner seeds its ledger with it, so the two sides of a fresh pair know each other
/// before any replication — the first session between them is then an ordinary delta
/// exchange, not a search for someone to dial.
///
/// In this order, and no other: redeem the secret for the name the invite was issued for
/// (constant time, single use, expiry checked — [`Invites::redeem_as`]), then write the device record with the joiner's node id
/// as a **local write** to the workgroup workspace, then reply. Writing the record before
/// replying is what makes the admission durable if the reply is lost: the joiner can retry
/// the *sync*, which is idempotent, rather than the *pairing*, which is not — the invite is
/// spent the moment it redeems.
///
/// A rejected join is a normal outcome, not a failure: it answers [`JoinResponse::Rejected`]
/// and returns `Ok(None)`. `Err` is for this host's own trouble — an unreadable invite file
/// or ledger — which no reply could answer for.
///
/// The stream is left as it is after the reply; hanging up is the caller's.
pub async fn admit<S>(
    mut stream: S,
    invites: &mut Invites,
    workgroup: &Workgroup,
    inviter: &Device,
) -> Result<Option<(Device, Device)>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (tag, payload) = match read_framed(&mut stream).await {
        Ok(read) => read,
        Err(err) => {
            return Err(Error::Protocol(format!(
                "could not read the join request: {err}"
            )));
        }
    }
    .ok_or_else(|| Error::Protocol("the joiner sent nothing".to_owned()))?;
    if tag != TAG_REQUEST {
        return Err(Error::Protocol(format!(
            "the join request arrived with tag {tag}, not {TAG_REQUEST}"
        )));
    }
    let request: JoinRequest = postcard::from_bytes(&payload)
        .map_err(|e| Error::Protocol(format!("could not decode the join request: {e}")))?;

    // A rejection is the answer to this request, not a failure of the bridge: the joiner is
    // told why, and the inviter goes on to the next connection. Nothing has been written to
    // the ledger on any path through here — that starts with a redeemed invite.
    async fn reject<S>(stream: &mut S, why: String) -> Result<Option<(Device, Device)>>
    where
        S: AsyncWrite + Unpin,
    {
        reply(stream, &JoinResponse::Rejected(why)).await?;
        Ok(None)
    }

    // The secret is the whole gate: constant time, single use, expiry checked. Anything it
    // refuses writes nothing to the ledger and answers with the reason.
    //
    // The invite named the device; the joiner repeats the name it is asking for. Taking the
    // joiner's word would let one ticket name a device the inviter never saw — the point of
    // asking for a name at invite time is that the inviter knows what will appear. The name
    // is checked before the invite is spent, so a mistyped name can be corrected and retried.
    if let Err(err) = invites.redeem_as(&request.secret, &request.device_name) {
        return reject(&mut stream, err.to_string()).await;
    }

    let mut devices = workgroup.devices()?;
    if devices.by_node_id(&request.node_id).is_some() {
        return reject(
            &mut stream,
            format!(
                "a device with node id {} is already in this workgroup",
                request.node_id
            ),
        )
        .await;
    }

    // `Devices::add` refuses a duplicate name with a message naming it, which is what a
    // user can act on: pick another name. The record is written here, before the reply —
    // see the doc comment above for why the order matters.
    let device = match devices.add(&request.device_name, Some(request.node_id.clone()), None) {
        Ok(device) => device,
        Err(err) => return reject(&mut stream, err.to_string()).await,
    };

    reply(
        &mut stream,
        &JoinResponse::Admitted {
            workgroup_id: workgroup.id,
            workgroup_name: workgroup.name.clone(),
            device: device.clone(),
            inviter: Box::new(inviter.clone()),
        },
    )
    .await?;
    Ok(Some((device, inviter.clone())))
}

/// Send one response frame.
async fn reply<S>(stream: &mut S, response: &JoinResponse) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let payload = postcard::to_stdvec(response)
        .map_err(|e| Error::Protocol(format!("could not encode the pairing reply: {e}")))?;
    write_framed(stream, TAG_RESPONSE, &payload)
        .await
        .map_err(|e| Error::Protocol(format!("could not send the pairing reply: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::BridgeDir;
    use crate::invite::{DEFAULT_TTL, Invites};

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn inviter() -> (tempfile::TempDir, BridgeDir, Workgroup) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        (tmp, dir, wg)
    }

    /// Run `join` and `admit` against each other over a duplex.
    async fn pair(
        dir: &BridgeDir,
        wg: &Workgroup,
        secret: [u8; 32],
        name: &str,
        node: &str,
    ) -> (
        Result<JoinResponse>,
        Result<Option<(sapphire_registry::Device, sapphire_registry::Device)>>,
    ) {
        let (left, right) = tokio::io::duplex(8 * 1024);
        let mut invites = Invites::load(&dir.root.join("invites.toml")).unwrap();
        let inviter = wg.this_device(NODE_A).unwrap();
        tokio::join!(
            join(
                left,
                JoinRequest {
                    secret,
                    device_name: name.to_owned(),
                    node_id: node.to_owned(),
                },
            ),
            admit(right, &mut invites, wg, &inviter),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_valid_secret_admits_the_device() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", DEFAULT_TTL)
            .unwrap();

        let (joined, admitted) = pair(&dir, &wg, secret, "phone", NODE_B).await;
        match joined.unwrap() {
            JoinResponse::Admitted {
                workgroup_id,
                workgroup_name,
                ..
            } => {
                assert_eq!(workgroup_id, wg.id);
                assert_eq!(workgroup_name, "home");
            }
            other => panic!("got {other:?}"),
        }
        assert!(admitted.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_device_record_is_written_before_the_reply() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", DEFAULT_TTL)
            .unwrap();

        let (_joined, _admitted) = pair(&dir, &wg, secret, "phone", NODE_B).await;

        let devices = wg.devices().unwrap();
        let phone = devices.by_node_id(NODE_B).expect("the record must exist");
        assert_eq!(phone.name, "phone");
        assert!(!phone.is_retired());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_secret_is_rejected_and_writes_nothing() {
        let (_tmp, dir, wg) = inviter();
        Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", DEFAULT_TTL)
            .unwrap();

        let (joined, admitted) = pair(&dir, &wg, [0u8; 32], "phone", NODE_B).await;
        assert!(matches!(joined.unwrap(), JoinResponse::Rejected(_)));
        assert!(admitted.unwrap().is_none());
        assert!(wg.devices().unwrap().by_node_id(NODE_B).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_cannot_be_used_twice() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", DEFAULT_TTL)
            .unwrap();

        let (first, _) = pair(&dir, &wg, secret, "phone", NODE_B).await;
        assert!(matches!(first.unwrap(), JoinResponse::Admitted { .. }));

        let third_node = "c1".repeat(32);
        let (second, _) = pair(&dir, &wg, secret, "intruder", &third_node).await;
        assert!(matches!(second.unwrap(), JoinResponse::Rejected(_)));
        assert!(wg.devices().unwrap().by_node_id(&third_node).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mistyped_name_does_not_spend_the_invite() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", DEFAULT_TTL)
            .unwrap();

        let (wrong, admitted) = pair(&dir, &wg, secret, "my-phone", NODE_B).await;
        assert!(
            matches!(wrong.unwrap(), JoinResponse::Rejected(ref why) if why.contains("\"phone\"")),
            "the refusal must name the device the invite is for"
        );
        assert!(admitted.unwrap().is_none());
        assert!(wg.devices().unwrap().by_node_id(NODE_B).is_none());

        let (right, admitted) = pair(&dir, &wg, secret, "phone", NODE_B).await;
        assert!(
            matches!(right.unwrap(), JoinResponse::Admitted { .. }),
            "the invite must still be usable after a wrong-name attempt"
        );
        assert!(admitted.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_invite_is_rejected() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", std::time::Duration::from_secs(0))
            .unwrap();

        let (joined, _) = pair(&dir, &wg, secret, "phone", NODE_B).await;
        assert!(matches!(joined.unwrap(), JoinResponse::Rejected(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_name_that_is_taken_is_rejected() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("laptop", DEFAULT_TTL)
            .unwrap();

        // "laptop" is already this host's own name.
        let (joined, _) = pair(&dir, &wg, secret, "laptop", NODE_B).await;
        assert!(
            matches!(joined.unwrap(), JoinResponse::Rejected(ref why) if why.contains("laptop")),
            "a duplicate name must be refused with a message a user can act on"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_id_that_is_already_a_member_is_rejected() {
        let (_tmp, dir, wg) = inviter();
        let (_invite, secret) = Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("laptop-again", DEFAULT_TTL)
            .unwrap();

        let (joined, _) = pair(&dir, &wg, secret, "laptop-again", NODE_A).await;
        assert!(matches!(joined.unwrap(), JoinResponse::Rejected(_)));
    }
}
