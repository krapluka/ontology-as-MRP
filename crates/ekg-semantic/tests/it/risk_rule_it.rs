//! Gated live integration test: the deterministic risk rule fires over typed
//! entities materialized through the mapping contract.
//!
//! Ignored by default. Run against a reachable GraphDB, e.g. the local substrate
//! (`just podman-local-up`, which supplies the license), with:
//!
//! ```text
//! EKG_E2E_REQUIRE_INFRA=1 cargo test -p ekg-semantic -- --ignored
//! ```
//!
//! Seed-shaped records are projected into typed entities by the contract, then the
//! authored `RISK_RULE_SPARQL` runs over the live store. The at-risk order must be
//! flagged with the correct shortfall; a covered order must stay clear. If the store
//! is unreachable the test skips, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as
//! `just test-e2e` does), in which case it fails.

// `expect`/`panic` are the assertion mechanism here; prose names products.
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{EntityUri, SourceRecord, WorldModel};
use ekg_semantic::tabs::{self, Day, Tab};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

use crate::common;
use common::{graphdb_url, infra_unavailable, provision_repo, rec};

const REPO: &str = "ekg_risk_rule_it";
const REPO_SHIP: &str = "ekg_risk_rule_ship_it";
const REPO_DONE: &str = "ekg_risk_rule_done_it";
const REPO_CAUSE: &str = "ekg_risk_rule_cause_it";
const REPO_TREE: &str = "ekg_risk_rule_tree_it";

/// An at-risk scenario plus an independent covered (negative-control) scenario, as
/// typed source records. At-risk: VeloMax orders 10 E-Bikes; each needs 1 Battery;
/// on-hand 2, safety 5, and the only inbound PO of 10 lands after the promise, so
/// the shortfall is (10 + 5) - 2 = 13. Covered: a Gadget order whose Widget
/// component is amply stocked, so no shortfall.
fn fixtures() -> Vec<SourceRecord> {
    vec![
        // at-risk: sale.order/1, E-Bike(100), BOM/20, Battery(101)
        rec(
            "sale.order",
            1,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[10]"),
            ],
        ),
        // Fields are keyed by Odoo source name; the contract maps product_id ->
        // ekg:line_product, order_line -> ekg:has_line_of, etc.
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
        // covered: sale.order/2, Gadget(102), BOM/21, Widget(103), amply stocked
        rec(
            "sale.order",
            2,
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

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn risk_rule_fires_on_live_typed_entities() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!(
                "no GraphDB reachable at {base} (set GRAPHDB_URL or start the substrate)"
            ));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    // Materialize every fixture as a typed entity through the mapping contract.
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }

    // Run the authored risk rule. It is a CONSTRUCT; execute its WHERE logic as an
    // INSERT overlay (only the leading keyword is swapped, `CONSTRUCT {` for
    // `INSERT {`, leaving the query's comments untouched) so the flags are queryable
    // over the live typed data.
    let client = GraphDbClient::new(&base, REPO).expect("build query client");
    let rule_insert = ontology::RISK_RULE_SPARQL.replacen("CONSTRUCT {", "INSERT {", 1);
    client.update(&rule_insert).await.expect("run risk rule");

    // The at-risk sales order is flagged with the correct shortfall; the covered
    // order is absent from the result set (the negative control stays clear).
    let rows = client
        .select(&format!(
            "PREFIX ekg: <{vocab}>\n\
             SELECT ?so ?sf WHERE {{ ?so ekg:at_risk true ; ekg:has_risk ?r . ?r ekg:shortfall ?sf }}",
            vocab = ontology::VOCAB_NAMESPACE,
        ))
        .await
        .expect("select at-risk flags");

    let at_risk = ontology::entity_iri("sale.order", 1);
    let covered = ontology::entity_iri("sale.order", 2);
    assert_eq!(
        rows.len(),
        1,
        "exactly the at-risk order is flagged: {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(row.get("so").map(String::as_str), Some(at_risk.as_str()));
    let shortfall: f64 = row
        .get("sf")
        .and_then(|s| s.parse::<f64>().ok())
        .expect("shortfall is numeric");
    assert!(
        (shortfall - 13.0).abs() < f64::EPSILON,
        "shortfall is 13, got {shortfall}"
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.get("so").map(String::as_str) == Some(covered.as_str())),
        "the covered order is not flagged at risk"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;
}

