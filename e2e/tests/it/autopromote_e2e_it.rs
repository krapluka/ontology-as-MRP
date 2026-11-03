//! Gated live e2e: the monitor bridge's `maybe_auto_promote` drives a `proposed`
//! action point to `promoted` (the governed write-back to Odoo and the durable
//! record) when a human-authored policy allowlists its reversible verb, and leaves
//! it `proposed` when the policy is empty (fully human-gated).
//!
//! Calls `ekg_backend::drift::maybe_auto_promote` directly against a live Restate
//! (the `ActionPointService`, served and registered here) and a live Odoo (the
//! `OdooActionExecutor`). It builds and cleans up its own throwaway sale orders, so
//! it needs no seed. Ignored by default; skips if the stack is unreachable.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it autopromote_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_backend::command::IngressLedger;
use ekg_backend::config::DEFAULT_COMMAND_CLAIM;
use ekg_backend::drift::maybe_auto_promote;
use ekg_domain::{
    ActionExecutor, ActionType, EntityUri, ExecutionPlan, GroundedProposal, Grounding, Plan,
    Proposal, ProposedAction, ReadContext, Result, Revision, SourceRecord, WorldModel,
};
use ekg_dynamic::{ActionPointRecord, ActionPointService, OpenRequest};
use ekg_kinetic::{OdooActionExecutor, PolicyEngine};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};

/// The policy id and version the test auto-promotes under (recorded as the promoter).
const POLICY_ID: &str = "test-autopromote/v1";

/// A no-op `WorldModel`: the policy-context gatherer only queries purchase-order targets,
/// and this test's actions target `sale.order`, so no method is called. Present only
/// to satisfy `maybe_auto_promote`'s reader parameter.
struct NoopWorld;

#[async_trait]
impl WorldModel for NoopWorld {
    async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
        unreachable!("not queried for a sale.order target")
    }
    async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
        unreachable!("not queried for a sale.order target")
    }
    async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
        unreachable!("not queried for a sale.order target")
    }
    async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
        Ok(Vec::new())
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Build an `ActionPointService` ingress URL for a handler.
fn ap_url(ingress: &str, key: &str, handler: &str) -> String {
    let mut url = reqwest::Url::parse(ingress).expect("ingress url");
    url.path_segments_mut()
        .expect("base url")
        .pop_if_empty()
        .extend(["ActionPointService", key, handler]);
    url.into()
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

/// Read the durable `ActionPointRecord` for `subject` via admin state introspection
/// and return the recorded promotion's reviewer, if any (the audit attribution).
async fn promotion_reviewer(
    http: &reqwest::Client,
    admin_url: &str,
    subject: &str,
) -> Option<String> {
    let query = "SELECT service_key, value_utf8 FROM state \
                 WHERE service_name = 'ActionPointService' AND key = 'record'";
    let resp = http
        .post(format!("{}/query", admin_url.trim_end_matches('/')))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .ok()?;
    let parsed: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    for row in parsed.get("rows").and_then(Value::as_array)? {
        if row.get("service_key").and_then(Value::as_str) != Some(subject) {
            continue;
        }
        let record = row.get("value_utf8").and_then(Value::as_str)?;
        let record: ActionPointRecord = serde_json::from_str(record).ok()?;
        return record.promotion.map(|p| p.reviewer.0);
    }
    None
}

/// Open a point and attach a `re_promise_date` proposal for `subject`, leaving it
/// `proposed`. Hand-built (no omissions) so the test is independent of the LLM.
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

    let attach = http
        .post(ap_url(ingress, &subject.0, "attach_proposal"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&grounded(subject, new_date)).expect("encode proposal"))
        .send()
        .await
        .expect("attach");
    assert!(
        attach.status().is_success(),
        "attach failed: {}",
        attach.status()
    );
    assert_eq!(
        point_state(http, ingress, &subject.0).await.as_deref(),
        Some("proposed"),
        "the point should be proposed after attach"
    );
}

