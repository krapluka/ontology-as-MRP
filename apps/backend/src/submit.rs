//! Submitting an accepted webhook to the durable ingest worker.
//!
//! After the endpoint authenticates and scope-checks a notification, it hands the
//! parsed [`SourceRef`] to the Restate ingress as a fire-and-forget invocation of the
//! keyed ingest virtual object (`<model>:<id>`). The authoritative fact is always the
//! governed re-read the worker performs; the POST body is never trusted.
//!
//! A submit that cannot reach Restate is reported (the endpoint answers 503). The
//! reconciliation sweep is the backstop for a missed webhook, so dropping it silently
//! would hide a gap in the governed path.

use async_trait::async_trait;
use ekg_domain::SourceRef;

use crate::worker::{INGEST_HANDLER, INGEST_OBJECT};

/// Why a durable submit failed. Both variants map to a 503 at the endpoint: the
/// webhook is not acknowledged and the sweep backfills the change later.
#[derive(Debug)]
pub enum SubmitError {
    /// The Restate ingress could not be reached (transport failure) or the target
    /// URL could not be built.
    Unreachable(String),
    /// The ingress rejected the submission with a non-success status code.
    Rejected(u16),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "restate ingress unreachable: {why}"),
            Self::Rejected(code) => write!(f, "restate ingress rejected submit: HTTP {code}"),
        }
    }
}

/// Hands an accepted source reference to the durable ingest worker.
///
/// A seam so the endpoint stays testable without a live Restate: the real driver
/// ([`RestateSubmitter`]) posts to the ingress; tests inject a fake.
#[async_trait]
pub trait Submitter: Send + Sync {
    /// Fire-and-forget submit of `source_ref` to the keyed ingest object.
    ///
    /// # Errors
    /// [`SubmitError`] if the ingress is unreachable or rejects the submission.
    async fn submit(&self, source_ref: &SourceRef) -> Result<(), SubmitError>;
}

/// The virtual-object key for a source ref: `<model>:<id>`. Keyed exclusivity
/// gives a single writer per entity at the durable layer.
#[must_use]
pub fn object_key(source_ref: &SourceRef) -> String {
    format!("{}:{}", source_ref.model, source_ref.id)
}

/// Build the ingress URL that fire-and-forget-invokes the ingest handler for
/// `source_ref`: `{ingress}/Ingest/<model>:<id>/ingest/send`.
///
/// # Errors
/// Returns a message if `ingress_base` is not a valid base URL.
pub fn ingest_send_url(ingress_base: &str, source_ref: &SourceRef) -> Result<String, String> {
    let mut url = reqwest::Url::parse(ingress_base).map_err(|e| format!("bad ingress url: {e}"))?;
    let key = object_key(source_ref);
    url.path_segments_mut()
        .map_err(|()| "ingress url cannot be a base".to_owned())?
        .pop_if_empty()
        .extend([INGEST_OBJECT, key.as_str(), INGEST_HANDLER, "send"]);
    Ok(url.into())
}

/// Submits over HTTP to the Restate ingress (plain `http://` locally, no TLS).
pub struct RestateSubmitter {
    client: reqwest::Client,
    ingress_base: String,
}

impl RestateSubmitter {
    /// Build a submitter targeting the ingress at `ingress_base`.
    #[must_use]
    pub fn new(ingress_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            ingress_base: ingress_base.into(),
        }
    }
}

#[async_trait]
impl Submitter for RestateSubmitter {
    async fn submit(&self, source_ref: &SourceRef) -> Result<(), SubmitError> {
        let url =
            ingest_send_url(&self.ingress_base, source_ref).map_err(SubmitError::Unreachable)?;
        // The handler argument is `Json<SourceRef>`, so the ingress body is the ref's JSON.
        let body =
            serde_json::to_vec(source_ref).map_err(|e| SubmitError::Unreachable(e.to_string()))?;
        let resp = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| SubmitError::Unreachable(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(SubmitError::Rejected(resp.status().as_u16()))
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn source_ref() -> SourceRef {
        SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 42,
        }
    }

    #[test]
    fn object_key_is_model_colon_id() {
        assert_eq!(object_key(&source_ref()), "sale.order:42");
    }

    #[test]
    fn ingest_url_targets_the_keyed_object_send_endpoint() {
        let url = ingest_send_url("http://localhost:8080", &source_ref()).expect("url");
        assert_eq!(
            url,
            "http://localhost:8080/Ingest/sale.order:42/ingest/send"
        );
    }

    #[test]
    fn ingest_url_tolerates_a_trailing_slash_on_the_base() {
        let url = ingest_send_url("http://localhost:8080/", &source_ref()).expect("url");
        assert_eq!(
            url,
            "http://localhost:8080/Ingest/sale.order:42/ingest/send"
        );
    }

    #[tokio::test]
    async fn submit_to_an_unreachable_ingress_is_a_loud_error() {
        // Port 1 is unbound; the POST fails at the transport layer.
        let submitter = RestateSubmitter::new("http://127.0.0.1:1");
        let err = submitter
            .submit(&source_ref())
            .await
            .expect_err("unreachable ingress must fail loud");
        assert!(matches!(err, SubmitError::Unreachable(_)));
    }
}
