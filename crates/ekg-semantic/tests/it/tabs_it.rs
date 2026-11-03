//! Gated live integration test: the four read-model tabs over a typed fixture world.
//!
//! Ignored by default. Run against a reachable GraphDB (`just podman-local-up`) with:
//!
//! ```text
//! EKG_E2E_REQUIRE_INFRA=1 cargo test -p ekg-semantic --test it tabs_it -- --ignored
//! ```
//!
//! The fixture is materialized through the supply-risk mapping contract into a throwaway
//! repo, with every date relative to today (the OTD and demand windows are day-relative):
//!
//! - VeloMax (partner 1) orders 10 E-Bikes (sale.order/1, promised today+10, €18,000); a
//!   second order (sale.order/2, 2 E-Bikes) is promised today+40, outside the demand window.
//! - The E-Bike's BOM (20) needs one battery (product 101) and one frame (product 102).
//! - The battery is sourced from CellTech (partner 2, sequence 1, so primary) and VoltPack
//!   (partner 3, sequence 2, so backup); the frame only from FrameCo (partner 4, single).
//! - Battery: 2 on hand, safety 5. Open inbound: CellTech's PO 60 (3, today+5, on time) and
//!   VoltPack's PO 61 (10, today+20, after the promise). CellTech's POs 62 to 65 are fully
//!   received (history only). So sale.order/1 is short of the battery by 10 + 5 - 2 - 3 = 10.
//! - Frame: 100 on hand, so never short.
//! - CellTech's receipts: PO 62 completed the day before its planned day but after its
//!   scheduled ETA (on time: the planned day is the basis, not the ETA), PO 63 completed after
//!   its planned day but before its moved-later scheduled ETA (late), PO 64 completed later on
//!   its planned day (on time at day granularity), PO 65 completed 150 days ago (outside the
//!   90-day window). That gives OTD 2/3 on the planned basis. An ETA basis (or a COALESCE of
//!   the two) would serve 3/3, so the figure proves the basis.
//! - MO 40 (origin "SO1") fulfils sale.order/1 (`ekg:fulfills` derived by the process-edge
//!   derivation). MO 39 (origin "SO1" too, a lower IRI) is done: a backorder Flow never
//!   draws, so it must never be the path's MO hop.
//!
//! A second, two-level point (the deep-cause, cause-order and per-line cases):
//!
//! - VeloMax orders 4 Cargo Trikes (sale.order/3, promised today+12, €9,000). The Trike's BOM
//!   (21) needs a Drive Unit sub-assembly (product 201, made by BOM 22 from one Hub Motor,
//!   product 202) and two frames; the Drive Unit's BOM uses two frames too. The frame's 30-day
//!   demand is SO1 10 x 1 + SO3 4 x 2 + 4 x 2 = 26: each order only through its own product's
//!   BOM lines (never the Trike's lines for SO1), and two equal lines both count.
//! - The Drive Unit has no stock, so it is short 4 (made); the motor has 1 on hand and only
//!   MotorWorks' (partner 5) late PO 66 (3, today+30), so it is short 3 (purchased). The rule
//!   lists the purchased motor first although the made Drive Unit is the larger shortfall and
//!   the lower IRI: the inbox reason and the lit chain both name the motor.
//! - PO 66 is a two-line PO: the motor line (the cause) and a frame line (not a cause).
//! - FrameCo's PO 67 is partly received: frame 8 ordered, 3 received, open picking ETA today+6,
//!   so open 5 arriving today+6; its other line (2 motors) is fully received, so no inbound
//!   edge. PO 68 is fully received with a still-open picking (out by the open-quantity clause
//!   alone); PO 69 is partly received but its only picking is done (out by the shipment clause
//!   alone). CellTech's open PO 60 also carries a fully received frame line with no picking:
//!   P60 is still drawn, but only its battery line is an inbound edge.
//!
//! A multi-line point with only made causes (sale.order/4, promised today+40, outside the
//! demand window): 3 Wheel Kits (product 199, whose BOM 27 uses a Wheel sub-assembly 211, made
//! by BOM 26 from one frame) and 1 Trike. The Wheel (short 3) and the Drive Unit (short 1) are
//! made; the motor is covered by PO 66 before today+40. So the chain component is the made
//! Wheel, lit in the Product and BOM column with no supply hop. The kept product is `.../199`,
//! whose BOM 27 has a higher IRI than the Trike's BOM 21. MO 37 (Trike) and MO 41 (Wheel Kit)
//! both fulfil SO4: the lit MO is MO 41, the one building the kept product; MO 37 as a subject
//! lights the Trike and its own BOM. MO 38 (origin SO1) is cancelled.

// `expect`/`panic` are the assertion mechanism here; prose names products.
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{EntityUri, SourceRecord, WorldModel};
use ekg_semantic::tabs::{self, Day, Row, Tab};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

use crate::common;
use common::{graphdb_url, infra_unavailable, provision_repo, rec};

const REPO: &str = "ekg_tabs_it";

/// `today + days` at noon, as the ISO dateTime the source would carry.
fn at(today: Day, days: i64, time: &str) -> String {
    format!("{}T{time}", today.plus(days).iso())
}

