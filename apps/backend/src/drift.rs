//! Capability wiring for the drift monitor: the concrete risk and coupling rules, and the
//! bridge from the `RdfChanged` fan-out to the durable monitor.
//!
//! The rules live here, not in `ekg-dynamic`, because the dynamic layer is
//! capability-neutral and is handed its rules. The SPARQL is authored in the semantic layer
//! ([`ekg_semantic::ontology`]); this module only maps solution rows to verdicts.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_nats::jetstream;
use async_trait::async_trait;
use ekg_domain::{
    ActionExecutor, ActionPointId, ActionPointState, CorrelationId, EngineError, EntityUri,
    GroundedProposal, GroupSlateProposer, Plan, PromotedPlan, Promotion, Proposer, RdfChanged,
    ReviewerId, Revision, Slate, SlateProposer, WorldModel,
};
use ekg_dynamic::{
    ClaimCommand, CouplingEdge, CouplingRule, DriftVerdict, MEMBER_AUTO_PROMOTE_REFUSAL,
    NotifyOutcome, RiskRule, Row,
};
use ekg_kinetic::{PolicyEngine, PromotionTier};
use ekg_semantic::{display, ontology};

use crate::command::{CommandLedger, Failure, IngressLedger, settle, write_back_problem};
use crate::policy_context;
use futures::StreamExt;
use tokio::sync::broadcast::{self, error::RecvError};

/// The registered Restate service name of the drift monitor (its struct name).
const MONITOR_SERVICE: &str = "DriftMonitorService";
/// The monitor handler the bridge invokes.
const MONITOR_HANDLER: &str = "notify";
/// The durable JetStream consumer name for the monitor bridge. Fixed so a restart
/// resumes the cursor instead of replaying the whole stream.
const MONITOR_CONSUMER: &str = "ekg-monitor-bridge";

/// The capability's rule set, built once and shared: the drift monitor evaluates it on every
/// change, and the read-model tabs evaluate the same set for their at-risk markers, so a
/// tab reflects what the monitor would decide.
///
/// Rule order is a priority: when one order drifts on several axes, the monitor coalesces
/// to a single action point carrying the first verdict's reason. Infeasibility precedes
/// contention so a missed customer promise is surfaced ahead of an internal resource
/// conflict, and a short sales order keeps its supply reason over a missed promise. Order
/// affects only the reason a coalesced point carries, not detection.
#[must_use]
pub fn monitor_rules() -> Arc<[RiskRule]> {
    Arc::from(vec![
        supply_risk_rule(),
        missed_promise_rule(),
        process_infeasibility_rule(),
        process_contention_rule(),
    ])
}

/// The capability's coupling rules, injected into the monitor alongside [`monitor_rules`]:
/// a sales order is coupled to each manufacturing order that fulfils it, and two
/// manufacturing orders are coupled when they contend for a work center beyond its capacity.
/// The monitor groups the coupled points awaiting oversight into portfolios.
#[must_use]
pub fn coupling_rules() -> Arc<[CouplingRule]> {
    Arc::from(vec![coupling_rule()])
}

/// The concrete coupling rule: [`ontology::COUPLING_SELECT_SPARQL`] interpreted by
/// [`interpret_coupling_row`].
#[must_use]
pub fn coupling_rule() -> CouplingRule {
    CouplingRule::new(ontology::COUPLING_SELECT_SPARQL, interpret_coupling_row)
}

/// Map one coupling row to an undirected edge.
///
/// - `fulfills` always couples the sales order `?a` and the manufacturing order `?b`;
/// - `contention` couples the two orders only when the anchor is over its work center's
///   capacity, by [`ontology::over_capacity`], the predicate the contention rule and the
///   scorer also use. Orders sharing a center within capacity stay apart.
///
/// Each edge records its facts beside the reason: its kind and, for a contention, the work
/// center and its capacity. [`display::describe_coupling`] words a portfolio's coupling
/// from them.
///
/// `None` for any other kind, a missing or malformed binding, or a within-capacity row.
fn interpret_coupling_row(row: &Row) -> Option<CouplingEdge> {
    let a = EntityUri(row.get("a")?.clone());
    let b = EntityUri(row.get("b")?.clone());
    let mut facts = BTreeMap::new();
    let reason = match row.get("kind")?.as_str() {
        display::COUPLING_FULFILLS => {
            facts.insert(
                display::COUPLING_KIND_KEY.to_owned(),
                display::COUPLING_FULFILLS.to_owned(),
            );
            format!("linked: {b} fulfils {a}")
        }
        display::COUPLING_CONTENTION => {
            let via = row.get("via")?;
            let capacity: f64 = row.get("capacity")?.parse().ok()?;
            let overlap_peers: i64 = row.get("overlapPeers")?.parse().ok()?;
            if !ontology::over_capacity(capacity, overlap_peers)? {
                return None;
            }
            // The query yields each contending pair from both anchors. Word the reason over
            // the ordered ends so both rows give the same edge.
            let (lo, hi) = if a <= b { (&a, &b) } else { (&b, &a) };
            facts.extend([
                (
                    display::COUPLING_KIND_KEY.to_owned(),
                    display::COUPLING_CONTENTION.to_owned(),
                ),
                (display::COUPLING_VIA_KEY.to_owned(), via.clone()),
                (
                    display::COUPLING_CAPACITY_KEY.to_owned(),
                    capacity.to_string(),
                ),
            ]);
            format!(
                "linked: {lo} and {hi} contend for work center {via} beyond its capacity {capacity}"
            )
        }
        _ => return None,
    };
    CouplingEdge::new(a, b, reason).map(|edge| edge.with_facts(facts))
}

/// The supply-risk rule.
///
/// Every row the SELECT returns is already at risk (its `WHERE` filters
/// `shortfall > 0`), so each row maps to a verdict keyed by the sales order
/// (`?so`), with a reason naming the short component and shortfall.
#[must_use]
pub fn supply_risk_rule() -> RiskRule {
    RiskRule::new(ontology::RISK_RULE_SELECT_SPARQL, interpret_risk_row)
}

/// Map one at-risk solution row to a verdict. `None` only if the row lacks the
/// keying bindings.
///
/// When the shortfall is attributable to a delivering shipment whose ETA slipped
/// past the commitment (`?lateShip` = 1 from the rule), the reason names the
/// transport delay; otherwise it is the standard availability shortfall.
fn interpret_risk_row(row: &Row) -> Option<DriftVerdict> {
    let subject = row.get("so")?;
    let shortfall = row.get("shortfall")?;
    let component = row
        .get("comp")
        .map_or("a required component", String::as_str);
    let transport_delayed = row.get("lateShip").is_some_and(|v| v == "1" || v == "true");
    let reason = if transport_delayed {
        format!(
            "delivery promise at risk: inbound shipment for {component} is delayed past the commitment date (short by {shortfall})"
        )
    } else {
        format!(
            "delivery promise at risk: {component} short by {shortfall} before the commitment date"
        )
    };
    Some(DriftVerdict {
        subject: EntityUri(subject.clone()),
        reason,
    })
}

/// The process resource-contention rule. The SELECT
/// ([`ekg_semantic::ontology::PROCESS_CONTENTION_SELECT_SPARQL`]) projects, per
/// contended anchor work order, its agent and derived capacity, its `part_of`
/// manufacturing order, and the count of overlapping peers. The interpreter flags
/// the order when concurrency exceeds capacity.
#[must_use]
pub fn process_contention_rule() -> RiskRule {
    RiskRule::new(
        ontology::PROCESS_CONTENTION_SELECT_SPARQL,
        interpret_contention_row,
    )
}

/// Map one contention row to a verdict. Concurrency is the overlapping peers plus
/// the anchor work order itself; a verdict is raised only when it exceeds the
/// agent's capacity (a capacity-2 agent running two overlapping jobs is fine, a
/// third is contention). The subject is the affected manufacturing order; the
/// reason names the agent, the contended work orders, and the window. `None` when
/// the keying bindings are missing or capacity is not exceeded.
fn interpret_contention_row(row: &Row) -> Option<DriftVerdict> {
    let mo = row.get("mo")?;
    let agent = row.get("wc")?;
    let capacity: f64 = row.get("capacity")?.parse().ok()?;
    let overlap_peers: i64 = row.get("overlapPeers")?.parse().ok()?;
    // The same predicate the slate scorer uses.
    if !ontology::over_capacity(capacity, overlap_peers)? {
        return None; // within capacity: the agent can run them in parallel.
    }
    let concurrency = overlap_peers + 1;
    let anchor = row.get("wo").map_or("a work order", String::as_str);
    let peers = row.get("peers").map_or("", String::as_str);
    let window = match (row.get("woStart"), row.get("woEnd")) {
        (Some(start), Some(end)) => format!(" over the window {start}–{end}"),
        _ => String::new(),
    };
    let reason = format!(
        "resource contention: work center {agent} is over capacity ({concurrency} work orders overlap, capacity {capacity}); \
         {anchor} contends with {peers}{window}"
    );
    Some(DriftVerdict {
        subject: EntityUri(mo.clone()),
        reason,
    })
}

