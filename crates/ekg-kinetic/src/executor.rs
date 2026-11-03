//! The real [`ActionExecutor`] driver: commits governed verbs back to Odoo via
//! the json2 client and reverses them.
//!
//! [`ActionExecutor::execute`] accepts a [`PromotedAction`] only: a governed
//! write is unrepresentable without a human promotion (the "AI never autonomously
//! commits" invariant, enforced by the type). Reverse is conditional: it refuses
//! rather than clobber a source change made after the write.
//!
//! The `odoo-client` is synchronous, so every call is bridged off the async seam
//! with `tokio::task::spawn_blocking`, mirroring `OdooIngestor`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_domain::{ActionExecutor, EngineError, ExecutionReceipt, PromotedAction, Result, Revision};
use odoo_client::Odoo;
use serde_json::{Map, Value, json};

use crate::verbs::{
    FieldRestore, Undo, Verb, Watermark, WorkOrderWindow, WriteKind, canonical_datetime,
    many2one_prior_id, odoo_datetime_to_epoch, parse_target, positive_number,
    resolve_work_center_id, same_value, shifted_finish, shifted_windows, validate_params,
};

/// The Odoo model of a receipt that can bring a purchase order's goods in.
const RECEIPT_MODEL: &str = "stock.picking";
/// The receipt field the risk rule reads as the arrival (`ekg:eta`).
const RECEIPT_DATE: &str = "scheduled_date";
/// The work-order field that ends its planned window (the reschedule moves it with
/// the start).
const WO_FINISH: &str = "date_finished";
/// The work-order field Odoo recomputes the planned finish from.
const WO_DURATION: &str = "duration_expected";
/// The manufacturing-order model a split targets.
const MO_MODEL: &str = "mrp.production";
const WO_MODEL: &str = "mrp.workorder";
/// The states Odoo lets a manufacturing order be split (and a split-off lot be removed) in.
const SPLITTABLE_STATES: [&str; 2] = ["draft", "confirmed"];

/// An [`ActionExecutor`] that writes back to Odoo via json2.
pub struct OdooActionExecutor {
    odoo: Arc<Odoo>,
}

impl OdooActionExecutor {
    /// Build the executor over an already-connected [`Odoo`] session.
    #[must_use]
    pub fn new(odoo: Arc<Odoo>) -> Self {
        Self { odoo }
    }

    /// Connect to Odoo (blocking probe on a runtime thread) and build the executor.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the connection/auth probe fails.
    pub async fn connect(base_url: &str, db: &str, login: &str, api_key: &str) -> Result<Self> {
        let (base_url, db, login, api_key) = (
            base_url.to_owned(),
            db.to_owned(),
            login.to_owned(),
            api_key.to_owned(),
        );
        let odoo = tokio::task::spawn_blocking(move || {
            Odoo::connect(&base_url, &db, &login, &api_key).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| EngineError::Backend(format!("odoo connect task join failed: {e}")))?
        .map_err(EngineError::Backend)?;
        Ok(Self::new(Arc::new(odoo)))
    }

    /// Run a blocking odoo-client call on a runtime thread, flattening the join and
    /// the client error into an [`EngineError::Backend`].
    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Odoo) -> std::result::Result<T, String> + Send + 'static,
        T: Send + 'static,
    {
        let odoo = Arc::clone(&self.odoo);
        tokio::task::spawn_blocking(move || f(&odoo))
            .await
            .map_err(|e| EngineError::Backend(format!("odoo task join failed: {e}")))?
            .map_err(EngineError::Backend)
    }

    /// Read `fields` of one record, each as Odoo renders it (JSON `false` when unset).
    async fn read_fields(
        &self,
        model: &str,
        id: i64,
        fields: &[&str],
    ) -> Result<BTreeMap<String, Value>> {
        let (m, names) = (model.to_owned(), json!(fields));
        let rows = self
            .blocking(move |o| o.read(&m, json!([id]), names).map_err(|e| e.to_string()))
            .await?;
        Ok(fields
            .iter()
            .map(|field| ((*field).to_owned(), first_field(&rows, field)))
            .collect())
    }

