//! Gated live e2e: a portfolio's plan is dismissed as one governed step (the portfolio
//! recorded `dismissed` with its audit, every member aborted with the portfolio named), and it
//! stays dismissed.
//!
//! Drives the real `portfolio_command` router (via `tower::oneshot`) against a live Restate: the
//! `ActionPointService`, `PortfolioService` and `DriftMonitorService` objects, served in-process
//! on port offset 20. The monitor runs over an in-memory world whose fixture risk rule flags the
//! two run subjects (while the test lets it) and whose fixture coupling rule links them, so a
//! real monitor pass forms the portfolio. The dismissal is given an executor that panics on any
//! call: it writes nothing to any system of record. It checks:
//!
//! 1. a dismissal naming a stale generation is refused with 409 and changes nothing;
//! 2. a clean dismissal records the portfolio `dismissed` (reviewer, reason, generation and
//!    members audited) and aborts both members, each audit naming the portfolio;
//! 3. a monitor pass over the same revision does not re-form it, and groups nothing;
//! 4. a later change re-opens the still-flagged subjects, which couple again and form the
//!    portfolio's next generation, the dismissal kept in its audit.
//!
//! Ignored by default; skips if Restate is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is set
//! (as `just test-e2e` does), when it fails.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it portfolio_dismiss_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ekg_backend::action_point::ApiState;
use ekg_backend::drift;
use ekg_backend::portfolio_command::router;
use ekg_domain::{
    ActionExecutor, ChangeKind, EntityUri, ExecutionReceipt, Grounding, NamedGraph, PromotedAction,
    RdfChanged, ReadContext, Result as DomainResult, Revision, SourceRecord, SourceRef, WorldModel,
};
use ekg_dynamic::{
    ActionPointService, CouplingEdge, CouplingRule, DriftMonitorService, DriftVerdict,
    PortfolioService, RiskRule, portfolio_id,
};
use ekg_semantic::ontology;
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tower::ServiceExt;

const OFFSET: u16 = 20;
/// The fixture risk rule's query; the world answers it with the flagged run subjects.
const RISK_SPARQL: &str = "# portfolio_dismiss_e2e_it risk\nSELECT ?entity WHERE {}";
/// The fixture coupling rule's query; the world answers it with the run's one edge.
const COUPLING_SPARQL: &str = "# portfolio_dismiss_e2e_it coupling\nSELECT ?a ?b WHERE {}";
const REASON: &str = "the order waits on the manufacturing order";

/// An in-memory world: the risk rule flags `subjects` while `flagging` is on (so a pass the
/// stack's own traffic routes here while the test is idle flags nothing), and the coupling rule
/// links them. It takes no write.
struct FixtureWorld {
    subjects: [String; 2],
    flagging: AtomicBool,
}

#[async_trait]
impl WorldModel for FixtureWorld {
    async fn materialize(&self, _record: &SourceRecord) -> DomainResult<Revision> {
        panic!("the dismissal scenario materializes nothing");
    }
    async fn retract(&self, _entity: &EntityUri) -> DomainResult<Revision> {
        panic!("the dismissal scenario retracts nothing");
    }
    async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> DomainResult<Grounding> {
        panic!("the dismissal scenario grounds nothing");
    }
    async fn select(&self, sparql: &str) -> DomainResult<Vec<BTreeMap<String, String>>> {
        if !self.flagging.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let [a, b] = &self.subjects;
        Ok(match sparql {
            RISK_SPARQL => vec![
                BTreeMap::from([("entity".to_owned(), a.clone())]),
                BTreeMap::from([("entity".to_owned(), b.clone())]),
            ],
            COUPLING_SPARQL => vec![BTreeMap::from([
                ("a".to_owned(), a.clone()),
                ("b".to_owned(), b.clone()),
            ])],
            _ => Vec::new(),
        })
    }
}

/// An executor that fails the test on any call: a dismissal writes nothing to Odoo.
struct NoWrites;

#[async_trait]
impl ActionExecutor for NoWrites {
    async fn execute(&self, _promoted: &PromotedAction) -> DomainResult<ExecutionReceipt> {
        panic!("a dismissal must write nothing to any system of record");
    }
    async fn reverse(&self, _receipt: &ExecutionReceipt) -> DomainResult<()> {
        panic!("a dismissal must reverse nothing");
    }
}

fn risk_rule() -> RiskRule {
    RiskRule::new(RISK_SPARQL, |row| {
        Some(DriftVerdict {
            subject: EntityUri(row.get("entity")?.clone()),
            reason: "late against the promise".to_owned(),
        })
    })
}

fn coupling_rule() -> CouplingRule {
    CouplingRule::new(COUPLING_SPARQL, |row| {
        CouplingEdge::new(
            EntityUri(row.get("a")?.clone()),
            EntityUri(row.get("b")?.clone()),
            REASON,
        )
    })
}

