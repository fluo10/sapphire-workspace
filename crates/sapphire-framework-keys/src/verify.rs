//! Who a presented token belongs to.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grain_id::GrainId;
use sapphire_bridge_api::ApiKey;
use sapphire_bridge_client::BridgeClient;
use sapphire_ipc::Endpoint;
use sha2::{Digest, Sha256};

/// How long a successful check is trusted without asking the bridge again. A retire, a
/// rotate or an application removed takes effect within this long (plus the sync delay).
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// The external device a request came from; put into the request's extensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticated {
    /// Its id in the workgroup's ledger.
    pub id: GrainId,
    /// Its name.
    pub name: String,
}

/// The answer to a presented token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The token belongs to this external device, which may use this application.
    Accepted(Authenticated),
    /// It does not, or it may not.
    Refused,
    /// Nobody could be asked: the bridge is not running or did not answer.
    Unavailable,
}

/// Checks presented tokens.
#[async_trait::async_trait]
pub trait Verifier: Send + Sync {
    /// The verdict on `token`.
    async fn verify(&self, token: &str) -> Verdict;
}

/// Asks the bridge, for one application.
pub struct BridgeVerifier {
    app: String,
    version: String,
    /// `None`: the host's standard bridge.
    endpoint: Option<Endpoint>,
    client: tokio::sync::Mutex<Option<BridgeClient>>,
    /// Successes by the token's hash: the token itself is never kept.
    cache: Mutex<HashMap<String, (Authenticated, Instant)>>,
    ttl: Duration,
}

impl BridgeVerifier {
    /// A verifier for `app`, asking the host's standard bridge. `version` is the caller's,
    /// for the bridge's client table.
    pub fn new(app: &str, version: &str) -> BridgeVerifier {
        BridgeVerifier {
            app: app.to_owned(),
            version: version.to_owned(),
            endpoint: None,
            client: tokio::sync::Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
            ttl: CACHE_TTL,
        }
    }

    /// Ask the bridge at `endpoint` instead.
    pub fn at(mut self, endpoint: Endpoint) -> BridgeVerifier {
        self.endpoint = Some(endpoint);
        self
    }

    /// Trust a success for `ttl` instead of [`CACHE_TTL`].
    pub fn cache_ttl(mut self, ttl: Duration) -> BridgeVerifier {
        self.ttl = ttl;
        self
    }

    async fn connect(&self) -> Option<BridgeClient> {
        match &self.endpoint {
            Some(ep) => BridgeClient::connect_at(ep, "http", &self.version)
                .await
                .ok()
                .flatten(),
            None => BridgeClient::connect_running("http", &self.version)
                .await
                .ok(),
        }
    }
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[async_trait::async_trait]
impl Verifier for BridgeVerifier {
    async fn verify(&self, token: &str) -> Verdict {
        let key = hash(token);
        if let Some((who, at)) = self.cache.lock().expect("auth cache").get(&key)
            && at.elapsed() < self.ttl
        {
            return Verdict::Accepted(who.clone());
        }
        let mut slot = self.client.lock().await;
        if slot.is_none() {
            *slot = self.connect().await;
        }
        let Some(client) = slot.as_ref() else {
            return Verdict::Unavailable;
        };
        match client
            .external_device_authenticate(ApiKey::new(token), &self.app)
            .await
        {
            Ok(r) => {
                let who = Authenticated {
                    id: r.id,
                    name: r.name,
                };
                self.cache
                    .lock()
                    .expect("auth cache")
                    .insert(key, (who.clone(), Instant::now()));
                Verdict::Accepted(who)
            }
            // The bridge answered, and the answer is no.
            Err(sapphire_ipc::Error::Rpc(_)) => {
                self.cache.lock().expect("auth cache").remove(&key);
                Verdict::Refused
            }
            // The connection is gone: ask on a fresh one next time.
            Err(err) => {
                tracing::warn!("could not ask the bridge to check a token: {err}");
                *slot = None;
                Verdict::Unavailable
            }
        }
    }
}
