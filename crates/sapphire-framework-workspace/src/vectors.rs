//! Synced vector files (#187): one embedding per file content, per model profile.
//!
//! `<root>/.<app>/embedded/<profile>/<sha256>.vec`, where `<sha256>` is the hex SHA-256 of
//! the file's bytes — the hash the sync layer addresses content by — and `<profile>` names
//! the model and every setting the vector depends on. Two files at one path were computed
//! from the same input, so whichever one sync keeps is right. The file is a JSON header
//! line followed by the values as little-endian f16.
//!
//! See `docs/superpowers/specs/2026-10-10-synced-vectors-design.md`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sapphire_bridge_api::EmbedModelInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The directory under `.<app>/` that holds the profiles.
pub const EMBEDDED_DIR: &str = "embedded";
/// The vector files' extension.
pub const EXTENSION: &str = "vec";
/// The format this module writes, and the only one it reads.
const FORMAT: u32 = 1;

/// The hex SHA-256 of `bytes`: a file's content address.
pub fn content_hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A model and its settings, as one directory name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// `<slug>-<dimension>-<hash8>`.
    pub name: String,
    /// What it was made from.
    pub info: EmbedModelInfo,
}

impl Profile {
    /// The profile of the model the bridge serves.
    pub fn of(info: &EmbedModelInfo) -> Profile {
        let slug: String = info
            .model
            .rsplit('/')
            .next()
            .unwrap_or(&info.model)
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug.trim_matches('-');
        let slug = if slug.is_empty() { "model" } else { slug };
        let identity = format!(
            "{}\n{}\n{}\n{}\n{}",
            info.model,
            info.revision.as_deref().unwrap_or(""),
            info.dimension,
            info.max_tokens.map(|n| n.to_string()).unwrap_or_default(),
            info.template_version
        );
        let digest = hex(&Sha256::digest(identity.as_bytes()));
        Profile {
            name: format!("{slug}-{}-{}", info.dimension, &digest[..8]),
            info: info.clone(),
        }
    }
}

/// The header line of a vector file.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
struct Header {
    format: u32,
    model: String,
    revision: Option<String>,
    dtype: String,
    dim: u32,
    max_tokens: Option<u32>,
    template_version: u32,
    content: String,
}

/// One profile's directory of vector files.
#[derive(Clone, Debug)]
pub struct VectorDir {
    dir: PathBuf,
    profile: Profile,
}

impl VectorDir {
    /// The directory for `profile` under the marker directory `marker` (`<root>/.<app>`).
    pub fn new(marker: &Path, profile: Profile) -> VectorDir {
        VectorDir {
            dir: marker.join(EMBEDDED_DIR).join(&profile.name),
            profile,
        }
    }