/// A `RdfChanged` for the first subject at `revision`.
fn changed(id: i64, revision: u64) -> RdfChanged {
    RdfChanged {
        entity_uri: EntityUri(ontology::entity_iri("sale.order", id)),
        named_graph: NamedGraph(ontology::named_graph("sale.order", id)),
        change_kind: ChangeKind::Updated,
        source: SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id,
        },
        revision: Revision(revision),
    }
}

/// POST `body` to `handler` on the `service` object keyed `key`; the JSON answer.
async fn call(
    http: &reqwest::Client,
    ingress: &str,
    service: &str,
    key: &str,
    handler: &str,
) -> Result<Value, String> {
    let mut url = reqwest::Url::parse(ingress).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|()| "base url".to_owned())?
        .pop_if_empty()
        .extend([service, key, handler]);
    let resp = http
        .post(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{service}/{handler} returned {status}: {text}"));
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))
}

async fn record(http: &reqwest::Client, ingress: &str, service: &str, key: &str) -> Value {
    call(http, ingress, service, key, "record")
        .await
        .unwrap_or(Value::Null)
}

/// One monitor pass over `event`, with the fixture rules flagging while it runs.
async fn notify(
    http: &reqwest::Client,
    ingress: &str,
    world: &FixtureWorld,
    event: &RdfChanged,
) -> Result<Value, String> {
    world.flagging.store(true, Ordering::SeqCst);
    let outcome = async {
        let url = drift::monitor_notify_url(ingress)?;
        let resp = http
            .post(url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(event).map_err(|e| e.to_string())?)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("monitor rejected notify: {}", resp.status()));
        }
        let text = resp.text().await.map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))
    }
    .await;
    world.flagging.store(false, Ordering::SeqCst);
    outcome
}

async fn post(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/portfolios/abort")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn dismiss(id: &str, generation: u32) -> Value {
    json!({
        "portfolio": id,
        "generation": generation,
        "reviewer": "planner-1",
        "reason": "the customer moved both orders to next month"
    })
}

