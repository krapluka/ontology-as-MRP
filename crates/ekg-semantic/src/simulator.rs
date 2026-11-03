//! The GraphDB driver for the [`Simulator`] seam.
//!
//! Projects a hypothetical [`Plan`] onto a what-if read model and hands back a [`Projection`]
//! a capability queries with its own unchanged rule. The projection is an ephemeral
//! repository (`sim_{id}`) created without SHACL. The base repository's statements are
//! copied into it (all named graphs, N-Quads) and the plan's diff is applied there: the
//! touched predicate's old value is removed across graphs and the new one asserted, so the
//! rule sees only the new value. The repo is dropped when the [`Projection`] is dropped.
//!
//! The sim repo is owned by a cleanup guard from before it is created, so a projection that
//! fails, times out or is cancelled while it is being built drops its repo too. The create
//! request runs as its own task and the teardown waits for it before it drops the repo, so a
//! drop cannot overtake a create the server goes on to complete.
//! [`GraphDbSimulator::sweep_stale`] covers a process that was killed and a runtime that
//! ended before a teardown ran: it drops the `sim_*` repos older than a maximum age.
//!
//! The projection permit is released only after the repo is dropped, so it is held for as
//! long as the create takes to answer. [`GraphDbClient`] sets no request timeout: a create
//! the server accepts and never answers holds its permit until the connection fails.
//!
//! The projection needs its own repository; a named graph in the live repo does not work.
//! The capability rules query the default graph, which GraphDB resolves as the union of all
//! named graphs, so the old value of a rescheduled `ekg:startedAt` has to be removed, not
//! overlaid. Inside the live repo the transient two-value state violates the
//! `WorkOrderShape` `sh:maxCount 1` write-shape, and a copy that reads the union would pick
//! up other concurrent projections' graphs.
//!
//! The driver only creates, writes and drops its own repo. It never publishes an
//! `RdfChanged` and never mutates the base repo or a system of record.
//!
//! The mapping from an opaque [`ProposedAction`] to its diff triples is capability knowledge
//! and lives here, mirroring the kinetic verbs' field targets.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ekg_domain::{EngineError, Plan, Projection, ProposedAction, Result, Simulator};
use opentelemetry::Context;
use opentelemetry::trace::FutureExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::world_model::GraphDbClient;

/// The ontology (predicate) namespace. Matches every capability rule's `PREFIX ekg:` and
/// the materialized triples.
const EKG_PREFIX: &str = "https://ekg.example/ontology#";
/// The id namespace for entity IRIs (e.g. a work centre).
const ID_NAMESPACE: &str = "https://ekg.example/id/";
/// The XSD dateTime datatype IRI the window predicates are typed with.
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
/// The XSD decimal datatype IRI quantities, durations and capacities are typed with.
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
/// The id offset of the synthetic IRIs a projected split mints for its split-off lot and
/// that lot's work orders: `…/mrp.production/{SIM_SPLIT_BASE + id}`. Reserved and
/// projection-only: such an entity exists only inside a sim repo.
pub const SIM_SPLIT_BASE: i64 = 1_000_000_000_000;
/// The named graph in the sim repo that a diff's new value lands in. The rule queries the
/// union, so which graph it is does not matter.
const DIFF_GRAPH: &str = "urn:sim:diff";

/// Monotonic within a process; combined with the pid and a wall-clock stamp it makes
/// each sim repo id unique across concurrent projections and across runs.
static SIM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many projections a process runs at once unless configured otherwise. GraphDB Free
/// allows 5 repositories per server: with the base repository and one fixture or spare, two
/// concurrent sim repos stay within it.
pub const DEFAULT_MAX_PROJECTIONS: usize = 2;

/// The process-wide projection permits, shared by every [`GraphDbSimulator`]. A backend (or
/// a test binary) may build several simulators against the same server, and the repository
/// cap is the server's, not an instance's.
static PROJECTION_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// A permit pool admitting `limit` concurrent projections.
fn permit_pool(limit: NonZeroUsize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(limit.get()))
}

/// The process-wide pool, fixed at [`DEFAULT_MAX_PROJECTIONS`] on first use unless
/// [`set_max_concurrent_projections`] fixed it first.
fn projection_permits() -> Arc<Semaphore> {
    Arc::clone(PROJECTION_PERMITS.get_or_init(|| {
        permit_pool(NonZeroUsize::new(DEFAULT_MAX_PROJECTIONS).unwrap_or(NonZeroUsize::MIN))
    }))
}

/// Fix how many projections this process runs at once. Call it at startup, before any
/// projection: it returns `false` (and changes nothing) once the limit is fixed, by an earlier
/// call or by a first projection at the default.
pub fn set_max_concurrent_projections(limit: NonZeroUsize) -> bool {
    PROJECTION_PERMITS.set(permit_pool(limit)).is_ok()
}

/// The prefix of every sim-repo id. Repo-id-safe (no colons), and reserved: the orphan sweep
/// considers only ids that carry it.
const SIM_REPO_PREFIX: &str = "sim_";

/// Nanoseconds since the Unix epoch, as a sim-repo id carries them.
fn epoch_nanos(at: SystemTime) -> u128 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// Mint a fresh, unique ephemeral sim-repo id: `sim_{pid}_{nanos}_{n}`. The `sim_` prefix
/// and the mint time let [`stale_sim_repos`] pick out a repo a crashed process left behind.
fn sim_repo_id() -> String {
    let nanos = epoch_nanos(SystemTime::now());
    let n = SIM_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{SIM_REPO_PREFIX}{}_{nanos}_{n}", std::process::id())
}

/// When a sim-repo id was minted, in nanoseconds since the Unix epoch. `None` for any id
/// that is not of the form `sim_{pid}_{nanos}_{n}`.
fn sim_repo_minted_at(repo_id: &str) -> Option<u128> {
    let mut parts = repo_id.strip_prefix(SIM_REPO_PREFIX)?.split('_');
    let (pid, nanos, n) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    pid.parse::<u32>().ok()?;
    n.parse::<u64>().ok()?;
    nanos.parse::<u128>().ok()
}

/// The sim repos among `repo_ids` minted more than `max_age` before `now`. A younger sim
/// repo may be a live projection of another process sharing the server, and an id that does
/// not parse as a sim-repo id is not ours to judge: both are left out.
fn stale_sim_repos(repo_ids: &[String], now: SystemTime, max_age: Duration) -> Vec<String> {
    let now = epoch_nanos(now);
    repo_ids
        .iter()
        .filter(|id| {
            sim_repo_minted_at(id)
                .is_some_and(|minted| now.saturating_sub(minted) > max_age.as_nanos())
        })
        .cloned()
        .collect()
}

/// Drop the sim repo `repo`, logging a failure with its id. Dropping a repo the server does
/// not have (never created, or already dropped) succeeds, so a drop sent while the create is
/// still in flight would leave the repo behind: callers wait for the create first.
async fn drop_scratch(base: &GraphDbClient, repo: &str) {
    if let Err(err) = base.drop_repo(repo).await {
        eprintln!("warning: simulator: scratch repo {repo} was not dropped: {err}");
    }
}

/// The owner of one sim repo and its projection permit. It exists from before the repo is
/// created, so whatever ends the projection (its handle dropped, a failed setup, a future
/// dropped mid-setup) the repo is dropped, and the permit is released only after that.
struct ScratchRepo {
    base: Arc<GraphDbClient>,
    /// The sim repo's id; empty once [`ScratchRepo::discard`] dropped it.
    repo: String,
    permit: Option<OwnedSemaphorePermit>,
    /// The create request while it is in flight. It runs as its own task, so a projection
    /// dropped mid-create does not cancel a request the server goes on to complete.
    creating: Option<JoinHandle<Result<()>>>,
}

impl ScratchRepo {
    /// Send the create request for the repo and wait for its outcome.
    async fn create(&mut self) -> Result<()> {
        let (base, repo) = (Arc::clone(&self.base), self.repo.clone());
        // A spawned task does not inherit the caller's trace context: hand it over, so the
        // create request is part of the operation the projection is made for.
        let creating = self.creating.insert(tokio::spawn(
            async move { base.create_scratch_repo(&repo).await }.with_context(Context::current()),
        ));
        let created = creating.await.unwrap_or_else(|err| {
            Err(EngineError::Backend(format!(
                "scratch repo create did not finish: {err}"
            )))
        });
        self.creating = None;
        created
    }

