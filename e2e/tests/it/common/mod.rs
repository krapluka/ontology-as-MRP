//! Shared helpers for the gated integration tests.
//!
//! Restate isolation: every gated e2e serves its own in-process Restate endpoint and
//! registers it with `force: true`. On the callback URI of a running backend (port 9080)
//! that registration would replace the backend's deployment and strand the services the
//! test does not bind, notably `Sweep`, which reschedules itself and would then fail with
//! RT0011 forever.
//!
//! So the tests register on a dedicated port (9091 plus a per-test offset), a separate
//! deployment that does not overwrite the backend's. This holds for both backend setups
//! (see the callback table in the deploy README): the containerized backend registers
//! `http://ekg-backend:9080` (compose DNS, from `just podman-local-up`), and a host-run
//! `just backend` registers `http://host.containers.internal:9080`. Tests that do not bind
//! `Sweep` (drift, propose, ingest) therefore cannot strand it. Each test also deregisters
//! its own deployment on completion, best-effort.
//!
//! The sweep tests (`sweep_e2e_it`, `sweep_bootstrap_it`, `sweep_resync_e2e_it`,
//! `sweep_resync_shared_e2e_it`, `sweep_scope_e2e_it`) bind and run `Sweep`, so they
//! force-claim it while running. All five claim it through [`with_sweep_claimed`], which notes
//! the deployment serving `Sweep`, pauses the stack's own sweep timer (so a production pass
//! cannot fire on the test deployment) and, whatever the test body does (a panic or a hang
//! included), cancels what the test left running, deregisters, and re-arms the timer on that
//! same deployment. Nothing else in the suite should cancel `Sweep` invocations.

#![allow(dead_code, clippy::expect_used, clippy::doc_markdown)]

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ekg_backend::sweep::{SWEEP_HANDLER, SWEEP_KEY, SWEEP_OBJECT, trigger_first_sweep};
use ekg_domain::{SourceRecord, SourceRef};
use ekg_semantic::{GraphDbClient, ontology};
use futures::FutureExt;
use odoo_client::Odoo;
use restate_sdk::prelude::{Endpoint, HttpServer};
use serde_json::json;

/// A unique id base for a single test run (nanoseconds since the epoch).
///
/// The `ActionPointService` is a Restate virtual object keyed by the subject entity
/// IRI (e.g. `…/id/sale.order/1`), and that state persists in Restate across runs: it
/// is not reset when a test drops its throwaway GraphDB repo. Two runs (or two sibling
/// tests) that reuse the same `sale.order` id therefore collide: the second sees the
/// first's open, proposed or aborted point and its own `open` is a no-op. Derive every
/// subject id in a test from this base (call it once, reuse it).
#[must_use]
pub fn unique_base() -> i64 {
    // Nanoseconds since the epoch are unique per run and fit an i64 until year 2262.
    // Only used to offset in-test subject ids.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos());
    i64::try_from(nanos).unwrap_or(i64::MAX)
}

/// Base e2e Restate port. Each gated test adds a fixed per-test offset, so gated
/// tests do not collide on a port or callback URI and all stay clear of the
/// backend's `:9080`.
const BASE_PORT: u16 = 9091;

/// The address the test's in-process Restate SDK endpoint binds: `BASE_PORT +
/// offset`, so each gated test gets a distinct port. `EKG_RESTATE_SERVICE_ADDR`,
/// when set, is a full override (offset ignored) for a single custom run. Leave
/// it unset when running more than one gated test.
#[must_use]
pub fn service_addr(offset: u16) -> String {
    std::env::var("EKG_RESTATE_SERVICE_ADDR")
        .unwrap_or_else(|_| format!("0.0.0.0:{}", BASE_PORT + offset))
}

/// The callback URI Restate dials the test endpoint at (`BASE_PORT + offset`); in
/// podman the host is `host.containers.internal`. Unique per test, so `deregister`
/// (which matches by prefix) only touches the caller's own deployment.
/// `EKG_SERVICE_CALLBACK_URL`, when set, is a full override (offset ignored).
#[must_use]
pub fn callback_url(offset: u16) -> String {
    std::env::var("EKG_SERVICE_CALLBACK_URL")
        .unwrap_or_else(|_| format!("http://host.containers.internal:{}", BASE_PORT + offset))
}

/// Bind the SDK endpoint's listener before serving, so a bind failure (e.g. the
/// port already in use) fails the test at the call site instead of panicking inside
/// a detached task and showing up as a later timeout. The socket is listening when
/// this returns; serving continues on a spawned task.
pub async fn spawn_endpoint(endpoint: Endpoint, service_addr: &str) {
    let listener = match tokio::net::TcpListener::bind(service_addr).await {
        Ok(listener) => listener,
        Err(err) => panic!(
            "e2e Restate endpoint cannot bind {service_addr}: {err} (is the port already in use?)"
        ),
    };
    tokio::spawn(async move {
        HttpServer::new(endpoint).serve(listener).await;
    });
}

