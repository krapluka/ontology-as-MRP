//! Gated live write-back e2e: the `OdooActionExecutor` commits the governed verbs
//! to Odoo and reverses them, and refuses a reverse when the source advanced after
//! the write.
//!
//! Ignored by default. Needs a reachable Odoo (the local substrate supplies it):
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a   # ODOO_API_KEY minted by podman-local-up
//! cargo test -p ekg-kinetic --test it writeback_it -- --ignored --nocapture
//! ```
//!
//! Each test skips if Odoo is unreachable:
//! - the supply-risk `re_promise_date` roundtrip (throwaway `sale.order` + partner);
//! - the Odoo 19 behaviour the reschedule is built on: a work order's start and
//!   finish written together in one write are accepted and keep the duration, while
//!   a start-only write past the finish is refused;
//! - the process-remediation roundtrip:
//!   `reschedule_work_order` (the planned window, `date_start` + `date_finished` in
//!   one write, reversed in one step) and `reassign_work_center` (a many-to-one
//!   integer write on `workcenter_id`, prior id captured for the inverse), over a
//!   throwaway product + MO + work order + two work centers;
//! - the Odoo 19 behaviour the expedite is built on: a PO line date write does not
//!   move its open receipt's `scheduled_date`;
//! - the `expedite_po` roundtrip on a PO line with an open receipt: both dates
//!   move, one reverse restores both, a conflicting reverse is refused;
//! - the value-aware reverse guard on a re-promise and on an expedite with its open
//!   receipt: an edit to another field does not refuse the reverse, a changed written
//!   field does, and putting the value back lets it through.
//!
//! Each builds and cleans up its own records, so none depends on the seeded
//! scenario. The value/prior mapping is additionally covered by unit tests in
//! `src/verbs.rs` and `src/executor.rs`.

// In this integration test `expect`/`panic` are the assertion mechanism, and the
// prose names products (Odoo) without backticks.
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::sync::Arc;

use ekg_domain::{
    ActionExecutor, ActionPointId, ActionType, CorrelationId, EngineError, EntityUri,
    PromotedAction, Promotion, ProposedAction, ReviewerId, Revision,
};
use ekg_kinetic::OdooActionExecutor;
use ekg_kinetic::verbs::odoo_datetime_to_epoch;
use odoo_client::Odoo;
use serde_json::{Value, json};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Run a blocking odoo-client call on a runtime thread (asserting success).
async fn odoo_do<T, F>(odoo: &Arc<Odoo>, f: F) -> T
where
    F: FnOnce(&Odoo) -> T + Send + 'static,
    T: Send + 'static,
{
    let odoo = Arc::clone(odoo);
    tokio::task::spawn_blocking(move || f(&odoo))
        .await
        .expect("odoo task join")
}

/// Read a `sale.order`'s current `commitment_date` (or `Value::Bool(false)`).
async fn commitment_date(odoo: &Arc<Odoo>, id: i64) -> Value {
    odoo_do(odoo, move |o| {
        let rows = o
            .read("sale.order", json!([id]), json!(["commitment_date"]))
            .expect("read commitment_date");
        rows.as_array()
            .and_then(|a| a.first())
            .and_then(|r| r.get("commitment_date"))
            .cloned()
            .unwrap_or(Value::Bool(false))
    })
    .await
}

fn promoted(target: &str, new_date: &str) -> PromotedAction {
    promoted_verb("re_promise_date", target, "new_date", new_date)
}

/// A promoted action for `action_type` on `target` carrying a single param, the
/// shape every field-write verb takes.
fn promoted_verb(action_type: &str, target: &str, param: &str, value: &str) -> PromotedAction {
    let mut params = BTreeMap::new();
    params.insert(param.to_owned(), value.to_owned());
    PromotedAction {
        action: ProposedAction {
            action_type: ActionType(action_type.to_owned()),
            target: EntityUri(target.to_owned()),
            params,
        },
        promotion: Promotion {
            action_point: ActionPointId("ap-writeback-it".to_owned()),
            correlation_id: CorrelationId("corr-writeback-it".to_owned()),
            reviewer: ReviewerId("reviewer-it".to_owned()),
            rationale: "e2e: promote to exercise the governed write-back".to_owned(),
            context_seen: "e2e".to_owned(),
        },
        // The world-model revision the decision was based on; the source-of-record
        // conflict guard uses Odoo's write_date, not this.
        basis_revision: Revision(0),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn executes_and_reverses_a_governed_re_promise_and_refuses_a_conflicting_reverse() {
    const INITIAL: &str = "2027-01-07 00:00:00";
    const REPROMISED: &str = "2027-12-31 00:00:00";

    let url = env_or("ODOO_URL", "http://localhost:8069");
    let db = env_or("ODOO_DB", "ekg");
    let user = env_or("ODOO_USER", "admin");
    let key = env_or("ODOO_API_KEY", "admin");

    // --- connect; skip if Odoo is unreachable ---
    let (c_url, c_db, c_user, c_key) = (url, db, user, key);
    let odoo =
        match tokio::task::spawn_blocking(move || Odoo::connect(&c_url, &c_db, &c_user, &c_key))
            .await
            .expect("connect join")
        {
            Ok(odoo) => Arc::new(odoo),
            Err(why) => {
                eprintln!("SKIP: Odoo unreachable ({why})");
                return;
            }
        };

    // --- throwaway partner + draft sale order with an initial promise date ---
    let partner_id = odoo_do(&odoo, |o| {
        o.create(
            "res.partner",
            json!({ "name": "EKG kinetic write-back IT" }),
        )
        .expect("create partner")
    })
    .await;
    let order_id = odoo_do(&odoo, move |o| {
        o.create("sale.order", json!({ "partner_id": partner_id }))
            .expect("create sale.order")
    })
    .await;
    odoo_do(&odoo, move |o| {
        o.write(
            "sale.order",
            json!([order_id]),
            json!({ "commitment_date": INITIAL }),
        )
        .expect("set initial commitment_date");
    })
    .await;

    let target = format!("https://ekg.example/id/sale.order/{order_id}");
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));

    // --- execute: the governed write-back moves the promise date ---
    let receipt = executor
        .execute(&promoted(&target, REPROMISED))
        .await
        .expect("execute re_promise_date");
    assert!(receipt.reversible, "a field write is reversible");
    let after_execute = commitment_date(&odoo, order_id).await;
    assert_eq!(
        after_execute,
        json!(REPROMISED),
        "the write-back set the new promise date"
    );

    // --- reverse: the inverse restores the prior date ---
    executor.reverse(&receipt).await.expect("reverse restores");
    let after_reverse = commitment_date(&odoo, order_id).await;
    assert_eq!(
        after_reverse,
        json!(INITIAL),
        "the inverse restored the prior promise date"
    );

    // --- conflict: a source change after the write makes reverse refuse ---
    let receipt2 = executor
        .execute(&promoted(&target, REPROMISED))
        .await
        .expect("execute again");
    // Odoo write_date has 1s resolution; wait past the boundary, then mutate the
    // record so its write_date is strictly newer than the executed watermark.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    odoo_do(&odoo, move |o| {
        o.write(
            "sale.order",
            json!([order_id]),
            json!({ "commitment_date": "2027-06-15 00:00:00" }),
        )
        .expect("intervening source change");
    })
    .await;
    let conflict = executor.reverse(&receipt2).await;

    // --- cleanup before the final assertions so a failure never leaks records ---
    odoo_do(&odoo, move |o| {
        let _ = o.unlink("sale.order", json!([order_id]));
        let _ = o.unlink("res.partner", json!([partner_id]));
    })
    .await;

    assert!(
        matches!(conflict, Err(EngineError::ReverseConflict { .. })),
        "reverse must refuse after an intervening source change, got {conflict:?}"
    );
}

