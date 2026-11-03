//! Seed the flagship "delivery-promise & supply-risk" dataset into Odoo.
//!
//! Loads one coherent at-risk scenario the supply-risk capability traverses end to end:
//!
//! ```text
//! SalesOrder --line--> E-Bike Model X --BOM--> Drive Subassembly --BOM--> Motor / Controller / Gear
//!    (commitment_date)          (finished good)        (sub-assembly, level 2)     (purchased parts)
//!                                    `----------------> Frame / Battery (purchased parts, level 1)
//!
//! Battery: single-sourced (CellTech, 45-day lead), on-hand below safety stock, and the
//! only inbound PO lands after the promised date, so the promise is at risk.
//! ```
//!
//! Every record is registered under the `ekg_seed` external-ID namespace, so
//! the whole script is idempotent: re-run it to update values in place
//! rather than duplicate the scenario.
//!
//! Run against the local substrate (`just podman-local-up`):
//! ```bash
//! cargo run -p ekg-odoo-seed
//! # or point it elsewhere:
//! ODOO_URL=http://localhost:8069 ODOO_DB=ekg ODOO_USER=admin ODOO_API_KEY=admin \
//!   cargo run -p ekg-odoo-seed
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use odoo_client::Odoo;
use serde_json::json;

/// A seeded product: its template id and its (auto-created) variant id.
///
/// Odoo's MRP models mix the two: BOM headers/suppliers key off the template,
/// while BOM lines, stock, orderpoints and order lines key off the variant.
#[derive(Clone, Copy)]
struct Prod {
    tmpl: i64,
    variant: i64,
}

/// The brake module's designed on-hand: none. The trike order depends entirely on its
/// inbound, so slipping that inbound (`just transport-sim`) leaves it short by 10.
const BRAKE_ON_HAND: f64 = 0.0;

