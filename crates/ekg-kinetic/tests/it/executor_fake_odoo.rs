//! Hermetic executor tests over an in-process fake of Odoo's json2 API.
//!
//! The fake speaks just enough json2 (`read`, `write`, `search_read`, the connect
//! probe) over a real socket that the production `OdooActionExecutor` drives it
//! unchanged. It keeps records in memory, stamps a monotonic `write_date` on every
//! write, and mimics the Odoo 19 side effect pinned by `writeback_it`: writing a
//! purchase-order line's `date_planned` touches (bumps `write_date` on) the receipts
//! of its order without moving their `scheduled_date`. A flag makes it move them
//! too, for the "Odoo already moved it" path. It also refuses, like Odoo 19, a
//! work-order write that would leave the planned start after the planned finish
//! (a `UserError`, pinned by `writeback_it`), so a reschedule must move the whole
//! window in one write.

// In tests `expect`/`panic` are the assertion mechanism.
#![allow(clippy::expect_used, clippy::panic, clippy::doc_markdown)]

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use ekg_domain::{
    ActionExecutor, ActionPointId, ActionType, CorrelationId, EngineError, EntityUri,
    ExecutionReceipt, PromotedAction, Promotion, ProposedAction, ReviewerId, Revision,
};
use ekg_kinetic::OdooActionExecutor;
use ekg_kinetic::verbs::Undo;
use odoo_client::Odoo;
use serde_json::{Map, Value, json};

const LINE: &str = "purchase.order.line";
const PICKING: &str = "stock.picking";
const WO: &str = "mrp.workorder";

// --- the fake ---

#[derive(Default)]
struct Store {
    records: HashMap<String, BTreeMap<i64, Map<String, Value>>>,
    clock: u64,
    /// When set, a line `date_planned` write also moves its order's receipts.
    line_write_moves_receipts: bool,
    /// Every `(model, id)` written, in order.
    writes: Vec<(String, i64)>,
    /// The fields of every write, in the same order as `writes`.
    written_fields: Vec<Vec<String>>,
}

impl Store {
    fn stamp(&mut self) -> Value {
        self.clock += 1;
        let c = self.clock;
        json!(format!(
            "2027-01-07 {:02}:{:02}:{:02}",
            c / 3600,
            (c / 60) % 60,
            c % 60
        ))
    }

    fn insert(&mut self, model: &str, id: i64, fields: Value) {
        let Value::Object(mut row) = fields else {
            panic!("fake json2: a record is a JSON object")
        };
        row.insert("id".to_owned(), json!(id));
        row.insert("write_date".to_owned(), self.stamp());
        self.records
            .entry(model.to_owned())
            .or_default()
            .insert(id, row);
    }

    fn get(&self, model: &str, id: i64, field: &str) -> Value {
        self.records
            .get(model)
            .and_then(|m| m.get(&id))
            .and_then(|r| r.get(field))
            .cloned()
            .unwrap_or(Value::Bool(false))
    }

    /// An out-of-band edit (a user in Odoo). Advances the record's watermark.
    fn external_edit(&mut self, model: &str, id: i64, field: &str, value: Value) {
        let stamp = self.stamp();
        let row = self
            .records
            .get_mut(model)
            .and_then(|m| m.get_mut(&id))
            .expect("record");
        row.insert(field.to_owned(), value);
        row.insert("write_date".to_owned(), stamp);
    }