/// A hand-built grounded `re_promise_date` proposal (no omissions).
fn grounded(subject: &EntityUri, new_date: &str) -> GroundedProposal {
    GroundedProposal {
        proposal: Proposal::single(
            "re-promise the delivery date to absorb the battery delay".to_owned(),
            re_promise(subject, new_date),
            Vec::new(),
        ),
        grounding_omissions: Vec::new(),
        grounding_edges: Vec::new(),
        subject_members: Vec::new(),
    }
}

/// A single-action plan wrapping `re_promise_date`.
fn re_promise_plan(subject: &EntityUri, new_date: &str) -> Plan {
    Plan {
        actions: vec![re_promise(subject, new_date)],
    }
}

/// The `re_promise_date` action against `subject`.
fn re_promise(subject: &EntityUri, new_date: &str) -> ProposedAction {
    let mut params = BTreeMap::new();
    params.insert("new_date".to_owned(), new_date.to_owned());
    ProposedAction {
        action_type: ActionType("re_promise_date".to_owned()),
        target: subject.clone(),
        params,
    }
}

/// Create a throwaway partner and draft sale order with an initial promise date.
async fn new_order(odoo: &Arc<Odoo>, initial: &str) -> (i64, i64) {
    let o = Arc::clone(odoo);
    let initial = initial.to_owned();
    tokio::task::spawn_blocking(move || {
        let partner = o
            .create("res.partner", json!({ "name": "EKG auto-promote IT" }))
            .expect("create partner");
        let order = o
            .create("sale.order", json!({ "partner_id": partner }))
            .expect("create sale.order");
        o.write(
            "sale.order",
            json!([order]),
            json!({ "commitment_date": initial }),
        )
        .expect("set initial commitment_date");
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn policy_auto_promotes_allowlisted_and_leaves_human_gated_proposed() {
    const INITIAL: &str = "2027-01-07 00:00:00";
    const REPROMISED: &str = "2027-12-31 00:00:00";
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");

    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(6);
    let callback_url = common::callback_url(6);
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

    // Connect Odoo (the write-back target and the executor); skip if unreachable.
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
            eprintln!("SKIP: Odoo unreachable ({why})");
            return;
        }
    };
    let executor = OdooActionExecutor::connect(&o_url, &o_db, &o_user, &o_key)
        .await
        .expect("executor connect");

    // Serve the action-point object (with the promote handler); register it.
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
        "register failed: {}",
        register.status()
    );

    // Two throwaway orders: one under an allowlisting policy, one under an empty one.
    let (auto_partner, auto_order) = new_order(&odoo, INITIAL).await;
    let (gated_partner, gated_order) = new_order(&odoo, INITIAL).await;
    let auto_subject = EntityUri(format!("https://ekg.example/id/sale.order/{auto_order}"));
    let gated_subject = EntityUri(format!("https://ekg.example/id/sale.order/{gated_order}"));

    drive_to_proposed(&http, &ingress_url, &auto_subject, REPROMISED).await;
    drive_to_proposed(&http, &ingress_url, &gated_subject, REPROMISED).await;

    // --- auto-promote: a policy allowlisting `re_promise_date` (a reversible verb) ---
    let auto_policy = PolicyEngine::new(
        POLICY_ID,
        ["re_promise_date".to_owned()].into_iter().collect(),
    );
    maybe_auto_promote(
        &IngressLedger::new(http.clone(), ingress_url.clone(), DEFAULT_COMMAND_CLAIM),
        &executor,
        &auto_policy,
        &NoopWorld,
        &auto_subject,
        &re_promise_plan(&auto_subject, REPROMISED),
    )
    .await;

    // --- human-gated: an empty policy must not promote ---
    let empty_policy = PolicyEngine::new(POLICY_ID, std::collections::BTreeSet::new());
    maybe_auto_promote(
        &IngressLedger::new(http.clone(), ingress_url.clone(), DEFAULT_COMMAND_CLAIM),
        &executor,
        &empty_policy,
        &NoopWorld,
        &gated_subject,
        &re_promise_plan(&gated_subject, REPROMISED),
    )
    .await;

    // Observe the outcomes before cleanup.
    let auto_state = point_state(&http, &ingress_url, &auto_subject.0).await;
    let gated_state = point_state(&http, &ingress_url, &gated_subject.0).await;
    let auto_date = commitment_date(&odoo, auto_order).await;
    let gated_date = commitment_date(&odoo, gated_order).await;
    let auto_reviewer = promotion_reviewer(&http, &admin_url, &auto_subject.0).await;

    // --- cleanup ---
    let cleanup = Arc::clone(&odoo);
    tokio::task::spawn_blocking(move || {
        let _ = cleanup.unlink("sale.order", json!([auto_order, gated_order]));
        let _ = cleanup.unlink("res.partner", json!([auto_partner, gated_partner]));
    })
    .await
    .expect("cleanup join");
    common::deregister(&http, &admin_url, &callback_url).await;

    // --- assertions: the auto-promoted point committed and is attributed to the policy ---
    assert_eq!(
        auto_state.as_deref(),
        Some("promoted"),
        "an allowlisted reversible verb should auto-promote to `promoted`"
    );
    assert_eq!(
        auto_date,
        json!(REPROMISED),
        "auto-promote must write the new promise date back to Odoo"
    );
    assert_eq!(
        auto_reviewer.as_deref(),
        Some(format!("policy:{POLICY_ID}").as_str()),
        "the audit must attribute the promotion to the policy, not a person"
    );

    // --- assertions: the human-gated point is untouched ---
    assert_eq!(
        gated_state.as_deref(),
        Some("proposed"),
        "an empty policy must leave the point `proposed` for a human"
    );
    assert_eq!(
        gated_date,
        json!(INITIAL),
        "a human-gated point must not write anything back to Odoo"
    );
}

