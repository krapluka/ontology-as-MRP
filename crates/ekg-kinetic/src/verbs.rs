//! The concrete governed verbs for the supply-risk capability, plus the pure
//! helpers the executor is built from.
//!
//! The neutral core carries verbs opaquely as `ekg_domain::ActionType`; the
//! concrete set (required params, target model/field, inverse, reversibility)
//! lives here. Everything in this module is pure and unit-tested; the Odoo I/O
//! lives in [`crate::executor`].

use std::collections::BTreeMap;

use ekg_domain::{ActionType, EngineError, Result};
use serde::{Deserialize, Serialize};

/// The ontology id-IRI namespace (`.../id/<model>/<id>`). Kept local so the kinetic
/// layer need not depend on `ekg-semantic` just to parse a target.
const ID_NAMESPACE: &str = "https://ekg.example/id/";

/// A governed verb the executor knows how to commit and reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Re-promise a sale order's delivery date (`sale.order.commitment_date`).
    RePromiseDate,
    /// Expedite an inbound purchase-order line by moving its effective arrival:
    /// the line's `date_planned` and, while an open receipt is delivering its
    /// purchase order, that receipt's `scheduled_date` (the date the risk rule
    /// prefers). One governed action, reversed in one step.
    ExpeditePo,
    /// Notify the owner by posting a message on the target record.
    NotifyOwner,
    /// Reschedule a work order by moving its planned window: the new planned start
    /// (`mrp.workorder.date_start`) and the finish shifted by the same delta
    /// (`date_finished`), written together in one write because Odoo refuses a start
    /// after the finish. Odoo recomputes the finish from the expected duration and the
    /// work centre's calendar. The inverse restores both
    /// in one step. An unset finish leaves just the start to move.
    RescheduleWorkOrder,
    /// Reassign a work order to another work center
    /// (`mrp.workorder.workcenter_id`); the inverse restores the prior center.
    ReassignWorkCenter,
    /// Split a manufacturing order (`mrp.production`) through Odoo's split wizard into the
    /// original lot, reduced by `split_quantity` and keeping its schedule, and a split-off
    /// lot of `split_quantity` for the same sales order planned from `new_date`. The
    /// inverse restores the original's quantity, reference and schedule and removes the
    /// split-off lot, in one step.
    SplitProduction,
    /// Raise a work center's capacity: set a capacity record's
    /// (`mrp.workcenter.capacity.capacity`) value to `new_capacity`, which must exceed the
    /// current one. Odoo capacity is not dated, so the raise applies until reversed; the
    /// inverse restores the prior capacity.
    RaiseWorkCenterCapacity,
}

/// How a field-write verb encodes its new value for the json2 write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// A date/text field written as a JSON string from the param verbatim.
    Text,
    /// A many-to-one id field written as a JSON integer resolved from the param.
    Many2OneId,
    /// A numeric field written as a JSON number parsed from the param (finite, > 0).
    Number,
}

impl Verb {
    /// Every governed verb. A proposer derives its catalog from this list.
    pub const ALL: [Self; 7] = [
        Self::RePromiseDate,
        Self::ExpeditePo,
        Self::NotifyOwner,
        Self::RescheduleWorkOrder,
        Self::ReassignWorkCenter,
        Self::SplitProduction,
        Self::RaiseWorkCenterCapacity,
    ];