    /// Drop the repo now and release the permit. If this future is itself dropped before
    /// the repo is gone, [`Drop`] still drops it.
    async fn discard(mut self) {
        drop_scratch(&self.base, &self.repo).await;
        self.repo.clear();
    }
}

impl Drop for ScratchRepo {
    fn drop(&mut self) {
        if self.repo.is_empty() {
            return;
        }
        // Async drop is not expressible, so spawn the repo drop on the current runtime; the
        // permit travels with it, so the next projection is admitted only once this repo is
        // gone. Outside a runtime the repo cannot be dropped here: it is logged, and the
        // stale sweep reclaims it.
        let repo = std::mem::take(&mut self.repo);
        let permit = self.permit.take();
        let creating = self.creating.take();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let base = Arc::clone(&self.base);
                handle.spawn(async move {
                    if let Some(creating) = creating {
                        let _ = creating.await;
                    }
                    drop_scratch(&base, &repo).await;
                    drop(permit);
                });
            }
            Err(_) => {
                eprintln!("warning: simulator: scratch repo {repo} was not dropped: no runtime");
            }
        }
    }
}

/// A governed verb's field write as the projection mirrors it: the Odoo `model` its
/// target must be, and every `(field, predicate)` pair it sets on the target: the Odoo field
/// the kinetic write-back sets and the `ekg:` predicate (local name) it materializes as, the
/// primary one first. This is capability knowledge, kept in step with the kinetic verbs'
/// `field_write` / `target_fields`, which a backend test pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerbWrite {
    /// The Odoo model of the verb's target.
    pub model: &'static str,
    /// Every `(field, predicate)` the write sets on the target, the primary one first.
    pub fields: &'static [(&'static str, &'static str)],
}

impl VerbWrite {
    /// The primary Odoo field the write-back sets (the one its params name).
    #[must_use]
    pub fn field(&self) -> &'static str {
        self.fields.first().map_or("", |(field, _)| field)
    }

    /// The `ekg:` predicate of the primary field: the fact whose current value is a change's
    /// from.
    #[must_use]
    pub fn predicate(&self) -> &'static str {
        self.fields.first().map_or("", |(_, predicate)| predicate)
    }
}

/// The field write of a governed verb, or `None` for a verb that writes no
/// rule-relevant fact (`notify_owner`) or is unknown.
#[must_use]
pub fn verb_write(verb: &str) -> Option<VerbWrite> {
    let (model, fields): (_, &'static [(&'static str, &'static str)]) = match verb {
        // Supply-risk: moving the customer promise gives the shortfall more time, so more
        // inbound arrives before it.
        "re_promise_date" => ("sale.order", &[("commitment_date", "commitment_date")]),
        // Moves the cause line's arrival; its open receipts move too (see `diff_of`).
        "expedite_po" => ("purchase.order.line", &[("date_planned", "date_planned")]),
        // Moves the whole planned window. The manufacturing order follows (see `order_diff`).
        "reschedule_work_order" => (
            "mrp.workorder",
            &[("date_start", "startedAt"), ("date_finished", "endedAt")],
        ),
        "reassign_work_center" => ("mrp.workorder", &[("workcenter_id", "performedBy")]),
        // Rewrites the capacity record; the agent's derived capacity follows it once the
        // structural derivations re-run in the sim repo.
        "raise_work_center_capacity" => ("mrp.workcenter.capacity", &[("capacity", "capacity")]),
        _ => return None,
    };
    Some(VerbWrite { model, fields })
}

/// A work order's current planned window in the sim repo, as materialized (ISO datetimes).
/// A reschedule's duration-preserving diff is built from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Window {
    /// The planned start.
    pub start: String,
    /// The planned end, when set.
    pub end: Option<String>,
}

/// Seconds since the Unix epoch of a strict ISO `YYYY-MM-DDTHH:MM:SS`, or `None`.
pub(crate) fn iso_to_epoch(iso: &str) -> Option<i64> {
    if !is_strict_xsd_datetime(iso) {
        return None;
    }
    let days = crate::tabs::Day::from_iso(iso)?.days_since(crate::tabs::Day::EPOCH);
    let n = |a: usize, b: usize| iso[a..b].parse::<i64>().ok();
    Some(days * 86_400 + n(11, 13)? * 3_600 + n(14, 16)? * 60 + n(17, 19)?)
}

/// The strict ISO `YYYY-MM-DDTHH:MM:SS` of seconds since the Unix epoch.
pub(crate) fn epoch_to_iso(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    format!(
        "{}T{:02}:{:02}:{:02}",
        crate::tabs::Day::EPOCH.plus(days).iso(),
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The reschedule's shifted end: `end + (new_start - start)`, as a typed literal, or `None`
/// when the window has no end (the write-back then moves the start only, as it does in Odoo).
///
/// # Errors
/// [`EngineError::Backend`] if the current window cannot be read as strict datetimes.
fn shifted_end(window: &Window, new_start: &str) -> Result<Option<String>> {
    let Some(end) = &window.end else {
        return Ok(None);
    };
    let unreadable = || {
        EngineError::Backend(format!(
            "the work order's planned window {window:?} is not a strict dateTime window"
        ))
    };
    let (start, end, new_start) = (
        iso_to_epoch(&window.start).ok_or_else(unreadable)?,
        iso_to_epoch(end).ok_or_else(unreadable)?,
        iso_to_epoch(new_start).ok_or_else(unreadable)?,
    );
    let shifted = end.checked_add(new_start - start).ok_or_else(unreadable)?;
    datetime_literal(&epoch_to_iso(shifted)).map(Some)
}

/// The world-model diff one field write makes on its target: the predicate it rewrites
/// and the new object term (already SPARQL-encoded: a typed literal or an IRI).
struct Diff {
    predicate: String,
    object_term: String,
}

/// Encode an Odoo-style datetime (`"YYYY-MM-DD HH:MM:SS"`) as a typed xsd:dateTime
/// literal matching the materialized form (ISO 8601 with a `T` separator).
///
/// `new_date` is unvalidated free text (it can come from a language model) and is
/// interpolated into a SPARQL UPDATE run with repository authority, so the value is checked
/// against `YYYY-MM-DDTHH:MM:SS` first. Only digits and the fixed `-`/`T`/`:` separators
/// pass, so nothing can terminate the literal or the surrounding update.
/// [`work_center_iri_term`] does the same by reducing its param to an `i64`.
///
/// # Errors
/// [`EngineError::Backend`] if the value is not a strict `YYYY-MM-DDTHH:MM:SS`.
fn datetime_literal(value: &str) -> Result<String> {
    let iso = value.trim().replacen(' ', "T", 1);
    if !is_strict_xsd_datetime(&iso) {
        return Err(EngineError::Backend(format!(
            "new_date {value:?} is not a strict YYYY-MM-DDTHH:MM:SS dateTime"
        )));
    }
    Ok(format!("\"{iso}\"^^<{XSD_DATETIME}>"))
}

/// True only for a well-formed, in-range `YYYY-MM-DDTHH:MM:SS`: digits in every numeric
/// position with the separators fixed (so no quote, brace, angle bracket, semicolon, hash
/// or whitespace can reach the SPARQL literal), and each component within range. The range
/// check matters because a value like `2027-13-45T99:00:00` would be an ill-typed
/// `xsd:dateTime` literal: the rule's dateTime `FILTER` comparison then errors and is
/// treated as false, which drops the anchor and reports the risk as cleared when it is not.
fn is_strict_xsd_datetime(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 19 {
        return false;
    }
    let shaped = bytes.iter().enumerate().all(|(i, &c)| match i {
        4 | 7 => c == b'-',
        10 => c == b'T',
        13 | 16 => c == b':',
        _ => c.is_ascii_digit(),
    });
    if !shaped {
        return false;
    }
    // Components are digits-only after the shape check above, so parsing cannot fail.
    let n = |a: usize, b: usize| s[a..b].parse::<u32>().unwrap_or(u32::MAX);
    let year = n(0, 4);
    let (month, day) = (n(5, 7), n(8, 10));
    let (hour, min, sec) = (n(11, 13), n(14, 16), n(17, 19));
    if !(1..=12).contains(&month) {
        return false;
    }
    // Bound the day by the actual month (and leap year for February), so a
    // calendar-invalid date like `2027-02-30` or `2027-04-31` is rejected. It would
    // otherwise be an ill-typed xsd:dateTime.
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap {
                29
            } else {
                28
            }
        }
    };
    (1..=max_day).contains(&day) && hour <= 23 && min <= 59 && sec <= 59
}

