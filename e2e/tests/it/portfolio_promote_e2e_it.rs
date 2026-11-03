//! Gated live e2e: a portfolio's joint plan is promoted, and undone, as one governed step
//! across every member.
//!
//! Drives the `portfolio_command` router (via `tower::oneshot`) against a live Restate
//! (the `ActionPointService` and `PortfolioService` objects, served in-process on port offset
//! 19) and a live Odoo (the `OdooActionExecutor`). A throwaway portfolio of two `proposed`
//! sale-order points is hand-built through the objects' own handlers (formation, membership,
//! a joint slate), since the command is under test here, not grouping or slate production
//! (covered by `portfolio_grouping_e2e_it` and `portfolio_slate_e2e_it`). Its joint slate's
//! option `A` expedites a purchase-order line and re-promises the first order. It checks:
//!
//! 1. an approve naming a stale generation is refused with 409 and writes nothing;
//! 2. a clean approve writes both changes once, promotes both members with the portfolio
//!    named, and records the portfolio as promoted;
//! 3. a portfolio undo inside the window reverses both changes and records both members and
//!    the portfolio as reversed;
//! 4. on a second portfolio, after its promotion someone changes the re-promised order's
//!    promise: the undo answers 502 naming one remaining action, reverses the expedite and
//!    leaves everything promoted with the progress recorded; once the promise is put back, a
//!    second undo reverses only the remaining action and records everything as reversed.
//!
//! Ignored by default; skips if the substrate is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1`
//! is set (as `just test-e2e` does), when it fails loudly.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it portfolio_promote_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ekg_backend::action_point::ApiState;
use ekg_backend::portfolio_command::router;
use ekg_domain::{
    ActionExecutor, ActionType, EntityUri, GroundedProposal, Plan, Proposal, ProposedAction,
    RankedPlan, Score, Slate, Trace,
};
use ekg_dynamic::{ActionPointService, PortfolioService, portfolio_id};
use ekg_kinetic::OdooActionExecutor;
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tower::ServiceExt;

const OFFSET: u16 = 19;
const PROMISED: &str = "2027-11-26 00:00:00";
const RE_PROMISED: &str = "2027-12-03 00:00:00";
const PLANNED: &str = "2027-11-24 08:00:00";
const EXPEDITED: &str = "2027-11-16 08:00:00";

/// The throwaway Odoo records: a supplier + customer, a product, a draft PO with one line, and
/// two sale orders (the portfolio's members).
struct Fixture {
    partner: i64,
    template: i64,
    po: i64,
    line: i64,
    orders: [i64; 2],
}

async fn odoo_do<T: Send + 'static>(
    odoo: &Arc<Odoo>,
    f: impl FnOnce(&Odoo) -> T + Send + 'static,
) -> T {
    let o = Arc::clone(odoo);
    tokio::task::spawn_blocking(move || f(&o))
        .await
        .expect("odoo join")
}

async fn fixture(odoo: &Arc<Odoo>) -> Fixture {
    odoo_do(odoo, |o| {
        let uom = o.xmlid("uom", "product_uom_unit").expect("uom");
        let partner = o
            .create("res.partner", json!({ "name": "EKG portfolio-promote IT" }))
            .expect("create partner");
        let template = o
            .create(
                "product.template",
                json!({
                    "name": "EKG portfolio-promote IT component",
                    "type": "consu",
                    "purchase_ok": true,
                    "uom_id": uom,
                }),
            )
            .expect("create product.template");
        let variant = o
            .read_m2o("product.template", template, "product_variant_id")
            .expect("product variant");
        let po = o
            .create("purchase.order", json!({ "partner_id": partner }))
            .expect("create purchase.order");
        let line = o
            .create(
                "purchase.order.line",
                json!({
                    "order_id": po,
                    "product_id": variant,
                    "name": "EKG portfolio-promote IT line",
                    "product_qty": 10,
                    "product_uom_id": uom,
                    "price_unit": 5.0,
                    "date_planned": PLANNED,
                }),
            )
            .expect("create purchase.order.line");
        let order = || {
            let id = o
                .create("sale.order", json!({ "partner_id": partner }))
                .expect("create sale.order");
            o.write(
                "sale.order",
                json!([id]),
                json!({ "commitment_date": PROMISED }),
            )
            .expect("set the promise");
            id
        };
        let orders = [order(), order()];
        Fixture {
            partner,
            template,
            po,
            line,
            orders,
        }
    })
    .await
}

