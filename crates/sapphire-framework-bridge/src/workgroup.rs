//! The workgroup this host belongs to, and who is allowed to connect.

use std::path::PathBuf;

use grain_id::GrainId;
use sapphire_registry::{Device, Devices};
use serde::{Deserialize, Serialize};

use crate::dir::BridgeDir;
use crate::error::{Error, Result};
use crate::invite::Ticket;
use crate::net::NetConfig;
use crate::pairing::{self, JoinRequest, JoinResponse};
use crate::peer::PeerTransport;

/// The ignore file written into every workgroup root at `create` and `join`.
///
/// The workgroup's root is not a user's workspace — it is the workgroup's own state
/// (`workgroup.toml`, `devices/`, `workspaces/`, `net.toml`), and every file in it has
/// exactly one right answer at any moment: whatever device wrote it last. A retirement
/// arriving over a device record this host had not yet seen is *meant* to overwrite it.
/// Keeping the superseded version alive as a `devices/*.conflict-*.toml` copy would
/// leave a file the ledger cannot open — a record's file name is a grain-id, and a
/// conflict copy's name is not — and every authorization and every dial after that
/// fails until a human intervenes. So the workgroup root bans conflict copies, and
/// last-writer-wins is the whole story. Nothing is lost by it: both versions sit in
/// every replica's store, and the winner is what every device converges on.
///
/// The ignore file is itself inside the synced root, so it replicates with the
/// workgroup: a device that joined before this rule existed adopts it at its next
/// session with any founder whose root already carries it.
const IGNORE_BODY: &str = "*.conflict-*\n";

/// Write [`IGNORE_BODY`] as the workgroup root's ignore file, unless it is already
/// exactly that. A version a future change ships simply replaces what is there.
fn write_ignore_file(root: &std::path::Path) -> Result<()> {
    let file = root.join(sapphire_sync::IGNORE_FILE);
    if std::fs::read_to_string(file.clone()).ok().as_deref() == Some(IGNORE_BODY) {
        return Ok(());
    }
    std::fs::write(file, IGNORE_BODY).map_err(Error::Io)
}

/// The `workgroup.toml` inside a workgroup's root: what the workgroup says about itself.
///
/// It lives in the workgroup's synced root, so every device of the workgroup sees the same
/// name once the workgroup is shared.
#[derive(Debug, Deserialize, Serialize)]
struct WorkgroupFile {
    name: String,
}

/// A set of devices that share workspaces.
#[derive(Clone, Debug)]
pub struct Workgroup {
    /// Its id.
    pub id: GrainId,
    /// Its name.
    pub name: String,
    /// `<bridge dir>/workgroups/<id>/`.
    pub dir: PathBuf,
    /// `<bridge dir>/workgroups/<id>/root/devices/`, the ledger directory.
    devices_dir: PathBuf,
}

/// One workspace the workgroup knows about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkgroupWorkspace {
    /// Its identity across devices. Carried by the file name, not repeated inside.
    pub workspace_id: GrainId,
    /// Which application owns it.
    pub app_name: String,
    /// A human-chosen name, used as a selector.
    pub name: String,
}

/// The on-file form of a workspace entry. The id is the file name, so it is not a field
/// here — the same convention as a device record.
#[derive(Debug, Deserialize, Serialize)]
struct RawWorkspace {
    app_name: String,
    name: String,
}

