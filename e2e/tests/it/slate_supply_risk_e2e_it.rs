//! Gated live e2e: an open supply-risk (`sale.order`) action point receives a ranked
//! remediation slate whose recommended option clears the shortfall signal under projection
//! while the do-nothing baseline does not, and nothing is committed (no write-back, the
//! world model unchanged).
//!
//! It runs the same slate loop as the process e2e (`slate_e2e_it`), with the supply-risk
//! scorer. The [`SlateAgent`] loop drives the real GraphDB [`Simulator`] (an ephemeral
//! what-if projection per candidate, including the `re_promise_date` projection mapping)
//! and the real [`SupplyRiskScorer`] (the capability's own risk rule over each projection).
//! The bridge's `propose_and_attach_slate` attaches the slate via `attach_slate`, and the
//! test reads it back from the `slate` handler.
//!
//! Candidates come from a stub proposer ([`SupplyRiskStub`]), so the test does not
//! depend on a live LLM. The clearing re-promise is tried first, so the on-time expedite
//! is recommended only because the rank puts an on-time plan ahead of one that moves the
//! promise later, not because of the loop's stable order. The fixture's inbound line
//! arrives through an open receipt, so the expedite only clears if the projection moves
//! the receipt's ETA with the line. The real `supply_risk_slate_agent` composes the
//! bounded agent instead; that path and the per-subject routing are covered by unit tests.
//!
//! Ignored by default. Requires GraphDB and Restate with the worker endpoint reachable:
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it slate_supply_risk_e2e_it -- --ignored --nocapture
//! ```
//!
//! Skips if the stack is unreachable.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ekg_backend::drift::propose_and_attach_slate;
use ekg_cognition::SlateAgent;
use ekg_domain::{
    ActionType, CandidateProposer, EntityUri, EvidenceEdge, GroundedProposal, Proposal,
    ProposedAction, Result, Revision, ScoredCandidate, Scorer, Simulator, Slate, SlateProposer,
    SourceRef, TraceTag, WorldModel,
};
use ekg_dynamic::{ActionPointService, OpenRequest};
use ekg_kinetic::PolicyEngine;
use ekg_semantic::{
    GraphDbClient, GraphDbSimulator, GraphDbWorldModel, SupplyRiskScorer, ontology,
};
use restate_sdk::prelude::Endpoint;
use serde_json::json;

use common::{provision_repo, rec};

