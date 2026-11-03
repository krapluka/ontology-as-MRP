//! Operational endpoints: liveness (`GET /api/health`) and readiness (`GET /api/ready`).
//!
//! Liveness reports that the process is serving and does not depend on downstream health,
//! so a supervisor can use it as a restart signal. Readiness probes the hard dependencies
//! (Restate, GraphDB, Odoo) concurrently with a short timeout; it gates traffic (the
//! frontend, a compose start-up check) but triggers no restart. Both are unauthenticated
//! and read-only, and expose no secrets or internal state. The cognition endpoint is not
//! probed: the backend degrades without it (proposals fall back; ingest, detect and promote
//! continue).

// Docs reference product names (Restate, GraphDB, Odoo).
#![allow(clippy::doc_markdown)]

use std::collections::BTreeMap;
use std::time::Duration;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;
use utoipa::ToSchema;

/// The service name reported by liveness.
const SERVICE: &str = "ekg-backend";
/// Per-probe timeout for the readiness checks, so a hung dependency cannot stall the
/// endpoint.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Liveness body: the process is up and serving.
#[derive(Debug, Serialize, ToSchema)]
pub struct HealthStatus {
    /// Always `ok` while the process serves.
    pub status: String,
    /// The service name (`ekg-backend`).
    pub service: String,
}

/// Readiness body: the aggregate verdict plus a per-dependency reachability map.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReadinessStatus {
    /// `ready` when every dependency is reachable, else `not_ready`.
    pub status: String,
    /// Per-dependency reachability (`ok` or `unreachable`), keyed by name.
    pub dependencies: BTreeMap<String, String>,
}

/// State for the readiness route: an HTTP client (carrying the probe timeout) and the
/// dependency base URLs cloned from the loaded `IngestionConfig`.
#[derive(Clone)]
pub struct ReadyState {
    http: reqwest::Client,
    restate_admin_url: String,
    graphdb_url: String,
    odoo_url: String,
}

impl ReadyState {
    /// Build the readiness state from the dependency base URLs.
    ///
    /// # Errors
    /// Returns a message if the HTTP client cannot be built.
    pub fn new(
        restate_admin_url: String,
        graphdb_url: String,
        odoo_url: String,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .map_err(|e| format!("readiness http client: {e}"))?;
        Ok(Self {
            http,
            restate_admin_url,
            graphdb_url,
            odoo_url,
        })
    }
}

/// `GET /api/health`: liveness. Always `200` while the process serves.
#[utoipa::path(
    get,
    path = "/api/health",
    tag = "meta",
    responses((status = 200, description = "The process is up and serving", body = HealthStatus))
)]
pub async fn health() -> Json<HealthStatus> {
    Json(HealthStatus {
        status: "ok".to_owned(),
        service: SERVICE.to_owned(),
    })
}

/// `GET /api/ready`: readiness. Probes each hard dependency concurrently and answers `200`
/// (all reachable) or `503` (any unreachable).
#[utoipa::path(
    get,
    path = "/api/ready",
    tag = "meta",
    responses(
        (status = 200, description = "Every hard dependency is reachable", body = ReadinessStatus),
        (status = 503, description = "A hard dependency is unreachable", body = ReadinessStatus)
    )
)]
pub async fn ready(State(st): State<ReadyState>) -> impl IntoResponse {
    let (status, body) = ready_verdict(&st).await;
    (status, Json(body))
}

/// Probe every dependency concurrently and aggregate to an HTTP status and body. Split
/// from the handler so it can be tested against unreachable URLs.
async fn ready_verdict(st: &ReadyState) -> (StatusCode, ReadinessStatus) {
    let restate = format!("{}/health", st.restate_admin_url.trim_end_matches('/'));
    let graphdb = format!("{}/protocol", st.graphdb_url.trim_end_matches('/'));
    let odoo = format!("{}/web/health", st.odoo_url.trim_end_matches('/'));
    let (restate, graphdb, odoo) = tokio::join!(
        probe(&st.http, &restate),
        probe(&st.http, &graphdb),
        probe(&st.http, &odoo),
    );
    verdict(&[("restate", restate), ("graphdb", graphdb), ("odoo", odoo)])
}

/// Probe one dependency: a GET that resolves to a success status within the client's
/// timeout is reachable. Any transport error or non-success status is unreachable.
async fn probe(http: &reqwest::Client, url: &str) -> bool {
    matches!(http.get(url).send().await, Ok(resp) if resp.status().is_success())
}

/// Aggregate probe results into an HTTP status and body: `200 ready` if every dependency
/// is reachable, else `503 not_ready`.
fn verdict(results: &[(&str, bool)]) -> (StatusCode, ReadinessStatus) {
    let all_ok = results.iter().all(|(_, ok)| *ok);
    let dependencies = results
        .iter()
        .map(|(name, ok)| {
            (
                (*name).to_owned(),
                if *ok { "ok" } else { "unreachable" }.to_owned(),
            )
        })
        .collect();
    let (status, label) = if all_ok {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    };
    (
        status,
        ReadinessStatus {
            status: label.to_owned(),
            dependencies,
        },
    )
}

/// Mount the operational routes: stateless `/api/health` and stateful `/api/ready`.
pub fn router(ready_state: ReadyState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/ready", get(ready))
        .with_state(ready_state)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_reports_the_process_is_up() {
        let Json(body) = health().await;
        assert_eq!(body.status, "ok");
        assert_eq!(body.service, "ekg-backend");
    }

    #[test]
    fn verdict_is_ready_only_when_every_dependency_is_ok() {
        let (status, body) = verdict(&[("restate", true), ("graphdb", true), ("odoo", true)]);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.status, "ready");
        assert!(body.dependencies.values().all(|v| v == "ok"));
    }

    #[test]
    fn verdict_is_not_ready_when_any_dependency_is_down() {
        let (status, body) = verdict(&[("restate", true), ("graphdb", false), ("odoo", true)]);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, "not_ready");
        assert_eq!(
            body.dependencies.get("graphdb").map(String::as_str),
            Some("unreachable")
        );
        assert_eq!(
            body.dependencies.get("restate").map(String::as_str),
            Some("ok")
        );
    }

    #[tokio::test]
    async fn ready_reports_503_when_dependencies_are_unreachable() {
        // Port 1 refuses immediately, so every probe fails fast.
        let st = ReadyState::new(
            "http://127.0.0.1:1".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "http://127.0.0.1:1".to_owned(),
        )
        .expect("client");
        let (status, body) = ready_verdict(&st).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, "not_ready");
        assert_eq!(body.dependencies.len(), 3);
        assert!(
            body.dependencies.values().all(|v| v == "unreachable"),
            "all probes should be unreachable, got {:?}",
            body.dependencies
        );
    }
}