/// A work order's current planned window `(date_start, date_finished)` (each
/// `Value::Bool(false)` when unset).
async fn wo_window(odoo: &Arc<Odoo>, id: i64) -> (Value, Value) {
    odoo_do(odoo, move |o| {
        let rows = o
            .read(
                "mrp.workorder",
                json!([id]),
                json!(["date_start", "date_finished"]),
            )
            .expect("read the planned window");
        let row = rows.as_array().and_then(|a| a.first());
        let field = |f: &str| {
            row.and_then(|r| r.get(f))
                .cloned()
                .unwrap_or(Value::Bool(false))
        };
        (field("date_start"), field("date_finished"))
    })
    .await
}

/// The seconds between a window's start and finish, both Odoo datetimes.
fn window_secs((start, finish): &(Value, Value)) -> i64 {
    let at = |v: &Value| {
        let s = v.as_str().expect("a planned datetime");
        i64::try_from(odoo_datetime_to_epoch(s)).expect("epoch fits i64")
    };
    at(finish) - at(start)
}

/// A throwaway product + manufacturing order + work order (on center A) + two work
/// centers, mirroring `apps/odoo-seed`, so no process test depends on the seeded
/// scenario.
struct WorkOrderFixture {
    template: i64,
    wc_a: i64,
    wc_b: i64,
    mo: i64,
    wo: i64,
}

/// Build a [`WorkOrderFixture`] whose MO and work order are planned from `start` to
/// `finish`.
async fn work_order_fixture(
    odoo: &Arc<Odoo>,
    start: &'static str,
    finish: &'static str,
) -> WorkOrderFixture {
    odoo_do(odoo, move |o| {
        let uom = o.xmlid("uom", "product_uom_unit").expect("uom");
        let template = o
            .create(
                "product.template",
                json!({ "name": "EKG remediation IT", "type": "consu", "uom_id": uom }),
            )
            .expect("create product.template");
        let variant = o
            .read_m2o("product.template", template, "product_variant_id")
            .expect("product variant");
        let wc_a = o
            .create(
                "mrp.workcenter",
                json!({ "name": "EKG WC A (IT)", "code": "EKGA" }),
            )
            .expect("create work center A");
        let wc_b = o
            .create(
                "mrp.workcenter",
                json!({ "name": "EKG WC B (IT)", "code": "EKGB" }),
            )
            .expect("create work center B");
        let mo = o
            .create(
                "mrp.production",
                json!({
                    "product_id": variant,
                    "product_qty": 1,
                    "product_uom_id": uom,
                    "date_start": start,
                    "date_finished": finish,
                }),
            )
            .expect("create mrp.production");
        let wo = o
            .create(
                "mrp.workorder",
                json!({
                    "production_id": mo,
                    "workcenter_id": wc_a,
                    "name": "EKG WO (IT)",
                    "date_start": start,
                    "date_finished": finish,
                    "duration_expected": 240.0,
                }),
            )
            .expect("create mrp.workorder");
        WorkOrderFixture {
            template,
            wc_a,
            wc_b,
            mo,
            wo,
        }
    })
    .await
}

/// Remove a [`WorkOrderFixture`] (best effort, so cleanup never masks the assertion
/// that follows it).
async fn drop_work_order_fixture(odoo: &Arc<Odoo>, f: &WorkOrderFixture) {
    let (template, wc_a, wc_b, mo, wo) = (f.template, f.wc_a, f.wc_b, f.mo, f.wo);
    odoo_do(odoo, move |o| {
        let _ = o.unlink("mrp.workorder", json!([wo]));
        let _ = o.unlink("mrp.production", json!([mo]));
        let _ = o.unlink("mrp.workcenter", json!([wc_a, wc_b]));
        let _ = o.unlink("product.template", json!([template]));
    })
    .await;
}

/// A work order planned 08:00 to 12:00 (a 4 h window), and the same window two days later.
const WO_START: &str = "2027-02-16 08:00:00";
const WO_FINISH: &str = "2027-02-16 12:00:00";
const WO_SHIFTED_START: &str = "2027-02-18 08:00:00";
const WO_SHIFTED_FINISH: &str = "2027-02-18 12:00:00";

/// Pins the Odoo 19 behaviour the reschedule is built on: a work order's
/// `date_start` and `date_finished` written together in one `write` are accepted and
/// keep the planned duration, while a start-only write past the finish is refused
/// (a `UserError`), so the governed reschedule must move both ends in one write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn odoo_accepts_a_one_write_window_move_and_refuses_a_start_past_the_finish() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = work_order_fixture(&odoo, WO_START, WO_FINISH).await;
    let wo = f.wo;

    let before = wo_window(&odoo, wo).await;
    let combined = odoo_do(&odoo, move |o| {
        o.write(
            "mrp.workorder",
            json!([wo]),
            json!({ "date_start": WO_SHIFTED_START, "date_finished": WO_SHIFTED_FINISH }),
        )
        .map_err(|e| e.to_string())
    })
    .await;
    let moved = wo_window(&odoo, wo).await;
    // Past the (moved) finish: 14 Feb 08:00 is after 12 Feb 12:00.
    let start_only = odoo_do(&odoo, move |o| {
        o.write(
            "mrp.workorder",
            json!([wo]),
            json!({ "date_start": "2027-02-20 08:00:00" }),
        )
        .map_err(|e| e.to_string())
    })
    .await;
    let after_refusal = wo_window(&odoo, wo).await;

    drop_work_order_fixture(&odoo, &f).await;

    assert_eq!(
        before,
        (json!(WO_START), json!(WO_FINISH)),
        "the fixture is planned 08:00–12:00"
    );
    combined.expect("Odoo accepts start and finish written together");
    assert_eq!(
        moved,
        (json!(WO_SHIFTED_START), json!(WO_SHIFTED_FINISH)),
        "both ends moved two days"
    );
    assert_eq!(window_secs(&moved), 4 * 3_600, "the 4 h duration is kept");
    let refused = start_only.expect_err("a start-only write past the finish is refused");
    assert!(
        refused.contains("UserError")
            && refused.contains("cannot be prior to the planned start date"),
        "Odoo refuses with the start <= finish UserError, got {refused}"
    );
    assert_eq!(after_refusal, moved, "a refused write changes nothing");
}

