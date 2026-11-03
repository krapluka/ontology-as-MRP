//! Gated live e2e: the Plant Pulse `/api` contract end to end against the podman substrate.
//!
//! Three paths:
//!
//! - The read-only GraphQL surface: a seeded contention point receives a ranked slate
//!   (the GraphDB [`Simulator`] and [`ProcessContentionScorer`], with a deterministic stub
//!   proposer, as in `slate_e2e`). The test then drives `POST /api/graphql` for the plan
//!   (its options, changes, `needs`, reasoning trace, opaque display metrics and
//!   before/after lanes) and a read-model tab, and asserts the reads commit nothing (the
//!   world model unchanged, the point still `open`).
//! - The governed `undo` command: a promoted, reversible change is listed by
//!   `autoApplied`, then reversed in-window through its recorded inverse (the real
//!   `OdooActionExecutor`), and an undo of a reversed or out-of-window change is refused.
//! - The supply-risk read surface: the display framing of a supply-risk plan, then the
//!   four tabs over a fixture dated from one relative base (today): the plant-wide structure
//!   with the point's chain lit and its causes marked, and, when the rule set cannot be
//!   evaluated, the same tabs served with every risk marker omitted (never denied).
//!
//! Ignored by default. Requires GraphDB and Restate (every part) and Odoo (the undo part),
//! with the worker endpoint reachable from the runtime. Skips if unreachable, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), in which case a missing
//! dependency fails.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it plant_pulse_api_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ekg_backend::action_point::{self, ApiState};
use ekg_backend::command::{CommandLedger, IngressLedger, promote_claimed};
use ekg_backend::config::DEFAULT_COMMAND_CLAIM;
use ekg_backend::drift::propose_and_attach_slate;
use ekg_backend::graphql::{self, GraphqlState, PolicyDisplay};
use ekg_cognition::{CognitionConfig, OpenAiCompatCognition, SlateAgent};
use ekg_domain::{
    ActionPointId, ActionPointState, ActionType, CandidateProposer, CognitionProvider,
    CorrelationId, EntityUri, EvidenceEdge, GroundedProposal, Plan, PromotedPlan, Promotion,
    Proposal, ProposedAction, Proposer, Result, ReviewerId, Revision, ScoredCandidate, Scorer,
    Simulator, SlateProposer, SourceRef, WorldModel,
};
use ekg_dynamic::{ActionPointService, ClaimCommand, OpenRequest, RiskRule};
use ekg_kinetic::{OdooActionExecutor, PolicyEngine};
use ekg_semantic::tabs::Day;
use ekg_semantic::{
    GraphDbClient, GraphDbSimulator, GraphDbWorldModel, ProcessContentionScorer, ontology,
};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{provision_repo, rec};

const REPO: &str = "ekg_plant_pulse_api_e2e_it";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Build an `ActionPointService` ingress URL for a handler.
fn ap_url(ingress: &str, key: &str, handler: &str) -> String {
    let mut url = reqwest::Url::parse(ingress).expect("ingress url");
    {
        let mut segs = url.path_segments_mut().expect("base url");
        segs.pop_if_empty()
            .extend(["ActionPointService", key, handler]);
    }
    url.into()
}

/// POST a GraphQL query to the router (via `oneshot`) and return the parsed JSON body.
async fn graphql_post(app: &axum::Router, query: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/graphql")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "query": query }).to_string()))
                .expect("graphql req"),
        )
        .await
        .expect("graphql oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "GraphQL POST should be 200");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).expect("json body");
    assert!(
        json.get("errors").is_none(),
        "GraphQL returned errors: {json}"
    );
    json
}

/// The contended process fixture from `slate_e2e`: a capacity-1 work centre performing two
/// overlapping work orders, so the contention rule fires. `base` offsets ids per run.
fn fixtures(base: i64) -> Vec<ekg_domain::SourceRecord> {
    let wc = base;
    let wo1 = base + 1;
    let wo2 = base + 2;
    vec![
        rec("mrp.workcenter", wc, &[("name", "Shared Line")]),
        rec(
            "mrp.workcenter.capacity",
            wc,
            &[
                ("workcenter_id", &format!("[{wc}]")),
                ("product_id", "[900]"),
                ("capacity", "1"),
            ],
        ),
        rec(
            "mrp.workorder",
            wo1,
            &[
                ("production_id", &format!("[{wo1}]")),
                ("workcenter_id", &format!("[{wc}]")),
                ("date_start", "2027-10-11T08:00:00"),
                ("date_finished", "2027-10-11T12:00:00"),
                ("duration_expected", "240"),
            ],
        ),
        rec(
            "mrp.workorder",
            wo2,
            &[
                ("production_id", &format!("[{wo2}]")),
                ("workcenter_id", &format!("[{wc}]")),
                ("date_start", "2027-10-11T09:00:00"),
                ("date_finished", "2027-10-11T13:00:00"),
                ("duration_expected", "240"),
            ],
        ),
    ]
}

/// The `slate_e2e` deterministic proposer: a first (rejected) reschedule that leaves the
/// overlap, revised to a clearing (admitted) one, so the attached slate carries a retained
/// rejected candidate alongside the recommended clearing one.
struct RevisingProposer {
    target: EntityUri,
}

#[async_trait]
impl CandidateProposer for RevisingProposer {
    async fn propose_candidate(
        &self,
        _subject: &EntityUri,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        let new_date = if feedback.is_empty() {
            "2027-10-11T09:00:00"
        } else {
            "2027-10-11T12:00:00"
        };
        let action = ProposedAction {
            action_type: ActionType("reschedule_work_order".to_owned()),
            target: self.target.clone(),
            params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
        };
        Ok(GroundedProposal {
            proposal: Proposal::single(
                format!("[stub] reschedule work order 2 to {new_date}"),
                action,
                Vec::new(),
            ),
            grounding_omissions: Vec::new(),
            grounding_edges: vec![EvidenceEdge {
                subject: self.target.clone(),
                predicate: "ekg:startedAt".to_owned(),
                object: "2027-10-11T09:00:00".to_owned(),
                source: SourceRef {
                    system: "odoo".to_owned(),
                    model: "mrp.workorder".to_owned(),
                    id: 0,
                },
                revision: Revision(1),
            }],
            subject_members: vec![self.target.clone()],
        })
    }
}