/// Encode a positive, finite quantity as a typed `xsd:decimal` literal. The value is parsed
/// to an `f64` before it is interpolated, so only digits and a `.` can reach the UPDATE.
///
/// # Errors
/// [`EngineError::Backend`] if `value` is not a finite number greater than zero.
fn decimal_literal(param: &str, value: &str) -> Result<String> {
    positive_number(param, value).map(decimal_term)
}

/// Parse a positive, finite number param.
///
/// # Errors
/// [`EngineError::Backend`] if `value` is not a finite number greater than zero.
fn positive_number(param: &str, value: &str) -> Result<f64> {
    match value.trim().parse::<f64>() {
        Ok(number) if number.is_finite() && number > 0.0 => Ok(number),
        _ => Err(EngineError::Backend(format!(
            "{param} {value:?} is not a positive number"
        ))),
    }
}

/// A finite number as a typed `xsd:decimal` literal (Rust's `Display` never uses an
/// exponent, so the lexical form is a valid decimal).
fn decimal_term(number: f64) -> String {
    format!("\"{number}\"^^<{XSD_DECIMAL}>")
}

/// Resolve a `new_work_center` param (a bare id `"5"` or an ontology work-centre IRI)
/// to the work-centre entity IRI term the `ekg:performedBy` edge points at.
fn work_center_iri_term(value: &str) -> Result<String> {
    let value = value.trim();
    let not_wc = || {
        EngineError::Backend(format!(
            "new_work_center {value:?} is not a work-centre id or IRI"
        ))
    };
    let id: i64 = if let Some(rest) = value.strip_prefix(ID_NAMESPACE) {
        // A full IRI must name the work-centre model. A different entity (e.g. a work
        // order) is never coerced into a work centre.
        let (model, id) = rest.rsplit_once('/').ok_or_else(not_wc)?;
        if model != "mrp.workcenter" {
            return Err(not_wc());
        }
        id.parse().map_err(|_| not_wc())?
    } else {
        value.parse().map_err(|_| not_wc())?
    };
    Ok(format!("<{ID_NAMESPACE}mrp.workcenter/{id}>"))
}

/// Validate an action target and return it as a bracketed IRI term safe to interpolate
/// into the UPDATE. Like the two params, the target is untrusted (nothing guarantees an
/// `EntityUri` is well-formed, and it can come from a language model): it must be an id
/// IRI of the shape `{ID_NAMESPACE}{model}/{i64}`, where `model` is only `[a-z0-9._]`, so
/// no `>`, whitespace or SPARQL metacharacter can break out of the `<…>`.
///
/// # Errors
/// [`EngineError::Backend`] if the target is not a well-formed ontology id IRI.
fn target_iri_term(uri: &str) -> Result<String> {
    let malformed = || EngineError::Backend(format!("target {uri:?} is not an ontology id IRI"));
    let rest = uri.strip_prefix(ID_NAMESPACE).ok_or_else(malformed)?;
    let (model, id) = rest.rsplit_once('/').ok_or_else(malformed)?;
    // `model` is the only free text left; keep it to Odoo model characters.
    if model.is_empty()
        || !model
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'_')
    {
        return Err(malformed());
    }
    // `id` must be a bare integer, which rejects any trailing metacharacters.
    let id: i64 = id.parse().map_err(|_| malformed())?;
    Ok(format!("<{ID_NAMESPACE}{model}/{id}>"))
}

/// Derive the SPARQL updates a proposed action makes to the sim repo, in order: none for
/// a verb that changes no rule-relevant fact (`notify_owner`, which creates a record).
///
/// The updates mirror the kinetic write-back, so a plan scored as clearing also clears
/// after its write-back:
/// - every field-write verb rewrites its predicate on its own target, which must be of
///   the verb's model. A mistargeted action is refused here as the executor refuses it;
/// - `expedite_po` also moves `ekg:eta` on every open shipment delivering the line's
///   purchase order, as the executor moves those receipts' `scheduled_date`;
/// - `reschedule_work_order` moves the work order's whole planned window by the same delta
///   (`ekg:startedAt` to the new start and `ekg:endedAt` to the old end shifted by as much),
///   as the executor writes `date_start` and `date_finished` together. Odoo recomputes the
///   finish from the expected duration and the work centre's calendar, which the graph does
///   not hold, so across non-working time the real finish can be later. `window` is the
///   target's current window (read from the sim repo by the caller); with no window, or no
///   end, only the start moves, as in the write-back.
///
/// An unknown verb or a missing or malformed param is an error, not a wrong projection.
fn diff_of(action: &ProposedAction, window: Option<&Window>) -> Result<Vec<String>> {
    let verb = action.action_type.0.as_str();
    if verb == "notify_owner" {
        return Ok(Vec::new());
    }
    let write = verb_write(verb).ok_or_else(|| {
        EngineError::Backend(format!(
            "no world-model projection mapping for verb {verb:?}"
        ))
    })?;
    let target = target_iri_term(&action.target.0)?;
    let model = action
        .target
        .0
        .strip_prefix(ID_NAMESPACE)
        .and_then(|rest| rest.rsplit_once('/'))
        .map_or("", |(model, _)| model);
    if model != write.model {
        return Err(EngineError::Backend(format!(
            "verb {verb:?} expects a {} target, got {:?}",
            write.model, action.target.0
        )));
    }
    let param = |key: &str| {
        action.params.get(key).cloned().ok_or_else(|| {
            EngineError::Backend(format!(
                "action {verb} is missing param {key:?} for projection"
            ))
        })
    };
    let object_term = match verb {
        "reassign_work_center" => work_center_iri_term(&param("new_work_center")?)?,
        "raise_work_center_capacity" => decimal_literal("new_capacity", &param("new_capacity")?)?,
        _ => datetime_literal(&param("new_date")?)?,
    };
    let update = |predicate: &str, object_term: &str| {
        diff_sparql(
            &target,
            &Diff {
                predicate: format!("<{EKG_PREFIX}{predicate}>"),
                object_term: object_term.to_owned(),
            },
        )
    };
    let mut updates = vec![update(write.predicate(), &object_term)];
    if verb == "expedite_po" {
        updates.push(open_shipments_eta_sparql(&target, &object_term));
    }
    if verb == "reschedule_work_order" {
        let new_start = param("new_date")?.trim().replacen(' ', "T", 1);
        if let Some(end_term) = window
            .map(|w| shifted_end(w, &new_start))
            .transpose()?
            .flatten()
        {
            for (_, predicate) in &write.fields[1..] {
                updates.push(update(predicate, &end_term));
            }
        }
    }
    Ok(updates)
}

/// The manufacturing order of a work order, and whether the work order opens or closes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderEnds {
    /// The manufacturing order's IRI.
    order: String,
    /// The work order is the order's first.
    first: bool,
    /// The work order is the order's last.
    last: bool,
}

/// The updates that move a manufacturing order with its work order, as Odoo does on the write:
/// the first work order's start is the order's start, the last one's finish the order's finish.
fn order_diff(
    action: &ProposedAction,
    window: Option<&Window>,
    ends: &OrderEnds,
) -> Result<Vec<String>> {
    let order = target_iri_term(&ends.order)?;
    let Some(new_date) = action.params.get("new_date") else {
        return Ok(Vec::new());
    };
    let update = |predicate: &str, object_term: String| {
        diff_sparql(
            &order,
            &Diff {
                predicate: format!("<{EKG_PREFIX}{predicate}>"),
                object_term,
            },
        )
    };
    let mut updates = Vec::new();
    if ends.first {
        updates.push(update("startedAt", datetime_literal(new_date)?));
    }
    if ends.last {
        let new_start = new_date.trim().replacen(' ', "T", 1);
        let end = window
            .map(|w| shifted_end(w, &new_start))
            .transpose()?
            .flatten();
        updates.extend(end.map(|end| update("endedAt", end)));
    }
    Ok(updates)
}

/// What a projected split reads of the order it splits: its quantity and planned window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SplitOrder {
    /// The order's quantity.
    pub qty: f64,
    /// Its planned start (strict ISO).
    pub start: String,
    /// Its planned end (strict ISO).
    pub end: String,
}