    /// The verb's wire name (the neutral [`ActionType`] string).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::RePromiseDate => "re_promise_date",
            Self::ExpeditePo => "expedite_po",
            Self::NotifyOwner => "notify_owner",
            Self::RescheduleWorkOrder => "reschedule_work_order",
            Self::ReassignWorkCenter => "reassign_work_center",
            Self::SplitProduction => "split_production",
            Self::RaiseWorkCenterCapacity => "raise_work_center_capacity",
        }
    }

    /// Parse a neutral [`ActionType`] into a known verb.
    ///
    /// # Errors
    /// [`EngineError::InvalidProposal`] if the verb is not in the governed catalog:
    /// the executor never widens its own authority to an unknown action.
    pub fn parse(action_type: &ActionType) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|verb| verb.name() == action_type.0)
            .ok_or_else(|| {
                EngineError::InvalidProposal(format!("unknown governed verb {:?}", action_type.0))
            })
    }

    /// The params this verb requires to be present and non-empty.
    #[must_use]
    pub fn required_params(self) -> &'static [&'static str] {
        match self {
            Self::RePromiseDate | Self::ExpeditePo | Self::RescheduleWorkOrder => &["new_date"],
            Self::ReassignWorkCenter => &["new_work_center"],
            Self::NotifyOwner => &["message"],
            Self::SplitProduction => &["split_quantity", "new_date"],
            Self::RaiseWorkCenterCapacity => &["new_capacity"],
        }
    }

    /// The Odoo model the verb's target must be, or `None` for a verb that may target
    /// any grounded record (notify). Every verb that writes its target names its model
    /// here, including one that is not a single field write (the split), so validation
    /// never infers the model from [`Self::field_write`].
    #[must_use]
    pub fn target_model(self) -> Option<&'static str> {
        match self {
            Self::RePromiseDate => Some("sale.order"),
            Self::ExpeditePo => Some("purchase.order.line"),
            Self::RescheduleWorkOrder | Self::ReassignWorkCenter => Some("mrp.workorder"),
            Self::SplitProduction => Some("mrp.production"),
            Self::RaiseWorkCenterCapacity => Some("mrp.workcenter.capacity"),
            Self::NotifyOwner => None,
        }
    }

    /// Whether the verb's write can be undone by an inverse. All are: the field
    /// writes restore the prior value; a notify posts a message the inverse
    /// unlinks.
    #[must_use]
    pub fn reversible(self) -> bool {
        true
    }

    /// For a field-write verb, the Odoo `(model, field)` it targets. This is the primary
    /// field, the one its param sets; [`Self::target_fields`] lists every field of the
    /// target record the write-back sets. `None` for a verb that is not a field write:
    /// one that creates a record (notify) or drives a source workflow (the split).
    #[must_use]
    pub fn field_write(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::RePromiseDate => Some(("sale.order", "commitment_date")),
            Self::ExpeditePo => Some(("purchase.order.line", "date_planned")),
            Self::RescheduleWorkOrder => Some(("mrp.workorder", "date_start")),
            Self::ReassignWorkCenter => Some(("mrp.workorder", "workcenter_id")),
            Self::RaiseWorkCenterCapacity => Some(("mrp.workcenter.capacity", "capacity")),
            Self::NotifyOwner | Self::SplitProduction => None,
        }
    }

    /// Every field of the target record the write-back sets, primary field first
    /// (the model is [`Self::field_write`]'s). A reschedule moves the whole planned
    /// window; the expedite's receipt writes are other records, so they are not
    /// listed; a notify sets no field of its target (it creates a message), and a split
    /// is not a field write either (it drives Odoo's split wizard; its projected facts
    /// are the simulator's own split diff).
    #[must_use]
    pub fn target_fields(self) -> &'static [&'static str] {
        match self {
            Self::RePromiseDate => &["commitment_date"],
            Self::ExpeditePo => &["date_planned"],
            Self::RescheduleWorkOrder => &["date_start", "date_finished"],
            Self::ReassignWorkCenter => &["workcenter_id"],
            Self::RaiseWorkCenterCapacity => &["capacity"],
            Self::NotifyOwner | Self::SplitProduction => &[],
        }
    }

    /// For a field-write verb, the param carrying the new value and how it is
    /// encoded for the json2 write ([`WriteKind`]). `None` for a non-field-write
    /// verb (notify, the split). Returns `Some` iff [`Self::field_write`] does.
    #[must_use]
    pub fn field_write_param(self) -> Option<(&'static str, WriteKind)> {
        match self {
            Self::RePromiseDate | Self::ExpeditePo | Self::RescheduleWorkOrder => {
                Some(("new_date", WriteKind::Text))
            }
            Self::ReassignWorkCenter => Some(("new_work_center", WriteKind::Many2OneId)),
            Self::RaiseWorkCenterCapacity => Some(("new_capacity", WriteKind::Number)),
            Self::NotifyOwner | Self::SplitProduction => None,
        }
    }
}

/// Parse a positive, finite decimal param (a split quantity, a new capacity).
///
/// # Errors
/// [`EngineError::InvalidProposal`] naming `param` when `value` is not a finite number
/// greater than zero.
pub fn positive_number(param: &str, value: &str) -> Result<f64> {
    match value.trim().parse::<f64>() {
        Ok(number) if number.is_finite() && number > 0.0 => Ok(number),
        _ => Err(EngineError::InvalidProposal(format!(
            "{param} {value:?} is not a positive number"
        ))),
    }
}

/// Validate that every required param for `verb` is present and non-empty.
///
/// # Errors
/// [`EngineError::InvalidProposal`] naming the first missing/empty param, so an
/// incomplete action is rejected before any write (never a partial commit).
pub fn validate_params(verb: Verb, params: &BTreeMap<String, String>) -> Result<()> {
    for key in verb.required_params() {
        match params.get(*key) {
            Some(value) if !value.trim().is_empty() => {}
            _ => {
                return Err(EngineError::InvalidProposal(format!(
                    "action {verb:?} is missing required param {key:?}"
                )));
            }
        }
    }
    Ok(())
}

/// Validate a proposed action against the governed catalog before it can reach a
/// human: the verb is known, every required param is present and well-formed
/// (a `new_date` is a canonical [`canonical_datetime`], a `new_work_center` resolves to an
/// id, a `split_quantity` or `new_capacity` is a positive number), and the target is of
/// the verb's [`Verb::target_model`]. These are the checks the executor makes at
/// write-back, made at attach time, so a defect never surfaces only after a promotion.
///
/// # Errors
/// [`EngineError::InvalidProposal`] naming the first defect.
pub fn validate_action(
    action_type: &ActionType,
    target: &str,
    params: &BTreeMap<String, String>,
) -> Result<Verb> {
    let verb = Verb::parse(action_type)?;
    validate_params(verb, params)?;
    if let Some(value) = params
        .get("new_date")
        .filter(|_| verb.required_params().contains(&"new_date"))
        && canonical_datetime(value).as_deref() != Some(value.as_str())
    {
        return Err(EngineError::InvalidProposal(format!(
            "action {} has a malformed new_date {value:?} (expected YYYY-MM-DD HH:MM:SS)",
            verb.name()
        )));
    }
    if let Some(value) = params
        .get("new_work_center")
        .filter(|_| verb.required_params().contains(&"new_work_center"))
    {
        resolve_work_center_id(value)?;
    }
    for param in ["split_quantity", "new_capacity"] {
        if let Some(value) = params
            .get(param)
            .filter(|_| verb.required_params().contains(&param))
        {
            positive_number(param, value)?;
        }
    }
    let (model, _) = parse_target(target)?;
    if let Some(expected) = verb.target_model()
        && model != expected
    {
        return Err(EngineError::InvalidProposal(format!(
            "action {} expects a {expected} target, got {target:?}",
            verb.name()
        )));
    }
    Ok(verb)
}

