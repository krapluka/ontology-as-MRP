//! Gated live e2e: the inbox's plans read over a live Restate and GraphDB. A portfolio is
//! listed as one plan with its members, generation, scope and opened time; a member's plan
//! read serves the joint slate; and, once the portfolio is promoted, the approved plans read
//! lists it once with its undo window.
//!
//! The portfolio is built deterministically through the objects' own handlers (served
//! in-process on port offset 21), over typed fixtures in a throwaway GraphDB repo. Its
//! recommended option re-promises a sales order, moves a manufacturing order's two work
//! orders on the Assembly and Paint work centers, and expedites one purchase-order line, so
//! its scope reads 2 orders, 2 work centers, 1 purchase order. The promotion runs through the
//! real portfolio command with a recording executor (the kinetic write-back is covered by
//! `portfolio_promote_e2e_it`). It checks:
//!
//! 1. `plans` lists the portfolio once: the plan id is the portfolio's digest, with both
//!    members and their state, the generation, the coupling reason as the situation, a
//!    recorded opened time, and the recommended option's scope;
//! 2. `plan(member)` serves the joint slate, resolving both members, framed through a lead;
//! 3. after a portfolio promote, `approvedPlans` lists it once (not its members) with the
//!    approved option, the reviewer and an undo window of `promotedAt` plus the window.
//!
//! Ignored by default; skips if Restate or GraphDB is unreachable, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), in which case it fails.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it plans_read_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ekg_backend::action_point::ApiState;
use ekg_backend::graphql::{self, GraphqlState, PolicyDisplay};
use ekg_domain::fakes::RecordingActionExecutor;
use ekg_domain::{
    ActionExecutor, ActionType, EntityUri, GroundedProposal, Plan, Proposal, ProposedAction,
    RankedPlan, Score, Simulator, Slate, Trace, WorldModel, short_digest,
};
use ekg_dynamic::{ActionPointService, PortfolioService, portfolio_id};
use ekg_semantic::{GraphDbClient, GraphDbSimulator, GraphDbWorldModel, ontology};
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{provision_repo, rec};

const OFFSET: u16 = 21;
const REPO: &str = "ekg_plans_read_e2e_it";
const WINDOW_SECS: u64 = 8 * 60 * 60;
const LINKED: &str = "the order waits on the manufacturing order";

/// The run's fixture ids, all offset from one unique base.
struct Ids {
    order: i64,
    production: i64,
    assembly: i64,
    paint: i64,
    wo_assembly: i64,
    wo_paint: i64,
    po: i64,
    line: i64,
}

impl Ids {
    fn new(base: i64) -> Self {
        Self {
            order: base,
            production: base,
            assembly: base + 1,
            paint: base + 2,
            wo_assembly: base + 3,
            wo_paint: base + 4,
            po: base,
            line: base + 5,
        }
    }

    fn iri(model: &str, id: i64) -> EntityUri {
        EntityUri(ontology::entity_iri(model, id))
    }

    /// The members, sorted (a manufacturing order's IRI sorts before a sales order's).
    fn members(&self) -> [EntityUri; 2] {
        [
            Self::iri("mrp.production", self.production),
            Self::iri("sale.order", self.order),
        ]
    }
}

/// The typed fixtures: a sales order; a manufacturing order with a work order on each of two
/// work centers; a purchase order with one line.
fn fixtures(ids: &Ids) -> Vec<ekg_domain::SourceRecord> {
    let r = |id: i64| format!("[{id}]");
    vec![
        rec("sale.order", ids.order, &[("name", "S-PLANS")]),
        rec("mrp.production", ids.production, &[("name", "MO-PLANS")]),
        rec("mrp.workcenter", ids.assembly, &[("name", "Assembly")]),
        rec("mrp.workcenter", ids.paint, &[("name", "Paint")]),
        rec(
            "mrp.workorder",
            ids.wo_assembly,
            &[
                ("production_id", &r(ids.production)),
                ("workcenter_id", &r(ids.assembly)),
                ("date_start", "2027-10-19T08:00:00"),
                ("date_finished", "2027-10-19T12:00:00"),
            ],
        ),
        rec(
            "mrp.workorder",
            ids.wo_paint,
            &[
                ("production_id", &r(ids.production)),
                ("workcenter_id", &r(ids.paint)),
                ("date_start", "2027-10-20T08:00:00"),
                ("date_finished", "2027-10-20T12:00:00"),
            ],
        ),
        rec(
            "purchase.order",
            ids.po,
            &[("name", "P-PLANS"), ("order_line", &r(ids.line))],
        ),
        rec(
            "purchase.order.line",
            ids.line,
            &[
                ("product_qty", "10"),
                ("date_planned", "2027-10-26T08:00:00"),
            ],
        ),
    ]
}