async fn drop_fixture(odoo: &Arc<Odoo>, f: &Fixture) {
    let (po, template, partner, orders) = (f.po, f.template, f.partner, f.orders);
    odoo_do(odoo, move |o| {
        let _ = o.unlink("sale.order", json!(orders));
        let _ = o.unlink("purchase.order", json!([po]));
        let _ = o.unlink("product.template", json!([template]));
        let _ = o.unlink("res.partner", json!([partner]));
    })
    .await;
}

async fn field(odoo: &Arc<Odoo>, model: &'static str, id: i64, name: &'static str) -> Value {
    odoo_do(odoo, move |o| {
        o.read(model, json!([id]), json!([name]))
            .ok()
            .and_then(|rows| rows.as_array()?.first()?.get(name).cloned())
            .unwrap_or(Value::Null)
    })
    .await
}

/// The two written fields: the line's planned arrival and the first order's promise.
async fn written(odoo: &Arc<Odoo>, f: &Fixture) -> (Value, Value) {
    (
        field(odoo, "purchase.order.line", f.line, "date_planned").await,
        field(odoo, "sale.order", f.orders[0], "commitment_date").await,
    )
}

/// POST `body` to `handler` on the `service` object keyed `key`.
async fn call(
    http: &reqwest::Client,
    ingress: &str,
    service: &str,
    key: &str,
    handler: &str,
    body: Option<Value>,
) -> Result<Value, String> {
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

fn line_iri(line: i64) -> EntityUri {
    EntityUri(format!("https://ekg.example/id/purchase.order.line/{line}"))
}

fn order_iri(order: i64) -> EntityUri {
    EntityUri(format!("https://ekg.example/id/sale.order/{order}"))
}

fn action(verb: &str, target: &EntityUri, new_date: &str) -> ProposedAction {
    ProposedAction {
        action_type: ActionType(verb.to_owned()),
        target: target.clone(),
        params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
    }
}

/// The joint slate: option `A` expedites the line and re-promises the first order; option `B`
/// only re-promises it.
fn joint_slate(f: &Fixture) -> Slate {
    let option = |key: &str, actions: Vec<ProposedAction>| RankedPlan {
        plan: Plan { actions },
        score: Score {
            metrics: BTreeMap::new(),
            rank_key: key.to_owned(),
        },
        recommended: key == "A",
        needs: Vec::new(),
        omissions: Vec::new(),
        trace: Trace::default(),
    };
    let order = order_iri(f.orders[0]);
    Slate {
        baseline: Score {
            metrics: BTreeMap::new(),
            rank_key: "Z".to_owned(),
        },
        options: vec![
            option(
                "A",
                vec![
                    action("expedite_po", &line_iri(f.line), EXPEDITED),
                    action("re_promise_date", &order, RE_PROMISED),
                ],
            ),
            option("B", vec![action("re_promise_date", &order, RE_PROMISED)]),
        ],
        trace: Trace::default(),
    }
}

/// Build the portfolio through the objects' own handlers: both points `proposed` and members
/// of the portfolio, which formed twice (generation 2) and carries the joint slate for it.
async fn build_portfolio(
    http: &reqwest::Client,
    ingress: &str,
    f: &Fixture,
    members: &[EntityUri],
    id: &str,
) -> Result<(), String> {
    for (i, member) in members.iter().enumerate() {
        let key = member.0.as_str();
        call(
            http,
            ingress,
            "ActionPointService",
            key,
            "open",
            Some(json!({ "reason": "late against the promise", "revision": 1 })),
        )
        .await?;
        let grounded = GroundedProposal {
            proposal: Proposal::single(
                "re-promise the order".to_owned(),
                action("re_promise_date", &order_iri(f.orders[i]), RE_PROMISED),
                Vec::new(),
            ),
            grounding_omissions: Vec::new(),
            grounding_edges: Vec::new(),
            subject_members: Vec::new(),
        };
        call(
            http,
            ingress,
            "ActionPointService",
            key,
            "attach_proposal",
            Some(serde_json::to_value(&grounded).map_err(|e| e.to_string())?),
        )
        .await?;
    }
    let form = json!({ "members": members, "reason": "both wait on the same inbound line" });
    call(
        http,
        ingress,
        "PortfolioService",
        id,
        "form",
        Some(form.clone()),
    )
    .await?;
    call(
        http,
        ingress,
        "PortfolioService",
        id,
        "supersede",
        Some(json!({ "reason": "a third order joined" })),
    )
    .await?;
    call(http, ingress, "PortfolioService", id, "form", Some(form)).await?;
    call(
        http,
        ingress,
        "PortfolioService",
        id,
        "attach_slate",
        Some(json!({ "generation": 2, "slate": joint_slate(f) })),
    )
    .await?;
    for member in members {
        call(
            http,
            ingress,
            "ActionPointService",
            &member.0,
            "set_portfolio",
            Some(json!({ "portfolio": id })),
        )
        .await?;
    }
    Ok(())
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
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

fn approve(id: &str, generation: u32) -> Value {
    json!({
        "portfolio": id,
        "generation": generation,
        "option_rank_key": "A",
        "reviewer": "planner-1",
        "rationale": "approve plan A: the expedite covers both orders",
        "context_seen": "joint slate, 2 options"
    })
}

fn undo(id: &str) -> Value {
    json!({ "portfolio": id, "reviewer": "planner-1", "reason": "the supplier cannot ship early" })
}

fn state_of(point: &Value) -> Value {
    point["point"]["state"].clone()
}

fn last_note(point: &Value) -> String {
    point["audit"]
        .as_array()
        .and_then(|a| a.last())
        .and_then(|e| e["note"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn events(portfolio: &Value) -> Vec<String> {
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
#[ignore = "requires Odoo + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn a_portfolio_plan_is_promoted_and_undone_as_one_governed_step() {
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
    let (o_url, o_db, o_user, o_key) = (
        common::env_or("ODOO_URL", "http://localhost:8069"),
        common::env_or("ODOO_DB", "ekg"),
        common::env_or("ODOO_USER", "admin"),
        common::env_or("ODOO_API_KEY", "admin"),
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
    let executor = Arc::new(OdooActionExecutor::new(Arc::clone(&odoo)));

    let endpoint = Endpoint::builder()
        .bind(ActionPointService::default())
        .bind(PortfolioService::default())
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let registered = common::register(&http, &admin_url, &callback_url).await;

    let f = fixture(&odoo).await;
    let members = [order_iri(f.orders[0]), order_iri(f.orders[1])];
    let id = portfolio_id(&members).0;
    let g = fixture(&odoo).await;
    let partial_members = [order_iri(g.orders[0]), order_iri(g.orders[1])];
    let partial_id = portfolio_id(&partial_members).0;
    let app = router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor: Arc::clone(&executor) as Arc<dyn ActionExecutor>,
        undo_window: Duration::from_secs(8 * 60 * 60),
        claim_deadline: ekg_backend::config::DEFAULT_COMMAND_CLAIM,
    });

    let run = async || -> Result<BTreeMap<String, Value>, String> {
        registered.clone()?;
        build_portfolio(&http, &ingress_url, &f, &members, &id).await?;
        let mut seen = BTreeMap::new();
        let snapshot = async |seen: &mut BTreeMap<String, Value>, at: &str| {
            let (line, promise) = written(&odoo, &f).await;
            seen.insert(format!("{at}/line"), line);
            seen.insert(format!("{at}/promise"), promise);
            seen.insert(
                format!("{at}/portfolio"),
                record(&http, &ingress_url, "PortfolioService", &id).await,
            );
            for (i, member) in members.iter().enumerate() {
                seen.insert(
                    format!("{at}/member{i}"),
                    record(&http, &ingress_url, "ActionPointService", &member.0).await,
                );
            }
        };

        // 1. A stale generation is refused and writes nothing.
        let (status, body) = post(&app, "/api/portfolios/promote", approve(&id, 1)).await;
        seen.insert("stale/status".to_owned(), json!(status.as_u16()));
        seen.insert("stale/body".to_owned(), body);
        snapshot(&mut seen, "stale").await;

        // 2. A clean approve writes both changes once and promotes every member.
        let (status, body) = post(&app, "/api/portfolios/promote", approve(&id, 2)).await;
        seen.insert("promote/status".to_owned(), json!(status.as_u16()));
        seen.insert("promote/body".to_owned(), body);
        snapshot(&mut seen, "promoted").await;

        // 3. The undo inside the window reverses both changes.
        let (status, body) = post(&app, "/api/portfolios/undo", undo(&id)).await;
        seen.insert("undo/status".to_owned(), json!(status.as_u16()));
        seen.insert("undo/body".to_owned(), body);
        snapshot(&mut seen, "undone").await;

        // 4. A partly reversed undo is recorded, and resumes once the value is put back.
        build_portfolio(&http, &ingress_url, &g, &partial_members, &partial_id).await?;
        let (status, body) = post(&app, "/api/portfolios/promote", approve(&partial_id, 2)).await;
        if status != StatusCode::OK {
            return Err(format!("the second promotion failed ({status}): {body}"));
        }
        let order = g.orders[0];
        let set_promise = async |value: &'static str| {
            odoo_do(&odoo, move |o| {
                o.write(
                    "sale.order",
                    json!([order]),
                    json!({ "commitment_date": value }),
                )
                .expect("edit the promise");
            })
            .await;
        };
        set_promise("2027-12-10 00:00:00").await;
        let partial_snapshot = async |seen: &mut BTreeMap<String, Value>, at: &str| {
            let (line, promise) = written(&odoo, &g).await;
            seen.insert(format!("{at}/line"), line);
            seen.insert(format!("{at}/promise"), promise);
            seen.insert(
                format!("{at}/portfolio"),
                record(&http, &ingress_url, "PortfolioService", &partial_id).await,
            );
            for (i, member) in partial_members.iter().enumerate() {
                seen.insert(
                    format!("{at}/member{i}"),
                    record(&http, &ingress_url, "ActionPointService", &member.0).await,
                );
            }
        };
        let (status, body) = post(&app, "/api/portfolios/undo", undo(&partial_id)).await;
        seen.insert("partial/status".to_owned(), json!(status.as_u16()));
        seen.insert("partial/body".to_owned(), body);
        partial_snapshot(&mut seen, "partial").await;
        set_promise(RE_PROMISED).await;
        let (status, body) = post(&app, "/api/portfolios/undo", undo(&partial_id)).await;
        seen.insert("resumed/status".to_owned(), json!(status.as_u16()));
        seen.insert("resumed/body".to_owned(), body);
        partial_snapshot(&mut seen, "resumed").await;
        Ok(seen)
    };
    let outcome = run().await;

    drop_fixture(&odoo, &f).await;
    drop_fixture(&odoo, &g).await;
    let deregistered = common::deregister_checked(&http, &admin_url, &callback_url).await;
    let seen = outcome.expect("the scenario ran");
    for (label, value) in &seen {
        println!("{label}: {value}");
    }
    deregistered.expect("the test deployment deregistered");

    // 1. Stale: 409, nothing written, nobody promoted.
    assert_eq!(seen["stale/status"], 409, "{}", seen["stale/body"]);
    assert!(
        seen["stale/body"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("generation 1 is stale")),
        "{}",
        seen["stale/body"]
    );
    assert_eq!(seen["stale/line"], json!(PLANNED));
    assert_eq!(seen["stale/promise"], json!(PROMISED));
    assert_eq!(seen["stale/portfolio"]["state"], "active");
    for i in 0..2 {
        assert_eq!(state_of(&seen[&format!("stale/member{i}")]), "proposed");
        assert!(seen[&format!("stale/member{i}")]["claim"].is_null());
    }

    // 2. Promoted: both changes written once, the portfolio promoted, every member promoted
    //    with the portfolio, its generation and the option named.
    assert_eq!(seen["promote/status"], 200, "{}", seen["promote/body"]);
    assert_eq!(seen["promoted/line"], json!(EXPEDITED));
    assert_eq!(seen["promoted/promise"], json!(RE_PROMISED));
    let portfolio = &seen["promoted/portfolio"];
    assert_eq!(portfolio["state"], "promoted", "{portfolio}");
    assert_eq!(portfolio["promotion"]["generation"], 2);
    assert_eq!(portfolio["promotion"]["rank_key"], "A");
    assert_eq!(
        portfolio["promotion"]["execution_plan"]["receipts"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "one write-back of both actions"
    );
    assert_eq!(
        events(portfolio).last().map(String::as_str),
        Some("promoted")
    );
    let supersessions = events(portfolio)
        .iter()
        .filter(|e| *e == "superseded")
        .count();
    assert_eq!(
        supersessions, 1,
        "only the staged one, none from the promotion"
    );
    for i in 0..2 {
        let member = &seen[&format!("promoted/member{i}")];
        assert_eq!(state_of(member), "promoted", "{member}");
        assert_eq!(member["promoted_via"]["portfolio"], id);
        assert_eq!(member["promoted_via"]["generation"], 2);
        assert_eq!(member["promoted_via"]["rank_key"], "A");
        assert!(member.get("portfolio").is_none_or(Value::is_null));
        let note = last_note(member);
        assert!(
            note.contains(&format!("portfolio {id} (generation 2), option A")),
            "{note}"
        );
    }

    // 3. Undone: both changes reversed, every member and the portfolio reversed.
    assert_eq!(seen["undo/status"], 200, "{}", seen["undo/body"]);
    assert_eq!(seen["undone/line"], json!(PLANNED));
    assert_eq!(seen["undone/promise"], json!(PROMISED));
    let portfolio = &seen["undone/portfolio"];
    assert_eq!(portfolio["state"], "reversed", "{portfolio}");
    assert_eq!(
        events(portfolio).last().map(String::as_str),
        Some("reversed")
    );
    for i in 0..2 {
        let member = &seen[&format!("undone/member{i}")];
        assert_eq!(state_of(member), "reversed", "{member}");
        assert_eq!(last_note(member), "the supplier cannot ship early");
    }

    // 4. Partial: 502 naming one remaining action; the expedite reversed, the edited promise
    //    kept; the portfolio and both members still promoted, the progress recorded.
    assert_eq!(seen["partial/status"], 502, "{}", seen["partial/body"]);
    assert!(
        seen["partial/body"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("1 of 2 action(s) remain unreversed")),
        "{}",
        seen["partial/body"]
    );
    assert_eq!(
        seen["partial/line"],
        json!(PLANNED),
        "the expedite is reversed"
    );
    assert_eq!(
        seen["partial/promise"],
        json!("2027-12-10 00:00:00"),
        "not clobbered"
    );
    let portfolio = &seen["partial/portfolio"];
    assert_eq!(portfolio["state"], "promoted", "{portfolio}");
    assert_eq!(
        portfolio["promotion"]["progress"]["reversed"],
        json!([0]),
        "{portfolio}"
    );
    assert_eq!(portfolio["promotion"]["progress"]["refused"][0]["index"], 1);
    assert_eq!(
        events(portfolio).last().map(String::as_str),
        Some("partially_reversed")
    );
    for i in 0..2 {
        let member = &seen[&format!("partial/member{i}")];
        assert_eq!(state_of(member), "promoted", "{member}");
        assert!(member["claim"].is_null(), "member {i} released: {member}");
    }

    // Resumed: only the re-promise is reversed now, and everything is recorded reversed.
    assert_eq!(seen["resumed/status"], 200, "{}", seen["resumed/body"]);
    assert_eq!(seen["resumed/line"], json!(PLANNED));
    assert_eq!(seen["resumed/promise"], json!(PROMISED));
    let portfolio = &seen["resumed/portfolio"];
    assert_eq!(portfolio["state"], "reversed", "{portfolio}");
    assert_eq!(
        portfolio["promotion"]["progress"]["reversed"],
        json!([0, 1])
    );
    for i in 0..2 {
        assert_eq!(state_of(&seen[&format!("resumed/member{i}")]), "reversed");
    }
}