const REPO: &str = "ekg_slate_supply_risk_e2e_it";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// An at-risk `sale.order` fixture, like the drift-monitor e2e's. Ids are offset per run so
/// the subject IRI, and with it the persistent `ActionPointService` key, is unique. The
/// order promises an E-Bike (`base`), whose Battery component (`base + 3`) is short at
/// the promise date: required 10, on-hand 2, safety 5, so shortfall 13, because the only
/// inbound purchase line lands `2027-12-01`, after the `2027-10-19` promise, through its
/// purchase order's open receipt (scheduled the same day). Expediting that line to before
/// the promise (`expedite_po`, moving the receipt with it), or moving the promise past the
/// inbound (`re_promise_date`), admits the 20-unit receipt and clears the shortfall.
fn fixtures(base: i64) -> Vec<ekg_domain::SourceRecord> {
    let so = base;
    let line = base + 1;
    let finished = base + 2;
    let comp = base + 3;
    let bom = base + 4;
    let bom_line = base + 5;
    let quant = base + 6;
    let orderpoint = base + 7;
    let po_line = base + 8;
    let po = base + 9;
    let receipt = base + 10;
    vec![
        rec(
            "sale.order",
            so,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", &format!("[{line}]")),
            ],
        ),
        rec(
            "sale.order.line",
            line,
            &[
                ("product_id", &format!("[{finished}]")),
                ("product_uom_qty", "10"),
            ],
        ),
        rec("product.product", finished, &[("name", "E-Bike")]),
        rec(
            "product.product",
            comp,
            &[("name", "Battery"), ("default_code", "BAT")],
        ),
        rec(
            "mrp.bom",
            bom,
            &[
                ("product_id", &format!("[{finished}]")),
                ("bom_line_ids", &format!("[{bom_line}]")),
            ],
        ),
        rec(
            "mrp.bom.line",
            bom_line,
            &[("product_id", &format!("[{comp}]")), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            quant,
            &[("product_id", &format!("[{comp}]")), ("quantity", "2")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            orderpoint,
            &[
                ("product_id", &format!("[{comp}]")),
                ("product_min_qty", "5"),
            ],
        ),
        rec(
            "purchase.order.line",
            po_line,
            &[
                ("product_id", &format!("[{comp}]")),
                ("product_qty", "20"),
                ("date_planned", "2027-12-01T00:00:00"),
            ],
        ),
        rec(
            "purchase.order",
            po,
            &[
                ("state", "purchase"),
                ("order_line", &format!("[{po_line}]")),
            ],
        ),
        rec(
            "stock.picking",
            receipt,
            &[
                ("scheduled_date", "2027-12-01T00:00:00"),
                ("purchase_id", &format!("[{po}]")),
                ("state", "assigned"),
            ],
        ),
    ]
}

/// The fixture's inbound purchase-order line (the shortfall's cause).
fn cause_line(base: i64) -> EntityUri {
    EntityUri(ontology::entity_iri("purchase.order.line", base + 8))
}

/// A scripted candidate generator standing in for the bounded agent, so the test needs
/// neither an LLM nor grounding. By call:
/// 1. a re-promise to `2027-12-20`, past the inbound: it clears the signal, but late
///    (admitted, below the expedite);
/// 2. a re-promise without its date: unscorable, so it must appear in the trace as a
///    rejected step with its reason;
/// 3. an expedite of the cause line to `2027-10-07`, before the promise: it clears
///    only because the projection moves the open receipt's ETA with the line, and keeps
///    the order on time (admitted, recommended);
/// 4. a re-promise to `2027-11-07`, still before the inbound (scored, rejected).
struct SupplyRiskStub {
    line: EntityUri,
    calls: AtomicUsize,
}

#[async_trait]
impl CandidateProposer for SupplyRiskStub {
    async fn propose_candidate(
        &self,
        subject: &EntityUri,
        _feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        let new_date = |date: &str| BTreeMap::from([("new_date".to_owned(), date.to_owned())]);
        let (verb, target, params) = match self.calls.fetch_add(1, Ordering::Relaxed) {
            0 => ("re_promise_date", subject, new_date("2027-12-20 00:00:00")),
            1 => ("re_promise_date", subject, BTreeMap::new()),
            2 => ("expedite_po", &self.line, new_date("2027-10-07 08:00:00")),
            _ => ("re_promise_date", subject, new_date("2027-11-07 00:00:00")),
        };
        let action = ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: target.clone(),
            params,
        };
        Ok(GroundedProposal {
            proposal: Proposal::single(format!("[stub] {verb}"), action, Vec::new()),
            grounding_omissions: Vec::new(),
            // One evidence edge so the reasoning trace emits a Graph step.
            grounding_edges: vec![EvidenceEdge {
                subject: subject.clone(),
                predicate: "ekg:commitment_date".to_owned(),
                object: "2027-10-19T00:00:00".to_owned(),
                source: SourceRef {
                    system: "odoo".to_owned(),
                    model: "sale.order".to_owned(),
                    id: 0,
                },
                revision: Revision(1),
            }],
            subject_members: vec![subject.clone(), self.line.clone()],
        })
    }
}