/// A work order's current `workcenter_id` reduced to its integer id (`None` when
/// unset). Odoo renders a many-to-one read as `[id, display_name]`.
async fn wo_workcenter_id(odoo: &Arc<Odoo>, id: i64) -> Option<i64> {
    odoo_do(odoo, move |o| {
        let rows = o
            .read("mrp.workorder", json!([id]), json!(["workcenter_id"]))
            .expect("read workcenter_id");
        rows.as_array()
            .and_then(|a| a.first())
            .and_then(|r| r.get("workcenter_id"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(Value::as_i64)
    })
    .await
}

/// Gated live roundtrip for the process-remediation verbs: the `OdooActionExecutor`
/// commits a governed `reschedule_work_order` (the planned window, `date_start` and
/// `date_finished` moved together in one write, keeping the duration) and a
/// `reassign_work_center` (a many-to-one integer write on `workcenter_id`) to Odoo,
/// reverses each in one step, and refuses a reverse when the source advanced after
/// the write.
///
/// It builds its own [`WorkOrderFixture`] and cleans it up. If Odoo is unreachable
/// it skips (returns).
// One linear scenario (build, reschedule, reassign, conflicts, cleanup) reads best
// as a single test.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn executes_and_reverses_process_remediation_verbs_and_refuses_a_conflicting_reverse() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = work_order_fixture(&odoo, WO_START, WO_FINISH).await;
    let (wo, wc_a, wc_b) = (f.wo, f.wc_a, f.wc_b);
    let target = format!("https://ekg.example/id/mrp.workorder/{wo}");
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));
    let reschedule = || {
        promoted_verb(
            "reschedule_work_order",
            &target,
            "new_date",
            WO_SHIFTED_START,
        )
    };
    let reassign = || {
        promoted_verb(
            "reassign_work_center",
            &target,
            "new_work_center",
            &wc_b.to_string(),
        )
    };
    // An external edit of the work order after the write (past write_date's 1 s
    // resolution). The value-aware guard refuses a reverse only when a field the write
    // set changed: a benign bump does not refuse, a change to the window or the work
    // center does.
    let external_edit = |vals: Value| async {
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        odoo_do(&odoo, move |o| {
            o.write("mrp.workorder", json!([wo]), vals)
                .expect("intervening source change");
        })
        .await;
    };
    let later_finish = ekg_kinetic::verbs::epoch_to_odoo_datetime(
        odoo_datetime_to_epoch(WO_SHIFTED_FINISH) + 3_600,
    );

    // --- reschedule: the whole window moves; one reverse restores both ends ---
    let rescheduled = executor.execute(&reschedule()).await;
    let moved = wo_window(&odoo, wo).await;
    external_edit(json!({ "name": "EKG IT renamed step" })).await;
    let reschedule_reversed = match &rescheduled {
        Ok(r) => executor.reverse(r).await,
        Err(_) => Ok(()),
    };
    let window_restored = wo_window(&odoo, wo).await;

    // --- a reschedule reverse after an external edit is refused, restoring nothing ---
    let rescheduled2 = executor.execute(&reschedule()).await;
    external_edit(json!({ "date_finished": later_finish.clone() })).await;
    let reschedule_conflict = match &rescheduled2 {
        Ok(r) => Some(executor.reverse(r).await),
        Err(_) => None,
    };
    let window_after_conflict = wo_window(&odoo, wo).await;

    // --- reassign: move workcenter_id to center B; the inverse restores A ---
    let reassigned = executor.execute(&reassign()).await;
    let center_moved = wo_workcenter_id(&odoo, wo).await;
    let reassign_reversed = match &reassigned {
        Ok(r) => executor.reverse(r).await,
        Err(_) => Ok(()),
    };
    let center_restored = wo_workcenter_id(&odoo, wo).await;

    // --- a reassign reverse after an external edit is refused ---
    let reassigned2 = executor.execute(&reassign()).await;
    external_edit(json!({ "workcenter_id": wc_a })).await;
    let reassign_conflict = match &reassigned2 {
        Ok(r) => Some(executor.reverse(r).await),
        Err(_) => None,
    };

    // --- cleanup before the assertions so a failure never leaks records ---
    drop_work_order_fixture(&odoo, &f).await;

    let receipt = rescheduled.expect("execute reschedule_work_order");
    assert!(receipt.reversible, "a field write is reversible");
    assert_eq!(
        moved,
        (json!(WO_SHIFTED_START), json!(WO_SHIFTED_FINISH)),
        "the write-back moved the planned start and finish"
    );
    assert_eq!(
        window_secs(&moved),
        4 * 3_600,
        "the move keeps the 4 h duration"
    );
    reschedule_reversed.expect("a benign edit does not refuse; the reverse restores the window");
    assert_eq!(
        window_restored,
        (json!(WO_START), json!(WO_FINISH)),
        "one inverse step restored the prior window"
    );
    rescheduled2.expect("execute reschedule again");
    assert!(
        matches!(
            reschedule_conflict,
            Some(Err(EngineError::ReverseConflict { .. }))
        ),
        "a reschedule reverse must refuse after an intervening change, got {reschedule_conflict:?}"
    );
    assert_eq!(
        window_after_conflict,
        (json!(WO_SHIFTED_START), json!(later_finish)),
        "a refused reverse restores nothing"
    );

    reassigned.expect("execute reassign_work_center");
    assert_eq!(
        center_moved,
        Some(wc_b),
        "the write-back reassigned the work center"
    );
    reassign_reversed.expect("reverse restores the work center");
    assert_eq!(
        center_restored,
        Some(wc_a),
        "the inverse restored the prior work center id"
    );
    reassigned2.expect("execute reassign again");
    assert!(
        matches!(
            reassign_conflict,
            Some(Err(EngineError::ReverseConflict { .. }))
        ),
        "reverse must refuse after an intervening source change, got {reassign_conflict:?}"
    );
}

// --- expedite on a purchase-order line ---