/// The process schedule-infeasibility rule. The SELECT
/// ([`ekg_semantic::ontology::PROCESS_INFEASIBILITY_SELECT_SPARQL`]) projects, per
/// manufacturing order, the summed operation duration, the planned window, and the
/// fulfilled `commitment`. The interpreter flags the order when its planned finish
/// (or earliest completion) falls after the commitment it fulfils, or when its
/// routing cannot fit the planned window.
#[must_use]
pub fn process_infeasibility_rule() -> RiskRule {
    RiskRule::new(
        ontology::PROCESS_INFEASIBILITY_SELECT_SPARQL,
        interpret_infeasibility_row,
    )
}

/// Map one infeasibility row to a verdict. Two independent triggers:
///
/// 1. Misses the commitment: the order's completion (its planned finish `endedAt`
///    when scheduled, else `startedAt` plus the summed duration) is after the
///    `commitment_date` it fulfils.
/// 2. Planned window too short: the summed routing duration exceeds the planned
///    window `endedAt - startedAt`.
///
/// The source scheduler fits `endedAt` to the routing, so the second trigger rarely
/// fires on source data; it catches an internally inconsistent plan. `None` when
/// neither trigger holds or the keying bindings are missing.
fn interpret_infeasibility_row(row: &Row) -> Option<DriftVerdict> {
    infeasibility_of(row).map(|(_, verdict)| verdict)
}

/// The missed-promise rule: the infeasibility SELECT again, flagging the sales order whose
/// promise a manufacturing order misses. Without it only the build is flagged, so the order
/// never joins its portfolio and no plan may move its promise.
#[must_use]
pub fn missed_promise_rule() -> RiskRule {
    RiskRule::new(
        ontology::PROCESS_INFEASIBILITY_SELECT_SPARQL,
        interpret_missed_promise_row,
    )
}

/// Map one infeasibility row to a verdict on its sales order `?so`, on the
/// missed-commitment trigger only: a routing over its window is the build's own problem.
fn interpret_missed_promise_row(row: &Row) -> Option<DriftVerdict> {
    let (Infeasibility::MissesCommitment, _) = infeasibility_of(row)? else {
        return None;
    };
    let so = row.get("so")?;
    let mo = row.get("mo")?;
    let commitment = row.get("commitment").map_or("", String::as_str);
    let completion = if let Some(finish) = row.get("endedAt") {
        format!("is planned to finish {finish}")
    } else {
        let required = row.get("required").map_or("0", String::as_str);
        let start = row.get("startedAt").map_or("", String::as_str);
        format!("needs {required} min from its start {start}")
    };
    Some(DriftVerdict {
        subject: EntityUri(so.clone()),
        reason: format!(
            "delivery promise at risk: manufacturing order {mo} {completion}, after the commitment {commitment}"
        ),
    })
}

/// Which of the two triggers an infeasibility row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Infeasibility {
    /// The order completes after the commitment it fulfils.
    MissesCommitment,
    /// Its routing does not fit its planned window.
    OutgrowsWindow,
}

/// The trigger an infeasibility row holds and the verdict it raises.
#[must_use]
pub fn infeasibility_of(row: &Row) -> Option<(Infeasibility, DriftVerdict)> {
    let mo = row.get("mo")?;
    // Summed routing minutes; absent or 0 when the BOM has no routing operations (the
    // routing block is OPTIONAL). Trigger 1 does not need it, so an order without a
    // routing is still assessed; trigger 2 stays inert at 0.
    let required: f64 = row
        .get("required")
        .and_then(|r| r.parse().ok())
        .unwrap_or(0.0);
    let start = to_f64_minutes(parse_iso_minutes(row.get("startedAt")?)?)?;
    let ended = row
        .get("endedAt")
        .and_then(|e| parse_iso_minutes(e))
        .and_then(to_f64_minutes);
    let commitment = row
        .get("commitment")
        .and_then(|c| parse_iso_minutes(c))
        .and_then(to_f64_minutes);

    // Trigger 1: the order cannot complete by the promise it fulfils. Completion is
    // the planned finish when scheduled, else the earliest finish from the routing.
    if let Some(commit) = commitment {
        let completion = ended.unwrap_or(start + required);
        if completion > commit {
            let commit_str = row.get("commitment").map_or("", String::as_str);
            let reason = if let Some(finish) = row.get("endedAt") {
                format!(
                    "schedule infeasibility: planned to finish {finish}, after the commitment {commit_str}"
                )
            } else {
                let start_str = row.get("startedAt").map_or("", String::as_str);
                format!(
                    "schedule infeasibility: earliest completion is {required} min after the start {start_str}, overrunning the commitment {commit_str}"
                )
            };
            return Some((
                Infeasibility::MissesCommitment,
                DriftVerdict {
                    subject: EntityUri(mo.clone()),
                    reason,
                },
            ));
        }
    }

    // Trigger 2: the routing cannot fit the planned window.
    if let Some(end) = ended {
        let window = end - start;
        if required > window {
            let reason = format!(
                "schedule infeasibility: routing needs {required} min but the planned window is only {window} min"
            );
            return Some((
                Infeasibility::OutgrowsWindow,
                DriftVerdict {
                    subject: EntityUri(mo.clone()),
                    reason,
                },
            ));
        }
    }
    None
}

/// Convert whole minutes to `f64` losslessly via `i32` (minutes fit in `i32` for any
/// realistic date), so the comparisons avoid a lossy `as` cast.
fn to_f64_minutes(minutes: i64) -> Option<f64> {
    i32::try_from(minutes).ok().map(f64::from)
}

/// Parse an ISO-8601 `xsd:dateTime` lexical form (`YYYY-MM-DDTHH:MM:SS`, the shape
/// the mapping layer normalises Odoo datetimes to) into whole minutes since the
/// Unix epoch. Only differences are used. Anything after the minutes, including a
/// timezone suffix, is ignored; no pipeline value carries a timezone, so differences
/// stay consistent. `None` on a malformed value.
fn parse_iso_minutes(value: &str) -> Option<i64> {
    // Split date and time on the ISO 'T', or on a space.
    let (date, time) = value.split_once('T').or_else(|| value.split_once(' '))?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    Some(days_from_civil(year, month, day) * 1440 + hour * 60 + minute)
}

/// Days since the Unix epoch for (year, month, day). Howard Hinnant's
/// `days_from_civil`; the inverse of the seed's `civil_from_days`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The action-point object and the handler the bridge calls back into.
const ACTION_POINT_OBJECT: &str = "ActionPointService";
const ATTACH_HANDLER: &str = "attach_proposal";

/// The margin the slate loop's outer guard leaves above its time budget and one model
/// call's total cap.
const SLATE_GUARD_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);

/// The outer guard over a spawned slate loop: the loop's own time budget, plus one model
/// call's total cap (a generation in flight when the budget runs out may finish), plus a
/// margin. The loop returns what it admitted when its budget runs out; the guard only
/// abandons a hung task (logged), so it must sit above the budget.
#[must_use]
pub fn slate_guard(
    budget: std::time::Duration,
    call_cap: std::time::Duration,
) -> std::time::Duration {
    budget + call_cap + SLATE_GUARD_MARGIN
}

/// The slate step the bridge runs for a pass: the (routed) point slate proposer, the joint
/// portfolio slate proposer, and the outer guard ([`slate_guard`]) that abandons a hung loop.
#[derive(Clone)]
pub struct SlateStep {
    /// The slate proposer a point is routed to.
    pub proposer: Arc<dyn SlateProposer>,
    /// The joint slate proposer a formed portfolio is planned by. Without one, every freshly
    /// opened point gets its own slate.
    pub portfolio: Option<Arc<dyn GroupSlateProposer>>,
    /// How long a slate loop may run before its task is abandoned (logged).
    pub guard: std::time::Duration,
    /// The agent's branch: beside each freshly opened point's slate, a separate guarded agent
    /// task while the rollout mode allows it. `None` runs no agent at all.
    pub shadow: Option<crate::slates::ShadowStep>,
}

/// Build an ingress URL from path segments.
fn ingress_url(ingress_base: &str, segments: &[&str]) -> Result<String, String> {
    let mut url = reqwest::Url::parse(ingress_base).map_err(|e| format!("bad ingress url: {e}"))?;
    url.path_segments_mut()
        .map_err(|()| "ingress url cannot be a base".to_owned())?
        .pop_if_empty()
        .extend(segments);
    Ok(url.into())
}

