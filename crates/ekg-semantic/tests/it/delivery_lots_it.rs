//! Gated live test of lot-level delivery over a throwaway GraphDB fixture: sales order S00001
//! promised 20 Oct for 40 bikes, fulfilled by one manufacturing lot of 40 (16 Oct 08:00 to
//! 17 Oct 08:00); 30 controllers on hand, the other 10 on a purchase line arriving 21 Oct,
//! after the promise.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-semantic --test it delivery_lots_it -- --ignored
//! ```
//!
//! - `order-lots.rq` reads the lot and its component row;
//! - doing nothing leaves all 40 units late (the lot waits for the 21 Oct arrival, completes
//!   22 Oct: late by 2 days);
//! - a projected split (30 now, 10 from 18 Oct) with an expedite of the missing 10 to 18 Oct
//!   leaves no unit late, and the order is on time.
//!
//! Skips (does not fail) when the store is unreachable.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use ekg_domain::{ActionType, EntityUri, Plan, ProposedAction, Scorer, Simulator, WorldModel};
use ekg_semantic::display::{self, Delivery, WorldSource, queries};
use ekg_semantic::{
    GraphDbClient, GraphDbSimulator, GraphDbWorldModel, SupplyRiskScorer, ontology,
};

use crate::common;
use common::{graphdb_url, provision_shacl_repo, rec};

const REPO: &str = "ekg_delivery_lots_it";

fn fixtures() -> Vec<ekg_domain::SourceRecord> {
    vec![
        rec(
            "sale.order",
            1,
            &[
                ("name", "S00001"),
                ("commitment_date", "2027-10-26T00:00:00"),
                ("order_line", "[11]"),
            ],
        ),
        rec(
            "sale.order.line",
            11,
            &[("product_id", "[100]"), ("product_uom_qty", "40")],
        ),
        rec("product.product", 100, &[("name", "City E-Bike S1")]),
        rec("product.product", 101, &[("name", "Motor Controller")]),
        rec(
            "mrp.bom",
            50,
            &[("product_id", "[100]"), ("bom_line_ids", "[51]")],
        ),
        rec(
            "mrp.bom.line",
            51,
            &[("product_id", "[101]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.production",
            60,
            &[
                ("name", "WH/MO/00060"),
                ("product_id", "[100]"),
                ("bom_id", "[50]"),
                ("product_qty", "40"),
                ("origin", "S00001"),
                ("date_start", "2027-10-22T08:00:00"),
                ("date_finished", "2027-10-23T08:00:00"),
            ],
        ),
        rec(
            "stock.quant",
            80,
            &[("product_id", "[101]"), ("quantity", "30")],
        ),
        rec(
            "purchase.order",
            90,
            &[
                ("name", "P00091"),
                ("order_line", "[91]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            91,
            &[
                ("product_id", "[101]"),
                ("product_qty", "10"),
                ("date_planned", "2027-10-27T08:00:00"),
            ],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn an_order_fulfilled_by_lots_is_judged_lot_by_lot() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("driver client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    for record in fixtures() {
        world.materialize(&record).await.expect("materialize");
    }
    let client = GraphDbClient::new(&base, REPO).expect("query client");
    for derivation in ontology::STRUCTURAL_DERIVATIONS {
        client.update(derivation.sparql).await.expect("derive");
    }
    let order = EntityUri(ontology::entity_iri("sale.order", 1));
    let current = WorldSource(&world);
    let simulator = GraphDbSimulator::new(GraphDbClient::new(&base, REPO).expect("sim client"));
    let action = |verb: &str, target: String, params: &[(&str, &str)]| ProposedAction {
        action_type: ActionType(verb.to_owned()),
        target: EntityUri(target),
        params: params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    };
    let split_and_expedite = Plan {
        actions: vec![
            action(
                "split_production",
                ontology::entity_iri("mrp.production", 60),
                &[
                    ("split_quantity", "10"),
                    ("new_date", "2027-10-24 08:00:00"),
                ],
            ),
            action(
                "expedite_po",
                ontology::entity_iri("purchase.order.line", 91),
                &[("new_date", "2027-10-24 08:00:00")],
            ),
        ],
    };

    let run = async || {
        let lots = client
            .select(&queries::order_lots_query(Some(&order)).expect("canonical"))
            .await
            .expect("order lots");
        let baseline = display::outcome(&current, &current, &order)
            .await
            .expect("baseline outcome");
        let baseline_score = SupplyRiskScorer::new()
            .score(&current)
            .await
            .expect("baseline score");
        let projection = simulator
            .project(&split_and_expedite)
            .await
            .expect("project the split and the expedite");
        let projected = display::outcome(&current, projection.as_ref(), &order)
            .await
            .expect("projected outcome");
        let projected_score = SupplyRiskScorer::new()
            .score(projection.as_ref())
            .await
            .expect("projected score");
        drop(projection);
        (lots, baseline, baseline_score, projected, projected_score)
    };
    let (lots, baseline, baseline_score, projected, projected_score) = run().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;

    println!(
        "lots {lots:?}\nbaseline {baseline:?} {baseline_score:?}\nprojected {projected:?} {projected_score:?}"
    );
    // The lot and its component row.
    assert_eq!(lots.len(), 1, "{lots:?}");
    let row = &lots[0];
    assert_eq!(
        row.get("mo").map(String::as_str),
        Some(ontology::entity_iri("mrp.production", 60).as_str())
    );
    assert_eq!(
        row.get("qty").and_then(|q| q.parse::<f64>().ok()),
        Some(40.0)
    );
    assert_eq!(
        row.get("comp").map(String::as_str),
        Some(ontology::entity_iri("product.product", 101).as_str())
    );
    assert_eq!(
        row.get("perUnit").and_then(|q| q.parse::<f64>().ok()),
        Some(1.0)
    );
    assert_eq!(
        row.get("onHand").and_then(|q| q.parse::<f64>().ok()),
        Some(30.0)
    );

    // Doing nothing: all 40 late, the order late by 2 days.
    let baseline = baseline.expect("a promised order");
    assert_eq!(baseline.delivery, Delivery::Late);
    assert_eq!(baseline.completion.as_deref(), Some("2027-10-28T08:00:00"));
    assert_eq!(baseline.slip, Some(2));
    let metric = |score: &ekg_domain::Score, key: &str| score.metrics.get(key).cloned();
    assert_eq!(metric(&baseline_score, "late_units").as_deref(), Some("40"));
    assert_eq!(metric(&baseline_score, "late_orders").as_deref(), Some("1"));

    // The split with the expedite: no unit late, on time.
    let projected = projected.expect("a promised order");
    assert_eq!(projected.delivery, Delivery::OnTime, "{projected:?}");
    assert_eq!(metric(&projected_score, "late_units").as_deref(), Some("0"));
    assert_eq!(
        metric(&projected_score, "late_orders").as_deref(),
        Some("0")
    );
    assert!(projected_score.rank_key < baseline_score.rank_key);
}