/// What a projected split reads of one of the order's work orders.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SplitWorkOrder {
    /// The work order's IRI.
    pub iri: String,
    /// Its planned window, when it has one (strict ISO).
    pub window: Option<(String, String)>,
    /// Its expected duration, when set.
    pub duration: Option<f64>,
}

/// The numeric id of an ontology id IRI of `model`, or `None`.
fn iri_id(iri: &str, model: &str) -> Option<i64> {
    iri.strip_prefix(ID_NAMESPACE)?
        .strip_prefix(model)?
        .strip_prefix('/')?
        .parse()
        .ok()
}

/// The updates a `split_production` makes in the sim repo, mirroring the governed
/// write-back (as observed in Odoo 19 by `writeback_it`):
///
/// - the order's `ekg:product_qty` becomes `qty - split_quantity`; it keeps its window and
///   its work orders keep theirs (the write-back re-plans them after Odoo clears them);
/// - a split-off lot `…/mrp.production/{SIM_SPLIT_BASE + id}` is inserted with the order's
///   types, `produced`, `builds_from`, `origin_ref` and state, `product_qty = split`,
///   `startedAt = new_date` and `endedAt = new_date + (end - start)`;
/// - each work order gets a clone `…/mrp.workorder/{SIM_SPLIT_BASE + id}` `part_of` the
///   lot, with the same types, `performedBy`, `realizes` and state; the planned windows
///   are shifted by one delta so the earliest starts at `new_date`, keeping offsets and
///   lengths;
/// - `duration_expected` follows Odoo's rescaling by quantity: the order's work orders
///   carry `* (qty - split) / qty`, the clones `* split / qty`.
///
/// Every interpolated value is a validated IRI, a strict dateTime or a parsed number;
/// the copied terms are copied by the UPDATE itself, never interpolated.
///
/// # Errors
/// [`EngineError::Backend`] for a target that is not a manufacturing order, a missing or
/// malformed param, a quantity not less than the order's, or an unreadable window.
fn split_diff(
    action: &ProposedAction,
    order: &SplitOrder,
    work_orders: &[SplitWorkOrder],
) -> Result<Vec<String>> {
    let target = target_iri_term(&action.target.0)?;
    let id = iri_id(&action.target.0, "mrp.production").ok_or_else(|| {
        EngineError::Backend(format!(
            "split_production expects a mrp.production target, got {:?}",
            action.target.0
        ))
    })?;
    let param = |key: &str| {
        action.params.get(key).cloned().ok_or_else(|| {
            EngineError::Backend(format!(
                "action split_production is missing param {key:?} for projection"
            ))
        })
    };
    let split = positive_number("split_quantity", &param("split_quantity")?)?;
    if split >= order.qty {
        return Err(EngineError::Backend(format!(
            "split_quantity {split} is not less than the order's quantity {}",
            order.qty
        )));
    }
    let new_start = param("new_date")?.trim().replacen(' ', "T", 1);
    let new_start_term = datetime_literal(&new_start)?;
    let lot_end = shifted_end(
        &Window {
            start: order.start.clone(),
            end: Some(order.end.clone()),
        },
        &new_start,
    )?
    .ok_or_else(|| EngineError::Backend("the order has no planned end".to_owned()))?;
    let lot = format!("<{ID_NAMESPACE}mrp.production/{}>", SIM_SPLIT_BASE + id);
    let (keep, share) = ((order.qty - split) / order.qty, split / order.qty);

    let pred = |local: &str| format!("<{EKG_PREFIX}{local}>");
    let rewrite = |subject: &str, local: &str, object_term: &str| {
        diff_sparql(
            subject,
            &Diff {
                predicate: pred(local),
                object_term: object_term.to_owned(),
            },
        )
    };
    let mut updates = vec![rewrite(
        &target,
        "product_qty",
        &decimal_term(order.qty - split),
    )];
    updates.push(lot_insert(&target, &lot, split, &new_start_term, &lot_end));
    // The clones' windows: shifted by one delta so the earliest starts at `new_date`.
    let earliest = work_orders
        .iter()
        .filter_map(|wo| {
            wo.window
                .as_ref()
                .and_then(|(start, _)| iso_to_epoch(start))
        })
        .min();
    let delta = match earliest {
        Some(earliest) => Some(
            iso_to_epoch(&new_start).ok_or_else(|| {
                EngineError::Backend(format!("new_date {new_start:?} is unreadable"))
            })? - earliest,
        ),
        None => None,
    };
    for wo in work_orders {
        updates.extend(clone_work_order(wo, &lot, delta, keep, share)?);
    }
    Ok(updates)
}

/// The UPDATE inserting a split-off `lot` of `split` units from `start_term` to `end_term`,
/// copying the order `target`'s types, `produced`, `builds_from`, `origin_ref` and state by
/// the UPDATE itself (never interpolated).
fn lot_insert(target: &str, lot: &str, split: f64, start_term: &str, end_term: &str) -> String {
    format!(
        "PREFIX ekg: <{EKG_PREFIX}>\n\
         INSERT {{ GRAPH <{DIFF_GRAPH}> {{\n\
           {lot} a ?type ; ekg:produced ?product ; ekg:builds_from ?bom ;\n\
             ekg:origin_ref ?origin ; ekg:lifecycle_state ?state ;\n\
             ekg:product_qty {qty} ; ekg:startedAt {start_term} ; ekg:endedAt {end_term} .\n\
         }} }}\n\
         WHERE {{\n\
           {target} a ?type .\n\
           OPTIONAL {{ {target} ekg:produced ?product }}\n\
           OPTIONAL {{ {target} ekg:builds_from ?bom }}\n\
           OPTIONAL {{ {target} ekg:origin_ref ?origin }}\n\
           OPTIONAL {{ {target} ekg:lifecycle_state ?state }}\n\
         }}",
        qty = decimal_term(split),
    )
}

/// The updates cloning work order `wo` onto the split-off `lot`: the clone (its window
/// shifted by `delta` seconds when both are known, its duration times `share`), and the
/// original's duration rescaled by `keep`.
///
/// # Errors
/// [`EngineError::Backend`] for an unreadable work-order IRI or window.
fn clone_work_order(
    wo: &SplitWorkOrder,
    lot: &str,
    delta: Option<i64>,
    keep: f64,
    share: f64,
) -> Result<Vec<String>> {
    use std::fmt::Write as _;
    let unreadable = || {
        EngineError::Backend(format!(
            "work order {}'s planned window is not a strict dateTime window",
            wo.iri
        ))
    };
    let wo_term = target_iri_term(&wo.iri)?;
    let wo_id = iri_id(&wo.iri, "mrp.workorder").ok_or_else(unreadable)?;
    let clone = format!("<{ID_NAMESPACE}mrp.workorder/{}>", SIM_SPLIT_BASE + wo_id);
    let mut updates = Vec::new();
    let mut facts = String::new();
    if let (Some((start, end)), Some(delta)) = (&wo.window, delta) {
        let shift = |iso: &str| -> Result<String> {
            datetime_literal(&epoch_to_iso(
                iso_to_epoch(iso).ok_or_else(unreadable)? + delta,
            ))
        };
        let _ = write!(
            facts,
            " ; ekg:startedAt {} ; ekg:endedAt {}",
            shift(start)?,
            shift(end)?
        );
    }
    if let Some(duration) = wo.duration {
        let _ = write!(
            facts,
            " ; ekg:duration_expected {}",
            decimal_term(duration * share)
        );
        updates.push(diff_sparql(
            &wo_term,
            &Diff {
                predicate: format!("<{EKG_PREFIX}duration_expected>"),
                object_term: decimal_term(duration * keep),
            },
        ));
    }
    updates.push(format!(
        "PREFIX ekg: <{EKG_PREFIX}>\n\
         INSERT {{ GRAPH <{DIFF_GRAPH}> {{\n\
           {clone} a ?type ; ekg:part_of {lot} ; ekg:performedBy ?wc ; ekg:realizes ?op ;\n\
             ekg:lifecycle_state ?state{facts} .\n\
           {lot} ekg:has_work_order {clone} .\n\
         }} }}\n\
         WHERE {{\n\
           {wo_term} a ?type .\n\
           OPTIONAL {{ {wo_term} ekg:performedBy ?wc }}\n\
           OPTIONAL {{ {wo_term} ekg:realizes ?op }}\n\
           OPTIONAL {{ {wo_term} ekg:lifecycle_state ?state }}\n\
         }}"
    ));
    Ok(updates)
}