/// A throwaway supplier + storable product + confirmed PO with one line, whose
/// confirmation made Odoo generate the open incoming receipt.
struct InboundFixture {
    partner: i64,
    template: i64,
    po: i64,
    line: i64,
    receipt: i64,
}

/// Build an [`InboundFixture`] whose line (and so its receipt) is planned at `planned`.
async fn inbound_fixture(odoo: &Arc<Odoo>, planned: &'static str) -> InboundFixture {
    odoo_do(odoo, move |o| {
        let uom = o.xmlid("uom", "product_uom_unit").expect("uom");
        let partner = o
            .create("res.partner", json!({ "name": "EKG expedite IT supplier" }))
            .expect("create supplier");
        let template = o
            .create(
                "product.template",
                json!({
                    "name": "EKG expedite IT component",
                    "type": "consu",
                    "is_storable": true,
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
                    "name": "EKG expedite IT line",
                    "product_qty": 10,
                    "product_uom_id": uom,
                    "price_unit": 5.0,
                    "date_planned": planned,
                }),
            )
            .expect("create purchase.order.line");
        o.call_method("purchase.order", "button_confirm", json!({ "ids": [po] }))
            .expect("confirm the PO");
        let receipt = *o
            .search(
                "stock.picking",
                json!([
                    ["purchase_id", "=", po],
                    ["picking_type_code", "=", "incoming"]
                ]),
            )
            .expect("search receipt")
            .first()
            .expect("confirming the PO generated an incoming receipt");
        InboundFixture {
            partner,
            template,
            po,
            line,
            receipt,
        }
    })
    .await
}

/// Cancel and remove an [`InboundFixture`] (best effort, so cleanup never masks
/// the assertion that follows it). The cancelled receipts go before the PO: they
/// outlive it otherwise, and their moves pin the product.
async fn drop_inbound_fixture(odoo: &Arc<Odoo>, f: &InboundFixture) {
    let (po, template, partner) = (f.po, f.template, f.partner);
    odoo_do(odoo, move |o| {
        let _ = o.call_method("purchase.order", "button_cancel", json!({ "ids": [po] }));
        if let Ok(receipts) = o.search("stock.picking", json!([["purchase_id", "=", po]])) {
            let _ = o.unlink("stock.picking", json!(receipts));
        }
        let _ = o.unlink("purchase.order", json!([po]));
        let _ = o.unlink("product.template", json!([template]));
        let _ = o.unlink("res.partner", json!([partner]));
    })
    .await;
}

/// One field of one record (or `Value::Bool(false)`).
async fn field_of(odoo: &Arc<Odoo>, model: &'static str, id: i64, field: &'static str) -> Value {
    odoo_do(odoo, move |o| {
        let rows = o
            .read(model, json!([id]), json!([field]))
            .expect("read field");
        rows.as_array()
            .and_then(|a| a.first())
            .and_then(|r| r.get(field))
            .cloned()
            .unwrap_or(Value::Bool(false))
    })
    .await
}

/// Connect to the live Odoo, or `None` (after a SKIP line) when it is unreachable.
async fn live_odoo() -> Option<Arc<Odoo>> {
    let (url, db, user, key) = (
        env_or("ODOO_URL", "http://localhost:8069"),
        env_or("ODOO_DB", "ekg"),
        env_or("ODOO_USER", "admin"),
        env_or("ODOO_API_KEY", "admin"),
    );
    match tokio::task::spawn_blocking(move || Odoo::connect(&url, &db, &user, &key))
        .await
        .expect("connect join")
    {
        Ok(odoo) => Some(Arc::new(odoo)),
        Err(why) => {
            eprintln!("SKIP: Odoo unreachable ({why})");
            None
        }
    }
}

const LINE_PLANNED: &str = "2027-11-15 08:00:00";
const LINE_EXPEDITED: &str = "2027-10-12 08:00:00";

/// Pins the Odoo 19 behaviour the expedite is built on: writing a PO line's
/// `date_planned` moves only its stock move's deadline, not the open receipt's
/// `scheduled_date` (the arrival the risk rule prefers). So the governed expedite
/// has to move the receipt itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn odoo_moves_only_the_move_deadline_not_the_receipt_when_a_line_date_is_written() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = inbound_fixture(&odoo, LINE_PLANNED).await;
    let (line, receipt) = (f.line, f.receipt);

    let before = field_of(&odoo, "stock.picking", receipt, "scheduled_date").await;
    odoo_do(&odoo, move |o| {
        o.write(
            "purchase.order.line",
            json!([line]),
            json!({ "date_planned": LINE_EXPEDITED }),
        )
        .expect("write the line date");
    })
    .await;
    let after = field_of(&odoo, "stock.picking", receipt, "scheduled_date").await;
    let deadline = field_of(&odoo, "stock.picking", receipt, "date_deadline").await;

    drop_inbound_fixture(&odoo, &f).await;

    assert_eq!(
        before,
        json!(LINE_PLANNED),
        "the receipt starts at the line's date"
    );
    assert_eq!(
        after,
        json!(LINE_PLANNED),
        "Odoo 19 does not move the receipt's scheduled_date with the line"
    );
    assert_eq!(
        deadline,
        json!(LINE_EXPEDITED),
        "it only moves the deadline"
    );
}

/// The governed expedite end to end against Odoo: it moves the line and its open
/// receipt, one reverse restores both, and a reverse after an external receipt edit
/// is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn executes_and_reverses_a_governed_expedite_on_a_po_line_with_an_open_receipt() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = inbound_fixture(&odoo, LINE_PLANNED).await;
    let (line, receipt) = (f.line, f.receipt);
    let target = format!("https://ekg.example/id/purchase.order.line/{line}");
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));
    let expedite = || promoted_verb("expedite_po", &target, "new_date", LINE_EXPEDITED);

    // --- execute: both the line and its open receipt carry the new arrival ---
    let executed = executor.execute(&expedite()).await;
    let line_after = field_of(&odoo, "purchase.order.line", line, "date_planned").await;
    let receipt_after = field_of(&odoo, "stock.picking", receipt, "scheduled_date").await;

    // --- reverse: one inverse step restores both ---
    let reversed = match &executed {
        Ok(r) => executor.reverse(r).await,
        Err(_) => Ok(()),
    };
    let line_restored = field_of(&odoo, "purchase.order.line", line, "date_planned").await;
    let receipt_restored = field_of(&odoo, "stock.picking", receipt, "scheduled_date").await;

    // --- conflict: an external receipt edit after the write makes reverse refuse ---
    let executed2 = executor.execute(&expedite()).await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    odoo_do(&odoo, move |o| {
        o.write(
            "stock.picking",
            json!([receipt]),
            json!({ "scheduled_date": "2027-10-26 08:00:00" }),
        )
        .expect("intervening receipt change");
    })
    .await;
    let conflict = match &executed2 {
        Ok(r) => Some(executor.reverse(r).await),
        Err(_) => None,
    };
    let line_after_conflict = field_of(&odoo, "purchase.order.line", line, "date_planned").await;

    // --- cleanup before the assertions so a failure never leaks records ---
    drop_inbound_fixture(&odoo, &f).await;

    let receipt1 = executed.expect("execute expedite_po");
    assert!(receipt1.reversible);
    assert_eq!(
        line_after,
        json!(LINE_EXPEDITED),
        "the line's planned date moved"
    );
    assert_eq!(
        receipt_after,
        json!(LINE_EXPEDITED),
        "the open receipt's arrival moved"
    );
    reversed.expect("reverse restores");
    assert_eq!(line_restored, json!(LINE_PLANNED), "line restored");
    assert_eq!(receipt_restored, json!(LINE_PLANNED), "receipt restored");
    executed2.expect("execute again");
    assert!(
        matches!(conflict, Some(Err(EngineError::ReverseConflict { .. }))),
        "reverse must refuse after an intervening receipt change, got {conflict:?}"
    );
    assert_eq!(
        line_after_conflict,
        json!(LINE_EXPEDITED),
        "a refused reverse restores nothing"
    );
}