// A seed script is one linear recipe; splitting it would scatter the scenario.
#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let url = env_or("ODOO_URL", "http://localhost:8069");
    let db = env_or("ODOO_DB", "ekg");
    let user = env_or("ODOO_USER", "admin");
    // Fresh `-i base` installs use admin/admin; a real deployment sets an API key.
    let secret = env_or("ODOO_API_KEY", "admin");

    println!("connecting to Odoo at {url} (db={db}, user={user})");
    let odoo = Odoo::connect(&url, &db, &user, &secret)?;
    println!("  authenticated to db {db} via API key (json2)");

    // --- static references resolved by external ID (stable across DB rebuilds) ---
    let uom = odoo.xmlid("uom", "product_uom_unit")?;
    let route_make = odoo.xmlid("mrp", "route_warehouse0_manufacture")?;
    let route_buy = odoo.xmlid("purchase_stock", "route_warehouse0_buy")?;
    let stock_loc = odoo.xmlid("stock", "stock_location_stock")?;
    let warehouse = odoo.xmlid("stock", "warehouse0")?;

    // --- helpers: keep the scenario below reading as data, not RPC calls ---
    let product =
        |xid: &str, name: &str, code: &str, routes: &[i64], purchase_ok: bool| -> Result<Prod> {
            let tmpl = odoo.ensure(
                xid,
                "product.template",
                json!({
                    "name": name,
                    "default_code": code,
                    "type": "consu",          // Odoo 19: "Goods"
                    "is_storable": true,       // makes the goods inventory-tracked
                    "purchase_ok": purchase_ok,
                    "uom_id": uom,
                    "route_ids": [[6, 0, routes]],
                }),
            )?;
            let variant = odoo.read_m2o("product.template", tmpl, "product_variant_id")?;
            Ok(Prod { tmpl, variant })
        };

    let bom = |xid: &str, prod: Prod, produce_delay: i64| -> Result<i64> {
        odoo.ensure(
            xid,
            "mrp.bom",
            json!({
                "product_tmpl_id": prod.tmpl,
                // Bind the BOM to the specific variant too: the world-model contract
                // maps mrp.bom.product_id to ekg:produces_product, and the risk rule
                // walks that edge to the finished variant the order line references.
                // With only product_tmpl_id set, product_id is empty and the edge,
                // and with it the whole detection path, never materialises.
                "product_id": prod.variant,
                "product_qty": 1,
                "type": "normal",           // manufacture this product (not a kit)
                "produce_delay": produce_delay,
            }),
        )
    };

    let bom_line = |xid: &str, bom_id: i64, part: Prod, qty: f64| -> Result<i64> {
        odoo.ensure(
            xid,
            "mrp.bom.line",
            json!({
                "bom_id": bom_id,
                "product_id": part.variant,
                "product_qty": qty,
                "product_uom_id": uom,
            }),
        )
    };

    let partner = |xid: &str, name: &str, supplier_rank: i64, customer_rank: i64| -> Result<i64> {
        odoo.ensure(
            xid,
            "res.partner",
            json!({
                "name": name,
                "is_company": true,
                "supplier_rank": supplier_rank,   // >0 means ekg:Supplier
                "customer_rank": customer_rank,   // >0 means ekg:Customer
            }),
        )
    };

    let seller = |xid: &str,
                  part: Prod,
                  partner_id: i64,
                  delay: i64,
                  price: f64,
                  sequence: i64|
     -> Result<i64> {
        odoo.ensure(
            xid,
            "product.supplierinfo",
            json!({
                "product_tmpl_id": part.tmpl,
                // The variant too: the contract maps `product_id` to ekg:supplies_product,
                // which the Suppliers tab's stock / sourcing / exposure reads join on.
                "product_id": part.variant,
                "partner_id": partner_id,
                "delay": delay,        // lead time in days ("the plan")
                "min_qty": 1,
                "price": price,
                "sequence": sequence,  // lower = preferred source
            }),
        )
    };

    let quant = |xid: &str, part: Prod, qty: f64| -> Result<i64> {
        odoo.ensure(
            xid,
            "stock.quant",
            json!({
                "product_id": part.variant,
                "location_id": stock_loc,
                "quantity": qty,
            }),
        )
    };

    let orderpoint = |xid: &str, part: Prod, min: f64, max: f64| -> Result<i64> {
        odoo.ensure(
            xid,
            "stock.warehouse.orderpoint",
            json!({
                "product_id": part.variant,
                "location_id": stock_loc,
                "warehouse_id": warehouse,
                "product_min_qty": min,     // safety stock
                "product_max_qty": max,
            }),
        )
    };

    // --- dates (relative to today; re-running keeps the scenario "fresh") ---
    let today = ymd_plus(0)?;
    let commitment = ymd_plus(21)?; // customer promise: 3 weeks out
    let battery_eta = ymd_plus(45)?; // only battery inbound lands well after the promise

    // --- the scenario ---
    println!("seeding products (multi-level BOM)...");
    // Level 0: finished good; level 1: sub-assembly; leaves: purchased components.
    let ebike = product(
        "prod_ebike",
        "E-Bike Model X",
        "FG-EBIKE-X",
        &[route_make],
        false,
    )?;
    let drive = product(
        "prod_drive",
        "Drive Subassembly",
        "SUB-DRIVE",
        &[route_make],
        false,
    )?;
    let frame = product(
        "prod_frame",
        "Aluminium Frame",
        "COMP-FRAME",
        &[route_buy],
        true,
    )?;
    let battery = product(
        "prod_battery",
        "Battery Pack 500Wh",
        "COMP-BATTERY",
        &[route_buy],
        true,
    )?;
    let motor = product(
        "prod_motor",
        "Hub Motor 250W",
        "COMP-MOTOR",
        &[route_buy],
        true,
    )?;
    let controller = product(
        "prod_controller",
        "Motor Controller",
        "COMP-CTRL",
        &[route_buy],
        true,
    )?;
    let gear = product("prod_gear", "Gear Set", "COMP-GEAR", &[route_buy], true)?;

    println!("seeding BOMs + edges...");
    // Level 0 BOM: E-Bike from Drive sub-assembly + Frame + Battery
    let bom_ebike = bom("bom_ebike", ebike, 5)?;
    bom_line("bomline_ebike_drive", bom_ebike, drive, 1.0)?;
    bom_line("bomline_ebike_frame", bom_ebike, frame, 1.0)?;
    bom_line("bomline_ebike_battery", bom_ebike, battery, 1.0)?;
    // Level 1 BOM: Drive from Motor + Controller + 2 Gear, the multi-level depth
    let bom_drive = bom("bom_drive", drive, 3)?;
    bom_line("bomline_drive_motor", bom_drive, motor, 1.0)?;
    bom_line("bomline_drive_controller", bom_drive, controller, 1.0)?;
    bom_line("bomline_drive_gear", bom_drive, gear, 2.0)?;

    println!("seeding partners (suppliers + customer)...");
    let celltech = partner("partner_celltech", "CellTech GmbH", 1, 0)?;
    let frameco = partner("partner_frameco", "FrameCo Metals", 1, 0)?;
    let motorworks = partner("partner_motorworks", "MotorWorks AG", 1, 0)?;
    let driveline = partner("partner_driveline", "Driveline Alt Supply", 1, 0)?;
    let partshub = partner("partner_partshub", "PartsHub Trading", 1, 0)?;
    let velomax = partner("partner_velomax", "VeloMax Retail", 0, 1)?;

    println!("seeding supplier links (lead times; battery single-sourced)...");
    // Battery: one supplier, 45-day lead. The single-source, long-lead risk driver.
    seller("seller_battery", battery, celltech, 45, 320.0, 1)?;
    seller("seller_frame", frame, frameco, 14, 180.0, 1)?;
    // Motor: two suppliers, so multi-sourced, unlike the battery.
    seller("seller_motor_primary", motor, motorworks, 21, 95.0, 1)?;
    seller("seller_motor_backup", motor, driveline, 30, 110.0, 2)?;
    seller("seller_controller", controller, partshub, 10, 40.0, 1)?;
    seller("seller_gear", gear, partshub, 12, 25.0, 1)?;

    println!("seeding on-hand stock + safety stock...");
    // Battery on-hand (2) is below both the 10 demanded and the safety stock (5).
    quant("quant_battery", battery, 2.0)?;
    quant("quant_frame", frame, 20.0)?;
    quant("quant_motor", motor, 15.0)?;
    quant("quant_controller", controller, 30.0)?;
    quant("quant_gear", gear, 40.0)?;
    orderpoint("op_battery", battery, 5.0, 20.0)?;
    orderpoint("op_frame", frame, 5.0, 30.0)?;

    println!("seeding the late inbound PO + the at-risk sales order...");
    // Late inbound: 10 batteries, but the ETA is after the customer promise.
    let po = odoo.ensure(
        "po_battery_inbound",
        "purchase.order",
        json!({ "partner_id": celltech, "date_order": today }),
    )?;
    odoo.ensure(
        "po_battery_line",
        "purchase.order.line",
        json!({
            "order_id": po,
            "product_id": battery.variant,
            "name": "Battery Pack 500Wh — inbound",
            "product_qty": 10,
            "product_uom_id": uom,
            "price_unit": 320.0,
            "date_planned": battery_eta,
        }),
    )?;

    // The promise under threat: 10 e-bikes due in 3 weeks.
    let so = odoo.ensure(
        "so_velomax",
        "sale.order",
        json!({
            "partner_id": velomax,
            "date_order": today,
            "commitment_date": commitment,
        }),
    )?;
    odoo.ensure(
        "so_velomax_line",
        "sale.order.line",
        json!({
            "order_id": so,
            "product_id": ebike.variant,
            "name": "E-Bike Model X",
            "product_uom_qty": 10,
            "product_uom_id": uom,
            "price_unit": 1800.0,
        }),
    )?;

    // --- Isolated transport scenario ---
    // An order covered by an on-time inbound shipment. Slipping the shipment's ETA
    // past the promise (via `just transport-sim`) flips it to at risk, which shows
    // the transport signal. Separate from the flagship battery scenario (which stays
    // at risk), so neither disturbs the other.
    println!("seeding the transport scenario (on-time shipment, so a covered baseline)...");
    let transport_eta_ok = ymd_plus(7)?; // shipment ETA well before the promise

    let trike = product(
        "prod_trike",
        "Cargo Trike",
        "FG-TRIKE",
        &[route_make],
        false,
    )?;
    let brake = product(
        "prod_brake",
        "Brake Module",
        "COMP-BRAKE",
        &[route_buy],
        true,
    )?;
    let bom_trike = bom("bom_trike", trike, 3)?;
    bom_line("bomline_trike_brake", bom_trike, brake, 1.0)?;
    seller("seller_brake", brake, partshub, 8, 60.0, 1)?;
    // No on-hand for the brake module: the order depends entirely on the inbound.
    quant("quant_brake", brake, BRAKE_ON_HAND)?;

    // Inbound PO for 10 brake modules, planned on time.
    let po_brake = odoo.ensure(
        "po_brake_inbound",
        "purchase.order",
        json!({ "partner_id": partshub, "date_order": today }),
    )?;
    odoo.ensure(
        "po_brake_line",
        "purchase.order.line",
        json!({
            "order_id": po_brake,
            "product_id": brake.variant,
            "name": "Brake Module — inbound",
            "product_qty": 10,
            "product_uom_id": uom,
            "price_unit": 60.0,
            "date_planned": transport_eta_ok,
        }),
    )?;

    // Confirm the PO so Odoo generates the inbound shipment (stock.picking) with a
    // real `purchase_id` link: that field is computed/readonly, so only the PO flow
    // can set it, not a manual create. Idempotent: only confirm from a draft/sent PO.
    let po_state = odoo
        .read("purchase.order", json!([po_brake]), json!(["state"]))?
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["state"].as_str())
        .unwrap_or("draft")
        .to_owned();
    if po_state == "draft" || po_state == "sent" {
        odoo.call_method(
            "purchase.order",
            "button_confirm",
            json!({ "ids": [po_brake] }),
        )?;
    }

    // Set the generated incoming picking's ETA to the on-time baseline so the
    // order starts covered (the simulator slips it later).
    let picking = *odoo
        .search("stock.picking", json!([["purchase_id", "=", po_brake]]))?
        .first()
        .context(
            "no inbound picking was generated for the brake PO (is purchase_stock installed?)",
        )?;
    odoo.write(
        "stock.picking",
        json!([picking]),
        json!({ "scheduled_date": transport_eta_ok }),
    )?;

    // --- Historical on-time receipt (supplier on-time delivery) ---
    // A past inbound receipt from the same supplier, validated and completed on time, so
    // PartsHub carries a real on-time-delivery figure once `stock.picking.date_done` is
    // materialized as `ekg:completed_at`. How "on time" is kept true across re-seeds is
    // explained where `date_done` is written below. Separate from the open brake picking
    // above (which `just transport-sim` slips), so neither disturbs the other.
    let receipt_scheduled = ymd_plus(-14)?; // scheduled two weeks ago
    let po_history = odoo.ensure(
        "po_brake_history",
        "purchase.order",
        json!({ "partner_id": partshub, "date_order": ymd_plus(-21)? }),
    )?;
    let history_line = odoo.ensure(
        "po_brake_history_line",
        "purchase.order.line",
        json!({
            "order_id": po_history,
            "product_id": brake.variant,
            "name": "Brake Module — historical receipt",
            "product_qty": 5,
            "product_uom_id": uom,
            "price_unit": 60.0,
            "date_planned": receipt_scheduled,
        }),
    )?;
    let po_history_state = odoo
        .read("purchase.order", json!([po_history]), json!(["state"]))?
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["state"].as_str())
        .unwrap_or("draft")
        .to_owned();
    if po_history_state == "draft" || po_history_state == "sent" {
        odoo.call_method(
            "purchase.order",
            "button_confirm",
            json!({ "ids": [po_history] }),
        )?;
    }
    let history_picking = *odoo
        .search("stock.picking", json!([["purchase_id", "=", po_history]]))?
        .first()
        .context("no inbound picking was generated for the historical brake PO")?;
    // Validate the picking (so its state is `done` and the goods really moved) rather than
    // stamping `date_done` onto an open picking: an unvalidated "completion" would be
    // phantom inbound. Idempotent: only an open picking is validated.
    if read_str(&odoo, "stock.picking", history_picking, "state")? != "done" {
        odoo.write(
            "stock.picking",
            json!([history_picking]),
            json!({ "scheduled_date": receipt_scheduled }),
        )?;
        let moves = odoo.search_read(
            "stock.move",
            json!([["picking_id", "=", history_picking]]),
            json!(["product_uom_qty"]),
        )?;
        for mv in moves.as_array().into_iter().flatten() {
            let id = mv["id"].as_i64().context("stock.move without an id")?;
            let qty = mv["product_uom_qty"]
                .as_f64()
                .context("stock.move without a demand quantity")?;
            // Receive the full demand, so validation needs no backorder wizard.
            odoo.write(
                "stock.move",
                json!([id]),
                json!({ "quantity": qty, "picked": true }),
            )?;
        }
        let outcome = odoo.call_method(
            "stock.picking",
            "button_validate",
            json!({ "ids": [history_picking] }),
        )?;
        let state = read_str(&odoo, "stock.picking", history_picking, "state")?;
        // A wizard action (backorder / immediate transfer / lots) instead of `true` means
        // Odoo wanted a confirmation. Name it, so the failure says what blocked it.
        ensure!(
            state == "done",
            "the historical brake receipt #{history_picking} did not validate (state {state}); button_validate returned {outcome}"
        );
    }
    // Validation stamps "now", so the delivered-on day is written back. It is derived every
    // run from what Odoo stores for the PO line's planned date (kept fresh by `ensure` above):
    // the day before it. The served on-time delivery compares the completion day with the
    // purchase order's planned day, so this reads on time. The done transfer's scheduled date
    // (frozen by Odoo at the run that validated it) plays no part, so a re-seed long after
    // validation still lands the completion inside the trailing 90-day window.
    let planned_stored = read_str(&odoo, "purchase.order.line", history_line, "date_planned")?;
    let receipt_done = day_before(&planned_stored)?;
    odoo.write(
        "stock.picking",
        json!([history_picking]),
        json!({ "date_done": receipt_done }),
    )?;
    let done_stored = read_str(&odoo, "stock.picking", history_picking, "date_done")?;
    ensure!(
        done_stored <= planned_stored,
        "the historical receipt's completion {done_stored} is not on time (planned {planned_stored})"
    );
    // The 5 received units are past, consumed history: book the brake on-hand back to the
    // scenario's designed quantity (an inventory adjustment, a real stock move), so the
    // flagship numbers stay as designed (after `just transport-sim`: short by 10).
    restore_on_hand(&odoo, brake.variant, stock_loc, BRAKE_ON_HAND)?;

    // The promise under (future) transport threat: 10 trikes at the same horizon.
    let citylogix = partner("partner_citylogix", "CityLogix Fleets", 0, 1)?;
    let so_trike = odoo.ensure(
        "so_citylogix",
        "sale.order",
        json!({
            "partner_id": citylogix,
            "date_order": today,
            "commitment_date": commitment,
        }),
    )?;
    odoo.ensure(
        "so_citylogix_line",
        "sale.order.line",
        json!({
            "order_id": so_trike,
            "product_id": trike.variant,
            "name": "Cargo Trike",
            "product_uom_qty": 10,
            "product_uom_id": uom,
            "price_unit": 2400.0,
        }),
    )?;
    println!(
        "  transport scenario: CityLogix 10x Cargo Trike due {commitment}; brake inbound picking #{picking} ETA {transport_eta_ok} (on time, so covered). Slip it with `just transport-sim`."
    );
    // Read back what the tabs depend on, so a seed that silently diverged fails here.
    let unlinked = seeded_sourcing_links_without_variant(&odoo)?;
    ensure!(
        unlinked.is_empty(),
        "seeded sourcing links without a product variant: {unlinked:?}"
    );
    println!(
        "  read-back: every seeded sourcing link names its variant; historical brake receipt #{history_picking} is done (delivered {done_stored}, on time); brake on-hand = {BRAKE_ON_HAND}."
    );

    // --- Manufacturing process (process-graph capability) ---
    // Work centers are agents; a shared, capacity-1 work center makes two overlapping
    // work orders contend, and a tight MO window whose routing cannot fit makes an
    // order schedule-infeasible: the two deterministic process-drift signals. MOs are
    // origin-linked to their sales orders (ekg:fulfills matches SO name); dates are
    // anchored to `today`, at hour granularity so the day-2 windows are coherent.
    println!(
        "seeding the manufacturing process (work centers, capacity, operations, MOs, work orders)..."
    );

    let workcenter = |xid: &str,
                      name: &str,
                      code: &str,
                      efficiency: f64,
                      cost: f64|
     -> Result<i64> {
        odoo.ensure(
                xid,
                "mrp.workcenter",
                json!({ "name": name, "code": code, "time_efficiency": efficiency, "costs_hour": cost }),
            )
    };
    let wc_capacity = |xid: &str, wc: i64, part: Prod, capacity: f64| -> Result<i64> {
        odoo.ensure(
            xid,
            "mrp.workcenter.capacity",
            json!({ "workcenter_id": wc, "product_id": part.variant, "capacity": capacity }),
        )
    };
    let operation = |xid: &str,
                     bom_id: i64,
                     wc: i64,
                     name: &str,
                     sequence: i64,
                     minutes: f64|
     -> Result<i64> {
        odoo.ensure(
                xid,
                "mrp.routing.workcenter",
                json!({ "bom_id": bom_id, "workcenter_id": wc, "name": name, "sequence": sequence, "time_cycle_manual": minutes }),
            )
    };
    let production = |xid: &str,
                      prod: Prod,
                      bom_id: i64,
                      qty: f64,
                      origin: &str,
                      start: &str,
                      finish: &str|
     -> Result<i64> {
        odoo.ensure(
                xid,
                "mrp.production",
                json!({ "product_id": prod.variant, "bom_id": bom_id, "product_qty": qty, "origin": origin, "date_start": start, "date_finished": finish }),
            )
    };
    let workorder = |xid: &str,
                     mo: i64,
                     wc: i64,
                     op: i64,
                     name: &str,
                     start: &str,
                     finish: &str,
                     duration: f64|
     -> Result<i64> {
        // Odoo creates a work order per routing operation when the MO is created from its
        // BOM: date that one rather than create a second (and drop a duplicate an earlier seed
        // left), so each operation has exactly one dated work order.
        odoo.adopt(
                xid,
                "mrp.workorder",
                json!([["production_id", "=", mo], ["operation_id", "=", op]]),
                json!({ "production_id": mo, "workcenter_id": wc, "operation_id": op, "name": name, "date_start": start, "date_finished": finish, "duration_expected": duration }),
            )
    };

    // Work centers (agents) + per-product capacity (the source of ekg:capacity).
    let wc_assembly = workcenter("wc_assembly", "Assembly Line", "ASSY", 100.0, 45.0)?;
    let wc_paint = workcenter("wc_paint", "Paint Booth", "PAINT", 90.0, 60.0)?;
    // Assembly capacity 1, so two overlapping assembly work orders exceed it (contention).
    wc_capacity("wccap_assembly", wc_assembly, ebike, 1.0)?;
    wc_capacity("wccap_paint", wc_paint, ebike, 1.0)?;

    // Routing operations on the E-Bike BOM, ordered by sequence (the source of ekg:precedes).
    let op_assemble = operation(
        "op_ebike_assemble",
        bom_ebike,
        wc_assembly,
        "Assemble drive + frame",
        10,
        120.0,
    )?;
    let op_paint = operation(
        "op_ebike_paint",
        bom_ebike,
        wc_paint,
        "Paint + finish",
        20,
        60.0,
    )?;
    // One operation on the Trike BOM: its work order shares the Assembly agent.
    let op_trike = operation(
        "op_trike_assemble",
        bom_trike,
        wc_assembly,
        "Assemble trike",
        10,
        180.0,
    )?;

    // Origin ties each MO to its sales-order promise (ekg:fulfills matches SO name).
    let so_name = read_name(&odoo, "sale.order", so)?;
    let so_trike_name = read_name(&odoo, "sale.order", so_trike)?;

    // Manufacturing orders, dated relative to today in day-2 hour windows. Both are
    // promised weeks out (their SO commitment) and Odoo schedules them well before that,
    // so they are feasible on the schedule; they exist to contend on the shared Assembly Line.
    let mo_ebike = production(
        "mo_ebike",
        ebike,
        bom_ebike,
        10.0,
        &so_name,
        &dt_plus(2, 8)?,
        &dt_plus(2, 18)?,
    )?;
    let mo_trike = production(
        "mo_trike",
        trike,
        bom_trike,
        10.0,
        &so_trike_name,
        &dt_plus(2, 9)?,
        &dt_plus(2, 11)?,
    )?;

    // Work orders: the Assembly Line (capacity 1) performs both the e-bike and trike
    // assembly in overlapping day-2 windows, so concurrency 2 exceeds capacity 1 and both
    // orders contend on one agent. The paint work order is alone on the Paint Booth, a
    // within-capacity control (no contention).
    workorder(
        "wo_ebike_assemble",
        mo_ebike,
        wc_assembly,
        op_assemble,
        "Assemble E-Bike",
        &dt_plus(2, 8)?,
        &dt_plus(2, 12)?,
        120.0,
    )?;
    workorder(
        "wo_ebike_paint",
        mo_ebike,
        wc_paint,
        op_paint,
        "Paint E-Bike",
        &dt_plus(2, 12)?,
        &dt_plus(2, 14)?,
        60.0,
    )?;
    workorder(
        "wo_trike_assemble",
        mo_trike,
        wc_assembly,
        op_trike,
        "Assemble Trike",
        &dt_plus(2, 9)?,
        &dt_plus(2, 13)?,
        180.0,
    )?;

    // A rush order promised sooner than it can be built: its sales-order commitment is
    // tomorrow, but the MO only starts on day 2, so whatever finish Odoo computes lands after
    // the promise and the schedule is infeasible (missing the commitment). Its assembly work
    // order also shares the capacity-1 Assembly Line in the same overlapping window, so it is
    // contended too. The order is flagged by both rules and coalesces into one action point.
    // Its one line is a service with no bill of materials, so the order carries a real value
    // (5 trikes at 2,400) yet adds no component demand or supply-risk noise. The MO links to
    // it by origin so ekg:fulfills resolves the commitment.
    let so_rush = odoo.ensure(
        "so_rush",
        "sale.order",
        json!({ "partner_id": citylogix, "date_order": today, "commitment_date": ymd_plus(1)? }),
    )?;
    let rush_service = odoo.ensure(
        "prod_rush_assembly",
        "product.template",
        json!({
            "name": "Rush assembly · Cargo Trike",
            "default_code": "SVC-RUSH-TRIKE",
            "type": "service", // no stock, no BOM, so no component demand
            "purchase_ok": false,
            "uom_id": uom,
        }),
    )?;
    let rush_service = odoo.read_m2o("product.template", rush_service, "product_variant_id")?;
    odoo.ensure(
        "so_rush_line",
        "sale.order.line",
        json!({
            "order_id": so_rush,
            "product_id": rush_service,
            "name": "Rush assembly · Cargo Trike",
            "product_uom_qty": 1,
            "product_uom_id": uom,
            "price_unit": 12000.0,
        }),
    )?;
    let so_rush_name = read_name(&odoo, "sale.order", so_rush)?;
    let mo_rush = production(
        "mo_rush",
        trike,
        bom_trike,
        5.0,
        &so_rush_name,
        &dt_plus(2, 10)?,
        &dt_plus(2, 12)?,
    )?;
    workorder(
        "wo_rush_assemble",
        mo_rush,
        wc_assembly,
        op_trike,
        "Assemble Rush Trike",
        &dt_plus(2, 10)?,
        &dt_plus(2, 12)?,
        120.0,
    )?;
    println!(
        "  process: 2 work centers (Assembly cap 1, shared, so contention), 3 operations, 3 MOs, 4 work orders; \
         rush MO promised tomorrow but started day 2, so schedule-infeasible (and contended, so it coalesces into one point); MOs origin-linked to SOs."
    );

    // --- Starter cases ---
    // Four more cases so a fresh stack shows both oversight paths: plans for a human and
    // changes the demo policy applies. Each new order has its own finished product with a
    // one-line BOM, so none reaches the flagship's battery; the risk rule judges each order
    // alone against total on-hand + inbound by its promise. Prices are the purchase-order
    // line price the auto-apply policy judges (cost limit 250 on the local stack).
    println!(
        "seeding the starter cases (single plan, gear expedite, frame reconfirm, split portfolio)..."
    );
    let purchase = |xid: &str,
                    supplier: i64,
                    part: Prod,
                    label: &str,
                    qty: f64,
                    price: f64,
                    eta: &str,
                    confirm: bool|
     -> Result<i64> {
        let po = odoo.ensure(
            xid,
            "purchase.order",
            json!({ "partner_id": supplier, "date_order": today }),
        )?;
        odoo.ensure(
            &format!("{xid}_line"),
            "purchase.order.line",
            json!({
                "order_id": po,
                "product_id": part.variant,
                "name": label,
                "product_qty": qty,
                "product_uom_id": uom,
                "price_unit": price,
                "date_planned": eta,
            }),
        )?;
        if confirm {
            // Confirm while still a draft (as the brake PO above), so the supplier state
            // reads `purchase` and Odoo generates the inbound shipment; then date that
            // shipment to the planned arrival.
            if matches!(
                read_str(&odoo, "purchase.order", po, "state")?.as_str(),
                "draft" | "sent"
            ) {
                odoo.call_method("purchase.order", "button_confirm", json!({ "ids": [po] }))?;
            }
            let picking = *odoo
                .search(
                    "stock.picking",
                    json!([
                        ["purchase_id", "=", po],
                        ["state", "not in", ["done", "cancel"]]
                    ]),
                )?
                .first()
                .with_context(|| format!("no open inbound picking was generated for {xid}"))?;
            odoo.write(
                "stock.picking",
                json!([picking]),
                json!({ "scheduled_date": eta }),
            )?;
        }
        Ok(po)
    };
    let sale = |xid: &str,
                customer: i64,
                prod: Prod,
                label: &str,
                qty: f64,
                price: f64,
                promise: &str|
     -> Result<i64> {
        let so = odoo.ensure(
            xid,
            "sale.order",
            json!({ "partner_id": customer, "date_order": today, "commitment_date": promise }),
        )?;
        odoo.ensure(
            &format!("{xid}_line"),
            "sale.order.line",
            json!({
                "order_id": so,
                "product_id": prod.variant,
                "name": label,
                "product_uom_qty": qty,
                "product_uom_id": uom,
                "price_unit": price,
            }),
        )?;
        Ok(so)
    };

    // Case 2, a single-point plan (human review): 25 kits need 25 motors, 15 on hand, and
    // the only inbound is an unconfirmed MotorWorks RFQ priced above the limit, arriving 14
    // days after the promise, so short 10. Expedite is gated (unconfirmed, 480 > 250),
    // re-promise moves a promised date. No MO fulfils it, so it is planned alone.
    let retrofit = product(
        "prod_retrofit_kit",
        "Retrofit Kit · Hub Motor",
        "KIT-RETRO-MOTOR",
        &[route_make],
        false,
    )?;
    let bom_retrofit = bom("bom_retrofit_kit", retrofit, 1)?;
    bom_line("bomline_retrofit_motor", bom_retrofit, motor, 1.0)?;
    let bergwerk = partner("partner_bergwerk", "Bergwerk Bikes", 0, 1)?;
    sale(
        "so_bergwerk",
        bergwerk,
        retrofit,
        "Retrofit Kit · Hub Motor",
        25.0,
        420.0,
        &ymd_plus(10)?,
    )?;
    purchase(
        "po_motor_rfq",
        motorworks,
        motor,
        "Hub Motor 250W — RFQ",
        20.0,
        480.0,
        &ymd_plus(24)?,
        false,
    )?;

    // Case 3, a gear-set expedite (auto-applied): 30 kits need 60 gears, 40 on hand; a
    // confirmed PartsHub line of 30 @ 150 lands after the promise, so short 20. Expediting it
    // to the promise covers the 60 with 70: confirmed, within the limit, reversible.
    let drivetrain = product(
        "prod_drivetrain_kit",
        "Drivetrain Service Kit",
        "KIT-DRIVETRAIN",
        &[route_make],
        false,
    )?;
    let bom_drivetrain = bom("bom_drivetrain_kit", drivetrain, 1)?;
    bom_line("bomline_drivetrain_gear", bom_drivetrain, gear, 2.0)?;
    sale(
        "so_velomax_service",
        velomax,
        drivetrain,
        "Drivetrain Service Kit",
        30.0,
        90.0,
        &ymd_plus(8)?,
    )?;
    purchase(
        "po_gear_inbound",
        partshub,
        gear,
        "Gear Set — inbound",
        30.0,
        150.0,
        &ymd_plus(14)?,
        true,
    )?;

    // Case 4, a frame reconfirm (auto-applied): 18 packs leave frames 3 below the safety
    // stock of 5 while the demand stays covered (18 of 20 on hand); a confirmed FrameCo line
    // of 10 @ 180 arrives after the promise. Notifying the owner or expediting that line both
    // pass.
    let frame_pack = product(
        "prod_frame_pack",
        "Frame Service Pack",
        "KIT-FRAME-SVC",
        &[route_make],
        false,
    )?;
    let bom_frame_pack = bom("bom_frame_pack", frame_pack, 1)?;
    bom_line("bomline_frame_pack_frame", bom_frame_pack, frame, 1.0)?;
    let urbanride = partner("partner_urbanride", "Urbanride Service", 0, 1)?;
    sale(
        "so_urbanride",
        urbanride,
        frame_pack,
        "Frame Service Pack",
        18.0,
        260.0,
        &ymd_plus(12)?,
    )?;
    purchase(
        "po_frame_inbound",
        frameco,
        frame,
        "Aluminium Frame — inbound",
        10.0,
        180.0,
        &ymd_plus(16)?,
        true,
    )?;

    // Case 5, a split portfolio (human review): 40 City S1 need 40 controllers, 30 on hand;
    // the rest comes from an unconfirmed PartsHub RFQ above the limit, a day after the
    // promise. One MO (origin = the order) fulfils it; a second MO's work orders overlap it on
    // both capacity-1 work centers, so they contend and {SO, MO S1, MO T4} couple into their
    // own portfolio. Every window is on day 16 or later, clear of the flagship's day-2
    // windows, so the two portfolios never merge; MO S1 finishes (+18) before the promise (+20).
    let city_s1 = product(
        "prod_city_s1",
        "City Bike S1",
        "FG-CITY-S1",
        &[route_make],
        false,
    )?;
    let emtb_t4 = product(
        "prod_emtb_t4",
        "E-MTB T4",
        "FG-EMTB-T4",
        &[route_make],
        false,
    )?;
    let bom_city = bom("bom_city_s1", city_s1, 2)?;
    bom_line("bomline_s1_controller", bom_city, controller, 1.0)?;
    let bom_emtb = bom("bom_emtb_t4", emtb_t4, 2)?;
    bom_line("bomline_t4_gear", bom_emtb, gear, 1.0)?;
    let op_city_assemble = operation(
        "op_s1_assemble",
        bom_city,
        wc_assembly,
        "Assemble City S1",
        10,
        120.0,
    )?;
    let op_city_paint = operation("op_s1_paint", bom_city, wc_paint, "Paint City S1", 20, 60.0)?;
    let op_emtb_assemble = operation(
        "op_t4_assemble",
        bom_emtb,
        wc_assembly,
        "Assemble E-MTB T4",
        10,
        120.0,
    )?;
    let op_emtb_paint = operation(
        "op_t4_paint",
        bom_emtb,
        wc_paint,
        "Paint E-MTB T4",
        20,
        60.0,
    )?;
    let stadtrad = partner("partner_stadtrad", "Stadtrad Leasing", 0, 1)?;
    let so_stadtrad = sale(
        "so_stadtrad",
        stadtrad,
        city_s1,
        "City Bike S1",
        40.0,
        1450.0,
        &ymd_plus(20)?,
    )?;
    purchase(
        "po_controller_rfq",
        partshub,
        controller,
        "Motor Controller — RFQ",
        50.0,
        320.0,
        &ymd_plus(21)?,
        false,
    )?;
    let so_stadtrad_name = read_name(&odoo, "sale.order", so_stadtrad)?;
    let mo_city = production(
        "mo_city_s1",
        city_s1,
        bom_city,
        40.0,
        &so_stadtrad_name,
        &dt_plus(16, 8)?,
        &dt_plus(18, 18)?,
    )?;
    // MO T4 serves stock, not an order: no origin, so it fulfils nothing and has no
    // promise to miss.
    let mo_emtb = odoo.ensure(
        "mo_emtb_t4",
        "mrp.production",
        json!({
            "product_id": emtb_t4.variant,
            "bom_id": bom_emtb,
            "product_qty": 10.0,
            "origin": false,
            "date_start": dt_plus(17, 10)?,
            "date_finished": dt_plus(18, 12)?,
        }),
    )?;
    workorder(
        "wo_s1_assemble",
        mo_city,
        wc_assembly,
        op_city_assemble,
        "Assemble City S1",
        &dt_plus(17, 8)?,
        &dt_plus(17, 16)?,
        120.0,
    )?;
    workorder(
        "wo_s1_paint",
        mo_city,
        wc_paint,
        op_city_paint,
        "Paint City S1",
        &dt_plus(18, 8)?,
        &dt_plus(18, 12)?,
        60.0,
    )?;
    workorder(
        "wo_t4_assemble",
        mo_emtb,
        wc_assembly,
        op_emtb_assemble,
        "Assemble E-MTB T4",
        &dt_plus(17, 10)?,
        &dt_plus(17, 14)?,
        120.0,
    )?;
    workorder(
        "wo_t4_paint",
        mo_emtb,
        wc_paint,
        op_emtb_paint,
        "Paint E-MTB T4",
        &dt_plus(18, 9)?,
        &dt_plus(18, 11)?,
        60.0,
    )?;
    println!(
        "  starter cases: Bergwerk 25x Retrofit Kit (motor RFQ, human review); VeloMax 30x Drivetrain Kit \
         (confirmed gear PO, expedite); Urbanride 18x Frame Pack (safety breach, confirmed frame PO); \
         Stadtrad 40x City S1 (controller RFQ + S1/T4 contention on day 17-18, so a split portfolio)."
    );

    print_summary(&commitment, &battery_eta);
    Ok(())
}

