//! The governed ingestion endpoint: `POST /api/ingest/odoo`.
//!
//! Thin-notify: the body is an untrusted `{_model, _id}` ping. The endpoint
//! authenticates every notification with a shared secret (constant-time) and enforces
//! the model allowlist. Anything unauthenticated or out of scope is rejected, with no
//! pull and no write. An accepted notification is submitted to the durable worker; the
//! authoritative fact is always a governed re-read there, never the POST body.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use ekg_domain::SourceRef;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::config::IngestionConfig;
use crate::submit::Submitter;

/// The header carrying the shared webhook secret.
pub const SECRET_HEADER: &str = "x-ekg-webhook-secret";

/// The source system this endpoint ingests from, stamped on every submitted ref.
pub const SOURCE_SYSTEM: &str = "odoo";

/// Shared endpoint state.
#[derive(Clone)]
pub struct AppState {
    /// Ingestion configuration (secret, allowlist, routing).
    pub config: Arc<IngestionConfig>,
    /// Hands accepted notifications to the durable ingest worker.
    pub submitter: Arc<dyn Submitter>,
}

/// Odoo's thin automation-rule notification payload.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OdooNotification {
    /// The changed record's model, e.g. `sale.order`.
    #[serde(rename = "_model")]
    pub model: String,
    /// The changed record's id.
    #[serde(rename = "_id")]
    pub id: i64,
}

/// Why a notification was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejection {
    /// Missing or wrong shared secret.
    Unauthenticated,
    /// Model (or record) outside the allowed scope.
    OutOfScope,
}

/// Decide whether to accept a notification: valid secret and allowlisted model.
/// Pure: nothing is pulled or written on the reject paths.
///
/// # Errors
/// [`Rejection::Unauthenticated`] for a missing/invalid secret;
/// [`Rejection::OutOfScope`] for a model outside the allowlist.
pub fn authorize(
    provided_secret: Option<&str>,
    model: &str,
    config: &IngestionConfig,
) -> Result<(), Rejection> {
    let ok = provided_secret
        .is_some_and(|s| constant_time_eq(s.as_bytes(), config.webhook_secret.as_bytes()));
    if !ok {
        return Err(Rejection::Unauthenticated);
    }
    if !config.allows(model) {
        return Err(Rejection::OutOfScope);
    }
    Ok(())
}

/// Constant-time byte-slice equality: no early return on the first differing byte,
/// so a timing side channel cannot leak how much of the secret matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build the ingestion router behind `/api`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/ingest/odoo", post(ingest_odoo))
        .with_state(state)
}

