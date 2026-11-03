//! Gated live end-to-end test: the bridge plans a formed portfolio with one joint slate and
//! its members with none of their own.
//!
//! Ignored by default. Requires GraphDB and Restate with the test endpoint reachable from the
//! runtime (Odoo is not needed: typed fixtures are materialized into a throwaway repo, and the
//! model is scripted):
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it portfolio_slate_e2e_it -- --ignored --nocapture
//! ```
//!
//! The drift monitor (a fixture risk rule flagging every entity whose `ekg:name` carries the
//! run's tag, and the capability's real coupling rule), the action-point and portfolio objects,
//! and the real bridge are bound. The point proposer and the group slate proposer are scripted:
//!
//! 1. a sales order and the manufacturing order fulfilling it form a portfolio: its record holds
//!    the joint slate for its generation (a `Linked` first step, a `slate_attached` audit
//!    entry); both members are `proposed` and carry no per-point slate;
//! 2. a second manufacturing order regroups them into a trio whose joint slate is held back
//!    until the trio is superseded (a member aborted): the late slate is refused, the trio keeps
//!    no slate, and no member falls back;
//! 3. a redelivery re-forms the first pair (generation 2) and the group proposer fails: its
//!    members get their own per-point slates.
//!
//! If any hop is unreachable the test skips, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as
//! `just test-e2e` does), when it fails.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ekg_backend::drift;
use ekg_domain::fakes::{CannedGroupSlateProposer, CannedSlateProposer, RecordingActionExecutor};
use ekg_domain::{
    ActionType, ChangeKind, EngineError, EntityUri, GroundedProposal, GroupSlateProposer,
    NamedGraph, Proposal, ProposedAction, Proposer, RdfChanged, Result, Revision, Slate, SourceRef,
    SubjectGroup, WorldModel,
};
use ekg_dynamic::{
    ActionPointService, DriftMonitorService, DriftVerdict, PortfolioService, RiskRule, portfolio_id,
};
use ekg_kinetic::PolicyEngine;
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, broadcast};

const REPO: &str = "ekg_portfolio_slate_e2e_it";