/// The policy display values the read surface shows (mirrors the `main` wiring).
fn policy_display(undo_window: Duration) -> PolicyDisplay {
    PolicyDisplay {
        policy_id: "local-autopromote/v1".to_owned(),
        cost_limit: Some(250.0),
        cost_limit_currency: Some("USD".to_owned()),
        confirmed_states: vec!["purchase".to_owned(), "done".to_owned()],
        auto_apply_actions: vec!["reschedule_work_order".to_owned()],
        undo_window,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn graphql_reads_a_plan_and_a_tab_and_commits_nothing() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    // One throwaway repo (and its projections) at a time: GraphDB Free's repository cap.
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(9);
    let callback_url = common::callback_url(9);
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
    match provision_repo(&http, &graphdb_url, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            common::infra_unavailable(&format!("GraphDB unreachable at {graphdb_url}"));
            return;
        }
        Err(why) => {
            common::infra_unavailable(&format!("GraphDB not ready: {why}"));
            return;
        }
    }

    // Seed and derive the contended fixture (ids offset per run for a unique subject).
    let base = common::unique_base();
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    for record in fixtures(base) {
        world.materialize(&record).await.expect("materialize");
    }
    let query_client = GraphDbClient::new(&graphdb_url, REPO).expect("query client");
    query_client
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive process edges");

    let contention = ontology::PROCESS_CONTENTION_SELECT_SPARQL;
    let base_rows = query_client
        .select(contention)
        .await
        .expect("base contention");
    assert!(!base_rows.is_empty(), "the base fixture must be contended");

    let subject = EntityUri(ontology::entity_iri("mrp.production", base + 2));
    let target_wo = EntityUri(ontology::entity_iri("mrp.workorder", base + 2));

    // Serve and register the action-point object.
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

    // Open the contention point.
    let open = http
        .post(ap_url(&ingress_url, &subject.0, "open"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&OpenRequest {
                reason: "resource contention".to_owned(),
                revision: 1,
            })
            .expect("encode"),
        )
        .send()
        .await
        .expect("open");
    assert!(open.status().is_success(), "open: {}", open.status());

    // Attach a slate (the GraphDB simulator and scorer, deterministic proposer).
    let simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("sim client"),
    )) as Arc<dyn Simulator>;
    let scorer = Arc::new(ProcessContentionScorer::new()) as Arc<dyn Scorer>;
    let proposer = Arc::new(RevisingProposer { target: target_wo }) as Arc<dyn CandidateProposer>;
    let slate_agent =
        Arc::new(SlateAgent::new(proposer, simulator, scorer)) as Arc<dyn SlateProposer>;
    let policy = PolicyEngine::new(
        "test-slate/v1",
        [
            "reschedule_work_order".to_owned(),
            "reassign_work_center".to_owned(),
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

    // The read-only GraphQL surface under test.
    let gql_simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("gql sim client"),
    )) as Arc<dyn Simulator>;
    let app = graphql::router(GraphqlState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        world: Arc::clone(&world) as Arc<dyn WorldModel>,
        simulator: gql_simulator,
        rules: ekg_backend::drift::monitor_rules(),
        policy: policy_display(Duration::from_secs(8 * 60 * 60)),
    });

    // --- the plan: options / needs / trace / display metrics / lanes ---
    let plan_query = format!(
        "{{ plan(subject: \"{}\") {{ subject resolves \
           baseline {{ metrics {{ key }} }} \
           trace {{ summary {{ rejected }} }} \
           options {{ recommended rankKey needs {{ code }} changes {{ verb object }} \
             trace {{ summary {{ rejected }} }} metrics {{ key value }} \
             lanes {{ workCenter before {{ workOrder }} after {{ workOrder changed }} }} }} }} }}",
        subject.0
    );
    let plan = graphql_post(&app, &plan_query).await;
    let plan = &plan["data"]["plan"];
    assert_eq!(plan["subject"].as_str(), Some(subject.0.as_str()));
    let resolves = plan["resolves"].as_array().expect("resolves list");
    assert_eq!(resolves.len(), 1, "resolves is a one-element list today");
    let options = plan["options"].as_array().expect("options");
    assert!(!options.is_empty(), "the slate carries at least one option");
    assert!(
        options
            .iter()
            .any(|o| o["recommended"].as_bool() == Some(true)),
        "one option is recommended"
    );
    // The slate-level trace retains the rejected candidate the loop tried.
    assert!(
        plan["trace"]["summary"]["rejected"].as_i64().unwrap_or(0) >= 1,
        "the reasoning trace retains a rejected candidate"
    );
    let recommended = options
        .iter()
        .find(|o| o["recommended"].as_bool() == Some(true))
        .expect("a recommended option");
    // The opaque display-metrics bag carries the derived Delivery/Cost.
    let metric_keys: Vec<&str> = recommended["metrics"]
        .as_array()
        .expect("metrics")
        .iter()
        .filter_map(|m| m["key"].as_str())
        .collect();
    assert!(
        metric_keys.contains(&"delivery"),
        "derived delivery metric present: {metric_keys:?}"
    );
    assert!(
        metric_keys.contains(&"cost"),
        "derived cost metric present: {metric_keys:?}"
    );
    // Before/after work-center lanes are derived (the reschedule moved a bar).
    let lanes = recommended["lanes"].as_array().expect("lanes");
    assert!(
        !lanes.is_empty(),
        "the contention plan exposes work-center lanes"
    );
    let any_before = lanes
        .iter()
        .any(|l| l["before"].as_array().is_some_and(|bars| !bars.is_empty()));
    assert!(any_before, "the lanes carry the current (before) schedule");
    let any_changed = lanes.iter().any(|l| {
        l["after"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|b| b["changed"].as_bool() == Some(true))
    });
    assert!(
        any_changed,
        "the recommended option moved a bar (after != before)"
    );

    // --- a read-model tab: the Flow projection of the subject's work orders ---
    let tab_query = format!(
        "{{ readModel(subject: \"{}\", tab: FLOW) {{ tab rows {{ fields {{ key value }} }} }} }}",
        subject.0
    );
    let tab = graphql_post(&app, &tab_query).await;
    let rows = tab["data"]["readModel"]["rows"]
        .as_array()
        .expect("tab rows");
    assert!(
        !rows.is_empty(),
        "the Flow tab projects the subject's work orders"
    );

    // --- reads commit nothing: the world model is unchanged and the point still open ---
    let after_rows = query_client
        .select(contention)
        .await
        .expect("contention after");
    assert_eq!(
        after_rows.len(),
        base_rows.len(),
        "serving GraphQL reads must not change the authoritative world model"
    );
    let point = http
        .post(ap_url(&ingress_url, &subject.0, "get"))
        .send()
        .await
        .expect("get point");
    let point: Value =
        serde_json::from_str(&point.text().await.expect("body")).expect("decode point");
    assert_eq!(
        point["state"].as_str(),
        Some("open"),
        "serving GraphQL reads must not transition the point"
    );

    // cleanup
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;
}

/// Open and attach a `re_promise_date` proposal for `subject`, leaving it `proposed`.
async fn drive_to_proposed(
    http: &reqwest::Client,
    ingress: &str,
    subject: &EntityUri,
    new_date: &str,
) {
    let open = http
        .post(ap_url(ingress, &subject.0, "open"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&OpenRequest {
                reason: "battery shortfall before the promise".to_owned(),
                revision: 1,
            })
            .expect("encode open"),
        )
        .send()
        .await
        .expect("open");
    assert!(open.status().is_success(), "open failed: {}", open.status());
    let grounded = GroundedProposal {
        proposal: Proposal::single(
            "re-promise the delivery date to absorb the battery delay".to_owned(),
            ProposedAction {
                action_type: ActionType("re_promise_date".to_owned()),
                target: subject.clone(),
                params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
            },
            Vec::new(),
        ),
        grounding_omissions: Vec::new(),
        grounding_edges: Vec::new(),
        subject_members: Vec::new(),
    };
    let attach = http
        .post(ap_url(ingress, &subject.0, "attach_proposal"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&grounded).expect("encode proposal"))
        .send()
        .await
        .expect("attach");
    assert!(
        attach.status().is_success(),
        "attach failed: {}",
        attach.status()
    );
}

async fn new_order(odoo: &Arc<Odoo>, initial: &str) -> (i64, i64) {
    let o = Arc::clone(odoo);
    let initial = initial.to_owned();
    tokio::task::spawn_blocking(move || {
        let partner = o
            .create("res.partner", json!({ "name": "EKG plant-pulse undo IT" }))
            .expect("create partner");
        let order = o
            .create("sale.order", json!({ "partner_id": partner }))
            .expect("create sale.order");
        o.write(
            "sale.order",
            json!([order]),
            json!({ "commitment_date": initial }),
        )
        .expect("set commitment_date");
        (partner, order)
    })
    .await
    .expect("odoo setup join")
}

async fn commitment_date(odoo: &Arc<Odoo>, order: i64) -> Value {
    let o = Arc::clone(odoo);
    tokio::task::spawn_blocking(move || {
        let rows = o
            .read("sale.order", json!([order]), json!(["commitment_date"]))
            .expect("read commitment_date");
        rows.as_array()
            .and_then(|a| a.first())
            .and_then(|r| r.get("commitment_date"))
            .cloned()
            .unwrap_or(Value::Bool(false))
    })
    .await
    .expect("read join")
}

async fn point_state(http: &reqwest::Client, ingress: &str, subject: &str) -> Option<String> {
    let resp = http
        .post(ap_url(ingress, subject, "get"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let value: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    value
        .get("state")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Promote `subject`'s staged re-promise as the auto-apply policy, through the same claim
/// protocol the monitor bridge uses, because a human command endpoint refuses a `policy:`
/// reviewer. Returns whether the promotion was written back and recorded.
async fn promote_as_policy(
    http: &reqwest::Client,
    ingress: &str,
    executor: &OdooActionExecutor,
    subject: &EntityUri,
    new_date: &str,
) -> bool {
    let ledger = IngressLedger::new(http.clone(), ingress.to_owned(), DEFAULT_COMMAND_CLAIM);
    let reviewer = "policy:local-autopromote/v1";
    let token = ledger
        .claim(
            &subject.0,
            ClaimCommand::AutoPromote,
            reviewer,
            &[ActionPointState::Proposed],
        )
        .await
        .expect("the policy claims the proposed point");
    let promoted = PromotedPlan {
        plan: Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("re_promise_date".to_owned()),
                target: subject.clone(),
                params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
            }],
        },
        promotion: Promotion {
            action_point: ActionPointId(format!("action-point/{}", subject.0)),
            correlation_id: CorrelationId(format!("auto-promote/{}", subject.0)),
            reviewer: ReviewerId(reviewer.to_owned()),
            rationale: "auto-promoted by policy local-autopromote/v1".to_owned(),
            context_seen: "promotion tier auto-promote".to_owned(),
        },
        basis_revision: Revision(0),
    };
    promote_claimed(&ledger, executor, &subject.0, &token, &promoted)
        .await
        .is_ok()
}

/// A human promote through the REST endpoint that names the policy as its reviewer.
async fn promote_via_rest_as_policy(app: &axum::Router, subject: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post("/api/action-points/promote")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "subject": subject,
                        "reviewer": "policy:local-autopromote/v1",
                        "rationale": "pretending to be the policy",
                    })
                    .to_string(),
                ))
                .expect("promote req"),
        )
        .await
        .expect("promote oneshot")
        .status()
}