impl Workgroup {
    /// Create a workgroup and write this device's own record into it.
    ///
    /// The founding device needs a record before its first sync, because `Entry.author` is
    /// its device id.
    ///
    /// The first release allows one workgroup per host; the layout and the wire format
    /// support several, so lifting the limit is a CLI change.
    pub fn create(
        dir: &BridgeDir,
        name: &str,
        this_device: &str,
        node_id: &str,
    ) -> Result<Workgroup> {
        if Workgroup::open(dir)?.is_some() {
            return Err(Error::Config(
                "this host already belongs to a workgroup".to_owned(),
            ));
        }
        let id = GrainId::random();
        let wg_dir = dir.workgroup_dir(id);
        std::fs::create_dir_all(wg_dir.join("root"))?;
        std::fs::create_dir_all(dir.devices_dir(id))?;
        write_ignore_file(&wg_dir.join("root"))?;
        let file = wg_dir.join("root").join("workgroup.toml");
        std::fs::write(
            &file,
            toml::to_string_pretty(&WorkgroupFile {
                name: name.to_owned(),
            })
            .map_err(|e| Error::Config(format!("{}: {e}", file.display())))?,
        )?;

        let workgroup = Workgroup {
            id,
            name: name.to_owned(),
            dir: wg_dir,
            devices_dir: dir.devices_dir(id),
        };
        let mut devices = workgroup.devices()?;
        devices.add(this_device, Some(node_id.to_owned()), None)?;
        Ok(workgroup)
    }

