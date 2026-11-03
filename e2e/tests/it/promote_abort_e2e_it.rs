//! Gated live e2e: the human-oversight API promotes a `proposed` action point, which
//! runs the governed write-back to Odoo and takes the point from `proposed` through
//! `under_review` to `promoted`, and aborts another with no write-back.
//!
//! Drives the `action_point` router (via `tower::oneshot`) against a live Restate
//! (the `ActionPointService`, served and registered here) and a live Odoo (the
//! `OdooActionExecutor`). It builds and cleans up its own throwaway sale orders, so
//! it needs no seed. Ignored by default; skips instead of failing if the substrate is
//! unreachable.
//!
//! A second test races two promotes of one point: the durable command claim lets one
//! write back and record, and refuses the other with 409.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it promote_abort_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ekg_backend::action_point::{ApiState, router};
use ekg_domain::{ActionType, EntityUri, GroundedProposal, Proposal, ProposedAction};
use ekg_dynamic::{ActionPointService, OpenRequest};
use ekg_kinetic::OdooActionExecutor;
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tower::ServiceExt;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Builds an `ActionPointService` ingress URL (optionally the fire-and-forget form).
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
    let value: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    value
        .get("state")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Opens a point and attaches a `re_promise_date` proposal for `subject`, leaving it
/// `proposed`. The grounded proposal is hand-built (no omissions), so the test does
/// not depend on the LLM or on grounding.
async fn drive_to_proposed(
    http: &reqwest::Client,
    ingress: &str,
    subject: &EntityUri,
    new_date: &str,
) {
    let mut params = BTreeMap::new();
    params.insert("new_date".to_owned(), new_date.to_owned());
    let action = ProposedAction {
        action_type: ActionType("re_promise_date".to_owned()),
        target: subject.clone(),
        params,
    };
    drive_to_proposed_with(http, ingress, subject, action).await;
}

/// Opens a point and attaches a one-action proposal of `action` for `subject`, leaving it
/// `proposed`.
async fn drive_to_proposed_with(
    http: &reqwest::Client,
    ingress: &str,
    subject: &EntityUri,
    action: ProposedAction,
) {
    let open = http
        .post(ap_url(ingress, &subject.0, "open", false))
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
        proposal: Proposal::single("remediate the battery delay".to_owned(), action, Vec::new()),
        grounding_omissions: Vec::new(),
        grounding_edges: Vec::new(),
        subject_members: Vec::new(),
    };
    let attach = http
        .post(ap_url(ingress, &subject.0, "attach_proposal", false))
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
    assert_eq!(
        point_state(http, ingress, &subject.0).await.as_deref(),
        Some("proposed"),
        "the point should be proposed after attach"
    );
}

/// Creates a throwaway partner and a draft sale order with an initial promise date.
async fn new_order(odoo: &Arc<Odoo>, initial: &str) -> (i64, i64) {
    let o = Arc::clone(odoo);
    let initial = initial.to_owned();
    tokio::task::spawn_blocking(move || {
        let partner = o
            .create("res.partner", json!({ "name": "EKG promote/abort IT" }))
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

/// Encodes `/api/action-points/one?subject=<iri>` with the subject percent-encoded.
fn one_uri(subject: &str) -> String {
    let mut url = reqwest::Url::parse("http://local/api/action-points/one").expect("url");
    url.query_pairs_mut().append_pair("subject", subject);
    format!("{}?{}", url.path(), url.query().expect("query"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn promote_writes_back_and_abort_does_not() {
    const INITIAL: &str = "2027-01-07 00:00:00";
    const REPROMISED: &str = "2027-12-31 00:00:00";
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");

    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(5);
    let callback_url = common::callback_url(5);
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

    // Odoo is both the write-back target and what the executor talks to.
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
        "register failed: {}",
        register.status()
    );

    // Two throwaway orders: one to promote, one to abort.
    let (promote_partner, promote_order) = new_order(&odoo, INITIAL).await;
    let (abort_partner, abort_order) = new_order(&odoo, INITIAL).await;
    let promote_subject = EntityUri(format!("https://ekg.example/id/sale.order/{promote_order}"));
    let abort_subject = EntityUri(format!("https://ekg.example/id/sale.order/{abort_order}"));

    drive_to_proposed(&http, &ingress_url, &promote_subject, REPROMISED).await;
    drive_to_proposed(&http, &ingress_url, &abort_subject, REPROMISED).await;

    let app = router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor,
        undo_window: std::time::Duration::from_secs(8 * 60 * 60),
        claim_deadline: ekg_backend::config::DEFAULT_COMMAND_CLAIM,
    });

    // --- comprehension: GET the proposed point ---
    let get_resp = app
        .clone()
        .oneshot(
            Request::get(one_uri(&promote_subject.0))
                .body(Body::empty())
                .expect("get req"),
        )
        .await
        .expect("get one");
    assert_eq!(get_resp.status(), StatusCode::OK, "GET one should be 200");

    // --- promote: the governed write-back ---
    let promote_resp = app
        .clone()
        .oneshot(
            Request::post("/api/action-points/promote")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({
                        "subject": promote_subject.0,
                        "reviewer": "planner-1",
                        "rationale": "shortfall confirmed; re-promise approved",
                        "context_seen": "evidence subgraph, no omissions"
                    })
                    .to_string(),
                ))
                .expect("promote req"),
        )
        .await
        .expect("promote");
    let promote_status = promote_resp.status();

    // --- abort: the stop control, no write-back ---
    let abort_resp = app
        .clone()
        .oneshot(
            Request::post("/api/action-points/abort")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({
                        "subject": abort_subject.0,
                        "reviewer": "planner-1",
                        "reason": "duplicate; handled manually"
                    })
                    .to_string(),
                ))
                .expect("abort req"),
        )
        .await
        .expect("abort");
    let abort_status = abort_resp.status();

    // Read the outcomes before the cleanup removes the orders.
    let promoted_state = point_state(&http, &ingress_url, &promote_subject.0).await;
    let aborted_state = point_state(&http, &ingress_url, &abort_subject.0).await;
    let promoted_date = commitment_date(&odoo, promote_order).await;
    let aborted_date = commitment_date(&odoo, abort_order).await;

    // --- cleanup ---
    let cleanup = Arc::clone(&odoo);
    tokio::task::spawn_blocking(move || {
        let _ = cleanup.unlink("sale.order", json!([promote_order, abort_order]));
        let _ = cleanup.unlink("res.partner", json!([promote_partner, abort_partner]));
    })
    .await
    .expect("cleanup join");
    common::deregister(&http, &admin_url, &callback_url).await;

    // --- assertions ---
    assert_eq!(promote_status, StatusCode::OK, "promote should be 200");
    assert_eq!(
        promoted_state.as_deref(),
        Some("promoted"),
        "the promoted point should reach `promoted`"
    );
    assert_eq!(
        promoted_date,
        json!(REPROMISED),
        "promote must write the new promise date back to Odoo"
    );

    assert_eq!(abort_status, StatusCode::NO_CONTENT, "abort should be 204");
    assert_eq!(
        aborted_state.as_deref(),
        Some("aborted"),
        "the aborted point should reach `aborted`"
    );
    assert_eq!(
        aborted_date,
        json!(INITIAL),
        "abort must not write anything back to Odoo"
    );
}