/// The seeded `product.supplierinfo` records (by the seed's external ids) that carry no
/// `product_id`. Each one would leave its `ekg:supplies_product` unmaterialized.
fn seeded_sourcing_links_without_variant(odoo: &Odoo) -> Result<Vec<i64>> {
    let seeded = odoo.search_read(
        "ir.model.data",
        json!([
            ["module", "=", odoo_client::SEED_MODULE],
            ["model", "=", "product.supplierinfo"]
        ]),
        json!(["res_id"]),
    )?;
    let ids: Vec<i64> = seeded
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["res_id"].as_i64())
        .collect();
    let links = odoo.read("product.supplierinfo", json!(ids), json!(["product_id"]))?;
    Ok(links
        .as_array()
        .into_iter()
        .flatten()
        // an unset many2one reads back as `false`
        .filter(|l| !l["product_id"].is_array())
        .filter_map(|l| l["id"].as_i64())
        .collect())
}

/// Read one string field of a record (e.g. a picking's `state`).
fn read_str(odoo: &Odoo, model: &str, id: i64, field: &str) -> Result<String> {
    odoo.read(model, json!([id]), json!([field]))?
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r[field].as_str())
        .map(str::to_owned)
        .with_context(|| format!("{model}/{id} has no readable `{field}`"))
}