fn ap_url(ingress: &str, key: &str, handler: &str) -> String {
    let mut url = reqwest::Url::parse(ingress).expect("ingress url");
    {
        let mut segs = url.path_segments_mut().expect("base url");
        segs.pop_if_empty()
            .extend(["ActionPointService", key, handler]);
    }
    url.into()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn an_open_supply_risk_point_receives_a_clearing_slate_and_commits_nothing() {
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(8);
    let callback_url = common::callback_url(8);
    let http = reqwest::Client::new();

    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        eprintln!("SKIP: Restate admin unreachable at {admin_url}");
        return;
    }
    match provision_repo(&http, &graphdb_url, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: GraphDB unreachable at {graphdb_url}");
            return;
        }
        Err(why) => {
            eprintln!("SKIP: GraphDB not ready: {why}");
            return;
        }
    }

    // Materialize the at-risk fixture. Ids are offset per run for a unique subject; no
    // derivation step is needed for the supply-risk rule (unlike the process capacity).
    let base = common::unique_base();
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    for record in fixtures(base) {
        world.materialize(&record).await.expect("materialize");
    }
    let client = GraphDbClient::new(&graphdb_url, REPO).expect("query client");

    let risk = ontology::RISK_RULE_SELECT_SPARQL;
    // The base is at risk, so the point is opened for real drift.
    let base_rows = client.select(risk).await.expect("base risk");
    assert!(!base_rows.is_empty(), "the base fixture must be at risk");

    // The point's subject: the at-risk sales order (the risk rule keys on `?so`).
    let subject = EntityUri(ontology::entity_iri("sale.order", base));

    // Serve the action-point object and register with Restate.
    let endpoint = Endpoint::builder()
        .bind(ActionPointService::default())
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let register = http
        .post(format!("{admin_url}/deployments"))
        .header("Content-Type", "application/json")
        .body(json!({ "uri": callback_url, "force": true }).to_string())
        .send()
        .await
        .expect("register");
    assert!(
        register.status().is_success(),
        "register: {}",
        register.status()
    );

    // Open the supply-risk point.
    let open = http
        .post(ap_url(&ingress_url, &subject.0, "open"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&OpenRequest {
                reason: "component shortfall".to_owned(),
                revision: 1,
            })
            .expect("encode"),
        )
        .send()
        .await
        .expect("open");
    assert!(open.status().is_success(), "open: {}", open.status());

    // The slate agent under test: real GraphDB simulator and real supply-risk scorer, with
    // stub candidate generation. Then run the bridge's attach. Only the scorer differs from
    // the process e2e.
    let simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("sim client"),
    )) as Arc<dyn Simulator>;
    let scorer = Arc::new(SupplyRiskScorer::new()) as Arc<dyn Scorer>;
    let proposer = Arc::new(SupplyRiskStub {
        line: cause_line(base),
        calls: AtomicUsize::new(0),
    }) as Arc<dyn CandidateProposer>;
    let slate_agent =
        Arc::new(SlateAgent::new(proposer, simulator, scorer)) as Arc<dyn SlateProposer>;

    // A policy allowlisting the supply-risk verbs. It has no predicates, so the real world
    // model is read for policy context but yields no needs on this plan.
    let policy = PolicyEngine::new(
        "test-slate/v1",
        [
            "re_promise_date".to_owned(),
            "expedite_po".to_owned(),
            "notify_owner".to_owned(),
        ]
        .into_iter()
        .collect(),
    );
    propose_and_attach_slate(
        &http,
        &ingress_url,
        slate_agent.as_ref(),
        &policy,
        world.as_ref(),
        &subject,
    )
    .await;

    // Read the attached slate back from the durable `slate` handler.
    let slate_resp = http
        .post(ap_url(&ingress_url, &subject.0, "slate"))
        .send()
        .await
        .expect("slate get");
    assert!(
        slate_resp.status().is_success(),
        "slate get: {}",
        slate_resp.status()
    );
    let slate: Option<Slate> =
        serde_json::from_str(&slate_resp.text().await.expect("body")).expect("decode slate");
    let slate = slate.expect("a slate was attached");

    // The expedite of the cause line clears the shortfall and outranks doing nothing.
    let expedite = slate
        .options
        .iter()
        .find(|o| {
            o.plan
                .actions
                .iter()
                .any(|a| a.action_type.0 == "expedite_po" && a.target == cause_line(base))
        })
        .expect("the cause-line expedite is an admitted option");
    assert_eq!(
        expedite
            .score
            .metrics
            .get("signal_cleared")
            .map(String::as_str),
        Some("true"),
        "expediting the cause line (and its receipt) clears the shortfall under projection"
    );
    assert!(
        expedite.score.rank_key < slate.baseline.rank_key,
        "the expedite outranks the do-nothing baseline"
    );

    // The clearing re-promise is admitted too, but below the on-time expedite, and still
    // ahead of doing nothing (the rank is: late, then promise shift, then risk).
    let re_promise = slate
        .options
        .iter()
        .find(|o| {
            o.plan.actions.iter().any(|a| {
                a.action_type.0 == "re_promise_date"
                    && a.params.get("new_date").map(String::as_str) == Some("2027-12-20 00:00:00")
            })
        })
        .expect("the clearing re-promise is an admitted option");
    assert_eq!(
        re_promise
            .score
            .metrics
            .get("signal_cleared")
            .map(String::as_str),
        Some("true"),
        "the re-promise past the inbound also clears the signal"
    );
    assert!(
        expedite.score.rank_key < re_promise.score.rank_key,
        "the on-time expedite ranks ahead of the late re-promise: {} vs {}",
        expedite.score.rank_key,
        re_promise.score.rank_key
    );
    assert!(
        re_promise.score.rank_key < slate.baseline.rank_key,
        "the re-promise ranks ahead of the baseline: {} vs {}",
        re_promise.score.rank_key,
        slate.baseline.rank_key
    );
    assert!(expedite.recommended, "the on-time expedite is recommended");
    assert!(!re_promise.recommended, "the late re-promise is not");
    let order_of: Vec<&str> = slate
        .options
        .iter()
        .filter_map(|o| o.plan.actions.first().map(|a| a.action_type.0.as_str()))
        .collect();
    assert_eq!(
        order_of,
        ["expedite_po", "re_promise_date"],
        "admitted best-first: the expedite, then the re-promise"
    );
    // The per-projection late-order count the rank leads with.
    let late = |metrics: &BTreeMap<String, String>| metrics.get("late_orders").cloned();
    assert_eq!(late(&slate.baseline.metrics).as_deref(), Some("1"));
    assert_eq!(late(&expedite.score.metrics).as_deref(), Some("0"));
    assert_eq!(late(&re_promise.score.metrics).as_deref(), Some("0"));

    // The paramless candidate is visible as rejected, with its reason.
    assert!(
        slate.trace.steps.iter().any(|s| s.rejected
            && s.tag == TraceTag::Simulated
            && s.text.contains("could not be scored")
            && s.text.contains("new_date")),
        "the candidate missing its date is a rejected trace step: {:?}",
        slate.trace.steps
    );

    // The recommended option clears the shortfall under projection; the baseline does not.
    let recommended = slate.recommended().expect("a recommended option");
    assert_eq!(
        recommended
            .score
            .metrics
            .get("signal_cleared")
            .map(String::as_str),
        Some("true"),
        "the recommended plan must clear the shortfall under projection"
    );
    assert_eq!(
        slate
            .baseline
            .metrics
            .get("signal_cleared")
            .map(String::as_str),
        Some("false"),
        "doing nothing must not clear the shortfall"
    );
    assert!(
        recommended.score.rank_key < slate.baseline.rank_key,
        "the recommended option must outrank the do-nothing baseline"
    );

    // The slate carries a structured reasoning trace for oversight (Art. 14): a non-empty
    // ordered trace that opens with the do-nothing baseline and retains the rejected
    // (still-short) candidate the loop tried and set aside.
    assert!(!slate.trace.steps.is_empty(), "the slate carries a trace");
    assert_eq!(
        slate.trace.steps.first().map(|s| s.tag),
        Some(TraceTag::Simulated),
        "the trace opens with the do-nothing baseline"
    );
    assert!(
        slate.trace.summary.rejected >= 1,
        "at least one rejected candidate is retained in the trace"
    );
    assert!(
        slate
            .trace
            .steps
            .iter()
            .any(|s| s.rejected && s.tag == TraceTag::Simulated),
        "the rejected candidate appears as a rejected simulated step"
    );
    // The recommended (admitted) option carries its own per-plan trace slice.
    assert!(
        !recommended.trace.steps.is_empty(),
        "the recommended option carries its own trace slice"
    );

    // Nothing committed: the world model is unchanged, so the base still fires (the slate
    // path touches no ActionExecutor), and the point is still `open` (a slate is not a
    // promotion).
    let base_after = client.select(risk).await.expect("base after");
    assert_eq!(
        base_after.len(),
        base_rows.len(),
        "the authoritative world model must be unchanged by producing a slate"
    );
    let point_after = http
        .post(ap_url(&ingress_url, &subject.0, "get"))
        .send()
        .await
        .expect("get point");
    let value: serde_json::Value =
        serde_json::from_str(&point_after.text().await.expect("body")).expect("decode point");
    assert_eq!(
        value.get("state").and_then(serde_json::Value::as_str),
        Some("open"),
        "producing a slate must not transition the point"
    );

    // Give any spawned projection teardown a moment, then clean up.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;
}
