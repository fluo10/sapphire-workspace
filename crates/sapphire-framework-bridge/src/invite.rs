//! Invite tickets and the file that tracks them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use data_encoding::BASE32_NOPAD;
use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};

/// What a ticket's text form starts with.
pub const TICKET_PREFIX: &str = "sapphire:";

/// How long an invite is good for unless told otherwise.
pub const DEFAULT_TTL: Duration = Duration::from_secs(10 * 60);

/// What a joiner needs to reach the inviter and prove it was invited.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ticket {
    /// The workgroup being joined.
    pub workgroup_id: GrainId,
    /// The inviter's address, as iroh encodes a `NodeAddr`.
    pub node_addr: Vec<u8>,
    /// The single-use secret.
    pub secret: [u8; 32],
    /// When the invite stops working.
    pub expires_at: DateTime<Utc>,
}

impl Ticket {
    /// The text a user copies between devices.
    ///
    /// Base32 without padding, so it survives a chat window, a QR code and a double click.
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("a ticket always encodes");
        format!("{TICKET_PREFIX}{}", BASE32_NOPAD.encode(&bytes))
    }

    /// Parse a ticket, tolerating surrounding whitespace.
    pub fn decode(text: &str) -> Result<Ticket> {
        let trimmed = text.trim();
        let body = trimmed
            .strip_prefix(TICKET_PREFIX)
            .ok_or_else(|| Error::Config(format!("a ticket starts with {TICKET_PREFIX:?}")))?;
        let bytes = BASE32_NOPAD
            .decode(body.as_bytes())
            .map_err(|e| Error::Config(format!("this is not a ticket: {e}")))?;
        postcard::from_bytes(&bytes)
            .map_err(|e| Error::Config(format!("this is not a ticket: {e}")))
    }
}

/// One outstanding invitation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Invite {
    /// Its id, for listing and revoking.
    pub id: GrainId,
    /// The secret, hex-encoded so the file stays human-readable.
    pub secret_hex: String,
    /// What the joining device will be called.
    pub device_name: String,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
    /// When it was used, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<DateTime<Utc>>,
}

impl Invite {
    /// Is this invite still usable?
    pub fn is_live(&self) -> bool {
        self.used_at.is_none() && self.expires_at > Utc::now()
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawInvites {
    #[serde(default)]
    invite: Vec<Invite>,
}

const HEADER: &str = "\
# Pending pairing invitations.
#
# Each is single use and expires. Deleting one cancels it; nothing else needs doing.
";

/// The invite file.
#[derive(Debug)]
pub struct Invites {
    path: PathBuf,
    entries: Vec<Invite>,
}

impl Invites {
    /// Read the file. A missing file is an empty list.
    pub fn load(path: &Path) -> Result<Invites> {
        let entries = match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str::<RawInvites>(&text)
                    .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?
                    .invite
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::Io(e)),
        };
        Ok(Invites {
            path: path.to_owned(),
            entries,
        })
    }