fn action(verb: &str, target: EntityUri, new_date: &str) -> ProposedAction {
    ProposedAction {
        action_type: ActionType(verb.to_owned()),
        target,
        params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
    }
}

/// The joint slate: `A` (recommended) re-promises the order, moves both work orders and
/// expedites the line; `B` only re-promises the order.
fn joint_slate(ids: &Ids) -> Slate {
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
    let order = Ids::iri("sale.order", ids.order);
    Slate {
        baseline: Score {
            metrics: BTreeMap::new(),
            rank_key: "Z".to_owned(),
        },
        options: vec![
            option(
                "A",
                vec![
                    action("re_promise_date", order.clone(), "2027-10-30 00:00:00"),
                    action(
                        "reschedule_work_order",
                        Ids::iri("mrp.workorder", ids.wo_assembly),
                        "2027-10-21 08:00:00",
                    ),
                    action(
                        "reschedule_work_order",
                        Ids::iri("mrp.workorder", ids.wo_paint),
                        "2027-10-22 08:00:00",
                    ),
                    action(
                        "expedite_po",
                        Ids::iri("purchase.order.line", ids.line),
                        "2027-10-18 08:00:00",
                    ),
                ],
            ),
            option(
                "B",
                vec![action("re_promise_date", order, "2027-10-30 00:00:00")],
            ),
        ],
        trace: Trace::default(),
    }
}

/// POST `body` to `handler` on the `service` object keyed `key`.
async fn call(
    http: &reqwest::Client,
    ingress: &str,
    service: &str,
    key: &str,
    handler: &str,
    body: Value,
) -> Result<(), String> {
    let mut url = reqwest::Url::parse(ingress).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|()| "base url".to_owned())?
        .pop_if_empty()
        .extend([service, key, handler]);
    let resp = http
        .post(url)
        .timeout(Duration::from_secs(30))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("{service}/{handler} returned {status}: {text}"));
    }
    Ok(())
}