/// The canonical Odoo datetime (`YYYY-MM-DD HH:MM:SS`) for a date or datetime value, or
/// `None` if it is not a real calendar date/time. Accepts a bare date (midnight), and a
/// `T` or space separator.
#[must_use]
pub fn canonical_datetime(value: &str) -> Option<String> {
    let value = value.trim();
    let full = match value.len() {
        10 => format!("{value} 00:00:00"),
        19 => value.replacen('T', " ", 1),
        _ => return None,
    };
    let b = full.as_bytes();
    let shaped = b.iter().enumerate().all(|(i, &c)| match i {
        4 | 7 => c == b'-',
        10 => c == b' ',
        13 | 16 => c == b':',
        _ => c.is_ascii_digit(),
    });
    if !shaped {
        return None;
    }
    let n = |a: usize, z: usize| full[a..z].parse::<u32>().ok();
    let (year, month, day) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hour, min, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    ((1..=max_day).contains(&day) && hour <= 23 && min <= 59 && sec <= 59).then_some(full)
}

/// Parse a target `EntityUri` in the ontology scheme into `(model, id)`.
///
/// # Errors
/// [`EngineError::InvalidProposal`] if `uri` is not an ontology id IRI, lacks a
/// `<model>/<id>` tail, or has an empty model / non-numeric id.
pub fn parse_target(uri: &str) -> Result<(String, i64)> {
    let rest = uri.strip_prefix(ID_NAMESPACE).ok_or_else(|| {
        EngineError::InvalidProposal(format!("target {uri:?} is not an ontology id IRI"))
    })?;
    let (model, id) = rest.rsplit_once('/').ok_or_else(|| {
        EngineError::InvalidProposal(format!("target {uri:?} has no <model>/<id>"))
    })?;
    if model.is_empty() {
        return Err(EngineError::InvalidProposal(format!(
            "target {uri:?} has an empty model"
        )));
    }
    let id: i64 = id.parse().map_err(|_| {
        EngineError::InvalidProposal(format!("target {uri:?} has a non-numeric id"))
    })?;
    Ok((model.to_owned(), id))
}

/// Resolve a `new_work_center` param to an Odoo work-center id.
///
/// Accepts a bare integer id (`"5"`) or an ontology id IRI
/// (`.../id/mrp.workcenter/5`, its trailing id parsed via [`parse_target`]).
///
/// # Errors
/// [`EngineError::InvalidProposal`] if the value is neither a bare id nor a
/// well-formed `mrp.workcenter` IRI: the executor never guesses a target center.
pub fn resolve_work_center_id(value: &str) -> Result<i64> {
    let value = value.trim();
    if let Ok(id) = value.parse::<i64>() {
        return Ok(id);
    }
    let (model, id) = parse_target(value)?;
    if model != "mrp.workcenter" {
        return Err(EngineError::InvalidProposal(format!(
            "new_work_center {value:?} is not an mrp.workcenter id or IRI"
        )));
    }
    Ok(id)
}

/// Reduce an Odoo many-to-one read value to the id stored for the inverse: Odoo
/// renders a many-to-one as `[id, display_name]` (or `false` when unset), so an
/// `[id, name]` pair or a bare id becomes the integer id, and anything else (unset)
/// becomes JSON `false`.
#[must_use]
pub fn many2one_prior_id(value: &serde_json::Value) -> serde_json::Value {
    if let Some(id) = value.as_i64() {
        return serde_json::Value::from(id);
    }
    if let Some(id) = value
        .as_array()
        .and_then(|a| a.first())
        .and_then(serde_json::Value::as_i64)
    {
        return serde_json::Value::from(id);
    }
    serde_json::Value::Bool(false)
}

/// The undo payload encoded in [`ekg_domain::ExecutionReceipt::external_ref`], so a
/// reverse is self-contained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Undo {
    /// Restore a field to its prior value, refused when the field does not hold the
    /// value the write left (or, for a receipt stored without it, when the record's
    /// watermark advanced).
    RestoreField {
        /// The Odoo model written.
        model: String,
        /// The record id.
        id: i64,
        /// The field written.
        field: String,
        /// The value before the write (may be JSON `false`/null when it was unset).
        prior: serde_json::Value,
        /// The field's value right after the write, read back from the source: what the
        /// reverse checks is still there. `None` on a receipt stored without it, which
        /// keeps the `write_date` guard.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        written: Option<serde_json::Value>,
    },
    /// Restore several fields in one inverse step (the expedite moves a line and
    /// its open receipts; the reschedule a work order's start and finish). Each
    /// restore carries its own guard: the reverse checks every one before writing
    /// any, and refuses the whole step if any written field changed since the write.
    /// A record's fields are restored in one write.
    RestoreFields {
        /// The restores, in the order the write-back applied them.
        restores: Vec<FieldRestore>,
    },
    /// Unlink a record the verb created (e.g. the notify message).
    UnlinkRecord {
        /// The Odoo model of the created record.
        model: String,
        /// The created record id.
        id: i64,
    },
    /// Undo a split: cancel and unlink the split-off lot, restore the original's quantity
    /// (through `change.production.qty`) and reference, and re-plan its work orders and
    /// dates as they were. Every record's guard is checked before anything is written,
    /// and the whole step is refused if any field the split set changed since, or if
    /// the split-off lot has left draft/confirmed.
    SplitProduction {
        /// The original manufacturing order (it keeps its id).
        production: i64,
        /// The split-off lot the split created.
        split_off: i64,
        /// The original's quantity before the split.
        prior_qty: serde_json::Value,
        /// The original's reference before the split (Odoo appends `-001` to it).
        prior_name: serde_json::Value,
        /// The original's planned start before the split.
        prior_start: serde_json::Value,
        /// The original's planned finish before the split.
        prior_finish: serde_json::Value,
        /// The original's work-order windows before the split, re-planned on reverse.
        windows: Vec<WorkOrderWindow>,
        /// The post-write `write_date` of every record the reverse touches.
        watermarks: Vec<Watermark>,
    },
}