/// Build the ingress URL that calls the monitor and awaits its result:
/// `{ingress}/DriftMonitorService/notify`.
///
/// # Errors
/// Returns a message if `ingress_base` is not a valid base URL.
pub fn monitor_notify_url(ingress_base: &str) -> Result<String, String> {
    ingress_url(ingress_base, &[MONITOR_SERVICE, MONITOR_HANDLER])
}

/// Whether a subject IRI names a manufacturing (`mrp.*`) entity, the process
/// capability's domain (its action points key on the `mrp.production` order). Any
/// other subject (e.g. `sale.order`) belongs to supply-risk.
fn subject_is_process(subject: &EntityUri) -> bool {
    subject
        .model()
        .is_some_and(|model| model.starts_with("mrp."))
}

/// Routes a freshly opened subject to the capability agent that owns it: a
/// manufacturing subject (`mrp.*`) to the process agent, anything else to the
/// supply-risk agent. A [`Proposer`] itself, so the bridge stays generic.
pub struct RoutingProposer {
    supply_risk: Arc<dyn Proposer>,
    process: Arc<dyn Proposer>,
}

impl RoutingProposer {
    /// Compose the two capability agents behind one proposer.
    #[must_use]
    pub fn new(supply_risk: Arc<dyn Proposer>, process: Arc<dyn Proposer>) -> Self {
        Self {
            supply_risk,
            process,
        }
    }
}

#[async_trait]
impl Proposer for RoutingProposer {
    async fn propose(&self, subject: &EntityUri) -> ekg_domain::Result<GroundedProposal> {
        if subject_is_process(subject) {
            self.process.propose(subject).await
        } else {
            self.supply_risk.propose(subject).await
        }
    }
}

/// The slate counterpart of [`RoutingProposer`]: a manufacturing subject (`mrp.*`) goes to
/// the process slate agent, every other subject to the supply-risk slate agent. Routing is
/// total, so no opened subject is left unrouted; a point ends up without a slate only when
/// generation or projection fails. Adding a capability means composing it here, not adding
/// a gate in the bridge.
pub struct RoutingSlateProposer {
    supply_risk: Arc<dyn SlateProposer>,
    process: Arc<dyn SlateProposer>,
}

impl RoutingSlateProposer {
    /// Compose the two capability slate agents behind one router.
    #[must_use]
    pub fn new(supply_risk: Arc<dyn SlateProposer>, process: Arc<dyn SlateProposer>) -> Self {
        Self {
            supply_risk,
            process,
        }
    }
}

#[async_trait]
impl SlateProposer for RoutingSlateProposer {
    async fn propose_slate(&self, subject: &EntityUri) -> ekg_domain::Result<Slate> {
        if subject_is_process(subject) {
            self.process.propose_slate(subject).await
        } else {
            self.supply_risk.propose_slate(subject).await
        }
    }
}

/// Bridge the in-process `RdfChanged` fan-out through the durable loop: call the
/// monitor (which opens a point on drift and reports the freshly opened subjects),
/// and on a fresh open run the proposal agent and attach its proposal (`open` to
/// `proposed`). Detection and attach are durable; the agent runs in-process.
///
/// Best-effort: a failure at any hop is logged, not fatal. The point stays `open`
/// (the escalation timer covers it) and the revision guard makes redelivery safe.
/// An auto-promotion claims the point for `claim_deadline`. Returns when the
/// publisher is dropped.
#[allow(clippy::too_many_arguments)] // a composition-root fan-out over the wired seams
pub async fn run_monitor_bridge(
    mut rx: broadcast::Receiver<RdfChanged>,
    ingress_base: String,
    claim_deadline: std::time::Duration,
    proposer: Arc<dyn Proposer>,
    policy: PolicyEngine,
    executor: Arc<dyn ActionExecutor>,
    world: Arc<dyn WorldModel>,
    slate_step: Option<SlateStep>,
) {
    let client = reqwest::Client::new();
    let ledger = IngressLedger::new(client.clone(), ingress_base.clone(), claim_deadline);
    loop {
        match rx.recv().await {
            Ok(event) => {
                forward(
                    &client,
                    &ingress_base,
                    &ledger,
                    proposer.as_ref(),
                    &policy,
                    executor.as_ref(),
                    &world,
                    slate_step.as_ref(),
                    &event,
                )
                .await;
            }
            // A slow bridge dropped events; the sweep backfills, so keep going.
            Err(RecvError::Lagged(skipped)) => {
                eprintln!(
                    "warning: monitor bridge lagged, {skipped} change events skipped (sweep backfills)"
                );
            }
            // The publisher was dropped: the process is shutting down.
            Err(RecvError::Closed) => break,
        }
    }
}

/// Bridge the durable NATS JetStream backbone through the same monitor loop as
/// [`run_monitor_bridge`]: bind a durable pull consumer on the `EKG_RDF` stream
/// filtered to `ekg.rdf.v1.>`, deserialize each message to an [`RdfChanged`], run
/// the shared [`forward`], then `ack`.
///
/// At-least-once: JetStream redelivers on a missed ack, and the action-point
/// revision guard drops a stale or replayed event, so no dedup is needed. A decode
/// or handler error is logged and the message acked anyway (the sweep and the
/// revision guard cover correctness). Returns when the message stream ends or the
/// consumer cannot be bound.
#[allow(clippy::too_many_arguments)] // a composition-root fan-out over the wired seams
pub async fn run_monitor_bridge_nats(
    js: jetstream::Context,
    ingress_base: String,
    claim_deadline: std::time::Duration,
    proposer: Arc<dyn Proposer>,
    policy: PolicyEngine,
    executor: Arc<dyn ActionExecutor>,
    world: Arc<dyn WorldModel>,
    slate_step: Option<SlateStep>,
) {
    let stream = match js.get_stream(crate::nats::STREAM_NAME).await {
        Ok(stream) => stream,
        Err(why) => {
            eprintln!(
                "warning: monitor bridge: NATS stream {} unavailable ({why})",
                crate::nats::STREAM_NAME
            );
            return;
        }
    };
    let consumer = match stream
        .get_or_create_consumer(
            MONITOR_CONSUMER,
            jetstream::consumer::pull::Config {
                durable_name: Some(MONITOR_CONSUMER.to_owned()),
                filter_subject: crate::nats::STREAM_SUBJECTS.to_owned(),
                ..Default::default()
            },
        )
        .await
    {
        Ok(consumer) => consumer,
        Err(why) => {
            eprintln!("warning: monitor bridge: could not bind the NATS consumer ({why})");
            return;
        }
    };
    let mut messages = match consumer.messages().await {
        Ok(messages) => messages,
        Err(why) => {
            eprintln!("warning: monitor bridge: could not stream NATS messages ({why})");
            return;
        }
    };

    let client = reqwest::Client::new();
    let ledger = IngressLedger::new(client.clone(), ingress_base.clone(), claim_deadline);
    while let Some(next) = messages.next().await {
        let message = match next {
            Ok(message) => message,
            Err(why) => {
                eprintln!("warning: monitor bridge: NATS message error ({why})");
                continue;
            }
        };
        match serde_json::from_slice::<RdfChanged>(&message.payload) {
            Ok(event) => {
                forward(
                    &client,
                    &ingress_base,
                    &ledger,
                    proposer.as_ref(),
                    &policy,
                    executor.as_ref(),
                    &world,
                    slate_step.as_ref(),
                    &event,
                )
                .await;
            }
            Err(why) => {
                eprintln!("warning: monitor bridge: undecodable RdfChanged, dropping ({why})");
            }
        }
        // Ack regardless: a decode or handler failure is covered by the sweep and the
        // revision guard, and a redelivery would be dropped as non-increasing, so
        // holding the message back would only stall the consumer.
        if let Err(why) = message.ack().await {
            eprintln!("warning: monitor bridge: NATS ack failed ({why})");
        }
    }
}