/// The note of the audit entry that moved `subject` to `state`, and its reviewer.
async fn audit_note(
    http: &reqwest::Client,
    ingress: &str,
    subject: &str,
    state: &str,
) -> (Option<String>, Option<String>) {
    let text = http
        .post(ap_url(ingress, subject, "record"))
        .send()
        .await
        .expect("record")
        .text()
        .await
        .expect("record body");
    let record: Value = serde_json::from_str(&text).expect("record json");
    let entry = record["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|e| e["state"].as_str() == Some(state) && e.get("kind").is_none());
    (
        entry.and_then(|e| e["note"].as_str().map(str::to_owned)),
        entry.and_then(|e| e["reviewer"].as_str().map(str::to_owned)),
    )
}

/// The reason every undo in this test sends; the audit trail must record it verbatim.
const UNDO_REASON: &str = "supplier fell through";

async fn undo(app: &axum::Router, subject: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post("/api/action-points/undo")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "subject": subject, "reviewer": "planner-1", "reason": UNDO_REASON })
                        .to_string(),
                ))
                .expect("undo req"),
        )
        .await
        .expect("undo oneshot")
        .status()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn undo_lists_and_reverses_in_window_and_refuses_out_of_window() {
    const INITIAL: &str = "2027-01-07 00:00:00";
    const REPROMISED: &str = "2027-12-31 00:00:00";
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    // One throwaway repo (and its projections) at a time: GraphDB Free's repository cap.
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");

    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(10);
    let callback_url = common::callback_url(10);
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
    let (o_url, o_db, o_user, o_key) = (
        env_or("ODOO_URL", "http://localhost:8069"),
        env_or("ODOO_DB", "ekg"),
        env_or("ODOO_USER", "admin"),
        env_or("ODOO_API_KEY", "admin"),
    );
    let (cu, cd, cus, ck) = (o_url.clone(), o_db.clone(), o_user.clone(), o_key.clone());
    let odoo = match tokio::task::spawn_blocking(move || Odoo::connect(&cu, &cd, &cus, &ck))
        .await
        .expect("connect join")
    {
        Ok(odoo) => Arc::new(odoo),
        Err(why) => {
            common::infra_unavailable(&format!("Odoo unreachable ({why})"));
            return;
        }
    };
    let executor = Arc::new(
        OdooActionExecutor::connect(&o_url, &o_db, &o_user, &o_key)
            .await
            .expect("executor connect"),
    );

    // Serve and register the action-point object.
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

    // A world model and a simulator are needed to build the read surface, but neither the
    // `autoApplied` list nor `undo` touches them, so they point at the default repo.
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, "ekg").expect("world client"),
        ontology::supply_risk_contract().expect("contract"),
    )) as Arc<dyn WorldModel>;
    let simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, "ekg").expect("sim client"),
    )) as Arc<dyn Simulator>;

    // Build the write API (with a generous undo window) merged with the read surface.
    let read_app = graphql::router(GraphqlState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        world,
        simulator,
        rules: ekg_backend::drift::monitor_rules(),
        policy: policy_display(Duration::from_secs(8 * 60 * 60)),
    });
    let open_window = action_point::router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor: Arc::clone(&executor) as Arc<dyn ekg_domain::ActionExecutor>,
        undo_window: Duration::from_secs(8 * 60 * 60),
        claim_deadline: DEFAULT_COMMAND_CLAIM,
    })
    .merge(read_app);
    // A second write API whose undo window is already closed (Duration::ZERO).
    let closed_window = action_point::router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor: Arc::clone(&executor) as Arc<dyn ekg_domain::ActionExecutor>,
        undo_window: Duration::ZERO,
        claim_deadline: DEFAULT_COMMAND_CLAIM,
    });

    // Two throwaway orders: one to reverse in-window, one to refuse out-of-window.
    let (in_partner, in_order) = new_order(&odoo, INITIAL).await;
    let (out_partner, out_order) = new_order(&odoo, INITIAL).await;
    let in_subject = EntityUri(format!("https://ekg.example/id/sale.order/{in_order}"));
    let out_subject = EntityUri(format!("https://ekg.example/id/sale.order/{out_order}"));

    drive_to_proposed(&http, &ingress_url, &in_subject, REPROMISED).await;
    drive_to_proposed(&http, &ingress_url, &out_subject, REPROMISED).await;

    // A human command cannot act as the policy: refused with 422, nothing written.
    assert_eq!(
        promote_via_rest_as_policy(&open_window, &in_subject.0).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a policy: reviewer on the human promote endpoint is refused"
    );

    // Promote both as the policy (the governed write-back moves the commitment date).
    assert!(
        promote_as_policy(&http, &ingress_url, &executor, &in_subject, REPROMISED).await,
        "in-window promote"
    );
    assert!(
        promote_as_policy(&http, &ingress_url, &executor, &out_subject, REPROMISED).await,
        "out-window promote"
    );

    // The read surface lists the auto-applied change with its criteria and reversal window.
    let auto = graphql_post(
        &open_window,
        "{ autoApplied { subject reversible undoUntil criteria { rule } changes { verb inverse } } }",
    )
    .await;
    let applied = auto["data"]["autoApplied"]
        .as_array()
        .expect("autoApplied list");
    let listed = applied
        .iter()
        .find(|a| a["subject"].as_str() == Some(in_subject.0.as_str()))
        .expect("the promoted change is listed as auto-applied");
    assert_eq!(
        listed["reversible"].as_bool(),
        Some(true),
        "the change is reversible"
    );
    assert!(
        listed["undoUntil"].as_str().is_some(),
        "it carries a reversal window"
    );
    assert!(
        listed["criteria"].as_array().is_some_and(|c| !c.is_empty()),
        "it lists the policy criteria it satisfied"
    );
    assert!(
        listed["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["inverse"].as_str().is_some()),
        "each auto-applied change carries its recorded inverse"
    );

    // Undo in-window: the governed reverse-write-back restores the commitment date.
    let undo_in = undo(&open_window, &in_subject.0).await;
    // Undo out-of-window: refused with an explicit reason (409), no reverse attempted. The
    // window is whole seconds and inclusive (`now <= promoted_at + window`), so a zero window
    // closes only once the promotion's second has passed. Wait it out, or an undo landing
    // in the same second is still in-window.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let undo_out = undo(&closed_window, &out_subject.0).await;

    let in_state = point_state(&http, &ingress_url, &in_subject.0).await;
    let out_state = point_state(&http, &ingress_url, &out_subject.0).await;
    let (reversed_note, reversed_by) =
        audit_note(&http, &ingress_url, &in_subject.0, "reversed").await;
    let in_date = commitment_date(&odoo, in_order).await;
    let out_date = commitment_date(&odoo, out_order).await;
    // A second undo of the now-reversed point is refused (not promoted).
    let undo_again = undo(&open_window, &in_subject.0).await;

    // cleanup
    let cleanup = Arc::clone(&odoo);
    tokio::task::spawn_blocking(move || {
        let _ = cleanup.unlink("sale.order", json!([in_order, out_order]));
        let _ = cleanup.unlink("res.partner", json!([in_partner, out_partner]));
    })
    .await
    .expect("cleanup join");
    common::deregister(&http, &admin_url, &callback_url).await;

    // --- assertions ---
    assert_eq!(undo_in, StatusCode::OK, "in-window undo should be 200");
    assert_eq!(
        in_state.as_deref(),
        Some("reversed"),
        "the undone point reaches `reversed`"
    );
    assert_eq!(
        in_date.as_str(),
        Some(INITIAL),
        "undo reversed the change through its recorded inverse (commitment date restored)"
    );
    assert_eq!(
        reversed_note.as_deref(),
        Some(UNDO_REASON),
        "the audit trail records the undo reason exactly as sent"
    );
    assert_eq!(reversed_by.as_deref(), Some("planner-1"));

    assert_eq!(
        undo_out,
        StatusCode::CONFLICT,
        "out-of-window undo should be refused (409)"
    );
    assert_eq!(
        out_state.as_deref(),
        Some("promoted"),
        "the refused point stays `promoted`"
    );
    assert_eq!(
        out_date.as_str(),
        Some(REPROMISED),
        "a refused undo attempts no reverse (the re-promised date stands)"
    );

    assert_eq!(
        undo_again,
        StatusCode::CONFLICT,
        "undo of a non-promoted (reversed) point is refused"
    );
}

// --- supply-risk display framing ---

const SUPPLY_REPO: &str = "ekg_plant_pulse_display_e2e_it";

/// `today + days` at `time`, as the ISO dateTime the source would carry. Every fixture date
/// derives from one base (today): the OTD and demand windows the tabs serve are day-relative.
fn at(today: Day, days: i64, time: &str) -> String {
    format!("{}T{time}", today.plus(days).iso())
}