/// The webhook handler: authenticate and scope-check, then submit the parsed ref to
/// the durable worker via the Restate ingress. Rejections return without pulling,
/// submitting or writing anything. A submit that cannot reach Restate answers 503;
/// the reconciliation sweep is the backstop.
#[utoipa::path(
    post,
    path = "/api/ingest/odoo",
    tag = "ingest",
    request_body = OdooNotification,
    params(("x-ekg-webhook-secret" = String, Header, description = "Shared webhook secret")),
    responses(
        (status = 202, description = "Accepted; submitted to the durable ingest worker"),
        (status = 401, description = "Missing or invalid webhook secret"),
        (status = 403, description = "Model outside the ingestion allowlist"),
        (status = 503, description = "Restate ingress unreachable (the sweep backstops)")
    )
)]
pub async fn ingest_odoo(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(notification): Json<OdooNotification>,
) -> StatusCode {
    let secret = headers.get(SECRET_HEADER).and_then(|v| v.to_str().ok());
    match authorize(secret, &notification.model, &state.config) {
        Err(Rejection::Unauthenticated) => StatusCode::UNAUTHORIZED,
        Err(Rejection::OutOfScope) => StatusCode::FORBIDDEN,
        Ok(()) => {
            let source_ref = SourceRef {
                system: SOURCE_SYSTEM.to_owned(),
                model: notification.model,
                id: notification.id,
            };
            match state.submitter.submit(&source_ref).await {
                Ok(()) => StatusCode::ACCEPTED,
                Err(err) => {
                    eprintln!("durable submit failed for {source_ref:?}: {err}");
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::submit::{SubmitError, object_key};

    /// A submitter that records what it was handed and can simulate an unreachable
    /// Restate ingress, so the endpoint is testable without a live runtime.
    struct FakeSubmitter {
        reachable: bool,
        seen: Mutex<Vec<SourceRef>>,
    }

    impl FakeSubmitter {
        fn new(reachable: bool) -> Self {
            Self {
                reachable,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Submitter for FakeSubmitter {
        async fn submit(&self, source_ref: &SourceRef) -> Result<(), SubmitError> {
            self.seen.lock().expect("lock").push(source_ref.clone());
            if self.reachable {
                Ok(())
            } else {
                Err(SubmitError::Unreachable("test".to_owned()))
            }
        }
    }

    fn config() -> IngestionConfig {
        let vars: BTreeMap<String, String> =
            [("EKG_WEBHOOK_SECRET", "s3cret"), ("ODOO_API_KEY", "admin")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        IngestionConfig::from_map(&vars).expect("config")
    }

    #[test]
    fn authorize_accepts_valid_secret_and_allowlisted_model() {
        assert_eq!(authorize(Some("s3cret"), "sale.order", &config()), Ok(()));
    }

    #[test]
    fn authorize_rejects_missing_or_wrong_secret() {
        assert_eq!(
            authorize(None, "sale.order", &config()),
            Err(Rejection::Unauthenticated)
        );
        assert_eq!(
            authorize(Some("nope"), "sale.order", &config()),
            Err(Rejection::Unauthenticated)
        );
    }

    #[test]
    fn authorize_rejects_out_of_allowlist_model() {
        assert_eq!(
            authorize(Some("s3cret"), "account.move", &config()),
            Err(Rejection::OutOfScope)
        );
    }

    fn state_with(submitter: Arc<dyn Submitter>) -> AppState {
        AppState {
            config: Arc::new(config()),
            submitter,
        }
    }

    async fn post_with(
        submitter: Arc<dyn Submitter>,
        secret: Option<&str>,
        body: &str,
    ) -> StatusCode {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/ingest/odoo")
            .header("content-type", "application/json");
        if let Some(s) = secret {
            req = req.header(SECRET_HEADER, s);
        }
        router(state_with(submitter))
            .oneshot(req.body(Body::from(body.to_owned())).expect("req"))
            .await
            .expect("response")
            .status()
    }

    async fn post(secret: Option<&str>, body: &str) -> StatusCode {
        post_with(Arc::new(FakeSubmitter::new(true)), secret, body).await
    }

    #[tokio::test]
    async fn accepted_notification_returns_202() {
        let status = post(Some("s3cret"), r#"{"_model":"sale.order","_id":42}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn unauthenticated_returns_401() {
        let status = post(None, r#"{"_model":"sale.order","_id":42}"#).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn out_of_scope_returns_403() {
        let status = post(Some("s3cret"), r#"{"_model":"account.move","_id":42}"#).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn accepted_notification_submits_to_the_keyed_object() {
        let submitter = Arc::new(FakeSubmitter::new(true));
        let status = post_with(
            submitter.clone(),
            Some("s3cret"),
            r#"{"_model":"sale.order","_id":42}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let seen = submitter.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1, "exactly one durable submit");
        assert_eq!(seen[0].system, SOURCE_SYSTEM);
        assert_eq!(object_key(&seen[0]), "sale.order:42");
    }

    #[tokio::test]
    async fn unreachable_restate_returns_503() {
        let submitter = Arc::new(FakeSubmitter::new(false));
        let status = post_with(
            submitter,
            Some("s3cret"),
            r#"{"_model":"sale.order","_id":42}"#,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rejected_notifications_never_submit() {
        // Neither an unauthenticated nor an out-of-scope POST reaches the submitter.
        let submitter = Arc::new(FakeSubmitter::new(true));
        let _ = post_with(
            submitter.clone(),
            None,
            r#"{"_model":"sale.order","_id":1}"#,
        )
        .await;
        let _ = post_with(
            submitter.clone(),
            Some("s3cret"),
            r#"{"_model":"account.move","_id":1}"#,
        )
        .await;
        assert!(
            submitter.seen.lock().expect("lock").is_empty(),
            "no pull/submit on the reject paths"
        );
    }

    #[test]
    fn constant_time_eq_matches_std_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