/// Read what a projected split needs from the sim repo: the order's quantity and window,
/// and its work orders (windows and durations), ordered by IRI.
async fn split_facts(
    sim: &GraphDbClient,
    target: &str,
) -> Result<(SplitOrder, Vec<SplitWorkOrder>)> {
    let term = target_iri_term(target)?;
    let rows = sim
        .select(&format!(
            "PREFIX ekg: <{EKG_PREFIX}>\n\
             SELECT ?qty ?start ?end WHERE {{ {term} a ekg:ManufacturingOrder ; ekg:product_qty ?qty ; \
             ekg:startedAt ?start ; ekg:endedAt ?end }} LIMIT 1"
        ))
        .await?;
    let row = rows.first().ok_or_else(|| {
        EngineError::Backend(format!(
            "{target} is not a manufacturing order with a quantity and a planned window"
        ))
    })?;
    let get = |key: &str| row.get(key).cloned().unwrap_or_default();
    let order = SplitOrder {
        qty: get("qty").parse().map_err(|_| {
            EngineError::Backend(format!(
                "{target}'s quantity {:?} is not a number",
                get("qty")
            ))
        })?,
        start: get("start"),
        end: get("end"),
    };
    let rows = sim
        .select(&format!(
            "PREFIX ekg: <{EKG_PREFIX}>\n\
             SELECT ?wo ?start ?end ?dur WHERE {{ ?wo a ekg:WorkOrder ; ekg:part_of {term} . \
             OPTIONAL {{ ?wo ekg:startedAt ?start ; ekg:endedAt ?end }} \
             OPTIONAL {{ ?wo ekg:duration_expected ?dur }} }} ORDER BY ?wo"
        ))
        .await?;
    let work_orders = rows
        .iter()
        .filter_map(|row| {
            let value = |key: &str| row.get(key).filter(|v| !v.is_empty()).cloned();
            Some(SplitWorkOrder {
                iri: value("wo")?,
                window: value("start").zip(value("end")),
                duration: value("dur").and_then(|d| d.parse().ok()),
            })
        })
        .collect();
    Ok((order, work_orders))
}

/// Whether a plan changes a structural derivation's inputs in the sim repo: a split (a new
/// lot to fulfil its order) or a capacity raise (the agent's derived capacity). The
/// derivations must then re-run there after the diffs.
fn needs_rederivation(plan: &Plan) -> bool {
    plan.actions.iter().any(|action| {
        matches!(
            action.action_type.0.as_str(),
            "split_production" | "raise_work_center_capacity"
        )
    })
}

/// The current planned window of the work order `target` in the sim repo, `None` when it has
/// no start. `target` is validated before it is interpolated.
async fn current_window(sim: &GraphDbClient, target: &str) -> Result<Option<Window>> {
    let term = target_iri_term(target)?;
    let rows = sim
        .select(&format!(
            "PREFIX ekg: <{EKG_PREFIX}>\n\
             SELECT ?start ?end WHERE {{ {term} ekg:startedAt ?start . OPTIONAL {{ {term} ekg:endedAt ?end }} }} LIMIT 1"
        ))
        .await?;
    Ok(rows.first().and_then(|row| {
        Some(Window {
            start: row.get("start").filter(|v| !v.is_empty())?.clone(),
            end: row.get("end").filter(|v| !v.is_empty()).cloned(),
        })
    }))
}

/// The order of the work order `target` in the sim repo and the ends it holds, by planned start
/// as the graph holds no work-order sequence. `None` when it belongs to no order.
async fn order_ends(sim: &GraphDbClient, target: &str) -> Result<Option<OrderEnds>> {
    let term = target_iri_term(target)?;
    let rows = sim
        .select(&format!(
            "PREFIX ekg: <{EKG_PREFIX}>\n\
             SELECT ?order ?wo WHERE {{ {term} ekg:part_of ?order . ?wo ekg:part_of ?order ; \
             ekg:startedAt ?start }} ORDER BY ?start ?wo"
        ))
        .await?;
    let is_target = |row: Option<&std::collections::BTreeMap<String, String>>| {
        row.and_then(|row| row.get("wo")).map(String::as_str) == Some(target)
    };
    Ok(rows
        .first()
        .and_then(|row| row.get("order"))
        .map(|order| OrderEnds {
            order: order.clone(),
            first: is_target(rows.first()),
            last: is_target(rows.last()),
        }))
}

/// SPARQL UPDATE that moves `ekg:eta` to `object_term` on every open shipment delivering
/// the purchase order of the line `line_term`: the arrival the risk rule prefers over the
/// line's planned date. "Open" is the rule's own test: not completed and not cancelled.
/// Both terms are already validated (see [`diff_of`]).
fn open_shipments_eta_sparql(line_term: &str, object_term: &str) -> String {
    format!(
        "PREFIX ekg: <{EKG_PREFIX}>\n\
         DELETE {{ GRAPH ?g {{ ?ship ekg:eta ?old }} }}\n\
         INSERT {{ GRAPH <{DIFF_GRAPH}> {{ ?ship ekg:eta {object_term} }} }}\n\
         WHERE {{\n\
           ?po ekg:has_po_line {line_term} .\n\
           ?ship a ekg:Shipment ; ekg:delivers ?po .\n\
           FILTER NOT EXISTS {{ ?ship ekg:completed_at ?done }}\n\
           FILTER NOT EXISTS {{ ?ship ekg:transport_status \"cancel\" }}\n\
           OPTIONAL {{ GRAPH ?g {{ ?ship ekg:eta ?old }} }}\n\
         }}"
    )
}

/// SPARQL UPDATE that applies one [`Diff`] inside the sim repo: remove the target's
/// existing value of the touched predicate wherever it lives, then assert the new one in
/// the diff graph, so the sim repo's union carries only the new value.
///
/// `target_term` is the validated bracketed IRI from [`target_iri_term`], so this builder
/// cannot emit an unvalidated target.
fn diff_sparql(target_term: &str, diff: &Diff) -> String {
    let Diff {
        predicate,
        object_term,
    } = diff;
    format!(
        "DELETE {{ GRAPH ?g {{ {target_term} {predicate} ?old }} }}\n\
         INSERT {{ GRAPH <{DIFF_GRAPH}> {{ {target_term} {predicate} {object_term} }} }}\n\
         WHERE {{ OPTIONAL {{ GRAPH ?g {{ {target_term} {predicate} ?old }} }} }}"
    )
}

/// The GraphDB [`Simulator`] driver. Holds a client on the base repo, which it only
/// reads; each `project` builds and hands out its own ephemeral sim repo.
///
/// Every simulator in the process shares one pool of projection permits: a projection
/// holds a permit from before its sim repo is created until after it is dropped, so the
/// server never holds more sim repos than the pool admits. A caller past the limit waits.
/// That cannot deadlock: no caller holds one projection while it asks for another.
pub struct GraphDbSimulator {
    base: Arc<GraphDbClient>,
}

impl GraphDbSimulator {
    /// Build the driver over a GraphDB client pointed at the base repository.
    #[must_use]
    pub fn new(client: GraphDbClient) -> Self {
        Self {
            base: Arc::new(client),
        }
    }

    /// Drop the sim repos on this server that were minted more than `max_age` ago, and
    /// return their ids. It reclaims what a killed process left behind; `max_age` must stay
    /// above the longest time a projection is held, because a younger sim repo may be another
    /// process's live projection. A repo that cannot be dropped is logged and left for the
    /// next sweep.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the server's repositories cannot be listed.
    pub async fn sweep_stale(&self, max_age: Duration) -> Result<Vec<String>> {
        let repo_ids = self.base.repository_ids().await?;
        let mut reclaimed = Vec::new();
        for repo in stale_sim_repos(&repo_ids, SystemTime::now(), max_age) {
            match self.base.drop_repo(&repo).await {
                Ok(()) => reclaimed.push(repo),
                Err(err) => {
                    eprintln!(
                        "warning: simulator: stale scratch repo {repo} was not dropped: {err}"
                    );
                }
            }
        }
        Ok(reclaimed)
    }

