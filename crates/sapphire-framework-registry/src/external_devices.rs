//! The external device ledger: clients that reach a workgroup's applications with a key.
//!
//! An external device does not sync. It talks HTTP to an application's server and proves
//! who it is with a bearer token. The ledger sits beside the device ledger, one record per
//! file named by the external device's id, and it is synced like the rest of the workgroup
//! root, so every host accepts the same keys.
//!
//! The token is never stored: a record holds its SHA-256, and the token itself is shown
//! once, when it is made. See `docs/superpowers/specs/2026-10-11-external-devices-design.md`.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::store::write_atomic;

/// Written at the top of every record file.
const HEADER: &str = "\
# A sapphire external device: a client that reaches the workgroup's apps with a key.
# The file name is its id.
#
# name         required. Unique among external devices. Accepted in place of the id.
# description  optional. A note for you; the system never reads it.
# apps         the applications it may use. Empty: none.
# token_sha256 the SHA-256 of its token. The token itself is never stored.
# created_at   when it was added.
# rotated_at   optional. When its token was last replaced.
# retired_at   optional. Set by `external-device retire`; cleared by `restore`.
";

/// What every token starts with, so a leaked one is recognisable.
pub const TOKEN_PREFIX: &str = "sapphire-ed-";

/// A token, as handed to the external device once. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A fresh token: 32 random bytes.
    pub fn generate() -> Result<Token> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| Error::File(format!("could not generate a token: {e}")))?;
        Ok(Token(format!(
            "{TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        )))
    }

    /// The token text, for the one place that shows it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The hex SHA-256 of a presented token: what a record stores and what it is compared by.
pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compare two hashes in time that does not depend on where they differ.
fn same_hash(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// One external device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDevice {
    /// Stable id; the record's file name.
    pub id: GrainId,
    /// Unique among external devices.
    pub name: String,
    /// A note for the user.
    pub description: Option<String>,
    /// The applications it may use.
    pub apps: Vec<String>,
    /// The SHA-256 of its token.
    pub token_sha256: String,
    /// When it was added.
    pub created_at: DateTime<Utc>,
    /// When its token was last replaced.
    pub rotated_at: Option<DateTime<Utc>>,
    /// When it was retired, if it is.
    pub retired_at: Option<DateTime<Utc>>,
}

impl ExternalDevice {
    /// Whether it is retired.
    pub fn is_retired(&self) -> bool {
        self.retired_at.is_some()
    }

    /// Whether it may use `app`.
    pub fn allows(&self, app: &str) -> bool {
        self.apps.iter().any(|a| a == app)
    }

    fn file_name(&self) -> String {
        format!("{}.toml", self.id)
    }
}

/// The on-file form: the id is the file name.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalDevice {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    apps: Vec<String>,
    token_sha256: String,
    created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rotated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retired_at: Option<DateTime<Utc>>,
}

/// The ledger directory, as read when it was opened. Every mutation rewrites one record.
#[derive(Debug)]
pub struct ExternalDevices {
    dir: PathBuf,
    entries: Vec<ExternalDevice>,
}

impl ExternalDevices {
    /// Read every record in `dir`. A missing directory is an empty ledger.
    pub fn open(dir: &Path) -> Result<ExternalDevices> {
        let mut entries = Vec::new();
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for entry in rd {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    let Some(stem) = name.strip_suffix(".toml") else {
                        continue;
                    };
                    let id: GrainId = stem.parse().map_err(|_| {
                        Error::File(format!(
                            "{}: the file name is not a grain-id",
                            entry.path().display()
                        ))
                    })?;
                    if stem != id.to_string() {
                        return Err(Error::File(format!(
                            "{}: the file name is not the canonical spelling of its id {id}",
                            entry.path().display()
                        )));
                    }
                    let text = std::fs::read_to_string(entry.path())?;
                    let raw: RawExternalDevice = toml::from_str(&text)
                        .map_err(|e| Error::File(format!("{}: {e}", entry.path().display())))?;
                    entries.push(ExternalDevice {
                        id,
                        name: raw.name,
                        description: raw.description,
                        apps: raw.apps,
                        token_sha256: raw.token_sha256,
                        created_at: raw.created_at,
                        rotated_at: raw.rotated_at,
                        retired_at: raw.retired_at,
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = entries.iter().find(|d| !seen.insert(d.name.as_str())) {
            return Err(Error::File(format!(
                "{}: two external devices are named {:?}",
                dir.display(),
                dup.name
            )));
        }
        Ok(ExternalDevices {
            dir: dir.to_owned(),
            entries,
        })
    }

    /// Every record, by name.
    pub fn entries(&self) -> &[ExternalDevice] {
        &self.entries
    }

    /// Add an external device for `apps`, and return it with its token — the only time
    /// the token exists outside the device that holds it.
    pub fn add(
        &mut self,
        name: &str,
        description: Option<String>,
        apps: Vec<String>,
    ) -> Result<(ExternalDevice, Token)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::File("an external device needs a name".to_owned()));
        }
        if self.entries.iter().any(|d| d.name == name) {
            return Err(Error::File(format!(
                "an external device named {name:?} already exists"
            )));
        }
        let apps = clean_apps(apps)?;
        let id = GrainId::random();
        if self.entries.iter().any(|d| d.id == id) {
            return Err(Error::File(format!(
                "generated id {id} collides with an existing external device; try again"
            )));
        }
        let token = Token::generate()?;
        let entry = ExternalDevice {
            id,
            name: name.to_owned(),
            description,
            apps,
            token_sha256: token_hash(token.expose()),
            created_at: Utc::now(),
            rotated_at: None,
            retired_at: None,
        };
        self.save_one(&entry)?;
        self.entries.push(entry.clone());
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((entry, token))
    }

    /// The record `selector` names: a name first, then an id.
    pub fn resolve(&self, selector: &str) -> Result<&ExternalDevice> {
        Ok(&self.entries[self.index_of(selector)?])
    }

    /// Retire it: its token stops working; the record stays.
    pub fn retire(&mut self, selector: &str) -> Result<ExternalDevice> {
        self.update(selector, |d| {
            if d.retired_at.is_some() {
                return false;
            }
            d.retired_at = Some(Utc::now());
            true
        })
    }

    /// Bring a retired one back, with the token it had.
    pub fn restore(&mut self, selector: &str) -> Result<ExternalDevice> {
        self.update(selector, |d| d.retired_at.take().is_some())
    }

    /// Replace its token, keeping its id. The old token stops working at once.
    pub fn rotate(&mut self, selector: &str) -> Result<(ExternalDevice, Token)> {
        let token = Token::generate()?;
        let hash = token_hash(token.expose());
        let updated = self.update(selector, |d| {
            d.token_sha256 = hash.clone();
            d.rotated_at = Some(Utc::now());
            true
        })?;
        Ok((updated, token))
    }

    /// Set the applications it may use.
    pub fn set_apps(&mut self, selector: &str, apps: Vec<String>) -> Result<ExternalDevice> {
        let apps = clean_apps(apps)?;
        self.update(selector, |d| {
            if d.apps == apps {
                return false;
            }
            d.apps = apps.clone();
            true
        })
    }

    /// The external device a presented `token` belongs to, if it may use `app` now.
    pub fn authenticate(&self, token: &str, app: &str) -> Option<&ExternalDevice> {
        let hash = token_hash(token);
        self.entries
            .iter()
            .find(|d| same_hash(&d.token_sha256, &hash))
            .filter(|d| !d.is_retired() && d.allows(app))
    }

    fn index_of(&self, selector: &str) -> Result<usize> {
        if let Some(pos) = self.entries.iter().position(|d| d.name == selector) {
            return Ok(pos);
        }
        if let Ok(id) = selector.parse::<GrainId>()
            && let Some(pos) = self.entries.iter().position(|d| d.id == id)
        {
            return Ok(pos);
        }
        Err(Error::File(format!(
            "no external device matches {selector:?}"
        )))
    }

    /// Apply `change` to one record and write it if it changed anything.
    fn update(
        &mut self,
        selector: &str,
        change: impl FnOnce(&mut ExternalDevice) -> bool,
    ) -> Result<ExternalDevice> {
        let i = self.index_of(selector)?;
        let mut updated = self.entries[i].clone();
        if change(&mut updated) {
            self.save_one(&updated)?;
            self.entries[i] = updated.clone();
        }
        Ok(updated)
    }

    fn save_one(&self, d: &ExternalDevice) -> Result<()> {
        let raw = RawExternalDevice {
            name: d.name.clone(),
            description: d.description.clone(),
            apps: d.apps.clone(),
            token_sha256: d.token_sha256.clone(),
            created_at: d.created_at,
            rotated_at: d.rotated_at,
            retired_at: d.retired_at,
        };
        let body = toml::to_string_pretty(&raw)
            .map_err(|e| Error::File(format!("{}: {e}", d.file_name())))?;
        write_atomic(&self.dir.join(d.file_name()), HEADER, &body)
    }
}

/// Trimmed, non-empty, without duplicates, in the order given.
fn clean_apps(apps: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for app in apps {
        let app = app.trim();
        if app.is_empty() {
            return Err(Error::File("an application name is empty".to_owned()));
        }
        if !out.iter().any(|a| a == app) {
            out.push(app.to_owned());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("external_devices");
        (dir, path)
    }

    #[test]
    fn add_stores_only_the_hash_and_reloads() {
        let (_d, path) = ledger();
        let mut l = ExternalDevices::open(&path).unwrap();
        let (added, token) = l
            .add(
                "pendant",
                Some("coat".into()),
                vec!["sapphire-agent".into()],
            )
            .unwrap();
        assert!(token.expose().starts_with(TOKEN_PREFIX));
        assert!(!format!("{token:?}").contains(token.expose()));
        let text = std::fs::read_to_string(path.join(format!("{}.toml", added.id))).unwrap();
        assert!(!text.contains(token.expose()), "{text}");
        assert!(text.contains(&token_hash(token.expose())));

        let again = ExternalDevices::open(&path).unwrap();
        assert_eq!(again.entries(), std::slice::from_ref(&added));
    }

    #[test]
    fn authenticate_checks_the_token_the_app_and_retirement() {
        let (_d, path) = ledger();
        let mut l = ExternalDevices::open(&path).unwrap();
        let (d, token) = l.add("pendant", None, vec!["agent".into()]).unwrap();
        let t = token.expose();

        assert_eq!(l.authenticate(t, "agent").map(|x| x.id), Some(d.id));
        assert!(
            l.authenticate(t, "journal").is_none(),
            "an app it may not use"
        );
        assert!(l.authenticate("sapphire-ed-wrong", "agent").is_none());

        l.set_apps("pendant", vec!["agent".into(), "journal".into()])
            .unwrap();
        assert!(
            l.authenticate(t, "journal").is_some(),
            "one device, two apps"
        );

        l.retire("pendant").unwrap();
        assert!(l.authenticate(t, "agent").is_none(), "retired");
        let restored = l.restore("pendant").unwrap();
        assert!(!restored.is_retired());
        assert!(
            l.authenticate(t, "agent").is_some(),
            "restored with its token"
        );
    }

    #[test]
    fn rotate_keeps_the_id_and_kills_the_old_token() {
        let (_d, path) = ledger();
        let mut l = ExternalDevices::open(&path).unwrap();
        let (d, old) = l.add("shortcut", None, vec!["agent".into()]).unwrap();
        let (rotated, new) = l.rotate(&d.id.to_string()).unwrap();
        assert_eq!(rotated.id, d.id);
        assert!(rotated.rotated_at.is_some());
        assert!(l.authenticate(old.expose(), "agent").is_none());
        assert!(l.authenticate(new.expose(), "agent").is_some());
        // Persisted.
        let again = ExternalDevices::open(&path).unwrap();
        assert!(again.authenticate(new.expose(), "agent").is_some());
    }

    #[test]
    fn names_are_unique_and_apps_are_cleaned() {
        let (_d, path) = ledger();
        let mut l = ExternalDevices::open(&path).unwrap();
        let (d, _) = l
            .add(" hook ", None, vec![" agent ".into(), "agent".into()])
            .unwrap();
        assert_eq!(
            (d.name.as_str(), d.apps.clone()),
            ("hook", vec!["agent".to_owned()])
        );
        assert!(l.add("hook", None, vec![]).is_err());
        assert!(l.add("x", None, vec![" ".into()]).is_err());
        assert!(l.add("  ", None, vec![]).is_err());
        let (none, t) = l.add("nothing-yet", None, vec![]).unwrap();
        assert!(none.apps.is_empty());
        assert!(
            l.authenticate(t.expose(), "agent").is_none(),
            "empty allows nothing"
        );
    }
}