/// Both points proposed and members of the portfolio at generation 1, which carries the
/// joint slate.
async fn build_portfolio(
    http: &reqwest::Client,
    ingress: &str,
    ids: &Ids,
    id: &str,
) -> Result<(), String> {
    let members = ids.members();
    for member in &members {
        call(
            http,
            ingress,
            "ActionPointService",
            &member.0,
            "open",
            json!({ "reason": "late against the promise", "revision": 1 }),
        )
        .await?;
        let grounded = GroundedProposal {
            proposal: Proposal::single(
                "re-promise the order".to_owned(),
                action(
                    "re_promise_date",
                    Ids::iri("sale.order", ids.order),
                    "2027-10-30 00:00:00",
                ),
                Vec::new(),
            ),
            grounding_omissions: Vec::new(),
            grounding_edges: Vec::new(),
            subject_members: vec![Ids::iri("sale.order", ids.order)],
        };
        call(
            http,
            ingress,
            "ActionPointService",
            &member.0,
            "attach_proposal",
            serde_json::to_value(&grounded).map_err(|e| e.to_string())?,
        )
        .await?;
    }
    call(
        http,
        ingress,
        "PortfolioService",
        id,
        "form",
        json!({ "members": members, "reason": LINKED }),
    )
    .await?;
    call(
        http,
        ingress,
        "PortfolioService",
        id,
        "attach_slate",
        json!({ "generation": 1, "slate": joint_slate(ids) }),
    )
    .await?;
    for member in &members {
        call(
            http,
            ingress,
            "ActionPointService",
            &member.0,
            "set_portfolio",
            json!({ "portfolio": id }),
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn the_plans_read_lists_a_portfolio_and_the_approved_read_lists_its_promotion() {
    // One test deployment per binary at a time (see `common::deployment_slot`), then one
    // throwaway repo at a time (GraphDB Free's repository cap).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
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

    let ids = Ids::new(common::unique_base());
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    for record in fixtures(&ids) {
        world.materialize(&record).await.expect("materialize");
    }
    let members = ids.members();
    let id = portfolio_id(&members).0;
    let digest = id.strip_prefix("portfolio/").expect("prefixed").to_owned();

    let endpoint = Endpoint::builder()
        .bind(ActionPointService::default())
        .bind(PortfolioService::default())
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let registered = common::register(&http, &admin_url, &callback_url).await;

    let reads = graphql::router(GraphqlState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        world: Arc::clone(&world) as Arc<dyn WorldModel>,
        simulator: Arc::new(GraphDbSimulator::new(
            GraphDbClient::new(&graphdb_url, REPO).expect("sim client"),
        )) as Arc<dyn Simulator>,
        rules: ekg_backend::drift::monitor_rules(),
        policy: PolicyDisplay {
            policy_id: "local-autopromote/v1".to_owned(),
            cost_limit: Some(250.0),
            cost_limit_currency: Some("USD".to_owned()),
            confirmed_states: Vec::new(),
            auto_apply_actions: Vec::new(),
            undo_window: Duration::from_secs(WINDOW_SECS),
        },
    });
    let commands = ekg_backend::portfolio_command::router(ApiState {
        http: http.clone(),
        ingress_url: ingress_url.clone(),
        admin_url: admin_url.clone(),
        executor: Arc::new(RecordingActionExecutor::default()) as Arc<dyn ActionExecutor>,
        undo_window: Duration::from_secs(WINDOW_SECS),
        claim_deadline: ekg_backend::config::DEFAULT_COMMAND_CLAIM,
    });

    let started = now_secs();
    let scenario = async || -> Result<BTreeMap<String, Value>, String> {
        registered.clone()?;
        build_portfolio(&http, &ingress_url, &ids, &id).await?;
        let mut seen = BTreeMap::new();
        let (status, body) = post(
            &reads,
            "/api/graphql",
            json!({ "query": "{ plans { id resolves { subject state digest } \
                portfolio { id generation } openedAt situation \
                plan { lead resolves options { rankKey recommended scope { kind count } } } } }" }),
        )
        .await;
        seen.insert("plans/status".to_owned(), json!(status.as_u16()));
        seen.insert("plans".to_owned(), body);
        let (status, body) = post(
            &reads,
            "/api/graphql",
            json!({ "query": format!(
                "{{ plan(subject: \"{}\") {{ subject lead resolves options {{ rankKey }} }} }}",
                members[1].0
            ) }),
        )
        .await;
        seen.insert("member/status".to_owned(), json!(status.as_u16()));
        seen.insert("member".to_owned(), body);
        let (status, body) = post(
            &commands,
            "/api/portfolios/promote",
            json!({
                "portfolio": id,
                "generation": 1,
                "option_rank_key": "A",
                "reviewer": "planner-1",
                "rationale": "approve plan A: it brings the order in",
            }),
        )
        .await;
        seen.insert("promote/status".to_owned(), json!(status.as_u16()));
        seen.insert("promote".to_owned(), body);
        let (status, body) = post(
            &reads,
            "/api/graphql",
            json!({ "query": "{ approvedPlans(since: \"0\") { id resolves { subject } \
                portfolio { id generation } rankKey reviewer promotedAt undoUntil remaining \
                changes { verb } } }" }),
        )
        .await;
        seen.insert("approved/status".to_owned(), json!(status.as_u16()));
        seen.insert("approved".to_owned(), body);
        Ok(seen)
    };
    let outcome = scenario().await;
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    let deregistered = common::deregister_checked(&http, &admin_url, &callback_url).await;
    let seen = outcome.expect("the scenario ran");
    deregistered.expect("the test deployment deregistered");
    let finished = now_secs();

    // 1. The plans read lists the portfolio once, as one plan.
    assert_eq!(seen["plans/status"], 200, "{}", seen["plans"]);
    assert!(seen["plans"].get("errors").is_none(), "{}", seen["plans"]);
    let plan_id = format!("plan/{digest}");
    let entries: Vec<&Value> = seen["plans"]["data"]["plans"]
        .as_array()
        .expect("listed")
        .iter()
        .filter(|entry| entry["id"] == plan_id)
        .collect();
    assert_eq!(entries.len(), 1, "listed once: {}", seen["plans"]);
    let entry = entries[0];
    println!("plan entry: {entry}");
    assert_eq!(entry["portfolio"], json!({ "id": id, "generation": 1 }));
    let resolved: Vec<&str> = entry["resolves"]
        .as_array()
        .expect("resolves")
        .iter()
        .filter_map(|p| p["subject"].as_str())
        .collect();
    assert_eq!(resolved, [members[0].0.as_str(), members[1].0.as_str()]);
    for point in entry["resolves"].as_array().expect("resolves") {
        assert_eq!(point["state"], "PROPOSED", "{point}");
        let subject = point["subject"].as_str().expect("subject");
        assert_eq!(point["digest"], short_digest(subject.as_bytes()));
    }
    assert_eq!(entry["situation"], LINKED);
    let opened: u64 = entry["openedAt"]
        .as_str()
        .and_then(|at| at.parse().ok())
        .expect("an opened time");
    assert!(
        (started..=finished).contains(&opened),
        "opened {opened} within the run [{started}, {finished}]"
    );
    let plan = &entry["plan"];
    assert_eq!(
        plan["resolves"],
        json!([members[0].0, members[1].0]),
        "{plan}"
    );
    let recommended = plan["options"]
        .as_array()
        .expect("options")
        .iter()
        .find(|o| o["recommended"] == true)
        .expect("a recommended option");
    assert_eq!(recommended["rankKey"], "A");
    assert_eq!(
        recommended["scope"],
        json!([
            { "kind": "order", "count": 2 },
            { "kind": "work center", "count": 2 },
            { "kind": "purchase order", "count": 1 },
        ]),
        "2 orders, 2 work centers, 1 purchase order"
    );

    // 2. A member's plan read serves the joint slate.
    assert_eq!(seen["member/status"], 200, "{}", seen["member"]);
    let member_plan = &seen["member"]["data"]["plan"];
    assert_eq!(member_plan["subject"], members[1].0);
    assert_eq!(
        member_plan["resolves"],
        json!([members[0].0, members[1].0]),
        "{member_plan}"
    );
    assert_eq!(
        member_plan["options"],
        json!([{ "rankKey": "A" }, { "rankKey": "B" }])
    );
    assert!(
        member_plan["lead"]
            .as_str()
            .is_some_and(|lead| members.iter().any(|m| m.0 == lead)),
        "{member_plan}"
    );

    // 3. After the promotion, the approved read lists the portfolio once, with its window.
    assert_eq!(seen["promote/status"], 200, "{}", seen["promote"]);
    assert_eq!(seen["approved/status"], 200, "{}", seen["approved"]);
    let approved = seen["approved"]["data"]["approvedPlans"]
        .as_array()
        .expect("listed");
    let ours: Vec<&Value> = approved.iter().filter(|a| a["id"] == plan_id).collect();
    assert_eq!(ours.len(), 1, "listed once: {}", seen["approved"]);
    let member_ids: Vec<String> = members
        .iter()
        .map(|m| format!("plan/{}", short_digest(m.0.as_bytes())))
        .collect();
    assert!(
        !approved.iter().any(|a| a["id"]
            .as_str()
            .is_some_and(|i| member_ids.iter().any(|m| m == i))),
        "its members are not listed on their own"
    );
    let ours = ours[0];
    println!("approved: {ours}");
    assert_eq!(ours["portfolio"], json!({ "id": id, "generation": 1 }));
    assert_eq!(ours["rankKey"], "A");
    assert_eq!(ours["reviewer"], "planner-1");
    assert_eq!(ours["remaining"], 4);
    assert_eq!(ours["changes"].as_array().map(Vec::len), Some(4));
    let promoted: u64 = ours["promotedAt"]
        .as_str()
        .and_then(|at| at.parse().ok())
        .expect("promotedAt");
    assert_eq!(
        ours["undoUntil"],
        (promoted + WINDOW_SECS).to_string(),
        "the window from when it was promoted"
    );
}
