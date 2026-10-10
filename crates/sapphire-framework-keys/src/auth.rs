#![cfg(feature = "axum")]

//! The bearer-token check as a tower layer.
//!
//! The same layer guards the framework's routes and the routes an application adds in the
//! same process (`/mcp`, `/acp`, `/audio/ingest`), so one is never left open while another
//! is guarded.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{Next, from_fn_with_state},
    response::Response,
};

use crate::{AuthConfig, Verdict};

/// `router`, guarded by `guard`.
///
/// Without a verifier the layer fails **closed**: every request is refused with 503 — a
/// server whose authentication is not wired is misconfigured, not open. The named
/// [`AuthConfig::insecure_for_tests`] is the only way through without one.
///
/// A missing or refused token is 401. A verifier that could not ask anyone (the bridge is
/// down) is 503: the client's credentials may be fine, and it should try again later. An
/// accepted request carries [`Authenticated`](crate::Authenticated) in its extensions.
pub fn protect(guard: AuthConfig, router: Router) -> Router {
    if guard.verifier().is_none() {
        if guard.is_insecure() {
            tracing::warn!(
                "AuthConfig::insecure_for_tests() is set; this router is unauthenticated"
            );
            return router;
        }
        tracing::error!("no verifier configured; this router will refuse every request");
        return router.layer(from_fn_with_state(Arc::new(guard), refuse));
    }
    router.layer(from_fn_with_state(Arc::new(guard), authenticate))
}

async fn refuse(
    State(_guard): State<Arc<AuthConfig>>,
    _request: Request,
    _next: Next,
) -> std::result::Result<Response, StatusCode> {
    Err(StatusCode::SERVICE_UNAVAILABLE)
}

async fn authenticate(
    State(guard): State<Arc<AuthConfig>>,
    mut request: Request,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    let Some(verifier) = guard.verifier() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_owned();
    match verifier.verify(&presented).await {
        Verdict::Accepted(who) => {
            request.extensions_mut().insert(who);
            Ok(next.run(request).await)
        }
        Verdict::Refused => Err(StatusCode::UNAUTHORIZED),
        Verdict::Unavailable => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use crate::{Authenticated, GrainId, Verifier};

    /// Accepts `good`, refuses anything else, or is unreachable.
    struct Fake {
        good: &'static str,
        id: GrainId,
        reachable: bool,
    }

    #[async_trait::async_trait]
    impl Verifier for Fake {
        async fn verify(&self, token: &str) -> Verdict {
            if !self.reachable {
                Verdict::Unavailable
            } else if token == self.good {
                Verdict::Accepted(Authenticated {
                    id: self.id,
                    name: "pendant".into(),
                })
            } else {
                Verdict::Refused
            }
        }
    }

    fn app(guard: AuthConfig) -> Router {
        protect(
            guard,
            Router::new().route(
                "/whoami",
                axum::routing::get(
                    |axum::Extension(who): axum::Extension<Authenticated>| async move {
                        format!("{} {}", who.name, who.id)
                    },
                ),
            ),
        )
    }

    async fn status(app: Router, token: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder().uri("/whoami");
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let code = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (code, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_layer_admits_the_right_token_and_names_its_device() {
        let id = GrainId::random();
        let guard = AuthConfig::new(Arc::new(Fake {
            good: "sapphire-ed-good",
            id,
            reachable: true,
        }));
        assert_eq!(
            status(app(guard.clone()), None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(app(guard.clone()), Some("sapphire-ed-bad")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (code, body) = status(app(guard), Some("sapphire-ed-good")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, format!("pendant {id}"));
    }

    #[tokio::test]
    async fn an_unreachable_verifier_is_503_not_a_pass() {
        let guard = AuthConfig::new(Arc::new(Fake {
            good: "x",
            id: GrainId::random(),
            reachable: false,
        }));
        assert_eq!(
            status(app(guard), Some("x")).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn without_a_verifier_everything_is_refused_unless_insecure() {
        assert_eq!(
            status(app(AuthConfig::unconfigured()), Some("x")).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let open = protect(
            AuthConfig::unconfigured().insecure_for_tests(),
            Router::new().route("/mcp", axum::routing::get(|| async { "ok" })),
        );
        let res = open
            .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