/// The at-risk order's promise: inside the 30-day demand window.
const COMMITMENT_DAYS: i64 = 10;
/// The late inbound battery line's planned date: after the promise.
const LATE_INBOUND_DAYS: i64 = 40;
/// Its open receipt's ETA: later still, and the line's true arrival (the rule's, the
/// coverage date's and the expedite's `from`), so an arrival read is told apart from the
/// planned date.
const LATE_RECEIPT_DAYS: i64 = 42;
/// The stub's first re-promise (still before the inbound, so still short and rejected).
const EARLY_REPROMISE_DAYS: i64 = 20;
/// The stub's revised re-promise (past the inbound, so it clears, but late: admitted, not
/// recommended).
const LATE_REPROMISE_DAYS: i64 = 60;
/// The stub's expedite of the cause line: before the fulfilling lot starts, so the lot gets its
/// battery and ships on time (an order fulfilled by a lot is judged lot by lot). It clears
/// on time, so it is recommended.
const EXPEDITE_DAYS: i64 = 1;
/// The fulfilling manufacturing lot's planned start and finish (a 6-day window).
const LOT_START_DAYS: i64 = 2;
/// See [`LOT_START_DAYS`]; before the promise, so the lot itself is feasible.
const LOT_END_DAYS: i64 = 8;

/// The ids of the supply-risk fixture world, offset by a per-run base.
#[derive(Clone, Copy)]
struct SupplyIds {
    so: i64,
    line: i64,
    finished: i64,
    comp: i64,
    bom: i64,
    bom_line: i64,
    quant: i64,
    orderpoint: i64,
    po_line: i64,
    supplier: i64,
    supplierinfo: i64,
    po: i64,
    po_history: i64,
    picking: i64,
    po_history_line: i64,
    customer: i64,
    frame: i64,
    frame_bom_line: i64,
    frame_quant: i64,
    frameco: i64,
    frameco_info: i64,
    mo: i64,
    currency: i64,
    receipt: i64,
}

impl SupplyIds {
    fn new(base: i64) -> Self {
        Self {
            so: base,
            line: base + 1,
            finished: base + 2,
            comp: base + 3,
            bom: base + 4,
            bom_line: base + 5,
            quant: base + 6,
            orderpoint: base + 7,
            po_line: base + 8,
            supplier: base + 9,
            supplierinfo: base + 10,
            po: base + 11,
            po_history: base + 12,
            picking: base + 13,
            po_history_line: base + 14,
            customer: base + 15,
            frame: base + 16,
            frame_bom_line: base + 17,
            frame_quant: base + 18,
            frameco: base + 19,
            frameco_info: base + 20,
            mo: base + 21,
            currency: base + 22,
            receipt: base + 23,
        }
    }

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }
}