    /// Issue an invite, returning it and the secret to put in the ticket.
    pub fn create(&mut self, device_name: &str, ttl: Duration) -> Result<(Invite, [u8; 32])> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret)
            .map_err(|e| Error::Config(format!("no system random source: {e}")))?;
        let invite = Invite {
            id: GrainId::random(),
            secret_hex: secret.iter().map(|b| format!("{b:02x}")).collect(),
            device_name: device_name.to_owned(),
            expires_at: Utc::now()
                + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::zero()),
            used_at: None,
        };
        self.reload()?;
        let mut next = self.entries.clone();
        next.push(invite.clone());
        self.save(next)?;
        Ok((invite, secret))
    }

    /// Use an invite up.
    ///
    /// Re-reads the file first: any process may have issued the invite, and the one
    /// answering the pairing is not necessarily the one that created it.
    pub fn redeem(&mut self, secret: &[u8; 32]) -> Result<Invite> {
        self.take(secret, None)
    }

    /// Use an invite up, but only for the device it was issued for.
    ///
    /// The name is checked before the invite is marked used, so a joiner who mistyped the
    /// name it was invited as is refused without spending the ticket and can try again. A
    /// wrong secret, or a used or expired invite, is refused exactly as by
    /// [`redeem`](Self::redeem).
    pub fn redeem_as(&mut self, secret: &[u8; 32], device_name: &str) -> Result<Invite> {
        self.take(secret, Some(device_name))
    }

    fn take(&mut self, secret: &[u8; 32], device_name: Option<&str>) -> Result<Invite> {
        self.reload()?;
        let offered = secret
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        // Constant time, and over every entry: returning early on the first match would
        // leak which invite matched through timing.
        let mut found: Option<usize> = None;
        for (i, invite) in self.entries.iter().enumerate() {
            let hit: bool = invite
                .secret_hex
                .as_bytes()
                .ct_eq(offered.as_bytes())
                .into();
            if hit && found.is_none() {
                found = Some(i);
            }
        }
        let Some(i) = found else {
            return Err(Error::Unauthorized("no matching invite".to_owned()));
        };
        if self.entries[i].used_at.is_some() {
            return Err(Error::Unauthorized(
                "this invite was already used".to_owned(),
            ));
        }
        if self.entries[i].expires_at <= Utc::now() {
            return Err(Error::Unauthorized("this invite has expired".to_owned()));
        }
        if let Some(name) = device_name
            && name != self.entries[i].device_name
        {
            return Err(Error::Unauthorized(format!(
                "this invite is for a device named {:?}, not {:?}",
                self.entries[i].device_name, name
            )));
        }

        let mut next = self.entries.clone();
        next[i].used_at = Some(Utc::now());
        let used = next[i].clone();
        // Marked used before returning, so a concurrent second attempt loses.
        self.save(next)?;
        Ok(used)
    }

    /// Every invite, live or not.
    pub fn entries(&self) -> &[Invite] {
        &self.entries
    }

    /// Drop used and expired invites. Returns how many went.
    pub fn prune(&mut self) -> Result<usize> {
        self.reload()?;
        let before = self.entries.len();
        let next: Vec<Invite> = self
            .entries
            .iter()
            .filter(|i| i.is_live())
            .cloned()
            .collect();
        let removed = before - next.len();
        if removed > 0 {
            self.save(next)?;
        }
        Ok(removed)
    }

    fn reload(&mut self) -> Result<()> {
        self.entries = Invites::load(&self.path)?.entries;
        Ok(())
    }

    fn save(&mut self, entries: Vec<Invite>) -> Result<()> {
        let body = toml::to_string_pretty(&RawInvites {
            invite: entries.clone(),
        })
        .map_err(|e| Error::Config(e.to_string()))?;
        crate::routes::write_atomic(&self.path, HEADER, &body)?;
        self.entries = entries;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invites(dir: &std::path::Path) -> Invites {
        Invites::load(&dir.join("invites.toml")).unwrap()
    }

    #[test]
    fn a_ticket_round_trips_through_its_text_form() {
        let ticket = Ticket {
            workgroup_id: grain_id::GrainId::random(),
            node_addr: vec![1, 2, 3, 4],
            secret: [7u8; 32],
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
        };
        let text = ticket.encode();
        assert!(text.starts_with(TICKET_PREFIX), "{text}");
        let back = Ticket::decode(&text).unwrap();
        assert_eq!(back.workgroup_id, ticket.workgroup_id);
        assert_eq!(back.secret, ticket.secret);
    }

    #[test]
    fn a_ticket_survives_being_pasted_with_whitespace() {
        let ticket = Ticket {
            workgroup_id: grain_id::GrainId::random(),
            node_addr: vec![],
            secret: [1u8; 32],
            expires_at: chrono::Utc::now(),
        };
        let text = format!("  {}\n", ticket.encode());
        assert!(Ticket::decode(&text).is_ok());
    }

    #[test]
    fn a_ticket_without_the_prefix_is_refused() {
        assert!(Ticket::decode("ABCDEF").is_err());
    }

    #[test]
    fn a_corrupt_ticket_is_an_error_not_a_panic() {
        assert!(Ticket::decode("sapphire:!!!!").is_err());
    }

    #[test]
    fn an_invite_can_be_redeemed_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        let (_invite, secret) = invites.create("phone", DEFAULT_TTL).unwrap();

        assert!(invites.redeem(&secret).is_ok());
        let err = invites.redeem(&secret).unwrap_err();
        assert!(err.to_string().contains("already used"), "{err}");
    }

    #[test]
    fn redeeming_for_the_wrong_name_leaves_the_invite_live() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        let (_invite, secret) = invites.create("phone", DEFAULT_TTL).unwrap();

        let err = invites.redeem_as(&secret, "tablet").unwrap_err();
        assert!(err.to_string().contains("\"phone\""), "{err}");
        assert!(invites.entries()[0].used_at.is_none());
        assert_eq!(
            invites.redeem_as(&secret, "phone").unwrap().device_name,
            "phone"
        );
    }

    #[test]
    fn a_wrong_secret_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        invites.create("phone", DEFAULT_TTL).unwrap();

        let err = invites.redeem(&[0u8; 32]).unwrap_err();
        assert!(err.to_string().contains("no matching invite"), "{err}");
    }

    #[test]
    fn an_expired_invite_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        let (_invite, secret) = invites.create("phone", Duration::from_secs(0)).unwrap();

        let err = invites.redeem(&secret).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn redeeming_rereads_the_file_so_any_process_may_issue_invites() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reader = invites(tmp.path());

        // A different process writes an invite after `reader` was loaded.
        let mut writer = invites(tmp.path());
        let (_invite, secret) = writer.create("phone", DEFAULT_TTL).unwrap();

        assert!(
            reader.redeem(&secret).is_ok(),
            "an invite issued elsewhere must be redeemable"
        );
    }

    #[test]
    fn two_invites_are_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        let (_a, secret_a) = invites.create("phone", DEFAULT_TTL).unwrap();
        let (_b, secret_b) = invites.create("tablet", DEFAULT_TTL).unwrap();

        assert_eq!(invites.redeem(&secret_a).unwrap().device_name, "phone");
        assert_eq!(invites.redeem(&secret_b).unwrap().device_name, "tablet");
    }

    #[test]
    fn pruning_removes_expired_and_used_invites() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        let (_a, secret_a) = invites.create("used", DEFAULT_TTL).unwrap();
        invites.create("stale", Duration::from_secs(0)).unwrap();
        invites.create("live", DEFAULT_TTL).unwrap();
        invites.redeem(&secret_a).unwrap();

        assert_eq!(invites.prune().unwrap(), 2);
        assert_eq!(invites.entries().len(), 1);
        assert_eq!(invites.entries()[0].device_name, "live");
    }

    #[test]
    fn a_secret_is_thirty_two_bytes_from_the_system_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invites = invites(tmp.path());
        let (_a, first) = invites.create("a", DEFAULT_TTL).unwrap();
        let (_b, second) = invites.create("b", DEFAULT_TTL).unwrap();
        assert_ne!(first, second, "two invites must not share a secret");
    }
}