/// A work order's planned window and expected duration, as recorded for a re-plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkOrderWindow {
    /// The work order id.
    pub id: i64,
    /// Its planned start (JSON `false` when unset).
    pub date_start: serde_json::Value,
    /// Its planned finish (JSON `false` when unset).
    pub date_finished: serde_json::Value,
    /// Its expected duration in minutes.
    pub duration_expected: serde_json::Value,
}

/// A record's state captured after a write-back: its `write_date` (Unix seconds) and the
/// values of the fields the write-back set on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    /// The Odoo model.
    pub model: String,
    /// The record id.
    pub id: i64,
    /// The record's `write_date` after the write-back.
    pub write_date: u64,
    /// The values of the fields the write-back set (and the reverse restores), read back
    /// after it: what the reverse checks is still there. Empty on a receipt stored
    /// without them, which keeps the `write_date` guard.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, serde_json::Value>,
}

/// One watermark-guarded field restore inside [`Undo::RestoreFields`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRestore {
    /// The Odoo model written.
    pub model: String,
    /// The record id.
    pub id: i64,
    /// The field written.
    pub field: String,
    /// The value before the write (may be JSON `false`/null when it was unset).
    pub prior: serde_json::Value,
    /// The record's `write_date` (Unix seconds) captured after the write-back.
    pub watermark: u64,
    /// The field's value right after the write-back, read back from the source. `None`
    /// on a receipt stored without it, which keeps the `write_date` guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<serde_json::Value>,
}

/// Whether a field read back from the source still holds `written`: equal JSON, a
/// many-to-one (`[id, name]`) compared by its id (a renamed target is the same value), and
/// numbers compared numerically (`2` and `2.0` are the same capacity).
#[must_use]
pub fn same_value(current: &serde_json::Value, written: &serde_json::Value) -> bool {
    if current == written {
        return true;
    }
    if current.is_array() || written.is_array() {
        let (a, b) = (many2one_prior_id(current), many2one_prior_id(written));
        return a.is_i64() && a == b;
    }
    match (current.as_f64(), written.as_f64()) {
        (Some(a), Some(b)) => (a - b).abs() < f64::EPSILON,
        _ => false,
    }
}

impl Undo {
    /// Encode for the receipt's `external_ref`.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if serialization fails.
    pub fn encode(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| EngineError::Backend(e.to_string()))
    }

    /// Decode from a receipt's `external_ref`.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the ref is not a valid undo payload (a
    /// malformed/legacy receipt fails loudly rather than mis-reversing).
    pub fn decode(s: &str) -> Result<Self> {
        serde_json::from_str(s)
            .map_err(|e| EngineError::Backend(format!("undecodable execution receipt {s:?}: {e}")))
    }
}