    /// The conditional-reverse guard: refuse unless every field in `written` still holds
    /// the value the write-back left. A receipt stored without values falls back to the
    /// record's `write_date`: refused when it advanced past `watermark`. The refusal
    /// names the record's current `write_date`.
    async fn still_as_written(
        &self,
        external_ref: &str,
        model: &str,
        id: i64,
        written: &BTreeMap<String, Value>,
        watermark: u64,
    ) -> Result<()> {
        let mut names: Vec<&str> = written.keys().map(String::as_str).collect();
        names.push("write_date");
        let current = self.read_fields(model, id, &names).await?;
        let found = current
            .get("write_date")
            .and_then(Value::as_str)
            .map_or(0, odoo_datetime_to_epoch);
        let changed = if written.is_empty() {
            found > watermark
        } else {
            written
                .iter()
                .any(|(field, value)| !current.get(field).is_some_and(|now| same_value(now, value)))
        };
        if changed {
            return Err(EngineError::ReverseConflict {
                external_ref: external_ref.to_owned(),
                executed: watermark,
                found,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl ActionExecutor for OdooActionExecutor {
    async fn execute(&self, promoted: &PromotedAction) -> Result<ExecutionReceipt> {
        let action = &promoted.action;
        let verb = Verb::parse(&action.action_type)?;
        validate_params(verb, &action.params)?;
        let (model, id) = parse_target(&action.target.0)?;
        if let Some(expected_model) = verb.target_model()
            && model != expected_model
        {
            return Err(EngineError::InvalidProposal(format!(
                "verb {verb:?} expects target model {expected_model:?}, got {model:?}"
            )));
        }
        let param = |key: &str| action.params.get(key).cloned().unwrap_or_default();

        // Each arm commits its write-back and returns the undo and the source watermark
        // a later reverse checks.
        let (undo, source_revision) = match verb {
            Verb::NotifyOwner => self.notify(&model, id, param("message"), promoted).await?,
            Verb::ExpeditePo => {
                let new_value = write_value(WriteKind::Text, &param("new_date"))?;
                self.expedite_line(id, "date_planned", new_value).await?
            }
            Verb::RescheduleWorkOrder => {
                let raw = param("new_date");
                let new_value = write_value(WriteKind::Text, &raw)?;
                self.reschedule_window(id, "date_start", &raw, new_value)
                    .await?
            }
            Verb::RePromiseDate | Verb::ReassignWorkCenter => {
                self.write_field(verb, &model, id, &action.params).await?
            }
            Verb::RaiseWorkCenterCapacity => {
                self.raise_capacity(id, &param("new_capacity")).await?
            }
            Verb::SplitProduction => {
                self.split_production(id, &param("split_quantity"), &param("new_date"))
                    .await?
            }
        };
        Ok(ExecutionReceipt {
            action: action.clone(),
            external_ref: undo.encode()?,
            source_revision,
            reversible: verb.reversible(),
        })
    }

    async fn reverse(&self, receipt: &ExecutionReceipt) -> Result<()> {
        if !receipt.reversible {
            return Err(EngineError::NotReversible(receipt.external_ref.clone()));
        }
        match Undo::decode(&receipt.external_ref)? {
            Undo::RestoreField {
                model,
                id,
                field,
                prior,
                written,
            } => {
                // Refuse if the written field changed after our write, so a later change
                // is not clobbered.
                let written: BTreeMap<String, Value> =
                    written.map(|v| (field.clone(), v)).into_iter().collect();
                self.still_as_written(
                    &receipt.external_ref,
                    &model,
                    id,
                    &written,
                    receipt.source_revision.0,
                )
                .await?;
                let m = model.clone();
                self.blocking(move |o| {
                    o.write(&m, json!([id]), one_field(&field, prior))
                        .map_err(|e| e.to_string())
                })
                .await
            }
            Undo::RestoreFields { restores } => {
                // Every guard is checked before anything is written, since restoring one
                // record can itself touch another (a line write bumps its receipt's
                // write_date).
                for restore in &restores {
                    let written: BTreeMap<String, Value> = restore
                        .written
                        .clone()
                        .map(|v| (restore.field.clone(), v))
                        .into_iter()
                        .collect();
                    self.still_as_written(
                        &receipt.external_ref,
                        &restore.model,
                        restore.id,
                        &written,
                        restore.watermark,
                    )
                    .await?;
                }
                // Each record's fields are restored in one write: a reschedule's
                // start and finish restored one at a time could pass through a
                // window Odoo refuses (start after finish).
                for (model, id, vals) in group_restores(restores) {
                    self.blocking(move |o| {
                        o.write(&model, json!([id]), vals)
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                }
                Ok(())
            }
            Undo::UnlinkRecord { model, id } => {
                let m = model.clone();
                self.blocking(move |o| o.unlink(&m, json!([id])).map_err(|e| e.to_string()))
                    .await
            }
            undo @ Undo::SplitProduction { .. } => {
                self.reverse_split(undo, &receipt.external_ref).await
            }
        }
    }
}

impl OdooActionExecutor {
    /// The notify write-back: post a message on the target; the inverse unlinks it. A
    /// created record has no prior state to guard, so the receipt carries the
    /// promotion's basis revision for audit.
    async fn notify(
        &self,
        model: &str,
        id: i64,
        body: String,
        promoted: &PromotedAction,
    ) -> Result<(Undo, Revision)> {
        let m = model.to_owned();
        let message_id = self
            .blocking(move |o| {
                o.create(
                    "mail.message",
                    json!({
                        "model": m,
                        "res_id": id,
                        "body": body,
                        "message_type": "comment",
                    }),
                )
                .map_err(|e| e.to_string())
            })
            .await?;
        let undo = Undo::UnlinkRecord {
            model: "mail.message".to_owned(),
            id: message_id,
        };
        Ok((undo, promoted.basis_revision))
    }

    /// A single-field write-back (re-promise, reassign): read the prior, write the new
    /// value, and record the restore guarded by the post-write watermark.
    async fn write_field(
        &self,
        verb: Verb,
        model: &str,
        id: i64,
        params: &std::collections::BTreeMap<String, String>,
    ) -> Result<(Undo, Revision)> {
        let (Some((_, field)), Some((param, kind))) =
            (verb.field_write(), verb.field_write_param())
        else {
            return Err(EngineError::InvalidProposal(format!(
                "verb {verb:?} is not a single-field write"
            )));
        };
        let raw = params.get(param).cloned().unwrap_or_default();
        let new_value = write_value(kind, &raw)?;

        // The prior value, for the inverse. A many-to-one is reduced from Odoo's
        // `[id, name]` render to its id.
        let m = model.to_owned();
        let prior = self
            .blocking(move |o| {
                let rows = o
                    .read(&m, json!([id]), json!([field]))
                    .map_err(|e| e.to_string())?;
                Ok(first_field(&rows, field))
            })
            .await?;
        let prior = match kind {
            WriteKind::Text | WriteKind::Number => prior,
            WriteKind::Many2OneId => many2one_prior_id(&prior),
        };

        let m = model.to_owned();
        self.blocking(move |o| {
            o.write(&m, json!([id]), one_field(field, new_value))
                .map_err(|e| e.to_string())
        })
        .await?;

        // The field as the write left it and the post-write write_date. Both have to be
        // read after our write for an intervening change to be detectable.
        let (source_revision, written) = self.as_written(model, id, field).await?;
        let undo = Undo::RestoreField {
            model: model.to_owned(),
            id,
            field: field.to_owned(),
            prior,
            written: Some(written),
        };
        Ok((undo, source_revision))
    }

    /// The restore of `field` to `prior`, guarded by the value the write left in it and the
    /// record's post-write `write_date`.
    async fn restore_as_written(
        &self,
        model: &str,
        id: i64,
        field: &str,
        prior: Value,
    ) -> Result<FieldRestore> {
        let (watermark, written) = self.as_written(model, id, field).await?;
        Ok(FieldRestore {
            model: model.to_owned(),
            id,
            field: field.to_owned(),
            prior,
            watermark: watermark.0,
            written: Some(written),
        })
    }

    /// A record's post-write `write_date` and the value `field` was left holding.
    async fn as_written(&self, model: &str, id: i64, field: &str) -> Result<(Revision, Value)> {
        let row = self.read_fields(model, id, &[field, "write_date"]).await?;
        let watermark = row
            .get("write_date")
            .and_then(Value::as_str)
            .map_or(0, odoo_datetime_to_epoch);
        let written = row.get(field).cloned().unwrap_or(Value::Bool(false));
        Ok((Revision(watermark), written))
    }

    /// The capacity-raise write-back: set a work-center capacity record's `capacity` to
    /// `new_capacity`, refusing a value that does not raise it. The inverse restores the
    /// prior capacity, guarded by the post-write watermark.
    ///
    /// # Errors
    /// [`EngineError::InvalidProposal`], before any write, when the record is missing
    /// or `new_capacity` is not a positive number greater than the current capacity.
    async fn raise_capacity(&self, id: i64, raw: &str) -> Result<(Undo, Revision)> {
        let model = "mrp.workcenter.capacity";
        let new_capacity = positive_number("new_capacity", raw)?;
        let row = self
            .blocking(move |o| {
                o.read(model, json!([id]), json!(["capacity"]))
                    .map_err(|e| e.to_string())
            })
            .await?;
        let prior = first_field(&row, "capacity");
        let Some(current) = prior.as_f64() else {
            return Err(EngineError::InvalidProposal(format!(
                "{model} {id} was not found or has no capacity"
            )));
        };
        if new_capacity <= current {
            return Err(EngineError::InvalidProposal(format!(
                "new_capacity {new_capacity} does not raise {model} {id}'s capacity {current}"
            )));
        }
        let value = write_value(WriteKind::Number, raw)?;
        self.blocking(move |o| {
            o.write(model, json!([id]), one_field("capacity", value))
                .map_err(|e| e.to_string())
        })
        .await?;
        let (source_revision, written) = self.as_written(model, id, "capacity").await?;
        let undo = Undo::RestoreField {
            model: model.to_owned(),
            id,
            field: "capacity".to_owned(),
            prior,
            written: Some(written),
        };
        Ok((undo, source_revision))
    }
}

impl OdooActionExecutor {
    /// The expedite write-back: move a purchase-order line's `date_planned` and, on
    /// every open incoming receipt of its purchase order that Odoo did not already
    /// move with it, the `scheduled_date` (the arrival the risk rule prefers while a
    /// receipt is open). Every prior is captured before any write and recorded in one
    /// [`Undo::RestoreFields`], each guarded by its record's post-write watermark.
    /// Returns the undo and the newest watermark.
    ///
    /// Odoo 19 does not move the receipt itself (a line write only moves its stock
    /// move's deadline), so the receipt write is what shifts the arrival; a receipt
    /// already at the new date is left alone and needs no restore.
    async fn expedite_line(
        &self,
        line_id: i64,
        field: &'static str,
        new_value: Value,
    ) -> Result<(Undo, Revision)> {
        let line_model = "purchase.order.line";

        // The line's prior date and its purchase order.
        let row = self
            .blocking(move |o| {
                o.read(line_model, json!([line_id]), json!([field, "order_id"]))
                    .map_err(|e| e.to_string())
            })
            .await?;
        let line_prior = first_field(&row, field);
        let Some(po_id) = many2one_prior_id(&first_field(&row, "order_id")).as_i64() else {
            return Err(EngineError::InvalidProposal(format!(
                "purchase.order.line {line_id} was not found or has no purchase order"
            )));
        };

        // The open incoming receipts delivering that order, with their priors, read
        // before the line write, which Odoo may propagate. The filter is the
        // contract's receipt scope plus the risk rule's open test.
        let receipts = self
            .blocking(move |o| {
                o.search_read(
                    RECEIPT_MODEL,
                    json!([
                        ["purchase_id", "=", po_id],
                        ["picking_type_code", "=", "incoming"],
                        ["location_dest_id.usage", "=", "internal"],
                        ["state", "not in", ["done", "cancel"]],
                    ]),
                    json!([RECEIPT_DATE]),
                )
                .map_err(|e| e.to_string())
            })
            .await?;
        let receipts: Vec<(i64, Value)> = receipts
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let id = r.get("id").and_then(Value::as_i64)?;
                        let prior = r.get(RECEIPT_DATE).cloned().unwrap_or(Value::Bool(false));
                        Some((id, prior))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Write the line, then read back Odoo's normalized value of it.
        self.blocking(move |o| {
            o.write(line_model, json!([line_id]), one_field(field, new_value))
                .map_err(|e| e.to_string())
        })
        .await?;
        let written = self
            .blocking(move |o| {
                let rows = o
                    .read(line_model, json!([line_id]), json!([field]))
                    .map_err(|e| e.to_string())?;
                Ok(first_field(&rows, field))
            })
            .await?;

        // Move each open receipt Odoo has not already moved to the same date.
        let mut moved = Vec::new();
        for (receipt_id, prior) in receipts {
            let current = self
                .blocking(move |o| {
                    let rows = o
                        .read(RECEIPT_MODEL, json!([receipt_id]), json!([RECEIPT_DATE]))
                        .map_err(|e| e.to_string())?;
                    Ok(first_field(&rows, RECEIPT_DATE))
                })
                .await?;
            if current == written {
                continue;
            }
            let date = written.clone();
            self.blocking(move |o| {
                o.write(
                    RECEIPT_MODEL,
                    json!([receipt_id]),
                    one_field(RECEIPT_DATE, date),
                )
                .map_err(|e| e.to_string())
            })
            .await?;
            moved.push((receipt_id, prior));
        }

        // Each record is read back after every write, so an intervening change to any
        // written field is detectable at reverse.
        let mut restores = vec![
            self.restore_as_written(line_model, line_id, field, line_prior)
                .await?,
        ];
        for (receipt_id, prior) in moved {
            restores.push(
                self.restore_as_written(RECEIPT_MODEL, receipt_id, RECEIPT_DATE, prior)
                    .await?,
            );
        }
        let newest = restores.iter().map(|r| r.watermark).max().unwrap_or(0);
        Ok((Undo::RestoreFields { restores }, Revision(newest)))
    }
}

impl OdooActionExecutor {
    /// The reschedule write-back: move a work order's planned window to
    /// start at `new_start`, shifting `date_finished` by the same delta so the
    /// planned duration is kept. Both go in one write, because Odoo refuses a start
    /// after the finish (a start-only write past the finish is a `UserError`). Both
    /// priors are recorded in one [`Undo::RestoreFields`], each guarded by the
    /// record's post-write watermark. With no finish (or no start) there is no window
    /// to keep, so only the start is written and restored. Returns the undo and the
    /// watermark.
    ///
    /// # Errors
    /// [`EngineError::InvalidProposal`], before any write, if the window cannot be
    /// shifted to the new start (a malformed new start or stored window).
    async fn reschedule_window(
        &self,
        wo_id: i64,
        field: &'static str,
        raw: &str,
        new_start: Value,
    ) -> Result<(Undo, Revision)> {
        let wo_model = "mrp.workorder";

        // Both priors and the duration, read before the write.
        let row = self
            .blocking(move |o| {
                o.read(
                    wo_model,
                    json!([wo_id]),
                    json!([field, WO_FINISH, WO_DURATION]),
                )
                .map_err(|e| e.to_string())
            })
            .await?;
        let (prior_start, prior_finish) = (first_field(&row, field), first_field(&row, WO_FINISH));
        let prior_duration = first_field(&row, WO_DURATION);

        // The new start, plus the shifted finish when a window is set.
        let mut vals = Map::new();
        vals.insert(field.to_owned(), new_start);
        let has_window = prior_start.is_string() && prior_finish.is_string();
        if let (Some(start), Some(finish)) = (prior_start.as_str(), prior_finish.as_str()) {
            let new_finish = shifted_finish(start, finish, raw).ok_or_else(|| {
                EngineError::InvalidProposal(format!(
                    "cannot move mrp.workorder {wo_id}'s window {start:?}–{finish:?} to start at {raw:?}"
                ))
            })?;
            vals.insert(WO_FINISH.to_owned(), Value::String(new_finish));
        }

        self.blocking(move |o| {
            o.write(wo_model, json!([wo_id]), Value::Object(vals))
                .map_err(|e| e.to_string())
        })
        .await?;

        // The window as the write left it and the post-write watermark guard every
        // restore of the step. The duration is included: Odoo recomputes a work order's
        // finish from it, so the restore writes it back with the window and an edit to
        // it refuses the reverse.
        let after = self
            .read_fields(
                wo_model,
                wo_id,
                &[field, WO_FINISH, WO_DURATION, "write_date"],
            )
            .await?;
        let watermark = after
            .get("write_date")
            .and_then(Value::as_str)
            .map_or(0, odoo_datetime_to_epoch);
        let restore = |field: &str, prior: Value| FieldRestore {
            model: wo_model.to_owned(),
            id: wo_id,
            field: field.to_owned(),
            prior,
            watermark,
            written: after.get(field).cloned(),
        };
        let mut restores = vec![restore(field, prior_start)];
        if has_window {
            restores.push(restore(WO_FINISH, prior_finish));
            restores.push(restore(WO_DURATION, prior_duration));
        }
        Ok((Undo::RestoreFields { restores }, Revision(watermark)))
    }
}

/// A manufacturing order as the split reads it.
struct SplitSubject {
    qty: f64,
    name: Value,
    state: String,
    start: Value,
    finish: Value,
    group: i64,
    work_orders: Vec<Value>,
}

/// What a split will write, worked out (and validated) before any write: the original's work
/// orders' prior windows, their operations, the split-off's shifted windows, and the lot's
/// dates.
struct SplitPlan {
    windows: Vec<WorkOrderWindow>,
    /// The operation of each planned work order, in the order of `shifted`.
    operations: Vec<Value>,
    shifted: Vec<(String, String)>,
    new_date: String,
    lot_finish: String,
}

impl SplitPlan {
    /// The plan for splitting `split` units of order `id` off to `new_date`.
    ///
    /// # Errors
    /// [`EngineError::InvalidProposal`] when the order is not in a splittable state, the
    /// quantity is not less than the order's, or a window cannot be shifted.
    fn of(id: i64, subject: &SplitSubject, split: f64, new_date: String) -> Result<Self> {
        if !SPLITTABLE_STATES.contains(&subject.state.as_str()) {
            return Err(EngineError::InvalidProposal(format!(
                "{MO_MODEL} {id} is {:?}; only a draft or confirmed order can be split",
                subject.state
            )));
        }
        if split >= subject.qty {
            return Err(EngineError::InvalidProposal(format!(
                "split_quantity {split} is not less than {MO_MODEL} {id}'s quantity {}",
                subject.qty
            )));
        }
        let windows: Vec<WorkOrderWindow> = subject
            .work_orders
            .iter()
            .filter_map(|wo| {
                Some(WorkOrderWindow {
                    id: wo["id"].as_i64()?,
                    date_start: wo["date_start"].clone(),
                    date_finished: wo["date_finished"].clone(),
                    duration_expected: wo["duration_expected"].clone(),
                })
            })
            .collect();
        let (operations, pairs): (Vec<Value>, Vec<(String, String)>) = subject
            .work_orders
            .iter()
            .filter_map(|wo| {
                Some((
                    many2one_prior_id(&wo["operation_id"]),
                    (
                        wo["date_start"].as_str()?.to_owned(),
                        wo["date_finished"].as_str()?.to_owned(),
                    ),
                ))
            })
            .unzip();
        let shifted = shifted_windows(&pairs, &new_date).ok_or_else(|| {
            EngineError::InvalidProposal(format!(
                "cannot shift {MO_MODEL} {id}'s work-order windows to {new_date:?}"
            ))
        })?;
        let (Some(prior_start), Some(prior_finish)) =
            (subject.start.as_str(), subject.finish.as_str())
        else {
            return Err(EngineError::InvalidProposal(format!(
                "{MO_MODEL} {id} has no planned window to split"
            )));
        };
        let lot_finish = shifted_finish(prior_start, prior_finish, &new_date).ok_or_else(|| {
            EngineError::InvalidProposal(format!(
                "cannot move {MO_MODEL} {id}'s window to start at {new_date:?}"
            ))
        })?;
        Ok(Self {
            windows,
            operations,
            shifted,
            new_date,
            lot_finish,
        })
    }
}

impl OdooActionExecutor {
    /// Read a manufacturing order for a split, with its work orders (`operation_id`,
    /// window and duration) in the order Odoo lists them.
    async fn split_subject(&self, id: i64) -> Result<SplitSubject> {
        let row = self
            .blocking(move |o| {
                o.read(
                    MO_MODEL,
                    json!([id]),
                    json!([
                        "product_qty",
                        "name",
                        "state",
                        "date_start",
                        "date_finished",
                        "production_group_id",
                        "workorder_ids"
                    ]),
                )
                .map_err(|e| e.to_string())
            })
            .await?;
        let (Some(qty), Some(group)) = (
            first_field(&row, "product_qty").as_f64(),
            many2one_prior_id(&first_field(&row, "production_group_id")).as_i64(),
        ) else {
            return Err(EngineError::InvalidProposal(format!(
                "{MO_MODEL} {id} was not found or has no production group"
            )));
        };
        let wo_ids = first_field(&row, "workorder_ids");
        let work_orders = self.work_orders(wo_ids).await?;
        Ok(SplitSubject {
            qty,
            name: first_field(&row, "name"),
            state: first_field(&row, "state")
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            start: first_field(&row, "date_start"),
            finish: first_field(&row, "date_finished"),
            group,
            work_orders,
        })
    }

    /// Read work orders' `operation_id`, window and duration, in the order of `ids`.
    async fn work_orders(&self, ids: Value) -> Result<Vec<Value>> {
        let order: Vec<i64> = ids
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if order.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .blocking(move |o| {
                o.read(
                    WO_MODEL,
                    ids,
                    json!([
                        "operation_id",
                        "date_start",
                        "date_finished",
                        "duration_expected"
                    ]),
                )
                .map_err(|e| e.to_string())
            })
            .await?;
        let rows = rows.as_array().cloned().unwrap_or_default();
        Ok(order
            .iter()
            .filter_map(|id| rows.iter().find(|r| r["id"].as_i64() == Some(*id)).cloned())
            .collect())
    }

    /// The ids of the manufacturing orders in production group `group`.
    async fn group_lots(&self, group: i64) -> Result<Vec<i64>> {
        self.blocking(move |o| {
            o.search(MO_MODEL, json!([["production_group_id", "=", group]]))
                .map_err(|e| e.to_string())
        })
        .await
    }

    /// Write a work order's window with its duration. Carrying `duration_expected` keeps
    /// the window as given; without it Odoo recomputes the finish over the work-center
    /// calendar (`writeback_it` pins this).
    async fn plan_work_order(
        &self,
        id: i64,
        start: Value,
        finish: Value,
        duration: Value,
    ) -> Result<()> {
        self.blocking(move |o| {
            o.write(
                WO_MODEL,
                json!([id]),
                json!({ "date_start": start, "date_finished": finish, "duration_expected": duration }),
            )
            .map_err(|e| e.to_string())
        })
        .await
    }

    /// Write a manufacturing order's dates under Odoo's `force_date` context, which keeps
    /// its work orders planned (a plain `date_start` write unplans them).
    async fn force_mo_dates(&self, id: i64, start: Value, finish: Value) -> Result<()> {
        self.blocking(move |o| {
            o.call_method(
                MO_MODEL,
                "write",
                json!({
                    "ids": [id],
                    "vals": { "date_start": start, "date_finished": finish },
                    "context": { "force_date": true },
                }),
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .await
    }

    /// The split write-back: split manufacturing order `id`
    /// through Odoo's `mrp.production.split` wizard into the original (reduced by
    /// `split_quantity`, keeping its schedule) and a split-off lot of `split_quantity`
    /// planned from `new_date`. Odoo clears both lots' work-order windows on a split, so
    /// both are re-planned: the original's back to its prior windows and dates, the
    /// split-off's shifted so the earliest starts at `new_date`. Returns the undo and the
    /// newest watermark.
    ///
    /// # Errors
    /// [`EngineError::InvalidProposal`], before any write, when the order is not in a
    /// splittable state, the quantity is not positive and less than the order's, or a date
    /// is malformed.
    async fn split_production(
        &self,
        id: i64,
        raw_qty: &str,
        raw_date: &str,
    ) -> Result<(Undo, Revision)> {
        let split = positive_number("split_quantity", raw_qty)?;
        let new_date = canonical_datetime(raw_date).ok_or_else(|| {
            EngineError::InvalidProposal(format!("new_date {raw_date:?} is not a date/time"))
        })?;
        let subject = self.split_subject(id).await?;
        let plan = SplitPlan::of(id, &subject, split, new_date)?;

        // The split, through Odoo's own wizard.
        let before = self.group_lots(subject.group).await?;
        let (keep, start, lot_date) = (
            subject.qty - split,
            subject.start.clone(),
            plan.new_date.clone(),
        );
        self.blocking(move |o| {
            let wizard = o
                .create(
                    "mrp.production.split",
                    json!({
                        "production_id": id,
                        "production_detailed_vals_ids": [
                            [0, 0, { "quantity": keep, "date": start }],
                            [0, 0, { "quantity": split, "date": lot_date }],
                        ],
                    }),
                )
                .map_err(|e| e.to_string())?;
            o.call_method(
                "mrp.production.split",
                "action_split",
                json!({ "ids": [wizard] }),
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .await?;
        // The split-off lot is the one new order in the group.
        let created: Vec<i64> = self
            .group_lots(subject.group)
            .await?
            .into_iter()
            .filter(|lot| !before.contains(lot))
            .collect();
        let [split_off] = created[..] else {
            return Err(EngineError::Backend(format!(
                "the split of {MO_MODEL} {id} created {} lot(s), expected one",
                created.len()
            )));
        };
        // Re-plan both lots, then read the watermarks after every write.
        self.replan_split(id, split_off, &subject, &plan).await?;
        let watermarks = self.split_watermarks(id, split_off, &plan.windows).await?;
        let newest = watermarks.iter().map(|w| w.write_date).max().unwrap_or(0);
        let undo = Undo::SplitProduction {
            production: id,
            split_off,
            prior_qty: json!(subject.qty),
            prior_name: subject.name,
            prior_start: subject.start,
            prior_finish: subject.finish,
            windows: plan.windows,
            watermarks,
        };
        Ok((undo, Revision(newest)))
    }

    /// Re-plan both lots after a split: Odoo cleared every work-order window of both. The
    /// original's work orders get their prior windows back; the split-off's (matched to the
    /// original's by operation, else by position) get them shifted to the new date. Work
    /// orders first (each write can move its order's dates), each carrying its current
    /// (rescaled) duration, then both orders' dates under `force_date`.
    async fn replan_split(
        &self,
        id: i64,
        split_off: i64,
        subject: &SplitSubject,
        plan: &SplitPlan,
    ) -> Result<()> {
        let replanned = self.split_subject(id).await?;
        for window in &plan.windows {
            let current = replanned
                .work_orders
                .iter()
                .find(|wo| wo["id"].as_i64() == Some(window.id));
            if let (true, Some(current)) = (
                window.date_start.is_string() && window.date_finished.is_string(),
                current,
            ) {
                self.plan_work_order(
                    window.id,
                    window.date_start.clone(),
                    window.date_finished.clone(),
                    current["duration_expected"].clone(),
                )
                .await?;
            }
        }
        let lot = self.split_subject(split_off).await?;
        for (index, wo) in lot.work_orders.iter().enumerate() {
            let op = many2one_prior_id(&wo["operation_id"]);
            let source = plan
                .operations
                .iter()
                .position(|source_op| op.is_i64() && *source_op == op)
                .or_else(|| (index < plan.shifted.len()).then_some(index));
            let (Some(source), Some(wo_id)) = (source, wo["id"].as_i64()) else {
                continue;
            };
            let (start, finish) = &plan.shifted[source];
            self.plan_work_order(
                wo_id,
                json!(start),
                json!(finish),
                wo["duration_expected"].clone(),
            )
            .await?;
        }
        self.force_mo_dates(id, subject.start.clone(), subject.finish.clone())
            .await?;
        self.force_mo_dates(split_off, json!(plan.new_date), json!(plan.lot_finish))
            .await
    }

    /// The post-write state of every record a split's reverse restores, i.e. its `write_date`
    /// and the fields the split set on it: the original's quantity, reference and dates,
    /// the split-off lot's quantity and dates, and the original's work-order windows.
    async fn split_watermarks(
        &self,
        id: i64,
        split_off: i64,
        windows: &[WorkOrderWindow],
    ) -> Result<Vec<Watermark>> {
        const ORIGINAL: &[&str] = &["product_qty", "name", "date_start", "date_finished"];
        const LOT: &[&str] = &["product_qty", "date_start", "date_finished"];
        const WINDOW: &[&str] = &["date_start", "date_finished"];
        let mut records = vec![(MO_MODEL, id, ORIGINAL), (MO_MODEL, split_off, LOT)];
        records.extend(windows.iter().map(|window| (WO_MODEL, window.id, WINDOW)));
        let mut watermarks = Vec::with_capacity(records.len());
        for (model, record, guarded) in records {
            let mut names = guarded.to_vec();
            names.push("write_date");
            let mut fields = self.read_fields(model, record, &names).await?;
            let write_date = fields
                .remove("write_date")
                .and_then(|v| v.as_str().map(odoo_datetime_to_epoch))
                .unwrap_or(0);
            watermarks.push(Watermark {
                model: model.to_owned(),
                id: record,
                write_date,
                fields,
            });
        }
        Ok(watermarks)
    }

    /// Reverse a split in one step: refuse if any record advanced past the write or the
    /// split-off lot left draft/confirmed; then cancel and unlink the split-off lot,
    /// restore the original's quantity and reference, and re-plan its work orders and
    /// dates as they were before the split.
    async fn reverse_split(&self, undo: Undo, external_ref: &str) -> Result<()> {
        let Undo::SplitProduction {
            production,
            split_off,
            prior_qty,
            prior_name,
            prior_start,
            prior_finish,
            windows,
            watermarks,
        } = undo
        else {
            return Err(EngineError::Backend(
                "reverse_split called with another undo".to_owned(),
            ));
        };
        // Every record's guard is checked before any write.
        for mark in &watermarks {
            self.still_as_written(
                external_ref,
                &mark.model,
                mark.id,
                &mark.fields,
                mark.write_date,
            )
            .await?;
        }
        let state = self
            .blocking(move |o| {
                o.read(MO_MODEL, json!([split_off]), json!(["state"]))
                    .map_err(|e| e.to_string())
            })
            .await?;
        let state = first_field(&state, "state");
        if !state
            .as_str()
            .is_some_and(|s| SPLITTABLE_STATES.contains(&s))
        {
            return Err(EngineError::NotReversible(format!(
                "the split-off {MO_MODEL} {split_off} is {state}; only a draft or confirmed \
                 lot can be removed"
            )));
        }
        // Remove the split-off lot.
        self.blocking(move |o| {
            o.call_method(MO_MODEL, "action_cancel", json!({ "ids": [split_off] }))
                .map_err(|e| e.to_string())?;
            o.unlink(MO_MODEL, json!([split_off]))
                .map_err(|e| e.to_string())
        })
        .await?;
        // The original's quantity and reference.
        self.blocking(move |o| {
            let wizard = o
                .create(
                    "change.production.qty",
                    json!({ "mo_id": production, "product_qty": prior_qty }),
                )
                .map_err(|e| e.to_string())?;
            o.call_method(
                "change.production.qty",
                "change_prod_qty",
                json!({ "ids": [wizard] }),
            )
            .map_err(|e| e.to_string())?;
            o.write(MO_MODEL, json!([production]), json!({ "name": prior_name }))
                .map_err(|e| e.to_string())
        })
        .await?;
        // The original's schedule, as the write-back re-planned it.
        for window in windows {
            if window.date_start.is_string() && window.date_finished.is_string() {
                self.plan_work_order(
                    window.id,
                    window.date_start,
                    window.date_finished,
                    window.duration_expected,
                )
                .await?;
            }
        }
        self.force_mo_dates(production, prior_start, prior_finish)
            .await
    }
}

/// Group a [`Undo::RestoreFields`] step's restores into one `(model, id, vals)` write
/// payload per record, records in first-seen order (the order the write-back
/// applied them).
fn group_restores(restores: Vec<FieldRestore>) -> Vec<(String, i64, Value)> {
    let mut groups: Vec<(String, i64, Map<String, Value>)> = Vec::new();
    for FieldRestore {
        model,
        id,
        field,
        prior,
        ..
    } in restores
    {
        if let Some((_, _, vals)) = groups.iter_mut().find(|(m, i, _)| *m == model && *i == id) {
            vals.insert(field, prior);
        } else {
            let mut vals = Map::new();
            vals.insert(field, prior);
            groups.push((model, id, vals));
        }
    }
    groups
        .into_iter()
        .map(|(model, id, vals)| (model, id, Value::Object(vals)))
        .collect()
}

/// The first record's `field` from a json2 `read` result, or JSON `false` when
/// absent/unset (Odoo renders an unset field as `false`).
fn first_field(rows: &Value, field: &str) -> Value {
    rows.as_array()
        .and_then(|a| a.first())
        .and_then(|row| row.get(field))
        .cloned()
        .unwrap_or(Value::Bool(false))
}

/// A single-field write payload `{ field: value }` for `odoo.write`.
fn one_field(field: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(field.to_owned(), value);
    Value::Object(map)
}

/// Encode a field-write verb's new value for the json2 write: a date/text field
/// is a JSON string written verbatim; a many-to-one id field is the resolved
/// integer id (Odoo needs an integer, not a string, for a many2one); a numeric
/// field is a JSON number.
///
/// # Errors
/// [`EngineError::InvalidProposal`] if a many-to-one value cannot be resolved to
/// an id (delegated to [`resolve_work_center_id`]).
fn write_value(kind: WriteKind, raw: &str) -> Result<Value> {
    match kind {
        WriteKind::Text => Ok(Value::String(raw.to_owned())),
        WriteKind::Many2OneId => Ok(Value::from(resolve_work_center_id(raw)?)),
        WriteKind::Number => Ok(Value::from(positive_number("value", raw)?)),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn one_field_builds_a_single_key_object() {
        let payload = one_field("commitment_date", Value::String("2027-10-07".to_owned()));
        assert_eq!(payload, json!({ "commitment_date": "2027-10-07" }));
    }

    fn restore(model: &str, id: i64, field: &str, prior: &str) -> FieldRestore {
        FieldRestore {
            model: model.to_owned(),
            id,
            field: field.to_owned(),
            prior: json!(prior),
            watermark: 1,
            written: None,
        }
    }

    #[test]
    fn restores_group_into_one_write_per_record_in_first_seen_order() {
        let grouped = group_restores(vec![
            restore("mrp.workorder", 3, "date_start", "2027-10-23 08:00:00"),
            restore("stock.picking", 9, "scheduled_date", "2027-11-15 08:00:00"),
            restore("mrp.workorder", 3, "date_finished", "2027-10-23 12:00:00"),
            restore("stock.picking", 8, "scheduled_date", "2027-11-16 08:00:00"),
        ]);
        assert_eq!(
            grouped,
            vec![
                (
                    "mrp.workorder".to_owned(),
                    3,
                    json!({
                        "date_start": "2027-10-23 08:00:00",
                        "date_finished": "2027-10-23 12:00:00",
                    })
                ),
                (
                    "stock.picking".to_owned(),
                    9,
                    json!({ "scheduled_date": "2027-11-15 08:00:00" })
                ),
                (
                    "stock.picking".to_owned(),
                    8,
                    json!({ "scheduled_date": "2027-11-16 08:00:00" })
                ),
            ]
        );
        // The same id on another model is another record.
        assert_eq!(
            group_restores(vec![
                restore("purchase.order.line", 3, "date_planned", "a"),
                restore("stock.picking", 3, "scheduled_date", "b"),
            ])
            .len(),
            2
        );
    }

    #[test]
    fn first_field_defaults_to_false_when_absent() {
        let rows = json!([{ "id": 1 }]);
        assert_eq!(first_field(&rows, "commitment_date"), Value::Bool(false));
        let rows = json!([{ "commitment_date": "2027-10-07" }]);
        assert_eq!(first_field(&rows, "commitment_date"), json!("2027-10-07"));
    }

    #[test]
    fn write_value_maps_text_to_string_and_many2one_to_integer() {
        // A date/text field is written verbatim as a JSON string.
        assert_eq!(
            write_value(WriteKind::Text, "2027-10-07 00:00:00").expect("text"),
            json!("2027-10-07 00:00:00")
        );
        // A many-to-one id field is written as a JSON integer, from a bare id...
        assert_eq!(
            write_value(WriteKind::Many2OneId, "5").expect("bare id"),
            json!(5)
        );
        // ...or an ontology work-center IRI.
        assert_eq!(
            write_value(
                WriteKind::Many2OneId,
                "https://ekg.example/id/mrp.workcenter/9"
            )
            .expect("iri"),
            json!(9)
        );
        // A malformed many-to-one value is rejected before any write.
        assert!(write_value(WriteKind::Many2OneId, "not-a-center").is_err());
    }
}
