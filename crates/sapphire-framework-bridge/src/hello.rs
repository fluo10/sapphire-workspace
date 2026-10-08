//! Hello: what each bridge tells its peers about itself, and the loop that keeps it said.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::Bridge;
use crate::election::{Elector, Own};
use crate::peer::BoxedStream;

/// The ALPN Hello streams speak.
pub const HELLO_ALPN: &[u8] = b"sapphire/hello/1";

/// How often a Hello is sent, and how long silence takes to mean "gone".
#[derive(Clone, Copy, Debug)]
pub struct HelloTiming {
    /// Between two Hellos on one stream.
    pub interval: Duration,
    /// Silence after which a peer is unreachable. Also the wait before claiming.
    pub dead: Duration,
}

impl Default for HelloTiming {
    fn default() -> HelloTiming {
        HelloTiming {
            interval: Duration::from_secs(10),
            dead: Duration::from_secs(40),
        }
    }
}

/// The peers this bridge has heard, and which ones it holds a Hello link to.
#[derive(Debug, Default)]
pub(crate) struct Neighbours {
    heard: Mutex<HashMap<GrainId, (Hello, Instant)>>,
    links: Mutex<HashSet<GrainId>>,
}

impl Neighbours {
    pub(crate) fn heard(&self, hello: Hello, at: Instant) {
        self.heard
            .lock()
            .expect("neighbours")
            .insert(hello.device_id, (hello, at));
    }

    pub(crate) fn forget(&self, device: GrainId) {
        self.heard.lock().expect("neighbours").remove(&device);
    }

    pub(crate) fn reachable(&self, now: Instant, dead: Duration) -> Vec<Hello> {
        self.heard
            .lock()
            .expect("neighbours")
            .values()
            .filter(|(_, at)| now.duration_since(*at) < dead)
            .map(|(h, _)| h.clone())
            .collect()
    }

    pub(crate) fn get(&self, device: GrainId) -> Option<Hello> {
        self.heard
            .lock()
            .expect("neighbours")
            .get(&device)
            .map(|(h, _)| h.clone())
    }

    pub(crate) fn begin_link(&self, device: GrainId) -> bool {
        self.links.lock().expect("neighbours").insert(device)
    }

    pub(crate) fn end_link(&self, device: GrainId) {
        self.links.lock().expect("neighbours").remove(&device);
    }
}

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

/// The longest Hello line read; a longer one is unreadable and ends the link.
const MAX_HELLO_LINE: usize = 16 * 1024;

/// Forgets a peer when dropped, so a cancelled or aborted [`exchange`] still does.
struct Forget<'a>(&'a Neighbours, GrainId);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0.forget(self.1);
    }
}