// --- multi-action plan ---

/// A `notify_owner` action against `subject` carrying a message body. Reversible (the
/// inverse unlinks the created `mail.message`), moves no promised date, and costs
/// nothing, so a plan of these can auto-apply under an allowlisting policy.
fn notify(subject: &EntityUri, body: &str) -> ProposedAction {
    let mut params = BTreeMap::new();
    params.insert("message".to_owned(), body.to_owned());
    ProposedAction {
        action_type: ActionType("notify_owner".to_owned()),
        target: subject.clone(),
        params,
    }
}

/// A minimal multi-action plan: two notifications on the subject order. Every action
/// targets the subject (grounded) and is reversible.
fn two_notify_plan(subject: &EntityUri) -> Plan {
    Plan {
        actions: vec![
            notify(subject, "EKG multi-action IT: notice A"),
            notify(subject, "EKG multi-action IT: notice B"),
        ],
    }
}

/// A grounded proposal carrying `plan` verbatim (no omissions), for the durable attach.
fn grounded_plan(plan: &Plan) -> GroundedProposal {
    GroundedProposal {
        proposal: Proposal {
            explanation: "coordinated multi-action remediation".to_owned(),
            actions: plan.actions.clone(),
            omissions: Vec::new(),
        },
        grounding_omissions: Vec::new(),
        grounding_edges: Vec::new(),
        subject_members: Vec::new(),
    }
}

/// Open a point and attach `plan` (each action grounded on the subject), leaving it
/// `proposed`. Asserts each hop landed, so a fault in the stack fails the test here.
async fn attach_plan_to_proposed(
    http: &reqwest::Client,
    ingress: &str,
    subject: &EntityUri,
    plan: &Plan,
) {
    let open = http
        .post(ap_url(ingress, &subject.0, "open"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&OpenRequest {
                reason: "coordinated drift needs several changes".to_owned(),
                revision: 1,
            })
            .expect("encode open"),
        )
        .send()
        .await
        .expect("open");
    assert!(open.status().is_success(), "open failed: {}", open.status());

    let attach = http
        .post(ap_url(ingress, &subject.0, "attach_proposal"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&grounded_plan(plan)).expect("encode plan"))
        .send()
        .await
        .expect("attach");
    assert!(
        attach.status().is_success(),
        "attach failed: {}",
        attach.status()
    );
    assert_eq!(
        point_state(http, ingress, &subject.0).await.as_deref(),
        Some("proposed"),
        "the point should be proposed after attaching the plan"
    );
}