fn fixtures(today: Day) -> Vec<SourceRecord> {
    let noon = |days| at(today, days, "12:00:00");
    vec![
        // --- parties ---
        rec(
            "res.partner",
            1,
            &[("name", "VeloMax Retail"), ("customer_rank", "1")],
        ),
        rec(
            "res.partner",
            2,
            &[("name", "CellTech GmbH"), ("supplier_rank", "1")],
        ),
        rec(
            "res.partner",
            3,
            &[("name", "VoltPack AG"), ("supplier_rank", "1")],
        ),
        rec(
            "res.partner",
            4,
            &[("name", "FrameCo Metals"), ("supplier_rank", "1")],
        ),
        // --- orders ---
        rec(
            "sale.order",
            1,
            &[
                ("name", "SO1"),
                ("commitment_date", &noon(10)),
                ("amount_total", "18000.00"),
                ("partner_id", "[1]"),
                ("order_line", "[10]"),
            ],
        ),
        rec(
            "sale.order.line",
            10,
            &[("product_id", "[100]"), ("product_uom_qty", "10")],
        ),
        rec(
            "sale.order",
            2,
            &[
                ("name", "SO2"),
                ("commitment_date", &noon(40)),
                ("amount_total", "3600.00"),
                ("partner_id", "[1]"),
                ("order_line", "[11]"),
            ],
        ),
        rec(
            "sale.order.line",
            11,
            &[("product_id", "[100]"), ("product_uom_qty", "2")],
        ),
        // --- products + BOM ---
        rec("product.product", 100, &[("name", "E-Bike Model X")]),
        rec("product.product", 101, &[("name", "Battery Pack 500Wh")]),
        rec("product.product", 102, &[("name", "Aluminium Frame")]),
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
            &[("product_id", "[102]"), ("product_qty", "1")],
        ),
        // --- sourcing ---
        rec(
            "product.supplierinfo",
            50,
            &[
                ("product_id", "[101]"),
                ("partner_id", "[2]"),
                ("delay", "45"),
                ("sequence", "1"),
            ],
        ),
        rec(
            "product.supplierinfo",
            51,
            &[
                ("product_id", "[101]"),
                ("partner_id", "[3]"),
                ("delay", "21"),
                ("sequence", "2"),
            ],
        ),
        rec(
            "product.supplierinfo",
            52,
            &[
                ("product_id", "[102]"),
                ("partner_id", "[4]"),
                ("delay", "14"),
                ("sequence", "1"),
            ],
        ),
        // --- stock + safety ---
        rec(
            "stock.quant",
            90,
            &[("product_id", "[101]"), ("quantity", "2")],
        ),
        rec(
            "stock.quant",
            91,
            &[("product_id", "[102]"), ("quantity", "100")],
        ),
        rec(
            "stock.warehouse.orderpoint",
            95,
            &[("product_id", "[101]"), ("product_min_qty", "5")],
        ),
        // --- open inbound: the primary's on-time line, the backup's late line ---
        rec(
            "purchase.order",
            60,
            &[
                ("name", "P60"),
                ("partner_id", "[2]"),
                ("order_line", "[70, 82]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            70,
            &[
                ("product_id", "[101]"),
                ("product_qty", "3"),
                ("date_planned", &noon(5)),
            ],
        ),
        // P60's second line: fully received, no picking. Not inbound, so no P60-to-frame edge.
        rec(
            "purchase.order.line",
            82,
            &[
                ("product_id", "[102]"),
                ("product_qty", "5"),
                ("qty_received", "5"),
                ("date_planned", &noon(5)),
            ],
        ),
        rec(
            "purchase.order",
            61,
            &[
                ("name", "P61"),
                ("partner_id", "[3]"),
                ("order_line", "[71]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            71,
            &[
                ("product_id", "[101]"),
                ("product_qty", "10"),
                ("date_planned", &noon(20)),
            ],
        ),
        // --- CellTech's fully received history (never inbound) + its OTD cases ---
        // PO 62: completed the day before its planned day, so on time (the scheduled ETA is D-10,
        // so an ETA basis would call it on time too; PO 63 is where the two bases differ).
        rec(
            "purchase.order",
            62,
            &[
                ("name", "P62"),
                ("partner_id", "[2]"),
                ("order_line", "[72]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            72,
            &[
                ("product_id", "[101]"),
                ("product_qty", "5"),
                ("qty_received", "5"),
                ("date_planned", &noon(-10)),
            ],
        ),
        rec(
            "stock.picking",
            80,
            &[
                ("purchase_id", "[62]"),
                ("scheduled_date", &noon(-10)),
                ("date_done", &at(today, -11, "10:00:00")),
                ("state", "done"),
            ],
        ),
        // PO 63: completed after its planned day, before its moved-later ETA, so late on the
        // planned basis (an ETA basis would call it on time, giving 3/3 instead of 2/3).
        rec(
            "purchase.order",
            63,
            &[
                ("name", "P63"),
                ("partner_id", "[2]"),
                ("order_line", "[73]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            73,
            &[
                ("product_id", "[101]"),
                ("product_qty", "4"),
                ("qty_received", "4"),
                ("date_planned", &noon(-20)),
            ],
        ),
        rec(
            "stock.picking",
            81,
            &[
                ("purchase_id", "[63]"),
                ("scheduled_date", &noon(-15)),
                ("date_done", &noon(-17)),
                ("state", "done"),
            ],
        ),
        // PO 64: completed later the same day than its planned time, so on time (day granular).
        rec(
            "purchase.order",
            64,
            &[
                ("name", "P64"),
                ("partner_id", "[2]"),
                ("order_line", "[74]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            74,
            &[
                ("product_id", "[101]"),
                ("product_qty", "6"),
                ("qty_received", "6"),
                ("date_planned", &at(today, -30, "08:00:00")),
            ],
        ),
        rec(
            "stock.picking",
            82,
            &[
                ("purchase_id", "[64]"),
                ("scheduled_date", &at(today, -30, "08:00:00")),
                ("date_done", &at(today, -30, "17:00:00")),
                ("state", "done"),
            ],
        ),
        // PO 65: on time but completed 150 days ago, outside the trailing 90-day window.
        rec(
            "purchase.order",
            65,
            &[
                ("name", "P65"),
                ("partner_id", "[2]"),
                ("order_line", "[75]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            75,
            &[
                ("product_id", "[101]"),
                ("product_qty", "2"),
                ("qty_received", "2"),
                ("date_planned", &noon(-149)),
            ],
        ),
        rec(
            "stock.picking",
            83,
            &[
                ("purchase_id", "[65]"),
                ("scheduled_date", &noon(-150)),
                ("date_done", &noon(-150)),
                ("state", "done"),
            ],
        ),
        // --- a cancelled earlier MO for the same order: never the path's MO hop either ---
        rec(
            "mrp.production",
            38,
            &[
                ("name", "MO00"),
                ("product_id", "[100]"),
                ("bom_id", "[20]"),
                ("product_qty", "1"),
                ("state", "cancel"),
                ("origin", "SO1"),
                ("date_start", &noon(-6)),
                ("date_finished", &noon(-3)),
            ],
        ),
        // --- a done earlier MO for the same order (a lower IRI): never the path's MO hop ---
        rec(
            "mrp.production",
            39,
            &[
                ("name", "MO0"),
                ("product_id", "[100]"),
                ("bom_id", "[20]"),
                ("product_qty", "2"),
                ("state", "done"),
                ("origin", "SO1"),
                ("date_start", &noon(-9)),
                ("date_finished", &noon(-5)),
            ],
        ),
        // --- the fulfilling manufacturing order ---
        rec(
            "mrp.production",
            40,
            &[
                ("name", "MO1"),
                ("product_id", "[100]"),
                ("bom_id", "[20]"),
                ("product_qty", "10"),
                ("state", "confirmed"),
                ("origin", "SO1"),
                ("date_start", &noon(2)),
                ("date_finished", &noon(8)),
            ],
        ),
        // --- the two-level point: sale.order/3 (Cargo Trike) ---
        rec(
            "res.partner",
            5,
            &[("name", "MotorWorks AG"), ("supplier_rank", "1")],
        ),
        rec(
            "sale.order",
            3,
            &[
                ("name", "SO3"),
                ("commitment_date", &noon(12)),
                ("amount_total", "9000.00"),
                ("partner_id", "[1]"),
                ("order_line", "[12]"),
            ],
        ),
        rec(
            "sale.order.line",
            12,
            &[("product_id", "[200]"), ("product_uom_qty", "4")],
        ),
        rec("product.product", 200, &[("name", "Cargo Trike")]),
        rec("product.product", 201, &[("name", "Drive Unit")]),
        rec("product.product", 202, &[("name", "Hub Motor")]),
        rec(
            "mrp.bom",
            21,
            &[("product_id", "[200]"), ("bom_line_ids", "[32, 33]")],
        ),
        rec(
            "mrp.bom.line",
            32,
            &[("product_id", "[201]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom.line",
            33,
            &[("product_id", "[102]"), ("product_qty", "2")],
        ),
        rec(
            "mrp.bom",
            22,
            &[("product_id", "[201]"), ("bom_line_ids", "[34, 35]")],
        ),
        rec(
            "mrp.bom.line",
            34,
            &[("product_id", "[202]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom.line",
            35,
            &[("product_id", "[102]"), ("product_qty", "2")],
        ),
        rec(
            "product.supplierinfo",
            53,
            &[
                ("product_id", "[202]"),
                ("partner_id", "[5]"),
                ("delay", "21"),
                ("sequence", "1"),
            ],
        ),
        rec(
            "stock.quant",
            92,
            &[("product_id", "[202]"), ("quantity", "1")],
        ),
        // PO 66: the motor line (late, so the cause) and a frame line (not a cause).
        rec(
            "purchase.order",
            66,
            &[
                ("name", "P66"),
                ("partner_id", "[5]"),
                ("order_line", "[76, 77]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            76,
            &[
                ("product_id", "[202]"),
                ("product_qty", "3"),
                ("date_planned", &noon(30)),
            ],
        ),
        rec(
            "purchase.order.line",
            77,
            &[
                ("product_id", "[102]"),
                ("product_qty", "4"),
                ("date_planned", &noon(30)),
            ],
        ),
        // PO 67: a partly received frame line on an open picking, and a fully received line.
        rec(
            "purchase.order",
            67,
            &[
                ("name", "P67"),
                ("partner_id", "[4]"),
                ("order_line", "[78, 79]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            78,
            &[
                ("product_id", "[102]"),
                ("product_qty", "8"),
                ("qty_received", "3"),
                ("date_planned", &noon(3)),
            ],
        ),
        rec(
            "purchase.order.line",
            79,
            &[
                ("product_id", "[202]"),
                ("product_qty", "2"),
                ("qty_received", "2"),
                ("date_planned", &noon(-5)),
            ],
        ),
        rec(
            "stock.picking",
            84,
            &[
                ("purchase_id", "[67]"),
                ("scheduled_date", &noon(6)),
                ("state", "assigned"),
            ],
        ),
        // PO 68: fully received, yet its picking is still open: out by open quantity alone.
        rec(
            "purchase.order",
            68,
            &[
                ("name", "P68"),
                ("partner_id", "[4]"),
                ("order_line", "[80]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            80,
            &[
                ("product_id", "[102]"),
                ("product_qty", "2"),
                ("qty_received", "2"),
                ("date_planned", &noon(2)),
            ],
        ),
        rec(
            "stock.picking",
            85,
            &[
                ("purchase_id", "[68]"),
                ("scheduled_date", &noon(4)),
                ("state", "assigned"),
            ],
        ),
        // PO 69: partly received, but its only picking is done: out by the shipment clause.
        rec(
            "purchase.order",
            69,
            &[
                ("name", "P69"),
                ("partner_id", "[4]"),
                ("order_line", "[81]"),
                ("state", "purchase"),
            ],
        ),
        rec(
            "purchase.order.line",
            81,
            &[
                ("product_id", "[102]"),
                ("product_qty", "4"),
                ("qty_received", "1"),
                ("date_planned", &noon(2)),
            ],
        ),
        rec(
            "stock.picking",
            86,
            &[
                ("purchase_id", "[69]"),
                ("scheduled_date", &noon(-1)),
                ("date_done", &noon(-1)),
                ("state", "done"),
            ],
        ),
        // --- the made-cause, multi-line point: sale.order/4 ---
        rec(
            "sale.order",
            4,
            &[
                ("name", "SO4"),
                ("commitment_date", &noon(40)),
                ("amount_total", "3000.00"),
                ("partner_id", "[1]"),
                ("order_line", "[13, 14]"),
            ],
        ),
        rec(
            "sale.order.line",
            13,
            &[("product_id", "[199]"), ("product_uom_qty", "3")],
        ),
        rec(
            "sale.order.line",
            14,
            &[("product_id", "[200]"), ("product_uom_qty", "1")],
        ),
        rec("product.product", 199, &[("name", "Wheel Kit")]),
        rec("product.product", 211, &[("name", "Wheel")]),
        rec(
            "mrp.bom",
            27,
            &[("product_id", "[199]"), ("bom_line_ids", "[36]")],
        ),
        rec(
            "mrp.bom.line",
            36,
            &[("product_id", "[211]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.bom",
            26,
            &[("product_id", "[211]"), ("bom_line_ids", "[37]")],
        ),
        rec(
            "mrp.bom.line",
            37,
            &[("product_id", "[102]"), ("product_qty", "1")],
        ),
        // MO 37 builds the Trike (the line product not kept, a lower IRI); MO 41 the Wheel Kit.
        rec(
            "mrp.production",
            37,
            &[
                ("name", "MO37"),
                ("product_id", "[200]"),
                ("bom_id", "[21]"),
                ("product_qty", "1"),
                ("state", "confirmed"),
                ("origin", "SO4"),
                ("date_start", &noon(30)),
                ("date_finished", &noon(35)),
            ],
        ),
        rec(
            "mrp.production",
            41,
            &[
                ("name", "MO41"),
                ("product_id", "[199]"),
                ("bom_id", "[27]"),
                ("product_qty", "3"),
                ("state", "confirmed"),
                ("origin", "SO4"),
                ("date_start", &noon(30)),
                ("date_finished", &noon(36)),
            ],
        ),
    ]
}

fn iri(model: &str, id: i64) -> String {
    ontology::entity_iri(model, id)
}

fn get<'r>(row: &'r Row, key: &str) -> Option<&'r str> {
    row.get(key).map(String::as_str)
}

/// The rows of `rows` naming `entity` under `key`.
fn rows_of<'r>(rows: &'r [Row], key: &str, entity: &str) -> Vec<&'r Row> {
    rows.iter()
        .filter(|r| get(r, key) == Some(entity))
        .collect()
}

fn one<'r>(rows: &'r [Row], key: &str, entity: &str) -> &'r Row {
    let found = rows_of(rows, key, entity);
    assert_eq!(found.len(), 1, "exactly one row for {entity}: {found:?}");
    found[0]
}

/// The one row matching every `(key, value)` pair.
fn one_by<'r>(rows: &'r [Row], pairs: &[(&str, &str)]) -> &'r Row {
    let found: Vec<&Row> = rows
        .iter()
        .filter(|r| pairs.iter().all(|(k, v)| get(r, k) == Some(*v)))
        .collect();
    assert_eq!(found.len(), 1, "exactly one row for {pairs:?}: {found:?}");
    found[0]
}

fn number(row: &Row, key: &str) -> Option<f64> {
    get(row, key).and_then(|v| v.parse::<f64>().ok())
}

/// Every served row stays inside the exported tab-row vocabulary (keys and enum values), and
/// the keys the builder adds are all served somewhere in the tab.
fn assert_within_vocabulary(tab: Tab, rows: &[Row], added: &[&str]) {
    let vocabulary = tabs::row_vocabulary();
    let vocab = &vocabulary[tab.name()];
    let mut served = BTreeSet::new();
    for row in rows {
        for (key, value) in row {
            assert!(
                vocab.keys.contains(key),
                "{}: `{key}` served outside the vocabulary: {row:?}",
                tab.name()
            );
            if let Some(allowed) = vocab.enums.get(key) {
                assert!(
                    allowed.contains(value),
                    "{}: {key}={value:?} outside the vocabulary",
                    tab.name()
                );
            }
            served.insert(key.as_str());
        }
    }
    for key in added {
        assert!(served.contains(key), "{}: `{key}` never served", tab.name());
    }
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn the_tabs_project_the_plant_and_light_the_points_chain() {
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
    let today = Day::today();
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in fixtures(today) {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }
    let client = GraphDbClient::new(&base, REPO).expect("query client");
    client
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive ekg:fulfills");

    // The at-risk set, as the monitor decides it for supply risk: the rule's own subjects.
    let at_risk: BTreeSet<String> = world
        .select(ontology::RISK_RULE_SELECT_SPARQL)
        .await
        .expect("risk rule")
        .iter()
        .filter_map(|row| row.get("so").cloned())
        .collect();
    let (so1, so2, so3, so4) = (
        iri("sale.order", 1),
        iri("sale.order", 2),
        iri("sale.order", 3),
        iri("sale.order", 4),
    );
    assert_eq!(
        at_risk,
        BTreeSet::from([so1.clone(), so3.clone(), so4.clone()]),
        "SO1, SO3 and SO4 are short; SO2 is covered"
    );

    let (customer, product, bom) = (
        iri("res.partner", 1),
        iri("product.product", 100),
        iri("mrp.bom", 20),
    );
    let (battery, frame) = (iri("product.product", 101), iri("product.product", 102));
    let (celltech, voltpack, frameco) = (
        iri("res.partner", 2),
        iri("res.partner", 3),
        iri("res.partner", 4),
    );
    let (po_on_time, po_late) = (iri("purchase.order", 60), iri("purchase.order", 61));
    let received: Vec<String> = [62, 63, 64, 65, 68, 69]
        .into_iter()
        .map(|id| iri("purchase.order", id))
        .collect();
    let (po_two_line, po_partial) = (iri("purchase.order", 66), iri("purchase.order", 67));
    let (trike, drive_unit, motor) = (
        iri("product.product", 200),
        iri("product.product", 201),
        iri("product.product", 202),
    );
    let (trike_bom, drive_bom) = (iri("mrp.bom", 21), iri("mrp.bom", 22));
    let motorworks = iri("res.partner", 5);
    let mo = iri("mrp.production", 40);
    let subject = EntityUri(so1.clone());
    let project = |tab: Tab, subject: EntityUri| {
        let (world, at_risk) = (&world, &at_risk);
        async move {
            let projection = tabs::project_on(world, &subject, tab, Some(at_risk), today)
                .await
                .expect("tab projected");
            assert!(
                projection.warnings.is_empty(),
                "no degraded marker: {:?}",
                projection.warnings
            );
            projection.rows
        }
    };

    // --- Graph ---
    let graph = project(Tab::Graph, subject.clone()).await;
    let nodes: Vec<&Row> = graph
        .iter()
        .filter(|r| get(r, "kind") == Some("node"))
        .collect();
    let roles: BTreeSet<&str> = nodes.iter().filter_map(|r| get(r, "role")).collect();
    assert_eq!(
        roles,
        BTreeSet::from([
            "Customer",
            "SalesOrder",
            "Product",
            "BOM",
            "Component",
            "Supplier",
            "PurchaseOrder"
        ]),
        "all six columns are served"
    );
    let role_of = |iri: &str| get(one(&graph, "node", iri), "role");
    assert_eq!(role_of(&battery), Some("Component"));
    assert_eq!(role_of(&product), Some("Product"));
    let on_path: BTreeSet<&str> = nodes
        .iter()
        .filter(|r| get(r, "on_path") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(
        on_path,
        BTreeSet::from([
            customer.as_str(),
            so1.as_str(),
            product.as_str(),
            bom.as_str(),
            battery.as_str(),
            voltpack.as_str(),
            po_late.as_str()
        ]),
        "the full chain: the backup and its late PO, not the on-time primary"
    );
    let causes: BTreeSet<&str> = nodes
        .iter()
        .filter(|r| get(r, "cause") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(causes, BTreeSet::from([battery.as_str(), po_late.as_str()]));
    assert_eq!(get(one(&graph, "node", &so1), "at_risk"), Some("true"));
    assert_eq!(get(one(&graph, "node", &so2), "at_risk"), Some("false"));
    assert_eq!(
        get(one(&graph, "node", &po_on_time), "on_path"),
        Some("false")
    );
    assert_eq!(
        get(one(&graph, "node", &po_on_time), "cause"),
        Some("false")
    );
    assert_eq!(get(one(&graph, "node", &frame), "cause"), Some("false"));
    for history in &received {
        assert!(
            rows_of(&graph, "node", history).is_empty(),
            "a PO with nothing still inbound is not drawn: {history}"
        );
    }
    // A still-open PO draws a product edge only for its open line: PO 67's fully received motor
    // line is not inbound.
    let po_edges: BTreeSet<&str> = graph
        .iter()
        .filter(|r| get(r, "kind") == Some("edge") && get(r, "s") == Some(po_partial.as_str()))
        .filter(|r| get(r, "p").is_some_and(|p| p.ends_with("po_line_product")))
        .filter_map(|r| get(r, "o"))
        .collect();
    assert_eq!(
        po_edges,
        BTreeSet::from([frame.as_str()]),
        "only the open line"
    );
    // P60 keeps its node and its open battery edge, but its fully received frame line (no
    // picking) draws no edge. Both ends are admitted nodes, so only the open-line join
    // keeps it out.
    assert_eq!(
        get(one(&graph, "node", &po_on_time), "role"),
        Some("PurchaseOrder")
    );
    let p60_edges: BTreeSet<&str> = graph
        .iter()
        .filter(|r| get(r, "kind") == Some("edge") && get(r, "s") == Some(po_on_time.as_str()))
        .filter(|r| get(r, "p").is_some_and(|p| p.ends_with("po_line_product")))
        .filter_map(|r| get(r, "o"))
        .collect();
    assert_eq!(
        p60_edges,
        BTreeSet::from([battery.as_str()]),
        "only P60's open line"
    );
    assert_eq!(get(one(&graph, "node", &po_late), "label"), Some("P61"));
    for row in &graph {
        for key in ["node", "s", "o"] {
            if let Some(value) = get(row, key) {
                assert!(
                    ontology::is_canonical_entity_iri(value),
                    "no provenance or metamodel node: {row:?}"
                );
            }
        }
    }
    let lit_edges: Vec<(&str, &str)> = graph
        .iter()
        .filter(|r| get(r, "kind") == Some("edge") && get(r, "on_path") == Some("true"))
        .filter_map(|r| Some((get(r, "s")?, get(r, "o")?)))
        .collect();
    for edge in [
        (so1.as_str(), customer.as_str()),
        (po_late.as_str(), voltpack.as_str()),
        (voltpack.as_str(), battery.as_str()),
        (bom.as_str(), battery.as_str()),
    ] {
        assert!(lit_edges.contains(&edge), "{edge:?} lit in {lit_edges:?}");
    }

    // --- Flow ---
    let flow = project(Tab::Flow, subject.clone()).await;
    let mut lit_per_stage: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for row in &flow {
        let stage = get(row, "stage").expect("stage");
        let entry = lit_per_stage.entry(stage).or_default();
        if get(row, "on_path") == Some("true") {
            entry.insert(get(row, "entity").expect("entity"));
        }
    }
    assert_eq!(
        lit_per_stage,
        BTreeMap::from([
            ("suppliers", BTreeSet::from([voltpack.as_str()])),
            ("on_order", BTreeSet::from([po_late.as_str()])),
            ("in_stock", BTreeSet::from([battery.as_str()])),
            ("in_production", BTreeSet::from([mo.as_str()])),
            ("promised", BTreeSet::from([so1.as_str()])),
        ]),
        "all five stages, exactly one on-path entity each"
    );
    let on_order: BTreeSet<&str> = flow
        .iter()
        .filter(|r| get(r, "stage") == Some("on_order"))
        .filter_map(|r| get(r, "entity"))
        .collect();
    assert_eq!(
        on_order,
        BTreeSet::from([
            po_on_time.as_str(),
            po_late.as_str(),
            po_two_line.as_str(),
            po_partial.as_str()
        ]),
        "only open inbound is on order (68: nothing open; 69: no shipment can bring it)"
    );
    // Only the open rest of a partly received line is on order, arriving at its open
    // shipment's ETA.
    let partial = one_by(
        &flow,
        &[
            ("stage", "on_order"),
            ("entity", &po_partial),
            ("product", &frame),
        ],
    );
    assert_eq!(
        number(partial, "quantity"),
        Some(5.0),
        "8 ordered - 3 received"
    );
    assert_eq!(
        get(partial, "arrival"),
        Some(at(today, 6, "12:00:00").as_str())
    );
    assert!(
        rows_of(&flow, "entity", &po_partial)
            .iter()
            .all(|r| get(r, "product") != Some(motor.as_str())),
        "a fully received line is not on order"
    );
    let battery_tile = one(&flow, "entity", &battery);
    assert_eq!(get(battery_tile, "stage"), Some("in_stock"));
    assert_eq!(
        get(battery_tile, "on_hand").and_then(|v| v.parse::<f64>().ok()),
        Some(2.0)
    );
    assert_eq!(
        get(battery_tile, "safety").and_then(|v| v.parse::<f64>().ok()),
        Some(5.0)
    );
    assert_eq!(
        get(one(&flow, "entity", &voltpack), "sourcing"),
        Some("backup")
    );
    assert_eq!(get(one(&flow, "entity", &po_late), "cause"), Some("true"));
    assert_within_vocabulary(
        Tab::Flow,
        &flow,
        &["at_risk", "on_path", "cause", "sourcing"],
    );
    assert_within_vocabulary(Tab::Graph, &graph, &["at_risk", "on_path", "cause"]);

    // --- a process point lights itself and its order's chain ---
    let mo_flow = project(Tab::Flow, EntityUri(mo.clone())).await;
    assert_eq!(get(one(&mo_flow, "entity", &mo), "on_path"), Some("true"));
    assert_eq!(get(one(&mo_flow, "entity", &so1), "on_path"), Some("true"));
    assert_eq!(
        get(one(&mo_flow, "entity", &battery), "on_path"),
        Some("false"),
        "a process point has no supply hops"
    );
    let mo_graph = project(Tab::Graph, EntityUri(mo.clone())).await;
    for lit in [&customer, &so1, &product, &bom] {
        assert_eq!(
            get(one(&mo_graph, "node", lit), "on_path"),
            Some("true"),
            "{lit}"
        );
    }

    // --- Suppliers ---
    let suppliers = project(Tab::Suppliers, subject.clone()).await;
    let table = |supplier: &str| {
        suppliers
            .iter()
            .find(|r| get(r, "kind") == Some("supplier") && get(r, "supplier") == Some(supplier))
            .unwrap_or_else(|| panic!("supplier row for {supplier}"))
    };
    assert_eq!(get(table(&celltech), "sourcing"), Some("primary"));
    assert_eq!(get(table(&voltpack), "sourcing"), Some("backup"));
    assert_eq!(get(table(&frameco), "sourcing"), Some("single"));
    assert_eq!(get(table(&celltech), "lead_time"), Some("45"));
    assert_eq!(
        get(table(&celltech), "otd"),
        Some("0.6667"),
        "2 of 3 in-window receipts on time vs the planned day (rescheduled-late is late, same-day is on time)"
    );
    assert!(
        !table(&voltpack).contains_key("otd"),
        "no receipts, so no OTD"
    );
    assert_eq!(get(table(&voltpack), "on_path"), Some("true"));
    assert_eq!(get(table(&celltech), "on_path"), Some("false"));
    let exposure = |supplier: &str| {
        get(table(supplier), "exposure")
            .and_then(|v| v.parse::<f64>().ok())
            .expect("exposure served")
    };
    assert!(
        (exposure(&celltech) - 18_000.0).abs() < 0.01,
        "sources the cause"
    );
    assert!(
        (exposure(&voltpack) - 18_000.0).abs() < 0.01,
        "sources the cause"
    );
    assert!(
        exposure(&frameco).abs() < 0.01,
        "the frame is not a cause, so FrameCo carries no exposure"
    );
    assert!(
        (exposure(&motorworks) - 9_000.0).abs() < 0.01,
        "MotorWorks sources SO3's cause"
    );
    let open_pos: BTreeSet<&str> = suppliers
        .iter()
        .filter(|r| get(r, "kind") == Some("open_po"))
        .filter_map(|r| get(r, "po"))
        .collect();
    assert_eq!(
        open_pos,
        BTreeSet::from([
            po_on_time.as_str(),
            po_late.as_str(),
            po_two_line.as_str(),
            po_partial.as_str()
        ]),
        "a PO with nothing still inbound is never listed as open"
    );
    let partial = one_by(
        &suppliers,
        &[
            ("kind", "open_po"),
            ("po", &po_partial),
            ("product", &frame),
        ],
    );
    assert_eq!(number(partial, "quantity"), Some(5.0), "the open rest");
    assert_eq!(
        get(partial, "arrival"),
        Some(at(today, 6, "12:00:00").as_str())
    );
    let frame_stock = one_by(
        &suppliers,
        &[
            ("kind", "stock"),
            ("supplier", &frameco),
            ("stock_of", &frame),
        ],
    );
    assert_eq!(
        number(frame_stock, "demand"),
        Some(26.0),
        "SO1 10 x 1 + SO3 4 x 2: each order only through its own product's BOM lines"
    );
    let battery_stock = suppliers
        .iter()
        .find(|r| {
            get(r, "kind") == Some("stock")
                && get(r, "supplier") == Some(celltech.as_str())
                && get(r, "stock_of") == Some(battery.as_str())
        })
        .expect("CellTech's battery stock row");
    assert_eq!(
        get(battery_stock, "demand").and_then(|v| v.parse::<f64>().ok()),
        Some(10.0),
        "30-day demand counts SO1 (today+10) and not SO2 (today+40)"
    );
    assert_eq!(get(battery_stock, "sourcing"), Some("primary"));
    let dependents: BTreeMap<&str, &str> = suppliers
        .iter()
        .filter(|r| {
            get(r, "kind") == Some("dependent_order") && get(r, "supplier") == Some(&celltech)
        })
        .filter_map(|r| Some((get(r, "order")?, get(r, "at_risk")?)))
        .collect();
    assert_eq!(
        dependents,
        BTreeMap::from([(so1.as_str(), "true"), (so2.as_str(), "false")]),
        "every dependent order, marked at_risk"
    );

    // --- Timeline ---
    let timeline = project(Tab::Timeline, subject.clone()).await;
    let promised: BTreeMap<&str, &str> = timeline
        .iter()
        .filter(|r| get(r, "kind") == Some("promise"))
        .filter_map(|r| Some((get(r, "entity")?, get(r, "on_path")?)))
        .collect();
    assert_eq!(
        promised,
        BTreeMap::from([
            (so1.as_str(), "true"),
            (so2.as_str(), "false"),
            (so3.as_str(), "false"),
            (so4.as_str(), "false")
        ]),
        "every promised order, the subject on its path"
    );
    let ready = timeline
        .iter()
        .find(|r| get(r, "kind") == Some("ready") && get(r, "entity") == Some(so1.as_str()))
        .expect("SO1's ready date");
    assert_eq!(get(ready, "date"), Some(at(today, 8, "12:00:00").as_str()));
    let inbound: BTreeSet<&str> = timeline
        .iter()
        .filter(|r| get(r, "kind") == Some("inbound"))
        .filter_map(|r| get(r, "entity"))
        .collect();
    assert_eq!(
        inbound,
        BTreeSet::from([
            po_on_time.as_str(),
            po_late.as_str(),
            po_two_line.as_str(),
            po_partial.as_str()
        ]),
        "only open inbound is drawn as an arrival"
    );
    let partial = one_by(
        &timeline,
        &[
            ("kind", "inbound"),
            ("entity", &po_partial),
            ("product", &frame),
        ],
    );
    assert_eq!(number(partial, "quantity"), Some(5.0), "the open rest");
    assert_eq!(
        get(partial, "date"),
        Some(at(today, 6, "12:00:00").as_str())
    );
    assert_within_vocabulary(Tab::Timeline, &timeline, &["at_risk", "on_path", "cause"]);
    assert_within_vocabulary(
        Tab::Suppliers,
        &suppliers,
        &["at_risk", "on_path", "cause", "sourcing", "otd", "exposure"],
    );

    // --- the two-level point: one cause, one lit path, one lit PO line ---
    // The inbox reason is the rule's first row for the order (the monitor opens the point
    // with it); the chain must light that same component: the purchased motor, although
    // the made Drive Unit is the larger shortfall and the lower IRI.
    let rule_rows = world
        .select(ontology::RISK_RULE_SELECT_SPARQL)
        .await
        .expect("risk rule");
    let first_cause = |order: &str| {
        rule_rows
            .iter()
            .find(|r| get(r, "so") == Some(order))
            .and_then(|r| get(r, "comp"))
            .expect("a flagged order has a first cause")
    };
    assert_eq!(first_cause(&so1), battery.as_str());
    assert_eq!(first_cause(&so3), motor.as_str(), "purchased before made");
    let so3_causes: BTreeSet<&str> = rule_rows
        .iter()
        .filter(|r| get(r, "so") == Some(so3.as_str()))
        .filter_map(|r| get(r, "comp"))
        .collect();
    assert_eq!(
        so3_causes,
        BTreeSet::from([drive_unit.as_str(), motor.as_str()])
    );
    let lit_component = |rows: &[Row]| -> Vec<String> {
        rows.iter()
            .filter(|r| get(r, "role") == Some("Component") && get(r, "on_path") == Some("true"))
            .filter_map(|r| get(r, "node").map(str::to_owned))
            .collect()
    };
    assert_eq!(
        lit_component(&graph),
        std::slice::from_ref(&battery),
        "SO1: the reason's component"
    );

    let so3_subject = EntityUri(so3.clone());
    let so3_graph = project(Tab::Graph, so3_subject.clone()).await;
    assert_eq!(
        lit_component(&so3_graph),
        std::slice::from_ref(&motor),
        "SO3: the reason's component"
    );
    let so3_lit: BTreeSet<&str> = so3_graph
        .iter()
        .filter(|r| get(r, "kind") == Some("node") && get(r, "on_path") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(
        so3_lit,
        BTreeSet::from([
            customer.as_str(),
            so3.as_str(),
            trike.as_str(),
            trike_bom.as_str(),
            drive_unit.as_str(),
            drive_bom.as_str(),
            motor.as_str(),
            motorworks.as_str(),
            po_two_line.as_str()
        ]),
        "one path down through the sub-assembly to the deep cause"
    );
    assert_eq!(
        get(one_by(&so3_graph, &[("node", &drive_unit)]), "role"),
        Some("Product")
    );
    assert_eq!(
        get(one_by(&so3_graph, &[("node", &drive_unit)]), "cause"),
        Some("true"),
        "the made shortfall is still a cause"
    );
    let so3_lit_edges: BTreeSet<(&str, &str)> = so3_graph
        .iter()
        .filter(|r| get(r, "kind") == Some("edge") && get(r, "on_path") == Some("true"))
        .filter_map(|r| Some((get(r, "s")?, get(r, "o")?)))
        .collect();
    for edge in [
        (trike_bom.as_str(), trike.as_str()),
        (trike_bom.as_str(), drive_unit.as_str()),
        (drive_bom.as_str(), drive_unit.as_str()),
        (drive_bom.as_str(), motor.as_str()),
        (motorworks.as_str(), motor.as_str()),
        (po_two_line.as_str(), motorworks.as_str()),
        (po_two_line.as_str(), motor.as_str()),
    ] {
        assert!(
            so3_lit_edges.contains(&edge),
            "{edge:?} lit: no gap in {so3_lit_edges:?}"
        );
    }
    assert!(
        !so3_lit_edges.contains(&(po_two_line.as_str(), frame.as_str())),
        "the chain PO's non-cause line is not lit"
    );

    // Per (PO, product): only the chain PO's cause line is on the path and a cause.
    let so3_flow = project(Tab::Flow, so3_subject.clone()).await;
    let so3_timeline = project(Tab::Timeline, so3_subject.clone()).await;
    let so3_suppliers = project(Tab::Suppliers, so3_subject.clone()).await;
    for (rows, kind_key, kind, po_key) in [
        (&so3_flow, "stage", "on_order", "entity"),
        (&so3_timeline, "kind", "inbound", "entity"),
        (&so3_suppliers, "kind", "open_po", "po"),
    ] {
        let line = |product: &str| {
            one_by(
                rows,
                &[
                    (kind_key, kind),
                    (po_key, &po_two_line),
                    ("product", product),
                ],
            )
        };
        assert_eq!(
            get(line(&motor), "on_path"),
            Some("true"),
            "{kind}: the cause line"
        );
        assert_eq!(get(line(&motor), "cause"), Some("true"), "{kind}: late");
        assert_eq!(
            get(line(&frame), "on_path"),
            Some("false"),
            "{kind}: another line"
        );
        assert_eq!(
            get(line(&frame), "cause"),
            Some("false"),
            "{kind}: another line"
        );
    }
    assert_eq!(
        get(
            one_by(&so3_flow, &[("stage", "in_stock"), ("entity", &motor)]),
            "on_path"
        ),
        Some("true")
    );
    // SO3's Flow lights the purchased cause's whole supply chain.
    let lit_in = |rows: &[Row], stage: &str| -> BTreeSet<String> {
        rows.iter()
            .filter(|r| get(r, "stage") == Some(stage) && get(r, "on_path") == Some("true"))
            .filter_map(|r| get(r, "entity").map(str::to_owned))
            .collect()
    };
    for (stage, lit) in [
        ("suppliers", &motorworks),
        ("on_order", &po_two_line),
        ("in_stock", &motor),
        ("promised", &so3),
    ] {
        assert_eq!(
            lit_in(&so3_flow, stage),
            BTreeSet::from([lit.clone()]),
            "SO3 {stage}"
        );
    }
    let motor_stock = one_by(
        &so3_suppliers,
        &[
            ("kind", "stock"),
            ("supplier", &motorworks),
            ("stock_of", &motor),
        ],
    );
    assert_eq!(
        number(motor_stock, "demand"),
        Some(4.0),
        "4 trikes x 1 motor"
    );

    // --- only made causes, on a multi-line order: SO4 ---
    let (wheel_kit, wheel) = (iri("product.product", 199), iri("product.product", 211));
    let wheel_kit_bom = iri("mrp.bom", 27);
    let (mo_trike, mo_wheel_kit) = (iri("mrp.production", 37), iri("mrp.production", 41));
    assert_eq!(
        first_cause(&so4),
        wheel.as_str(),
        "made causes: the larger shortfall"
    );
    let so4_subject = EntityUri(so4.clone());
    let so4_graph = project(Tab::Graph, so4_subject.clone()).await;
    let so4_lit: BTreeSet<&str> = so4_graph
        .iter()
        .filter(|r| get(r, "kind") == Some("node") && get(r, "on_path") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(
        so4_lit,
        BTreeSet::from([
            customer.as_str(),
            so4.as_str(),
            wheel_kit.as_str(),
            wheel_kit_bom.as_str(),
            wheel.as_str()
        ]),
        "the kept product's own BOM (not the Trike's lower-IRI BOM 21), and the made cause"
    );
    assert_eq!(
        get(one_by(&so4_graph, &[("node", &wheel)]), "role"),
        Some("Product")
    );
    for row in so4_graph.iter().filter(|r| {
        matches!(
            get(r, "role"),
            Some("Component" | "Supplier" | "PurchaseOrder")
        )
    }) {
        assert_eq!(get(row, "on_path"), Some("false"), "no supply hop: {row:?}");
    }
    let so4_lit_edges: BTreeSet<(&str, &str)> = so4_graph
        .iter()
        .filter(|r| get(r, "kind") == Some("edge") && get(r, "on_path") == Some("true"))
        .filter_map(|r| Some((get(r, "s")?, get(r, "o")?)))
        .collect();
    assert!(
        so4_lit_edges.contains(&(wheel_kit_bom.as_str(), wheel.as_str())),
        "BOM 27 to the made cause: no gap in {so4_lit_edges:?}"
    );
    let so4_flow = project(Tab::Flow, so4_subject.clone()).await;
    assert_eq!(
        lit_in(&so4_flow, "in_production"),
        BTreeSet::from([mo_wheel_kit.clone()]),
        "the MO building the kept product, not the lower-IRI Trike MO"
    );
    // MO 37 as the subject lights the product it builds and its own BOM.
    let mo_trike_graph = project(Tab::Graph, EntityUri(mo_trike.clone())).await;
    let mo_trike_lit: BTreeSet<&str> = mo_trike_graph
        .iter()
        .filter(|r| get(r, "kind") == Some("node") && get(r, "on_path") == Some("true"))
        .filter_map(|r| get(r, "node"))
        .collect();
    assert_eq!(
        mo_trike_lit,
        BTreeSet::from([
            customer.as_str(),
            so4.as_str(),
            trike.as_str(),
            trike_bom.as_str()
        ]),
        "a process point lights what it builds"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;
}

const SCHEDULE_REPO: &str = "ekg_tabs_schedule_it";

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn a_work_centres_schedule_lists_the_work_orders_of_every_order_on_it() {
    let _slot = common::fixture_repo_slot().await.expect("fixture slot");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, SCHEDULE_REPO).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, SCHEDULE_REPO).expect("driver client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    let work_order = |id: i64, mo: i64, wc: i64| {
        rec(
            "mrp.workorder",
            id,
            &[
                ("production_id", &format!("[{mo}]")),
                ("workcenter_id", &format!("[{wc}]")),
                ("date_start", "2027-10-23T08:00:00"),
                ("date_finished", "2027-10-23T10:00:00"),
                ("duration_expected", "60"),
            ],
        )
    };
    // Two orders share the Assembly Line; a third order runs on the Paint Booth; the Test
    // Bench has nothing scheduled.
    for record in [
        rec("mrp.production", 1, &[("origin", "S00001")]),
        rec("mrp.production", 2, &[("origin", "S00002")]),
        rec("mrp.production", 3, &[("origin", "S00003")]),
        rec("mrp.workcenter", 1, &[("name", "Assembly Line")]),
        rec("mrp.workcenter", 2, &[("name", "Paint Booth")]),
        rec("mrp.workcenter", 3, &[("name", "Test Bench")]),
        work_order(11, 1, 1),
        work_order(12, 2, 1),
        work_order(31, 3, 2),
    ] {
        world
            .materialize(&record)
            .await
            .expect("materialize fixture");
    }
    let client = GraphDbClient::new(&base, SCHEDULE_REPO).expect("query client");
    for derivation in ontology::STRUCTURAL_DERIVATIONS {
        client.update(derivation.sparql).await.expect("derive");
    }

    let centre = |n: i64| EntityUri(iri("mrp.workcenter", n));
    let schedule = |n: i64| {
        let world = &world;
        async move {
            tabs::work_centre_schedule(world, &centre(n))
                .await
                .expect("schedule")
        }
    };
    assert_eq!(
        schedule(1).await,
        [
            EntityUri(iri("mrp.workorder", 11)),
            EntityUri(iri("mrp.workorder", 12))
        ],
        "both orders' work orders"
    );
    assert_eq!(schedule(2).await, [EntityUri(iri("mrp.workorder", 31))]);
    assert!(schedule(3).await.is_empty(), "nothing is scheduled there");
    // Only a work centre has a schedule.
    assert!(
        tabs::work_centre_schedule(&world, &EntityUri(iri("mrp.production", 1)))
            .await
            .expect("schedule")
            .is_empty()
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{SCHEDULE_REPO}"))
        .send()
        .await;
}