    /// The workgroup this host belongs to, if any.
    pub fn open(dir: &BridgeDir) -> Result<Option<Workgroup>> {
        let entries = match std::fs::read_dir(dir.workgroups_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            // A directory that is not named after a workgroup id is not one: a leftover
            // temporary file, or something a user put here.
            let Some(id) = name.to_str().and_then(|s| s.parse::<GrainId>().ok()) else {
                continue;
            };
            let file = entry.path().join("root").join("workgroup.toml");
            let text = std::fs::read_to_string(&file).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::Config(format!(
                        "{}: there is no workgroup here ({} is missing)",
                        entry.path().display(),
                        file.display()
                    ))
                } else {
                    Error::Io(e)
                }
            })?;
            let parsed: WorkgroupFile = toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", file.display())))?;
            return Ok(Some(Workgroup {
                id,
                name: parsed.name,
                dir: entry.path(),
                devices_dir: dir.devices_dir(id),
            }));
        }
        Ok(None)
    }

    /// The device ledger, read fresh.
    ///
    /// Always reads from disk: the ledger is synced, so a pairing or a retirement that
    /// arrived from another device must take effect without restarting the bridge.
    pub fn devices(&self) -> Result<Devices> {
        Ok(Devices::open(&self.devices_dir)?)
    }

    /// This host's own record: the one whose `node_id` is this host's.
    ///
    /// A `join`ed host did not found its workgroup, so its record may sort anywhere in the
    /// ledger; which record is ours is a question only this host's node id can answer.
    pub fn this_device(&self, node_id: &str) -> Result<Device> {
        self.devices()?.by_node_id(node_id).cloned().ok_or_else(|| {
            Error::Config(format!(
                "no device of this workgroup has the node id {node_id}"
            ))
        })
    }

    /// Retire the device `selector` names, unless it is this host's own.
    ///
    /// Shared by the control plane and the bridge CLI's offline path. Retiring oneself would
    /// lock this host out of its own workgroup at the next authorization, which no user means
    /// to do from a device list.
    pub fn retire_device(&self, selector: &str, this_node_id: &str) -> Result<Device> {
        let mut devices = self.devices()?;
        let target = devices.resolve(selector)?.clone();
        if target.node_id.as_deref() == Some(this_node_id) {
            return Err(Error::Config(format!(
                "{} is this host's own device; retire it from another device",
                target.name
            )));
        }
        Ok(devices.retire(selector)?)
    }

    /// Join the workgroup a ticket names, and write it into this bridge directory.
    ///
    /// Dials the ticket's address over the pairing protocol
    /// ([`PAIR_ALPN`](crate::pairing::PAIR_ALPN)), and on an
    /// [`Admitted`](crate::pairing::JoinResponse::Admitted) answer creates the workgroup
    /// directory — the id and the
    /// name come from the answer, so both sides agree on them without a second exchange —
    /// and writes this host's own device record under the id the inviter assigned: the
    /// record already exists in the inviter's ledger, and the ledger is replicated, so a
    /// second id for one device would fork it. A refused join removes everything it
    /// created, so the host may simply try again.
    pub async fn join(
        dir: &BridgeDir,
        ticket: &Ticket,
        device_name: &str,
        transport: &dyn PeerTransport,
    ) -> Result<Workgroup> {
        if Workgroup::open(dir)?.is_some() {
            return Err(Error::Config(
                "this host already belongs to a workgroup".to_owned(),
            ));
        }

        let node_id = transport.node_id();
        let stream = transport.open_pairing(&ticket.node_addr).await?;
        let request = JoinRequest {
            secret: ticket.secret,
            device_name: device_name.to_owned(),
            node_id: node_id.clone(),
        };
        let response = pairing::join(stream, request).await?;
        let (workgroup_id, workgroup_name, own, inviter) = match response {
            JoinResponse::Admitted {
                workgroup_id,
                workgroup_name,
                device,
                inviter,
            } => (workgroup_id, workgroup_name, device, inviter),
            JoinResponse::Rejected(why) => {
                return Err(Error::Unauthorized(format!(
                    "the invite was refused: {why}"
                )));
            }
        };

        match Workgroup::materialize(dir, workgroup_id, &workgroup_name, &own, &inviter) {
            Ok(workgroup) => Ok(workgroup),
            Err(err) => {
                // Whatever half of it landed goes away again: a workgroup that was not
                // joined must not be one this host "already belongs to".
                std::fs::remove_dir_all(dir.workgroup_dir(workgroup_id)).ok();
                Err(err)
            }
        }
    }

    /// Write a workgroup directory with `id` and `name`, this host's record and the
    /// inviter's.
    ///
    /// Both records are the ones the inviter's ledger holds, byte for byte: the ledger is
    /// replicated, so a local lookalike that differed in anything — even the timestamp the
    /// joiner would have to invent — makes the replication see two competing versions of
    /// one device, and every device's ledger drowns in conflict copies.
    ///
    /// The inviter's record is what lets the pair meet without a third party: replication
    /// runs over the ordinary connection path, which is authorized with this ledger, and
    /// the ledger the join starts with names only its own device. Which side of a pair
    /// dials is an id-ordering rule (see `wgsync`), and an id the chance of two random
    /// grain-ids decides; on the wrong side of it, a joiner with no record but its own and
    /// an inviter that dials only smaller ids would never open a stream to each other, and
    /// the workgroup would grow only as far as the pairs whose ids happened to line up.
    /// A member known on both sides at pair time needs no dial to be learned.
    pub(crate) fn materialize(
        dir: &BridgeDir,
        id: GrainId,
        name: &str,
        own: &Device,
        inviter: &Device,
    ) -> Result<Workgroup> {
        let wg_dir = dir.workgroup_dir(id);
        // `join` only gets here when `open` reports no workgroup, so anything already
        // sitting under this id is a leftover of a crashed attempt — possibly with a
        // record of a device this host no longer is. Start clean; a workgroup that was
        // successfully joined would have been refused above.
        std::fs::remove_dir_all(&wg_dir).ok();
        std::fs::create_dir_all(wg_dir.join("root"))?;
        std::fs::create_dir_all(dir.devices_dir(id))?;
        write_ignore_file(&wg_dir.join("root"))?;
        let file = wg_dir.join("root").join("workgroup.toml");
        std::fs::write(
            &file,
            toml::to_string_pretty(&WorkgroupFile {
                name: name.to_owned(),
            })
            .map_err(|e| Error::Config(format!("{}: {e}", file.display())))?,
        )?;

        let workgroup = Workgroup {
            id,
            name: name.to_owned(),
            dir: wg_dir,
            devices_dir: dir.devices_dir(id),
        };
        Devices::write_record(&workgroup.devices_dir, own)?;
        // The inviter may be this device — a founder answering its own later `join` cannot
        // happen (the node id is already a member, and is refused), so this only guards a
        // test that reuses one record.
        if inviter.id != own.id {
            Devices::write_record(&workgroup.devices_dir, inviter)?;
        }
        Ok(workgroup)
    }

    /// The device behind `node_id`, if it may connect.
    ///
    /// Reads the ledger on every call: authorization is a live question, and a retirement
    /// that arrived from another device must close the door without a restart.
    pub fn authorize(&self, node_id: &str) -> Result<Device> {
        let devices = self.devices()?;
        let Some(device) = devices.by_node_id(node_id) else {
            return Err(Error::Unauthorized(format!(
                "{node_id} is not a device of this workgroup"
            )));
        };
        if device.is_retired() {
            return Err(Error::Unauthorized(format!(
                "the device {} ({node_id}) is retired",
                device.name
            )));
        }
        Ok(device.clone())
    }

    /// The workgroup's workspace list directory, inside the synced root.
    fn workspaces_dir(&self) -> PathBuf {
        self.dir.join("root").join("workspaces")
    }

    /// Every workspace the workgroup knows about.
    ///
    /// One file per workspace under the synced root; the file name is the workspace's id,
    /// and the file says which application owns it and what it is called. The list is
    /// ordered by name, so a listing is stable across hosts whose directories enumerate
    /// differently.
    pub fn workspaces(&self) -> Result<Vec<WorkgroupWorkspace>> {
        let dir = self.workspaces_dir();
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::Io(e)),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".toml") else {
                continue;
            };
            let workspace_id: GrainId = stem.parse().map_err(|_| {
                Error::Config(format!(
                    "{}: the file name is not a grain-id",
                    entry.path().display()
                ))
            })?;
            let text = std::fs::read_to_string(entry.path())?;
            let raw: RawWorkspace = toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", entry.path().display())))?;
            out.push(WorkgroupWorkspace {
                workspace_id,
                app_name: raw.app_name,
                name: raw.name,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Announce a workspace to the workgroup, or update its entry.
    ///
    /// One file per workspace, for the same reason as one file per device: two hosts
    /// publishing at the same moment write different files instead of contending for one.
    pub fn publish_workspace(&self, ws: &WorkgroupWorkspace) -> Result<()> {
        let dir = self.workspaces_dir();
        std::fs::create_dir_all(&dir)?;
        let body = toml::to_string_pretty(&RawWorkspace {
            app_name: ws.app_name.clone(),
            name: ws.name.clone(),
        })
        .map_err(|e| Error::Config(e.to_string()))?;
        crate::routes::write_atomic(
            &dir.join(format!("{}.toml", ws.workspace_id)),
            "# A workspace of this workgroup. The file name is its id.\n",
            &body,
        )
    }

    /// Announce this workgroup's network configuration, replacing what was published before.
    ///
    /// What a self-hosted server writes so its devices learn about its relay: the file lives
    /// in the synced root, so every device of the workgroup reads the same one. Each device
    /// then merges it with its own `net.toml` in [`relays`](crate::relays) — announced never
    /// replaces local.
    pub fn publish_net(&self, net: &NetConfig) -> Result<()> {
        crate::routes::write_atomic(
            &self.net_toml(),
            "# The network configuration this workgroup announces to its devices.\n",
            &toml::to_string_pretty(net)
                .map_err(|e| Error::Config(format!("could not encode net.toml: {e}")))?,
        )
    }

    /// The workgroup's published [`NetConfig`], or the defaults when nothing is published.
    ///
    /// A missing file contributes nothing rather than failing: a workgroup that has not
    /// announced a configuration is a normal state, not a broken one. An unreadable one
    /// fails, for the same reason an unreadable `workgroup.toml` does — guessing at what a
    /// half-written configuration meant is worse than reporting it.
    pub fn published_net(&self) -> Result<NetConfig> {
        match std::fs::read_to_string(self.net_toml()) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", self.net_toml().display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NetConfig::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// The workgroup's published `root/net.toml`, inside the synced root.
    fn net_toml(&self) -> PathBuf {
        self.dir.join("root").join("net.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::BridgeDir;
    use crate::net::NetConfig;

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn bridge_dir() -> (tempfile::TempDir, BridgeDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        (tmp, dir)
    }

    #[test]
    fn creating_a_workgroup_writes_this_devices_record() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();

        let me = wg.this_device(NODE_A).unwrap();
        assert_eq!(me.name, "laptop");
        assert_eq!(me.node_id.as_deref(), Some(NODE_A));
    }

    #[test]
    fn a_created_workgroup_reopens() {
        let (_tmp, dir) = bridge_dir();
        let created = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let reopened = Workgroup::open(&dir).unwrap().expect("a workgroup");
        assert_eq!(reopened.id, created.id);
        assert_eq!(reopened.name, "home");
    }

    #[test]
    fn a_host_without_a_workgroup_reports_none() {
        let (_tmp, dir) = bridge_dir();
        assert!(Workgroup::open(&dir).unwrap().is_none());
    }

    #[test]
    fn a_second_workgroup_is_refused_for_now() {
        let (_tmp, dir) = bridge_dir();
        Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let err = Workgroup::create(&dir, "work", "laptop", NODE_A).unwrap_err();
        assert!(err.to_string().contains("already"), "{err}");
    }

    #[test]
    fn the_root_bans_conflict_copies() {
        // The workgroup root's files are the workgroup's own state, where whatever device
        // wrote last holds the one right answer. A conflict copy there — a retirement
        // arriving over a device record this host had not yet seen is exactly such a
        // race — would be a `devices/*.conflict-*.toml` file the ledger cannot open: its
        // name is not a grain-id. So both `create` and `join` ship the ignore file that
        // rules conflict copies out, and the replication carries it to every device.
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let ignore = wg.dir.join("root").join(".sapphireignore");
        let body = std::fs::read_to_string(&ignore).unwrap();
        // One basename rule: a conflict copy of *anything* in the root is out, wherever
        // the sync core would put it.
        assert!(body.contains("*.conflict-*"), "{body}");

        // The filter agrees: the copy of a device record the sync core would otherwise
        // write is not allowed.
        let filter = sapphire_sync::SyncFilter::load(&wg.dir.join("root"), "bridge").unwrap();
        let copy = "devices/0abcd3.conflict-abcdef01-7.toml";
        assert!(
            !filter.allows(copy, false),
            "a conflict copy in the ledger must not take part in sync"
        );

        // `join` writes the same file, so a joiner's root is covered from birth.
        let (_tb, dir_b) = bridge_dir();
        let own = wg.this_device(NODE_A).unwrap();
        Workgroup::materialize(&dir_b, wg.id, "home", &own, &own).unwrap();
        assert_eq!(
            std::fs::read_to_string(
                dir_b
                    .workgroup_dir(wg.id)
                    .join("root")
                    .join(".sapphireignore")
            )
            .unwrap(),
            body
        );
    }

    #[test]
    fn a_known_node_is_authorized() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let mut devices = wg.devices().unwrap();
        devices.add("phone", Some(NODE_B.to_owned()), None).unwrap();

        let device = wg.authorize(NODE_B).unwrap();
        assert_eq!(device.name, "phone");
    }

    #[test]
    fn an_unknown_node_is_refused() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let err = wg.authorize(NODE_B).unwrap_err();
        assert!(
            err.to_string().contains("not a device of this workgroup"),
            "{err}"
        );
    }

    #[test]
    fn a_retired_node_is_refused_and_says_so() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let mut devices = wg.devices().unwrap();
        devices.add("phone", Some(NODE_B.to_owned()), None).unwrap();
        devices.retire("phone").unwrap();

        let err = wg.authorize(NODE_B).unwrap_err();
        assert!(err.to_string().contains("retired"), "{err}");
    }

    #[test]
    fn authorization_rereads_the_ledger_each_time() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let mut devices = wg.devices().unwrap();
        devices.add("phone", Some(NODE_B.to_owned()), None).unwrap();
        assert!(wg.authorize(NODE_B).is_ok());

        // Retirement arrives from another device while the bridge is running.
        let mut fresh = wg.devices().unwrap();
        fresh.retire("phone").unwrap();

        assert!(
            wg.authorize(NODE_B).is_err(),
            "a revocation must take effect without restarting the bridge"
        );
    }

    #[test]
    fn net_configuration_defaults_to_waking_owners() {
        let (_tmp, dir) = bridge_dir();
        let net = NetConfig::load(&dir.net_toml()).unwrap();
        assert!(net.wake_on_sync);
        assert!(net.discovery);
        assert!(net.relays.is_empty());
    }

    #[test]
    fn this_device_is_found_by_node_id_not_by_position() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        // Another device is added and happens to sort first.
        wg.devices()
            .unwrap()
            .add("aaa-phone", Some(NODE_B.to_owned()), None)
            .unwrap();

        assert_eq!(wg.this_device(NODE_A).unwrap().name, "laptop");
        assert_eq!(wg.this_device(NODE_B).unwrap().name, "aaa-phone");
    }

    #[test]
    fn a_node_id_that_is_not_a_member_has_no_record() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        assert!(wg.this_device(&"c1".repeat(32)).is_err());
    }

    /// An inviter on a loopback network, plus a ticket for it.
    ///
    /// `LoopbackTransport` carries `node_addr` as the node id's bytes, so a ticket made here
    /// reaches it the same way a real one reaches an iroh address.
    async fn invited(
        net: &crate::peer::LoopbackNetwork,
        device_name: &str,
    ) -> (
        tempfile::TempDir,
        BridgeDir,
        Workgroup,
        crate::invite::Ticket,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();

        let (_invite, secret) = crate::invite::Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create(device_name, crate::invite::DEFAULT_TTL)
            .unwrap();
        let ticket = crate::invite::Ticket {
            workgroup_id: wg.id,
            node_addr: NODE_A.as_bytes().to_vec(),
            secret,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
        };

        // Answer one pairing connection in the background, as `Bridge::run` would.
        let transport = net.transport(NODE_A);
        let answering_dir = dir.clone();
        let answering_wg = wg.clone();
        let inviter = wg.this_device(NODE_A).unwrap();
        tokio::spawn(async move {
            if let Ok((_from, stream)) = transport.accept_pairing().await {
                let mut invites =
                    crate::invite::Invites::load(&answering_dir.root.join("invites.toml")).unwrap();
                let _ = crate::pairing::admit(stream, &mut invites, &answering_wg, &inviter).await;
            }
        });

        (tmp, dir, wg, ticket)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn joining_writes_the_workgroup_locally() {
        let net = crate::peer::LoopbackNetwork::new();
        let (_inviter_tmp, _inviter_dir, inviter_wg, ticket) = invited(&net, "phone").await;

        let joiner_tmp = tempfile::tempdir().unwrap();
        let joiner_dir = BridgeDir::at(joiner_tmp.path().join("bridge")).unwrap();
        let joiner_transport = net.transport(NODE_B);

        let joined = Workgroup::join(&joiner_dir, &ticket, "phone", &joiner_transport)
            .await
            .unwrap();

        assert_eq!(
            joined.id, inviter_wg.id,
            "both sides must agree on the workgroup"
        );
        assert_eq!(joined.name, "home");
        assert_eq!(
            joined.this_device(NODE_B).unwrap().name,
            "phone",
            "the joiner must have its own record locally"
        );
        assert!(
            Workgroup::open(&joiner_dir).unwrap().is_some(),
            "the workgroup must survive a reopen"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn joining_a_second_workgroup_is_refused_for_now() {
        let net = crate::peer::LoopbackNetwork::new();
        let (_inviter_tmp, _inviter_dir, _wg, ticket) = invited(&net, "phone").await;

        let joiner_tmp = tempfile::tempdir().unwrap();
        let joiner_dir = BridgeDir::at(joiner_tmp.path().join("bridge")).unwrap();
        // The joiner already belongs to one.
        Workgroup::create(&joiner_dir, "work", "phone", NODE_B).unwrap();

        let err = Workgroup::join(&joiner_dir, &ticket, "phone", &net.transport(NODE_B))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_join_leaves_no_workgroup_directory_behind() {
        // A half-created workgroup would make every later attempt fail with "already belongs
        // to a workgroup", which is the worst possible message for someone who has joined
        // nothing.
        let net = crate::peer::LoopbackNetwork::new();
        let (_inviter_tmp, _inviter_dir, wg, mut ticket) = invited(&net, "phone").await;
        ticket.secret = [0u8; 32]; // not the invited secret

        let joiner_tmp = tempfile::tempdir().unwrap();
        let joiner_dir = BridgeDir::at(joiner_tmp.path().join("bridge")).unwrap();

        assert!(
            Workgroup::join(&joiner_dir, &ticket, "phone", &net.transport(NODE_B))
                .await
                .is_err()
        );
        assert!(
            Workgroup::open(&joiner_dir).unwrap().is_none(),
            "a failed join must leave the host able to try again"
        );
        assert!(
            !joiner_dir.workgroup_dir(wg.id).exists(),
            "no partial directory may remain"
        );
    }
}

#[cfg(test)]
mod workspace_list_tests {
    use super::*;
    use crate::dir::BridgeDir;

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn workgroup() -> (tempfile::TempDir, BridgeDir, Workgroup) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let wg = Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        (tmp, dir, wg)
    }

    fn entry(app: &str, name: &str) -> WorkgroupWorkspace {
        WorkgroupWorkspace {
            workspace_id: GrainId::random(),
            app_name: app.to_owned(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn publishing_writes_one_file_per_workspace() {
        let (_tmp, _dir, wg) = workgroup();
        let notes = entry("sapphire-journal", "notes");
        let books = entry("sapphire-ledger", "books");
        wg.publish_workspace(&notes).unwrap();
        wg.publish_workspace(&books).unwrap();

        let listing = wg.dir.join("root").join("workspaces");
        let mut names: Vec<String> = std::fs::read_dir(&listing)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let mut want = vec![
            format!("{}.toml", notes.workspace_id),
            format!("{}.toml", books.workspace_id),
        ];
        want.sort();
        assert_eq!(names, want);
    }

    #[test]
    fn the_list_survives_a_reload() {
        let (_tmp, dir, wg) = workgroup();
        let notes = entry("sapphire-journal", "notes");
        wg.publish_workspace(&notes).unwrap();

        let reopened = Workgroup::open(&dir).unwrap().expect("the workgroup");
        let listed = reopened.workspaces().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].workspace_id, notes.workspace_id);
        assert_eq!(listed[0].app_name, "sapphire-journal");
        assert_eq!(listed[0].name, "notes");
    }

    #[test]
    fn publishing_the_same_workspace_twice_does_not_duplicate_it() {
        let (_tmp, _dir, wg) = workgroup();
        let notes = entry("sapphire-journal", "notes");
        wg.publish_workspace(&notes).unwrap();
        wg.publish_workspace(&notes).unwrap();

        assert_eq!(wg.workspaces().unwrap().len(), 1);
    }

    #[test]
    fn republishing_with_a_new_name_updates_that_file_only() {
        let (_tmp, _dir, wg) = workgroup();
        let notes = entry("sapphire-journal", "notes");
        let books = entry("sapphire-ledger", "books");
        wg.publish_workspace(&notes).unwrap();
        wg.publish_workspace(&books).unwrap();

        let renamed = WorkgroupWorkspace {
            name: "journal".into(),
            ..notes.clone()
        };
        wg.publish_workspace(&renamed).unwrap();

        let listed = wg.workspaces().unwrap();
        assert_eq!(listed.len(), 2);
        let found = listed
            .iter()
            .find(|w| w.workspace_id == notes.workspace_id)
            .unwrap();
        assert_eq!(found.name, "journal");
        let other = listed
            .iter()
            .find(|w| w.workspace_id == books.workspace_id)
            .unwrap();
        assert_eq!(
            other.name, "books",
            "the other file must not have been touched"
        );
    }

    #[test]
    fn a_file_whose_name_is_not_a_grain_id_is_refused() {
        let (_tmp, _dir, wg) = workgroup();
        let listing = wg.dir.join("root").join("workspaces");
        std::fs::create_dir_all(&listing).unwrap();
        std::fs::write(
            listing.join("not-an-id!.toml"),
            "app_name = \"x\"\nname = \"y\"\n",
        )
        .unwrap();

        let err = wg.workspaces().unwrap_err();
        assert!(err.to_string().contains("not-an-id!"), "{err}");
    }

    #[test]
    fn an_empty_workgroup_lists_nothing() {
        let (_tmp, _dir, wg) = workgroup();
        assert!(wg.workspaces().unwrap().is_empty());
    }
}