// --- the value-aware reverse guard ---

/// Wait past Odoo's 1 s `write_date` resolution, then write `vals` to one record: an
/// external edit made after the governed write.
async fn edit_later(odoo: &Arc<Odoo>, model: &'static str, id: i64, vals: Value) {
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    odoo_do(odoo, move |o| {
        o.write(model, json!([id]), vals).expect("external edit");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn a_re_promise_reverse_is_judged_on_the_promise_alone() {
    const INITIAL: &str = "2027-01-07 00:00:00";
    const REPROMISED: &str = "2027-12-31 00:00:00";
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let (partner, order) = odoo_do(&odoo, |o| {
        let partner = o
            .create("res.partner", json!({ "name": "EKG value-guard IT" }))
            .expect("create partner");
        let order = o
            .create("sale.order", json!({ "partner_id": partner }))
            .expect("create sale.order");
        o.write(
            "sale.order",
            json!([order]),
            json!({ "commitment_date": INITIAL }),
        )
        .expect("set the promise");
        (partner, order)
    })
    .await;
    let target = format!("https://ekg.example/id/sale.order/{order}");
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));

    // 1. An edit to another field of the order does not refuse the reverse.
    let first = executor.execute(&promoted(&target, REPROMISED)).await;
    if first.is_ok() {
        edit_later(
            &odoo,
            "sale.order",
            order,
            json!({ "client_order_ref": "PO-4711" }),
        )
        .await;
    }
    let unrelated = match &first {
        Ok(receipt) => Some(executor.reverse(receipt).await),
        Err(_) => None,
    };
    let after_unrelated = commitment_date(&odoo, order).await;

    // 2. A changed promise refuses it, until the promise is put back.
    let second = executor.execute(&promoted(&target, REPROMISED)).await;
    if second.is_ok() {
        edit_later(
            &odoo,
            "sale.order",
            order,
            json!({ "commitment_date": "2027-06-15 00:00:00" }),
        )
        .await;
    }
    let refused = match &second {
        Ok(receipt) => Some(executor.reverse(receipt).await),
        Err(_) => None,
    };
    let after_refused = commitment_date(&odoo, order).await;
    edit_later(
        &odoo,
        "sale.order",
        order,
        json!({ "commitment_date": REPROMISED }),
    )
    .await;
    let put_back = match &second {
        Ok(receipt) => Some(executor.reverse(receipt).await),
        Err(_) => None,
    };
    let after_put_back = commitment_date(&odoo, order).await;

    odoo_do(&odoo, move |o| {
        let _ = o.unlink("sale.order", json!([order]));
        let _ = o.unlink("res.partner", json!([partner]));
    })
    .await;

    first.expect("execute re_promise_date");
    second.expect("execute again");
    assert!(
        matches!(unrelated, Some(Ok(()))),
        "an unrelated edit does not refuse: {unrelated:?}"
    );
    assert_eq!(after_unrelated, json!(INITIAL), "the promise is restored");
    assert!(
        matches!(refused, Some(Err(EngineError::ReverseConflict { .. }))),
        "a changed promise refuses: {refused:?}"
    );
    assert_eq!(
        after_refused,
        json!("2027-06-15 00:00:00"),
        "a refused reverse restores nothing"
    );
    assert!(
        matches!(put_back, Some(Ok(()))),
        "the value put back unblocks it: {put_back:?}"
    );
    assert_eq!(after_put_back, json!(INITIAL));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn an_expedite_reverse_is_judged_on_the_written_dates_alone() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = inbound_fixture(&odoo, LINE_PLANNED).await;
    let (line, receipt) = (f.line, f.receipt);
    let target = format!("https://ekg.example/id/purchase.order.line/{line}");
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));
    let expedite = || promoted_verb("expedite_po", &target, "new_date", LINE_EXPEDITED);
    let dates = async || {
        (
            field_of(&odoo, "purchase.order.line", line, "date_planned").await,
            field_of(&odoo, "stock.picking", receipt, "scheduled_date").await,
        )
    };

    // 1. An edit to another field of the receipt does not refuse the reverse.
    let first = executor.execute(&expedite()).await;
    if first.is_ok() {
        edit_later(
            &odoo,
            "stock.picking",
            receipt,
            json!({ "origin": "EKG IT relabel" }),
        )
        .await;
    }
    let unrelated = match &first {
        Ok(r) => Some(executor.reverse(r).await),
        Err(_) => None,
    };
    let after_unrelated = dates().await;

    // 2. A changed line date refuses it (restoring nothing), until the date is put back.
    let second = executor.execute(&expedite()).await;
    if second.is_ok() {
        edit_later(
            &odoo,
            "purchase.order.line",
            line,
            json!({ "date_planned": "2027-11-18 08:00:00" }),
        )
        .await;
    }
    let refused = match &second {
        Ok(r) => Some(executor.reverse(r).await),
        Err(_) => None,
    };
    let after_refused = dates().await;
    edit_later(
        &odoo,
        "purchase.order.line",
        line,
        json!({ "date_planned": LINE_EXPEDITED }),
    )
    .await;
    let put_back = match &second {
        Ok(r) => Some(executor.reverse(r).await),
        Err(_) => None,
    };
    let after_put_back = dates().await;

    drop_inbound_fixture(&odoo, &f).await;

    first.expect("execute expedite_po");
    second.expect("execute again");
    assert!(
        matches!(unrelated, Some(Ok(()))),
        "an unrelated receipt edit does not refuse: {unrelated:?}"
    );
    assert_eq!(after_unrelated, (json!(LINE_PLANNED), json!(LINE_PLANNED)));
    assert!(
        matches!(refused, Some(Err(EngineError::ReverseConflict { .. }))),
        "a changed line date refuses: {refused:?}"
    );
    assert_eq!(
        after_refused,
        (json!("2027-11-18 08:00:00"), json!(LINE_EXPEDITED)),
        "a refused reverse restores nothing"
    );
    assert!(
        matches!(put_back, Some(Ok(()))),
        "the date put back unblocks it: {put_back:?}"
    );
    assert_eq!(after_put_back, (json!(LINE_PLANNED), json!(LINE_PLANNED)));
}