    /// Populate the already created sim repo with a copy of the base and apply the plan's
    /// diff. Split out so `project` can drop the repo on any failure here.
    async fn populate_and_diff(&self, sim_repo: &str, plan: &Plan) -> Result<GraphDbClient> {
        let sim = GraphDbClient::new(self.base.base_url(), sim_repo)?;
        // Copy the base world (all named graphs) into the sim repo.
        let dump = self.base.export_nquads().await?;
        sim.import_nquads(&dump).await?;
        // Apply each action's diff in order: a reschedule reads its target's window as the
        // earlier actions left it.
        for action in &plan.actions {
            let updates = if action.action_type.0 == "split_production" {
                let (order, work_orders) = split_facts(&sim, &action.target.0).await?;
                split_diff(action, &order, &work_orders)?
            } else {
                let window = if action.action_type.0 == "reschedule_work_order" {
                    current_window(&sim, &action.target.0).await?
                } else {
                    None
                };
                let mut updates = diff_of(action, window.as_ref())?;
                if action.action_type.0 == "reschedule_work_order"
                    && let Some(ends) = order_ends(&sim, &action.target.0).await?
                {
                    updates.extend(order_diff(action, window.as_ref(), &ends)?);
                }
                updates
            };
            for update in updates {
                sim.update(&update).await?;
            }
        }
        // A split or a capacity raise changes what the structural derivations read (a lot
        // to fulfil its order; the agent's capacity), so they re-run inside the sim repo, as
        // the ingest path derives on the base. Other plans skip it.
        if needs_rederivation(plan) {
            for derivation in crate::ontology::STRUCTURAL_DERIVATIONS {
                sim.update(derivation.sparql).await?;
            }
        }
        Ok(sim)
    }
}

#[async_trait]
impl Simulator for GraphDbSimulator {
    async fn project(&self, plan: &Plan) -> Result<Box<dyn Projection>> {
        // Wait for capacity before the sim repo exists.
        let permit = projection_permits()
            .acquire_owned()
            .await
            .map_err(|e| EngineError::Backend(format!("projection permits closed: {e}")))?;
        // The guard owns the repo before the create request is sent: from here on, a
        // dropped future drops the repo.
        let mut scratch = ScratchRepo {
            base: Arc::clone(&self.base),
            repo: sim_repo_id(),
            permit: Some(permit),
            creating: None,
        };
        let built = match scratch.create().await {
            Ok(()) => self.populate_and_diff(&scratch.repo, plan).await,
            Err(err) => Err(err),
        };
        match built {
            Ok(sim) => Ok(Box::new(GraphDbProjection {
                sim: Arc::new(sim),
                _scratch: scratch,
            })),
            Err(err) => {
                // A failed setup drops the repo before the caller sees the error.
                scratch.discard().await;
                Err(err)
            }
        }
    }
}

/// A handle over one ephemeral projection repo. A caller may run several rules against
/// it. Dropping it drops the sim repo, then releases its projection permit (through its
/// scratch-repo guard).
pub struct GraphDbProjection {
    sim: Arc<GraphDbClient>,
    _scratch: ScratchRepo,
}

