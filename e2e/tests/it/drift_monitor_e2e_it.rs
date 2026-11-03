//! Gated live end-to-end test: the durable drift monitor opens an action point when
//! the deterministic rule flags a sales order off-plan, keeps a single open point per
//! subject under redelivery, ignores a stale event, and opens nothing for a covered
//! order.
//!
//! Ignored by default. Requires GraphDB and Restate with the worker endpoint
//! reachable from the runtime. Odoo is not needed: the typed fixtures are
//! materialized directly into a throwaway repo.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it drift_monitor_e2e_it -- --ignored --nocapture
//! ```
//!
//! If any hop is unreachable the test skips instead of failing.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::drift::{self, supply_risk_rule};
use ekg_domain::{
    ChangeKind, EntityUri, NamedGraph, RdfChanged, Revision, SourceRecord, SourceRef, WorldModel,
};
use ekg_dynamic::{ActionPointService, DriftMonitorService};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};
use restate_sdk::prelude::Endpoint;
use serde_json::json;

const REPO: &str = "ekg_drift_monitor_e2e_it";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn rec(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
    SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        },
        fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

/// The flagship at-risk scenario (sale.order/1, shortfall 13 on the Battery) plus
/// a covered negative control (sale.order/2), as typed source records. The
/// semantic-layer risk-rule test uses the same fixtures.
fn fixtures(at_risk_id: i64, covered_id: i64) -> Vec<SourceRecord> {
    vec![
        rec(
            "sale.order",
            at_risk_id,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[10]"),
            ],
        ),
        rec(
            "sale.order.line",
            10,
            &[("product_id", "[100]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 100, &[("name", "E-Bike")]),
        rec(
            "product.product",
            101,
            &[("name", "Battery"), ("default_code", "BAT")],
        ),
        rec(
            "mrp.bom",
            20,
            &[("product_id", "[100]"), ("bom_line_ids", "[30]")],
        ),
        rec(
            "mrp.bom.line",
            30,
            &[("product_id", "[101]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            40,
            &[("product_id", "[101]"), ("quantity", "2")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            50,
            &[("product_id", "[101]"), ("product_min_qty", "5")],
        ),
        rec(
            "purchase.order.line",
            60,
            &[
                ("product_id", "[101]"),
                ("product_qty", "10"),
                ("date_planned", "2027-12-20T00:00:00"),
            ],
        ),
        rec(
            "sale.order",
            covered_id,
            &[
                ("commitment_date", "2027-12-07T00:00:00"),
                ("order_line", "[11]"),
            ],
        ),
        rec(
            "sale.order.line",
            11,
            &[("product_id", "[102]"), ("product_uom_qty", "1")],
        ),
        rec("product.product", 102, &[("name", "Gadget")]),
        rec("product.product", 103, &[("name", "Widget")]),
        rec(
            "mrp.bom",
            21,
            &[("product_id", "[102]"), ("bom_line_ids", "[31]")],
        ),
        rec(
            "mrp.bom.line",
            31,
            &[("product_id", "[103]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            41,
            &[("product_id", "[103]"), ("quantity", "100")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            51,
            &[("product_id", "[103]"), ("product_min_qty", "5")],
        ),
    ]
}

/// Create a fresh throwaway repo from GraphDB's default template (no SHACL, the
/// rule needs none). `Ok(false)` means the store is unreachable and the test skips.
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
    cfg["title"] = serde_json::Value::from("EKG drift-monitor e2e IT (throwaway)");
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

/// A `RdfChanged` for a materialized `sale.order` at a given revision.
fn sale_order_changed(id: i64, revision: u64) -> RdfChanged {
    let source = SourceRef {
        system: "odoo".to_owned(),
        model: "sale.order".to_owned(),
        id,
    };
    RdfChanged {
        entity_uri: EntityUri(ontology::entity_iri("sale.order", id)),
        named_graph: NamedGraph(ontology::named_graph("sale.order", id)),
        change_kind: ChangeKind::Updated,
        source,
        revision: Revision(revision),
    }
}

/// Fire-and-forget a change event at the durable monitor through the ingress.
async fn notify(http: &reqwest::Client, ingress: &str, event: &RdfChanged) {
    let url = drift::monitor_notify_url(ingress).expect("notify url");
    let body = serde_json::to_vec(event).expect("encode event");
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .expect("post notify");
    assert!(
        resp.status().is_success(),
        "monitor rejected notify: {}",
        resp.status()
    );
}

/// Build the ingress URL that calls the action-point object's `get` handler.
fn action_point_get_url(ingress: &str, key: &str) -> String {
    let mut url = reqwest::Url::parse(ingress).expect("ingress url");
    url.path_segments_mut()
        .expect("base url")
        .pop_if_empty()
        .extend(["ActionPointService", key, "get"]);
    url.into()
}

/// The current lifecycle state of the subject's action point. `None` if the object
/// has never opened one: the covered case returns JSON `null`.
async fn point_state(http: &reqwest::Client, ingress: &str, subject_iri: &str) -> Option<String> {
    let url = action_point_get_url(ingress, subject_iri);
    let resp = http.post(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
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
async fn monitor_opens_one_action_point_on_drift_and_none_when_covered() {
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(1);
    let callback_url = common::callback_url(1);

    let http = reqwest::Client::new();

    // --- guards: skip if the substrate is not up ---
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

    // --- materialize the typed fixtures into the throwaway repo ---
    let seed = GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("seed client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    // Unique subject ids per run, so the persistent, subject-keyed ActionPointService
    // state does not collide with a prior run or a sibling test (see common::unique_base).
    let base = common::unique_base();
    let at_risk_id = base + 1;
    let covered_id = base + 2;
    for record in fixtures(at_risk_id, covered_id) {
        seed.materialize(&record)
            .await
            .expect("materialize fixture");
    }

    // --- serve the real monitor and action-point object; register with Restate ---
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("monitor client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let monitor = DriftMonitorService::new(
        Arc::clone(&world) as Arc<dyn WorldModel>,
        vec![supply_risk_rule()],
    );
    let action_point = ActionPointService::default();
    let endpoint = Endpoint::builder().bind(monitor).bind(action_point).build();
    // The listener is bound first, so a port collision fails here and not as a later
    // timeout, and the socket is listening before we register.
    common::spawn_endpoint(endpoint, &service_addr).await;
    let register = http
        .post(format!("{admin_url}/deployments"))
        .header("Content-Type", "application/json")
        .body(json!({ "uri": callback_url, "force": true }).to_string())
        .send()
        .await
        .expect("register deployment");
    assert!(
        register.status().is_success(),
        "Restate rejected registration ({}): is {callback_url} reachable from the runtime?",
        register.status()
    );

    let at_risk = ontology::entity_iri("sale.order", at_risk_id);
    let covered = ontology::entity_iri("sale.order", covered_id);

    // --- drift: the monitor opens an action point for the at-risk order ---
    notify(&http, &ingress_url, &sale_order_changed(at_risk_id, 1)).await;
    let opened = wait_until(Duration::from_secs(30), async || {
        point_state(&http, &ingress_url, &at_risk).await.as_deref() == Some("open")
    })
    .await;
    assert!(
        opened,
        "the monitor did not open an action point for the at-risk order"
    );

    // --- idempotency: redelivered (newer) and stale (same/older) events keep one
    //     open point; the covered order still has none ---
    notify(&http, &ingress_url, &sale_order_changed(at_risk_id, 2)).await; // redelivered, newer
    notify(&http, &ingress_url, &sale_order_changed(at_risk_id, 1)).await; // stale, ignored
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        point_state(&http, &ingress_url, &at_risk).await.as_deref(),
        Some("open"),
        "the action point should remain a single open point under redelivery"
    );

    // --- covered order: the monitor opens nothing ---
    notify(&http, &ingress_url, &sale_order_changed(covered_id, 1)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let covered_state = point_state(&http, &ingress_url, &covered).await;

    // --- cleanup before asserting ---
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;

    assert!(
        covered_state.is_none(),
        "a covered order must not open an action point (got {covered_state:?})"
    );
}