/// An at-risk `sale.order` fixture carrying an `amount_total` (read as `ekg:order_value`),
/// all dates relative to `today`:
///
/// - A customer orders 10 E-Bikes promised at today+10 (12,500 USD; the order and its POs carry
///   `currency_id`, a `res.currency` named USD). The E-Bike's BOM needs one Battery and one
///   Frame.
/// - Battery: 2 on hand, safety 5, sourced only from CellTech (lead time 14). Its only open
///   inbound is CellTech's PO line of 20 planned at today+40, arriving through its PO's open
///   receipt at today+42, after the promise, so short by 10 + 5 - 2 = 13. The order is fulfilled
///   by one lot (below), so it is judged lot by lot: doing nothing, the lot waits for that
///   receipt and then takes its 6-day window, late by 38 days. Expediting the line before
///   the lot starts keeps the order on time (recommended); re-promising past the inbound
///   clears the signal too, but the lot still waits, so it is late (admitted below it).
/// - CellTech's historical PO carries a battery line planned at today-10 that is never open
///   inbound, for two independent reasons under the shared open-inbound definition: the line
///   is fully received (`qty_received = product_qty`, so no open quantity) and its only receipt
///   is completed (no open shipment). So its absence below does not prove either rule alone.
///   That receipt completed a day before the planned day, inside the 90-day OTD window, so
///   OTD is 1.0.
/// - Frame: 100 on hand, sourced only from FrameCo, so never short and FrameCo carries no
///   exposure.
/// - A manufacturing order (origin = the order's name) fulfils the order (`ekg:fulfills`,
///   derived), planned from today+2 to today+8, before the promise, so it is feasible.
fn supply_risk_fixtures(ids: SupplyIds, today: Day) -> Vec<ekg_domain::SourceRecord> {
    let SupplyIds {
        so,
        line,
        finished,
        comp,
        bom,
        bom_line,
        quant,
        orderpoint,
        po_line,
        supplier,
        supplierinfo,
        po,
        po_history,
        picking,
        po_history_line,
        customer,
        frame,
        frame_bom_line,
        frame_quant,
        frameco,
        frameco_info,
        mo,
        currency,
        receipt,
    } = ids;
    let so_name = format!("SO{so}");
    let in_usd = format!("[{currency}]");
    vec![
        rec("res.currency", currency, &[("name", "USD")]),
        rec(
            "res.partner",
            customer,
            &[("name", "VeloMax Retail"), ("customer_rank", "1")],
        ),
        rec(
            "sale.order",
            so,
            &[
                ("name", &so_name),
                ("partner_id", &format!("[{customer}]")),
                ("commitment_date", &at(today, COMMITMENT_DAYS, "00:00:00")),
                ("order_line", &format!("[{line}]")),
                ("amount_total", "12500.00"),
                ("currency_id", &in_usd),
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
        rec("product.product", frame, &[("name", "Frame")]),
        rec(
            "mrp.bom",
            bom,
            &[
                ("product_id", &format!("[{finished}]")),
                ("bom_line_ids", &format!("[{bom_line}, {frame_bom_line}]")),
            ],
        ),
        rec(
            "mrp.bom.line",
            bom_line,
            &[("product_id", &format!("[{comp}]")), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom.line",
            frame_bom_line,
            &[("product_id", &format!("[{frame}]")), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            quant,
            &[("product_id", &format!("[{comp}]")), ("quantity", "2")],
        ),
        rec(
            "stock.quant",
            frame_quant,
            &[("product_id", &format!("[{frame}]")), ("quantity", "100")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            orderpoint,
            &[
                ("product_id", &format!("[{comp}]")),
                ("product_min_qty", "5"),
            ],
        ),
        // The battery supplier and its sourcing lead time (Suppliers lead-time signal).
        rec(
            "res.partner",
            supplier,
            &[("name", "CellTech"), ("supplier_rank", "1")],
        ),
        rec(
            "product.supplierinfo",
            supplierinfo,
            &[
                ("product_id", &format!("[{comp}]")),
                ("partner_id", &format!("[{supplier}]")),
                ("delay", "14"),
                ("sequence", "1"),
            ],
        ),
        // The frame's only supplier: sources a non-cause component, so no exposure.
        rec(
            "res.partner",
            frameco,
            &[("name", "FrameCo"), ("supplier_rank", "1")],
        ),
        rec(
            "product.supplierinfo",
            frameco_info,
            &[
                ("product_id", &format!("[{frame}]")),
                ("partner_id", &format!("[{frameco}]")),
                ("delay", "7"),
                ("sequence", "1"),
            ],
        ),
        // The at-risk inbound PO: its open line arrives after the promise (through its open
        // receipt, whose ETA is the arrival), so the rule cannot count it.
        rec(
            "purchase.order",
            po,
            &[
                ("name", &format!("P{po}")),
                ("partner_id", &format!("[{supplier}]")),
                ("order_line", &format!("[{po_line}]")),
                ("state", "purchase"),
                ("currency_id", &in_usd),
            ],
        ),
        rec(
            "stock.picking",
            receipt,
            &[
                ("purchase_id", &format!("[{po}]")),
                ("scheduled_date", &at(today, LATE_RECEIPT_DAYS, "00:00:00")),
                ("state", "assigned"),
            ],
        ),
        rec(
            "purchase.order.line",
            po_line,
            &[
                ("product_id", &format!("[{comp}]")),
                ("product_qty", "20"),
                ("date_planned", &at(today, LATE_INBOUND_DAYS, "00:00:00")),
            ],
        ),
        // A separate historical PO from the same supplier, received on time. Its completed
        // receipt gives the supplier an on-time-delivery figure. It is never open inbound
        // (never lit, never on order): its battery line is fully received and its only
        // receipt is completed, and either alone would keep it out.
        rec(
            "purchase.order",
            po_history,
            &[
                ("partner_id", &format!("[{supplier}]")),
                ("order_line", &format!("[{po_history_line}]")),
                ("state", "purchase"),
                ("currency_id", &in_usd),
            ],
        ),
        rec(
            "purchase.order.line",
            po_history_line,
            &[
                ("product_id", &format!("[{comp}]")),
                ("product_qty", "5"),
                ("qty_received", "5"),
                ("date_planned", &at(today, -10, "12:00:00")),
            ],
        ),
        rec(
            "stock.picking",
            picking,
            &[
                ("purchase_id", &format!("[{po_history}]")),
                ("scheduled_date", &at(today, -10, "12:00:00")),
                ("date_done", &at(today, -11, "10:00:00")), // a day early, so on time
                ("state", "done"),
            ],
        ),
        // The manufacturing order fulfilling the order (origin = the order's name).
        rec(
            "mrp.production",
            mo,
            &[
                ("name", &format!("MO{mo}")),
                ("product_id", &format!("[{finished}]")),
                ("bom_id", &format!("[{bom}]")),
                ("product_qty", "10"),
                ("state", "confirmed"),
                ("origin", &so_name),
                ("date_start", &at(today, LOT_START_DAYS, "12:00:00")),
                ("date_finished", &at(today, LOT_END_DAYS, "12:00:00")),
            ],
        ),
    ]
}

/// Materialize the supply-risk fixture into `repo` and derive `ekg:fulfills`.
async fn seed_supply_world(
    graphdb_url: &str,
    repo: &str,
    ids: SupplyIds,
    today: Day,
) -> Arc<GraphDbWorldModel> {
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    for record in supply_risk_fixtures(ids, today) {
        world.materialize(&record).await.expect("materialize");
    }
    GraphDbClient::new(graphdb_url, repo)
        .expect("derive client")
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive ekg:fulfills");
    world
}

/// The supply-risk stub cognition behind the real supply-risk agent: it answers in the
/// grounded `ACTION/TARGET/PARAM` block the parser requires. Its first round re-promises to a
/// date still before the inbound (the shortfall stays: rejected); once the prompt carries the
/// earlier round's outcome (the revise feedback), it re-promises past the inbound (clears, but
/// late: admitted); once that re-promise is in the feedback too, it expedites the cause line to
/// before the promise (clears on time: recommended).
struct RePromisingCognition {
    /// The at-risk order (the re-promise target).
    subject: String,
    /// The first, still-short re-promise.
    early: String,
    /// The revised, clearing re-promise.
    late: String,
    /// The cause line (the expedite target).
    line: String,
    /// The expedite's new arrival, before the promise.
    expedite: String,
}

#[async_trait]
impl CognitionProvider for RePromisingCognition {
    async fn generate(&self, prompt: &str) -> Result<String> {
        let tried_late =
            prompt.contains(&self.late) || prompt.contains(&self.late.replace('T', " "));
        let (verb, target, date) = if !prompt.contains("Already tried") {
            ("re_promise_date", &self.subject, &self.early)
        } else if tried_late {
            ("expedite_po", &self.line, &self.expedite)
        } else {
            ("re_promise_date", &self.subject, &self.late)
        };
        Ok(format!(
            "The battery inbound lands after the promise.\n\
             ACTION: {verb}\nTARGET: {target}\nPARAM: new_date={date}"
        ))
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }
}

/// A stand-in for the Restate admin `/query` that forwards to the real one and keeps only
/// `subject`'s rows, so the headline metrics are read over this test's point only, through
/// the resolver's own records path, however many points other suites (or the stack) left open.
async fn spawn_scoped_admin(http: reqwest::Client, admin_url: String, subject: String) -> String {
    let route = move |body: String| {
        let (http, admin_url, subject) = (http.clone(), admin_url.clone(), subject.clone());
        async move {
            let text = http
                .post(format!("{}/query", admin_url.trim_end_matches('/')))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(body)
                .send()
                .await
                .expect("restate admin /query")
                .text()
                .await
                .expect("admin body");
            let mut value: Value = serde_json::from_str(&text).expect("admin json");
            if let Some(rows) = value.get_mut("rows").and_then(Value::as_array_mut) {
                rows.retain(|row| {
                    row.get("service_key").and_then(Value::as_str) == Some(subject.as_str())
                });
            }
            axum::Json(value)
        }
    };
    let app = axum::Router::new().route("/query", axum::routing::post(route));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the scoped admin");
    let addr = listener.local_addr().expect("scoped admin addr");
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// One tab's rows for `subject`, each as a map from key to value.
async fn tab_rows(app: &axum::Router, tab: &str, subject: &str) -> Vec<BTreeMap<String, String>> {
    let query = format!(
        "{{ readModel(subject: \"{subject}\", tab: {tab}) {{ rows {{ fields {{ key value }} }} }} }}"
    );
    let resp = graphql_post(app, &query).await;
    resp["data"]["readModel"]["rows"]
        .as_array()
        .expect("tab rows")
        .iter()
        .map(|row| {
            row["fields"]
                .as_array()
                .expect("fields")
                .iter()
                .filter_map(|f| {
                    Some((
                        f["key"].as_str()?.to_owned(),
                        f["value"].as_str()?.to_owned(),
                    ))
                })
                .collect::<BTreeMap<String, String>>()
        })
        .collect()
}

fn get<'r>(row: &'r BTreeMap<String, String>, key: &str) -> Option<&'r str> {
    row.get(key).map(String::as_str)
}

/// The single row whose `key` names `entity`.
fn one<'r>(
    rows: &'r [BTreeMap<String, String>],
    key: &str,
    entity: &str,
) -> &'r BTreeMap<String, String> {
    let found: Vec<_> = rows
        .iter()
        .filter(|r| get(r, key) == Some(entity))
        .collect();
    assert_eq!(found.len(), 1, "exactly one row for {entity}: {found:?}");
    found[0]
}

/// The Graph's node rows.
fn graph_nodes(graph: &[BTreeMap<String, String>]) -> Vec<&BTreeMap<String, String>> {
    graph
        .iter()
        .filter(|r| get(r, "kind") == Some("node"))
        .collect()
}

/// A supply-risk plan serves the display framing (a projected completion date, day-slip,
/// severity, order value, each change's from and to values) through the same neutral read
/// fields the process plan uses (`graphql_reads_a_plan_and_a_tab_and_commits_nothing` above
/// selects the identical shape), differing only in the opaque bag's values. The
/// revenue-at-risk and late-orders KPIs come back as a neutral, capability-filled list. Then
/// the four tabs serve the plant-wide structure with the point's chain lit and its causes
/// marked. All reads; nothing is committed.
///
/// The slate is produced by the live OpenRouter agent when `MODEL_API_KEY` is set, and by a
/// deterministic stub otherwise, so the test still runs, and pins exact values, without a
/// live LLM.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn graphql_serves_supply_risk_display_framing_over_the_neutral_surface() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    // One throwaway repo (and its projections) at a time: GraphDB Free's repository cap.
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(11);
    let callback_url = common::callback_url(11);
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
    match provision_repo(&http, &graphdb_url, SUPPLY_REPO).await {
        Ok(true) => {}
        Ok(false) => {
            common::infra_unavailable(&format!("GraphDB unreachable at {graphdb_url}"));
            return;
        }
        Err(why) => {
            common::infra_unavailable(&format!("GraphDB not ready: {why}"));
            return;
        }
    }

    // One relative base for every date: the fixture, the stub's re-promises, its grounding
    // edge and every pinned date assertion below.
    let today = Day::today();
    let commitment = at(today, COMMITMENT_DAYS, "00:00:00");
    let ids = SupplyIds::new(common::unique_base());
    let world = seed_supply_world(&graphdb_url, SUPPLY_REPO, ids, today).await;
    let query_client = GraphDbClient::new(&graphdb_url, SUPPLY_REPO).expect("query client");

    // The order carries its materialized total (as ekg:order_value) up front.
    let subject = EntityUri(ontology::entity_iri("sale.order", ids.so));
    let order_value_rows = query_client
        .select(&format!(
            "PREFIX ekg: <{}>\nSELECT ?v WHERE {{ <{}> ekg:order_value ?v }}",
            ontology::VOCAB_NAMESPACE,
            subject.0
        ))
        .await
        .expect("order_value select");
    let materialized_value: f64 = order_value_rows
        .first()
        .and_then(|r| r.get("v"))
        .and_then(|v| v.parse().ok())
        .expect("the order total materialized as ekg:order_value");
    assert!(
        (materialized_value - 12500.0).abs() < 1.0,
        "amount_total materialized as ekg:order_value"
    );

    // Serve and register the action-point object.
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
                reason: "battery shortfall before the promise".to_owned(),
                revision: 1,
            })
            .expect("encode"),
        )
        .send()
        .await
        .expect("open");
    assert!(open.status().is_success(), "open: {}", open.status());

    // Attach a supply-risk slate over the GraphDB simulator and SupplyRiskScorer. Candidate
    // generation uses the live OpenRouter agent when `MODEL_API_KEY` is set, and a
    // deterministic stub otherwise, so the test still runs (and asserts exact values)
    // without a live LLM. Either way the loop only proposes; it commits nothing.
    let simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, SUPPLY_REPO).expect("sim client"),
    )) as Arc<dyn Simulator>;
    let live = std::env::var("MODEL_API_KEY").is_ok_and(|key| !key.is_empty());
    let late_repromise = at(today, LATE_REPROMISE_DAYS, "00:00:00");
    let slate_agent: Arc<dyn SlateProposer> = if live {
        eprintln!("using the live OpenRouter agent (MODEL_API_KEY set)");
        let cognition = Arc::new(
            OpenAiCompatCognition::new(CognitionConfig {
                // This in-process agent calls the provider itself (it holds the key); the
                // containerized backend under test goes through the runtime.
                route: ekg_cognition::CognitionRoute::Direct,
                base_url: env_or("MODEL_BASE_URL", "https://openrouter.ai/api/v1"),
                api_key: std::env::var("MODEL_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty()),
                chat_model: env_or("CHAT_MODEL", "openai/gpt-oss-120b"),
                embed_model: env_or("EMBED_MODEL", "qwen/qwen3-embedding-8b"),
                embed_dim: env_or("EMBED_DIM", "4096").parse().unwrap_or(4096),
                limits: ekg_cognition::CognitionLimits::default(),
            })
            .expect("cognition provider"),
        ) as Arc<dyn CognitionProvider>;
        Arc::new(ekg_backend::propose::supply_risk_slate_agent(
            Arc::clone(&world) as Arc<dyn WorldModel>,
            cognition,
            Arc::clone(&simulator),
            3,
        )) as Arc<dyn SlateProposer>
    } else {
        eprintln!("using the real supply-risk agent over a stub cognition (MODEL_API_KEY unset)");
        let cognition = Arc::new(RePromisingCognition {
            subject: subject.0.clone(),
            early: at(today, EARLY_REPROMISE_DAYS, "00:00:00"),
            late: late_repromise.clone(),
            line: SupplyIds::iri("purchase.order.line", ids.po_line),
            expedite: at(today, EXPEDITE_DAYS, "08:00:00"),
        }) as Arc<dyn CognitionProvider>;

        // The agent grounds the point on its cause chain: the late battery line, its
        // purchase order and supplier are evidence and legal targets.
        let grounded = ekg_backend::propose::supply_risk_agent(
            Arc::clone(&world) as Arc<dyn WorldModel>,
            Arc::clone(&cognition),
            3,
        )
        .propose(&subject)
        .await
        .expect("the supply-risk agent proposes over the grounded cause chain");
        let cause_line = SupplyIds::iri("purchase.order.line", ids.po_line);
        assert!(
            grounded
                .grounding_edges
                .iter()
                .any(|e| e.subject.0 == cause_line && e.predicate == "ekg:date_planned"),
            "the grounding carries the cause line's arrival: {:?}",
            grounded.grounding_edges
        );
        for member in [
            cause_line.clone(),
            SupplyIds::iri("purchase.order", ids.po),
            SupplyIds::iri("res.partner", ids.supplier),
        ] {
            assert!(
                grounded.subject_members.iter().any(|m| m.0 == member),
                "{member} is a legal target"
            );
        }
        let action = &grounded.proposal.actions[0];
        assert_eq!(action.action_type.0, "re_promise_date");
        assert_eq!(action.target, subject);
        assert_eq!(
            action.params.get("new_date").map(String::as_str),
            Some(
                at(today, EARLY_REPROMISE_DAYS, "00:00:00")
                    .replace('T', " ")
                    .as_str()
            ),
            "the grounded block's date is carried, canonical"
        );

        Arc::new(ekg_backend::propose::supply_risk_slate_agent(
            Arc::clone(&world) as Arc<dyn WorldModel>,
            cognition,
            Arc::clone(&simulator),
            3,
        )) as Arc<dyn SlateProposer>
    };
    let policy = PolicyEngine::new(
        "test-slate/v1",
        ["re_promise_date".to_owned(), "expedite_po".to_owned()]
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

    // The read-only GraphQL surface under test.
    let gql_simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, SUPPLY_REPO).expect("gql sim client"),
    )) as Arc<dyn Simulator>;
    let app = graphql::router(GraphqlState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        world: Arc::clone(&world) as Arc<dyn WorldModel>,
        simulator: gql_simulator,
        rules: ekg_backend::drift::monitor_rules(),
        policy: policy_display(Duration::from_secs(8 * 60 * 60)),
    });

    // The same neutral field selection the process-plan test uses: the display framing rides
    // the opaque `metrics` bag and each change's `from` / `label`, never a capability-named
    // field.
    let plan_query = format!(
        "{{ plan(subject: \"{}\") {{ \
           baseline {{ metrics {{ key value }} }} \
           options {{ recommended metrics {{ key value }} \
             changes {{ verb object label from params {{ key value }} }} }} }} }}",
        subject.0
    );
    let plan = graphql_post(&app, &plan_query).await;
    let options = plan["data"]["plan"]["options"].as_array().expect("options");
    let metrics_of = |score: &Value| -> BTreeMap<String, String> {
        score["metrics"]
            .as_array()
            .expect("metrics")
            .iter()
            .filter_map(|m| {
                Some((
                    m["key"].as_str()?.to_owned(),
                    m["value"].as_str()?.to_owned(),
                ))
            })
            .collect()
    };
    let metric = |metrics: &BTreeMap<String, String>, key: &str| metrics.get(key).cloned();

    // The do-nothing baseline serves its own framing. The order is fulfilled by one lot, so it
    // is judged lot by lot: the lot waits for the open receipt, then takes its planned window,
    // so it completes the lot's duration after that arrival.
    let lot_days = LOT_END_DAYS - LOT_START_DAYS;
    let baseline = metrics_of(&plan["data"]["plan"]["baseline"]);
    assert_eq!(
        metric(&baseline, "delivery").as_deref(),
        Some("late"),
        "the baseline's delivery: {baseline:?}"
    );
    assert_eq!(
        metric(&baseline, "day_slip").as_deref(),
        Some(
            (LATE_RECEIPT_DAYS + lot_days - COMMITMENT_DAYS)
                .to_string()
                .as_str()
        ),
        "the baseline's lot waits for the covering arrival: {baseline:?}"
    );
    assert!(
        metric(&baseline, "projected_completion")
            .is_some_and(|c| c.starts_with(&today.plus(LATE_RECEIPT_DAYS + lot_days).iso())),
        "the baseline's lot completes its window after the receipt lands: {baseline:?}"
    );
    assert_eq!(
        metric(&baseline, "order_value").as_deref(),
        Some("12500.00"),
        "{baseline:?}"
    );
    assert_eq!(
        metric(&baseline, "currency").as_deref(),
        Some("USD"),
        "{baseline:?}"
    );

    let recommended = options
        .iter()
        .find(|o| o["recommended"].as_bool() == Some(true));
    if let Some(recommended) = recommended {
        let metrics = metrics_of(recommended);

        // The display framing is served, all through opaque bag keys (no capability field).
        assert!(
            metrics.contains_key("severity"),
            "severity served: {metrics:?}"
        );
        assert!(
            metrics.contains_key("projected_completion"),
            "projected completion served: {metrics:?}"
        );
        assert!(
            metrics.contains_key("delivery"),
            "delivery served: {metrics:?}"
        );
        assert_eq!(
            metric(&metrics, "order_value").as_deref(),
            Some("12500.00"),
            "the order value is served, exact: {metrics:?}"
        );
        assert_eq!(
            metric(&metrics, "currency").as_deref(),
            Some("USD"),
            "the order value carries its materialized currency: {metrics:?}"
        );
        // Every option change carries `from` beside the new value (params).
        let change = recommended["changes"]
            .as_array()
            .and_then(|c| c.first())
            .expect("a change");
        assert!(
            change.get("from").is_some() && change.get("label").is_some(),
            "each change exposes its from value and label (queryable): {change}"
        );

        // The deterministic stub path pins the exact outcomes, labels and froms; the live
        // OpenRouter agent chooses its own actions, so only the served shape is asserted.
        if !live {
            let option_by_verb = |verb: &str| {
                options
                    .iter()
                    .find(|o| o["changes"][0]["verb"].as_str() == Some(verb))
                    .unwrap_or_else(|| panic!("an admitted {verb} option: {options:?}"))
            };
            let expedite = option_by_verb("expedite_po");
            let re_promise = option_by_verb("re_promise_date");
            assert_eq!(
                options.len(),
                2,
                "the early re-promise is rejected; the expedite and the late re-promise are \
                 admitted: {options:?}"
            );

            // The on-time expedite is recommended over the cheaper late re-promise.
            assert_eq!(
                expedite["recommended"].as_bool(),
                Some(true),
                "the on-time expedite is recommended: {options:?}"
            );
            let expedited = metrics_of(expedite);
            assert_eq!(
                metric(&expedited, "delivery").as_deref(),
                Some("on_time"),
                "{expedited:?}"
            );
            assert_eq!(
                metric(&expedited, "day_slip").as_deref(),
                Some((LOT_END_DAYS - COMMITMENT_DAYS).to_string().as_str()),
                "the expedited lot ships on its own schedule, before the promise: {expedited:?}"
            );
            assert!(
                metric(&expedited, "projected_completion")
                    .is_some_and(|c| c.starts_with(&today.plus(LOT_END_DAYS).iso())),
                "the expedited order ships with its lot: {expedited:?}"
            );
            let expedite_change = &expedite["changes"][0];
            assert_eq!(
                expedite_change["object"].as_str(),
                Some(SupplyIds::iri("purchase.order.line", ids.po_line).as_str())
            );
            assert_eq!(
                expedite_change["label"].as_str(),
                Some(format!("P{} · 20 × Battery", ids.po).as_str()),
                "the line is labelled by its PO, open quantity and product"
            );
            assert_eq!(
                expedite_change["from"].as_str(),
                Some(at(today, LATE_RECEIPT_DAYS, "00:00:00").as_str()),
                "the expedite's from is the line's current ARRIVAL (its open receipt's ETA), \
                 not its planned date"
            );

            // The late re-promise clears the signal but is served `late` by its days.
            assert_eq!(re_promise["recommended"].as_bool(), Some(false));
            let repromised = metrics_of(re_promise);
            assert_eq!(
                metric(&repromised, "delivery").as_deref(),
                Some("late"),
                "a re-promise that clears is late by the days it moves the promise: \
                 {repromised:?}"
            );
            assert_eq!(
                metric(&repromised, "day_slip").as_deref(),
                Some(
                    (LATE_RECEIPT_DAYS + lot_days - COMMITMENT_DAYS)
                        .to_string()
                        .as_str()
                ),
                "a re-promise leaves the lot waiting for the receipt: late against the \
                 original promise"
            );
            assert!(
                metric(&repromised, "projected_completion")
                    .is_some_and(|c| c.starts_with(&today.plus(LATE_RECEIPT_DAYS + lot_days).iso())),
                "the re-promised order completes with its lot: {repromised:?}"
            );
            let re_promise_change = &re_promise["changes"][0];
            assert_eq!(
                re_promise_change["label"].as_str(),
                Some(format!("SO{} · VeloMax Retail", ids.so).as_str()),
                "the order is labelled by its reference and customer"
            );
            assert_eq!(
                re_promise_change["from"].as_str(),
                Some(commitment.as_str()),
                "the re-promise's from is the current promise"
            );
        }
    } else {
        // The stub always yields a clearing recommended option; the live agent may abstain.
        assert!(
            live,
            "the deterministic stub path must produce a recommended clearing option"
        );
        eprintln!("live agent produced no recommended option; asserting KPIs + no-commit only");
    }

    // The headline KPIs come back as a neutral, capability-filled list. They summarise every
    // point awaiting oversight in the shared Restate, other suites' points and the stack's
    // own included, whose subjects this throwaway world does not hold (so their counts and
    // revenue are omitted here). So the KPIs are also read through a Restate admin `/query`
    // scoped to this test's point: the same resolver, the same records path.
    let kpi_map_of = |kpis: &Value| -> BTreeMap<String, String> {
        kpis["data"]["headlineMetrics"]
            .as_array()
            .expect("headlineMetrics list")
            .iter()
            .filter_map(|m| {
                Some((
                    m["key"].as_str()?.to_owned(),
                    m["value"].as_str()?.to_owned(),
                ))
            })
            .collect()
    };
    let shared = kpi_map_of(&graphql_post(&app, "{ headlineMetrics { key value } }").await);
    if let Some(revenue) = shared.get("revenue_at_risk") {
        assert!(
            revenue.parse::<f64>().is_ok() && shared.contains_key("currency"),
            "a served revenue is numeric and carries its currency: {shared:?}"
        );
    }
    let scoped_admin = spawn_scoped_admin(http.clone(), admin_url.clone(), subject.0.clone()).await;
    let scoped = graphql::router(GraphqlState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: scoped_admin,
        world: Arc::clone(&world) as Arc<dyn WorldModel>,
        simulator: Arc::new(GraphDbSimulator::new(
            GraphDbClient::new(&graphdb_url, SUPPLY_REPO).expect("scoped sim client"),
        )) as Arc<dyn Simulator>,
        rules: ekg_backend::drift::monitor_rules(),
        policy: policy_display(Duration::from_secs(8 * 60 * 60)),
    });
    let kpi_map = kpi_map_of(&graphql_post(&scoped, "{ headlineMetrics { key value } }").await);
    assert_eq!(
        kpi_map.get("late_orders_now").map(String::as_str),
        Some("1"),
        "the order is late if nothing is done: {kpi_map:?}"
    );
    assert_eq!(
        kpi_map.get("revenue_at_risk").map(String::as_str),
        Some("12500.00"),
        "revenue at risk is the order's exact value: {kpi_map:?}"
    );
    assert_eq!(
        kpi_map.get("currency").map(String::as_str),
        Some("USD"),
        "revenue at risk carries the orders' materialized currency: {kpi_map:?}"
    );
    let late_if_approved: i64 = kpi_map
        .get("late_orders")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("the late-if-approved KPI is a served count: {kpi_map:?}"));
    if !live {
        assert_eq!(
            late_if_approved, 0,
            "the recommended on-time expedite leaves no order late: {kpi_map:?}"
        );
    }

    // --- the four read-model tabs over the real supply-risk point ---
    // Plant-wide structure, the point's chain lit (`on_path`), its causes marked (`cause`),
    // the rules' current verdict on every order (`at_risk`). All reads; nothing is written.
    let iri = SupplyIds::iri;
    let so = subject.0.clone();
    let customer = iri("res.partner", ids.customer);
    let product = iri("product.product", ids.finished);
    let bom = iri("mrp.bom", ids.bom);
    let battery = iri("product.product", ids.comp);
    let frame = iri("product.product", ids.frame);
    let celltech = iri("res.partner", ids.supplier);
    let frameco = iri("res.partner", ids.frameco);
    let late_po = iri("purchase.order", ids.po);
    let received_po = iri("purchase.order", ids.po_history);
    let mo = iri("mrp.production", ids.mo);

    // Graph: all six columns, the on-path chain, the causes.
    let graph = tab_rows(&app, "GRAPH", &so).await;
    let nodes = graph_nodes(&graph);
    let roles: BTreeSet<&str> = nodes.iter().filter_map(|r| get(r, "role")).collect();
    assert_eq!(
        roles,
        BTreeSet::from([
            "Customer",
            "SalesOrder",
            "Product",
            "BOM",
            "Component",
            "Supplier",
            "PurchaseOrder"
        ]),
        "the Graph spans all six columns: {graph:?}"
    );
    assert_eq!(
        get(one(&graph, "node", &battery), "role"),
        Some("Component")
    );
    assert_eq!(get(one(&graph, "node", &product), "role"), Some("Product"));
    let on_path: BTreeSet<&str> = nodes
        .iter()
        .filter(|r| get(r, "on_path") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(
        on_path,
        BTreeSet::from([
            customer.as_str(),
            so.as_str(),
            product.as_str(),
            bom.as_str(),
            battery.as_str(),
            celltech.as_str(),
            late_po.as_str(),
        ]),
        "the full chain: customer, order, product, BOM, short component, supplier, late PO"
    );
    assert_eq!(
        get(one(&graph, "node", &so), "at_risk"),
        Some("true"),
        "the at-risk order is marked at_risk"
    );
    let causes: BTreeSet<&str> = nodes
        .iter()
        .filter(|r| get(r, "cause") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(
        causes,
        BTreeSet::from([battery.as_str(), late_po.as_str()]),
        "the short component and the late PO are the causes"
    );
    assert_eq!(get(one(&graph, "node", &frame), "cause"), Some("false"));
    assert_eq!(get(one(&graph, "node", &frame), "on_path"), Some("false"));
    assert!(
        !graph
            .iter()
            .any(|r| get(r, "node") == Some(received_po.as_str())),
        "the fully received, completed historical PO is never drawn: {graph:?}"
    );
    let lit_edges: Vec<(&str, &str)> = graph
        .iter()
        .filter(|r| get(r, "kind") == Some("edge") && get(r, "on_path") == Some("true"))
        .filter_map(|r| Some((get(r, "s")?, get(r, "o")?)))
        .collect();
    for edge in [
        (so.as_str(), customer.as_str()),
        (bom.as_str(), battery.as_str()),
        (celltech.as_str(), battery.as_str()),
        (late_po.as_str(), celltech.as_str()),
    ] {
        assert!(lit_edges.contains(&edge), "{edge:?} lit in {lit_edges:?}");
    }

    // Flow: all five stages, exactly one on-path entity each.
    let flow = tab_rows(&app, "FLOW", &so).await;
    let mut lit_per_stage: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for row in &flow {
        let stage = get(row, "stage").expect("stage");
        let entry = lit_per_stage.entry(stage).or_default();
        if get(row, "on_path") == Some("true") {
            entry.insert(get(row, "entity").expect("entity"));
        }
    }
    assert_eq!(
        lit_per_stage,
        BTreeMap::from([
            ("suppliers", BTreeSet::from([celltech.as_str()])),
            ("on_order", BTreeSet::from([late_po.as_str()])),
            ("in_stock", BTreeSet::from([battery.as_str()])),
            ("in_production", BTreeSet::from([mo.as_str()])),
            ("promised", BTreeSet::from([so.as_str()])),
        ]),
        "Flow reaches all five stages: {flow:?}"
    );
    assert_eq!(get(one(&flow, "entity", &late_po), "cause"), Some("true"));
    assert!(
        !flow
            .iter()
            .any(|r| get(r, "entity") == Some(received_po.as_str())),
        "a fully received, completed PO is never on order"
    );

    // Suppliers: sourcing, OTD, cause-scoped exposure, stock with demand, dependent orders.
    let suppliers = tab_rows(&app, "SUPPLIERS", &so).await;
    let table = |supplier: &str| {
        suppliers
            .iter()
            .find(|r| get(r, "kind") == Some("supplier") && get(r, "supplier") == Some(supplier))
            .unwrap_or_else(|| panic!("supplier row for {supplier}: {suppliers:?}"))
    };
    assert_eq!(get(table(&celltech), "sourcing"), Some("single"));
    assert_eq!(get(table(&frameco), "sourcing"), Some("single"));
    assert_eq!(
        get(table(&celltech), "lead_time"),
        Some("14"),
        "lead time served"
    );
    assert_eq!(
        get(table(&celltech), "otd"),
        Some("1.0000"),
        "the one in-window receipt completed before its planned day, so OTD 1.0"
    );
    assert_eq!(get(table(&celltech), "on_path"), Some("true"));
    assert_eq!(get(table(&frameco), "on_path"), Some("false"));
    let exposure = |supplier: &str| -> f64 {
        get(table(supplier), "exposure")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("exposure served for {supplier}"))
    };
    assert!(
        (exposure(&celltech) - 12500.0).abs() < 0.01,
        "the cause supplier's exposure is the at-risk order value"
    );
    assert!(
        exposure(&frameco).abs() < 0.01,
        "a supplier of no cause component carries no exposure"
    );
    let open_pos: BTreeSet<&str> = suppliers
        .iter()
        .filter(|r| get(r, "kind") == Some("open_po"))
        .filter_map(|r| get(r, "po"))
        .collect();
    assert_eq!(
        open_pos,
        BTreeSet::from([late_po.as_str()]),
        "only the late PO is open; the received historical PO never is"
    );
    let battery_stock = suppliers
        .iter()
        .find(|r| {
            get(r, "kind") == Some("stock")
                && get(r, "supplier") == Some(celltech.as_str())
                && get(r, "stock_of") == Some(battery.as_str())
        })
        .expect("CellTech's battery stock row");
    assert_eq!(
        get(battery_stock, "demand").and_then(|v| v.parse::<f64>().ok()),
        Some(10.0),
        "the 30-day demand counts the order promised at today+{COMMITMENT_DAYS}"
    );
    let dependents: BTreeMap<&str, &str> = suppliers
        .iter()
        .filter(|r| {
            get(r, "kind") == Some("dependent_order")
                && get(r, "supplier") == Some(celltech.as_str())
        })
        .filter_map(|r| Some((get(r, "order")?, get(r, "at_risk")?)))
        .collect();
    assert_eq!(
        dependents,
        BTreeMap::from([(so.as_str(), "true")]),
        "the dependent order, marked at_risk"
    );

    // Timeline: the order's promise (on path) and only the open inbound arrival.
    let timeline = tab_rows(&app, "TIMELINE", &so).await;
    let promise = timeline
        .iter()
        .find(|r| get(r, "kind") == Some("promise") && get(r, "entity") == Some(so.as_str()))
        .expect("the order's promise");
    assert_eq!(get(promise, "on_path"), Some("true"));
    let inbound: BTreeSet<&str> = timeline
        .iter()
        .filter(|r| get(r, "kind") == Some("inbound"))
        .filter_map(|r| get(r, "entity"))
        .collect();
    assert_eq!(
        inbound,
        BTreeSet::from([late_po.as_str()]),
        "only the open (late) inbound is an arrival"
    );

    // --- the same tabs when the rule set cannot be evaluated ---
    // The monitor's rules plus one whose SELECT the store refuses, so the set is unevaluable.
    // Over the same world (no extra repository: GraphDB Free caps them).
    let mut broken_rules: Vec<RiskRule> = ekg_backend::drift::monitor_rules().to_vec();
    broken_rules.push(RiskRule::new("THIS IS NOT SPARQL {", |_| None));
    let unevaluable = graphql::router(GraphqlState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        world: Arc::clone(&world) as Arc<dyn WorldModel>,
        simulator: Arc::new(GraphDbSimulator::new(
            GraphDbClient::new(&graphdb_url, SUPPLY_REPO).expect("unevaluable sim client"),
        )) as Arc<dyn Simulator>,
        rules: Arc::from(broken_rules),
        policy: policy_display(Duration::from_secs(8 * 60 * 60)),
    });
    assert_risk_markers_omitted(&unevaluable, &so).await;

    // Reads commit nothing: the point is still open (unchanged by serving the framing/tabs).
    let point = http
        .post(ap_url(&ingress_url, &subject.0, "get"))
        .send()
        .await
        .expect("get point");
    let point: Value =
        serde_json::from_str(&point.text().await.expect("body")).expect("decode point");
    assert_eq!(
        point["state"].as_str(),
        Some("open"),
        "serving the display framing must not transition the point"
    );

    // cleanup
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{SUPPLY_REPO}"))
        .send()
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;
}