/// Book a product's on-hand at `location` back to `designed` through an inventory
/// adjustment (`inventory_quantity`, then `action_apply_inventory`, a real stock move), and
/// fail loudly unless it lands. Idempotent: a no-op when the on-hand already matches.
fn restore_on_hand(odoo: &Odoo, product: i64, location: i64, designed: f64) -> Result<()> {
    let on_hand = |odoo: &Odoo| -> Result<(Vec<i64>, f64)> {
        let quants = odoo.search_read(
            "stock.quant",
            json!([["product_id", "=", product], ["location_id", "=", location]]),
            json!(["quantity"]),
        )?;
        let rows = quants.as_array().cloned().unwrap_or_default();
        let ids = rows.iter().filter_map(|q| q["id"].as_i64()).collect();
        let total = rows.iter().filter_map(|q| q["quantity"].as_f64()).sum();
        Ok((ids, total))
    };
    let (ids, total) = on_hand(odoo)?;
    if (total - designed).abs() < f64::EPSILON {
        return Ok(());
    }
    // The whole designed quantity on the first quant, the rest counted to zero.
    let (first, rest) = ids
        .split_first()
        .with_context(|| format!("no stock.quant for product {product} to adjust"))?;
    odoo.write(
        "stock.quant",
        json!([first]),
        json!({ "inventory_quantity": designed }),
    )?;
    if !rest.is_empty() {
        odoo.write(
            "stock.quant",
            json!(rest),
            json!({ "inventory_quantity": 0.0 }),
        )?;
    }
    let outcome = odoo.call_method(
        "stock.quant",
        "action_apply_inventory",
        json!({ "ids": ids }),
    )?;
    let (_, after) = on_hand(odoo)?;
    ensure!(
        (after - designed).abs() < f64::EPSILON,
        "restoring product {product}'s on-hand to {designed} left {after}; action_apply_inventory returned {outcome}"
    );
    Ok(())
}