/// Parse an Odoo `write_date` (`YYYY-MM-DD HH:MM:SS`, UTC) into Unix seconds, the
/// monotonic source watermark used for conditional reverse. A missing/unparseable
/// value yields 0 ("unknown", never blocking a reverse).
#[must_use]
pub fn odoo_datetime_to_epoch(dt: &str) -> u64 {
    let parts: Vec<i64> = dt
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let [year, month, day, hour, min, sec] = parts[..] else {
        return 0;
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec;
    u64::try_from(secs).unwrap_or(0)
}

/// Format Unix seconds as an Odoo datetime (`YYYY-MM-DD HH:MM:SS`, UTC). The
/// inverse of [`odoo_datetime_to_epoch`].
#[must_use]
pub fn epoch_to_odoo_datetime(secs: u64) -> String {
    // u64::MAX / 86_400 fits an i64, so the fallback is unreachable.
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

/// The finish of a planned window moved to start at `new_start`, keeping its
/// duration: `new_start + (finish - start)`, as an Odoo datetime. `None` if any
/// value is not a real date/time ([`canonical_datetime`]) or the shifted finish
/// would fall before the Unix epoch.
#[must_use]
pub fn shifted_finish(start: &str, finish: &str, new_start: &str) -> Option<String> {
    let at = |v: &str| {
        canonical_datetime(v)
            .map(|c| odoo_datetime_to_epoch(&c))
            .and_then(|secs| i64::try_from(secs).ok())
    };
    let moved = at(new_start)? + (at(finish)? - at(start)?);
    u64::try_from(moved).ok().map(epoch_to_odoo_datetime)
}

/// The windows of a split-off lot's work orders: every `(start, finish)` window shifted
/// by one delta, so the earliest start moves to `new_start`. Offsets between the work
/// orders and each window's length are kept. `None` if any value is not a real
/// date/time, or a shifted value would fall before the Unix epoch. Empty in, empty out.
#[must_use]
pub fn shifted_windows(
    windows: &[(String, String)],
    new_start: &str,
) -> Option<Vec<(String, String)>> {
    let at = |v: &str| {
        canonical_datetime(v)
            .map(|c| odoo_datetime_to_epoch(&c))
            .and_then(|secs| i64::try_from(secs).ok())
    };
    let Some(earliest) = windows.iter().map(|(start, _)| at(start)).min() else {
        return Some(Vec::new());
    };
    let delta = at(new_start)? - earliest?;
    let shift = |v: &str| -> Option<String> {
        u64::try_from(at(v)? + delta)
            .ok()
            .map(epoch_to_odoo_datetime)
    };
    windows
        .iter()
        .map(|(start, finish)| Some((shift(start)?, shift(finish)?)))
        .collect()
}

/// Days since the Unix epoch for a civil (proleptic Gregorian) date. Howard
/// Hinnant's algorithm, the inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil (proleptic Gregorian) `(year, month, day)` for days since the Unix
/// epoch. Howard Hinnant's algorithm, the inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn known_verbs_parse_and_unknown_is_rejected() {
        assert_eq!(
            Verb::parse(&ActionType("re_promise_date".to_owned())).expect("verb"),
            Verb::RePromiseDate
        );
        assert_eq!(
            Verb::parse(&ActionType("reschedule_work_order".to_owned())).expect("verb"),
            Verb::RescheduleWorkOrder
        );
        assert_eq!(
            Verb::parse(&ActionType("reassign_work_center".to_owned())).expect("verb"),
            Verb::ReassignWorkCenter
        );
        assert!(Verb::parse(&ActionType("delete_everything".to_owned())).is_err());
    }

    #[test]
    fn required_params_are_enforced() {
        let mut params = BTreeMap::new();
        assert!(validate_params(Verb::RePromiseDate, &params).is_err());
        params.insert("new_date".to_owned(), "  ".to_owned());
        assert!(
            validate_params(Verb::RePromiseDate, &params).is_err(),
            "a blank param is missing"
        );
        params.insert("new_date".to_owned(), "2027-10-07".to_owned());
        assert!(validate_params(Verb::RePromiseDate, &params).is_ok());
        assert!(
            validate_params(Verb::RescheduleWorkOrder, &params).is_ok(),
            "reschedule shares the new_date param"
        );

        // reassign requires new_work_center, not new_date.
        assert!(validate_params(Verb::ReassignWorkCenter, &params).is_err());
        params.insert("new_work_center".to_owned(), "5".to_owned());
        assert!(validate_params(Verb::ReassignWorkCenter, &params).is_ok());
    }

    #[test]
    fn every_verb_round_trips_through_its_name() {
        for verb in Verb::ALL {
            assert_eq!(
                Verb::parse(&ActionType(verb.name().to_owned())).expect("verb"),
                verb
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_well_formed_action_on_its_model_validates() {
        let line = "https://ekg.example/id/purchase.order.line/40";
        let date = params(&[("new_date", "2027-10-12 08:00:00")]);
        assert_eq!(
            validate_action(&ActionType("expedite_po".to_owned()), line, &date).expect("valid"),
            Verb::ExpeditePo
        );
        let so = "https://ekg.example/id/sale.order/1";
        assert!(validate_action(&ActionType("re_promise_date".to_owned()), so, &date).is_ok());
        // notify writes no field, so any grounded target is fine.
        let msg = params(&[("message", "heads up")]);
        assert!(validate_action(&ActionType("notify_owner".to_owned()), so, &msg).is_ok());
    }

    #[test]
    fn a_defective_action_is_refused_before_attach() {
        let line = "https://ekg.example/id/purchase.order.line/40";
        let so = "https://ekg.example/id/sale.order/1";
        let date = params(&[("new_date", "2027-10-12 08:00:00")]);
        let expedite = ActionType("expedite_po".to_owned());
        // the sale-order-targeted expedite
        assert!(validate_action(&expedite, so, &date).is_err());
        // missing, blank and malformed dates
        assert!(validate_action(&expedite, line, &params(&[])).is_err());
        assert!(validate_action(&expedite, line, &params(&[("new_date", " ")])).is_err());
        for bad in [
            "6 Oct",
            "2027-10-12",
            "2027-02-30 00:00:00",
            "2027-10-12T08:00:00",
        ] {
            assert!(
                validate_action(&expedite, line, &params(&[("new_date", bad)])).is_err(),
                "a non-canonical date is refused: {bad}"
            );
        }
        // a re-promise off its sale order, an unknown verb, a bad work center
        assert!(validate_action(&ActionType("re_promise_date".to_owned()), line, &date).is_err());
        assert!(validate_action(&ActionType("split_shipment".to_owned()), line, &date).is_err());
        let wo = "https://ekg.example/id/mrp.workorder/3";
        let wc = params(&[("new_work_center", "a center")]);
        assert!(validate_action(&ActionType("reassign_work_center".to_owned()), wo, &wc).is_err());
    }

    #[test]
    fn a_split_validates_its_quantity_date_and_target() {
        let mo = "https://ekg.example/id/mrp.production/7";
        let split = ActionType("split_production".to_owned());
        let ok = params(&[
            ("split_quantity", "10"),
            ("new_date", "2027-10-24 08:00:00"),
        ]);
        assert_eq!(
            validate_action(&split, mo, &ok).expect("valid"),
            Verb::SplitProduction
        );
        for qty in ["0", "-5", "ten", "NaN", "inf", " "] {
            let bad = params(&[("split_quantity", qty), ("new_date", "2027-10-24 08:00:00")]);
            assert!(
                validate_action(&split, mo, &bad).is_err(),
                "quantity {qty:?}"
            );
        }
        for date in ["18 Oct", "2027-10-24", "2027-10-32 08:00:00"] {
            let bad = params(&[("split_quantity", "10"), ("new_date", date)]);
            assert!(validate_action(&split, mo, &bad).is_err(), "date {date:?}");
        }
        assert!(
            validate_action(&split, mo, &params(&[("split_quantity", "10")])).is_err(),
            "a missing new_date"
        );
        let wo = "https://ekg.example/id/mrp.workorder/3";
        assert!(
            validate_action(&split, wo, &ok).is_err(),
            "a wrong-model target"
        );
    }

    #[test]
    fn a_capacity_raise_validates_its_number_and_target() {
        let cap = "https://ekg.example/id/mrp.workcenter.capacity/2";
        let raise = ActionType("raise_work_center_capacity".to_owned());
        for good in ["2", "2.5", " 3 "] {
            assert_eq!(
                validate_action(&raise, cap, &params(&[("new_capacity", good)])).expect("valid"),
                Verb::RaiseWorkCenterCapacity
            );
        }
        for bad in ["0", "-1", "two", "NaN", "inf", ""] {
            assert!(
                validate_action(&raise, cap, &params(&[("new_capacity", bad)])).is_err(),
                "capacity {bad:?}"
            );
        }
        let wc = "https://ekg.example/id/mrp.workcenter/2";
        assert!(
            validate_action(&raise, wc, &params(&[("new_capacity", "2")])).is_err(),
            "the work center itself is not the capacity record"
        );
    }

    #[test]
    fn every_target_writing_verb_names_its_target_model() {
        assert_eq!(Verb::SplitProduction.target_model(), Some("mrp.production"));
        assert_eq!(
            Verb::RaiseWorkCenterCapacity.target_model(),
            Some("mrp.workcenter.capacity")
        );
        assert_eq!(Verb::NotifyOwner.target_model(), None);
        // A field write's model is its target model.
        for verb in Verb::ALL {
            if let Some((model, _)) = verb.field_write() {
                assert_eq!(verb.target_model(), Some(model), "{verb:?}");
            }
        }
        assert_eq!(
            Verb::RaiseWorkCenterCapacity.field_write_param(),
            Some(("new_capacity", WriteKind::Number))
        );
    }

    #[test]
    fn split_off_windows_keep_offsets_and_lengths() {
        let windows = vec![
            (
                "2027-10-22 12:00:00".to_owned(),
                "2027-10-22 14:00:00".to_owned(),
            ),
            (
                "2027-10-22 08:00:00".to_owned(),
                "2027-10-22 12:00:00".to_owned(),
            ),
        ];
        assert_eq!(
            shifted_windows(&windows, "2027-10-24 08:00:00").expect("shifted"),
            vec![
                (
                    "2027-10-24 12:00:00".to_owned(),
                    "2027-10-24 14:00:00".to_owned()
                ),
                (
                    "2027-10-24 08:00:00".to_owned(),
                    "2027-10-24 12:00:00".to_owned()
                ),
            ]
        );
        assert_eq!(
            shifted_windows(&[], "2027-10-24 08:00:00"),
            Some(Vec::new())
        );
        assert_eq!(shifted_windows(&windows, "someday"), None);
    }

    #[test]
    fn a_split_undo_round_trips_through_the_external_ref() {
        let undo = Undo::SplitProduction {
            production: 7,
            split_off: 8,
            prior_qty: serde_json::json!(40.0),
            prior_name: serde_json::json!("WH/MO/00007"),
            prior_start: serde_json::json!("2027-10-22 08:00:00"),
            prior_finish: serde_json::json!("2027-10-23 08:00:00"),
            windows: vec![WorkOrderWindow {
                id: 11,
                date_start: serde_json::json!("2027-10-22 08:00:00"),
                date_finished: serde_json::json!("2027-10-22 12:00:00"),
                duration_expected: serde_json::json!(2400.0),
            }],
            watermarks: vec![Watermark {
                model: "mrp.production".to_owned(),
                id: 7,
                write_date: 42,
                fields: BTreeMap::from([("product_qty".to_owned(), serde_json::json!(30.0))]),
            }],
        };
        let encoded = undo.encode().expect("encode");
        assert!(
            encoded.contains("\"kind\":\"split_production\""),
            "{encoded}"
        );
        assert_eq!(Undo::decode(&encoded).expect("decode"), undo);
    }

    #[test]
    fn datetimes_canonicalize_or_are_rejected() {
        assert_eq!(
            canonical_datetime("2027-10-12").as_deref(),
            Some("2027-10-12 00:00:00")
        );
        assert_eq!(
            canonical_datetime("2027-10-12T08:30:00").as_deref(),
            Some("2027-10-12 08:30:00")
        );
        assert_eq!(
            canonical_datetime(" 2028-02-29 00:00:00 ").as_deref(),
            Some("2028-02-29 00:00:00")
        );
        for bad in [
            "2027-02-29",
            "2027-13-01",
            "2027-10-12 24:00:00",
            "tomorrow",
            "2027-10-12\"}",
        ] {
            assert_eq!(canonical_datetime(bad), None, "{bad}");
        }
    }

    #[test]
    fn target_iri_parses_model_and_id() {
        assert_eq!(
            parse_target("https://ekg.example/id/sale.order/42").expect("target"),
            ("sale.order".to_owned(), 42)
        );
        assert!(parse_target("https://example.com/other/1").is_err());
        assert!(parse_target("https://ekg.example/id/sale.order/x").is_err());
    }

    #[test]
    fn field_write_maps_only_the_field_verbs() {
        assert_eq!(
            Verb::RePromiseDate.field_write(),
            Some(("sale.order", "commitment_date"))
        );
        assert_eq!(
            Verb::ExpeditePo.field_write(),
            Some(("purchase.order.line", "date_planned"))
        );
        assert_eq!(
            Verb::RescheduleWorkOrder.field_write(),
            Some(("mrp.workorder", "date_start"))
        );
        assert_eq!(
            Verb::ReassignWorkCenter.field_write(),
            Some(("mrp.workorder", "workcenter_id"))
        );
        assert_eq!(Verb::NotifyOwner.field_write(), None);
    }

    #[test]
    fn target_fields_list_every_field_of_the_target_the_write_back_sets() {
        assert_eq!(
            Verb::RescheduleWorkOrder.target_fields(),
            ["date_start", "date_finished"],
            "a reschedule moves the whole planned window"
        );
        assert_eq!(Verb::RePromiseDate.target_fields(), ["commitment_date"]);
        assert_eq!(Verb::ExpeditePo.target_fields(), ["date_planned"]);
        assert_eq!(Verb::ReassignWorkCenter.target_fields(), ["workcenter_id"]);
        assert!(Verb::NotifyOwner.target_fields().is_empty());
        // The primary field leads, so `field_write` stays the first target field.
        for verb in Verb::ALL {
            assert_eq!(
                verb.field_write().map(|(_, field)| field),
                verb.target_fields().first().copied(),
                "{verb:?}"
            );
        }
    }

    #[test]
    fn field_write_param_agrees_with_field_write() {
        // field_write_param is Some exactly when field_write is, and the reassign
        // verb is the only many-to-one write.
        for verb in [
            Verb::RePromiseDate,
            Verb::ExpeditePo,
            Verb::RescheduleWorkOrder,
            Verb::ReassignWorkCenter,
            Verb::NotifyOwner,
        ] {
            assert_eq!(
                verb.field_write().is_some(),
                verb.field_write_param().is_some(),
                "{verb:?}: field_write and field_write_param must agree"
            );
        }
        assert_eq!(
            Verb::RescheduleWorkOrder.field_write_param(),
            Some(("new_date", WriteKind::Text))
        );
        assert_eq!(
            Verb::ReassignWorkCenter.field_write_param(),
            Some(("new_work_center", WriteKind::Many2OneId))
        );
    }

    #[test]
    fn work_center_id_resolves_from_bare_id_and_iri() {
        assert_eq!(resolve_work_center_id("5").expect("bare id"), 5);
        assert_eq!(resolve_work_center_id("  7 ").expect("padded id"), 7);
        assert_eq!(
            resolve_work_center_id("https://ekg.example/id/mrp.workcenter/12").expect("iri"),
            12
        );
        // A foreign-model IRI or a malformed value is rejected.
        assert!(resolve_work_center_id("https://ekg.example/id/sale.order/12").is_err());
        assert!(resolve_work_center_id("not-a-center").is_err());
    }

    #[test]
    fn many2one_prior_id_extracts_the_id_or_false() {
        // Odoo renders a many-to-one read as [id, display_name].
        assert_eq!(
            many2one_prior_id(&serde_json::json!([3, "Assembly Line"])),
            serde_json::json!(3)
        );
        assert_eq!(
            many2one_prior_id(&serde_json::json!(3)),
            serde_json::json!(3)
        );
        assert_eq!(
            many2one_prior_id(&serde_json::Value::Bool(false)),
            serde_json::Value::Bool(false)
        );
    }

    #[test]
    fn undo_round_trips_through_the_external_ref() {
        let undo = Undo::RestoreField {
            model: "sale.order".to_owned(),
            id: 7,
            field: "commitment_date".to_owned(),
            prior: serde_json::Value::Bool(false),
            written: Some(serde_json::json!("2027-10-26 00:00:00")),
        };
        let encoded = undo.encode().expect("encode");
        assert_eq!(Undo::decode(&encoded).expect("decode"), undo);
        assert!(Undo::decode("not json").is_err());
    }

    #[test]
    fn undo_round_trips_a_multi_field_restore() {
        let undo = Undo::RestoreFields {
            restores: vec![
                FieldRestore {
                    model: "purchase.order.line".to_owned(),
                    id: 4,
                    field: "date_planned".to_owned(),
                    prior: serde_json::json!("2027-11-15 08:00:00"),
                    watermark: 1_700_000_000,
                    written: Some(serde_json::json!("2027-11-11 08:00:00")),
                },
                FieldRestore {
                    model: "stock.picking".to_owned(),
                    id: 3,
                    field: "scheduled_date".to_owned(),
                    prior: serde_json::json!("2027-11-15 08:00:00"),
                    watermark: 1_700_000_001,
                    written: None,
                },
            ],
        };
        let encoded = undo.encode().expect("encode");
        assert_eq!(Undo::decode(&encoded).expect("decode"), undo);
    }

    #[test]
    fn a_stored_single_field_undo_still_decodes() {
        // A stored receipt in the single-field wire shape must keep decoding, so an
        // action promoted with it stays reversible.
        let stored = r#"{"kind":"restore_field","model":"sale.order","id":7,"field":"commitment_date","prior":"2027-10-22 00:00:00"}"#;
        assert_eq!(
            Undo::decode(stored).expect("legacy decode"),
            Undo::RestoreField {
                model: "sale.order".to_owned(),
                id: 7,
                field: "commitment_date".to_owned(),
                prior: serde_json::json!("2027-10-22 00:00:00"),
                written: None,
            }
        );
    }

    #[test]
    fn a_stored_undo_without_written_values_keeps_its_wire_shape() {
        // A stored receipt may carry no written values. It decodes with none (so the
        // reverse keeps the write_date guard) and re-encodes byte-identically.
        let field = r#"{"kind":"restore_field","model":"sale.order","id":7,"field":"commitment_date","prior":false}"#;
        assert_eq!(
            Undo::decode(field)
                .expect("decode")
                .encode()
                .expect("encode"),
            field
        );
        let fields = r#"{"kind":"restore_fields","restores":[{"model":"stock.picking","id":3,"field":"scheduled_date","prior":false,"watermark":9}]}"#;
        assert_eq!(
            Undo::decode(fields)
                .expect("decode")
                .encode()
                .expect("encode"),
            fields
        );
        let split = r#"{"kind":"split_production","production":7,"split_off":8,"prior_qty":40.0,"prior_name":"MO","prior_start":false,"prior_finish":false,"windows":[],"watermarks":[{"model":"mrp.production","id":7,"write_date":42}]}"#;
        let Undo::SplitProduction { watermarks, .. } = Undo::decode(split).expect("decode") else {
            panic!("a split");
        };
        assert!(watermarks[0].fields.is_empty());
    }

    #[test]
    fn same_value_compares_many_to_ones_by_id_and_numbers_numerically() {
        use serde_json::json;
        assert!(same_value(
            &json!("2027-10-26 00:00:00"),
            &json!("2027-10-26 00:00:00")
        ));
        assert!(!same_value(
            &json!("2027-10-27 00:00:00"),
            &json!("2027-10-26 00:00:00")
        ));
        assert!(same_value(
            &json!([4, "Assembly Line"]),
            &json!([4, "Assembly (renamed)"])
        ));
        assert!(same_value(&json!([4, "Assembly Line"]), &json!(4)));
        assert!(!same_value(
            &json!([5, "Paint"]),
            &json!([4, "Assembly Line"])
        ));
        assert!(!same_value(&json!(false), &json!([4, "Assembly Line"])));
        assert!(same_value(&json!(2), &json!(2.0)));
        assert!(!same_value(&json!(3.0), &json!(2.0)));
        assert!(same_value(&json!(false), &json!(false)));
        assert!(!same_value(&json!(false), &json!("2027-10-26 00:00:00")));
    }

    #[test]
    fn undo_round_trips_a_many_to_one_prior_id() {
        // The reassign inverse restores the prior work-center as an integer id
        // (or `false` when it was unset). The reverse writes it back verbatim.
        for prior in [serde_json::json!(4), serde_json::Value::Bool(false)] {
            let undo = Undo::RestoreField {
                model: "mrp.workorder".to_owned(),
                id: 11,
                field: "workcenter_id".to_owned(),
                prior: prior.clone(),
                written: Some(serde_json::json!([5, "Paint"])),
            };
            let encoded = undo.encode().expect("encode");
            assert_eq!(Undo::decode(&encoded).expect("decode"), undo);
        }
    }

    #[test]
    fn odoo_datetime_parses_to_epoch() {
        // 1_700_000_000 = 2023-11-14 22:13:20 UTC (matches sweep.rs's forward map).
        assert_eq!(odoo_datetime_to_epoch("2023-11-14 22:13:20"), 1_700_000_000);
        assert_eq!(odoo_datetime_to_epoch("1970-01-01 00:00:00"), 0);
        assert_eq!(
            odoo_datetime_to_epoch(""),
            0,
            "a false/blank write_date is 0"
        );
    }

    #[test]
    fn epoch_formats_as_an_odoo_datetime_and_round_trips() {
        assert_eq!(epoch_to_odoo_datetime(0), "1970-01-01 00:00:00");
        assert_eq!(epoch_to_odoo_datetime(1_700_000_000), "2023-11-14 22:13:20");
        for dt in [
            "1970-01-01 00:00:00",
            "1999-12-31 23:59:59", // year boundary
            "2000-01-01 00:00:00",
            "2000-02-29 12:00:00", // leap day of a 400-year leap year
            "2024-02-29 23:59:59", // leap day
            "2024-03-01 00:00:00", // the day after
            "2025-02-28 23:59:59", // a common year's February end
            "2027-12-31 23:59:59",
            "2027-01-01 00:00:00",
            "2100-03-01 00:00:00", // 2100 is not a leap year
        ] {
            assert_eq!(
                epoch_to_odoo_datetime(odoo_datetime_to_epoch(dt)),
                dt,
                "{dt}"
            );
        }
        // And the other way, across a leap-year span, day by day.
        let start = odoo_datetime_to_epoch("2023-12-30 07:30:15");
        for day in 0..800 {
            let secs = start + day * 86_400;
            assert_eq!(odoo_datetime_to_epoch(&epoch_to_odoo_datetime(secs)), secs);
        }
    }

    #[test]
    fn a_shifted_finish_keeps_the_window_duration() {
        assert_eq!(
            shifted_finish(
                "2027-10-23 08:00:00",
                "2027-10-23 12:00:00",
                "2027-10-25 08:00:00"
            )
            .as_deref(),
            Some("2027-10-25 12:00:00")
        );
        // Earlier, across a month and a leap day.
        assert_eq!(
            shifted_finish(
                "2028-03-01 22:00:00",
                "2028-03-02 02:00:00",
                "2028-02-28 22:00:00"
            )
            .as_deref(),
            Some("2028-02-29 02:00:00")
        );
        // A bare-date new start is midnight.
        assert_eq!(
            shifted_finish("1999-12-31 20:00:00", "2000-01-01 01:00:00", "2000-01-05").as_deref(),
            Some("2000-01-05 05:00:00")
        );
        assert_eq!(
            shifted_finish("2027-10-23 08:00:00", "not a date", "2027-10-25 08:00:00"),
            None
        );
        assert_eq!(
            shifted_finish(
                "1970-01-02 00:00:00",
                "1970-01-01 00:00:00",
                "1970-01-01 00:00:00"
            ),
            None,
            "a finish that would fall before the epoch is refused, never wrapped"
        );
    }
}