fn events(portfolio: &Value) -> Vec<(String, u64)> {
    portfolio["audit"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| Some((e["event"].as_str()?.to_owned(), e["generation"].as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn a_portfolio_is_dismissed_as_one_governed_step_and_stays_dismissed() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(OFFSET);
    let callback_url = common::callback_url(OFFSET);
    let http = reqwest::Client::new();
    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        common::infra_unavailable(&format!("Restate admin unreachable at {admin_url}"));
        return;
    }

    // Two subjects unique to this run, so their durable objects are fresh.
    let order = common::unique_base();
    let subjects = [
        ontology::entity_iri("mrp.production", order),
        ontology::entity_iri("sale.order", order),
    ];
    let members: Vec<EntityUri> = subjects.iter().cloned().map(EntityUri).collect();
    let id = portfolio_id(&members).0;
    let world = Arc::new(FixtureWorld {
        subjects: subjects.clone(),
        flagging: AtomicBool::new(false),
    });

    let endpoint = Endpoint::builder()
        .bind(ActionPointService::default())
        .bind(PortfolioService::default())
        .bind(
            DriftMonitorService::new(Arc::clone(&world) as Arc<dyn WorldModel>, vec![risk_rule()])
                .with_coupling(vec![coupling_rule()]),
        )
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let registered = common::register(&http, &admin_url, &callback_url).await;
    let app = router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor: Arc::new(NoWrites) as Arc<dyn ActionExecutor>,
        undo_window: Duration::from_secs(8 * 60 * 60),
        claim_deadline: ekg_backend::config::DEFAULT_COMMAND_CLAIM,
    });

    let scenario = async || -> Result<BTreeMap<String, Value>, String> {
        registered.clone()?;
        let mut seen = BTreeMap::new();
        let snapshot = async |seen: &mut BTreeMap<String, Value>, at: &str| {
            seen.insert(
                format!("{at}/portfolio"),
                record(&http, &ingress_url, "PortfolioService", &id).await,
            );
            for (i, subject) in subjects.iter().enumerate() {
                seen.insert(
                    format!("{at}/member{i}"),
                    record(&http, &ingress_url, "ActionPointService", subject).await,
                );
            }
        };

        // The monitor opens both points and forms the portfolio (generation 1).
        let formed = notify(&http, &ingress_url, &world, &changed(order, 1)).await?;
        seen.insert("formed/outcome".to_owned(), formed);
        snapshot(&mut seen, "formed").await;

        // 1. A stale generation is refused and changes nothing.
        let (status, body) = post(&app, dismiss(&id, 2)).await;
        seen.insert("stale/status".to_owned(), json!(status.as_u16()));
        seen.insert("stale/body".to_owned(), body);
        snapshot(&mut seen, "stale").await;

        // 2. A clean dismissal.
        let (status, body) = post(&app, dismiss(&id, 1)).await;
        seen.insert("dismiss/status".to_owned(), json!(status.as_u16()));
        seen.insert("dismiss/body".to_owned(), body);
        snapshot(&mut seen, "dismissed").await;

        // 3. A monitor pass over the same revision.
        let again = notify(&http, &ingress_url, &world, &changed(order, 1)).await?;
        seen.insert("again/outcome".to_owned(), again);
        snapshot(&mut seen, "again").await;

        // 4. A later change: the still-flagged subjects re-open and couple again.
        let later = notify(&http, &ingress_url, &world, &changed(order, 2)).await?;
        seen.insert("later/outcome".to_owned(), later);
        snapshot(&mut seen, "later").await;
        Ok(seen)
    };
    let outcome = scenario().await;
    let deregistered = common::deregister_checked(&http, &admin_url, &callback_url).await;
    let seen = outcome.expect("the scenario ran");
    for (label, value) in &seen {
        println!("{label}: {value}");
    }
    deregistered.expect("the test deployment deregistered");

    // Formed: one portfolio at generation 1, both points open and members of it.
    let formed = &seen["formed/outcome"]["formed"];
    assert_eq!(formed[0]["id"], id, "{formed}");
    assert_eq!(formed[0]["generation"], 1);
    for i in 0..2 {
        let member = &seen[&format!("formed/member{i}")];
        assert_eq!(member["point"]["state"], "open", "{member}");
        assert_eq!(member["portfolio"], id);
    }

    // 1. Stale: 409, the portfolio still active, both points open and unclaimed.
    assert_eq!(seen["stale/status"], 409, "{}", seen["stale/body"]);
    assert!(
        seen["stale/body"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("generation 2 is stale")),
        "{}",
        seen["stale/body"]
    );
    assert_eq!(seen["stale/portfolio"]["state"], "active");
    for i in 0..2 {
        let member = &seen[&format!("stale/member{i}")];
        assert_eq!(member["point"]["state"], "open", "{member}");
        assert!(member["claim"].is_null(), "{member}");
    }

    // 2. Dismissed: the portfolio audited, both members aborted with the portfolio named.
    assert_eq!(seen["dismiss/status"], 200, "{}", seen["dismiss/body"]);
    let portfolio = &seen["dismissed/portfolio"];
    assert_eq!(portfolio["state"], "dismissed", "{portfolio}");
    let last = portfolio["audit"]
        .as_array()
        .and_then(|a| a.last())
        .expect("audited");
    assert_eq!(last["event"], "dismissed");
    assert_eq!(last["generation"], 1);
    assert_eq!(last["reviewer"], "planner-1");
    assert_eq!(
        last["reason"],
        "the customer moved both orders to next month"
    );
    assert_eq!(last["members"], json!(subjects));
    assert!(
        !events(portfolio).iter().any(|(e, _)| e == "superseded"),
        "the members leaving does not supersede a dismissed portfolio: {portfolio}"
    );
    for i in 0..2 {
        let member = &seen[&format!("dismissed/member{i}")];
        assert_eq!(member["point"]["state"], "aborted", "{member}");
        assert!(member["claim"].is_null(), "{member}");
        assert!(member.get("portfolio").is_none_or(Value::is_null));
        let entry = member["audit"]
            .as_array()
            .and_then(|a| a.last())
            .expect("audited");
        assert_eq!(entry["reviewer"], "planner-1");
        assert_eq!(
            entry["note"],
            format!(
                "the customer moved both orders to next month (dismissed with portfolio {id}, \
                 generation 1)"
            )
        );
    }

    // 3. The same revision: nothing re-opens, nothing is grouped, the portfolio stays dismissed.
    let again = &seen["again/outcome"];
    assert_eq!(again["opened"], json!([]), "{again}");
    assert_eq!(again["formed"], json!([]), "{again}");
    assert_eq!(seen["again/portfolio"]["state"], "dismissed");
    assert_eq!(seen["again/portfolio"]["generation"], 1);
    for i in 0..2 {
        assert_eq!(
            seen[&format!("again/member{i}")]["point"]["state"],
            "aborted"
        );
    }

    // 4. A later change re-opens both and forms generation 2; the dismissal stays audited.
    let later = &seen["later/outcome"];
    assert_eq!(later["opened"].as_array().map(Vec::len), Some(2), "{later}");
    assert_eq!(later["formed"][0]["id"], id, "{later}");
    assert_eq!(later["formed"][0]["generation"], 2);
    let portfolio = &seen["later/portfolio"];
    assert_eq!(portfolio["state"], "active");
    assert!(
        events(portfolio).contains(&("dismissed".to_owned(), 1)),
        "{portfolio}"
    );
}