/// Drive one event through detect, propose, attach and (maybe) auto-promote. Errors
/// are logged, never returned.
#[allow(clippy::too_many_arguments)] // a composition-root fan-out over the wired seams
async fn forward(
    client: &reqwest::Client,
    ingress_base: &str,
    ledger: &dyn CommandLedger,
    proposer: &dyn Proposer,
    policy: &PolicyEngine,
    executor: &dyn ActionExecutor,
    world: &Arc<dyn WorldModel>,
    slate_step: Option<&SlateStep>,
    event: &RdfChanged,
) {
    // Detect: the monitor returns the subjects a point was freshly opened for (empty on
    // no drift or a redelivered or stale event). A change to a shared dependency (e.g. a
    // slipped inbound shipment ETA) can open points for several downstream orders at
    // once, so each is driven independently.
    let outcome = call_monitor(client, ingress_base, event).await;
    // Slates: a joint slate for each formed portfolio, a per-point slate for each freshly
    // opened point in no formed portfolio, and one for each released point without a slate.
    // Each is spawned under the outer guard, so a slow or hung loop cannot delay the
    // single-proposal path or stall later events. A slate attaches to a point whether
    // `open` or already `proposed`, so it does not race the proposal transition.
    if let Some(slate_step) = slate_step {
        let sink = Arc::new(crate::slates::IngressSink::new(
            client.clone(),
            ingress_base,
        ));
        let joint = slate_step.portfolio.is_some();
        for task in crate::slates::slate_tasks(&outcome, joint) {
            spawn_slate_task(task, slate_step, &sink, policy, world, true);
        }
    }
    // Every freshly opened point, portfolio member or not, gets its grounded proposal
    // (`open` to `proposed`).
    for subject in outcome.opened {
        propose_and_maybe_promote(
            &IngressProposals {
                client,
                ingress_base,
            },
            ledger,
            proposer,
            policy,
            executor,
            world.as_ref(),
            &subject,
        )
        .await;
    }
}

/// Spawn one slate task under the step's guard. A portfolio's fallback member slates are each
/// spawned as their own guarded task. A freshly opened point and a formed portfolio (`shadowed`)
/// also get the agent's slate in its own task; a released point and a fallback member do not.
fn spawn_slate_task(
    task: crate::slates::SlateTask,
    step: &SlateStep,
    sink: &Arc<crate::slates::IngressSink>,
    policy: &PolicyEngine,
    world: &Arc<dyn WorldModel>,
    shadowed: bool,
) {
    use crate::slates::{self, SlateTask};
    let (step, sink, policy, world) = (
        step.clone(),
        Arc::clone(sink),
        policy.clone(),
        Arc::clone(world),
    );
    tokio::spawn(async move {
        let agents_joint_slate = match (&task, &step.shadow) {
            (SlateTask::Portfolio(_), Some(shadow)) if shadowed => Some(shadow.joint_guard),
            _ => None,
        };
        let guard = task_guard(step.guard, agents_joint_slate);
        let label = match &task {
            SlateTask::Portfolio(formed) => formed.id.0.clone(),
            SlateTask::Point(subject) | SlateTask::Released(subject) => subject.0.clone(),
        };
        let run = async {
            let fallback = slates::run_task(
                &task,
                sink.as_ref(),
                step.proposer.as_ref(),
                step.portfolio.as_deref(),
                &policy,
                world.as_ref(),
                step.shadow.as_ref().filter(|_| shadowed),
            )
            .await;
            for member in fallback {
                spawn_slate_task(
                    SlateTask::Point(member),
                    &step,
                    &sink,
                    &policy,
                    &world,
                    false,
                );
            }
        };
        if tokio::time::timeout(guard, run).await.is_err() {
            eprintln!("warning: slate loop for {label} exceeded {guard:?}; skipped");
        }
    });
}

/// The guard over one slate task: the step's own, raised to the guard of the agent's joint
/// slate for a portfolio that may wait for it. A portfolio's task shows the existing joint
/// slate once the agent's has run out of time, so it must not be abandoned before then.
fn task_guard(
    step: std::time::Duration,
    agents_joint_slate: Option<std::time::Duration>,
) -> std::time::Duration {
    agents_joint_slate.map_or(step, |joint| step.max(joint))
}

/// Where a point's proposal is attached: the action-point object over the ingress in
/// production.
#[async_trait]
trait ProposalSink: Send + Sync {
    /// Attaches `grounded` to `subject`'s point; returns whether it landed.
    async fn attach(&self, subject: &EntityUri, grounded: &GroundedProposal) -> bool;
}

/// The [`ProposalSink`] over the Restate ingress.
struct IngressProposals<'a> {
    client: &'a reqwest::Client,
    ingress_base: &'a str,
}

#[async_trait]
impl ProposalSink for IngressProposals<'_> {
    async fn attach(&self, subject: &EntityUri, grounded: &GroundedProposal) -> bool {
        attach_proposal(self.client, self.ingress_base, subject, grounded).await
    }
}

/// Drive one freshly opened subject through propose, attach and (maybe) auto-promote.
/// Errors are logged, never returned, so one subject's failure does not block the others.
///
/// The proposal comes from `proposer` whatever the agent's rollout mode is: this path is
/// handed neither the rollout nor a slate, so auto-promotion never acts on an agent's plan.
async fn propose_and_maybe_promote(
    proposals: &dyn ProposalSink,
    ledger: &dyn CommandLedger,
    proposer: &dyn Proposer,
    policy: &PolicyEngine,
    executor: &dyn ActionExecutor,
    world: &dyn WorldModel,
    subject: &EntityUri,
) {
    // Propose: ground and generate in-process. A failure, or a plan the governed
    // catalog refuses, leaves the point open for the escalation timer.
    let Some(grounded) = admit_for_attach(subject, proposer.propose(subject).await) else {
        return;
    };
    // Attach: move the point from open to proposed. Awaited so `proposed` is
    // observable before the policy is consulted. On failure the point stays open and
    // the auto-promote step is skipped.
    if !proposals.attach(subject, &grounded).await {
        return;
    }
    // Policy: a human-authored policy may auto-promote a plan that meets every
    // predicate; otherwise the point waits for a human with structured reasons.
    // The whole plan is promoted as one governed step.
    let plan = Plan {
        actions: grounded.proposal.actions.clone(),
    };
    maybe_auto_promote(ledger, executor, policy, world, subject, &plan).await;
}

/// Admit a generated proposal for attach: only a plan whose every action the governed
/// catalog accepts (known verb, every required param present and well-formed, a target of
/// the verb's model) may reach a human. A generation failure or a refused plan is logged and
/// returns `None`: the point stays `open` for a later proposal or the escalation timer, and
/// no promotion can reach a write-back with that action.
fn admit_for_attach(
    subject: &EntityUri,
    generated: ekg_domain::Result<GroundedProposal>,
) -> Option<GroundedProposal> {
    let grounded = match generated {
        Ok(grounded) => grounded,
        Err(why) => {
            eprintln!(
                "warning: proposal agent failed for {} ({why}); point stays open",
                subject.0
            );
            return None;
        }
    };
    if let Err(why) = crate::propose::validate_plan(&grounded.proposal.actions) {
        eprintln!(
            "warning: proposal for {} refused before attach ({why}); point stays open",
            subject.0
        );
        return None;
    }
    Some(grounded)
}

/// Call the monitor and return what its pass did: the subjects a point was freshly
/// opened for, the portfolios it formed and the subjects it released. All empty on any
/// error or no drift.
async fn call_monitor(
    client: &reqwest::Client,
    ingress_base: &str,
    event: &RdfChanged,
) -> NotifyOutcome {
    let url = match monitor_notify_url(ingress_base) {
        Ok(url) => url,
        Err(why) => {
            eprintln!("warning: monitor bridge bad url: {why}");
            return NotifyOutcome::default();
        }
    };
    let body = match serde_json::to_vec(event) {
        Ok(body) => body,
        Err(why) => {
            eprintln!("warning: monitor bridge encode: {why}");
            return NotifyOutcome::default();
        }
    };
    let resp = match client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(why) => {
            eprintln!(
                "warning: monitor bridge: ingress unreachable for {} ({why})",
                event.entity_uri.0
            );
            return NotifyOutcome::default();
        }
    };
    if !resp.status().is_success() {
        eprintln!(
            "warning: monitor bridge: notify rejected for {} (HTTP {})",
            event.entity_uri.0,
            resp.status().as_u16()
        );
        return NotifyOutcome::default();
    }
    let text = match resp.text().await {
        Ok(text) => text,
        Err(why) => {
            eprintln!(
                "warning: monitor bridge: notify reply unreadable for {} ({why})",
                event.entity_uri.0
            );
            return NotifyOutcome::default();
        }
    };
    // A reply that does not decode is logged, not silently read as "nothing opened".
    serde_json::from_str::<NotifyOutcome>(&text).unwrap_or_else(|why| {
        eprintln!(
            "warning: monitor bridge: notify reply undecodable for {} ({why})",
            event.entity_uri.0
        );
        NotifyOutcome::default()
    })
}