// --- probe the Odoo 19 split ---

/// A throwaway product, component, work center and BoM (one component line, one operation),
/// and a manufacturing order of `qty` for a fixed origin with one work order on the center,
/// the seed's shape. `confirm` confirms the order.
struct SplitFixture {
    template: i64,
    component: i64,
    wc: i64,
    bom: i64,
    mo: i64,
}

async fn split_fixture(odoo: &Arc<Odoo>, qty: f64, confirm: bool) -> SplitFixture {
    odoo_do(odoo, move |o| {
        let uom = o.xmlid("uom", "product_uom_unit").expect("uom");
        let template = o
            .create(
                "product.template",
                json!({ "name": "EKG split IT", "type": "consu", "uom_id": uom }),
            )
            .expect("create product");
        let variant = o
            .read_m2o("product.template", template, "product_variant_id")
            .expect("variant");
        let component = o
            .create(
                "product.template",
                json!({ "name": "EKG split IT part", "type": "consu", "uom_id": uom }),
            )
            .expect("create component");
        let part = o
            .read_m2o("product.template", component, "product_variant_id")
            .expect("part variant");
        let wc = o
            .create(
                "mrp.workcenter",
                json!({ "name": "EKG split WC (IT)", "code": "EKGS" }),
            )
            .expect("create work center");
        let bom = o
            .create(
                "mrp.bom",
                json!({
                    "product_tmpl_id": template,
                    "product_qty": 1,
                    "bom_line_ids": [[0, 0, { "product_id": part, "product_qty": 1 }]],
                }),
            )
            .expect("create bom");
        let op = o
            .create(
                "mrp.routing.workcenter",
                json!({ "bom_id": bom, "workcenter_id": wc, "name": "Assemble", "sequence": 10, "time_cycle_manual": 60.0 }),
            )
            .expect("create operation");
        let mo = o
            .create(
                "mrp.production",
                json!({
                    "product_id": variant,
                    "bom_id": bom,
                    "product_qty": qty,
                    "product_uom_id": uom,
                    "origin": "EKG-SPLIT-SO",
                    "date_start": "2027-10-22 08:00:00",
                    "date_finished": "2027-10-23 08:00:00",
                }),
            )
            .expect("create mo");
        let existing = o
            .search("mrp.workorder", json!([["production_id", "=", mo]]))
            .expect("search wo");
        if existing.is_empty() {
            o.create(
                "mrp.workorder",
                json!({
                    "production_id": mo, "workcenter_id": wc, "operation_id": op,
                    "name": "Assemble", "date_start": "2027-10-22 08:00:00",
                    "date_finished": "2027-10-22 12:00:00", "duration_expected": 240.0,
                }),
            )
            .expect("create wo");
        } else {
            o.write(
                "mrp.workorder",
                json!(existing),
                json!({ "date_start": "2027-10-22 08:00:00", "date_finished": "2027-10-22 12:00:00" }),
            )
            .expect("plan wo");
        }
        if confirm {
            o.call_method("mrp.production", "action_confirm", json!({ "ids": [mo] }))
                .expect("confirm");
        }
        SplitFixture {
            template,
            component,
            wc,
            bom,
            mo,
        }
    })
    .await
}

const MO_FIELDS: [&str; 9] = [
    "name",
    "state",
    "product_qty",
    "date_start",
    "date_finished",
    "origin",
    "production_group_id",
    "workorder_ids",
    "write_date",
];
const WO_FIELDS: [&str; 4] = [
    "production_id",
    "date_start",
    "date_finished",
    "duration_expected",
];

/// Remove a [`SplitFixture`] and every lot split from it (best effort).
async fn drop_split_fixture(odoo: &Arc<Odoo>, f: &SplitFixture) {
    let (template, component, wc, bom) = (f.template, f.component, f.wc, f.bom);
    odoo_do(odoo, move |o| {
        let ids = o
            .search(
                "mrp.production",
                json!([["product_id.product_tmpl_id", "=", template]]),
            )
            .unwrap_or_default();
        let _ = o.call_method("mrp.production", "action_cancel", json!({ "ids": ids }));
        let _ = o.unlink("mrp.production", json!(ids));
        let _ = o.unlink("mrp.bom", json!([bom]));
        let _ = o.unlink("mrp.workcenter", json!([wc]));
        let _ = o.unlink("product.template", json!([template, component]));
    })
    .await;
}