/// Read the durable `ExecutionPlan` recorded on the point (the N receipts a
/// promote-plan wrote), via admin state introspection.
async fn recorded_execution_plan(
    http: &reqwest::Client,
    admin_url: &str,
    subject: &str,
) -> Option<ExecutionPlan> {
    let query = "SELECT service_key, value_utf8 FROM state \
                 WHERE service_name = 'ActionPointService' AND key = 'record'";
    let resp = http
        .post(format!("{}/query", admin_url.trim_end_matches('/')))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .ok()?;
    let parsed: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    for row in parsed.get("rows").and_then(Value::as_array)? {
        if row.get("service_key").and_then(Value::as_str) != Some(subject) {
            continue;
        }
        let record = row.get("value_utf8").and_then(Value::as_str)?;
        let record: ActionPointRecord = serde_json::from_str(record).ok()?;
        return record.execution_plan;
    }
    None
}

/// Count the comment `mail.message`s posted on a sale order (the notify write-back).
async fn notice_count(odoo: &Arc<Odoo>, order: i64) -> usize {
    let o = Arc::clone(odoo);
    tokio::task::spawn_blocking(move || {
        o.search(
            "mail.message",
            json!([
                ["model", "=", "sale.order"],
                ["res_id", "=", order],
                ["message_type", "=", "comment"]
            ]),
        )
        .map_or(0, |ids| ids.len())
    })
    .await
    .expect("search join")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn multi_action_plan_promotes_atomically_and_reverses_as_a_unit() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
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
            eprintln!("SKIP: Odoo unreachable ({why})");
            return;
        }
    };
    let executor = OdooActionExecutor::connect(&o_url, &o_db, &o_user, &o_key)
        .await
        .expect("executor connect");

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
        "register failed: {}",
        register.status()
    );

    let (partner, order) = new_order(&odoo, "2027-01-07 00:00:00").await;
    let subject = EntityUri(format!("https://ekg.example/id/sale.order/{order}"));
    let plan = two_notify_plan(&subject);

    // A policy allowlisting the (reversible, no-date-move, zero-cost) notify verb, so
    // the whole plan auto-applies as one governed step.
    let policy = PolicyEngine::new(POLICY_ID, ["notify_owner".to_owned()].into_iter().collect());

    let before = notice_count(&odoo, order).await;
    attach_plan_to_proposed(&http, &ingress_url, &subject, &plan).await;

    // One governed promotion authorizes every action, committed as a unit.
    maybe_auto_promote(
        &IngressLedger::new(http.clone(), ingress_url.clone(), DEFAULT_COMMAND_CLAIM),
        &executor,
        &policy,
        &NoopWorld,
        &subject,
        &plan,
    )
    .await;

    // Observe the promoted outcome before reversing.
    let promoted_state = point_state(&http, &ingress_url, &subject.0).await;
    let after_promote = notice_count(&odoo, order).await;
    let execution_plan = recorded_execution_plan(&http, &admin_url, &subject.0).await;

    // Reverse the executed plan as a unit (undo each receipt in reverse order).
    let reverse_result = match &execution_plan {
        Some(ep) => Some(executor.reverse_plan(ep).await),
        None => None,
    };
    let after_reverse = notice_count(&odoo, order).await;

    // --- cleanup ---
    let cleanup = Arc::clone(&odoo);
    tokio::task::spawn_blocking(move || {
        let _ = cleanup.unlink("sale.order", json!([order]));
        let _ = cleanup.unlink("res.partner", json!([partner]));
    })
    .await
    .expect("cleanup join");
    common::deregister(&http, &admin_url, &callback_url).await;

    // --- assertions ---
    assert_eq!(
        promoted_state.as_deref(),
        Some("promoted"),
        "the multi-action plan should promote as one governed step"
    );
    let execution_plan = execution_plan.expect("an execution plan was recorded durably");
    assert_eq!(
        execution_plan.receipts.len(),
        2,
        "one promotion committed all N (=2) actions atomically"
    );
    assert_eq!(
        after_promote - before,
        2,
        "both notifications were written back to Odoo"
    );
    reverse_result
        .expect("a plan was reversed")
        .expect("the plan reverses cleanly as a unit");
    assert_eq!(
        after_reverse, before,
        "reversing the plan as a unit undoes all N actions"
    );
}
