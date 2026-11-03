//! Gated live e2e: the bounded agent grounds an open action point over real
//! GraphDB and advances it from `open` to `proposed` with a grounded proposal,
//! writing nothing back.
//!
//! Uses a stub cognition provider (a deterministic, grounded
//! `ACTION/TARGET/PARAM` re-promise of the order), so the test exercises the
//! durable propose path (real grounding, real Restate object lifecycle) without
//! depending on a live or compliant LLM. The real model over Hayhooks is
//! exercised manually (`just backend`).
//!
//! Ignored by default. Requires GraphDB and Restate with the worker endpoint
//! reachable from the runtime:
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it propose_stage_e2e_it -- --ignored --nocapture
//! ```
//!
//! Skips if the stack is unreachable.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ekg_backend::propose::supply_risk_agent;
use ekg_domain::{
    CognitionProvider, EntityUri, GroundedProposal, Proposer, SourceRecord, SourceRef, WorldModel,
};
use ekg_dynamic::{ActionPointService, OpenRequest};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};
use restate_sdk::prelude::Endpoint;
use serde_json::json;

const REPO: &str = "ekg_propose_stage_e2e_it";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// A deterministic cognition stub: re-promises the grounded order (its subject) to a
/// concrete date, in the `ACTION/TARGET/PARAM` block the supply-risk parser requires.
struct StubCognition {
    subject: EntityUri,
}
#[async_trait]
impl CognitionProvider for StubCognition {
    async fn generate(&self, prompt: &str) -> ekg_domain::Result<String> {
        assert!(
            prompt.contains(&self.subject.0),
            "the order is in the grounded prompt"
        );
        Ok(format!(
            "The battery is short before the promise.\n\
             ACTION: re_promise_date\nTARGET: {}\nPARAM: new_date=2027-11-16",
            self.subject.0
        ))
    }
    async fn embed(&self, _text: &str) -> ekg_domain::Result<Vec<f32>> {
        Ok(Vec::new())
    }
}

async fn provision_repo(http: &reqwest::Client, base: &str) -> Result<bool, String> {
    let Ok(resp) = http.get(format!("{base}/rest/repositories")).send().await else {
        return Ok(false);
    };
    if !resp.status().is_success() {
        return Ok(false);
    }
    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
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
    cfg["id"] = serde_json::Value::from(REPO);
    cfg["title"] = serde_json::Value::from("EKG propose-stage e2e IT (throwaway)");
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
    if resp.status().is_success() {
        Ok(true)
    } else {
        Err(format!("repo create failed {}", resp.status()))
    }
}

fn ap_url(ingress: &str, key: &str, handler: &str, send: bool) -> String {
    let mut url = reqwest::Url::parse(ingress).expect("ingress url");
    {
        let mut segs = url.path_segments_mut().expect("base url");
        segs.pop_if_empty()
            .extend(["ActionPointService", key, handler]);
        if send {
            segs.push("send");
        }
    }
    url.into()
}

async fn point_state(http: &reqwest::Client, ingress: &str, subject: &str) -> Option<String> {
    let resp = http
        .post(ap_url(ingress, subject, "get", false))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    value
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

async fn wait_until<F>(timeout: Duration, mut condition: F) -> bool
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn agent_advances_an_open_point_to_proposed() {
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(2);
    let callback_url = common::callback_url(2);
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
    match provision_repo(&http, &graphdb_url).await {
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

    // Materialize a minimal sale.order so the agent can ground it. A sensitive
    // `supplier` field is hidden by read policy, so the proposal must disclose it.
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    // Unique subject id per run, so the persistent, subject-keyed ActionPointService
    // state does not collide with a prior run or sibling test (see common::unique_base).
    let order_id = common::unique_base() + 1;
    let source = SourceRef {
        system: "odoo".to_owned(),
        model: "sale.order".to_owned(),
        id: order_id,
    };
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "commitment_date".to_owned(),
        "2027-10-19T00:00:00".to_owned(),
    );
    let record = SourceRecord {
        source_ref: source.clone(),
        fields,
    };
    world
        .materialize(&record)
        .await
        .expect("materialize sale.order");
    let subject = EntityUri(ontology::entity_iri("sale.order", order_id));

    // Serve the action-point object; register with Restate.
    let action_point = ActionPointService::default();
    let endpoint = Endpoint::builder().bind(action_point).build();
    // The listener is bound first, so a port collision fails here and not as a later
    // timeout, and the socket is listening before we register.
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
        "register failed: {}",
        register.status()
    );

    // Open the point (call), then run the agent in-process and attach: the same
    // sequence the backend bridge performs on a fresh open.
    let open = http
        .post(ap_url(&ingress_url, &subject.0, "open", false))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&OpenRequest {
                reason: "at risk".to_owned(),
                revision: 1,
            })
            .expect("encode open"),
        )
        .send()
        .await
        .expect("open");
    assert!(open.status().is_success(), "open failed: {}", open.status());

    let agent = supply_risk_agent(
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::new(StubCognition {
            subject: subject.clone(),
        }) as Arc<dyn CognitionProvider>,
        ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS,
    );
    let grounded: GroundedProposal = agent.propose(&subject).await.expect("agent proposes");
    assert_eq!(grounded.proposal.actions.len(), 1);
    let action = &grounded.proposal.actions[0];
    assert_eq!(action.action_type.0, "re_promise_date");
    assert_eq!(
        action.target, subject,
        "the re-promise targets the grounded order"
    );
    assert_eq!(
        action.params.get("new_date").map(String::as_str),
        Some("2027-11-16 00:00:00"),
        "the proposed date is carried, canonical, so a promotion can write it back"
    );

    let attach = http
        .post(ap_url(&ingress_url, &subject.0, "attach_proposal", true))
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

    // The point advances to `proposed`; nothing was written back.
    let proposed = wait_until(Duration::from_secs(20), async || {
        point_state(&http, &ingress_url, &subject.0)
            .await
            .as_deref()
            == Some("proposed")
    })
    .await;

    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;

    assert!(proposed, "the agent did not advance the point to proposed");
}