/// How many chatter comments `order` carries. Each `notify_owner` write-back posts one.
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

/// POSTs a human promote of `subject` by `reviewer`, returning the status and the body.
async fn promote_as(app: &axum::Router, subject: &str, reviewer: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/action-points/promote")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({
                        "subject": subject,
                        "reviewer": reviewer,
                        "rationale": "owner must confirm the new battery date"
                    })
                    .to_string(),
                ))
                .expect("promote req"),
        )
        .await
        .expect("promote");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("promote body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn concurrent_promotes_of_one_point_write_back_once() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(15);
    let callback_url = common::callback_url(15);
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
    let executor = Arc::new(
        OdooActionExecutor::connect(&o_url, &o_db, &o_user, &o_key)
            .await
            .expect("executor connect"),
    );

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

    // A point whose plan posts one chatter comment, so every write-back is countable.
    let (partner, order) = new_order(&odoo, "2027-01-07 00:00:00").await;
    let subject = EntityUri(format!("https://ekg.example/id/sale.order/{order}"));
    let mut params = BTreeMap::new();
    params.insert(
        "message".to_owned(),
        "EKG concurrency IT: confirm the battery date".to_owned(),
    );
    let notify = ProposedAction {
        action_type: ActionType("notify_owner".to_owned()),
        target: subject.clone(),
        params,
    };
    drive_to_proposed_with(&http, &ingress_url, &subject, notify).await;

    let app = router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor,
        undo_window: std::time::Duration::from_secs(8 * 60 * 60),
        claim_deadline: ekg_backend::config::DEFAULT_COMMAND_CLAIM,
    });

    // Two operators approve the same point at the same time.
    let before = notice_count(&odoo, order).await;
    let (first, second) = tokio::join!(
        promote_as(&app, &subject.0, "planner-a"),
        promote_as(&app, &subject.0, "planner-b"),
    );
    let after = notice_count(&odoo, order).await;
    let state = point_state(&http, &ingress_url, &subject.0).await;
    let record: Value = serde_json::from_str(
        &http
            .post(ap_url(&ingress_url, &subject.0, "record", false))
            .send()
            .await
            .expect("record")
            .text()
            .await
            .expect("record body"),
    )
    .expect("record json");
    let promoted_entries = record["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["state"].as_str() == Some("promoted"))
        .count();

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
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "exactly one promote wins, the other is refused: {first:?} / {second:?}"
    );
    let refused = if first.0 == StatusCode::CONFLICT {
        &first.1
    } else {
        &second.1
    };
    assert!(
        refused.contains("promote by planner-") || refused.contains("Promoted"),
        "the refusal names the holder or the settled state: {refused}"
    );
    assert_eq!(
        after - before,
        1,
        "the source was written back exactly once"
    );
    assert_eq!(state.as_deref(), Some("promoted"));
    assert_eq!(promoted_entries, 1, "the audit records one promotion");
    assert!(
        record.get("claim").is_none(),
        "the winner's record cleared its claim: {record}"
    );
}
