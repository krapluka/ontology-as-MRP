//! The neutral GraphQL output types and their mappings from the durable/domain types.
//!
//! Every type names a plan, option, change, metric, trace, lane, tab, applied change or
//! policy, never a capability (supply-risk, process) or a metric (delivery, cost, risk).
//! Capability-specific values ride the opaque [`Metric`] bag, which the schema does not
//! enumerate by name, so the same types serve a supply-risk plan and a
//! manufacturing-process plan.

use std::collections::BTreeMap;

use async_graphql::{ComplexObject, Context, Enum, ID, SimpleObject};
use ekg_domain::{
    ActionPoint, ActionPointState, ExecutionReceipt, Need, Plan, ProposedAction, RankedPlan, Score,
    Trace, TraceStep, TraceTag,
};

use super::GraphqlState;
use super::projections;

/// One entry of the opaque, capability-filled metrics bag. The schema enumerates no metric
/// name: `key`/`value` carry whatever the capability's scorer (and the derived display
/// metrics) put there. Delivery/cost ride this bag unnamed.
#[derive(Debug, Clone, SimpleObject)]
pub struct Metric {
    /// The capability-defined metric name (opaque to the contract).
    pub key: String,
    /// Its value, stringly like every neutral field.
    pub value: String,
}

/// A capability-filled [`Score`]: the opaque metrics bag plus the capability-defined
/// ranking value (`rank_key`; lexically smaller sorts first, so best-first). Reused for a
/// plan's do-nothing baseline and for each option.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "Score")]
pub struct ScoreView {
    /// The opaque metrics bag.
    pub metrics: Vec<Metric>,
    /// The capability-defined ordering value (best-first). Ranking is authoritative;
    /// the derived display metrics never change it.
    pub rank_key: String,
}

/// The kind of a reasoning-trace step, mirroring [`TraceTag`] (Art. 14 comprehension).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
#[graphql(name = "TraceTag")]
pub enum TraceTagView {
    /// A candidate plan was projected onto a what-if and scored.
    Simulated,
    /// A traversal of the governed evidence subgraph the reasoning rested on.
    Graph,
    /// An open assumption forced by a grounding omission.
    Assumption,
    /// A model generation attempt that yielded no candidate (its failure class and
    /// reason), or the time budget ending generation. Set aside, never simulated.
    Generation,
}

/// One reasoning-trace step: a tag, a human line, and whether the candidate it describes
/// was tried and set aside, which keeps rejected candidates visible to oversight.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "TraceStep")]
pub struct TraceStepView {
    /// Which kind of reasoning this step records.
    pub tag: TraceTagView,
    /// The human-readable line surfaced for oversight.
    pub text: String,
    /// Whether this step describes a candidate that was tried and set aside.
    pub rejected: bool,
}

/// Summary counts over a trace, so oversight sees the reasoning shape at a glance.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "TraceSummary")]
pub struct TraceSummaryView {
    /// How many candidate plans were simulated (incl. the do-nothing baseline).
    pub simulated: i32,
    /// How many steps describe a candidate that was tried and set aside.
    pub rejected: i32,
    /// How many open-assumption steps the trace carries.
    pub open_assumptions: i32,
}

/// A structured reasoning trace: the ordered steps plus the derived summary. The
/// comprehension surface for meaningful human oversight (EU AI Act Art. 14/12).
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "Trace")]
pub struct TraceView {
    /// The reasoning steps, in the order the loop performed them.
    pub steps: Vec<TraceStepView>,
    /// Counts derived from the steps (cannot disagree with them).
    pub summary: TraceSummaryView,
}

/// A structured reason a candidate cannot be auto-applied and needs a human: the policy
/// predicate it fails, carried as data so oversight sees why a plan waits without parsing
/// prose.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "Need")]
pub struct NeedView {
    /// A stable, capability-defined reason code.
    pub code: String,
    /// A human-readable detail for the reason.
    pub detail: String,
}

/// One parameter of a change (e.g. the new date), stringly like the neutral core.
#[derive(Debug, Clone, SimpleObject)]
pub struct ChangeParam {
    /// The parameter name.
    pub key: String,
    /// Its value.
    pub value: String,
}

