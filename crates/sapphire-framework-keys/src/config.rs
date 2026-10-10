//! What the [`protect`](crate::protect) layer checks tokens with.

use std::sync::Arc;

use crate::Verifier;

/// The [`protect`](crate::protect) layer's configuration: the verifier, and whether the
/// test-only bypass is on.
#[derive(Clone, Default)]
pub struct AuthConfig {
    verifier: Option<Arc<dyn Verifier>>,
    insecure: bool,
}

impl AuthConfig {
    /// Check every request with `verifier`.
    pub fn new(verifier: Arc<dyn Verifier>) -> Self {
        Self {
            verifier: Some(verifier),
            insecure: false,
        }
    }

    /// No verifier: the layer fails **closed** and refuses every request with 503.
    pub fn unconfigured() -> Self {
        Self::default()
    }

    /// The test-only bypass: without a verifier, requests go through instead of being
    /// refused. Never reachable from production wiring.
    pub fn insecure_for_tests(mut self) -> Self {
        self.insecure = true;
        self
    }

    /// The verifier, if any.
    pub fn verifier(&self) -> Option<&Arc<dyn Verifier>> {
        self.verifier.as_ref()
    }

    /// Whether the test-only bypass is on.
    pub fn is_insecure(&self) -> bool {
        self.insecure
    }
}