// --- Restate admin (introspection, cancel, deployments) ---

/// How long a cancel loop keeps re-querying before it reports what is still unfinished.
const CANCEL_DEADLINE: Duration = Duration::from_secs(20);

/// Rows of a Restate admin `/query` SQL introspection. A transport error, a non-success
/// status, or a body without a `rows` array is an error, not "no rows", so a check that the
/// result is empty cannot pass on a broken introspection query.
///
/// # Errors
/// The failed hop, as text.
pub async fn admin_rows(
    http: &reqwest::Client,
    admin_url: &str,
    query: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http
        .post(format!("{admin_url}/query"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .map_err(|e| format!("admin /query unreachable: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("admin /query returned {status}: {text}"));
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("rows").and_then(serde_json::Value::as_array).cloned())
        .ok_or_else(|| format!("admin /query body has no rows array: {text}"))
}

/// A SQL string literal (single quotes doubled).
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Which unfinished (`status <> 'completed'`) invocations a helper looks at. Built from a
/// service, then narrowed by key, key prefix, handler and deployment.
#[derive(Debug, Clone, Copy, Default)]
pub struct Invocations<'a> {
    service: Option<&'a str>,
    key: Option<&'a str>,
    key_prefix: Option<&'a str>,
    handler: Option<&'a str>,
    pinned_to: Option<&'a str>,
    run_by: Option<&'a str>,
    not_scheduled: bool,
}

impl<'a> Invocations<'a> {
    /// The unfinished invocations of `service`.
    #[must_use]
    pub fn of(service: &'a str) -> Self {
        Self {
            service: Some(service),
            ..Self::default()
        }
    }

    /// Every unfinished invocation already pinned to `deployment` (any service): what would
    /// retry against that deployment's endpoint once it is gone.
    #[must_use]
    pub fn pinned_to(deployment: &'a str) -> Self {
        Self {
            pinned_to: Some(deployment),
            ..Self::default()
        }
    }

    /// Only this object key.
    #[must_use]
    pub fn key(self, key: &'a str) -> Self {
        Self {
            key: Some(key),
            ..self
        }
    }

    /// Only keys starting with `prefix` (SQL `LIKE 'prefix%'`; a `_` in it matches any one
    /// character, harmless for the `model:` prefixes used here).
    #[must_use]
    pub fn key_prefix(self, prefix: &'a str) -> Self {
        Self {
            key_prefix: Some(prefix),
            ..self
        }
    }

    /// Only this handler.
    #[must_use]
    pub fn handler(self, handler: &'a str) -> Self {
        Self {
            handler: Some(handler),
            ..self
        }
    }

    /// Only those `deployment` runs: pinned to it, or not yet pinned (queued, so the deployment
    /// currently claiming the service will run them). Excludes ones pinned elsewhere.
    #[must_use]
    pub fn run_by(self, deployment: &'a str) -> Self {
        Self {
            run_by: Some(deployment),
            ..self
        }
    }

    /// Only those not merely `scheduled` (a timer's future successor): what is still in flight.
    #[must_use]
    pub fn not_scheduled(self) -> Self {
        Self {
            not_scheduled: true,
            ..self
        }
    }

    /// The `sys_invocation` WHERE clause.
    fn filter(&self) -> String {
        let mut terms = vec!["status <> 'completed'".to_owned()];
        if self.not_scheduled {
            terms.push("status <> 'scheduled'".to_owned());
        }
        if let Some(service) = self.service {
            terms.push(format!("target_service_name = {}", sql_literal(service)));
        }
        if let Some(key) = self.key {
            terms.push(format!("target_service_key = {}", sql_literal(key)));
        }
        if let Some(prefix) = self.key_prefix {
            terms.push(format!(
                "target_service_key LIKE {}",
                sql_literal(&format!("{prefix}%"))
            ));
        }
        if let Some(handler) = self.handler {
            terms.push(format!("target_handler_name = {}", sql_literal(handler)));
        }
        if let Some(deployment) = self.pinned_to {
            terms.push(format!(
                "pinned_deployment_id = {}",
                sql_literal(deployment)
            ));
        }
        if let Some(deployment) = self.run_by {
            terms.push(format!(
                "(pinned_deployment_id = {} OR pinned_deployment_id IS NULL)",
                sql_literal(deployment)
            ));
        }
        terms.join(" AND ")
    }
}

/// The ids of the selected unfinished invocations.
///
/// # Errors
/// A failed introspection (never read as "none").
pub async fn unfinished(
    http: &reqwest::Client,
    admin_url: &str,
    selection: Invocations<'_>,
) -> Result<Vec<String>, String> {
    let rows = admin_rows(
        http,
        admin_url,
        &format!("SELECT id FROM sys_invocation WHERE {}", selection.filter()),
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect())
}

/// How to end an unfinished invocation through the admin API.
#[derive(Debug, Clone, Copy)]
pub enum Termination {
    /// `PATCH /invocations/{id}/cancel`: the handler is told and unwinds (needs its deployment
    /// reachable to take effect on a running invocation).
    Cancel,
    /// `PATCH /invocations/{id}/kill`: ended by the runtime alone, for an invocation whose
    /// deployment is already gone.
    Kill,
}

impl Termination {
    fn verb(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Kill => "kill",
        }
    }
}

/// What a termination loop saw: every selected invocation it met (`id` mapped to `invoked_by_id`),
/// how many of them the admin API accepted to end, and why it gave up, if it did.
#[derive(Debug, Default)]
pub struct Terminated {
    /// Every selected unfinished invocation seen across the re-queries.
    pub seen: std::collections::BTreeMap<String, Option<String>>,
    /// How many distinct invocations the admin API accepted to end.
    pub accepted: usize,
    /// Why the loop gave up (still unfinished at the deadline, naming refusals and the last
    /// introspection error), or `None` when nothing selected remains.
    pub error: Option<String>,
}

/// The timer chains among `seen` invocations: those not invoked by another seen one (a
/// running pass and the successor it already scheduled are one chain).
#[must_use]
pub fn chain_roots(seen: &std::collections::BTreeMap<String, Option<String>>) -> usize {
    seen.values()
        .filter(|parent| parent.as_ref().is_none_or(|p| !seen.contains_key(p)))
        .count()
}

/// End the selected invocations and re-query until none remain, so one that appears
/// meanwhile (a running pass journaling its reschedule) is ended too. A failed introspection
/// is retried, not fatal, until [`CANCEL_DEADLINE`]; a refused end is recorded and retried.
pub async fn terminate(
    http: &reqwest::Client,
    admin_url: &str,
    selection: Invocations<'_>,
    how: Termination,
) -> Terminated {
    let deadline = tokio::time::Instant::now() + CANCEL_DEADLINE;
    let mut done = Terminated::default();
    let mut accepted = std::collections::BTreeSet::new();
    // The latest refusal per invocation, and the latest introspection error, so a deadline
    // error says why they are still unfinished.
    let mut refused = std::collections::BTreeMap::new();
    let mut query_error: Option<String>;
    let query = format!(
        "SELECT id, invoked_by_id FROM sys_invocation WHERE {}",
        selection.filter()
    );
    loop {
        let rows = match admin_rows(http, admin_url, &query).await {
            Ok(rows) => {
                query_error = None;
                Some(rows)
            }
            Err(why) => {
                query_error = Some(why);
                None
            }
        };
        let ids: Vec<String> = rows
            .iter()
            .flatten()
            .filter_map(|row| {
                let id = row.get("id").and_then(serde_json::Value::as_str)?;
                let parent = row
                    .get("invoked_by_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                done.seen.insert(id.to_owned(), parent);
                Some(id.to_owned())
            })
            .collect();
        if rows.is_some() && ids.is_empty() {
            done.accepted = accepted.len();
            return done;
        }
        if tokio::time::Instant::now() >= deadline {
            let refusals: Vec<String> = ids
                .iter()
                .filter_map(|id| refused.get(id).map(|why| format!("{id}: {why}")))
                .collect();
            done.accepted = accepted.len();
            done.error = Some(format!(
                "still unfinished after {CANCEL_DEADLINE:?} of {}ing ({}): {ids:?}; refused: {refusals:?}; last introspection error: {query_error:?}",
                how.verb(),
                selection.filter()
            ));
            return done;
        }
        for id in ids {
            // A refused end is not fatal here: the re-query decides (an invocation that
            // completed meanwhile is gone). It is recorded for the deadline error.
            match http
                .patch(format!("{admin_url}/invocations/{id}/{}", how.verb()))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    refused.remove(&id);
                    accepted.insert(id);
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    eprintln!("{} {id}: HTTP {status}: {body}", how.verb());
                    refused.insert(id, format!("HTTP {status}: {body}"));
                }
                Err(err) => {
                    eprintln!("{} {id}: {err}", how.verb());
                    refused.insert(id, err.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Cancel the selected invocations until none remain ([`terminate`] with
/// [`Termination::Cancel`]). Returns how many distinct invocations were accepted as cancelled.
///
/// # Errors
/// Invocations still unfinished at the deadline (named, with the refusals).
pub async fn cancel_all(
    http: &reqwest::Client,
    admin_url: &str,
    selection: Invocations<'_>,
) -> Result<usize, String> {
    let done = terminate(http, admin_url, selection, Termination::Cancel).await;
    match done.error {
        None => Ok(done.accepted),
        Some(why) => Err(why),
    }
}

/// Wait (without cancelling) until none of the selected invocations is unfinished.
///
/// # Errors
/// A failed introspection, or invocations still unfinished at `timeout` (named).
pub async fn wait_finished(
    http: &reqwest::Client,
    admin_url: &str,
    selection: Invocations<'_>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let ids = unfinished(http, admin_url, selection).await?;
        if ids.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "{} invocation(s) still unfinished after {timeout:?} ({}): {ids:?}",
                ids.len(),
                selection.filter()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Best-effort: cancel any non-completed invocation of `service`/`handler` (notably a
/// self-rescheduled `Sweep/run`) so a gated run leaves nothing pending. Logged, never fatal.
/// Where the outcome matters, use [`cancel_all`] and assert its result.
pub async fn cancel_invocations(
    http: &reqwest::Client,
    admin_url: &str,
    service: &str,
    handler: &str,
) {
    if let Err(why) = cancel_all(http, admin_url, Invocations::of(service).handler(handler)).await {
        eprintln!("cancel_invocations ({service}/{handler}): {why}");
    }
}

/// Register the test's endpoint at `callback_url` as a Restate deployment (`force: true`) and
/// return the deployment id.
///
/// # Errors
/// The registration was not accepted (the reason names the likely cause).
pub async fn register(
    http: &reqwest::Client,
    admin_url: &str,
    callback_url: &str,
) -> Result<String, String> {
    let resp = http
        .post(format!("{admin_url}/deployments"))
        .header("Content-Type", "application/json")
        .body(json!({ "uri": callback_url, "force": true }).to_string())
        .send()
        .await
        .map_err(|e| format!("register {callback_url}: admin unreachable: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Restate rejected registration ({status}): is {callback_url} reachable from the runtime? {text}"
        ));
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| format!("registration answered without a deployment id: {text}"))
}

/// Delete the test's own deployment(s) at `callback_url`, matched by prefix: Restate may store
/// the URI with a trailing slash.
///
/// # Errors
/// A failed listing or a delete that was not accepted.
pub async fn deregister_checked(
    http: &reqwest::Client,
    admin_url: &str,
    callback_url: &str,
) -> Result<(), String> {
    let resp = http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .map_err(|e| format!("list deployments: {e}"))?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("list deployments: {e}: {text}"))?;
    let deployments = json
        .get("deployments")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("list deployments: no deployments array: {text}"))?;
    for dep in deployments {
        let uri = dep.get("uri").and_then(serde_json::Value::as_str);
        if !uri.is_some_and(|u| u.starts_with(callback_url)) {
            continue;
        }
        let id = dep
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("a deployment at {callback_url} without an id: {dep}"))?;
        let resp = http
            .delete(format!("{admin_url}/deployments/{id}?force=true"))
            .send()
            .await
            .map_err(|e| format!("delete deployment {id}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("delete deployment {id}: HTTP {}", resp.status()));
        }
    }
    Ok(())
}

/// Best-effort: delete the test's own Restate deployment (the one at `callback_url`) so its
/// dead endpoint does not linger. Logged, never fatal. Where the outcome matters, use
/// [`deregister_checked`] and assert its result.
pub async fn deregister(http: &reqwest::Client, admin_url: &str, callback_url: &str) {
    if let Err(why) = deregister_checked(http, admin_url, callback_url).await {
        eprintln!("deregister {callback_url}: {why}");
    }
}

// --- GraphDB helpers for gated tests that run a rule on a live store ---
// These mirror ekg-semantic's `tests/it/common` (a sibling crate's test module cannot be
// imported), so a backend test can materialize typed fixtures and run a capability
// rule's SELECT against a throwaway repo.

/// GraphDB Free caps a server at five repositories, and the stack's own `ekg` is one of
/// them. Tests that each provision a throwaway repo, and project into ephemeral `sim_*`
/// repos, must not run all at once, or a projection fails for lack of a repo and a slate
/// silently loses its options. The slot is process-wide, so it spans every suite of this
/// crate's one `it` test binary: holding it for a test's whole life keeps one fixture repo
/// per crate at a time. `ekg-semantic`'s test harness does the same.
static FIXTURE_REPO_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Wait for the crate-wide fixture-repo slot; keep the permit until the test ends.
/// The slot is never closed, so this only errs if that invariant is broken.
pub async fn fixture_repo_slot()
-> Result<tokio::sync::SemaphorePermit<'static>, tokio::sync::AcquireError> {
    FIXTURE_REPO_SLOT.acquire().await
}

/// Every test that serves its own Restate endpoint registers a deployment for the same
/// services, and Restate routes a new invocation to the newest deployment of a service. So
/// two such tests running at once can route one's calls to the other's endpoint, and when
/// that test ends and force-removes its deployment, the in-flight invocation is orphaned
/// ("deployment not found in the registry") and its ingress request hangs forever. Holding
/// this slot from before the registration until after the deregistration keeps one test
/// deployment at a time across every suite of this crate's one `it` test binary (the slot is
/// process-wide). Take it before [`fixture_repo_slot`], so the two are always acquired in
/// one order.
static DEPLOYMENT_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Wait for the crate-wide test-deployment slot; keep the permit until the test ends.
/// The slot is never closed, so this only errs if that invariant is broken.
pub async fn deployment_slot()
-> Result<tokio::sync::SemaphorePermit<'static>, tokio::sync::AcquireError> {
    DEPLOYMENT_SLOT.acquire().await
}

/// The GraphDB base URL under test (`GRAPHDB_URL`, default the local stack).
pub fn graphdb_url() -> String {
    std::env::var("GRAPHDB_URL").unwrap_or_else(|_| "http://localhost:7200".to_owned())
}

/// A seed-shaped source record. Reference fields carry the referenced record's id(s)
/// as the normalized wire form the ingestion layer produces.
pub fn rec(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
    SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        },
        fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

/// Create a fresh throwaway `repo` from GraphDB's default JSON template (no SHACL).
/// `Ok(false)` means the store is unreachable and the caller should skip.
pub async fn provision_repo(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
) -> Result<bool, String> {
    let Ok(resp) = http.get(format!("{base}/rest/repositories")).send().await else {
        return Ok(false);
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GraphDB reachable but returned {status} listing repos: {body}"
        ));
    }
    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
    let template = http
        .get(format!("{base}/rest/repositories/default-config/graphdb"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = serde_json::from_str(&template).map_err(|e| e.to_string())?;
    cfg["id"] = serde_json::Value::from(repo);
    cfg["title"] = serde_json::Value::from("EKG integration test (throwaway)");
    if let Some(v) = cfg.pointer_mut("/params/enableContextIndex/value") {
        *v = serde_json::Value::from("true");
    }
    let body = serde_json::to_string(&cfg).map_err(|e| e.to_string())?;
    let resp = http
        .post(format!("{base}/rest/repositories"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("repo create failed {status}: {text}"));
    }
    Ok(true)
}

// --- Odoo and GraphDB helpers shared by the sweep e2e suites ---

/// Read an env var, falling back to a local-stack default.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Does the entity carry provenance (i.e. is it currently materialized)? A failed read is an
/// error, not "absent": use this where "not materialized" is the asserted outcome.
///
/// # Errors
/// The failed SPARQL read, as text.
pub async fn provenance(client: &GraphDbClient, model: &str, id: i64) -> Result<bool, String> {
    let entity = ontology::entity_iri(model, id);
    let query = format!(
        "SELECT ?o WHERE {{ GRAPH ?g {{ <{entity}> <{vocab}wasDerivedFrom> ?o }} }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    client
        .select(&query)
        .await
        .map(|rows| !rows.is_empty())
        .map_err(|e| format!("provenance read of {entity}: {e}"))
}

/// [`provenance`], with a failed read counted as "not materialized". Only for checks that
/// expect the entity to be present, where a read failure fails them anyway.
pub async fn has_provenance(client: &GraphDbClient, model: &str, id: i64) -> bool {
    provenance(client, model, id)
        .await
        .is_ok_and(|present| present)
}

/// Create an active `res.partner` and return its id (blocking RPC off the runtime).
pub async fn create_partner(odoo: &Arc<Odoo>, name: &str) -> i64 {
    let odoo = Arc::clone(odoo);
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || odoo.create("res.partner", json!({ "name": name })))
        .await
        .expect("join")
        .expect("create partner")
}

/// Best-effort hard-delete of a throwaway partner (cleanup).
pub async fn unlink_partner(odoo: &Arc<Odoo>, id: i64) {
    let odoo = Arc::clone(odoo);
    let _ = tokio::task::spawn_blocking(move || odoo.unlink("res.partner", json!([id]))).await;
}

/// Poll until `condition` holds or the deadline passes.
pub async fn wait_until<F>(timeout: Duration, mut condition: F) -> bool
where
    F: AsyncFnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// --- claiming `Sweep` from the running stack ---

/// How long a claimed test body may run before it is abandoned (its cleanup still runs): a
/// hung request must not leave `Ingest` and `Sweep` claimed and the stack's timer paused.
pub const CLAIM_BODY_DEADLINE: Duration = Duration::from_secs(600);

/// Pause the stack's own reconciliation timer: cancel every unfinished `Sweep/default/run`
/// (re-querying until none remain, so a running pass's freshly journaled reschedule is caught
/// too), so a production pass cannot fire on a test deployment that has claimed `Sweep`.
///
/// Returns the number of timer chains paused, to pass to [`resume_default_sweep`]. It is
/// counted over every invocation the cancel loop met, not over one snapshot before it, which
/// a successor appearing just after would miss.
///
/// # Errors
/// `(chains, why)`: the cancel gave up; `chains` (possibly partly cancelled) is still what a
/// resume must re-arm.
pub async fn pause_default_sweep(
    http: &reqwest::Client,
    admin_url: &str,
) -> Result<usize, (usize, String)> {
    let done = terminate(http, admin_url, default_sweep_runs(), Termination::Cancel).await;
    let chains = chain_roots(&done.seen);
    match done.error {
        None => Ok(chains),
        Some(why) => Err((chains, format!("pause the stack's sweep timer: {why}"))),
    }
}

/// The stack's own timer passes: unfinished `Sweep/default/run`.
fn default_sweep_runs() -> Invocations<'static> {
    Invocations::of(SWEEP_OBJECT)
        .key(SWEEP_KEY)
        .handler(SWEEP_HANDLER)
}

/// How many `Sweep/default/run` timer chains are alive now ([`chain_roots`] over the
/// unfinished ones).
///
/// # Errors
/// A failed introspection.
pub async fn live_default_sweep_chains(
    http: &reqwest::Client,
    admin_url: &str,
) -> Result<usize, String> {
    let rows = admin_rows(
        http,
        admin_url,
        &format!(
            "SELECT id, invoked_by_id FROM sys_invocation WHERE {}",
            default_sweep_runs().filter()
        ),
    )
    .await?;
    let seen = rows
        .iter()
        .filter_map(|r| {
            let id = r.get("id").and_then(serde_json::Value::as_str)?;
            let parent = r
                .get("invoked_by_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            Some((id.to_owned(), parent))
        })
        .collect();
    Ok(chain_roots(&seen))
}

/// The deployment `Sweep` currently routes to (`None` when `Sweep` is not registered).
///
/// # Errors
/// A failed lookup (anything but a success or a 404).
pub async fn sweep_deployment(
    http: &reqwest::Client,
    admin_url: &str,
) -> Result<Option<String>, String> {
    let resp = http
        .get(format!("{admin_url}/services/{SWEEP_OBJECT}"))
        .send()
        .await
        .map_err(|e| format!("{SWEEP_OBJECT} service lookup failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!(
            "{SWEEP_OBJECT} service lookup returned HTTP {status}: {text}"
        ));
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("deployment_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .map(Some)
        .ok_or_else(|| format!("{SWEEP_OBJECT} service answered without a deployment_id: {text}"))
}

/// Re-arm the stack's reconciliation timer after [`pause_default_sweep`]: one status-checked
/// trigger per paused chain that is not alive any more (a pass then reschedules itself), so the
/// number of chains is unchanged. A chain whose pause failed (a refused cancel) is still
/// running and is not doubled. Call it after the test deployment is deregistered: it first
/// requires that `Sweep` routes to `stack_deployment`, the deployment that served it before
/// the claim, so the re-armed pass runs where the stack's timer ran and not on a leftover
/// deployment of another test.
///
/// # Errors
/// `Sweep` is unregistered or routed anywhere but `stack_deployment` (or no deployment served
/// it before the claim), or a trigger was refused; the reason names the manual remedy.
pub async fn resume_default_sweep(
    http: &reqwest::Client,
    admin_url: &str,
    ingress_url: &str,
    chains: usize,
    stack_deployment: Option<&str>,
) -> Result<(), String> {
    if chains == 0 {
        return Ok(());
    }
    let remedy = format!(
        "the stack's sweep timer is paused ({chains} chain(s)); re-register the backend and run `just sweep-trigger` {chains} time(s)"
    );
    let Some(expected) = stack_deployment else {
        return Err(format!(
            "{remedy}: no deployment served {SWEEP_OBJECT} before the claim, so where to re-arm is unknown"
        ));
    };
    let routed_to = sweep_deployment(http, admin_url)
        .await
        .map_err(|why| format!("{remedy}: {why}"))?;
    if routed_to.as_deref() != Some(expected) {
        return Err(format!(
            "{remedy}: {SWEEP_OBJECT} routes to {routed_to:?}, not the stack's {expected}"
        ));
    }
    let live = live_default_sweep_chains(http, admin_url)
        .await
        .map_err(|why| format!("{remedy}: count the live chains: {why}"))?;
    for _ in 0..chains.saturating_sub(live) {
        trigger_first_sweep(ingress_url)
            .await
            .map_err(|why| format!("{remedy}: {why}"))?;
    }
    Ok(())
}

/// Where a test that force-claims `Ingest` and `Sweep` runs, and its own `Sweep` key.
pub struct SweepClaim<'a> {
    /// The HTTP client for the admin and ingress APIs.
    pub http: &'a reqwest::Client,
    /// The Restate admin base URL.
    pub admin_url: &'a str,
    /// The Restate ingress base URL.
    pub ingress_url: &'a str,
    /// The test endpoint's callback URL (its deployment).
    pub callback_url: &'a str,
    /// The `Sweep` key the test runs passes under: every unfinished invocation under it (a
    /// pass, its reschedule) is cancelled on the way out. Its own key normally; the stack's
    /// `SWEEP_KEY` for a test of the production trigger (the paused chains are re-armed after).
    pub sweep_key: &'a str,
}

/// What [`with_sweep_claimed`] observed: the body's outcome, and whether the cleanup left the
/// stack as it was. Do the test's own teardown (repos, source fixtures), then call
/// [`Claimed::into_inner`].
#[must_use = "call into_inner() to surface a body panic or a failed cleanup"]
pub struct Claimed<T> {
    outcome: Result<std::thread::Result<T>, String>,
    cleanup: Result<(), String>,
}

impl<T> Claimed<T> {
    /// The body's value. Re-raises the body's panic first; then fails on a setup error or an
    /// abandoned (timed-out) body, then on a cleanup error (the stack's sweep timer may not be
    /// re-armed; the message says how to do it).
    ///
    /// # Panics
    /// As described.
    pub fn into_inner(self) -> T {
        let cleanup = self.cleanup;
        match self.outcome {
            Ok(Err(panic)) => {
                if let Err(why) = &cleanup {
                    eprintln!("gated test cleanup ALSO failed: {why}");
                }
                std::panic::resume_unwind(panic)
            }
            Err(setup) => panic!(
                "gated test setup/body failed: {setup}{}",
                cleanup
                    .err()
                    .map_or_else(String::new, |why| format!("; cleanup failed: {why}"))
            ),
            Ok(Ok(value)) => match cleanup {
                Ok(()) => value,
                Err(why) => panic!("gated test cleanup failed: {why}"),
            },
        }
    }
}

/// Force-claim `Ingest` and `Sweep` for a test: note which deployment serves `Sweep`, pause
/// the stack's sweep timer, register the test's endpoint, and run `body` with the deployment
/// id (abandoned after [`CLAIM_BODY_DEADLINE`]). Then, whatever the body does (a panic or a
/// hang included): cancel every unfinished invocation under the test's `Sweep` key, cancel
/// what is pinned to the test deployment, deregister, kill anything that got pinned to it in
/// between (a cancel needs the deployment; a kill does not), and re-arm the stack's timer on
/// the deployment that served `Sweep` before.
///
/// The body should itself wait for the work it started (e.g. [`wait_finished`] on its ingests)
/// and return its observations; assertions belong after [`Claimed::into_inner`], so a failed
/// check never skips the cleanup.
pub async fn with_sweep_claimed<T>(
    claim: &SweepClaim<'_>,
    body: impl AsyncFnOnce(&str) -> T,
) -> Claimed<T> {
    let SweepClaim {
        http,
        admin_url,
        ingress_url,
        callback_url,
        sweep_key,
    } = *claim;
    // Where the stack's timer runs, before anything changes the routing.
    let stack_deployment = sweep_deployment(http, admin_url).await;
    let (chains, paused) = match &stack_deployment {
        Ok(_) => match pause_default_sweep(http, admin_url).await {
            Ok(chains) => (chains, Ok(())),
            Err((chains, why)) => (chains, Err(why)),
        },
        Err(why) => (0, Err(why.clone())),
    };
    let deployment = match paused {
        Ok(()) => register(http, admin_url, callback_url).await,
        Err(why) => Err(why),
    };
    let outcome = match &deployment {
        Ok(id) => match tokio::time::timeout(
            CLAIM_BODY_DEADLINE,
            AssertUnwindSafe(body(id)).catch_unwind(),
        )
        .await
        {
            Ok(ran) => Ok(ran),
            Err(_) => Err(format!(
                "the test body did not finish within {CLAIM_BODY_DEADLINE:?} and was abandoned"
            )),
        },
        Err(why) => Err(why.clone()),
    };

    let mut problems = Vec::new();
    // The key first: its reschedules would otherwise keep landing on the test deployment.
    if let Err(why) = cancel_all(
        http,
        admin_url,
        Invocations::of(SWEEP_OBJECT).key(sweep_key),
    )
    .await
    {
        problems.push(format!("cancel the test key's sweep invocations: {why}"));
    }
    if let Ok(id) = &deployment
        && let Err(why) = cancel_all(http, admin_url, Invocations::pinned_to(id)).await
    {
        problems.push(format!("cancel the test deployment's invocations: {why}"));
    }
    if let Err(why) = deregister_checked(http, admin_url, callback_url).await {
        problems.push(format!("deregister {callback_url}: {why}"));
    }
    if let Ok(id) = &deployment {
        // Pinned between the cancel and the delete: stranded now, so killed.
        let killed = terminate(
            http,
            admin_url,
            Invocations::pinned_to(id),
            Termination::Kill,
        )
        .await;
        if let Some(why) = killed.error {
            problems.push(format!(
                "kill what stayed pinned to the test deployment: {why}"
            ));
        }
    }
    let stack_deployment = stack_deployment.ok().flatten();
    if let Err(why) = resume_default_sweep(
        http,
        admin_url,
        ingress_url,
        chains,
        stack_deployment.as_deref(),
    )
    .await
    {
        problems.push(why);
    }
    Claimed {
        outcome,
        cleanup: if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        },
    }
}

// --- loud skip ---
// One shared copy for every crate's harness (see the file's own docs).

#[path = "../../../../test-support/loud_skip.rs"]
mod loud_skip;
// Not every suite uses every helper.
#[allow(unused_imports)]
pub use loud_skip::{REQUIRE_INFRA_ENV, infra_required, infra_unavailable, skip_or_fail};

// --- Odoo fixtures ---
// Shared with ekg-semantic's gated harness (see the file's own docs).

#[path = "../../../../test-support/odoo_fixture.rs"]
pub mod odoo_fixture;

// --- a portfolio and a scripted joint agent, for the agent rollout suites ---

/// `POST {ingress}/{object}/{key}/{handler}` with an optional JSON body: the JSON answer
/// (`null` for an empty one).
///
/// # Errors
/// The status and body of a refused call, or why the ingress could not be reached.
pub async fn post_object(
    http: &reqwest::Client,
    ingress: &str,
    path: [&str; 3],
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut url = reqwest::Url::parse(ingress).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|()| "the ingress url cannot be a base".to_owned())?
        .pop_if_empty()
        .extend(path);
    let mut request = http.post(url);
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{} answered {status}: {text}", path.join("/")));
    }
    if text.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Form a portfolio of `members` in the served `PortfolioService`, as the monitor does.
pub async fn form_portfolio(
    http: &reqwest::Client,
    ingress: &str,
    members: &[ekg_domain::EntityUri],
) -> ekg_dynamic::FormedPortfolio {
    let mut members = members.to_vec();
    members.sort();
    let id = ekg_dynamic::portfolio_id(&members);
    let reason = "linked: the one fulfils the other".to_owned();
    let receipt = post_object(
        http,
        ingress,
        ["PortfolioService", &id.0, "form"],
        Some(&json!({ "members": members, "reason": reason })),
    )
    .await
    .expect("the portfolio forms");
    let generation = receipt["generation"].as_u64().expect("a generation");
    ekg_dynamic::FormedPortfolio {
        id,
        generation: u32::try_from(generation).expect("a small generation"),
        members,
        links: vec![reason],
        coupling: Vec::new(),
    }
}

/// The served portfolio's record.
pub async fn portfolio_record(
    http: &reqwest::Client,
    ingress: &str,
    id: &str,
) -> ekg_dynamic::PortfolioRecord {
    let record = post_object(http, ingress, ["PortfolioService", id, "record"], None)
        .await
        .expect("the portfolio's record");
    serde_json::from_value(record).expect("a formed portfolio")
}

/// The marker on every action of the scripted agent's joint plan.
pub const SCRIPTED_AGENT_PARAM: &str = "drafted_by";

/// The agent's joint slate, scripted: it asks the run's gate, records one run of the given
/// model and skill version, and answers the canned joint slate with every action marked as
/// the agent's. No model is called.
struct ScriptedJointAgent {
    run: ekg_backend::slates::ShadowRun,
    model: String,
    skill_version: String,
}

#[async_trait::async_trait]
impl ekg_domain::GroupSlateProposer for ScriptedJointAgent {
    async fn propose_group_slate(
        &self,
        group: &ekg_domain::SubjectGroup,
    ) -> ekg_domain::Result<ekg_domain::Slate> {
        use std::collections::BTreeMap;
        if !self.run.gate.proceed().await {
            return Err(ekg_domain::EngineError::Unavailable(
                "the rollout gate is closed".to_owned(),
            ));
        }
        self.run.recorder.record(ekg_cognition::RunRecord {
            termination: ekg_cognition::Termination::Submitted,
            steps: 3,
            input_tokens: 100,
            output_tokens: 20,
            model: self.model.clone(),
            elapsed_ms: 1,
            tool_calls: BTreeMap::new(),
            rejections: Vec::new(),
            skills: BTreeMap::from([("propose-plan".to_owned(), self.skill_version.clone())]),
            tools: vec!["ground_entity".to_owned(), "submit_plan".to_owned()],
            trace_id: "scripted".to_owned(),
            closing_answer: None,
            role: None,
            finding: None,
        });
        let mut slate = ekg_domain::fakes::CannedGroupSlateProposer
            .propose_group_slate(group)
            .await?;
        for option in &mut slate.options {
            for action in &mut option.plan.actions {
                action
                    .params
                    .insert(SCRIPTED_AGENT_PARAM.to_owned(), "agent".to_owned());
            }
        }
        Ok(slate)
    }
}

/// The scripted agent's joint slate builder for a shadow step.
pub fn scripted_joint_slates(
    model: &str,
    skill_version: &str,
) -> ekg_backend::slates::JointShadowSlates {
    let (model, skill_version) = (model.to_owned(), skill_version.to_owned());
    Arc::new(move |run| {
        Arc::new(ScriptedJointAgent {
            run,
            model: model.clone(),
            skill_version: skill_version.clone(),
        }) as Arc<dyn ekg_domain::GroupSlateProposer>
    })
}

/// Whether every action of `slate`'s options is marked as the scripted agent's.
pub fn is_scripted_agents(slate: &ekg_domain::Slate) -> bool {
    !slate.options.is_empty()
        && slate.options.iter().all(|option| {
            option
                .plan
                .actions
                .iter()
                .all(|action| action.params.contains_key(SCRIPTED_AGENT_PARAM))
        })
}