/// Attach the grounded proposal to the subject's action point (`open` to `proposed`),
/// awaiting the call so `proposed` is observable to the auto-promote that follows.
/// Returns whether the attach landed; a failure logs and leaves the point open.
async fn attach_proposal(
    client: &reqwest::Client,
    ingress_base: &str,
    subject: &EntityUri,
    grounded: &GroundedProposal,
) -> bool {
    let url = match ingress_url(
        ingress_base,
        &[ACTION_POINT_OBJECT, &subject.0, ATTACH_HANDLER],
    ) {
        Ok(url) => url,
        Err(why) => {
            eprintln!("warning: monitor bridge bad attach url: {why}");
            return false;
        }
    };
    let body = match serde_json::to_vec(grounded) {
        Ok(body) => body,
        Err(why) => {
            eprintln!("warning: monitor bridge could not encode a proposal: {why}");
            return false;
        }
    };
    match client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            eprintln!(
                "warning: monitor bridge: attach rejected for {} (HTTP {})",
                subject.0,
                resp.status().as_u16()
            );
            false
        }
        Err(why) => {
            eprintln!(
                "warning: monitor bridge: attach unreachable for {} ({why})",
                subject.0
            );
            false
        }
    }
}

/// Run the slate loop for a freshly opened subject and attach the ranked slate.
/// Best-effort: a generation or attach failure logs and leaves the point without a slate
/// (the single-proposal path and escalation timer still cover it). Producing a slate
/// commits nothing.
pub async fn propose_and_attach_slate(
    client: &reqwest::Client,
    ingress_base: &str,
    slate_proposer: &dyn SlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    subject: &EntityUri,
) {
    let sink = crate::slates::IngressSink::new(client.clone(), ingress_base);
    let _ = crate::slates::point_slate(&sink, slate_proposer, policy, world, subject).await;
}

/// Consult the policy for a freshly proposed plan and, on `AutoPromote`, run the same
/// governed write-back as the human path. The promoter is the human-authored policy,
/// never the model.
///
/// On [`PromotionTier::HumanGated`] this is a no-op: the point waits for a human. On
/// [`PromotionTier::AutoPromote`] it builds a [`Promotion`] authored by the policy
/// (reviewer `policy:<id>`), claims the point like any governed command (a human already
/// acting on it wins), executes the whole `PromotedPlan` via `execute_plan`, then records
/// it via the `promote` handler. One promotion authorizes every action in the plan.
///
/// A clean failure releases the claim (audited), logs, and leaves the point `proposed`
/// for a human; `execute_plan` has rolled back any committed actions. The source is not
/// transactional, so a rollback can itself be refused by a concurrent source edit
/// ([`EngineError::PlanPartiallyApplied`]). That is logged separately: the source is
/// partially changed and the point is left for human attention.
pub async fn maybe_auto_promote(
    ledger: &dyn CommandLedger,
    executor: &dyn ActionExecutor,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    subject: &EntityUri,
    plan: &Plan,
) {
    // Evaluate the whole plan against every predicate, gathering the cost and supplier-state
    // context the predicates consult. On any failed predicate the point is left `proposed`
    // for a human, with the structured reasons logged.
    //
    // Fail closed: if the cost or supplier context cannot be read, leave the point for a
    // human instead of auto-promoting on a partial context.
    let ctx = match policy_context::gather(world, plan).await {
        Ok(ctx) => ctx,
        Err(why) => {
            println!(
                "{} left human-gated: policy context unavailable ({why})",
                subject.0
            );
            return;
        }
    };
    let decision = policy.evaluate(plan, &ctx);
    if decision.tier == PromotionTier::HumanGated {
        if !decision.needs.is_empty() {
            let reasons: Vec<&str> = decision.needs.iter().map(|n| n.code.as_str()).collect();
            println!(
                "{} left human-gated by policy {}: {}",
                subject.0,
                policy.policy_id(),
                reasons.join(", ")
            );
        }
        return; // the default: a human must promote.
    }

    // Provenance: the policy is the promoter, not a person (EU AI Act Art. 12). Same
    // record shape as the human gate; reviewer and rationale name the policy.
    let promotion = Promotion {
        action_point: ActionPointId(format!("action-point/{}", subject.0)),
        correlation_id: CorrelationId(format!("auto-promote/{}", subject.0)),
        reviewer: ReviewerId(format!("policy:{}", policy.policy_id())),
        rationale: format!("auto-promoted by policy {}", policy.policy_id()),
        context_seen: format!(
            "promotion tier auto-promote (policy {})",
            policy.policy_id()
        ),
    };
    let promoted = PromotedPlan {
        plan: plan.clone(),
        promotion,
        // Audit-only, like the human gate: a reverse guards on each receipt's own
        // source watermark, not this basis revision.
        basis_revision: Revision(0),
    };

    auto_promote_claimed(ledger, executor, subject, &promoted).await;
}