/// Pins the Odoo 19 behaviour `split_production` is built on, for a draft (the seed's
/// shape) and a confirmed order:
/// - `mrp.production.split` + `action_split` can be driven through json2;
/// - the original keeps its id and origin, is renamed to backorder naming (`<name>-001`)
///   and reduced; the split-off lot is a new record (`<name>-002`) in the same production
///   group with the same origin, starting at its line's date;
/// - Odoo clears both lots' work-order windows and rescales `duration_expected` by
///   quantity, so the executor must re-plan them;
/// - a work-order write carrying `duration_expected` keeps the given window, and an
///   MO date write under the `force_date` context keeps its work orders planned;
/// - the inverse holds: `change.production.qty` restores the quantity, a `name` write the
///   reference, and `action_cancel` + `unlink` remove the split-off lot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn split_probe_pins_the_odoo_split_and_its_inverse() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    for confirm in [false, true] {
        let f = split_fixture(&odoo, 40.0, confirm).await;
        let mo = f.mo;
        let seen = odoo_do(&odoo, move |o| {
            let read_mo = |ids: Value| o.read("mrp.production", ids, json!(MO_FIELDS)).expect("read mo");
            let work_orders_of = |ids: Value| o.read("mrp.workorder", ids, json!(WO_FIELDS)).expect("read wo");
            let before = read_mo(json!([mo]))[0].clone();
            let wo = before["workorder_ids"][0].as_i64().expect("a work order");
            let wizard = o
                .create(
                    "mrp.production.split",
                    json!({
                        "production_id": mo,
                        "production_detailed_vals_ids": [
                            [0, 0, { "quantity": 30.0, "date": "2027-10-22 08:00:00" }],
                            [0, 0, { "quantity": 10.0, "date": "2027-10-25 08:00:00" }],
                        ],
                    }),
                )
                .expect("create the split wizard");
            o.call_method("mrp.production.split", "action_split", json!({ "ids": [wizard] }))
                .expect("action_split");
            let group = before["production_group_id"][0].as_i64().expect("group");
            let off = o
                .search(
                    "mrp.production",
                    json!([["production_group_id", "=", group], ["id", "!=", mo]]),
                )
                .expect("split-off")
                .into_iter()
                .max()
                .expect("a split-off lot");
            let split = read_mo(json!([mo, off]));
            let off_wo = split[1]["workorder_ids"][0].as_i64().expect("split-off work order");
            let cleared = work_orders_of(json!([wo, off_wo]));
            // Re-plan: work orders first (windows + duration), then the MO dates.
            for (id, start, finish, duration) in [
                (wo, "2027-10-22 08:00:00", "2027-10-22 12:00:00", &cleared[0]["duration_expected"]),
                (off_wo, "2027-10-25 08:00:00", "2027-10-25 12:00:00", &cleared[1]["duration_expected"]),
            ] {
                o.write(
                    "mrp.workorder",
                    json!([id]),
                    json!({ "date_start": start, "date_finished": finish, "duration_expected": duration }),
                )
                .expect("re-plan a work order");
            }
            o.call_method(
                "mrp.production",
                "write",
                json!({
                    "ids": [mo],
                    "vals": { "date_start": "2027-10-22 08:00:00", "date_finished": "2027-10-23 08:00:00" },
                    "context": { "force_date": true },
                }),
            )
            .expect("force the MO dates");
            let replanned_mo = read_mo(json!([mo]))[0].clone();
            let replanned = work_orders_of(json!([wo, off_wo]));
            // The inverse.
            let change = o
                .create("change.production.qty", json!({ "mo_id": mo, "product_qty": 40.0 }))
                .expect("create the quantity wizard");
            o.call_method("change.production.qty", "change_prod_qty", json!({ "ids": [change] }))
                .expect("change_prod_qty");
            o.write("mrp.production", json!([mo]), json!({ "name": before["name"] }))
                .expect("restore the name");
            o.call_method("mrp.production", "action_cancel", json!({ "ids": [off] }))
                .expect("cancel the split-off");
            o.unlink("mrp.production", json!([off])).expect("unlink the split-off");
            let restored = read_mo(json!([mo]))[0].clone();
            let restored_wo = work_orders_of(json!([wo]))[0].clone();
            let gone = o.search("mrp.production", json!([["id", "=", off]])).expect("search");
            (before, split, cleared, replanned_mo, replanned, restored, restored_wo, gone, off)
        })
        .await;
        drop_split_fixture(&odoo, &f).await;
        let (before, split, cleared, replanned_mo, replanned, restored, restored_wo, gone, off) =
            seen;
        println!(
            "confirm={confirm}: split {split}; cleared {cleared}; replanned {replanned_mo} {replanned}; restored {restored} {restored_wo}"
        );
        let name = before["name"].as_str().expect("name").to_owned();

        // The original keeps its id and origin, is renamed and reduced.
        assert_eq!(split[0]["id"].as_i64(), Some(mo));
        assert_eq!(split[0]["name"], json!(format!("{name}-001")));
        assert_eq!(split[0]["product_qty"], json!(30.0));
        assert_eq!(split[0]["origin"], before["origin"]);
        // The split-off is a new lot in the same group, same origin, from its line's date.
        assert_eq!(split[1]["id"].as_i64(), Some(off));
        assert_eq!(split[1]["name"], json!(format!("{name}-002")));
        assert_eq!(split[1]["product_qty"], json!(10.0));
        assert_eq!(split[1]["origin"], before["origin"]);
        assert_eq!(
            split[1]["production_group_id"],
            before["production_group_id"]
        );
        assert_eq!(split[1]["date_start"], json!("2027-10-25 08:00:00"));
        assert_eq!(split[1]["state"], before["state"]);
        // Odoo clears both lots' work-order windows and rescales the expected duration.
        for wo in cleared.as_array().expect("rows") {
            assert_eq!(wo["date_start"], json!(false), "{wo}");
            assert_eq!(wo["date_finished"], json!(false), "{wo}");
        }
        assert_eq!(cleared[0]["duration_expected"], json!(1800.0));
        assert_eq!(cleared[1]["duration_expected"], json!(600.0));
        // A window written with its duration sticks; forced MO dates keep them planned.
        assert_eq!(replanned[0]["date_start"], json!("2027-10-22 08:00:00"));
        assert_eq!(replanned[0]["date_finished"], json!("2027-10-22 12:00:00"));
        assert_eq!(replanned[1]["date_start"], json!("2027-10-25 08:00:00"));
        assert_eq!(replanned[1]["date_finished"], json!("2027-10-25 12:00:00"));
        assert_eq!(replanned_mo["date_start"], json!("2027-10-22 08:00:00"));
        assert_eq!(replanned_mo["date_finished"], json!("2027-10-23 08:00:00"));
        // The inverse restores the quantity and the reference and removes the split-off.
        assert_eq!(restored["product_qty"], json!(40.0));
        assert_eq!(restored["name"], json!(name));
        assert!(gone.is_empty(), "the split-off lot is gone");
        assert_eq!(restored_wo["duration_expected"], json!(2400.0));
    }
}

/// A promoted action carrying several params.
fn promoted_params(action_type: &str, target: &str, pairs: &[(&str, &str)]) -> PromotedAction {
    let mut promoted = promoted_verb(action_type, target, "unused", "");
    promoted.action.params = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    promoted
}

/// A manufacturing order's quantity, name, dates and work orders, and those work orders'
/// windows.
async fn mo_state(odoo: &Arc<Odoo>, id: i64) -> (Value, Value) {
    odoo_do(odoo, move |o| {
        let mo = o
            .read("mrp.production", json!([id]), json!(MO_FIELDS))
            .expect("read mo")[0]
            .clone();
        let wos = o
            .read(
                "mrp.workorder",
                mo["workorder_ids"].clone(),
                json!(WO_FIELDS),
            )
            .expect("read wos");
        (mo, wos)
    })
    .await
}