/// A fixture risk rule: flag every entity whose `ekg:name` starts with `tag`.
fn tagged_rule(tag: &str) -> RiskRule {
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\nSELECT ?entity WHERE {{ ?entity ekg:name ?n . \
         FILTER(STRSTARTS(STR(?n), \"{tag}\")) }}",
        vocab = ontology::VOCAB_NAMESPACE
    );
    RiskRule::new(sparql, |row| {
        Some(DriftVerdict {
            subject: EntityUri(row.get("entity")?.clone()),
            reason: "fixture flag".to_owned(),
        })
    })
}

fn changed(model: &str, id: i64, revision: u64) -> RdfChanged {
    RdfChanged {
        entity_uri: EntityUri(ontology::entity_iri(model, id)),
        named_graph: NamedGraph(ontology::named_graph(model, id)),
        change_kind: ChangeKind::Updated,
        source: SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        },
        revision: Revision(revision),
    }
}

/// A point proposer: a well-formed notify on the subject, so every point moves to proposed.
struct NotifyProposer;

#[async_trait]
impl Proposer for NotifyProposer {
    async fn propose(&self, subject: &EntityUri) -> Result<GroundedProposal> {
        Ok(GroundedProposal {
            proposal: Proposal::single(
                "[scripted] tell the owner".to_owned(),
                ProposedAction {
                    action_type: ActionType("notify_owner".to_owned()),
                    target: subject.clone(),
                    params: BTreeMap::from([("message".to_owned(), "heads up".to_owned())]),
                },
                Vec::new(),
            ),
            grounding_omissions: Vec::new(),
            grounding_edges: Vec::new(),
            subject_members: Vec::new(),
        })
    }
}

/// The scripted group slate proposer: the canned joint slate, except that the `held` group's
/// proposal waits for the gate, and the `failing` group's second proposal fails.
struct Scripted {
    held: String,
    failing: String,
    gate: Semaphore,
    calls: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
}

#[async_trait]
impl GroupSlateProposer for Scripted {
    async fn propose_group_slate(&self, group: &SubjectGroup) -> Result<Slate> {
        let seen = {
            let mut calls = self.calls.lock().expect("lock");
            calls.push(group.key.clone());
            calls.iter().filter(|k| **k == group.key).count()
        };
        if group.key == self.held {
            let _permit = self.gate.acquire().await.expect("gate");
        }
        let outcome = if group.key == self.failing && seen > 1 {
            Err(EngineError::Unavailable("scripted model outage".to_owned()))
        } else {
            CannedGroupSlateProposer.propose_group_slate(group).await
        };
        self.released.lock().expect("lock").push(group.key.clone());
        outcome
    }
}

async fn call(
    http: &reqwest::Client,
    ingress: &str,
    service: &str,
    key: &str,
    handler: &str,
    body: Option<Value>,
) -> std::result::Result<Value, String> {
    let mut url = reqwest::Url::parse(ingress).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|()| "base url".to_owned())?
        .pop_if_empty()
        .extend([service, key, handler]);
    let mut req = http.post(url).timeout(Duration::from_secs(30));
    if let Some(body) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
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
    call(http, ingress, service, key, "record", None)
        .await
        .unwrap_or(Value::Null)
}

async fn abort(
    http: &reqwest::Client,
    ingress: &str,
    subject: &str,
) -> std::result::Result<(), String> {
    let token = call(
        http,
        ingress,
        "ActionPointService",
        subject,
        "claim",
        Some(json!({
            "command": "abort",
            "reviewer": "planner-1",
            "expected_state": ["open", "proposed", "under_review"],
            "deadline_secs": 60
        })),
    )
    .await?;
    call(
        http,
        ingress,
        "ActionPointService",
        subject,
        "abort",
        Some(json!({ "token": token, "reviewer": "planner-1", "reason": "not needed" })),
    )
    .await?;
    Ok(())
}

fn has_slate(point: &Value) -> bool {
    point.get("slate").is_some_and(|s| !s.is_null())
}

fn audit_events(portfolio: &Value) -> Vec<String> {
    portfolio["audit"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e["event"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn a_portfolio_is_planned_by_one_joint_slate_and_its_members_by_none_of_their_own() {
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(18);
    let callback_url = common::callback_url(18);
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
    let _slot = common::fixture_repo_slot().await.expect("fixture slot");
    match common::provision_repo(&http, &graphdb_url, REPO).await {
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

    let base = common::unique_base();
    let tag = format!("T{base}");
    let (so, mo_a, mo_b) = (base + 1, base + 2, base + 3);
    let so_name = format!("{tag}-S");
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let writer = GraphDbClient::new(&graphdb_url, REPO).expect("client");
    let materialize = async |record: ekg_domain::SourceRecord| {
        world.materialize(&record).await.expect("materialize");
        for derivation in ontology::STRUCTURAL_DERIVATIONS {
            writer.update(derivation.sparql).await.expect("derive");
        }
    };
    let manufacturing = |id: i64, label: &str| {
        common::rec(
            "mrp.production",
            id,
            &[("name", &format!("{tag}-{label}")), ("origin", &so_name)],
        )
    };

    let monitor = DriftMonitorService::new(
        Arc::clone(&world) as Arc<dyn WorldModel>,
        vec![tagged_rule(&tag)],
    )
    .with_coupling(drift::coupling_rules());
    let endpoint = Endpoint::builder()
        .bind(monitor)
        .bind(ActionPointService::default())
        .bind(PortfolioService::default())
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let registered = common::register(&http, &admin_url, &callback_url).await;

    let so_iri = ontology::entity_iri("sale.order", so);
    let a_iri = ontology::entity_iri("mrp.production", mo_a);
    let b_iri = ontology::entity_iri("mrp.production", mo_b);
    let pair = portfolio_id(&[EntityUri(so_iri.clone()), EntityUri(a_iri.clone())]);
    let trio = portfolio_id(&[
        EntityUri(so_iri.clone()),
        EntityUri(a_iri.clone()),
        EntityUri(b_iri.clone()),
    ]);
    let scripted = Arc::new(Scripted {
        held: trio.0.clone(),
        failing: pair.0.clone(),
        gate: Semaphore::new(0),
        calls: Mutex::new(Vec::new()),
        released: Mutex::new(Vec::new()),
    });

    // The real bridge, over a broadcast channel.
    let (tx, rx) = broadcast::channel(16);
    let bridge = tokio::spawn(drift::run_monitor_bridge(
        rx,
        ingress_url.clone(),
        Duration::from_secs(60),
        Arc::new(NotifyProposer),
        PolicyEngine::new("p/v1", BTreeSet::new()),
        Arc::new(RecordingActionExecutor::default()),
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Some(drift::SlateStep {
            proposer: Arc::new(CannedSlateProposer),
            portfolio: Some(Arc::clone(&scripted) as Arc<dyn GroupSlateProposer>),
            guard: Duration::from_secs(60),
            shadow: None,
        }),
    ));

    let run = async || -> std::result::Result<BTreeMap<String, Value>, String> {
        registered.clone()?;
        let mut seen = BTreeMap::new();
        let point = async |iri: &str| record(&http, &ingress_url, "ActionPointService", iri).await;
        let portfolio = async |id: &str| record(&http, &ingress_url, "PortfolioService", id).await;

        // 1. The pair forms and gets its joint slate; both members are proposed.
        materialize(common::rec("sale.order", so, &[("name", &so_name)])).await;
        materialize(manufacturing(mo_a, "A")).await;
        tx.send(changed("mrp.production", mo_a, 1))
            .map_err(|e| e.to_string())?;
        common::wait_until(Duration::from_secs(60), async || {
            portfolio(&pair.0).await["slate_generation"] == 1
                && point(&so_iri).await["point"]["state"] == "proposed"
                && point(&a_iri).await["point"]["state"] == "proposed"
        })
        .await;
        seen.insert("pair@1".to_owned(), portfolio(&pair.0).await);
        seen.insert("so@1".to_owned(), point(&so_iri).await);
        seen.insert("a@1".to_owned(), point(&a_iri).await);

        // 2. The trio forms; its joint slate is held until the trio is superseded.
        materialize(manufacturing(mo_b, "B")).await;
        tx.send(changed("mrp.production", mo_b, 2))
            .map_err(|e| e.to_string())?;
        common::wait_until(Duration::from_secs(60), async || {
            scripted.calls.lock().expect("lock").contains(&trio.0)
        })
        .await;
        abort(&http, &ingress_url, &b_iri).await?;
        common::wait_until(Duration::from_secs(30), async || {
            portfolio(&trio.0).await["state"] == "superseded"
        })
        .await;
        scripted.gate.add_permits(1);
        common::wait_until(Duration::from_secs(30), async || {
            scripted.released.lock().expect("lock").contains(&trio.0)
        })
        .await;
        // Give the refused attach (and any wrong fallback) time to land.
        tokio::time::sleep(Duration::from_secs(3)).await;
        seen.insert("trio@2".to_owned(), portfolio(&trio.0).await);
        seen.insert("so@2".to_owned(), point(&so_iri).await);
        seen.insert("a@2".to_owned(), point(&a_iri).await);

        // 3. A redelivery re-forms the pair (generation 2); the group proposer fails, so the
        //    members fall back to their own slates.
        tx.send(changed("mrp.production", mo_b, 2))
            .map_err(|e| e.to_string())?;
        common::wait_until(Duration::from_secs(60), async || {
            has_slate(&point(&so_iri).await) && has_slate(&point(&a_iri).await)
        })
        .await;
        seen.insert("pair@3".to_owned(), portfolio(&pair.0).await);
        seen.insert("so@3".to_owned(), point(&so_iri).await);
        seen.insert("a@3".to_owned(), point(&a_iri).await);
        Ok(seen)
    };
    let outcome = run().await;
    bridge.abort();

    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    let deregistered = common::deregister_checked(&http, &admin_url, &callback_url).await;
    let seen = outcome.expect("the scenario ran");
    for (label, value) in &seen {
        println!("{label}: {value}");
    }
    deregistered.expect("the test deployment deregistered");

    // 1. One joint slate for generation 1, opening with Linked (the coupling worded from its
    //    facts: the manufacturing order builds the sales order), audited; members unslated.
    let first = &seen["pair@1"];
    assert_eq!(first["slate_generation"], 1, "{first}");
    let step = &first["slate"]["trace"]["steps"][0];
    assert_eq!(step["tag"], "graph", "{step}");
    assert!(
        step["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("Linked: ")
                && t.contains(&so_iri)
                && t.contains(&a_iri)
                && t.contains(&format!("{a_iri} builds {so_iri}."))),
        "{step}"
    );
    assert_eq!(
        audit_events(first).last().map(String::as_str),
        Some("slate_attached")
    );
    for label in ["so@1", "a@1"] {
        assert_eq!(seen[label]["point"]["state"], "proposed", "{label}");
        assert!(
            !has_slate(&seen[label]),
            "{label} carries no slate of its own"
        );
    }

    // 2. The late trio slate was refused: no slate, no attach audit, no fallback.
    let trio_after = &seen["trio@2"];
    assert_eq!(trio_after["state"], "superseded", "{trio_after}");
    assert!(
        trio_after.get("slate").is_none_or(Value::is_null),
        "{trio_after}"
    );
    assert!(!audit_events(trio_after).contains(&"slate_attached".to_owned()));
    for label in ["so@2", "a@2"] {
        assert!(
            !has_slate(&seen[label]),
            "{label}: a stale slate falls back to nothing"
        );
    }

    // 3. The failed joint slate for generation 2 gave the members their own slates.
    assert_eq!(seen["pair@3"]["generation"], 2, "{}", seen["pair@3"]);
    assert!(
        seen["pair@3"].get("slate").is_none_or(Value::is_null),
        "the re-formed pair has no current slate"
    );
    for label in ["so@3", "a@3"] {
        assert!(
            has_slate(&seen[label]),
            "{label} fell back to its own slate"
        );
    }
}