/// Read a record's `name` (an order reference like `S00001`) for origin/name matching.
///
/// Fails loud if the name is missing rather than substituting an empty string: an
/// empty `origin` would silently make the `ekg:fulfills` derivation unresolvable.
fn read_name(odoo: &Odoo, model: &str, id: i64) -> Result<String> {
    odoo.read(model, json!([id]), json!(["name"]))?
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["name"].as_str())
        .map(str::to_owned)
        .with_context(|| {
            format!("{model}/{id} has no readable `name` (needed to map MO origin to fulfills)")
        })
}

/// Read an env var, falling back to a local-substrate default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// `today + days` as an Odoo datetime string (`YYYY-MM-DD 12:00:00`, UTC).
fn ymd_plus(days: i64) -> Result<String> {
    dt_plus(days, 12)
}

/// `today + days` at `hour`:00 as an Odoo datetime string (`YYYY-MM-DD HH:00:00`,
/// UTC). Hour granularity lets the manufacturing scenario put overlapping work-order
/// windows and a tight MO window within a single day.
fn dt_plus(days: i64, hour: u32) -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before Unix epoch")?
        .as_secs();
    let epoch_day = i64::try_from(secs / 86_400).context("date overflow")? + days;
    let (y, m, d) = civil_from_days(epoch_day);
    Ok(format!("{y:04}-{m:02}-{d:02} {hour:02}:00:00"))
}