/// Speak Hello with `peer` over `stream` until either side goes away.
///
/// Sends this host's current Hello on open, then every `interval` and whenever it changes.
/// Records each Hello read, provided it names `peer`. Anything unreadable (or longer than
/// 16 KiB), or a Hello that names a different device, ends this link and only this link.
/// The peer is forgotten when the link ends, or when this future is dropped, so it stops
/// counting at once instead of after `dead`.
///
/// A dialer must not open a Hello stream before `local` holds `Some(Hello)`: on iroh a
/// QUIC stream is invisible to the acceptor's `accept_bi` until the opener writes, and the
/// accept loop is serial, so a dialer that stays silent stalls every later connection.
pub(crate) async fn exchange(
    stream: BoxedStream,
    peer: GrainId,
    mut local: watch::Receiver<Option<Hello>>,
    neighbours: Arc<Neighbours>,
    timing: HelloTiming,
) {
    let _forget = Forget(&neighbours, peer);
    let (read, mut write) = tokio::io::split(stream);
    let reader = async {
        let mut reader = BufReader::new(read);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // One byte more than the cap, so an over-long line is told from a full one.
            let n = match (&mut reader)
                .take(MAX_HELLO_LINE as u64 + 1)
                .read_until(b'\n', &mut buf)
                .await
            {
                Ok(n) => n,
                Err(_) => return,
            };
            if n == 0 || buf.last() != Some(&b'\n') {
                if n > MAX_HELLO_LINE {
                    tracing::debug!(%peer, "a Hello line is too long");
                }
                return;
            }
            match serde_json::from_slice::<Hello>(&buf) {
                Ok(hello) if hello.device_id == peer => neighbours.heard(hello, Instant::now()),
                Ok(hello) => {
                    tracing::debug!(%peer, claimed = %hello.device_id, "a Hello named another device");
                    return;
                }
                Err(err) => {
                    tracing::debug!(%peer, "an unreadable Hello: {err}");
                    return;
                }
            }
        }
    };
    let writer = async {
        let mut tick = tokio::time::interval(timing.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the loop below already sends on open.
        tick.tick().await;
        loop {
            let current = local.borrow_and_update().clone();
            if let Some(hello) = current {
                let mut line = match serde_json::to_vec(&hello) {
                    Ok(line) => line,
                    Err(_) => return,
                };
                line.push(b'\n');
                if write.write_all(&line).await.is_err() || write.flush().await.is_err() {
                    return;
                }
            }
            tokio::select! {
                _ = tick.tick() => {}
                changed = local.changed() => if changed.is_err() { return },
            }
        }
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

/// Keep Hello links to every peer, and re-run the election, every interval.
///
/// Only the lower device id of a pair dials, as everywhere else. Never fails: a bridge with
/// no workgroup yet simply has nobody to greet, and asks again next tick.
///
/// Each tick publishes this host's fresh Hello before it dials, so a new link always has
/// something to say at once (see [`exchange`]).
pub(crate) async fn run(bridge: Arc<Bridge>) -> crate::Result<()> {
    let timing = bridge.hello_timing;
    let mut tick = tokio::time::interval(timing.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut elector: Option<Elector> = None;
    loop {
        tick.tick().await;
        let Ok(Some(workgroup)) = bridge.workgroup() else {
            continue;
        };
        let Ok(me) = workgroup.this_device(&bridge.transport().node_id()) else {
            continue;
        };
        let elector = elector.get_or_insert_with(|| Elector::new(me.id, timing.dead));

        let hosting: Vec<GrainId> = bridge
            .route_entries()
            .into_iter()
            .filter(|r| r.app_name != crate::wgsync::WORKSPACE_APP_NAME)
            .filter(|r| bridge.owners().is_online(&r.app_name))
            .map(|r| r.workspace_id)
            .collect();
        let own = Own {
            priority: me.priority,
            availability: None,
            hosting,
        };
        let now = Instant::now();
        let heard = bridge.neighbours.reachable(now, timing.dead);
        let (hello, roles) = elector.step(now, &own, &heard);
        *bridge.roles.lock().expect("roles") = roles;
        bridge.hello_tx.send_if_modified(|current| {
            let changed = current.as_ref() != Some(&hello);
            *current = Some(hello);
            changed
        });

        // A dialer must not open a link before it has a Hello to send: a silent opener
        // stalls the peer's whole accept loop (see `exchange`).
        if bridge.hello_tx.borrow().is_none() {
            continue;
        }
        let Ok(devices) = workgroup.devices() else {
            continue;
        };
        for device in devices.entries() {
            if device.id <= me.id || device.is_retired() {
                continue;
            }
            let Some(node_id) = device.node_id.clone() else {
                continue;
            };
            if !bridge.neighbours.begin_link(device.id) {
                continue;
            }
            let bridge = Arc::clone(&bridge);
            let peer = device.id;
            tokio::spawn(async move {
                match bridge.transport().open_hello(&node_id).await {
                    Ok(stream) => {
                        exchange(
                            stream,
                            peer,
                            bridge.hello_tx.subscribe(),
                            Arc::clone(&bridge.neighbours),
                            bridge.hello_timing,
                        )
                        .await;
                    }
                    Err(err) => tracing::debug!(%peer, "no hello link: {err}"),
                }
                bridge.neighbours.end_link(peer);
            });
        }
    }
}

/// Answer an inbound Hello stream from an authorized device.
pub(crate) fn serve_inbound(bridge: &Arc<Bridge>, peer: GrainId, stream: BoxedStream) {
    let rx = bridge.hello_tx.subscribe();
    let neighbours = Arc::clone(&bridge.neighbours);
    let timing = bridge.hello_timing;
    tokio::spawn(exchange(stream, peer, rx, neighbours, timing));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::{Inbound, LoopbackNetwork, PeerTransport};

    fn timing() -> HelloTiming {
        HelloTiming {
            interval: Duration::from_millis(50),
            dead: Duration::from_millis(300),
        }
    }

    fn hello(id: GrainId) -> Hello {
        Hello {
            device_id: id,
            priority: 1,
            availability: None,
            hosting: vec![],
            designated: vec![],
            backup: vec![],
        }
    }

    #[test]
    fn a_neighbour_is_reachable_until_dead() {
        let n = Neighbours::default();
        let id = GrainId::random();
        let t0 = Instant::now();
        n.heard(hello(id), t0);
        assert_eq!(
            n.reachable(t0 + Duration::from_millis(100), Duration::from_millis(300))
                .len(),
            1
        );
        assert!(
            n.reachable(t0 + Duration::from_millis(400), Duration::from_millis(300))
                .is_empty()
        );
    }

    #[test]
    fn a_link_is_claimed_once() {
        let n = Neighbours::default();
        let id = GrainId::random();
        assert!(n.begin_link(id));
        assert!(!n.begin_link(id));
        n.end_link(id);
        assert!(n.begin_link(id));
    }

    #[tokio::test]
    async fn two_ends_hear_each_other_and_forget_on_close() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let (ida, idb) = (GrainId::random(), GrainId::random());
        let (na, nb) = (
            Arc::new(Neighbours::default()),
            Arc::new(Neighbours::default()),
        );
        let (_ta, ra) = watch::channel(Some(hello(ida)));
        let (_tb, rb) = watch::channel(Some(hello(idb)));

        let out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(from, inb) = b.accept().await.unwrap() else {
            panic!("not a hello")
        };
        assert_eq!(from, "node-a");
        let ja = tokio::spawn(exchange(out, idb, ra, Arc::clone(&na), timing()));
        let jb = tokio::spawn(exchange(inb, ida, rb, Arc::clone(&nb), timing()));

        let deadline = Instant::now() + Duration::from_secs(5);
        while na.get(idb).is_none() || nb.get(ida).is_none() {
            assert!(Instant::now() < deadline, "the Hellos never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ja.abort();
        let _ = ja.await;
        tokio::time::timeout(Duration::from_secs(5), jb)
            .await
            .unwrap()
            .unwrap();
        assert!(nb.get(ida).is_none(), "the closed link is forgotten");
    }

    #[tokio::test]
    async fn a_garbage_hello_drops_only_that_link() {
        use tokio::io::AsyncWriteExt;
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let nb = Arc::new(Neighbours::default());
        let (_tb, rb) = watch::channel(Some(hello(GrainId::random())));
        let mut out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(_, inb) = b.accept().await.unwrap() else {
            panic!()
        };
        let job = tokio::spawn(exchange(
            inb,
            GrainId::random(),
            rb,
            Arc::clone(&nb),
            timing(),
        ));

        out.write_all(b"not json\n").await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .unwrap()
            .unwrap();
        // The transport still serves workspace streams.
        let _ws = a.open("node-b", GrainId::random()).await.unwrap();
        assert!(matches!(b.accept().await.unwrap(), Inbound::Workspace(..)));
    }

    #[tokio::test]
    async fn a_hello_naming_another_device_is_refused() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let nb = Arc::new(Neighbours::default());
        let (_ta, ra) = watch::channel(Some(hello(GrainId::random()))); // not the id b expects
        let (_tb, rb) = watch::channel(Some(hello(GrainId::random())));
        let out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(_, inb) = b.accept().await.unwrap() else {
            panic!()
        };
        let expected = GrainId::random();
        let _ja = tokio::spawn(exchange(
            out,
            GrainId::random(),
            ra,
            Arc::new(Neighbours::default()),
            timing(),
        ));
        tokio::time::timeout(
            Duration::from_secs(5),
            exchange(inb, expected, rb, Arc::clone(&nb), timing()),
        )
        .await
        .unwrap();
        assert!(nb.get(expected).is_none());
    }

    #[tokio::test]
    async fn hello_streams_are_not_counted_as_frames() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let _b = net.transport("node-b");
        let mut out = a.open_hello("node-b").await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut out, b"x\n")
            .await
            .unwrap();
        assert_eq!(net.frames_sent("node-a"), 0);
    }

    #[tokio::test]
    async fn an_aborted_exchange_forgets_its_peer() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let (ida, idb) = (GrainId::random(), GrainId::random());
        let (na, nb) = (
            Arc::new(Neighbours::default()),
            Arc::new(Neighbours::default()),
        );
        let (_ta, ra) = watch::channel(Some(hello(ida)));
        let (_tb, rb) = watch::channel(Some(hello(idb)));
        let out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(_, inb) = b.accept().await.unwrap() else {
            panic!()
        };
        let _ja = tokio::spawn(exchange(out, idb, ra, Arc::clone(&na), timing()));
        let jb = tokio::spawn(exchange(inb, ida, rb, Arc::clone(&nb), timing()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while nb.get(ida).is_none() {
            assert!(Instant::now() < deadline, "the Hello never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        jb.abort();
        let _ = jb.await;
        assert!(
            nb.get(ida).is_none(),
            "an aborted exchange forgets its peer"
        );
    }

    #[tokio::test]
    async fn an_overlong_hello_line_ends_the_link() {
        use tokio::io::AsyncWriteExt;
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let nb = Arc::new(Neighbours::default());
        let (_tb, rb) = watch::channel(Some(hello(GrainId::random())));
        let mut out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(_, inb) = b.accept().await.unwrap() else {
            panic!()
        };
        let job = tokio::spawn(exchange(inb, GrainId::random(), rb, nb, timing()));
        let long = vec![b'x'; MAX_HELLO_LINE + 100];
        let _ = out.write_all(&long).await;
        tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .unwrap()
            .unwrap();
    }
}
