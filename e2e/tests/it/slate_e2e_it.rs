//! Gated live e2e: an open contention action point receives a ranked remediation
//! slate whose recommended option clears the drift signal under projection while the
//! do-nothing baseline does not, and nothing is committed (no write-back, the world
//! model unchanged).
//!
//! The [`SlateAgent`] loop drives the GraphDB [`Simulator`] (an ephemeral what-if
//! projection per candidate) and the [`ProcessContentionScorer`] (the capability's
//! own contention rule over each projection), then the bridge's
//! `propose_and_attach_slate` durably attaches the slate via the `attach_slate`
//! handler. The test reads it back from the `slate` handler.
//!
//! Candidate generation is a deterministic stub proposer, as in the propose-stage
//! e2e, so the test does not depend on a live LLM or on grounding. The real
//! `process_slate_agent` composes the bounded agent instead; that path is exercised
//! manually (`just backend`).
//!
//! Ignored by default. Requires GraphDB and Restate with the worker endpoint reachable:
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it slate_e2e_it -- --ignored --nocapture
//! ```
//!
//! Skips (never fails) if the substrate is unreachable.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
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
    GraphDbClient, GraphDbSimulator, GraphDbWorldModel, ProcessContentionScorer, ontology,
};
use restate_sdk::prelude::Endpoint;
use serde_json::json;

use common::{provision_repo, rec};

const REPO: &str = "ekg_slate_e2e_it";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// A contended process fixture (the same shape as the simulator e2e): work centre
/// `wc` (capacity 1) performs two work orders whose 08:00–12:00 and 09:00–13:00
/// windows overlap, so the contention rule fires. `base` offsets every id so the
/// subject MO IRI (and thus the persistent `ActionPointService` key) is unique per run.
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

/// A deterministic two-step candidate generator that also exercises the reasoning
/// trace. Its first candidate reschedules work order `wo2` to 09:00, still inside work
/// order 1's 08:00–12:00 window, so contention persists and the candidate is rejected
/// (it does not beat doing nothing). Given that score as feedback, it revises to 12:00,
/// clear of the window, so the contention clears under projection and that candidate
/// is admitted. The attached slate therefore carries a retained rejected candidate
/// alongside the clearing one. Stands in for the bounded agent so the test needs
/// neither an LLM nor grounding.
struct RevisingProposer {
    target: EntityUri,
}

#[async_trait]
impl CandidateProposer for RevisingProposer {
    // The subject is the MO; the remediation targets its work order (`self.target`).
    async fn propose_candidate(
        &self,
        _subject: &EntityUri,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        // First attempt: a reschedule that does not clear the overlap (rejected). After
        // observing it, revise to the clearing 12:00 reschedule (admitted).
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
            // One evidence edge so the reasoning trace emits a Graph step.
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
async fn an_open_contention_point_receives_a_clearing_slate_and_commits_nothing() {
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(7);
    let callback_url = common::callback_url(7);
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

    // Materialize the contended fixture, then derive ekg:capacity onto the work-centre
    // agent (the contention rule reads it). Ids are offset per run for a unique subject.
    let base = common::unique_base();
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    for record in fixtures(base) {
        world.materialize(&record).await.expect("materialize");
    }
    let client = GraphDbClient::new(&graphdb_url, REPO).expect("query client");
    client
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive capacity");

    let contention = ontology::PROCESS_CONTENTION_SELECT_SPARQL;
    // Sanity: the base is contended (the point is opened for real drift).
    let base_rows = client.select(contention).await.expect("base contention");
    assert!(!base_rows.is_empty(), "the base fixture must be contended");

    // The point's subject: the manufacturing order that work order 2 is part of.
    let subject = EntityUri(ontology::entity_iri("mrp.production", base + 2));
    let target_wo = EntityUri(ontology::entity_iri("mrp.workorder", base + 2));

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

    // Build the slate agent: the GraphDB simulator and the contention scorer, with
    // stub candidate generation. Then run the bridge's durable attach.
    let simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("sim client"),
    )) as Arc<dyn Simulator>;
    let scorer = Arc::new(ProcessContentionScorer::new()) as Arc<dyn Scorer>;
    let proposer = Arc::new(RevisingProposer { target: target_wo }) as Arc<dyn CandidateProposer>;
    let slate_agent =
        Arc::new(SlateAgent::new(proposer, simulator, scorer)) as Arc<dyn SlateProposer>;

    // A policy allowlisting the process verbs so the clearing reschedule option carries no
    // needs (it touches no purchase order, so cost/supplier predicates pass vacuously). The
    // real world model backs the (empty, for this process plan) policy-context gather.
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

    // The recommended option clears the signal under projection; the baseline does not.
    let recommended = slate.recommended().expect("a recommended option");
    assert_eq!(
        recommended
            .score
            .metrics
            .get("signal_cleared")
            .map(String::as_str),
        Some("true"),
        "the recommended plan must clear the contention under projection"
    );
    assert_eq!(
        slate
            .baseline
            .metrics
            .get("signal_cleared")
            .map(String::as_str),
        Some("false"),
        "doing nothing must not clear the contention"
    );
    assert!(
        recommended.score.rank_key < slate.baseline.rank_key,
        "the recommended option must outrank the do-nothing baseline"
    );

    // The slate carries a structured reasoning trace for oversight (Art. 14): a
    // non-empty ordered trace that opens with the do-nothing baseline and retains the
    // rejected (still-contended) candidate the loop tried and set aside.
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

    // Nothing committed: the world model is unchanged. The base still fires (the
    // slate is a read of hypotheticals; the slate path touches no ActionExecutor), and
    // the point is still `open` (a slate is not a promotion).
    let base_after = client.select(contention).await.expect("base after");
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