#[async_trait]
impl Projection for GraphDbProjection {
    async fn select(
        &self,
        sparql: &str,
    ) -> Result<Vec<std::collections::BTreeMap<String, String>>> {
        // The whole sim repo is the projection, so the rule runs against it as is (its
        // default graph is the union of the copied world).
        self.sim.select(sparql).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use ekg_domain::{ActionType, ProposedAction};

    use super::{OrderEnds, Window, diff_of, order_diff, sim_repo_id, target_iri_term, verb_write};

    /// An action on a target of the verb's own model (id 7).
    fn action(verb: &str, params: &[(&str, &str)]) -> ProposedAction {
        let model = verb_write(verb).map_or("mrp.workorder", |w| w.model);
        action_on(verb, &format!("https://ekg.example/id/{model}/7"), params)
    }

    fn action_on(verb: &str, target: &str, params: &[(&str, &str)]) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: ekg_domain::EntityUri(target.to_owned()),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// The single update a single-field verb makes.
    fn only_update(a: &ProposedAction) -> String {
        let mut updates = diff_of(a, None).expect("mapping");
        assert_eq!(updates.len(), 1, "one field, one update: {updates:?}");
        updates.remove(0)
    }

    #[test]
    fn the_permit_pool_admits_at_most_its_limit() {
        // Past the limit a projection waits; a released permit admits the next one.
        let pool = super::permit_pool(std::num::NonZeroUsize::new(2).expect("non-zero"));
        let first = Arc::clone(&pool).try_acquire_owned().expect("first");
        let _second = Arc::clone(&pool).try_acquire_owned().expect("second");
        assert!(
            Arc::clone(&pool).try_acquire_owned().is_err(),
            "a third must wait"
        );
        drop(first);
        assert!(
            Arc::clone(&pool).try_acquire_owned().is_ok(),
            "a release admits the next"
        );
    }

    #[test]
    fn only_a_sim_repo_older_than_the_maximum_age_is_stale() {
        use std::time::{Duration, UNIX_EPOCH};

        use super::stale_sim_repos;

        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let minted = |secs_ago: u64| {
            let at = Duration::from_secs(10_000 - secs_ago).as_nanos();
            format!("sim_4242_{at}_7")
        };
        let stale = minted(901);
        let ids = vec![
            stale.clone(),
            minted(900),                    // exactly the maximum age: not yet stale
            minted(5),                      // a live projection of another process
            "sim_4242_soon_7".to_owned(),   // not a mint time
            "sim_4242_1000".to_owned(),     // too few parts
            "sim_4242_1000_7_x".to_owned(), // too many parts
            "sim_fixture".to_owned(),       // someone else's `sim_` repo
            "ekg".to_owned(),               // the world model's own repository
        ];
        assert_eq!(
            stale_sim_repos(&ids, now, Duration::from_secs(900)),
            vec![stale]
        );
    }

    #[test]
    fn a_minted_sim_repo_id_is_young_until_its_age_passes() {
        use std::time::{Duration, SystemTime};

        use super::stale_sim_repos;

        let ids = vec![sim_repo_id()];
        let now = SystemTime::now();
        assert!(stale_sim_repos(&ids, now, Duration::from_secs(900)).is_empty());
        assert_eq!(
            stale_sim_repos(
                &ids,
                now + Duration::from_secs(901),
                Duration::from_secs(900)
            ),
            ids
        );
    }

    #[test]
    fn sim_repo_ids_are_prefixed_and_unique() {
        let a = sim_repo_id();
        let b = sim_repo_id();
        assert!(a.starts_with("sim_"), "sweepable, repo-id-safe prefix: {a}");
        assert!(
            !a.contains(':') && !a.contains('/'),
            "must be a valid repo id: {a}"
        );
        assert_ne!(a, b, "each projection gets its own repo");
    }

    #[test]
    fn reschedule_diff_removes_old_and_asserts_new_typed_datetime() {
        let a = action(
            "reschedule_work_order",
            &[("new_date", "2027-10-11 08:00:00")],
        );
        let sql = only_update(&a);
        assert!(sql.contains("DELETE") && sql.contains("INSERT") && sql.contains("WHERE"));
        assert!(sql.contains("startedAt"));
        // Old value removed across graphs; new asserted in the diff graph.
        assert!(sql.contains("GRAPH ?g"));
        assert!(sql.contains("GRAPH <urn:sim:diff>"));
        // Encoded to ISO xsd:dateTime so the rule's dateTime comparison holds.
        assert!(
            sql.contains("\"2027-10-11T08:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>")
        );
        assert!(sql.contains("<https://ekg.example/id/mrp.workorder/7>"));
    }

    #[test]
    fn a_reschedule_moves_the_whole_window_keeping_its_duration() {
        // 17 Oct 08:00 to 12:00 rescheduled to 19 Oct 08:00 ends at 19 Oct 12:00, as the
        // write-back sets date_start and date_finished together.
        let a = action(
            "reschedule_work_order",
            &[("new_date", "2027-10-25 08:00:00")],
        );
        let window = Window {
            start: "2027-10-23T08:00:00".to_owned(),
            end: Some("2027-10-23T12:00:00".to_owned()),
        };
        let updates = diff_of(&a, Some(&window)).expect("mapping");
        assert_eq!(updates.len(), 2, "{updates:?}");
        let dt = |v: &str| format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#dateTime>");
        assert!(updates[0].contains("<https://ekg.example/ontology#startedAt>"));
        assert!(updates[0].contains(&dt("2027-10-25T08:00:00")));
        assert!(updates[1].contains("<https://ekg.example/ontology#endedAt>"));
        assert!(updates[1].contains(&dt("2027-10-25T12:00:00")));
        // Across a month end, and moving earlier.
        let window = Window {
            start: "2027-11-06T22:00:00".to_owned(),
            end: Some("2027-11-07T02:00:00".to_owned()),
        };
        let earlier = action(
            "reschedule_work_order",
            &[("new_date", "2027-11-05 22:00:00")],
        );
        let updates = diff_of(&earlier, Some(&window)).expect("mapping");
        assert!(
            updates[1].contains(&dt("2027-11-06T02:00:00")),
            "{updates:?}"
        );
    }

    #[test]
    fn the_order_follows_only_the_work_orders_at_its_ends() {
        let a = action(
            "reschedule_work_order",
            &[("new_date", "2027-10-25 08:00:00")],
        );
        let window = Window {
            start: "2027-10-23T08:00:00".to_owned(),
            end: Some("2027-10-23T12:00:00".to_owned()),
        };
        let ends = |first, last| OrderEnds {
            order: "https://ekg.example/id/mrp.production/2".to_owned(),
            first,
            last,
        };
        let moved = |first, last| order_diff(&a, Some(&window), &ends(first, last)).expect("diff");
        assert!(moved(false, false).is_empty());
        let start = moved(true, false);
        assert_eq!(start.len(), 1);
        assert!(start[0].contains("startedAt"));
        let finish = moved(false, true);
        assert_eq!(finish.len(), 1);
        assert!(finish[0].contains("endedAt"));
        assert!(finish[0].contains("2027-10-25T12:00:00"), "{finish:?}");
    }

    #[test]
    fn a_reschedule_with_no_end_moves_the_start_only() {
        let a = action(
            "reschedule_work_order",
            &[("new_date", "2027-10-25 08:00:00")],
        );
        let window = Window {
            start: "2027-10-23T08:00:00".to_owned(),
            end: None,
        };
        assert_eq!(diff_of(&a, Some(&window)).expect("mapping").len(), 1);
        // An unreadable window is an error, not a projection with a wrong end.
        let bad = Window {
            start: "not a date".to_owned(),
            end: Some("2027-10-23T12:00:00".to_owned()),
        };
        assert!(diff_of(&a, Some(&bad)).is_err());
    }

    #[test]
    fn reassign_diff_rewrites_performed_by_as_workcenter_iri() {
        let a = action("reassign_work_center", &[("new_work_center", "5")]);
        let sql = only_update(&a);
        assert!(sql.contains("performedBy"));
        assert!(sql.contains("<https://ekg.example/id/mrp.workcenter/5>"));
    }

    #[test]
    fn reassign_accepts_a_workcenter_iri_param() {
        let a = action(
            "reassign_work_center",
            &[("new_work_center", "https://ekg.example/id/mrp.workcenter/9")],
        );
        assert!(only_update(&a).contains("mrp.workcenter/9"));
    }

    #[test]
    fn notify_owner_makes_no_world_model_change() {
        let a = action("notify_owner", &[("message", "hi")]);
        assert!(diff_of(&a, None).expect("mapping").is_empty());
    }

    #[test]
    fn re_promise_date_diff_rewrites_commitment_date_as_typed_datetime() {
        // The clearing supply-risk verb: moving the promise later must project onto
        // ekg:commitment_date as an xsd:dateTime the risk rule can compare.
        let a = action("re_promise_date", &[("new_date", "2027-02-01 00:00:00")]);
        let sql = only_update(&a);
        assert!(sql.contains("<https://ekg.example/id/sale.order/7>"));
        assert!(sql.contains("commitment_date"));
        assert!(
            sql.contains("\"2027-02-01T00:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>")
        );
    }

    #[test]
    fn expedite_po_moves_the_line_and_its_open_shipments_arrival() {
        // The projection mirrors the executor: the line's planned date and the ETA of
        // every open shipment delivering its purchase order, and no other fact.
        let a = action_on(
            "expedite_po",
            "https://ekg.example/id/purchase.order.line/40",
            &[("new_date", "2027-10-12 08:00:00")],
        );
        let updates = diff_of(&a, None).expect("mapping");
        assert_eq!(updates.len(), 2, "{updates:?}");
        let new = "\"2027-10-12T08:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>";

        let line = &updates[0];
        assert!(line.contains("<https://ekg.example/id/purchase.order.line/40>"));
        assert!(line.contains("<https://ekg.example/ontology#date_planned>"));
        assert!(line.contains(new));

        let shipments = &updates[1];
        assert!(
            shipments.contains("ekg:has_po_line <https://ekg.example/id/purchase.order.line/40>")
        );
        assert!(shipments.contains("?ship a ekg:Shipment ; ekg:delivers ?po"));
        assert!(shipments.contains("ekg:eta ?old"));
        assert!(shipments.contains(&format!("?ship ekg:eta {new}")));
        // The rule's open test: a completed or cancelled shipment is left alone.
        assert!(shipments.contains("FILTER NOT EXISTS { ?ship ekg:completed_at ?done }"));
        assert!(shipments.contains("FILTER NOT EXISTS { ?ship ekg:transport_status \"cancel\" }"));
    }

    #[test]
    fn a_remediation_on_the_wrong_model_is_refused_as_the_executor_refuses_it() {
        for (verb, target) in [
            ("expedite_po", "https://ekg.example/id/sale.order/1"),
            ("expedite_po", "https://ekg.example/id/purchase.order/1"),
            (
                "re_promise_date",
                "https://ekg.example/id/purchase.order.line/1",
            ),
        ] {
            let a = action_on(verb, target, &[("new_date", "2027-10-12 08:00:00")]);
            assert!(
                diff_of(&a, None).is_err(),
                "{verb} on {target} must be refused"
            );
        }
    }

    #[test]
    fn a_missing_new_date_is_refused() {
        let a = action("expedite_po", &[]);
        assert!(diff_of(&a, None).is_err());
    }

    #[test]
    fn a_re_promise_date_that_could_break_out_of_the_sparql_is_rejected() {
        // The supply-risk verbs share the strict dateTime validation, so an injected or
        // out-of-range new_date is rejected rather than interpolated.
        let injection = "2027-01-01 00:00:00\"}}; DROP GRAPH <urn:x>; INSERT DATA { #";
        let a = action("re_promise_date", &[("new_date", injection)]);
        assert!(
            diff_of(&a, None).is_err(),
            "an un-strict new_date must be rejected"
        );
        let a = action("re_promise_date", &[("new_date", "2027-13-45T00:00:00")]);
        assert!(
            diff_of(&a, None).is_err(),
            "out-of-range dateTime must be rejected"
        );
    }

    #[test]
    fn an_unmapped_verb_fails_loud() {
        let a = action("delete_everything", &[]);
        assert!(diff_of(&a, None).is_err(), "never silently mis-project");
    }

    #[test]
    fn a_reschedule_new_date_that_could_break_out_of_the_sparql_is_rejected() {
        // `new_date` is interpolated into a repository-authority UPDATE; a value that
        // closes the literal and appends operations must never reach the store.
        let injection = "2027-01-07 00:00:00\"}}; DROP GRAPH <urn:x>; INSERT DATA { #";
        let a = action("reschedule_work_order", &[("new_date", injection)]);
        assert!(
            diff_of(&a, None).is_err(),
            "an un-strict new_date must be rejected, not interpolated"
        );
        // A malformed-but-quiet value (wrong shape, no metacharacters) is also rejected.
        let a = action("reschedule_work_order", &[("new_date", "next tuesday")]);
        assert!(diff_of(&a, None).is_err());
        // Digit-correct but out of range must be an error, not an ill-typed literal (whose
        // comparison error would read as a false "cleared").
        let a = action(
            "reschedule_work_order",
            &[("new_date", "2027-13-45T99:00:00")],
        );
        assert!(
            diff_of(&a, None).is_err(),
            "out-of-range dateTime must be rejected"
        );
    }

    #[test]
    fn a_calendar_invalid_new_date_is_rejected() {
        // In-range digits but not a real calendar day: still an ill-typed xsd:dateTime.
        for bad in [
            "2027-02-30T00:00:00", // February never has 30
            "2027-04-31T00:00:00", // April has 30
            "2027-02-29T00:00:00", // 2027 is not a leap year
        ] {
            let a = action("reschedule_work_order", &[("new_date", bad)]);
            assert!(
                diff_of(&a, None).is_err(),
                "calendar-invalid date must be rejected: {bad}"
            );
        }
        // A real leap day is accepted (2028 is a leap year).
        let a = action(
            "reschedule_work_order",
            &[("new_date", "2028-02-29T00:00:00")],
        );
        assert!(
            !diff_of(&a, None).expect("mapping").is_empty(),
            "a real leap day is valid"
        );
    }

    #[test]
    fn reassign_rejects_a_non_workcentre_iri_instead_of_coercing_it() {
        // A work-order IRI passed as new_work_center must be an error, not become a work
        // centre with the same id.
        let a = action(
            "reassign_work_center",
            &[("new_work_center", "https://ekg.example/id/mrp.workorder/9")],
        );
        assert!(
            diff_of(&a, None).is_err(),
            "a non-work-centre IRI must be rejected"
        );
    }

    #[test]
    fn a_strict_iso_or_odoo_space_new_date_is_accepted() {
        for good in ["2027-10-11T12:00:00", "2027-10-11 12:00:00"] {
            let a = action("reschedule_work_order", &[("new_date", good)]);
            assert!(only_update(&a).contains("2027-10-11T12:00:00"));
        }
    }

    #[test]
    fn a_target_that_could_break_out_of_the_iri_is_rejected() {
        // The target is interpolated inside `<…>` in the same authority-scoped UPDATE.
        for bad in [
            "https://ekg.example/id/mrp.workorder/7> ; DROP GRAPH <urn:x>",
            "https://ekg.example/id/mrp.work>order/7",
            "https://evil.example/id/mrp.workorder/7",
            "https://ekg.example/id/mrp.workorder/not-a-number",
        ] {
            assert!(
                target_iri_term(bad).is_err(),
                "must reject a malformed/hostile target: {bad}"
            );
        }
        assert_eq!(
            target_iri_term("https://ekg.example/id/mrp.workorder/7").expect("valid"),
            "<https://ekg.example/id/mrp.workorder/7>"
        );
    }

    // --- split and capacity-raise projections ---

    #[test]
    fn a_capacity_raise_rewrites_the_records_capacity() {
        let raise = action("raise_work_center_capacity", &[("new_capacity", "2")]);
        let update = only_update(&raise);
        assert!(
            update.contains("<https://ekg.example/id/mrp.workcenter.capacity/7>"),
            "{update}"
        );
        assert!(
            update.contains("<https://ekg.example/ontology#capacity>"),
            "{update}"
        );
        assert!(
            update.contains("\"2\"^^<http://www.w3.org/2001/XMLSchema#decimal>"),
            "{update}"
        );
        assert!(super::needs_rederivation(&ekg_domain::Plan {
            actions: vec![raise]
        }));
    }

    #[test]
    fn a_capacity_raise_refuses_a_wrong_model_or_a_malformed_capacity() {
        let wc = action_on(
            "raise_work_center_capacity",
            "https://ekg.example/id/mrp.workcenter/7",
            &[("new_capacity", "2")],
        );
        assert!(diff_of(&wc, None).is_err(), "the agent is not the record");
        for bad in ["0", "-1", "two", "NaN", "2\"} ; DROP ALL ; #"] {
            let raise = action("raise_work_center_capacity", &[("new_capacity", bad)]);
            assert!(diff_of(&raise, None).is_err(), "capacity {bad:?}");
        }
    }

    fn order() -> super::SplitOrder {
        super::SplitOrder {
            qty: 40.0,
            start: "2027-10-22T08:00:00".to_owned(),
            end: "2027-10-23T08:00:00".to_owned(),
        }
    }

    fn work_orders() -> Vec<super::SplitWorkOrder> {
        vec![
            super::SplitWorkOrder {
                iri: "https://ekg.example/id/mrp.workorder/11".to_owned(),
                window: Some((
                    "2027-10-22T08:00:00".to_owned(),
                    "2027-10-22T12:00:00".to_owned(),
                )),
                duration: Some(2400.0),
            },
            super::SplitWorkOrder {
                iri: "https://ekg.example/id/mrp.workorder/12".to_owned(),
                window: Some((
                    "2027-10-22T12:00:00".to_owned(),
                    "2027-10-22T14:00:00".to_owned(),
                )),
                duration: None,
            },
        ]
    }

    fn split(params: &[(&str, &str)]) -> ProposedAction {
        action_on(
            "split_production",
            "https://ekg.example/id/mrp.production/7",
            params,
        )
    }

    #[test]
    fn a_split_reduces_the_order_and_inserts_a_lot_with_shifted_work_orders() {
        let updates = super::split_diff(
            &split(&[
                ("split_quantity", "10"),
                ("new_date", "2027-10-24 08:00:00"),
            ]),
            &order(),
            &work_orders(),
        )
        .expect("split");
        let all = updates.join("\n");
        let decimal = |v: &str| format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#decimal>");
        let datetime = |v: &str| format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#dateTime>");
        // The order keeps 30.
        assert!(updates[0].contains("<https://ekg.example/id/mrp.production/7>"));
        assert!(updates[0].contains(&decimal("30")), "{}", updates[0]);
        // The lot: synthetic id, 10, from the new date, the order's length kept.
        let lot = "<https://ekg.example/id/mrp.production/1000000000007>";
        assert!(all.contains(&format!("{lot} a ?type")), "{all}");
        assert!(
            all.contains(&format!("ekg:product_qty {}", decimal("10"))),
            "{all}"
        );
        assert!(all.contains(&format!(
            "ekg:startedAt {}",
            datetime("2027-10-24T08:00:00")
        )));
        assert!(all.contains(&format!("ekg:endedAt {}", datetime("2027-10-25T08:00:00"))));
        assert!(
            all.contains("ekg:origin_ref ?origin"),
            "the lot fulfils the same order"
        );
        // The clones: shifted by one delta, lengths and offsets kept, same agent.
        let c11 = "<https://ekg.example/id/mrp.workorder/1000000000011>";
        let c12 = "<https://ekg.example/id/mrp.workorder/1000000000012>";
        assert!(all.contains(&format!(
            "{c11} a ?type ; ekg:part_of {lot} ; ekg:performedBy ?wc"
        )));
        assert!(all.contains(&format!(
            "ekg:startedAt {} ; ekg:endedAt {}",
            datetime("2027-10-24T08:00:00"),
            datetime("2027-10-24T12:00:00")
        )));
        assert!(all.contains(&format!(
            "ekg:startedAt {} ; ekg:endedAt {}",
            datetime("2027-10-24T12:00:00"),
            datetime("2027-10-24T14:00:00")
        )));
        assert!(all.contains(&format!("{lot} ekg:has_work_order {c12}")));
        // Durations follow Odoo's rescaling: 2400 becomes 1800 on the order, 600 on the clone.
        assert!(all.contains(&format!("ekg:duration_expected {}", decimal("600"))));
        assert!(all.contains(&decimal("1800")), "{all}");
    }

    #[test]
    fn a_split_refuses_a_wrong_model_or_a_malformed_param() {
        let wo = action_on(
            "split_production",
            "https://ekg.example/id/mrp.workorder/7",
            &[
                ("split_quantity", "10"),
                ("new_date", "2027-10-24 08:00:00"),
            ],
        );
        assert!(super::split_diff(&wo, &order(), &work_orders()).is_err());
        for params in [
            vec![("split_quantity", "0"), ("new_date", "2027-10-24 08:00:00")],
            vec![
                ("split_quantity", "40"),
                ("new_date", "2027-10-24 08:00:00"),
            ],
            vec![
                ("split_quantity", "ten"),
                ("new_date", "2027-10-24 08:00:00"),
            ],
            vec![("split_quantity", "10"), ("new_date", "18 Oct")],
            vec![("split_quantity", "10")],
        ] {
            assert!(
                super::split_diff(&split(&params), &order(), &work_orders()).is_err(),
                "{params:?}"
            );
        }
    }

    #[test]
    fn only_a_split_or_a_capacity_raise_re_derives() {
        let plan = |verb: &str| ekg_domain::Plan {
            actions: vec![action(verb, &[])],
        };
        assert!(super::needs_rederivation(&plan("split_production")));
        assert!(!super::needs_rederivation(&plan("reschedule_work_order")));
        assert!(!super::needs_rederivation(&plan("expedite_po")));
    }
}