/// Run a policy promotion under a claim: claim the point (a refusal means another
/// command holds it; it is logged and nothing is written), run the governed write-back,
/// then record it, or release the claim with the failure's reason. Returns whether the
/// plan was promoted and recorded.
async fn auto_promote_claimed(
    ledger: &dyn CommandLedger,
    executor: &dyn ActionExecutor,
    subject: &EntityUri,
    promoted: &PromotedPlan,
) -> bool {
    let token = match ledger
        .claim(
            &subject.0,
            ClaimCommand::AutoPromote,
            &promoted.promotion.reviewer.0,
            &[ActionPointState::Proposed],
        )
        .await
    {
        Ok(token) => token,
        // A portfolio member waits for its portfolio's joint plan: a human-gated outcome,
        // not a contended claim.
        Err(refused) if refused.detail.contains(MEMBER_AUTO_PROMOTE_REFUSAL) => {
            println!(
                "{} left for its portfolio's plan: a member is not auto-promoted alone ({})",
                subject.0, refused.detail
            );
            return false;
        }
        Err(refused) => {
            println!(
                "auto-promote skipped for {}: the point was not claimable ({})",
                subject.0, refused.detail
            );
            return false;
        }
    };

    // The governed write-back, the only commit path. The whole plan is applied as a unit;
    // each action still goes through the single-action `execute`.
    let execution_plan = match executor.execute_plan(promoted).await {
        Ok(execution_plan) => execution_plan,
        Err(why) => {
            match &why {
                // A partial commit: a mid-plan rollback was itself refused because a
                // concurrent source edit advanced a just-written watermark, so some governed
                // writes landed and could not be undone. Logged with its own message, since
                // the generic "point stays proposed" reads as if nothing happened. The point
                // stays `proposed`, but the source needs human attention first: a blind
                // re-promote would re-apply the writes that survived.
                EngineError::PlanPartiallyApplied { .. } => eprintln!(
                    "‼ auto-promote partially applied for {}: {why}; point left proposed. The \
                     source is partially changed and needs human attention before any re-promote",
                    subject.0
                ),
                // A clean failure: `execute_plan` rolled back any committed actions, so
                // nothing is left applied and the point simply waits for a human.
                _ => eprintln!(
                    "warning: auto-promote write-back failed for {} ({why}); point stays proposed \
                     (any committed actions were rolled back)",
                    subject.0
                ),
            }
            // Release the claim; the record audits the failed attempt.
            let failed: Result<(), Failure> = Err(Failure::Release(write_back_problem(&why)));
            let _ = settle(ledger, &subject.0, &token, failed).await;
            return false;
        }
    };

    // Record the auto-promotion (`proposed`, `under_review`, `promoted`). A record that
    // does not land keeps the claim until its deadline (the executed plan's receipts
    // still enable a manual reverse).
    match ledger
        .promote(
            &subject.0,
            &token,
            &promoted.promotion,
            &execution_plan,
            None,
        )
        .await
    {
        Ok(()) => true,
        Err(why) => {
            eprintln!(
                "warning: auto-promote: the promotion of {} was written back but not recorded ({}); \
                 its claim lapses at the deadline",
                subject.0, why.detail
            );
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    mod auto_promote_claim {
        use super::super::*;
        use crate::action_point::Problem;
        use crate::command::fakes::{Call, FakeExecutor, FakeLedger};

        fn subject() -> EntityUri {
            EntityUri("https://ekg.example/id/sale.order/1".to_owned())
        }

        fn promoted() -> PromotedPlan {
            PromotedPlan {
                plan: Plan {
                    actions: Vec::new(),
                },
                promotion: Promotion {
                    action_point: ActionPointId("action-point/s".to_owned()),
                    correlation_id: CorrelationId("auto-promote/s".to_owned()),
                    reviewer: ReviewerId("policy:auto-apply".to_owned()),
                    rationale: "auto-promoted by policy auto-apply".to_owned(),
                    context_seen: String::new(),
                },
                basis_revision: Revision(0),
            }
        }

        #[tokio::test]
        async fn a_refused_claim_runs_no_write_back() {
            let mut ledger = FakeLedger::with(None);
            ledger.refuse_claim = Some(Problem::conflict("promote by planner-1 in progress"));
            let executor = FakeExecutor::default();
            assert!(!auto_promote_claimed(&ledger, &executor, &subject(), &promoted()).await);
            assert_eq!(
                executor.executed(),
                0,
                "a human holds the point: nothing written"
            );
            assert_eq!(ledger.calls(), vec![Call::Claim(ClaimCommand::AutoPromote)]);
        }

        #[tokio::test]
        async fn a_members_refused_claim_runs_no_write_back() {
            let mut ledger = FakeLedger::with(None);
            ledger.refuse_claim = Some(Problem::conflict(format!(
                "action point is a member of portfolio portfolio/a; it is \
                 {MEMBER_AUTO_PROMOTE_REFUSAL}"
            )));
            let executor = FakeExecutor::default();
            assert!(!auto_promote_claimed(&ledger, &executor, &subject(), &promoted()).await);
            assert_eq!(executor.executed(), 0, "a member waits: nothing written");
            assert_eq!(
                ledger.calls(),
                vec![Call::Claim(ClaimCommand::AutoPromote)],
                "no release and no promote: the point stays proposed"
            );
        }

        #[tokio::test]
        async fn a_failed_write_back_releases_the_claim() {
            let ledger = FakeLedger::with(None);
            let executor = FakeExecutor {
                fail: Some(|| EngineError::Backend("odoo refused".to_owned())),
                ..FakeExecutor::default()
            };
            assert!(!auto_promote_claimed(&ledger, &executor, &subject(), &promoted()).await);
            let calls = ledger.calls();
            assert!(
                matches!(calls.as_slice(), [Call::Claim(ClaimCommand::AutoPromote), Call::Release(why)] if why.contains("odoo refused")),
                "{calls:?}"
            );
        }

        #[tokio::test]
        async fn a_claimed_write_back_is_recorded() {
            let ledger = FakeLedger::with(None);
            let executor = FakeExecutor::default();
            assert!(auto_promote_claimed(&ledger, &executor, &subject(), &promoted()).await);
            assert_eq!(
                ledger.calls(),
                vec![Call::Claim(ClaimCommand::AutoPromote), Call::Promote]
            );
        }
    }

    #[test]
    fn a_portfolios_task_outlasts_the_agents_joint_slate() {
        use std::time::Duration;
        let budget = ekg_cognition::DEFAULT_SLATE_BUDGET;
        let step = slate_guard(
            budget,
            ekg_cognition::CognitionLimits::default().total_timeout,
        );
        let joint = slate_guard(
            budget,
            ekg_cognition::DEFAULT_ANALYST_RUN_CAP + ekg_cognition::DEFAULT_JOINT_RUN_CAP,
        );
        assert_eq!(joint, Duration::from_secs(540), "about 540 s by default");
        assert_eq!(task_guard(step, Some(joint)), joint);
        // A point's task, and a portfolio's without the agent, keep the step's guard.
        assert_eq!(task_guard(step, None), step);
        assert_eq!(task_guard(step, Some(Duration::from_secs(1))), step);
    }

    #[test]
    fn the_slate_guard_sits_above_the_budget_and_one_call() {
        use std::time::Duration;
        let budget = ekg_cognition::DEFAULT_SLATE_BUDGET;
        let call_cap = ekg_cognition::CognitionLimits::default().total_timeout;
        let guard = slate_guard(budget, call_cap);
        // A loop that ran out its budget while a call was in flight still returns its
        // slate before the guard abandons it.
        assert!(guard > budget + call_cap, "{guard:?}");
        assert_eq!(guard, Duration::from_secs(300), "about 300 s by default");
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn proposal_of(verb: &str, target: &str, params: &[(&str, &str)]) -> GroundedProposal {
        GroundedProposal {
            proposal: ekg_domain::Proposal {
                explanation: "test".to_owned(),
                actions: vec![ekg_domain::ProposedAction {
                    action_type: ekg_domain::ActionType(verb.to_owned()),
                    target: EntityUri(target.to_owned()),
                    params: params
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                }],
                omissions: Vec::new(),
            },
            grounding_omissions: Vec::new(),
            grounding_edges: Vec::new(),
            subject_members: Vec::new(),
        }
    }

    #[test]
    fn a_paramless_re_promise_never_attaches() {
        // A re-promise with no new date must be refused before attach, not surface as a
        // 502 after a human promoted it.
        let subject = EntityUri("https://ekg.example/id/sale.order/1".to_owned());
        let paramless = proposal_of("re_promise_date", &subject.0, &[]);
        assert!(admit_for_attach(&subject, Ok(paramless)).is_none());
        // So are a malformed date and an expedite targeting a sale order.
        let malformed = proposal_of("re_promise_date", &subject.0, &[("new_date", "16 Oct")]);
        assert!(admit_for_attach(&subject, Ok(malformed)).is_none());
        let mistargeted = proposal_of(
            "expedite_po",
            &subject.0,
            &[("new_date", "2027-10-12 08:00:00")],
        );
        assert!(admit_for_attach(&subject, Ok(mistargeted)).is_none());
        // A generation failure leaves the point open too.
        let failed = Err(ekg_domain::EngineError::InvalidProposal(
            "abstain".to_owned(),
        ));
        assert!(admit_for_attach(&subject, failed).is_none());
    }

    #[test]
    fn a_well_formed_plan_is_admitted_for_attach() {
        let subject = EntityUri("https://ekg.example/id/sale.order/1".to_owned());
        let expedite = proposal_of(
            "expedite_po",
            "https://ekg.example/id/purchase.order.line/4",
            &[("new_date", "2027-10-12 08:00:00")],
        );
        assert!(admit_for_attach(&subject, Ok(expedite)).is_some());
    }

    /// A proposal sink that keeps what it was handed.
    #[derive(Default)]
    struct Proposals(std::sync::Mutex<Vec<(EntityUri, GroundedProposal)>>);

    #[async_trait]
    impl ProposalSink for Proposals {
        async fn attach(&self, subject: &EntityUri, grounded: &GroundedProposal) -> bool {
            if let Ok(mut attached) = self.0.lock() {
                attached.push((subject.clone(), grounded.clone()));
            }
            true
        }
    }

    /// The existing path's proposer: one fixed proposal.
    struct Prompted(GroundedProposal);

    #[async_trait]
    impl Proposer for Prompted {
        async fn propose(&self, _subject: &EntityUri) -> ekg_domain::Result<GroundedProposal> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn the_proposal_and_its_auto_promotion_come_from_the_existing_path() {
        use crate::command::fakes::{Call, FakeExecutor, FakeLedger};

        let subject = EntityUri("https://ekg.example/id/sale.order/1".to_owned());
        let prompted = proposal_of("notify_owner", &subject.0, &[("message", "late inbound")]);
        let policy = PolicyEngine::new(
            "auto-apply",
            std::iter::once("notify_owner".to_owned()).collect(),
        );
        let (proposals, ledger, executor) = (
            Proposals::default(),
            FakeLedger::with(None),
            FakeExecutor::default(),
        );
        // The path is handed the prompt proposer only: no rollout, no slate, no agent. So what
        // it attaches and auto-promotes is the same in `legacy`, `shadow` and `agent`.
        propose_and_maybe_promote(
            &proposals,
            &ledger,
            &Prompted(prompted.clone()),
            &policy,
            &executor,
            &ekg_domain::fakes::InMemoryWorldModel::default(),
            &subject,
        )
        .await;
        let attached = proposals.0.lock().expect("lock").clone();
        assert_eq!(attached, vec![(subject, prompted)]);
        assert_eq!(executor.executed(), 1, "the proposal's one action");
        assert_eq!(
            ledger.calls(),
            vec![Call::Claim(ClaimCommand::AutoPromote), Call::Promote]
        );
    }

    #[test]
    fn routing_selects_the_process_agent_for_mrp_subjects_only() {
        let process = |iri: &str| subject_is_process(&EntityUri(iri.to_owned()));
        // Manufacturing subjects route to the process agent.
        assert!(process("https://ekg.example/id/mrp.production/1"));
        assert!(process("https://ekg.example/id/mrp.workorder/5"));
        // Everything else (supply-risk domain) does not.
        assert!(!process("https://ekg.example/id/sale.order/1"));
        assert!(!process("https://ekg.example/id/purchase.order/2"));
        assert!(!process("https://ekg.example/id/product.product/9"));
    }

    /// A [`SlateProposer`] that records its identity in the slate's baseline `rank_key`,
    /// so a routing test can assert which capability agent was dispatched to.
    struct TaggingSlateProposer(&'static str);

    #[async_trait]
    impl SlateProposer for TaggingSlateProposer {
        async fn propose_slate(&self, _subject: &EntityUri) -> ekg_domain::Result<Slate> {
            Ok(Slate {
                baseline: ekg_domain::Score {
                    metrics: std::collections::BTreeMap::new(),
                    rank_key: self.0.to_owned(),
                },
                options: Vec::new(),
                trace: ekg_domain::Trace::from_steps(Vec::new()),
            })
        }
    }

    #[tokio::test]
    async fn slate_routing_dispatches_each_subject_to_its_capability_agent() {
        let router = RoutingSlateProposer::new(
            Arc::new(TaggingSlateProposer("supply_risk")),
            Arc::new(TaggingSlateProposer("process")),
        );
        // A sale.order subject routes to the supply-risk slate agent.
        let sale = router
            .propose_slate(&EntityUri("https://ekg.example/id/sale.order/1".to_owned()))
            .await
            .expect("slate");
        assert_eq!(sale.baseline.rank_key, "supply_risk");
        // An mrp.* subject routes to the process slate agent.
        let mo = router
            .propose_slate(&EntityUri(
                "https://ekg.example/id/mrp.production/1".to_owned(),
            ))
            .await
            .expect("slate");
        assert_eq!(mo.baseline.rank_key, "process");
    }

    #[test]
    fn an_at_risk_row_maps_to_a_verdict_for_its_sales_order() {
        let verdict = interpret_risk_row(&row(&[
            ("so", "https://ekg.example/id/sale.order/42"),
            ("comp", "https://ekg.example/id/product.product/9"),
            ("shortfall", "13"),
        ]))
        .expect("an at-risk row is a verdict");
        assert_eq!(verdict.subject.0, "https://ekg.example/id/sale.order/42");
        assert!(verdict.reason.contains("13"));
    }

    #[test]
    fn a_late_shipment_row_names_the_transport_delay() {
        let verdict = interpret_risk_row(&row(&[
            ("so", "https://ekg.example/id/sale.order/42"),
            ("comp", "https://ekg.example/id/product.product/9"),
            ("shortfall", "13"),
            ("lateShip", "1"),
        ]))
        .expect("an at-risk row is a verdict");
        assert!(verdict.reason.contains("shipment"));
        assert!(verdict.reason.to_lowercase().contains("delayed"));
    }

    #[test]
    fn a_non_transport_shortfall_keeps_the_standard_reason() {
        // lateShip = 0 (or absent) gives the availability-shortfall wording.
        let verdict = interpret_risk_row(&row(&[
            ("so", "https://ekg.example/id/sale.order/42"),
            ("comp", "https://ekg.example/id/product.product/9"),
            ("shortfall", "13"),
            ("lateShip", "0"),
        ]))
        .expect("an at-risk row is a verdict");
        assert!(!verdict.reason.contains("shipment"));
    }

    #[test]
    fn a_row_missing_the_subject_binding_is_not_a_verdict() {
        // No ?so binding (an empty solution) yields nothing.
        assert!(interpret_risk_row(&row(&[("shortfall", "13")])).is_none());
    }

    #[test]
    fn the_injected_rule_carries_the_semantic_layer_select() {
        let rule = supply_risk_rule();
        assert!(rule.sparql().contains("SELECT ?so"));
        assert!(rule.sparql().contains("FILTER(?shortfall > 0)"));
    }

    #[test]
    fn contention_beyond_capacity_flags_the_manufacturing_order() {
        // Capacity 2, anchor plus 2 overlapping peers: concurrency 3 > 2, so contention.
        let verdict = interpret_contention_row(&row(&[
            ("wo", "https://ekg.example/id/mrp.workorder/1"),
            ("wc", "https://ekg.example/id/mrp.workcenter/9"),
            ("capacity", "2"),
            ("mo", "https://ekg.example/id/mrp.production/5"),
            ("woStart", "2027-10-11T08:00:00"),
            ("woEnd", "2027-10-11T12:00:00"),
            ("overlapPeers", "2"),
            (
                "peers",
                "https://ekg.example/id/mrp.workorder/2, https://ekg.example/id/mrp.workorder/3",
            ),
        ]))
        .expect("over-capacity concurrency is a verdict");
        assert_eq!(verdict.subject.0, "https://ekg.example/id/mrp.production/5");
        assert!(verdict.reason.contains("contention"));
        assert!(verdict.reason.contains("mrp.workcenter/9"));
        assert!(verdict.reason.contains("2027-10-11T08:00:00"));
    }

    #[test]
    fn contention_within_capacity_is_not_flagged() {
        // Capacity 2, anchor plus 1 overlapping peer: concurrency 2, within capacity.
        assert!(
            interpret_contention_row(&row(&[
                ("wo", "https://ekg.example/id/mrp.workorder/1"),
                ("wc", "https://ekg.example/id/mrp.workcenter/9"),
                ("capacity", "2"),
                ("mo", "https://ekg.example/id/mrp.production/5"),
                ("woStart", "2027-10-11T08:00:00"),
                ("woEnd", "2027-10-11T12:00:00"),
                ("overlapPeers", "1"),
                ("peers", "https://ekg.example/id/mrp.workorder/2"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn infeasibility_planned_finish_after_the_commitment_flags_the_order() {
        // Trigger 1: the Odoo-computed finish (10-10) is after the promise it fulfils
        // (commitment 10-05).
        let verdict = interpret_infeasibility_row(&row(&[
            ("mo", "https://ekg.example/id/mrp.production/5"),
            ("startedAt", "2027-10-11T08:00:00"),
            ("endedAt", "2027-10-16T16:00:00"),
            ("commitment", "2027-10-11T00:00:00"),
            ("required", "180"),
        ]))
        .expect("a finish after the commitment is a verdict");
        assert_eq!(verdict.subject.0, "https://ekg.example/id/mrp.production/5");
        assert!(verdict.reason.contains("finish"));
        assert!(verdict.reason.contains("2027-10-16T16:00:00"));
        assert!(verdict.reason.contains("commitment"));
        assert!(verdict.reason.contains("2027-10-11T00:00:00"));
    }

    #[test]
    fn infeasibility_unscheduled_routing_overruns_the_commitment() {
        // Trigger 1 without a planned finish: earliest completion is start plus routing
        // (08:00 + 300 min = 13:00), which overruns the commitment (12:00).
        let verdict = interpret_infeasibility_row(&row(&[
            ("mo", "https://ekg.example/id/mrp.production/6"),
            ("startedAt", "2027-10-11T08:00:00"),
            ("commitment", "2027-10-11T12:00:00"),
            ("required", "300"),
        ]))
        .expect("an earliest completion past the commitment is a verdict");
        assert!(verdict.reason.contains("earliest completion"));
        assert!(verdict.reason.contains("300"));
        assert!(verdict.reason.contains("commitment"));
    }

    #[test]
    fn infeasibility_completion_within_the_commitment_is_not_flagged() {
        // Finish (10-05) is before the commitment (10-10) and the routing fits the
        // window, so neither trigger holds.
        assert!(
            interpret_infeasibility_row(&row(&[
                ("mo", "https://ekg.example/id/mrp.production/5"),
                ("startedAt", "2027-10-11T08:00:00"),
                ("endedAt", "2027-10-11T16:00:00"),
                ("commitment", "2027-10-16T00:00:00"),
                ("required", "300"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn the_two_infeasibility_triggers_are_told_apart() {
        let trigger = |commitment: &'static str| {
            infeasibility_of(&row(&[
                ("mo", "https://ekg.example/id/mrp.production/7"),
                ("startedAt", "2027-10-11T08:00:00"),
                ("endedAt", "2027-10-11T10:00:00"),
                ("commitment", commitment),
                ("required", "180"),
            ]))
            .map(|(trigger, _)| trigger)
        };
        assert_eq!(
            trigger("2027-10-11T09:00:00"),
            Some(Infeasibility::MissesCommitment)
        );
        assert_eq!(
            trigger("2027-10-12T00:00:00"),
            Some(Infeasibility::OutgrowsWindow)
        );
    }

    #[test]
    fn a_missed_promise_flags_both_orders_and_a_window_overrun_only_the_build() {
        const MO: &str = "https://ekg.example/id/mrp.production/3";
        const SO: &str = "https://ekg.example/id/sale.order/3";
        let infeasible = |commitment: &'static str| {
            row(&[
                ("mo", MO),
                ("so", SO),
                ("startedAt", "2027-10-11T08:00:00"),
                ("endedAt", "2027-10-11T10:00:00"),
                ("commitment", commitment),
                ("required", "180"),
            ])
        };
        let missed = infeasible("2027-10-11T09:00:00");
        let build = interpret_infeasibility_row(&missed).expect("the build is flagged");
        assert_eq!(build.subject.0, MO);
        let order = interpret_missed_promise_row(&missed).expect("the order is flagged");
        assert_eq!(order.subject.0, SO);
        for named in [MO, "2027-10-11T10:00:00", "2027-10-11T09:00:00"] {
            assert!(order.reason.contains(named), "{named} in {}", order.reason);
        }

        let overrun = infeasible("2027-10-12T00:00:00");
        assert!(interpret_infeasibility_row(&overrun).is_some());
        assert!(interpret_missed_promise_row(&overrun).is_none());
    }

    #[test]
    fn infeasibility_routing_too_short_for_the_planned_window_flags_the_order() {
        // Trigger 2: no commitment, but the routing (180) cannot fit the planned window
        // (08:00 to 10:00, 120 min).
        let verdict = interpret_infeasibility_row(&row(&[
            ("mo", "https://ekg.example/id/mrp.production/7"),
            ("startedAt", "2027-10-11T08:00:00"),
            ("endedAt", "2027-10-11T10:00:00"),
            ("required", "180"),
        ]))
        .expect("routing beyond the planned window is a verdict");
        assert!(verdict.reason.contains("180"));
        assert!(verdict.reason.contains("120"));
        assert!(verdict.reason.contains("planned window"));
    }

    #[test]
    fn infeasibility_finish_after_commitment_flags_even_without_routing() {
        // An order whose BOM has no routing (no `required`) but whose planned finish is
        // after its commitment is still flagged: trigger 1 does not need the duration sum.
        let verdict = interpret_infeasibility_row(&row(&[
            ("mo", "https://ekg.example/id/mrp.production/8"),
            ("startedAt", "2027-10-11T08:00:00"),
            ("endedAt", "2027-10-16T16:00:00"),
            ("commitment", "2027-10-11T00:00:00"),
        ]))
        .expect("a finish after the commitment is a verdict even with no routing");
        assert_eq!(verdict.subject.0, "https://ekg.example/id/mrp.production/8");
        assert!(verdict.reason.contains("finish"));
        assert!(verdict.reason.contains("commitment"));
    }

    #[test]
    fn infeasibility_without_a_commitment_or_finish_is_not_flagged() {
        // No commitment and no planned finish: neither trigger can hold.
        assert!(
            interpret_infeasibility_row(&row(&[
                ("mo", "https://ekg.example/id/mrp.production/5"),
                ("startedAt", "2027-10-11T08:00:00"),
                ("required", "600"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn iso_minutes_differences_are_correct() {
        let start = parse_iso_minutes("2027-10-11T08:00:00").expect("parse start");
        let end = parse_iso_minutes("2027-10-11T16:00:00").expect("parse end");
        assert_eq!(end - start, 480, "8 hours is 480 minutes");
        let next_day = parse_iso_minutes("2027-10-12T08:00:00").expect("parse next day");
        assert_eq!(next_day - start, 1440, "one day is 1440 minutes");
    }

    #[test]
    fn the_process_rules_carry_the_semantic_layer_selects() {
        assert!(process_contention_rule().sparql().contains("?overlapPeers"));
        assert!(
            process_infeasibility_rule()
                .sparql()
                .contains("SUM(?duration)")
        );
    }

    #[test]
    fn the_shared_rule_set_keeps_the_monitors_priority_order() {
        // Supply risk ahead of the missed promise on the same order, then infeasibility ahead
        // of contention (the coalesced reason's priority).
        let rules = monitor_rules();
        let sparql: Vec<&str> = rules.iter().map(RiskRule::sparql).collect();
        assert_eq!(
            sparql,
            [
                ontology::RISK_RULE_SELECT_SPARQL,
                ontology::PROCESS_INFEASIBILITY_SELECT_SPARQL,
                ontology::PROCESS_INFEASIBILITY_SELECT_SPARQL,
                ontology::PROCESS_CONTENTION_SELECT_SPARQL,
            ]
        );
    }

    #[test]
    fn notify_url_targets_the_monitor_call_endpoint() {
        let url = monitor_notify_url("http://localhost:8080").expect("url");
        assert_eq!(url, "http://localhost:8080/DriftMonitorService/notify");
    }

    #[test]
    fn notify_url_tolerates_a_trailing_slash() {
        let url = monitor_notify_url("http://localhost:8080/").expect("url");
        assert_eq!(url, "http://localhost:8080/DriftMonitorService/notify");
    }

    mod coupling {
        use super::super::*;

        fn row(pairs: &[(&str, &str)]) -> Row {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        }

        const SO: &str = "https://ekg.example/id/sale.order/1";
        const MO1: &str = "https://ekg.example/id/mrp.production/1";
        const MO2: &str = "https://ekg.example/id/mrp.production/2";
        const WC: &str = "https://ekg.example/id/mrp.workcenter/1";

        fn contention(capacity: &str, overlap_peers: &str) -> Row {
            row(&[
                ("a", MO1),
                ("b", MO2),
                ("kind", "contention"),
                ("via", WC),
                ("capacity", capacity),
                ("overlapPeers", overlap_peers),
            ])
        }

        #[test]
        fn a_fulfils_row_couples_the_order_and_its_manufacturing_order() {
            let edges =
                coupling_rule().edges(&[row(&[("a", SO), ("b", MO1), ("kind", "fulfills")])]);
            assert_eq!(edges.len(), 1);
            let edge = &edges[0];
            let ends = [edge.a.0.as_str(), edge.b.0.as_str()];
            assert!(ends.contains(&SO) && ends.contains(&MO1), "{edge:?}");
            assert!(edge.reason.contains("fulfils"), "{edge:?}");
        }

        #[test]
        fn an_over_capacity_contention_row_couples_the_two_orders() {
            let edges = coupling_rule().edges(&[contention("1", "1")]);
            assert_eq!(edges.len(), 1);
            assert_eq!((edges[0].a.0.as_str(), edges[0].b.0.as_str()), (MO1, MO2));
            assert!(edges[0].reason.contains(WC), "{:?}", edges[0]);
        }

        #[test]
        fn both_anchors_of_a_contending_pair_yield_one_edge() {
            let mut reverse = contention("1", "1");
            reverse.insert("a".to_owned(), MO2.to_owned());
            reverse.insert("b".to_owned(), MO1.to_owned());
            let edges = coupling_rule().edges(&[contention("1", "1"), reverse]);
            assert_eq!(edges.len(), 2);
            assert_eq!(
                edges[0], edges[1],
                "the same undirected edge, reason included"
            );
        }

        #[test]
        fn a_fulfils_edge_records_its_kind_and_keeps_its_reason() {
            let edges =
                coupling_rule().edges(&[row(&[("a", SO), ("b", MO1), ("kind", "fulfills")])]);
            let edge = &edges[0];
            assert_eq!(edge.reason, format!("linked: {MO1} fulfils {SO}"));
            assert_eq!(
                edge.facts,
                BTreeMap::from([("kind".to_owned(), "fulfills".to_owned())])
            );
        }

        #[test]
        fn a_contention_edge_records_its_work_center_and_capacity() {
            let edges = coupling_rule().edges(&[contention("1", "1")]);
            let edge = &edges[0];
            assert_eq!(
                edge.reason,
                format!(
                    "linked: {MO1} and {MO2} contend for work center {WC} beyond its capacity 1"
                )
            );
            assert_eq!(
                edge.facts,
                BTreeMap::from([
                    ("capacity".to_owned(), "1".to_owned()),
                    ("kind".to_owned(), "contention".to_owned()),
                    ("via".to_owned(), WC.to_owned()),
                ])
            );
        }

        #[test]
        fn a_within_capacity_contention_row_couples_nothing() {
            assert!(coupling_rule().edges(&[contention("2", "1")]).is_empty());
            assert!(coupling_rule().edges(&[contention("3", "2")]).is_empty());
        }

        #[test]
        fn a_row_missing_a_binding_yields_no_edge() {
            let rule = coupling_rule();
            assert!(
                rule.edges(&[row(&[("a", SO), ("kind", "fulfills")])])
                    .is_empty()
            );
            assert!(
                rule.edges(&[row(&[("a", SO), ("b", MO1)])]).is_empty(),
                "no kind"
            );
            assert!(
                rule.edges(&[row(&[
                    ("a", MO1),
                    ("b", MO2),
                    ("kind", "contention"),
                    ("via", WC),
                    ("capacity", "1")
                ])])
                .is_empty(),
                "no overlap count"
            );
            assert!(
                rule.edges(&[row(&[("a", SO), ("b", MO1), ("kind", "shares_a_supplier")])])
                    .is_empty(),
                "an unknown kind"
            );
            assert!(
                rule.edges(&[contention("one", "1")]).is_empty(),
                "malformed"
            );
        }
    }
}