/// One governed change an option would commit: the verb, the target object, its
/// parameters (the intended new values) and the target's from value. An executed change
/// (an auto-applied one) also carries the recorded inverse that reverses it; for an option
/// not yet promoted the inverse is `null`, since it is only recorded once the write-back
/// runs. The `params` carry the to value, so `from` plus `params` give the change's from
/// and to.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(complex, name = "Change")]
pub struct ChangeView {
    /// The governed verb (defined by the kinetic layer).
    pub verb: String,
    /// The entity the change targets.
    pub object: ID,
    /// The change parameters (the intended new value(s)), key-ordered.
    pub params: Vec<ChangeParam>,
    /// The recorded inverse that reverses this change, present only once executed.
    pub inverse: Option<String>,
    /// The from value when it is already known: for an executed change, the recorded prior
    /// value the inverse restores. `None` for an option not yet promoted, whose `from` the
    /// resolver reads live from the world model. Not a schema field.
    #[graphql(skip)]
    pub known_from: Option<String>,
}

#[ComplexObject]
impl ChangeView {
    /// The change's from value: the target's current value of the fact the verb moves, as
    /// the capability reads it. An expedite's from is its line's current arrival, and a change
    /// that moves no fact (a notification) has none. For an executed change this is the
    /// recorded prior; for an option not yet promoted it is read live from the world model.
    /// `null` when the fact is unknown or unread.
    async fn from(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<String>> {
        if self.known_from.is_some() {
            return Ok(self.known_from.clone());
        }
        // An executed change with no recorded prior: the current value is the post-write
        // value, not the from, so there is nothing to return.
        if self.inverse.is_some() {
            return Ok(None);
        }
        let state = ctx.data::<GraphqlState>()?;
        Ok(projections::change_from(state, &self.object.0, &self.verb).await)
    }

    /// A display label for the change's object, composed by the capability from the
    /// materialized names (for example an order line as its order reference, open quantity
    /// and product). `null` when no name is materialized; the view then falls back to the
    /// object id.
    async fn label(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<String>> {
        let state = ctx.data::<GraphqlState>()?;
        Ok(projections::change_label(state, &self.object.0).await)
    }
}

/// The lifecycle state of an action point, mirroring [`ActionPointState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
#[graphql(name = "ActionPointState")]
pub enum ActionPointStateView {
    /// Raised by the monitor; awaiting a proposal.
    Open,
    /// The bounded agent attached a grounded proposal.
    Proposed,
    /// A human is reviewing (oversight in progress).
    UnderReview,
    /// A human (or the auto-apply policy) promoted the plan; the write-back ran.
    Promoted,
    /// A human aborted it via the stop control.
    Aborted,
    /// A promoted change was reversed through its inverse.
    Reversed,
    /// It expired via an escalation timer without promotion.
    Expired,
}

/// An action point as the list card shows it: its id, the subject it is keyed by, its
/// current state, the deterministic reason the monitor opened it, and its short digest.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "ActionPoint")]
pub struct ActionPointView {
    /// Stable action-point id.
    pub id: ID,
    /// The subject entity this point is keyed by.
    pub subject: ID,
    /// Current lifecycle state.
    pub state: ActionPointStateView,
    /// The deterministic reason the monitor opened it (not model-generated).
    pub reason: String,
    /// The point's short digest: the digest that names a portfolio and a plan, over the
    /// point's subject, so a client can show a short, reproducible point id without
    /// re-implementing the function.
    pub digest: String,
    /// The point's short display code: four hex digits from [`display_code`] over its id, shown
    /// as `AP-XXXX`.
    pub code: String,
}

/// One ranked option on a plan's slate: its changes, the policy `needs` that keep it
/// human-gated, whether it is recommended, its reasoning-trace slice, and (resolved on
/// demand) its opaque display metrics and before/after work-center lanes.
///
/// The plan, subject, and the scorer's raw metrics are carried but not exposed as fields
/// (`skip`); they back the derived `metrics`/`lanes` resolvers.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(complex, name = "PlanOption")]
pub struct OptionCard {
    /// Whether this option is the agent's recommended one (at most one per slate).
    pub recommended: bool,
    /// The capability-defined ordering value (best-first); ranking is authoritative.
    pub rank_key: String,
    /// The policy reasons this option cannot be auto-applied (empty if none).
    pub needs: Vec<NeedView>,
    /// The governed changes this option would commit, in listed order.
    pub changes: Vec<ChangeView>,
    /// This option's reasoning-trace slice.
    pub trace: TraceView,
    /// The scorer's raw opaque metrics (merged with the derived display metrics on read).
    #[graphql(skip)]
    pub raw_metrics: BTreeMap<String, String>,
    /// The candidate plan (backs the derived metrics/lanes; not exposed directly).
    #[graphql(skip)]
    pub plan: Plan,
    /// The point the option is framed through: the plan's lead point (backs the derived
    /// metrics; not exposed directly).
    #[graphql(skip)]
    pub subject: String,
    /// Every point the plan resolves (backs the merged lanes; not exposed directly).
    #[graphql(skip)]
    pub members: Vec<String>,
}

#[ComplexObject]
impl OptionCard {
    /// The opaque display-metrics bag: the scorer's stored metrics plus the delivery and cost
    /// display metrics derived read-only from this option's plan. Display-only: they do not
    /// change the ranking (`rank_key`). Computed only when the field is selected.
    async fn metrics(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Metric>> {
        let state = ctx.data::<GraphqlState>()?;
        let mut merged = self.raw_metrics.clone();
        // Derived display metrics are additive: never clobber a metric the scorer set.
        for (key, value) in
            projections::derive_display_metrics(state, &self.plan, &self.subject).await?
        {
            merged.entry(key).or_insert(value);
        }
        Ok(metrics_from(&merged))
    }

    /// The before/after per-work-center lanes for this option: the current world-model
    /// schedule ("before") and the schedule under this option's proposed changes ("after"),
    /// derived as a projection read that writes nothing. Computed only when the field is
    /// selected.
    async fn lanes(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Lane>> {
        let state = ctx.data::<GraphqlState>()?;
        projections::option_lanes(state, &self.members, &self.plan).await
    }

    /// The option's scope: how many distinct objects its changes touch, per kind, counted
    /// by the capability's stated rule over the world model. The kinds are data, never schema
    /// names; a kind with none is omitted, and a failed read omits the scope rather than
    /// estimating it. Computed only when the field is selected.
    async fn scope(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<ScopeCount>> {
        let state = ctx.data::<GraphqlState>()?;
        Ok(projections::option_scope(state, &self.plan)
            .await
            .into_iter()
            .map(ScopeCount::from)
            .collect())
    }
}

/// An entity id's display label: the name an operator reads for it, composed by the
/// capability from the world model. A display projection (texts keep their ids), served
/// only for an id the world can name.
#[derive(Debug, Clone, PartialEq, Eq, SimpleObject)]
pub struct EntityLabel {
    /// The entity id, exactly as asked.
    pub id: ID,
    /// The label to show for it.
    pub label: String,
}

/// One kind's count in an option's scope: a neutral kind label from the capability's fixed
/// vocabulary (data, not a schema name) and how many distinct objects of it the option's
/// changes touch.
#[derive(Debug, Clone, PartialEq, Eq, SimpleObject)]
pub struct ScopeCount {
    /// The kind of object counted (opaque to the contract).
    pub kind: String,
    /// How many distinct objects of that kind the changes touch (never 0).
    pub count: i32,
}

impl From<ekg_semantic::display::ScopeCount> for ScopeCount {
    fn from(count: ekg_semantic::display::ScopeCount) -> Self {
        Self {
            kind: count.kind.to_owned(),
            count: i32::try_from(count.count).unwrap_or(i32::MAX),
        }
    }
}

/// A plan: a ranked slate over the points it resolves. `resolves` is a list, so a plan that
/// resolves one point and a portfolio's joint plan that resolves several are the same
/// contract.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "Plan")]
pub struct PlanView {
    /// The action-point subject this plan was read for (a listed plan: its lead point).
    pub subject: ID,
    /// The action points this plan resolves: the one point, or every member of its portfolio.
    pub resolves: Vec<ID>,
    /// The point whose baseline frames the plan: the one it resolves, or the member with the
    /// worst baseline. The baseline and every option's display metrics are judged for it.
    pub lead: ID,
    /// The do-nothing baseline score (the outcome if no action is taken).
    pub baseline: ScoreView,
    /// The ranked options, best-first.
    pub options: Vec<OptionCard>,
    /// The slate-level reasoning trace over the whole loop (incl. rejected candidates).
    pub trace: TraceView,
}

/// One before/after bar on a work-center lane: a work order, its planned window, its row
/// (for stacking), and whether this option's changes moved it.
#[derive(Debug, Clone, SimpleObject)]
pub struct Bar {
    /// The work order this bar represents.
    pub work_order: ID,
    /// Planned start (as materialized).
    pub start: String,
    /// Planned end (as materialized).
    pub end: String,
    /// The row for stacking overlapping bars on the lane.
    pub row: i32,
    /// Whether this option's proposed changes moved this bar between before and after.
    pub changed: bool,
}

/// A work-center lane: its capacity band plus the "before" (current) and "after" (under
/// the option's changes) bars. A projection read, never a persisted artifact.
#[derive(Debug, Clone, SimpleObject)]
pub struct Lane {
    /// The work-center agent this lane belongs to.
    pub work_center: ID,
    /// The agent's parallel capacity, if materialized.
    pub capacity: Option<f64>,
    /// The current schedule's bars.
    pub before: Vec<Bar>,
    /// The schedule's bars under this option's proposed changes.
    pub after: Vec<Bar>,
}

/// Which read-model tab a [`ReadModelView`] projects. Neutral: the tab kind selects the
/// projection query; the returned shape is the same neutral rows for every tab. Every tab is
/// plant-wide; the subject only decides what is lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum TabKind {
    /// Every order against its promise date, plus the inbound purchase orders; the subject's
    /// order is marked.
    Timeline,
    /// The plant-wide dependency graph from orders to their inbound sources, with the
    /// subject's path lit and its cause nodes marked.
    Graph,
    /// The plant's flow stages, from sources to promised orders, with the subject's chain
    /// lit across them.
    Flow,
    /// The whole supplier base with lead-time, delivery and exposure signals; the subject's
    /// supplier is marked.
    Suppliers,
}

/// One field of a projection row (a SPARQL solution binding of a variable to a term).
#[derive(Debug, Clone, SimpleObject)]
pub struct Field {
    /// The projection column / SPARQL variable name.
    pub key: String,
    /// Its term value.
    pub value: String,
}

/// One row of a read-model tab projection: a neutral set of key/value fields (a SPARQL
/// solution). The tabs differ by which projection query runs, not by the row shape.
#[derive(Debug, Clone, SimpleObject)]
pub struct ProjectionRow {
    /// The row's fields, key-ordered.
    pub fields: Vec<Field>,
}

/// A read-model tab projection for a subject: the tab kind and its rows, each row a neutral
/// projection of the world model. Non-authoritative: reading mutates nothing.
#[derive(Debug, Clone, SimpleObject)]
pub struct ReadModelView {
    /// The subject the tab was projected for.
    pub subject: ID,
    /// Which tab this is.
    pub tab: TabKind,
    /// The projected rows.
    pub rows: Vec<ProjectionRow>,
}

/// One policy criterion an auto-applied change satisfied (or the active policy exposes):
/// a rule name and its threshold/value.
#[derive(Debug, Clone, SimpleObject)]
pub struct Criterion {
    /// The rule (e.g. `cost_within_limit`, `single_step_reversible`).
    pub rule: String,
    /// Its threshold / value (e.g. the cost limit).
    pub value: String,
}

/// A change an agent auto-applied under policy, listed with the criteria it satisfied
/// and the window within which it may still be reversed. The changes carry their recorded
/// inverse (they are executed), so the governed undo can reverse them.
#[derive(Debug, Clone, SimpleObject)]
pub struct AppliedChange {
    /// The action point whose plan was auto-applied.
    pub action_point: ID,
    /// The subject the plan resolved.
    pub subject: ID,
    /// The change's short display code, shown as `AU-XXXX`: [`display_code`] over
    /// `applied/` plus the action point's id.
    pub code: String,
    /// The display code of the action point it closed, shown as `AP-XXXX`: the point's own
    /// [`ActionPointView::code`].
    pub action_point_code: String,
    /// The changes that were committed, each with its recorded inverse.
    pub changes: Vec<ChangeView>,
    /// The policy criteria this auto-apply satisfied.
    pub criteria: Vec<Criterion>,
    /// When it was applied, as Unix epoch seconds (stringly).
    pub applied_at: Option<String>,
    /// The end of the reversal window, as Unix epoch seconds; an undo after this is
    /// refused. `null` if the applied time is unknown.
    pub undo_until: Option<String>,
    /// Whether every committed change is reversible (an undo can reverse the whole plan).
    pub reversible: bool,
    /// The opaque, capability-filled display bag for this auto-applied change: a composed
    /// `title` (its verbs) and `effect` sentence (its changes), so the view need not fabricate
    /// prose. Rides the same neutral [`Metric`] container as a score's metrics; the contract
    /// names none of the keys.
    pub metrics: Vec<Metric>,
}

/// The active auto-apply policy as the operator sees it: the criteria a change must
/// meet, the reversal window, and the pre-authorized verbs.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(name = "Policy")]
pub struct PolicyView {
    /// The policy version id.
    pub policy_id: String,
    /// The criteria a change must satisfy to be auto-applied.
    pub criteria: Vec<Criterion>,
    /// The currency the cost limit is expressed in (ISO 4217 code), when configured.
    pub cost_limit_currency: Option<String>,
    /// A human-readable statement of the reversal window (e.g. "reversible for 8h").
    pub undo_window: String,
    /// The verbs the policy pre-authorizes for auto-apply.
    pub auto_apply_actions: Vec<String>,
}

// --- mappings ---

/// Maps an opaque metrics bag to the neutral `[Metric]` list (key-ordered by the `BTreeMap`).
#[must_use]
pub fn metrics_from(map: &BTreeMap<String, String>) -> Vec<Metric> {
    map.iter()
        .map(|(key, value)| Metric {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// Adds the display keys that need no world or projection read: the capability severity
/// tier, a pure function of the score's own risk metric. It only fills a key the scorer
/// left unset and never changes the ranking. The projection-derived display keys (projected
/// date, day-slip, order value) are merged separately by the `metrics` resolver.
#[must_use]
pub fn with_derived_display(metrics: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut enriched = metrics.clone();
    if let Some(severity) = ekg_semantic::display::severity_tier(metrics) {
        enriched
            .entry(ekg_semantic::display::SEVERITY_KEY.to_owned())
            .or_insert_with(|| severity.to_owned());
    }
    enriched
}

impl From<&Score> for ScoreView {
    fn from(score: &Score) -> Self {
        Self {
            metrics: metrics_from(&with_derived_display(&score.metrics)),
            rank_key: score.rank_key.clone(),
        }
    }
}

impl From<TraceTag> for TraceTagView {
    fn from(tag: TraceTag) -> Self {
        match tag {
            TraceTag::Simulated => Self::Simulated,
            TraceTag::Graph => Self::Graph,
            TraceTag::Assumption => Self::Assumption,
            TraceTag::Generation => Self::Generation,
        }
    }
}

impl From<&TraceStep> for TraceStepView {
    fn from(step: &TraceStep) -> Self {
        Self {
            tag: step.tag.into(),
            text: step.text.clone(),
            rejected: step.rejected,
        }
    }
}

impl From<&Trace> for TraceView {
    fn from(trace: &Trace) -> Self {
        Self {
            steps: trace.steps.iter().map(TraceStepView::from).collect(),
            summary: TraceSummaryView {
                simulated: i32::try_from(trace.summary.simulated).unwrap_or(i32::MAX),
                rejected: i32::try_from(trace.summary.rejected).unwrap_or(i32::MAX),
                open_assumptions: i32::try_from(trace.summary.open_assumptions).unwrap_or(i32::MAX),
            },
        }
    }
}

impl From<&Need> for NeedView {
    fn from(need: &Need) -> Self {
        Self {
            code: need.code.clone(),
            detail: need.detail.clone(),
        }
    }
}

impl From<ActionPointState> for ActionPointStateView {
    fn from(state: ActionPointState) -> Self {
        match state {
            ActionPointState::Open => Self::Open,
            ActionPointState::Proposed => Self::Proposed,
            ActionPointState::UnderReview => Self::UnderReview,
            ActionPointState::Promoted => Self::Promoted,
            ActionPointState::Aborted => Self::Aborted,
            ActionPointState::Reversed => Self::Reversed,
            ActionPointState::Expired => Self::Expired,
        }
    }
}

impl From<&ActionPoint> for ActionPointView {
    fn from(point: &ActionPoint) -> Self {
        Self {
            id: ID(point.id.0.clone()),
            subject: ID(point.subject.0.clone()),
            state: point.state.into(),
            reason: point.reason.clone(),
            digest: ekg_domain::short_digest(point.subject.0.as_bytes()),
            code: display_code(&point.id.0),
        }
    }
}

/// A short display code for an object with id `seed`: four upper-case hex digits of the
/// seed's [`ekg_domain::short_digest`], first passed through the 64-bit MurmurHash3 finalizer.
/// FNV-1a barely carries a change in the last bytes into its leading digits, so sequential
/// ids (`…/1`, `…/2`) would share a prefix; the finalizer mixes every bit into every digit.
/// Display-only: a code can collide, and commands never use it.
#[must_use]
pub fn display_code(seed: &str) -> String {
    let mut h = u64::from_str_radix(&ekg_domain::short_digest(seed.as_bytes()), 16).unwrap_or(0);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    format!("{:04X}", h >> 48)
}

/// Maps a proposed action to a change with no recorded inverse (an option not yet
/// executed). Its `from` is unknown here; the resolver reads it live from the world model.
#[must_use]
pub fn change_from_action(action: &ProposedAction) -> ChangeView {
    ChangeView {
        verb: action.action_type.0.clone(),
        object: ID(action.target.0.clone()),
        params: params_of(&action.params),
        inverse: None,
        known_from: None,
    }
}

/// Maps an execution receipt to a change carrying its recorded inverse (an executed change).
/// Its `from` is the recorded prior value the inverse restores, decoded from the receipt's
/// undo payload, or `None` if the payload records no prior field value (e.g. a created
/// record) or cannot be decoded.
#[must_use]
pub fn change_from_receipt(receipt: &ExecutionReceipt) -> ChangeView {
    ChangeView {
        verb: receipt.action.action_type.0.clone(),
        object: ID(receipt.action.target.0.clone()),
        params: params_of(&receipt.action.params),
        inverse: Some(receipt.external_ref.clone()),
        known_from: prior_from_undo(&receipt.external_ref),
    }
}

/// The prior field value recorded in an undo payload: the `from` of an executed change.
/// `None` for a non-field undo (an unlinked created record) or an undecodable or legacy ref.
fn prior_from_undo(external_ref: &str) -> Option<String> {
    let prior = match ekg_kinetic::verbs::Undo::decode(external_ref).ok()? {
        ekg_kinetic::verbs::Undo::RestoreField { prior, .. } => prior,
        // A multi-field restore (an expedite) records the target's own field first; the
        // receipts it also moved are not the change's object.
        ekg_kinetic::verbs::Undo::RestoreFields { mut restores } => {
            if restores.is_empty() {
                return None;
            }
            restores.swap_remove(0).prior
        }
        ekg_kinetic::verbs::Undo::UnlinkRecord { .. } => return None,
        // A split's from is the order's quantity before it was split.
        ekg_kinetic::verbs::Undo::SplitProduction { prior_qty, .. } => prior_qty,
    };
    match prior {
        serde_json::Value::String(value) => Some(value),
        serde_json::Value::Number(value) => Some(value.to_string()),
        // A JSON `false` or null prior means the field was unset, so there is no from.
        _ => None,
    }
}

/// Composes an auto-applied change's title/effect display bag from its changes: a `title`
/// of its distinct verbs and an `effect` sentence of `verb object → to` per change, so the
/// view does not have to write prose. Carried in the same neutral [`Metric`] container.
/// Empty for a change with no committed actions.
#[must_use]
pub fn applied_display(changes: &[ChangeView]) -> Vec<Metric> {
    if changes.is_empty() {
        return Vec::new();
    }
    let mut verbs: Vec<String> = Vec::new();
    for change in changes {
        let humanized = humanize_verb(&change.verb);
        if !verbs.contains(&humanized) {
            verbs.push(humanized);
        }
    }
    let effect = changes
        .iter()
        .map(|change| {
            let object = short_object(&change.object.0);
            let to: Vec<&str> = change
                .params
                .iter()
                .map(|param| param.value.as_str())
                .filter(|value| !value.is_empty())
                .collect();
            if to.is_empty() {
                format!("{} {object}", humanize_verb(&change.verb))
            } else {
                format!(
                    "{} {object} → {}",
                    humanize_verb(&change.verb),
                    to.join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    vec![
        Metric {
            key: "title".to_owned(),
            value: verbs.join(" · "),
        },
        Metric {
            key: "effect".to_owned(),
            value: effect,
        },
    ]
}

/// Humanizes a snake_case governed verb for display: `re_promise_date` becomes
/// `Re promise date`.
fn humanize_verb(verb: &str) -> String {
    let spaced = verb.replace('_', " ");
    let mut chars = spaced.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or(spaced)
}

/// The short form of a target IRI for display: its `<model>/<id>` tail, or the value
/// unchanged if it is not an ontology id IRI.
fn short_object(object: &str) -> String {
    object
        .strip_prefix(ekg_semantic::ontology::ID_NAMESPACE)
        .unwrap_or(object)
        .to_owned()
}

/// Maps an action's parameter map to the neutral key-ordered `[ChangeParam]`.
fn params_of(params: &BTreeMap<String, String>) -> Vec<ChangeParam> {
    params
        .iter()
        .map(|(key, value)| ChangeParam {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// Maps one ranked option to an [`OptionCard`] framed through `subject` (the plan's lead
/// point), over the plan's `members`. The subject backs the derived-metrics resolver, the
/// members the lanes; the raw score metrics back the derived metrics.
#[must_use]
pub fn option_card(option: &RankedPlan, subject: &str, members: &[String]) -> OptionCard {
    OptionCard {
        recommended: option.recommended,
        rank_key: option.score.rank_key.clone(),
        needs: option.needs.iter().map(NeedView::from).collect(),
        changes: option.plan.actions.iter().map(change_from_action).collect(),
        trace: TraceView::from(&option.trace),
        raw_metrics: with_derived_display(&option.score.metrics),
        plan: option.plan.clone(),
        subject: subject.to_owned(),
        members: members.to_vec(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ekg_domain::{ActionType, EntityUri};

    use super::*;

    #[test]
    fn option_card_maps_the_neutral_fields_of_a_ranked_plan() {
        let option = RankedPlan {
            plan: Plan {
                actions: vec![ProposedAction {
                    action_type: ActionType("expedite_po".to_owned()),
                    target: EntityUri("https://ekg.example/id/purchase.order/7".to_owned()),
                    params: BTreeMap::from([("new_date".to_owned(), "2027-10-16".to_owned())]),
                }],
            },
            score: Score {
                metrics: BTreeMap::from([("risk_left".to_owned(), "0".to_owned())]),
                rank_key: "00000".to_owned(),
            },
            recommended: true,
            needs: vec![Need {
                code: "cost_above_limit".to_owned(),
                detail: "320 > 250".to_owned(),
            }],
            omissions: Vec::new(),
            trace: Trace::from_steps(vec![
                TraceStep {
                    tag: TraceTag::Graph,
                    text: "read the PO".to_owned(),
                    rejected: false,
                },
                TraceStep {
                    tag: TraceTag::Simulated,
                    text: "a cheaper candidate left shortfall".to_owned(),
                    rejected: true,
                },
            ]),
        };
        let card = option_card(
            &option,
            "https://ekg.example/id/sale.order/1",
            &["https://ekg.example/id/sale.order/1".to_owned()],
        );

        assert!(card.recommended);
        assert_eq!(card.rank_key, "00000");
        // The policy `needs` are carried as data.
        assert_eq!(card.needs.len(), 1);
        assert_eq!(card.needs[0].code, "cost_above_limit");
        // The change carries verb, object and params; no inverse, as it is not yet executed.
        assert_eq!(card.changes.len(), 1);
        assert_eq!(card.changes[0].verb, "expedite_po");
        assert_eq!(
            card.changes[0].object.0,
            "https://ekg.example/id/purchase.order/7"
        );
        assert_eq!(card.changes[0].params[0].key, "new_date");
        assert!(card.changes[0].inverse.is_none());
        // The reasoning trace keeps the rejected candidate.
        assert!(card.trace.steps.iter().any(|step| step.rejected));
        assert_eq!(card.trace.summary.rejected, 1);
        // The scorer's raw metrics are preserved for the derived-metrics resolver.
        assert_eq!(
            card.raw_metrics.get("risk_left").map(String::as_str),
            Some("0")
        );
        // The severity tier (a pure display derivation) rides the same opaque bag.
        assert_eq!(
            card.raw_metrics.get("severity").map(String::as_str),
            Some("on_track"),
            "a cleared option (risk_left 0) is on track"
        );
        // A change not yet executed carries no recorded prior; the resolver reads its
        // `from` live from the world model.
        assert!(card.changes[0].known_from.is_none());
    }

    #[test]
    fn display_codes_differ_for_sequential_ids_and_are_stable() {
        // Plain FNV-1a gives mrp.production/1, /2 and /3 the same leading digits.
        let codes: Vec<String> = (1..=3)
            .map(|n| {
                display_code(&format!(
                    "action-point/https://ekg.example/id/mrp.production/{n}"
                ))
            })
            .collect();
        assert_eq!(codes.len(), 3);
        assert!(
            codes[0] != codes[1] && codes[1] != codes[2] && codes[0] != codes[2],
            "{codes:?}"
        );
        for code in &codes {
            assert_eq!(code.len(), 4);
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
            );
        }
        assert_eq!(
            display_code("action-point/https://ekg.example/id/mrp.production/1"),
            codes[0],
            "reproducible"
        );
        // Over 1000 sequential ids, codes spread (no systematic shared prefix).
        let spread: std::collections::BTreeSet<String> = (0..1000)
            .map(|n| display_code(&format!("plan/{n}")))
            .collect();
        assert!(spread.len() > 950, "{} distinct of 1000", spread.len());
    }

    #[test]
    fn a_scope_count_maps_to_the_neutral_kind_and_count() {
        let counts = [
            ekg_semantic::display::ScopeCount {
                kind: ekg_semantic::display::SCOPE_ORDER,
                count: 2,
            },
            ekg_semantic::display::ScopeCount {
                kind: ekg_semantic::display::SCOPE_PURCHASE_ORDER,
                count: 1,
            },
        ];
        let mapped: Vec<ScopeCount> = counts.into_iter().map(ScopeCount::from).collect();
        assert_eq!(
            mapped,
            [
                ScopeCount {
                    kind: "order".to_owned(),
                    count: 2
                },
                ScopeCount {
                    kind: "purchase order".to_owned(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn with_derived_display_adds_the_capability_severity_tier() {
        // Severity rides the opaque bag, derived from risk_left, and never clobbers a key the
        // scorer already set.
        let one_at_risk =
            with_derived_display(&BTreeMap::from([("risk_left".to_owned(), "1".to_owned())]));
        assert_eq!(
            one_at_risk.get("severity").map(String::as_str),
            Some("high")
        );
        // A capability that fills no risk count gets no fabricated severity.
        assert!(!with_derived_display(&BTreeMap::new()).contains_key("severity"));
    }

    #[test]
    fn applied_display_composes_title_and_effect_from_the_changes() {
        // The title and effect are derived from the committed changes.
        let changes = vec![
            change_from_action(&ProposedAction {
                action_type: ActionType("expedite_po".to_owned()),
                target: EntityUri("https://ekg.example/id/purchase.order/95".to_owned()),
                params: BTreeMap::from([("new_date".to_owned(), "2027-11-01".to_owned())]),
            }),
            change_from_action(&ProposedAction {
                action_type: ActionType("notify_owner".to_owned()),
                target: EntityUri("https://ekg.example/id/purchase.order/95".to_owned()),
                params: BTreeMap::new(),
            }),
        ];
        let display = applied_display(&changes);
        let get = |key: &str| {
            display
                .iter()
                .find(|m| m.key == key)
                .map(|m| m.value.as_str())
        };
        assert_eq!(get("title"), Some("Expedite po · Notify owner"));
        assert_eq!(
            get("effect"),
            Some("Expedite po purchase.order/95 → 2027-11-01; Notify owner purchase.order/95")
        );
        // An empty change set composes nothing.
        assert!(applied_display(&[]).is_empty());
    }

    #[test]
    fn change_from_receipt_recovers_the_recorded_prior_as_from() {
        // An executed change's from is the prior value the inverse restores, decoded from the
        // receipt's undo payload, not a re-read of the current (post-write) value.
        use ekg_domain::{ExecutionReceipt, Revision};
        let undo = ekg_kinetic::verbs::Undo::RestoreField {
            model: "sale.order".to_owned(),
            id: 7,
            field: "commitment_date".to_owned(),
            prior: serde_json::Value::String("2027-10-18".to_owned()),
            written: None,
        };
        let receipt = ExecutionReceipt {
            action: ProposedAction {
                action_type: ActionType("re_promise_date".to_owned()),
                target: EntityUri("https://ekg.example/id/sale.order/7".to_owned()),
                params: BTreeMap::from([("new_date".to_owned(), "2027-10-30".to_owned())]),
            },
            external_ref: undo.encode().expect("encode undo"),
            source_revision: Revision(1),
            reversible: true,
        };
        let change = change_from_receipt(&receipt);
        assert_eq!(
            change.known_from.as_deref(),
            Some("2027-10-18"),
            "from = the prior"
        );
        // An unset (JSON false) prior records no from.
        let unset = ekg_kinetic::verbs::Undo::RestoreField {
            model: "sale.order".to_owned(),
            id: 7,
            field: "commitment_date".to_owned(),
            prior: serde_json::Value::Bool(false),
            written: None,
        };
        assert!(prior_from_undo(&unset.encode().expect("encode")).is_none());
        // An expedite's multi-field restore: the from is the target line's own prior,
        // not the receipt it also moved.
        let restore = |model: &str, prior: &str| ekg_kinetic::verbs::FieldRestore {
            model: model.to_owned(),
            id: 1,
            field: "date".to_owned(),
            prior: serde_json::Value::String(prior.to_owned()),
            watermark: 1,
            written: None,
        };
        let expedite = ekg_kinetic::verbs::Undo::RestoreFields {
            restores: vec![
                restore("purchase.order.line", "2027-11-15 08:00:00"),
                restore("stock.picking", "2027-11-18 08:00:00"),
            ],
        };
        assert_eq!(
            prior_from_undo(&expedite.encode().expect("encode")).as_deref(),
            Some("2027-11-15 08:00:00")
        );
    }
}
