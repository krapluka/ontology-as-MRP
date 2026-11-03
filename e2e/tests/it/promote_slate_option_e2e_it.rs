//! Gated live e2e: the governed `promote` commits the slate option the operator
//! selected, not the point's single staged proposal.
//!
//! Drives the real `action_point` router (via `tower::oneshot`) against a live Restate
//! (the `ActionPointService`) and a live Odoo (the `OdooActionExecutor`). For each of three
//! throwaway sale orders it opens a point, attaches a single staged proposal (with its own
//! distinct date) and then a ranked slate with two options, and finally promotes:
//!
//!   1. the recommended option's `rank_key`: Odoo's `commitment_date` becomes that
//!      option's date, not the staged proposal's;
//!   2. a non-recommended option's `rank_key`: Odoo commits its (different) date;
//!   3. a stale or unknown `rank_key`: refused with 409 and nothing written back.
//!
//! The slate is attached by hand to keep the test deterministic, as the sibling promote e2e
//! builds its grounded proposal by hand. Under test is the promote handler, not how a slate
//! is produced (`slate_supply_risk_e2e_it` and `slate_e2e_it` cover the real loop). Ignored
//! by default; skips if the stack is unreachable.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it promote_slate_option_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ekg_backend::action_point::{ApiState, router};
use ekg_domain::{
    ActionType, EntityUri, GroundedProposal, Plan, Proposal, ProposedAction, RankedPlan, Score,
    Slate, Trace,
};
use ekg_dynamic::{ActionPointRecord, ActionPointService, OpenRequest};
use ekg_kinetic::OdooActionExecutor;
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tower::ServiceExt;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Build an `ActionPointService` ingress URL for `handler`.
fn ap_url(ingress: &str, key: &str, handler: &str) -> String {
    let mut url = reqwest::Url::parse(ingress).expect("ingress url");
    {
        let mut segs = url.path_segments_mut().expect("base url");
        segs.pop_if_empty()
            .extend(["ActionPointService", key, handler]);
    }
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

/// A `re_promise_date` action on `subject` carrying `new_date`: the verb the
/// `OdooActionExecutor` maps to a `commitment_date` write-back.
fn re_promise(subject: &EntityUri, new_date: &str) -> ProposedAction {
    ProposedAction {
        action_type: ActionType("re_promise_date".to_owned()),
        target: subject.clone(),
        params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
    }
}

/// One ranked slate option: a single-action `re_promise_date` plan carrying `rank_key`.
fn option(subject: &EntityUri, rank_key: &str, new_date: &str, recommended: bool) -> RankedPlan {
    RankedPlan {
        plan: Plan {
            actions: vec![re_promise(subject, new_date)],
        },
        score: Score {
            metrics: BTreeMap::new(),
            rank_key: rank_key.to_owned(),
        },
        recommended,
        needs: Vec::new(),
        omissions: Vec::new(),
        trace: Trace::default(),
    }
}

/// Open a point, attach a single staged proposal (with `proposal_date`), then attach a
/// two-option slate, leaving the point `proposed` with both a proposal and a slate. The
/// recommended option promises `rec_date`, the alternate `alt_date`, and the staged
/// proposal a third `proposal_date`, so a promotion that committed the staged proposal
/// would show up as a different write-back.
async fn drive_to_proposed_with_slate(
    http: &reqwest::Client,
    ingress: &str,
    subject: &EntityUri,
    proposal_date: &str,
    rec_date: &str,
    alt_date: &str,
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
            re_promise(subject, proposal_date),
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
        .expect("attach proposal");
    assert!(
        attach.status().is_success(),
        "attach proposal failed: {}",
        attach.status()
    );

    // Best-first: the recommended option carries the smaller rank_key.
    let slate = Slate {
        baseline: Score {
            metrics: BTreeMap::new(),
            rank_key: "99999".to_owned(),
        },
        options: vec![
            option(subject, "00001", rec_date, true),
            option(subject, "00002", alt_date, false),
        ],
        trace: Trace::default(),
    };
    let attach_slate = http
        .post(ap_url(ingress, &subject.0, "attach_slate"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&slate).expect("encode slate"))
        .send()
        .await
        .expect("attach slate");
    assert!(
        attach_slate.status().is_success(),
        "attach slate failed: {}",
        attach_slate.status()
    );
    assert_eq!(
        point_state(http, ingress, &subject.0).await.as_deref(),
        Some("proposed"),
        "the point should be proposed after attach (a slate does not transition it)"
    );
}

/// Read the durable promotion's `context_seen` for `subject` via Restate admin state
/// introspection: the audit provenance that must name which option (`rank_key`) the reviewer
/// committed (EU AI Act Art. 12). Mirrors `autopromote_e2e_it::promotion_reviewer`.
async fn promotion_context_seen(
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
        return record.promotion.map(|promotion| promotion.context_seen);
    }
    None
}