/// `app` serves the tabs over a rule set that cannot be evaluated. Each tab is still served,
/// with every risk marker omitted, never denied. No row carries `at_risk`, `cause` or
/// `exposure`, the cause-dependent hops (component / supplier / PO; Flow's suppliers /
/// on_order / in_stock; Timeline's inbound) carry no `on_path`, and the rule-independent hops
/// (customer / order / product / BOM; Flow's in_production / promised; the promise and
/// dependent orders) keep it.
async fn assert_risk_markers_omitted(app: &axum::Router, so: &str) {
    let no_risk_markers = |tab: &str, rows: &[BTreeMap<String, String>]| {
        assert!(!rows.is_empty(), "{tab} is still served");
        for row in rows {
            for key in ["at_risk", "cause", "exposure"] {
                assert!(
                    !row.contains_key(key),
                    "{tab}: `{key}` is omitted, never denied: {row:?}"
                );
            }
        }
    };

    let graph = tab_rows(app, "GRAPH", so).await;
    no_risk_markers("GRAPH", &graph);
    let mut rule_independent_seen = BTreeSet::new();
    for node in graph_nodes(&graph) {
        let role = get(node, "role").expect("role");
        match role {
            "Customer" | "SalesOrder" | "Product" | "BOM" => {
                assert!(
                    node.contains_key("on_path"),
                    "a rule-independent hop keeps on_path: {node:?}"
                );
                if get(node, "on_path") == Some("true") {
                    rule_independent_seen.insert(role);
                }
            }
            "Component" | "Supplier" | "PurchaseOrder" => assert!(
                !node.contains_key("on_path"),
                "a cause-dependent hop's path is unknown, so on_path omitted: {node:?}"
            ),
            other => panic!("unexpected role {other}"),
        }
    }
    assert_eq!(
        rule_independent_seen,
        BTreeSet::from(["Customer", "SalesOrder", "Product", "BOM"]),
        "the rule-independent chain is still lit"
    );

    let flow = tab_rows(app, "FLOW", so).await;
    no_risk_markers("FLOW", &flow);
    let mut stages = BTreeSet::new();
    for row in &flow {
        let stage = get(row, "stage").expect("stage");
        stages.insert(stage);
        match stage {
            "in_production" | "promised" => assert!(
                row.contains_key("on_path"),
                "a rule-independent stage keeps on_path: {row:?}"
            ),
            _ => assert!(
                !row.contains_key("on_path"),
                "a cause-dependent stage omits on_path: {row:?}"
            ),
        }
    }
    assert_eq!(
        stages.len(),
        5,
        "all five stages are still served: {stages:?}"
    );

    let suppliers = tab_rows(app, "SUPPLIERS", so).await;
    no_risk_markers("SUPPLIERS", &suppliers);
    let mut kinds = BTreeSet::new();
    for row in &suppliers {
        let kind = get(row, "kind").expect("kind");
        kinds.insert(kind);
        match kind {
            "supplier" | "open_po" | "stock" => assert!(
                !row.contains_key("on_path"),
                "a supplier / PO / component path is unknown, so on_path omitted: {row:?}"
            ),
            "dependent_order" => assert!(
                row.contains_key("on_path"),
                "an order is a rule-independent hop and keeps on_path: {row:?}"
            ),
            _ => {}
        }
    }
    assert!(
        ["supplier", "open_po", "stock", "dependent_order"]
            .iter()
            .all(|k| kinds.contains(k)),
        "every Suppliers row kind is still served: {kinds:?}"
    );

    let timeline = tab_rows(app, "TIMELINE", so).await;
    no_risk_markers("TIMELINE", &timeline);
    for row in &timeline {
        match get(row, "kind") {
            Some("inbound") => assert!(
                !row.contains_key("on_path"),
                "an inbound PO's path is unknown, so on_path omitted: {row:?}"
            ),
            Some("promise") => assert!(
                row.contains_key("on_path"),
                "a promise keeps on_path: {row:?}"
            ),
            _ => {}
        }
    }
}