/// Two orders whose components would be covered by an on-time inbound PO, each with
/// a delivering shipment (stock.picking) that overrides the PO line's planned date:
///
/// - order/3 (Cart, component Chip): PO of 10 lands 2027-09-07 (before the 2027-10-19
///   promise, so PO-only would cover), but the delivering shipment's ETA slips to 2027, so
///   the inbound drops out: shortfall 10, at risk.
/// - order/4 (Dock, component Bolt): identical, but the shipment ETA is on time
///   (2027-10-07, not after the promise): covered, not at risk.
///
/// This isolates the shipment-ETA override: same PO planned dates, opposite outcomes
/// driven only by the shipment's ETA.
fn shipment_fixtures() -> Vec<SourceRecord> {
    vec![
        // order/3: slipped shipment, at risk
        rec(
            "sale.order",
            3,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[70]"),
            ],
        ),
        rec(
            "sale.order.line",
            70,
            &[("product_id", "[110]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 110, &[("name", "Cart")]),
        rec("product.product", 111, &[("name", "Chip")]),
        rec(
            "mrp.bom",
            22,
            &[("product_id", "[110]"), ("bom_line_ids", "[80]")],
        ),
        rec(
            "mrp.bom.line",
            80,
            &[("product_id", "[111]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            81,
            &[("product_id", "[111]"), ("quantity", "0")],
        ),
        rec(
            "purchase.order.line",
            71,
            &[
                ("product_id", "[111]"),
                ("product_qty", "10"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        rec("purchase.order", 72, &[("order_line", "[71]")]),
        rec(
            "stock.picking",
            73,
            &[
                ("scheduled_date", "2027-12-20T00:00:00"),
                ("purchase_id", "[72]"),
                ("state", "delayed"),
            ],
        ),
        // order/4: on-time shipment, covered
        rec(
            "sale.order",
            4,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[90]"),
            ],
        ),
        rec(
            "sale.order.line",
            90,
            &[("product_id", "[120]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 120, &[("name", "Dock")]),
        rec("product.product", 121, &[("name", "Bolt")]),
        rec(
            "mrp.bom",
            23,
            &[("product_id", "[120]"), ("bom_line_ids", "[91]")],
        ),
        rec(
            "mrp.bom.line",
            91,
            &[("product_id", "[121]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            92,
            &[("product_id", "[121]"), ("quantity", "0")],
        ),
        rec(
            "purchase.order.line",
            93,
            &[
                ("product_id", "[121]"),
                ("product_qty", "10"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        rec("purchase.order", 94, &[("order_line", "[93]")]),
        rec(
            "stock.picking",
            95,
            &[
                ("scheduled_date", "2027-10-07T00:00:00"),
                ("purchase_id", "[94]"),
                ("state", "in_transit"),
            ],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn shipment_eta_overrides_the_po_planned_date() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO_SHIP).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_SHIP).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in shipment_fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }

    // Run the SELECT projection the monitor uses (it carries ?lateShip); a slipped
    // shipment must flag order/3 with the transport attribution, the on-time
    // shipment must leave order/4 clear.
    let client = GraphDbClient::new(&base, REPO_SHIP).expect("build query client");
    let rows = client
        .select(ontology::RISK_RULE_SELECT_SPARQL)
        .await
        .expect("run risk-rule SELECT");

    let slipped = ontology::entity_iri("sale.order", 3);
    let on_time = ontology::entity_iri("sale.order", 4);
    let slipped_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.get("so").map(String::as_str) == Some(slipped.as_str()))
        .collect();
    assert_eq!(
        slipped_rows.len(),
        1,
        "the slipped-shipment order is flagged: {rows:?}"
    );
    // shortfall 10 (required 10, on-hand 0, safety 0, inbound dropped by the slip).
    let shortfall: f64 = slipped_rows[0]
        .get("shortfall")
        .and_then(|s| s.parse::<f64>().ok())
        .expect("shortfall is numeric");
    assert!(
        (shortfall - 10.0).abs() < f64::EPSILON,
        "shortfall is 10, got {shortfall}"
    );
    // the transport attribution flag is set for the slipped shipment.
    assert_eq!(
        slipped_rows[0].get("lateShip").map(String::as_str),
        Some("1"),
        "the slip is attributed to the transport delay"
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.get("so").map(String::as_str) == Some(on_time.as_str())),
        "the on-time shipment leaves its order covered"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_SHIP}"))
        .send()
        .await;
}

/// A purchase order that was partially received (Odoo's "create backorder" flow):
/// order/5 (Rack, component Nut) needs 20 Nuts with nothing on hand (the units received
/// earlier were consumed). PO/134 has two Nut lines: line 133 (5, fully received, its picking
/// 135 is completed) and line 137 (12 ordered, 2 received, the other 10 still due on the open
/// backorder picking 138, on time). Only the open quantity is inbound: available = 0 + 10, so
/// shortfall 10.
///
/// Discriminates every wrong reading: counting received units as inbound too reads 17, so
/// shortfall 3; counting a line's whole quantity unless fully received reads 12, so
/// shortfall 8; dropping every line of a PO once any of its receipts completed (a per-PO
/// exclusion) loses the due 10, so shortfall 20.
///
/// Five more orders pin what a shipment can still bring (each needs 10 of its own part with
/// nothing on hand, commitment 2027-10-19; orders 6 to 8 and 10 have one PO line of 10 with 4
/// received, order/9 one line of 10 with nothing received and no shipment):
/// - order/6: the completed receipt's ETA (09-01) is on time but the open backorder carrying
///   the rest lands 2027-12-20, so the rest is late: shortfall 10 with `lateShip` = 1. Letting
///   the completed receipt's ETA speak for the rest would read it as covered.
/// - order/7: "no backorder": the only picking is completed and nothing else is coming, so
///   the unreceived 6 are not inbound: shortfall 10 (falling back to the planned date reads 4).
/// - order/8: the backorder was cancelled (on-time ETA, never arriving): shortfall 10
///   (treating the cancelled picking as open reads 4).
/// - order/9: the purchase order itself was cancelled before any shipment existed (its line
///   keeps its 10 on-time units in the source): shortfall 10 (counting it reads covered).
/// - order/10: the rest has an open late picking (2027-12-20) and a cancelled on-time one
///   (2027-10-07). The cancelled ETA must not speak: shortfall 10 with `lateShip` = 1
///   (letting it speak reads the rest as on time: shortfall 4, `lateShip` 0).
fn completed_receipt_fixtures() -> Vec<SourceRecord> {
    vec![
        rec(
            "sale.order",
            5,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[130]"),
            ],
        ),
        rec(
            "sale.order.line",
            130,
            &[("product_id", "[140]"), ("product_uom_qty", "20")],
        ),
        rec("product.product", 140, &[("name", "Rack")]),
        rec("product.product", 141, &[("name", "Nut")]),
        rec(
            "mrp.bom",
            24,
            &[("product_id", "[140]"), ("bom_line_ids", "[131]")],
        ),
        rec(
            "mrp.bom.line",
            131,
            &[("product_id", "[141]"), ("product_qty", "1")],
        ),
        // the received units were consumed: nothing on hand
        rec(
            "stock.quant",
            132,
            &[("product_id", "[141]"), ("quantity", "0")],
        ),
        rec("purchase.order", 134, &[("order_line", "[133, 137]")]),
        // line 133: fully received
        rec(
            "purchase.order.line",
            133,
            &[
                ("product_id", "[141]"),
                ("product_qty", "5"),
                ("qty_received", "5"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        // line 137: partly received (2 of 12), the other 10 still due
        rec(
            "purchase.order.line",
            137,
            &[
                ("product_id", "[141]"),
                ("product_qty", "12"),
                ("qty_received", "2"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        // the completed receipt
        rec(
            "stock.picking",
            135,
            &[
                ("scheduled_date", "2027-09-07T00:00:00"),
                ("date_done", "2027-09-06T10:00:00"),
                ("purchase_id", "[134]"),
                ("state", "done"),
            ],
        ),
        // and the open backorder carrying the rest, on time
        rec(
            "stock.picking",
            138,
            &[
                ("scheduled_date", "2027-10-07T00:00:00"),
                ("purchase_id", "[134]"),
                ("state", "assigned"),
            ],
        ),
        // order/6
        rec(
            "sale.order",
            6,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[200]"),
            ],
        ),
        rec(
            "sale.order.line",
            200,
            &[("product_id", "[210]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 210, &[("name", "Assembly 6")]),
        rec("product.product", 211, &[("name", "Part 6")]),
        rec(
            "mrp.bom",
            25,
            &[("product_id", "[210]"), ("bom_line_ids", "[201]")],
        ),
        rec(
            "mrp.bom.line",
            201,
            &[("product_id", "[211]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            202,
            &[("product_id", "[211]"), ("quantity", "0")],
        ),
        rec("purchase.order", 204, &[("order_line", "[203]")]),
        rec(
            "purchase.order.line",
            203,
            &[
                ("product_id", "[211]"),
                ("product_qty", "10"),
                ("qty_received", "4"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        rec(
            "stock.picking",
            205,
            &[
                ("scheduled_date", "2027-09-07T00:00:00"),
                ("date_done", "2027-09-06T10:00:00"),
                ("purchase_id", "[204]"),
                ("state", "done"),
            ],
        ),
        rec(
            "stock.picking",
            206,
            &[
                ("scheduled_date", "2027-12-20T00:00:00"),
                ("purchase_id", "[204]"),
                ("state", "assigned"),
            ],
        ),
        // order/7
        rec(
            "sale.order",
            7,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[220]"),
            ],
        ),
        rec(
            "sale.order.line",
            220,
            &[("product_id", "[230]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 230, &[("name", "Assembly 7")]),
        rec("product.product", 231, &[("name", "Part 7")]),
        rec(
            "mrp.bom",
            26,
            &[("product_id", "[230]"), ("bom_line_ids", "[221]")],
        ),
        rec(
            "mrp.bom.line",
            221,
            &[("product_id", "[231]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            222,
            &[("product_id", "[231]"), ("quantity", "0")],
        ),
        rec("purchase.order", 224, &[("order_line", "[223]")]),
        rec(
            "purchase.order.line",
            223,
            &[
                ("product_id", "[231]"),
                ("product_qty", "10"),
                ("qty_received", "4"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        rec(
            "stock.picking",
            225,
            &[
                ("scheduled_date", "2027-09-07T00:00:00"),
                ("date_done", "2027-09-06T10:00:00"),
                ("purchase_id", "[224]"),
                ("state", "done"),
            ],
        ),
        // order/8
        rec(
            "sale.order",
            8,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[240]"),
            ],
        ),
        rec(
            "sale.order.line",
            240,
            &[("product_id", "[250]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 250, &[("name", "Assembly 8")]),
        rec("product.product", 251, &[("name", "Part 8")]),
        rec(
            "mrp.bom",
            27,
            &[("product_id", "[250]"), ("bom_line_ids", "[241]")],
        ),
        rec(
            "mrp.bom.line",
            241,
            &[("product_id", "[251]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            242,
            &[("product_id", "[251]"), ("quantity", "0")],
        ),
        rec("purchase.order", 244, &[("order_line", "[243]")]),
        rec(
            "purchase.order.line",
            243,
            &[
                ("product_id", "[251]"),
                ("product_qty", "10"),
                ("qty_received", "4"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        rec(
            "stock.picking",
            245,
            &[
                ("scheduled_date", "2027-09-07T00:00:00"),
                ("date_done", "2027-09-06T10:00:00"),
                ("purchase_id", "[244]"),
                ("state", "done"),
            ],
        ),
        rec(
            "stock.picking",
            246,
            &[
                ("scheduled_date", "2027-10-07T00:00:00"),
                ("purchase_id", "[244]"),
                ("state", "cancel"),
            ],
        ),
        // order/9: a cancelled purchase order
        rec(
            "sale.order",
            9,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[260]"),
            ],
        ),
        rec(
            "sale.order.line",
            260,
            &[("product_id", "[270]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 270, &[("name", "Assembly 9")]),
        rec("product.product", 271, &[("name", "Part 9")]),
        rec(
            "mrp.bom",
            28,
            &[("product_id", "[270]"), ("bom_line_ids", "[261]")],
        ),
        rec(
            "mrp.bom.line",
            261,
            &[("product_id", "[271]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            262,
            &[("product_id", "[271]"), ("quantity", "0")],
        ),
        rec(
            "purchase.order",
            264,
            &[("order_line", "[263]"), ("state", "cancel")],
        ),
        rec(
            "purchase.order.line",
            263,
            &[
                ("product_id", "[271]"),
                ("product_qty", "10"),
                ("qty_received", "0"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        // order/10: an open late picking beside a cancelled on-time one
        rec(
            "sale.order",
            10,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[280]"),
            ],
        ),
        rec(
            "sale.order.line",
            280,
            &[("product_id", "[290]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 290, &[("name", "Assembly 10")]),
        rec("product.product", 291, &[("name", "Part 10")]),
        rec(
            "mrp.bom",
            29,
            &[("product_id", "[290]"), ("bom_line_ids", "[281]")],
        ),
        rec(
            "mrp.bom.line",
            281,
            &[("product_id", "[291]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            282,
            &[("product_id", "[291]"), ("quantity", "0")],
        ),
        rec("purchase.order", 284, &[("order_line", "[283]")]),
        rec(
            "purchase.order.line",
            283,
            &[
                ("product_id", "[291]"),
                ("product_qty", "10"),
                ("qty_received", "4"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        rec(
            "stock.picking",
            285,
            &[
                ("scheduled_date", "2027-12-20T00:00:00"),
                ("purchase_id", "[284]"),
                ("state", "assigned"),
            ],
        ),
        rec(
            "stock.picking",
            286,
            &[
                ("scheduled_date", "2027-10-07T00:00:00"),
                ("purchase_id", "[284]"),
                ("state", "cancel"),
            ],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn only_a_lines_open_quantity_is_inbound() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO_DONE).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_DONE).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in completed_receipt_fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }

    let client = GraphDbClient::new(&base, REPO_DONE).expect("build query client");
    let rows = client
        .select(ontology::RISK_RULE_SELECT_SPARQL)
        .await
        .expect("run risk-rule SELECT");

    // (order, expected shortfall, expected lateShip, why)
    let expected = [
        (
            5,
            10.0,
            "0",
            "only the lines' open quantity (10) is inbound",
        ),
        (6, 10.0, "1", "the rest rides a LATE open backorder"),
        (7, 10.0, "0", "no backorder: nothing more is coming"),
        (8, 10.0, "0", "the backorder was cancelled"),
        (9, 10.0, "0", "the purchase order was cancelled"),
        (
            10,
            10.0,
            "1",
            "a cancelled on-time picking does not speak for the rest",
        ),
    ];
    // The tabs' shared open-inbound fragment restates this receipt logic rather than
    // splicing it: run it over the same world and require that, per line, it keeps the
    // open quantity (and arrival) the rule counts, so the two cannot drift apart.
    let fragment = client
        .select(&format!(
            "PREFIX ekg: <{vocab}>\nSELECT ?ib_pol ?ib_open ?ib_arrival WHERE {{ {fragment} }}",
            vocab = ontology::VOCAB_NAMESPACE,
            fragment = tabs::queries::OPEN_INBOUND_FRAGMENT,
        ))
        .await
        .expect("run the open-inbound fragment");
    let open: BTreeMap<String, (f64, String)> = fragment
        .iter()
        .map(|r| {
            let line = r.get("ib_pol").cloned().expect("a line");
            let qty = r
                .get("ib_open")
                .and_then(|q| q.parse::<f64>().ok())
                .expect("an open quantity");
            let arrival = r.get("ib_arrival").cloned().expect("an arrival");
            (line, (qty, arrival))
        })
        .collect();
    let line = |id| ontology::entity_iri("purchase.order.line", id);
    assert_eq!(
        open.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([line(137), line(203), line(283)]),
        "open: the backordered rests only (133 received; 223 no backorder; 243 backorder \
         cancelled; 263 PO cancelled)"
    );
    let day = |iso: &str| iso.get(..10).map(str::to_owned).expect("an ISO day");
    for (id, qty, arrival) in [
        (137, 10.0, "2027-10-07"),
        (203, 6.0, "2027-12-20"),
        (283, 6.0, "2027-12-20"),
    ] {
        let (got_qty, got_arrival) = &open[&line(id)];
        assert!(
            (got_qty - qty).abs() < f64::EPSILON,
            "line {id} open {got_qty}"
        );
        assert_eq!(
            day(got_arrival),
            arrival,
            "line {id}: the open shipment's ETA"
        );
    }
    // What the rule counts on time is the fragment's on-time open quantity: each
    // order's shortfall is its requirement minus it (nothing on hand, no safety stock).
    let lines_of = BTreeMap::from([
        (5, vec![133, 137]),
        (6, vec![203]),
        (7, vec![223]),
        (8, vec![243]),
        (9, vec![263]),
        (10, vec![283]),
    ]);
    let required = BTreeMap::from([
        (5, 20.0),
        (6, 10.0),
        (7, 10.0),
        (8, 10.0),
        (9, 10.0),
        (10, 10.0),
    ]);

    for (id, want_shortfall, want_late, why) in expected {
        let order = ontology::entity_iri("sale.order", id);
        let order_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.get("so").map(String::as_str) == Some(order.as_str()))
            .collect();
        assert_eq!(order_rows.len(), 1, "order/{id} is short ({why}): {rows:?}");
        let shortfall: f64 = order_rows[0]
            .get("shortfall")
            .and_then(|s| s.parse::<f64>().ok())
            .expect("shortfall is numeric");
        assert!(
            (shortfall - want_shortfall).abs() < f64::EPSILON,
            "order/{id} shortfall is {want_shortfall} ({why}), got {shortfall}"
        );
        assert_eq!(
            order_rows[0].get("lateShip").map(String::as_str),
            Some(want_late),
            "order/{id} lateShip ({why})"
        );
        let on_time_open: f64 = lines_of[&id]
            .iter()
            .filter_map(|l| open.get(&line(*l)))
            .filter(|(_, arrival)| day(arrival).as_str() <= "2027-10-19")
            .map(|(qty, _)| qty)
            .sum();
        assert!(
            (required[&id] - on_time_open - shortfall).abs() < f64::EPSILON,
            "order/{id}: the fragment's on-time open quantity is what the rule counts ({why})"
        );
    }

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_DONE}"))
        .send()
        .await;
}

/// The seeded shape: VeloMax orders 10 E-Bikes; the E-Bike's BOM needs a made Drive
/// Subassembly (product 101, the lower IRI, no stock, so short 10) and a purchased battery
/// (product 104: 2 on hand, safety 5, only a late PO of 10, so short 13).
fn cause_order_fixtures() -> Vec<SourceRecord> {
    vec![
        rec(
            "res.partner",
            6,
            &[("name", "CellTech"), ("supplier_rank", "1")],
        ),
        rec(
            "sale.order",
            1,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("amount_total", "18000.00"),
                ("order_line", "[10]"),
            ],
        ),
        rec(
            "sale.order.line",
            10,
            &[("product_id", "[100]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 100, &[("name", "E-Bike")]),
        rec("product.product", 101, &[("name", "Drive Subassembly")]),
        rec("product.product", 103, &[("name", "Hub Motor")]),
        rec("product.product", 104, &[("name", "Battery")]),
        rec(
            "mrp.bom",
            20,
            &[("product_id", "[100]"), ("bom_line_ids", "[30, 31]")],
        ),
        rec(
            "mrp.bom.line",
            30,
            &[("product_id", "[101]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom.line",
            31,
            &[("product_id", "[104]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom",
            21,
            &[("product_id", "[101]"), ("bom_line_ids", "[32]")],
        ),
        rec(
            "mrp.bom.line",
            32,
            &[("product_id", "[103]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            40,
            &[("product_id", "[103]"), ("quantity", "15")],
        ),
        rec(
            "stock.quant",
            41,
            &[("product_id", "[104]"), ("quantity", "2")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            50,
            &[("product_id", "[104]"), ("product_min_qty", "5")],
        ),
        rec(
            "product.supplierinfo",
            55,
            &[
                ("product_id", "[104]"),
                ("partner_id", "[6]"),
                ("sequence", "1"),
            ],
        ),
        rec(
            "purchase.order",
            60,
            &[
                ("partner_id", "[6]"),
                ("order_line", "[70]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            70,
            &[
                ("product_id", "[104]"),
                ("product_qty", "10"),
                ("date_planned", "2027-12-20T00:00:00"),
            ],
        ),
    ]
}

/// The battery fix: an on-time PO of 10, so the battery is short only 3, less than the made
/// Drive's 10.
fn battery_fix_fixtures() -> Vec<SourceRecord> {
    vec![
        rec(
            "purchase.order",
            61,
            &[
                ("partner_id", "[6]"),
                ("order_line", "[71]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            71,
            &[
                ("product_id", "[104]"),
                ("product_qty", "10"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn the_rules_first_cause_is_the_component_the_tabs_light() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO_CAUSE).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_CAUSE).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in cause_order_fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }
    let so = ontology::entity_iri("sale.order", 1);
    let (drive, battery) = (
        ontology::entity_iri("product.product", 101),
        ontology::entity_iri("product.product", 104),
    );

    // The rule's rows for the order, in the order the monitor reads them (its first verdict's
    // reason is the one the point is opened with), and the component the Graph lights.
    let causes_and_lit = || async {
        let rows = world
            .select(ontology::RISK_RULE_SELECT_SPARQL)
            .await
            .expect("risk rule");
        let causes: Vec<(String, f64)> = rows
            .iter()
            .filter(|r| r.get("so") == Some(&so))
            .map(|r| {
                (
                    r.get("comp").cloned().expect("a component"),
                    r.get("shortfall")
                        .and_then(|s| s.parse::<f64>().ok())
                        .expect("a shortfall"),
                )
            })
            .collect();
        let at_risk: BTreeSet<String> = rows.iter().filter_map(|r| r.get("so").cloned()).collect();
        let graph = tabs::project_on(
            &world,
            &EntityUri(so.clone()),
            Tab::Graph,
            Some(&at_risk),
            Day::today(),
        )
        .await
        .expect("graph projected");
        assert!(graph.warnings.is_empty(), "{:?}", graph.warnings);
        let lit: Vec<String> = graph
            .rows
            .iter()
            .filter(|r| {
                r.get("role").is_some_and(|x| x == "Component")
                    && r.get("on_path").is_some_and(|x| x == "true")
            })
            .filter_map(|r| r.get("node").cloned())
            .collect();
        (causes, lit)
    };

    // Before the fix: the purchased battery (13) first, then the made Drive (10), whatever
    // row order the store would otherwise pick (the Drive is the lower IRI).
    let (causes, lit) = causes_and_lit().await;
    assert_eq!(
        causes,
        [(battery.clone(), 13.0), (drive.clone(), 10.0)],
        "purchased before made"
    );
    assert_eq!(
        lit,
        [causes[0].0.clone()],
        "the reason's component is the lit one"
    );

    // After the battery fix the made Drive is the larger shortfall, yet the rule still
    // leads with the purchased battery, and the chain stays on it (the supply columns do
    // not go dark while a purchased cause is short).
    for record in battery_fix_fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize the fix");
    }
    let (causes, lit) = causes_and_lit().await;
    assert_eq!(
        causes,
        [(battery.clone(), 3.0), (drive, 10.0)],
        "purchased before made, whatever the shortfall"
    );
    assert_eq!(
        lit,
        [battery],
        "the reason's component is still the lit one"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_CAUSE}"))
        .send()
        .await;
}

/// The requirement is tied to the ordered product's own BOM tree and summed per line:
///
/// - order/1: 10 E-Bikes (BOM 20: battery x1). An unordered Scooter's BOM 21 uses the battery
///   x4. Battery: 12 on hand, safety 5, an on-time PO of 3, so 10 + 5 - 15 = 0: covered. An
///   untied BOM-line join would also require 10 x 4 = 40, a false shortfall of 30.
/// - order/2: 10 Cargo carts (BOM 22: part 111 x1, part 113 x1, sub-assembly 112 x1; BOM 23
///   makes the sub-assembly from part 111 x1). Part 111 is used on two lines of the tree, so
///   required 10 + 10 = 20, 15 on hand, short 5 (a per-value collapse would say 10, covered).
///   Part 113: 5 on hand, short 5 (a tie with part 111). Sub-assembly 112: made, no stock,
///   short 10. The rule's cause order: purchased first (the tie to the lower IRI), made last.
fn requirement_tree_fixtures() -> Vec<SourceRecord> {
    vec![
        rec(
            "sale.order",
            1,
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
        rec("product.product", 101, &[("name", "Battery")]),
        rec("product.product", 102, &[("name", "Scooter")]),
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
            "mrp.bom",
            21,
            &[("product_id", "[102]"), ("bom_line_ids", "[31]")],
        ),
        rec(
            "mrp.bom.line",
            31,
            &[("product_id", "[101]"), ("product_qty", "4")],
        ),
        rec(
            "stock.quant",
            40,
            &[("product_id", "[101]"), ("quantity", "12")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            50,
            &[("product_id", "[101]"), ("product_min_qty", "5")],
        ),
        rec(
            "purchase.order.line",
            70,
            &[
                ("product_id", "[101]"),
                ("product_qty", "3"),
                ("date_planned", "2027-09-07T00:00:00"),
            ],
        ),
        // order/2
        rec(
            "sale.order",
            2,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[11]"),
            ],
        ),
        rec(
            "sale.order.line",
            11,
            &[("product_id", "[110]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 110, &[("name", "Cargo")]),
        rec("product.product", 111, &[("name", "Part 111")]),
        rec("product.product", 112, &[("name", "Sub-assembly 112")]),
        rec("product.product", 113, &[("name", "Part 113")]),
        rec(
            "mrp.bom",
            22,
            &[("product_id", "[110]"), ("bom_line_ids", "[32, 33, 35]")],
        ),
        rec(
            "mrp.bom.line",
            32,
            &[("product_id", "[111]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom.line",
            33,
            &[("product_id", "[112]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom.line",
            35,
            &[("product_id", "[113]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom",
            23,
            &[("product_id", "[112]"), ("bom_line_ids", "[34]")],
        ),
        rec(
            "mrp.bom.line",
            34,
            &[("product_id", "[111]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            41,
            &[("product_id", "[111]"), ("quantity", "15")],
        ),
        rec(
            "stock.quant",
            42,
            &[("product_id", "[113]"), ("quantity", "5")],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn the_requirement_sums_only_the_ordered_products_own_bom_lines() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO_TREE).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_TREE).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in requirement_tree_fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }
    let rows = world
        .select(ontology::RISK_RULE_SELECT_SPARQL)
        .await
        .expect("risk rule");
    let causes = |order: i64| -> Vec<(String, f64)> {
        let so = ontology::entity_iri("sale.order", order);
        rows.iter()
            .filter(|r| r.get("so") == Some(&so))
            .map(|r| {
                (
                    r.get("comp").cloned().expect("a component"),
                    r.get("shortfall")
                        .and_then(|s| s.parse::<f64>().ok())
                        .expect("a shortfall"),
                )
            })
            .collect()
    };
    assert!(
        causes(1).is_empty(),
        "an unordered product's BOM line must not raise order/1's requirement: {:?}",
        causes(1)
    );
    let part = |id| ontology::entity_iri("product.product", id);
    assert_eq!(
        causes(2),
        [(part(111), 5.0), (part(113), 5.0), (part(112), 10.0)],
        "both tree lines of part 111 count (20 - 15); purchased first, the tie to the lower \
         IRI, the made sub-assembly last"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_TREE}"))
        .send()
        .await;
}

const REPO_DELIVERY: &str = "ekg_risk_rule_delivery_it";

/// Delivery by demand coverage, judged against the live store:
///
/// - order/1: 10 E-Bikes promised 2027-10-19, each needing 1 purchased Battery (104): 2 on
///   hand, safety 5, and one open inbound line of 10 (PO line 70) whose open receipt
///   (picking 62) lands 2027-11-15, after the promise. The rule's shortfall is
///   10 + 5 - 2 = 13 > safety, so demand is uncovered at the promise; on hand + the receipt
///   (12) covers the demand of 10 on 2027-11-15.
/// - order/2: 5 Trikes promised 2027-10-19, needing a made Drive Subassembly (201) with no
///   stock and no inbound at all, so nothing covers the demand.
fn delivery_fixtures() -> Vec<SourceRecord> {
    vec![
        rec(
            "sale.order",
            1,
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
        rec("product.product", 104, &[("name", "Battery")]),
        rec(
            "mrp.bom",
            20,
            &[("product_id", "[100]"), ("bom_line_ids", "[30]")],
        ),
        rec(
            "mrp.bom.line",
            30,
            &[("product_id", "[104]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            40,
            &[("product_id", "[104]"), ("quantity", "2")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            50,
            &[("product_id", "[104]"), ("product_min_qty", "5")],
        ),
        rec(
            "purchase.order",
            60,
            &[("order_line", "[70]"), ("state", "purchase")],
        ),
        rec(
            "purchase.order.line",
            70,
            &[
                ("product_id", "[104]"),
                ("product_qty", "10"),
                ("date_planned", "2027-11-15T08:00:00"),
            ],
        ),
        rec(
            "stock.picking",
            62,
            &[
                ("scheduled_date", "2027-11-15T08:00:00"),
                ("purchase_id", "[60]"),
                ("state", "assigned"),
            ],
        ),
        // order/2: a made component with no inbound
        rec(
            "sale.order",
            2,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[11]"),
            ],
        ),
        rec(
            "sale.order.line",
            11,
            &[("product_id", "[200]"), ("product_uom_qty", "5")],
        ),
        rec("product.product", 200, &[("name", "Trike")]),
        rec("product.product", 201, &[("name", "Drive Subassembly")]),
        rec("product.product", 202, &[("name", "Hub Motor")]),
        rec(
            "mrp.bom",
            22,
            &[("product_id", "[200]"), ("bom_line_ids", "[32]")],
        ),
        rec(
            "mrp.bom.line",
            32,
            &[("product_id", "[201]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom",
            23,
            &[("product_id", "[201]"), ("bom_line_ids", "[33]")],
        ),
        rec(
            "mrp.bom.line",
            33,
            &[("product_id", "[202]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            42,
            &[("product_id", "[202]"), ("quantity", "50")],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn delivery_is_judged_by_demand_coverage_against_the_original_promise() {
    use ekg_domain::{ActionType, Plan, ProposedAction, Simulator};
    use ekg_semantic::GraphDbSimulator;
    use ekg_semantic::display::{self, Delivery, Outcome, WorldSource};

    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO_DELIVERY).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_DELIVERY).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in delivery_fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }
    let current = WorldSource(&world);
    let order = EntityUri(ontology::entity_iri("sale.order", 1));
    let uncoverable = EntityUri(ontology::entity_iri("sale.order", 2));
    let line = ontology::entity_iri("purchase.order.line", 70);
    let simulator =
        GraphDbSimulator::new(GraphDbClient::new(&base, REPO_DELIVERY).expect("sim client"));
    let plan = |verb: &str, target: String, new_date: &str| Plan {
        actions: vec![ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: EntityUri(target),
            params: [("new_date".to_owned(), new_date.to_owned())]
                .into_iter()
                .collect(),
        }],
    };

    // The rule still flags the safety breach on the base (13 > 5), and it is the only thing a
    // covering expedite leaves (3 <= 5): safety is a planning buffer, not a delivery blocker.
    let base_rows = world
        .select(ontology::RISK_RULE_SELECT_SPARQL)
        .await
        .expect("risk rule");
    let shortfall_of = |rows: &[BTreeMap<String, String>]| -> Option<f64> {
        rows.iter()
            .find(|r| r.get("so") == Some(&order.0))
            .and_then(|r| r.get("shortfall")?.parse().ok())
    };
    assert_eq!(shortfall_of(&base_rows), Some(13.0), "{base_rows:?}");

    // (c) Do nothing: late until the receipt covers the demand, 13 Oct to 9 Nov = 27 days.
    let baseline = display::outcome(&current, &current, &order)
        .await
        .expect("baseline outcome");
    // (d) Uncoverable demand: at risk, with no completion and no day count.
    let at_risk = display::outcome(&current, &current, &uncoverable)
        .await
        .expect("uncoverable outcome");

    // (a) Expedite the line to before the promise: the demand is covered (2 + 10 >= 10) while
    // safety stays unmet: on time, completing at the promise, slip 0.
    let (expedite_rows, expedited, expedited_uncoverable) = {
        let projection = simulator
            .project(&plan("expedite_po", line.clone(), "2027-10-12 08:00:00"))
            .await
            .expect("project the expedite");
        (
            projection
                .select(ontology::RISK_RULE_SELECT_SPARQL)
                .await
                .expect("projected rule"),
            display::outcome(&current, &*projection, &order)
                .await
                .expect("expedite outcome"),
            display::outcome(&current, &*projection, &uncoverable)
                .await
                .expect("expedite outcome for order/2"),
        )
    };
    // (b) Re-promise to 20 Nov (after the receipt): covered at the new promise, but 38 days
    // after the original one, so late by that many days.
    let (repromised, framing) = {
        let projection = simulator
            .project(&plan(
                "re_promise_date",
                order.0.clone(),
                "2027-11-26 00:00:00",
            ))
            .await
            .expect("project the re-promise");
        (
            display::outcome(&current, &*projection, &order)
                .await
                .expect("re-promise outcome"),
            display::plan_framing(&current, &*projection, &order).await,
        )
    };
    let baseline_framing = display::plan_framing(&current, &current, &order).await;

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_DELIVERY}"))
        .send()
        .await;

    let day = |o: &Option<Outcome>| {
        o.as_ref()
            .and_then(|o| o.completion.as_deref())
            .map(|c| c.split('T').next().unwrap_or(c).to_owned())
    };
    let delivery_of = |o: &Option<Outcome>| o.as_ref().map(|o| o.delivery);
    let slip_of = |o: &Option<Outcome>| o.as_ref().and_then(|o| o.slip);

    assert_eq!(delivery_of(&baseline), Some(Delivery::Late), "{baseline:?}");
    assert_eq!(slip_of(&baseline), Some(27), "{baseline:?}");
    assert_eq!(
        day(&baseline).as_deref(),
        Some("2027-11-15"),
        "{baseline:?}"
    );

    assert_eq!(delivery_of(&at_risk), Some(Delivery::AtRisk), "{at_risk:?}");
    assert_eq!(slip_of(&at_risk), None, "{at_risk:?}");
    assert_eq!(day(&at_risk), None, "{at_risk:?}");

    assert_eq!(
        shortfall_of(&expedite_rows),
        Some(3.0),
        "the expedite leaves only the safety breach: {expedite_rows:?}"
    );
    assert_eq!(
        delivery_of(&expedited),
        Some(Delivery::OnTime),
        "{expedited:?}"
    );
    assert_eq!(slip_of(&expedited), Some(0), "{expedited:?}");
    assert_eq!(
        day(&expedited).as_deref(),
        Some("2027-10-19"),
        "{expedited:?}"
    );
    assert_eq!(
        delivery_of(&expedited_uncoverable),
        Some(Delivery::AtRisk),
        "the battery expedite leaves the made Drive uncovered: {expedited_uncoverable:?}"
    );

    assert_eq!(
        delivery_of(&repromised),
        Some(Delivery::Late),
        "{repromised:?}"
    );
    assert_eq!(slip_of(&repromised), Some(38), "{repromised:?}");
    assert_eq!(
        day(&repromised).as_deref(),
        Some("2027-11-26"),
        "{repromised:?}"
    );

    // The served framing carries the same judgment, for the option and the baseline.
    assert_eq!(
        framing.get(display::DELIVERY_KEY).map(String::as_str),
        Some("late"),
        "{framing:?}"
    );
    assert_eq!(
        framing.get(display::DAY_SLIP_KEY).map(String::as_str),
        Some("38"),
        "{framing:?}"
    );
    assert_eq!(
        baseline_framing
            .get(display::DELIVERY_KEY)
            .map(String::as_str),
        Some("late"),
        "{baseline_framing:?}"
    );
    assert_eq!(
        baseline_framing
            .get(display::DAY_SLIP_KEY)
            .map(String::as_str),
        Some("27"),
        "{baseline_framing:?}"
    );
}
