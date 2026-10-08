// Used from Task 5 (the bridge wiring); unused outside tests until then.
#![cfg_attr(not(test), allow(dead_code))]

//! Hello: what each bridge tells its peers about itself, and the loop that keeps it said.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

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

/// Speak Hello with `peer` over `stream` until either side goes away.
///
/// Sends this host's current Hello every `interval` and whenever it changes. Records each
/// Hello read, provided it names `peer`. Anything unreadable, or a Hello that names a
/// different device, ends this link and only this link. The peer is forgotten when the
/// link ends, so it stops counting at once instead of after `dead`.
pub(crate) async fn exchange(
    stream: BoxedStream,
    peer: GrainId,
    mut local: watch::Receiver<Option<Hello>>,
    neighbours: Arc<Neighbours>,
    timing: HelloTiming,
) {
    let (read, mut write) = tokio::io::split(stream);
    let reader = async {
        let mut lines = BufReader::new(read).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<Hello>(&line) {
                    Ok(hello) if hello.device_id == peer => neighbours.heard(hello, Instant::now()),
                    Ok(hello) => {
                        tracing::debug!(%peer, claimed = %hello.device_id, "a Hello named another device");
                        return;
                    }
                    Err(err) => {
                        tracing::debug!(%peer, "an unreadable Hello: {err}");
                        return;
                    }
                },
                Ok(None) | Err(_) => return,
            }
        }
    };
    let writer = async {
        let mut tick = tokio::time::interval(timing.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
    neighbours.forget(peer);
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
}