/// The governed split roundtrip on a confirmed order of 40: the write-back leaves the
/// original at 30 with its schedule and a split-off lot of 10 planned from `new_date`;
/// one reverse restores the quantity, reference and schedule and removes the split-off
/// lot; a reverse after a source change is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn executes_and_reverses_a_governed_split_and_refuses_a_conflicting_reverse() {
    const NEW_DATE: &str = "2027-10-25 08:00:00";
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = split_fixture(&odoo, 40.0, true).await;
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));
    let target = format!("https://ekg.example/id/mrp.production/{}", f.mo);
    let split = promoted_params(
        "split_production",
        &target,
        &[("split_quantity", "10"), ("new_date", NEW_DATE)],
    );
    let (before, before_wos) = mo_state(&odoo, f.mo).await;

    let receipt = executor.execute(&split).await.expect("execute the split");
    let (after, after_wos) = mo_state(&odoo, f.mo).await;
    let template = f.template;
    let lots = odoo_do(&odoo, move |o| {
        o.search_read(
            "mrp.production",
            json!([["product_id.product_tmpl_id", "=", template]]),
            json!(MO_FIELDS),
        )
        .expect("lots")
    })
    .await;
    let off = lots
        .as_array()
        .and_then(|l| l.iter().find(|l| l["id"].as_i64() != Some(f.mo)))
        .cloned()
        .expect("a split-off lot");
    let off_id = off["id"].as_i64().expect("id");
    let (_, off_wos) = mo_state(&odoo, off_id).await;

    // A benign edit of the original does not refuse the reverse.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let original = f.mo;
    odoo_do(&odoo, move |o| {
        o.write(
            "mrp.production",
            json!([original]),
            json!({ "priority": "1" }),
        )
        .expect("benign edit");
    })
    .await;
    executor.reverse(&receipt).await.expect("reverse the split");
    let (restored, restored_wos) = mo_state(&odoo, f.mo).await;
    let gone = odoo_do(&odoo, move |o| {
        o.search("mrp.production", json!([["id", "=", off_id]]))
            .expect("search")
    })
    .await;

    // A reverse after a change to a field the split set (the original's reference) is
    // refused and writes nothing.
    let receipt2 = executor.execute(&split).await.expect("split again");
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let mo = f.mo;
    odoo_do(&odoo, move |o| {
        o.write(
            "mrp.production",
            json!([mo]),
            json!({ "name": "EKG renamed lot" }),
        )
        .expect("intervening source change");
    })
    .await;
    let conflict = executor.reverse(&receipt2).await;
    let (after_conflict, _) = mo_state(&odoo, f.mo).await;
    drop_split_fixture(&odoo, &f).await;

    println!(
        "before {before} {before_wos}\nafter {after} {after_wos}\noff {off} {off_wos}\nrestored {restored} {restored_wos}"
    );
    let name = before["name"].as_str().expect("name").to_owned();
    // The write-back: the original at 30, keeping its schedule and work-order windows.
    assert_eq!(after["product_qty"], json!(30.0));
    assert_eq!(after["name"], json!(format!("{name}-001")));
    assert_eq!(after["date_start"], before["date_start"]);
    assert_eq!(after["date_finished"], before["date_finished"]);
    assert_eq!(after_wos[0]["date_start"], before_wos[0]["date_start"]);
    assert_eq!(
        after_wos[0]["date_finished"],
        before_wos[0]["date_finished"]
    );
    // The split-off lot of 10, same origin, planned from the new date with the window
    // shifted (its length kept).
    assert_eq!(off["product_qty"], json!(10.0));
    assert_eq!(off["origin"], before["origin"]);
    assert_eq!(off["date_start"], json!(NEW_DATE));
    assert_eq!(off_wos[0]["date_start"], json!(NEW_DATE));
    let len = |wo: &Value| {
        odoo_datetime_to_epoch(wo["date_finished"].as_str().unwrap_or_default())
            - odoo_datetime_to_epoch(wo["date_start"].as_str().unwrap_or_default())
    };
    assert_eq!(
        len(&off_wos[0]),
        len(&before_wos[0]),
        "the window length is kept"
    );
    // One reverse restores quantity, reference and schedule, and removes the lot.
    assert_eq!(restored["product_qty"], json!(40.0));
    assert_eq!(restored["name"], json!(name));
    assert_eq!(restored["date_start"], before["date_start"]);
    assert_eq!(restored["date_finished"], before["date_finished"]);
    assert_eq!(restored_wos[0]["date_start"], before_wos[0]["date_start"]);
    assert_eq!(
        restored_wos[0]["date_finished"],
        before_wos[0]["date_finished"]
    );
    assert!(gone.is_empty(), "the split-off lot is removed");
    // The conflicting reverse is refused, and the split stays as it was.
    assert!(
        matches!(conflict, Err(EngineError::ReverseConflict { .. })),
        "{conflict:?}"
    );
    assert_eq!(after_conflict["product_qty"], json!(30.0));
}

/// The governed capacity roundtrip: from 1 to 2 and back, and a value that does not raise
/// the capacity is refused before any write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn executes_and_reverses_a_governed_capacity_raise_and_refuses_a_non_raise() {
    let Some(odoo) = live_odoo().await else {
        return;
    };
    let f = split_fixture(&odoo, 1.0, false).await;
    let (template, wc) = (f.template, f.wc);
    let record = odoo_do(&odoo, move |o| {
        let variant = o
            .read_m2o("product.template", template, "product_variant_id")
            .expect("variant");
        o.create(
            "mrp.workcenter.capacity",
            json!({ "workcenter_id": wc, "product_id": variant, "capacity": 1.0 }),
        )
        .expect("create capacity")
    })
    .await;
    let capacity = async || field_of(&odoo, "mrp.workcenter.capacity", record, "capacity").await;
    let executor = OdooActionExecutor::new(Arc::clone(&odoo));
    let target = format!("https://ekg.example/id/mrp.workcenter.capacity/{record}");
    let raise = |to: &str| {
        promoted_params(
            "raise_work_center_capacity",
            &target,
            &[("new_capacity", to)],
        )
    };

    let receipt = executor.execute(&raise("2")).await.expect("raise");
    let raised = capacity().await;
    executor.reverse(&receipt).await.expect("reverse");
    let reversed = capacity().await;
    let same = executor.execute(&raise("1")).await;
    let lower = executor.execute(&raise("0.5")).await;
    let untouched = capacity().await;
    odoo_do(&odoo, move |o| {
        let _ = o.unlink("mrp.workcenter.capacity", json!([record]));
    })
    .await;
    drop_split_fixture(&odoo, &f).await;

    assert_eq!(raised, json!(2.0));
    assert_eq!(
        reversed,
        json!(1.0),
        "the inverse restores the prior capacity"
    );
    assert!(
        matches!(same, Err(EngineError::InvalidProposal(_))),
        "{same:?}"
    );
    assert!(
        matches!(lower, Err(EngineError::InvalidProposal(_))),
        "{lower:?}"
    );
    assert_eq!(untouched, json!(1.0), "a refused raise writes nothing");
}