/// The day before an Odoo datetime's date (`YYYY-MM-DD …`), at 12:00:00.
fn day_before(datetime: &str) -> Result<String> {
    let date = datetime.get(..10).context("not an Odoo datetime")?;
    let mut parts = date.split('-').map(str::parse::<i64>);
    let (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) = (parts.next(), parts.next(), parts.next()) else {
        anyhow::bail!("not an Odoo date: {datetime}");
    };
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) - 1);
    Ok(format!("{y:04}-{m:02}-{d:02} 12:00:00"))
}

/// Days since 1970-01-01 of a proleptic-Gregorian civil date (Hinnant's algorithm; the
/// inverse of [`civil_from_days`]).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since epoch to (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

fn print_summary(commitment: &str, battery_eta: &str) {
    println!(
        "\n✓ Seed complete: at-risk supply-risk scenario loaded (idempotent; re-run to update).\n\n\
         Graph shape (multi-level BOM + edges):\n\
         \x20 SalesOrder(VeloMax, 10× E-Bike, due {commitment})\n\
         \x20   └─▶ E-Bike Model X ──BOM──┬─▶ Drive Subassembly ──BOM──┬─▶ Hub Motor 250W  (MotorWorks · Driveline — multi-source)\n\
         \x20                             │                            ├─▶ Motor Controller (PartsHub)\n\
         \x20                             │                            └─▶ Gear Set ×2      (PartsHub)\n\
         \x20                             ├─▶ Aluminium Frame  (FrameCo, 14d)\n\
         \x20                             └─▶ Battery Pack 500Wh  (CellTech, 45d — SINGLE SOURCE)\n\n\
         Why it's at risk: battery on-hand 2 < safety 5 and < 10 demanded; the only\n\
         inbound PO of 10 batteries lands {battery_eta}, after the {commitment} promise.\n\n\
         Inspect: in Odoo under Sales / Purchase / Manufacturing, or filter ir.model.data by module 'ekg_seed'."
    );
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, day_before, days_from_civil};

    #[test]
    fn day_before_crosses_month_year_and_leap_boundaries() {
        assert_eq!(
            day_before("2027-09-17 12:00:00").ok().as_deref(),
            Some("2027-09-16 12:00:00")
        );
        assert_eq!(
            day_before("2027-03-07 08:30:00").ok().as_deref(),
            Some("2027-03-06 12:00:00")
        );
        assert_eq!(
            day_before("2024-03-01 00:00:00").ok().as_deref(),
            Some("2024-02-29 12:00:00")
        );
        assert_eq!(
            day_before("2000-01-01 12:00:00").ok().as_deref(),
            Some("1999-12-31 12:00:00")
        );
        assert!(day_before("false").is_err());
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for z in [-1, 0, 1, 19_000, 20_723, 20_724, 60_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }
}