    /// Apply a write, or refuse it (changing nothing) with Odoo's error body.
    fn write(&mut self, model: &str, id: i64, vals: &Map<String, Value>) -> Result<(), Value> {
        // Odoo 19: a work order's planned finish may not precede its start. Fixed-width
        // datetimes compare chronologically as strings.
        if model == WO {
            let after = |f: &str| {
                vals.get(f)
                    .cloned()
                    .unwrap_or_else(|| self.get(model, id, f))
            };
            if let (Value::String(start), Value::String(finish)) =
                (after("date_start"), after("date_finished"))
                && finish < start
            {
                return Err(json!({
                    "name": "odoo.exceptions.UserError",
                    "message": "The planned end date of the work order cannot be prior to the planned start date, please correct this to save the work order.",
                }));
            }
        }
        let stamp = self.stamp();
        let row = self
            .records
            .get_mut(model)
            .and_then(|m| m.get_mut(&id))
            .expect("write to a missing record");
        for (k, v) in vals {
            row.insert(k.clone(), v.clone());
        }
        row.insert("write_date".to_owned(), stamp.clone());
        self.writes.push((model.to_owned(), id));
        self.written_fields.push(vals.keys().cloned().collect());

        // Odoo 19: a line date write touches its order's receipts.
        if model == LINE
            && let Some(date) = vals.get("date_planned")
        {
            let po = self.get(LINE, id, "order_id");
            let moves = self.line_write_moves_receipts;
            if let Some(pickings) = self.records.get_mut(PICKING) {
                for row in pickings.values_mut() {
                    if row.get("purchase_id") == Some(&po) {
                        row.insert("write_date".to_owned(), stamp.clone());
                        if moves {
                            row.insert("scheduled_date".to_owned(), date.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn matches(row: &Map<String, Value>, domain: &Value) -> bool {
        domain.as_array().into_iter().flatten().all(|leaf| {
            let (field, op, want) = (
                leaf[0].as_str().unwrap_or_default(),
                leaf[1].as_str().unwrap_or_default(),
                &leaf[2],
            );
            // A many-to-one is stored as Odoo renders it, `[id, name]`, and matches
            // on its id.
            let have = match row.get(field) {
                Some(Value::Array(pair)) => pair.first().cloned().unwrap_or(Value::Bool(false)),
                Some(v) => v.clone(),
                None => Value::Bool(false),
            };
            match op {
                "=" => &have == want,
                "not in" => !want.as_array().is_some_and(|a| a.contains(&have)),
                other => panic!("fake json2: unsupported domain operator {other:?}"),
            }
        })
    }

    fn project(row: &Map<String, Value>, fields: &Value) -> Value {
        let mut out = Map::new();
        out.insert("id".to_owned(), row["id"].clone());
        for f in fields
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            out.insert(
                f.to_owned(),
                row.get(f).cloned().unwrap_or(Value::Bool(false)),
            );
        }
        Value::Object(out)
    }

    /// One json2 call: `Ok(result)`, or `Err(error body)` for an Odoo-side refusal.
    fn handle(&mut self, model: &str, method: &str, body: &Value) -> Result<Value, Value> {
        Ok(match method {
            "search_count" => json!(1),
            "read" => {
                let rows = self.records.get(model);
                let out: Vec<Value> = body["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_i64)
                    .filter_map(|id| rows.and_then(|m| m.get(&id)))
                    .map(|row| Self::project(row, &body["fields"]))
                    .collect();
                Value::Array(out)
            }
            "search_read" => {
                let out: Vec<Value> = self
                    .records
                    .get(model)
                    .into_iter()
                    .flat_map(BTreeMap::values)
                    .filter(|row| Self::matches(row, &body["domain"]))
                    .map(|row| Self::project(row, &body["fields"]))
                    .collect();
                Value::Array(out)
            }
            "write" => {
                let vals = body["vals"].as_object().cloned().unwrap_or_default();
                for id in body["ids"].as_array().into_iter().flatten() {
                    self.write(model, id.as_i64().expect("id"), &vals)?;
                }
                json!(true)
            }
            other => panic!("fake json2: unsupported method {model}.{other}"),
        })
    }
}

/// Serve the store over HTTP on a random local port; returns the base URL.
fn serve(store: Arc<Mutex<Store>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake odoo");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut len = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                if header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().expect("content-length");
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).expect("body");
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

            // POST /json/2/<model>/<method>?db=<db>
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let path = path.split('?').next().unwrap_or_default();
            let mut parts = path.trim_start_matches("/json/2/").splitn(2, '/');
            let (model, method) = (
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
            );
            let result = store
                .lock()
                .expect("store lock")
                .handle(&model, &method, &body);
            let (status, payload) = match result {
                Ok(result) => ("200 OK", result.to_string()),
                Err(error) => ("422 Unprocessable Entity", error.to_string()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    url
}

// --- fixture ---

const PO: i64 = 4;
const LINE_ID: i64 = 40;
const OPEN_RECEIPT: i64 = 3;
const PLANNED: &str = "2027-11-15 08:00:00";
const EXPEDITED: &str = "2027-10-12 08:00:00";

/// A PO line whose order has one open receipt, plus receipts the expedite must
/// leave alone: a done one, a cancelled one, a return (non-internal destination),
/// and an open receipt of another order.
fn flagship_store(line_write_moves_receipts: bool) -> Arc<Mutex<Store>> {
    let mut s = Store {
        line_write_moves_receipts,
        ..Store::default()
    };
    let po = json!([PO, "P00001"]);
    s.insert(
        LINE,
        LINE_ID,
        json!({ "date_planned": PLANNED, "order_id": po }),
    );
    let receipt = |state: &str, code: &str, usage: &str, purchase: &Value| {
        json!({
            "scheduled_date": PLANNED,
            "state": state,
            "picking_type_code": code,
            "location_dest_id.usage": usage,
            "purchase_id": purchase,
        })
    };
    s.insert(
        PICKING,
        OPEN_RECEIPT,
        receipt("assigned", "incoming", "internal", &po),
    );
    s.insert(PICKING, 5, receipt("done", "incoming", "internal", &po));
    s.insert(PICKING, 6, receipt("cancel", "incoming", "internal", &po));
    s.insert(PICKING, 7, receipt("assigned", "incoming", "supplier", &po));
    s.insert(
        PICKING,
        8,
        receipt("assigned", "incoming", "internal", &json!([9, "P00009"])),
    );
    s.insert(
        "sale.order",
        1,
        json!({ "commitment_date": "2027-10-22 00:00:00" }),
    );
    Arc::new(Mutex::new(s))
}

async fn executor(store: &Arc<Mutex<Store>>) -> OdooActionExecutor {
    let url = serve(Arc::clone(store));
    let odoo = tokio::task::spawn_blocking(move || Odoo::connect(&url, "ekg", "admin", "key"))
        .await
        .expect("join")
        .expect("connect to the fake");
    OdooActionExecutor::new(Arc::new(odoo))
}

fn promoted(verb: &str, target: &str, new_date: &str) -> PromotedAction {
    let mut params = BTreeMap::new();
    params.insert("new_date".to_owned(), new_date.to_owned());
    PromotedAction {
        action: ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: EntityUri(target.to_owned()),
            params,
        },
        promotion: Promotion {
            action_point: ActionPointId("ap-fake".to_owned()),
            correlation_id: CorrelationId("corr-fake".to_owned()),
            reviewer: ReviewerId("reviewer-fake".to_owned()),
            rationale: "hermetic executor test".to_owned(),
            context_seen: "test".to_owned(),
        },
        basis_revision: Revision(0),
    }
}

fn line_target() -> String {
    format!("https://ekg.example/id/purchase.order.line/{LINE_ID}")
}

fn get(store: &Arc<Mutex<Store>>, model: &str, id: i64, field: &str) -> Value {
    store.lock().expect("lock").get(model, id, field)
}

fn restored_records(receipt: &ExecutionReceipt) -> Vec<(String, i64)> {
    match Undo::decode(&receipt.external_ref).expect("undo") {
        Undo::RestoreFields { restores } => restores.into_iter().map(|r| (r.model, r.id)).collect(),
        other => panic!("expedite must record a multi-field restore, got {other:?}"),
    }
}

// --- tests ---

#[tokio::test(flavor = "multi_thread")]
async fn expedite_moves_the_line_and_its_open_receipt_and_reverses_both_in_one_step() {
    let store = flagship_store(false);
    let exec = executor(&store).await;

    let receipt = exec
        .execute(&promoted("expedite_po", &line_target(), EXPEDITED))
        .await
        .expect("expedite");

    assert_eq!(get(&store, LINE, LINE_ID, "date_planned"), json!(EXPEDITED));
    assert_eq!(
        get(&store, PICKING, OPEN_RECEIPT, "scheduled_date"),
        json!(EXPEDITED),
        "the open receipt's arrival moved with the line"
    );
    for untouched in [5, 6, 7, 8] {
        assert_eq!(
            get(&store, PICKING, untouched, "scheduled_date"),
            json!(PLANNED),
            "receipt {untouched} is done, cancelled, a return, or another order's"
        );
    }
    assert_eq!(
        restored_records(&receipt),
        vec![
            (LINE.to_owned(), LINE_ID),
            (PICKING.to_owned(), OPEN_RECEIPT)
        ]
    );
    assert!(receipt.reversible);

    exec.reverse(&receipt).await.expect("one-step reverse");
    assert_eq!(get(&store, LINE, LINE_ID, "date_planned"), json!(PLANNED));
    assert_eq!(
        get(&store, PICKING, OPEN_RECEIPT, "scheduled_date"),
        json!(PLANNED)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_receipt_odoo_already_moved_is_not_written_or_restored() {
    let store = flagship_store(true);
    let exec = executor(&store).await;

    let receipt = exec
        .execute(&promoted("expedite_po", &line_target(), EXPEDITED))
        .await
        .expect("expedite");

    assert_eq!(
        get(&store, PICKING, OPEN_RECEIPT, "scheduled_date"),
        json!(EXPEDITED)
    );
    let writes = store.lock().expect("lock").writes.clone();
    assert_eq!(
        writes,
        vec![(LINE.to_owned(), LINE_ID)],
        "only the line is written when Odoo moves the receipt itself"
    );
    assert_eq!(restored_records(&receipt), vec![(LINE.to_owned(), LINE_ID)]);

    exec.reverse(&receipt).await.expect("reverse");
    assert_eq!(get(&store, LINE, LINE_ID, "date_planned"), json!(PLANNED));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expedite_on_anything_but_a_po_line_is_refused_before_any_write() {
    let store = flagship_store(false);
    let exec = executor(&store).await;

    for target in [
        "https://ekg.example/id/sale.order/1",
        "https://ekg.example/id/purchase.order/4",
    ] {
        let refused = exec
            .execute(&promoted("expedite_po", target, EXPEDITED))
            .await;
        assert!(
            matches!(refused, Err(EngineError::InvalidProposal(_))),
            "{target}: {refused:?}"
        );
    }
    assert!(store.lock().expect("lock").writes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reverse_after_an_external_receipt_edit_is_refused_and_restores_nothing() {
    let store = flagship_store(false);
    let exec = executor(&store).await;

    let receipt = exec
        .execute(&promoted("expedite_po", &line_target(), EXPEDITED))
        .await
        .expect("expedite");
    store.lock().expect("lock").external_edit(
        PICKING,
        OPEN_RECEIPT,
        "scheduled_date",
        json!("2027-10-26 08:00:00"),
    );

    let conflict = exec.reverse(&receipt).await;
    assert!(
        matches!(conflict, Err(EngineError::ReverseConflict { .. })),
        "{conflict:?}"
    );
    assert_eq!(
        get(&store, LINE, LINE_ID, "date_planned"),
        json!(EXPEDITED),
        "the line is not restored when another record of the step conflicts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_single_field_receipt_still_reverses() {
    // A stored receipt in the single-field wire shape stays reversible.
    let store = flagship_store(false);
    let exec = executor(&store).await;
    store.lock().expect("lock").external_edit(
        "sale.order",
        1,
        "commitment_date",
        json!("2027-12-31 00:00:00"),
    );
    let watermark = 1_799_280_000 + store.lock().expect("lock").clock;

    let legacy = ExecutionReceipt {
        action: promoted(
            "re_promise_date",
            "https://ekg.example/id/sale.order/1",
            "2027-12-31 00:00:00",
        )
        .action,
        external_ref: r#"{"kind":"restore_field","model":"sale.order","id":1,"field":"commitment_date","prior":"2027-10-22 00:00:00"}"#.to_owned(),
        source_revision: Revision(watermark),
        reversible: true,
    };
    exec.reverse(&legacy).await.expect("legacy reverse");
    assert_eq!(
        get(&store, "sale.order", 1, "commitment_date"),
        json!("2027-10-22 00:00:00")
    );
}

// --- reschedule a work order ---

const WO_ID: i64 = 3;
const WO_START: &str = "2027-10-23 08:00:00";
const WO_FINISH: &str = "2027-10-23 12:00:00";

/// A work order planned from `WO_START` to `finish` (unset when `None`).
fn work_order_store(finish: Option<&str>) -> Arc<Mutex<Store>> {
    let mut s = Store::default();
    s.insert(
        WO,
        WO_ID,
        json!({
            "date_start": WO_START,
            "date_finished": finish.map_or(Value::Bool(false), |f| json!(f)),
            "duration_expected": 240.0,
        }),
    );
    Arc::new(Mutex::new(s))
}

fn wo_target() -> String {
    format!("https://ekg.example/id/mrp.workorder/{WO_ID}")
}

fn window(store: &Arc<Mutex<Store>>) -> (Value, Value) {
    (
        get(store, WO, WO_ID, "date_start"),
        get(store, WO, WO_ID, "date_finished"),
    )
}

/// The last write's fields, sorted.
fn last_write_fields(store: &Arc<Mutex<Store>>) -> Vec<String> {
    let mut fields = store
        .lock()
        .expect("lock")
        .written_fields
        .last()
        .cloned()
        .expect("a write");
    fields.sort();
    fields
}

fn write_count(store: &Arc<Mutex<Store>>) -> usize {
    store.lock().expect("lock").writes.len()
}

#[test]
fn the_fake_refuses_a_start_only_write_past_the_finish_like_odoo() {
    // Guards the tests below: a start-only move past the finish would be refused.
    let store = work_order_store(Some(WO_FINISH));
    let refused = store.lock().expect("lock").handle(
        WO,
        "write",
        &json!({ "ids": [WO_ID], "vals": { "date_start": "2027-10-25 08:00:00" } }),
    );
    assert!(refused.is_err());
    assert_eq!(window(&store), (json!(WO_START), json!(WO_FINISH)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reschedule_moves_the_whole_window_in_one_write_and_reverses_it_in_one_write() {
    // Later and earlier than the window's length, so either end written alone first
    // would leave start after finish (the fake, like Odoo, refuses that).
    for (new_start, new_finish) in [
        ("2027-10-25 08:00:00", "2027-10-25 12:00:00"),
        ("2027-10-21 08:00:00", "2027-10-21 12:00:00"),
    ] {
        let store = work_order_store(Some(WO_FINISH));
        let exec = executor(&store).await;

        let receipt = exec
            .execute(&promoted("reschedule_work_order", &wo_target(), new_start))
            .await
            .expect("reschedule");

        assert_eq!(
            window(&store),
            (json!(new_start), json!(new_finish)),
            "start and finish moved together, keeping the 4 h duration"
        );
        assert_eq!(write_count(&store), 1, "one write for the window");
        assert_eq!(last_write_fields(&store), ["date_finished", "date_start"]);
        let Undo::RestoreFields { restores } = Undo::decode(&receipt.external_ref).expect("undo")
        else {
            panic!("a reschedule records one multi-field restore");
        };
        let restored: Vec<_> = restores
            .iter()
            .map(|r| (r.model.as_str(), r.id, r.field.as_str(), r.prior.clone()))
            .collect();
        assert_eq!(
            restored,
            vec![
                (WO, WO_ID, "date_start", json!(WO_START)),
                (WO, WO_ID, "date_finished", json!(WO_FINISH)),
                // Odoo recomputes the finish from the duration, so the restore carries it
                // and an edit to it refuses the reverse.
                (
                    WO,
                    WO_ID,
                    "duration_expected",
                    get(&store, WO, WO_ID, "duration_expected")
                ),
            ]
        );
        assert!(
            restores
                .iter()
                .all(|r| Revision(r.watermark) == receipt.source_revision),
            "each restore carries the record's post-write watermark"
        );

        exec.reverse(&receipt).await.expect("one-step reverse");
        assert_eq!(window(&store), (json!(WO_START), json!(WO_FINISH)));
        assert_eq!(write_count(&store), 2, "the reverse is one write");
        assert_eq!(
            last_write_fields(&store),
            ["date_finished", "date_start", "duration_expected"]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reschedule_of_a_work_order_with_no_finish_writes_the_start_only() {
    let store = work_order_store(None);
    let exec = executor(&store).await;
    let new_start = "2027-10-25 08:00:00";

    let receipt = exec
        .execute(&promoted("reschedule_work_order", &wo_target(), new_start))
        .await
        .expect("reschedule");

    assert_eq!(window(&store), (json!(new_start), Value::Bool(false)));
    assert_eq!(last_write_fields(&store), ["date_start"]);
    let Undo::RestoreFields { restores } = Undo::decode(&receipt.external_ref).expect("undo")
    else {
        panic!("a reschedule records one multi-field restore");
    };
    assert_eq!(restores.len(), 1, "only the start is restored");
    assert_eq!(restores[0].field, "date_start");

    exec.reverse(&receipt).await.expect("reverse");
    assert_eq!(window(&store), (json!(WO_START), Value::Bool(false)));
}

// The guard is value-aware: it refuses only when a field the reschedule wrote changed.
// An edit to another field of the work order does not block the undo.
#[tokio::test(flavor = "multi_thread")]
async fn a_reschedule_reverse_after_an_edit_to_another_field_goes_through_but_not_after_a_duration_edit()
 {
    let store = work_order_store(Some(WO_FINISH));
    let exec = executor(&store).await;
    let receipt = exec
        .execute(&promoted(
            "reschedule_work_order",
            &wo_target(),
            "2027-10-25 08:00:00",
        ))
        .await
        .expect("reschedule");
    store
        .lock()
        .expect("lock")
        .external_edit(WO, WO_ID, "name", json!("renamed step"));

    exec.reverse(&receipt)
        .await
        .expect("an unrelated edit does not refuse");
    assert_eq!(window(&store), (json!(WO_START), json!(WO_FINISH)));
    assert_eq!(
        get(&store, WO, WO_ID, "name"),
        json!("renamed step"),
        "the other edit is kept"
    );

    // The duration is not unrelated: Odoo recomputes the finish from it.
    let receipt = exec
        .execute(&promoted(
            "reschedule_work_order",
            &wo_target(),
            "2027-10-25 08:00:00",
        ))
        .await
        .expect("reschedule again");
    store
        .lock()
        .expect("lock")
        .external_edit(WO, WO_ID, "duration_expected", json!(90.0));
    assert!(matches!(
        exec.reverse(&receipt).await,
        Err(EngineError::ReverseConflict { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reschedule_reverse_is_refused_while_its_window_changed_and_goes_through_once_it_is_back()
{
    let store = work_order_store(Some(WO_FINISH));
    let exec = executor(&store).await;
    let receipt = exec
        .execute(&promoted(
            "reschedule_work_order",
            &wo_target(),
            "2027-10-25 08:00:00",
        ))
        .await
        .expect("reschedule");
    store.lock().expect("lock").external_edit(
        WO,
        WO_ID,
        "date_finished",
        json!("2027-10-25 14:00:00"),
    );

    let conflict = exec.reverse(&receipt).await;
    assert!(
        matches!(conflict, Err(EngineError::ReverseConflict { .. })),
        "{conflict:?}"
    );
    assert_eq!(
        window(&store),
        (json!("2027-10-25 08:00:00"), json!("2027-10-25 14:00:00")),
        "a refused reverse restores nothing"
    );
    assert_eq!(write_count(&store), 1, "only the reschedule itself wrote");

    // The conflicting edit is put back: the reverse goes through.
    store.lock().expect("lock").external_edit(
        WO,
        WO_ID,
        "date_finished",
        json!("2027-10-25 12:00:00"),
    );
    exec.reverse(&receipt).await.expect("the value is back");
    assert_eq!(window(&store), (json!(WO_START), json!(WO_FINISH)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_receipt_without_written_values_is_refused_after_any_edit() {
    // A stored receipt without written values keeps the write_date guard.
    let store = work_order_store(Some(WO_FINISH));
    let exec = executor(&store).await;
    let watermark = 1_799_280_000 + store.lock().expect("lock").clock;
    let legacy = ExecutionReceipt {
        action: promoted("reschedule_work_order", &wo_target(), "2027-10-23 10:00:00").action,
        external_ref: format!(
            r#"{{"kind":"restore_field","model":"mrp.workorder","id":{WO_ID},"field":"date_start","prior":"2027-10-22 08:00:00"}}"#
        ),
        source_revision: Revision(watermark),
        reversible: true,
    };
    store
        .lock()
        .expect("lock")
        .external_edit(WO, WO_ID, "duration_expected", json!(90.0));
    let conflict = exec.reverse(&legacy).await;
    assert!(
        matches!(conflict, Err(EngineError::ReverseConflict { .. })),
        "{conflict:?}"
    );
    assert_eq!(window(&store), (json!(WO_START), json!(WO_FINISH)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_re_promise_reverse_checks_only_the_promise() {
    let store = flagship_store(false);
    let exec = executor(&store).await;
    let receipt = exec
        .execute(&promoted(
            "re_promise_date",
            "https://ekg.example/id/sale.order/1",
            "2027-12-03 00:00:00",
        ))
        .await
        .expect("re-promise");
    let prior = || get(&store, "sale.order", 1, "commitment_date");
    let Undo::RestoreField {
        written,
        prior: before,
        ..
    } = Undo::decode(&receipt.external_ref).expect("undo")
    else {
        panic!("a single-field restore");
    };
    assert_eq!(
        written,
        Some(json!("2027-12-03 00:00:00")),
        "the value is recorded"
    );

    // Another field of the order changes: no refusal.
    store
        .lock()
        .expect("lock")
        .external_edit("sale.order", 1, "note", json!("call the customer"));
    // The promise changes: refused, until it is put back.
    store.lock().expect("lock").external_edit(
        "sale.order",
        1,
        "commitment_date",
        json!("2027-12-07 00:00:00"),
    );
    assert!(matches!(
        exec.reverse(&receipt).await,
        Err(EngineError::ReverseConflict { .. })
    ));
    store.lock().expect("lock").external_edit(
        "sale.order",
        1,
        "commitment_date",
        json!("2027-12-03 00:00:00"),
    );
    exec.reverse(&receipt).await.expect("the value is back");
    assert_eq!(prior(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_start_only_reschedule_receipt_still_reverses() {
    // A stored receipt that recorded only the prior start (this wire shape), for a
    // start moved inside the window.
    let store = work_order_store(Some(WO_FINISH));
    let exec = executor(&store).await;
    store.lock().expect("lock").external_edit(
        WO,
        WO_ID,
        "date_start",
        json!("2027-10-23 10:00:00"),
    );
    let watermark = 1_799_280_000 + store.lock().expect("lock").clock;

    let legacy = ExecutionReceipt {
        action: promoted("reschedule_work_order", &wo_target(), "2027-10-23 10:00:00").action,
        external_ref: format!(
            r#"{{"kind":"restore_field","model":"mrp.workorder","id":{WO_ID},"field":"date_start","prior":"{WO_START}"}}"#
        ),
        source_revision: Revision(watermark),
        reversible: true,
    };
    exec.reverse(&legacy).await.expect("legacy reverse");
    assert_eq!(window(&store), (json!(WO_START), json!(WO_FINISH)));
}