/// Create a throwaway partner and draft sale order with an initial promise date.
async fn new_order(odoo: &Arc<Odoo>, initial: &str) -> (i64, i64) {
    let o = Arc::clone(odoo);
    let initial = initial.to_owned();
    tokio::task::spawn_blocking(move || {
        let partner = o
            .create("res.partner", json!({ "name": "EKG promote-slate IT" }))
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

/// POST `/api/action-points/promote` for `subject` naming `option_rank_key` (when `Some`).
async fn promote(app: &axum::Router, subject: &str, option_rank_key: Option<&str>) -> StatusCode {
    let mut body = json!({
        "subject": subject,
        "reviewer": "planner-1",
        "rationale": "shortfall confirmed; selected option approved",
        "context_seen": "evidence subgraph, no omissions",
    });
    if let Some(key) = option_rank_key {
        body["option_rank_key"] = json!(key);
    }
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/action-points/promote")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("promote req"),
        )
        .await
        .expect("promote");
    resp.status()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn promote_commits_the_selected_slate_option() {
    const INITIAL: &str = "2027-01-07 00:00:00";
    // Three distinct dates so the write-back's date identifies which plan was committed.
    const PROPOSAL_DATE: &str = "2027-06-12 00:00:00"; // the staged single proposal (must not win)
    const REC_DATE: &str = "2027-12-31 00:00:00"; // the recommended option
    const ALT_DATE: &str = "2027-03-03 00:00:00"; // the non-recommended option

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

    // Three throwaway orders: recommended-option promote, non-recommended promote, stale refuse.
    let (rec_partner, rec_order) = new_order(&odoo, INITIAL).await;
    let (alt_partner, alt_order) = new_order(&odoo, INITIAL).await;
    let (stale_partner, stale_order) = new_order(&odoo, INITIAL).await;
    let rec_subject = EntityUri(format!("https://ekg.example/id/sale.order/{rec_order}"));
    let alt_subject = EntityUri(format!("https://ekg.example/id/sale.order/{alt_order}"));
    let stale_subject = EntityUri(format!("https://ekg.example/id/sale.order/{stale_order}"));

    for subject in [&rec_subject, &alt_subject, &stale_subject] {
        drive_to_proposed_with_slate(
            &http,
            &ingress_url,
            subject,
            PROPOSAL_DATE,
            REC_DATE,
            ALT_DATE,
        )
        .await;
    }

    let app = router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor,
        undo_window: std::time::Duration::from_secs(8 * 60 * 60),
        claim_deadline: ekg_backend::config::DEFAULT_COMMAND_CLAIM,
    });

    // 1) Promote the recommended option: commits its date (rank_key "00001").
    let rec_status = promote(&app, &rec_subject.0, Some("00001")).await;
    // 2) Promote the non-recommended option: commits its date (rank_key "00002").
    let alt_status = promote(&app, &alt_subject.0, Some("00002")).await;
    // 3) Promote a stale or unknown option: refused 409, nothing written.
    let stale_status = promote(&app, &stale_subject.0, Some("00099")).await;

    // Observe the outcomes before cleanup.
    let rec_state = point_state(&http, &ingress_url, &rec_subject.0).await;
    let alt_state = point_state(&http, &ingress_url, &alt_subject.0).await;
    let stale_state = point_state(&http, &ingress_url, &stale_subject.0).await;
    let rec_date_after = commitment_date(&odoo, rec_order).await;
    let alt_date_after = commitment_date(&odoo, alt_order).await;
    let stale_date_after = commitment_date(&odoo, stale_order).await;
    // The audit provenance of which option was committed (Art. 12).
    let rec_ctx = promotion_context_seen(&http, &admin_url, &rec_subject.0).await;
    let alt_ctx = promotion_context_seen(&http, &admin_url, &alt_subject.0).await;

    // --- cleanup ---
    let cleanup = Arc::clone(&odoo);
    tokio::task::spawn_blocking(move || {
        let _ = cleanup.unlink("sale.order", json!([rec_order, alt_order, stale_order]));
        let _ = cleanup.unlink(
            "res.partner",
            json!([rec_partner, alt_partner, stale_partner]),
        );
    })
    .await
    .expect("cleanup join");
    common::deregister(&http, &admin_url, &callback_url).await;

    // --- assertions ---
    // Recommended option committed: its date, not the staged proposal's.
    assert_eq!(
        rec_status,
        StatusCode::OK,
        "recommended promote should be 200"
    );
    assert_eq!(
        rec_state.as_deref(),
        Some("promoted"),
        "recommended is promoted"
    );
    assert_eq!(
        rec_date_after,
        json!(REC_DATE),
        "promote must write the recommended option's date back to Odoo"
    );
    assert_ne!(
        rec_date_after,
        json!(PROPOSAL_DATE),
        "promote must not write the staged single proposal's date"
    );
    // The durable audit records which option (rank_key) the reviewer committed.
    assert!(
        rec_ctx
            .as_deref()
            .is_some_and(|ctx| ctx.contains("rank_key=00001")),
        "the promotion provenance must record the committed option rank_key=00001, got {rec_ctx:?}"
    );

    // Non-recommended option committed: its own (different) date.
    assert_eq!(
        alt_status,
        StatusCode::OK,
        "non-recommended promote should be 200"
    );
    assert_eq!(
        alt_state.as_deref(),
        Some("promoted"),
        "non-recommended is promoted"
    );
    assert_eq!(
        alt_date_after,
        json!(ALT_DATE),
        "promote must write the NON-RECOMMENDED option's date back to Odoo"
    );
    assert!(
        alt_ctx
            .as_deref()
            .is_some_and(|ctx| ctx.contains("rank_key=00002")),
        "the promotion provenance must record the committed option rank_key=00002, got {alt_ctx:?}"
    );

    // Stale or unknown option refused: nothing written, point unchanged.
    assert_eq!(
        stale_status,
        StatusCode::CONFLICT,
        "a stale/unknown option rank_key must be refused with 409"
    );
    assert_eq!(
        stale_state.as_deref(),
        Some("proposed"),
        "a refused promote must leave the point proposed (nothing committed)"
    );
    assert_eq!(
        stale_date_after,
        json!(INITIAL),
        "a refused promote must not write anything back to Odoo"
    );
}