    /// Its path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file holding the vector of content `hash`.
    pub fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.{EXTENSION}"))
    }

    /// The vector of content `hash`, if a valid file holds one. A file of another profile,
    /// another content, the wrong length or an unknown format reads as absent.
    pub fn read(&self, hash: &str) -> Option<Vec<f32>> {
        let bytes = std::fs::read(self.path(hash)).ok()?;
        self.decode(hash, &bytes)
    }

    /// Store the vector of content `hash`: through a temporary file and a rename, so a
    /// reader — and the sync scanner — never sees half of it.
    pub fn write(&self, hash: &str, vector: &[f32]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(hash);
        // A dot-name: the sync filter skips hidden names, so the scanner never picks up
        // the temporary file.
        let tmp = self.dir.join(format!(".{hash}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, self.encode(hash, vector))?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// The content hashes this directory holds a file for, with the file's modification
    /// time.
    pub fn hashes(&self) -> Vec<(String, SystemTime)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let hash = name.strip_suffix(&format!(".{EXTENSION}"))?;
                if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((hash.to_owned(), mtime))
            })
            .collect()
    }

    /// Remove the files whose hash is not in `live` and which are older than `older_than`.
    /// Returns how many were removed.
    pub fn remove_stale(
        &self,
        live: &std::collections::HashSet<String>,
        older_than: Duration,
    ) -> usize {
        let now = SystemTime::now();
        let mut removed = 0;
        for (hash, mtime) in self.hashes() {
            if live.contains(&hash) {
                continue;
            }
            let age = now.duration_since(mtime).unwrap_or_default();
            if age < older_than {
                continue;
            }
            if std::fs::remove_file(self.path(&hash)).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    fn encode(&self, hash: &str, vector: &[f32]) -> Vec<u8> {
        let info = &self.profile.info;
        let header = Header {
            format: FORMAT,
            model: info.model.clone(),
            revision: info.revision.clone(),
            dtype: "f16".to_owned(),
            dim: vector.len() as u32,
            max_tokens: info.max_tokens,
            template_version: info.template_version,
            content: hash.to_owned(),
        };
        let mut out = serde_json::to_vec(&header).expect("a header always serializes");
        out.push(b'\n');
        out.reserve(vector.len() * 2);
        for v in vector {
            out.extend_from_slice(&half::f16::from_f32(*v).to_le_bytes());
        }
        out
    }

    fn decode(&self, hash: &str, bytes: &[u8]) -> Option<Vec<f32>> {
        let newline = bytes.iter().position(|b| *b == b'\n')?;
        let header: Header = serde_json::from_slice(&bytes[..newline]).ok()?;
        let info = &self.profile.info;
        let expected = Header {
            format: FORMAT,
            model: info.model.clone(),
            revision: info.revision.clone(),
            dtype: "f16".to_owned(),
            dim: info.dimension,
            max_tokens: info.max_tokens,
            template_version: info.template_version,
            content: hash.to_owned(),
        };
        if header != expected {
            return None;
        }
        let body = &bytes[newline + 1..];
        if body.len() != header.dim as usize * 2 {
            return None;
        }
        Some(
            body.as_chunks::<2>()
                .0
                .iter()
                .map(|c| half::f16::from_le_bytes(*c).to_f32())
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> EmbedModelInfo {
        EmbedModelInfo {
            model: "Qwen/Qwen3-VL-Embedding-2B".into(),
            dimension: 4,
            template_version: 1,
            revision: None,
            max_tokens: Some(1024),
        }
    }

    fn dir(tmp: &tempfile::TempDir) -> VectorDir {
        VectorDir::new(&tmp.path().join(".app"), Profile::of(&info()))
    }

    #[test]
    fn the_profile_is_readable_and_changes_with_every_input() {
        let base = Profile::of(&info());
        assert!(
            base.name.starts_with("qwen3-vl-embedding-2b-4-"),
            "{}",
            base.name
        );
        assert_eq!(base.name.len(), "qwen3-vl-embedding-2b-4-".len() + 8);
        let variants = [
            EmbedModelInfo {
                model: "other".into(),
                ..info()
            },
            EmbedModelInfo {
                revision: Some("abc".into()),
                ..info()
            },
            EmbedModelInfo {
                dimension: 8,
                ..info()
            },
            EmbedModelInfo {
                max_tokens: Some(512),
                ..info()
            },
            EmbedModelInfo {
                template_version: 2,
                ..info()
            },
        ];
        for v in variants {
            assert_ne!(Profile::of(&v).name, base.name, "{v:?}");
        }
    }

    #[test]
    fn a_vector_round_trips_through_f16() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(&tmp);
        let hash = content_hash(b"hello");
        let v = [0.5, -0.25, 0.123_456, 1.0];
        d.write(&hash, &v).unwrap();
        let back = d.read(&hash).unwrap();
        for (a, b) in v.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
        assert_eq!(d.hashes().len(), 1);
        let bytes = std::fs::read(d.path(&hash)).unwrap();
        let line = bytes.split(|b| *b == b'\n').next().unwrap();
        let header: serde_json::Value = serde_json::from_slice(line).unwrap();
        assert_eq!(header["dtype"], "f16");
        assert_eq!(header["content"], hash);
    }

    #[test]
    fn a_mismatched_or_broken_file_reads_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(&tmp);
        let hash = content_hash(b"x");
        // Another dimension than the profile's.
        d.write(&hash, &[1.0, 2.0]).unwrap();
        assert_eq!(d.read(&hash), None);
        // Another content than its name.
        let other = content_hash(b"y");
        d.write(&other, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        std::fs::rename(d.path(&other), d.path(&hash)).unwrap();
        assert_eq!(d.read(&hash), None);
        // Truncated.
        d.write(&hash, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        let bytes = std::fs::read(d.path(&hash)).unwrap();
        std::fs::write(d.path(&hash), &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(d.read(&hash), None);
        // Another profile reading the same directory.
        d.write(&hash, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        let other_profile = VectorDir {
            dir: d.dir().to_owned(),
            profile: Profile::of(&EmbedModelInfo {
                template_version: 9,
                ..info()
            }),
        };
        assert_eq!(other_profile.read(&hash), None);
    }

    #[test]
    fn stale_removal_keeps_live_and_young_files() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(&tmp);
        let (live, dead) = (content_hash(b"live"), content_hash(b"dead"));
        d.write(&live, &[0.0; 4]).unwrap();
        d.write(&dead, &[0.0; 4]).unwrap();
        let keep: std::collections::HashSet<String> = [live.clone()].into();
        assert_eq!(
            d.remove_stale(&keep, Duration::from_secs(3600)),
            0,
            "too young"
        );
        assert_eq!(d.remove_stale(&keep, Duration::ZERO), 1);
        assert!(d.path(&live).is_file());
        assert!(!d.path(&dead).exists());
    }
}
