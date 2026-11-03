//! A paired report over the shadow records: for every shadow run, the agent's slate beside
//! the slate the point got, and what was decided on the point. A joint record is listed the
//! same way, for its portfolio: both joint slates, and what was decided on the portfolio.
//!
//! [`build`] is a pure function of the records and, for a record without a decision, the
//! subject's point record or the portfolio's record. [`collect`] reads those over the Restate
//! ingress, after listing the keys that have records through the admin introspection. Nothing
//! is written: a record that lacks a decision is shown with the point's or the portfolio's,
//! never settled in the store.
//!
//! The report counts and compares. It says nothing about whether the agent is good enough.
//! Point records and joint records are counted apart, so a count over one kind is never read
//! as a count over both.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use ekg_domain::{ActionPointState, EntityUri, ProposedAction, Score, Slate};
use ekg_dynamic::{ActionPointRecord, PortfolioEvent, PortfolioRecord};
use serde::{Deserialize, Serialize};

use crate::rollout::call_object;
use crate::shadow::{
    AGENT_SHADOW_OBJECT, DecisionState, Paired, ShadowClient, ShadowGroup, ShadowOutcome,
    ShadowRecord, Shown, VersionTuple,
};

/// The registered name of the action-point object.
const ACTION_POINT_OBJECT: &str = "ActionPointService";
/// The registered name of the portfolio object.
const PORTFOLIO_OBJECT: &str = "PortfolioService";

/// What a record without a decision is completed from: its subject's point record, or its
/// portfolio's record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// The point record of each subject that has an undecided point record.
    pub points: BTreeMap<EntityUri, ActionPointRecord>,
    /// The record of each portfolio that has an undecided joint record, by id.
    pub portfolios: BTreeMap<String, PortfolioRecord>,
}

impl From<BTreeMap<EntityUri, ActionPointRecord>> for Sources {
    fn from(points: BTreeMap<EntityUri, ActionPointRecord>) -> Self {
        Self {
            points,
            portfolios: BTreeMap::new(),
        }
    }
}

/// Whether a record is of a point's run or of a portfolio's joint run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A point's run.
    #[default]
    Point,
    /// A portfolio's joint run.
    Joint,
}

/// A slate's best option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BestOption {
    /// Its actions, in order.
    pub actions: Vec<ProposedAction>,
    /// Its score.
    pub score: Score,
}

/// How the two slates' best options compare. Exactly one holds for a compared record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// Both best options are the same plan.
    SamePlan,
    /// The agent's best option ranks better.
    AgentBetter,
    /// The existing path's best option ranks better.
    LegacyBetter,
    /// They rank equal and are not the same plan (or neither slate has an option).
    EqualRank,
}

impl Comparison {
    fn name(self) -> &'static str {
        match self {
            Self::SamePlan => "the same plan",
            Self::AgentBetter => "the agent's ranks better",
            Self::LegacyBetter => "the existing path's ranks better",
            Self::EqualRank => "equal rank, different plan",
        }
    }
}

/// Why a record is listed without a comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotCompared {
    /// The existing path attached no slate in the same step.
    Unpaired(String),
    /// The agent's slate could not be produced.
    AgentFailed(String),
    /// The kill switch stopped the agent's run.
    AgentAborted(String),
}

/// What a promotion executed, set against both slates. Positions count from 1, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Executed {
    /// The existing slate's option with exactly the executed actions, if it has one.
    pub legacy_option: Option<u32>,
    /// The agent's option with exactly the executed actions, if it has one.
    pub agent_option: Option<u32>,
}

/// The decision on a compared record's point, or portfolio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowDecision {
    /// How the point or portfolio ended.
    pub state: DecisionState,
    /// Whether the promotion was reversed later.
    pub reversed: bool,
    /// Whether the point still carried the paired slate. `false` means the decision was no
    /// choice among the paired options.
    pub on_paired_slate: bool,
    /// Whether the decision was read from the point, or the portfolio, because the record
    /// carries none.
    pub from_point: bool,
    /// The executed plan of a promotion against both slates.
    pub executed: Option<Executed>,
}

/// One compared record: both best options, how they compare, and the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compared {
    /// The score of doing nothing, as the existing slate holds it.
    pub baseline: Score,
    /// The existing slate's best option; `None` when it offers none.
    pub legacy_best: Option<BestOption>,
    /// The agent's best option; `None` when its slate offers none.
    pub agent_best: Option<BestOption>,
    /// How they compare.
    pub comparison: Comparison,
    /// The decision on the point, once there is one.
    pub decision: Option<RowDecision>,
    /// Whether a later point of the subject replaced the paired one before a decision was
    /// recorded.
    pub point_replaced: bool,
}

/// One shadow record in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Whether the record is a point's or a joint one.
    #[serde(default)]
    pub kind: Kind,
    /// The subject the run planned for; for a joint record, the first member.
    pub subject: EntityUri,
    /// The portfolio, its generation and its members, for a joint record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<ShadowGroup>,
    /// The run's id.
    pub run_id: String,
    /// What the run ran with.
    pub version: VersionTuple,
    /// The comparison, or why there is none.
    pub outcome: Result<Compared, NotCompared>,
    /// Which slate the point got, for a run in `agent` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<Shown>,
}

/// What the report found, as counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// The records.
    pub records: u32,
    /// Those with both slates.
    pub compared: u32,
    /// Those without a paired slate.
    pub unpaired: u32,
    /// Those whose agent run failed.
    pub agent_failed: u32,
    /// Those whose agent run was aborted.
    pub agent_aborted: u32,
    /// Those of `agent` mode whose point got the agent's slate.
    #[serde(default)]
    pub agent_shown: u32,
    /// Those of `agent` mode whose point got the existing path's slate instead.
    #[serde(default)]
    pub fallback: u32,
    /// Compared records whose best options are the same plan.
    pub same_plan: u32,
    /// Compared records where the agent's best option ranks better.
    pub agent_better: u32,
    /// Compared records where the existing path's best option ranks better.
    pub legacy_better: u32,
    /// Compared records whose best options rank equal without being the same plan.
    pub equal_rank: u32,
    /// The decisions taken on the paired slate, by state.
    pub decisions: BTreeMap<String, u32>,
    /// The decisions taken on a slate attached after the shadow run, whatever their state.
    pub on_replaced_slate: u32,
    /// Compared records without a decision.
    pub undecided: u32,
    /// Decisions read from the point, among all of the above.
    pub from_point: u32,
    /// Promotions that were reversed, among all of the above.
    pub reversed: u32,
    /// Promotions taken on the paired slate.
    pub promotions: u32,
    /// Of those, how often the executed plan was the existing slate's best option.
    pub executed_legacy_best: u32,
    /// Of those, how often the agent's slate offered the executed plan.
    pub executed_offered_by_agent: u32,
    /// Of those, how often the executed plan was the agent's best option.
    pub executed_agent_best: u32,
    /// The engine versions the records ran with.
    pub engines: BTreeSet<String>,
    /// The models the runs reported.
    pub models: BTreeSet<String>,
    /// The skill versions the records ran with, by skill.
    pub skills: BTreeMap<String, BTreeSet<String>>,
}

/// The paired report over shadow records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowReport {
    /// One row per record, by key and then in the order recorded.
    pub rows: Vec<Row>,
    /// The counts over point records.
    pub totals: Totals,
    /// The counts over joint records, kept apart.
    #[serde(default)]
    pub joint: Totals,
}

fn best(slate: &Slate) -> Option<BestOption> {
    slate.options.first().map(|option| BestOption {
        actions: option.plan.actions.clone(),
        score: option.score.clone(),
    })
}

/// The capability's part of an option's rank key. The slate loop appends `~` and a digest of
/// the plan to tell options apart; that part ranks nothing.
fn capability_key(rank_key: &str) -> &str {
    rank_key.split_once('~').map_or(rank_key, |(key, _)| key)
}

fn compare(legacy: Option<&BestOption>, agent: Option<&BestOption>) -> Comparison {
    match (legacy, agent) {
        (Some(legacy), Some(agent)) if legacy.actions == agent.actions => Comparison::SamePlan,
        (Some(legacy), Some(agent)) => {
            // Lexically smaller sorts first, as the slate loop orders options.
            match capability_key(&agent.score.rank_key).cmp(capability_key(&legacy.score.rank_key))
            {
                std::cmp::Ordering::Less => Comparison::AgentBetter,
                std::cmp::Ordering::Greater => Comparison::LegacyBetter,
                std::cmp::Ordering::Equal => Comparison::EqualRank,
            }
        }
        // A slate without an option is worse than any option.
        (None, Some(_)) => Comparison::AgentBetter,
        (Some(_), None) => Comparison::LegacyBetter,
        (None, None) => Comparison::EqualRank,
    }
}

/// The position, from 1, of the option of `slate` with exactly `actions`.
fn position(slate: &Slate, actions: &[ProposedAction]) -> Option<u32> {
    let index = slate
        .options
        .iter()
        .position(|option| option.plan.actions == actions)?;
    u32::try_from(index + 1).ok()
}

/// What a point's own record says about the point a record is paired with.
enum PointDecision {
    /// It was decided.
    Decided {
        state: DecisionState,
        reversed: bool,
        on_paired_slate: bool,
        actions: Vec<ProposedAction>,
    },
    /// It is still open.
    Open,
    /// A later point of the subject has replaced it.
    Replaced,
}

/// The decision a point's own record shows for the point numbered `point_seq`, paired with
/// the slate attached at `attached_at`.
fn decision_of_point(
    point: &ActionPointRecord,
    point_seq: u32,
    attached_at: &str,
) -> PointDecision {
    if point.points_opened > point_seq {
        return PointDecision::Replaced;
    }
    if point.points_opened < point_seq {
        return PointDecision::Open;
    }
    let (state, reversed) = match point.state() {
        Some(ActionPointState::Promoted) => (DecisionState::Promoted, false),
        Some(ActionPointState::Reversed) => (DecisionState::Promoted, true),
        Some(ActionPointState::Aborted) => (DecisionState::Aborted, false),
        Some(ActionPointState::Expired) => (DecisionState::Expired, false),
        _ => return PointDecision::Open,
    };
    PointDecision::Decided {
        state,
        reversed,
        on_paired_slate: point.slate_attached_at.as_deref() == Some(attached_at),
        actions: point
            .execution_plan
            .as_ref()
            .filter(|_| state == DecisionState::Promoted)
            .map(|plan| plan.actions.clone())
            .unwrap_or_default(),
    }
}

/// The decision a portfolio's own record shows for `generation`, paired with the joint slate
/// attached at `attached_at`: what its audit trail holds for that generation.
fn decision_of_portfolio(
    portfolio: &PortfolioRecord,
    generation: u32,
    attached_at: &str,
) -> PointDecision {
    let happened = |event: PortfolioEvent| {
        portfolio
            .audit
            .iter()
            .any(|entry| entry.generation == generation && entry.event == event)
    };
    let state = if happened(PortfolioEvent::Promoted) {
        DecisionState::Promoted
    } else if happened(PortfolioEvent::Dismissed) {
        DecisionState::Dismissed
    } else if happened(PortfolioEvent::Superseded) {
        DecisionState::Superseded
    } else if portfolio.generation > generation {
        return PointDecision::Replaced;
    } else {
        return PointDecision::Open;
    };
    let promotion = portfolio
        .promotion
        .as_ref()
        .filter(|promotion| state == DecisionState::Promoted && promotion.generation == generation);
    PointDecision::Decided {
        state,
        reversed: happened(PortfolioEvent::Reversed) || happened(PortfolioEvent::PartiallyReversed),
        // A later generation cleared the slate, so its time can no longer be compared; a
        // generation's joint slate is attached once.
        on_paired_slate: portfolio.slate_generation != Some(generation)
            || portfolio.slate_attached_at.as_deref() == Some(attached_at),
        actions: promotion
            .map(|promotion| promotion.execution_plan.actions.clone())
            .unwrap_or_default(),
    }
}

/// The decision of a compared record: the one it carries, else the point's own, or for a joint
/// record the portfolio's own.
fn decision(
    record: &ShadowRecord,
    paired: &Paired,
    agent: &Slate,
    sources: &Sources,
) -> (Option<RowDecision>, bool) {
    let (state, reversed, on_paired_slate, actions, from_point) = match &record.decision {
        Some(decision) => (
            decision.state,
            decision.reversed_at.is_some(),
            decision.on_paired_slate,
            decision.actions.clone(),
            false,
        ),
        None => match match paired {
            Paired::Point(paired) => sources
                .points
                .get(&record.subject)
                .map(|point| decision_of_point(point, paired.point_seq, &paired.attached_at)),
            Paired::Joint(paired) => sources.portfolios.get(&paired.portfolio).map(|portfolio| {
                decision_of_portfolio(portfolio, paired.generation, &paired.attached_at)
            }),
        } {
            Some(PointDecision::Decided {
                state,
                reversed,
                on_paired_slate,
                actions,
            }) => (state, reversed, on_paired_slate, actions, true),
            Some(PointDecision::Replaced) => return (None, true),
            Some(PointDecision::Open) | None => return (None, false),
        },
    };
    let decision = RowDecision {
        state,
        reversed,
        on_paired_slate,
        from_point,
        executed: (state == DecisionState::Promoted).then(|| Executed {
            legacy_option: position(paired.slate(), &actions),
            agent_option: position(agent, &actions),
        }),
    };
    (Some(decision), false)
}

fn row(record: &ShadowRecord, sources: &Sources) -> Row {
    let reason = || record.reason.clone().unwrap_or_default();
    let outcome = match (record.outcome, &record.slate, &record.paired) {
        (ShadowOutcome::Aborted, _, _) => Err(NotCompared::AgentAborted(reason())),
        (ShadowOutcome::Failed, _, _) => Err(NotCompared::AgentFailed(reason())),
        (ShadowOutcome::Completed, None, _) => Err(NotCompared::AgentFailed(
            "the run completed without a slate".to_owned(),
        )),
        (ShadowOutcome::Completed, Some(_), None) => Err(NotCompared::Unpaired(
            record
                .paired_missing
                .clone()
                .unwrap_or_else(|| "recorded before slates were paired".to_owned()),
        )),
        (ShadowOutcome::Completed, Some(agent), Some(paired)) => {
            let (legacy_best, agent_best) = (best(paired.slate()), best(agent));
            let (decision, point_replaced) = decision(record, paired, agent, sources);
            Ok(Compared {
                baseline: paired.slate().baseline.clone(),
                comparison: compare(legacy_best.as_ref(), agent_best.as_ref()),
                legacy_best,
                agent_best,
                decision,
                point_replaced,
            })
        }
    };
    Row {
        kind: if record.group.is_some() {
            Kind::Joint
        } else {
            Kind::Point
        },
        subject: record.subject.clone(),
        group: record.group.clone(),
        run_id: record.run_id.clone(),
        version: record.version.clone(),
        outcome,
        shown: record.shown.clone(),
    }
}

fn state_name(state: DecisionState) -> &'static str {
    match state {
        DecisionState::Promoted => "promoted",
        DecisionState::Aborted => "aborted",
        DecisionState::Expired => "expired",
        DecisionState::Dismissed => "dismissed",
        DecisionState::Superseded => "superseded",
    }
}

/// The counts over the rows of `kind`.
fn totals(rows: &[Row], kind: Kind) -> Totals {
    let mut totals = Totals::default();
    for row in rows.iter().filter(|row| row.kind == kind) {
        totals.records += 1;
        match &row.shown {
            Some(Shown::Agent) => totals.agent_shown += 1,
            Some(Shown::Legacy { .. }) => totals.fallback += 1,
            None => {}
        }
        if !row.version.engine.is_empty() {
            totals.engines.insert(row.version.engine.clone());
        }
        if !row.version.model.is_empty() {
            totals.models.insert(row.version.model.clone());
        }
        for (skill, version) in &row.version.skills {
            totals
                .skills
                .entry(skill.clone())
                .or_default()
                .insert(version.clone());
        }
        let compared = match &row.outcome {
            Ok(compared) => compared,
            Err(NotCompared::Unpaired(_)) => {
                totals.unpaired += 1;
                continue;
            }
            Err(NotCompared::AgentFailed(_)) => {
                totals.agent_failed += 1;
                continue;
            }
            Err(NotCompared::AgentAborted(_)) => {
                totals.agent_aborted += 1;
                continue;
            }
        };
        totals.compared += 1;
        *match compared.comparison {
            Comparison::SamePlan => &mut totals.same_plan,
            Comparison::AgentBetter => &mut totals.agent_better,
            Comparison::LegacyBetter => &mut totals.legacy_better,
            Comparison::EqualRank => &mut totals.equal_rank,
        } += 1;
        let Some(decision) = &compared.decision else {
            totals.undecided += 1;
            continue;
        };
        totals.from_point += u32::from(decision.from_point);
        totals.reversed += u32::from(decision.reversed);
        if !decision.on_paired_slate {
            totals.on_replaced_slate += 1;
            continue;
        }
        *totals
            .decisions
            .entry(state_name(decision.state).to_owned())
            .or_default() += 1;
        if let Some(executed) = decision.executed {
            totals.promotions += 1;
            totals.executed_legacy_best += u32::from(executed.legacy_option == Some(1));
            totals.executed_offered_by_agent += u32::from(executed.agent_option.is_some());
            totals.executed_agent_best += u32::from(executed.agent_option == Some(1));
        }
    }
    totals
}

/// The report over `records`. `sources` holds the point record of each subject, and the
/// record of each portfolio, that has a record without a decision, where it could be read.
#[must_use]
pub fn build(records: &[ShadowRecord], sources: &Sources) -> ShadowReport {
    let rows: Vec<Row> = records.iter().map(|record| row(record, sources)).collect();
    ShadowReport {
        totals: totals(&rows, Kind::Point),
        joint: totals(&rows, Kind::Joint),
        rows,
    }
}

fn plan_text(option: Option<&BestOption>) -> String {
    let Some(option) = option else {
        return "no option".to_owned();
    };
    let actions: Vec<String> = option
        .actions
        .iter()
        .map(|action| {
            let params: Vec<String> = action
                .params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            format!(
                "{} on {} ({})",
                action.action_type.0,
                action.target.0,
                params.join(", ")
            )
        })
        .collect();
    format!("rank {}: {}", option.score.rank_key, actions.join("; "))
}

fn nth(option: Option<u32>, of: &str) -> String {
    match option {
        Some(1) => format!("{of} best option"),
        Some(n) => format!("{of} option {n}"),
        None => format!("none of {of} options"),
    }
}

fn decision_text(compared: &Compared, kind: Kind) -> String {
    let (what, later) = match kind {
        Kind::Point => ("point", "the point was replaced by a later one"),
        Kind::Joint => ("portfolio", "the portfolio formed again"),
    };
    let Some(decision) = &compared.decision else {
        return if compared.point_replaced {
            format!("undecided ({later})")
        } else {
            "undecided".to_owned()
        };
    };
    let mut text = state_name(decision.state).to_owned();
    if decision.reversed {
        text.push_str(", later reversed");
    }
    if let Some(executed) = decision.executed {
        let _ = write!(
            text,
            "; executed {} and {}",
            nth(executed.legacy_option, "the existing slate's"),
            nth(executed.agent_option, "the agent's")
        );
    }
    if !decision.on_paired_slate {
        text.push_str(" (on a slate attached after the shadow run)");
    }
    if decision.from_point {
        let _ = write!(text, " (read from the {what})");
    }
    text
}

fn row_text(row: &Row) -> String {
    let skills: Vec<String> = row
        .version
        .skills
        .iter()
        .map(|(name, version)| format!("{name}@{version}"))
        .collect();
    let what = match &row.group {
        Some(group) => format!(
            "joint: {} generation {} ({})",
            group.portfolio,
            group.generation,
            group
                .members
                .iter()
                .map(|member| member.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => row.subject.0.clone(),
    };
    let mut out = format!(
        "{what}  run {}  (engine {}, model {}, skills {})\n",
        row.run_id,
        row.version.engine,
        if row.version.model.is_empty() {
            "none reported"
        } else {
            &row.version.model
        },
        if skills.is_empty() {
            "none".to_owned()
        } else {
            skills.join(" ")
        }
    );
    let _ = match &row.outcome {
        Ok(compared) => writeln!(
            out,
            "  existing path: {}\n  agent:         {}\n  doing nothing: rank {}\n  best \
             options:  {}\n  decision:      {}",
            plan_text(compared.legacy_best.as_ref()),
            plan_text(compared.agent_best.as_ref()),
            compared.baseline.rank_key,
            compared.comparison.name(),
            decision_text(compared, row.kind)
        ),
        Err(NotCompared::Unpaired(why)) => {
            writeln!(out, "  not compared: no paired slate: {why}")
        }
        Err(NotCompared::AgentFailed(why)) => {
            writeln!(out, "  not compared: the agent's run failed: {why}")
        }
        Err(NotCompared::AgentAborted(why)) => {
            writeln!(out, "  not compared: the agent's run was aborted: {why}")
        }
    };
    let _ = match &row.shown {
        Some(Shown::Agent) => writeln!(out, "  shown:         the agent's slate"),
        Some(Shown::Legacy { reason }) => {
            writeln!(out, "  shown:         the existing path's slate: {reason}")
        }
        // A shadow run: the point got the existing path's slate, as always.
        None => Ok(()),
    };
    out
}

fn list(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "none".to_owned()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

impl Totals {
    /// The counts as text. Counts only, never rates: a window of a few records supports
    /// none.
    fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} record(s): {} compared, {} unpaired, {} with a failed and {} with an aborted \
             agent run",
            self.records, self.compared, self.unpaired, self.agent_failed, self.agent_aborted
        );
        if self.agent_shown + self.fallback > 0 {
            let _ = writeln!(
                out,
                "in agent mode: {} point(s) got the agent's slate, {} the existing path's instead",
                self.agent_shown, self.fallback
            );
        }
        let _ = writeln!(
            out,
            "best options of the {} compared: {} the same plan, {} the agent's better, {} the \
             existing path's better, {} equal rank",
            self.compared, self.same_plan, self.agent_better, self.legacy_better, self.equal_rank
        );
        let by_state: Vec<String> = self
            .decisions
            .iter()
            .map(|(state, count)| format!("{count} {state}"))
            .collect();
        let _ = writeln!(
            out,
            "decisions on the paired slate: {}; {} on a slate attached later; {} undecided\nof \
             all decisions: {} read from the point, {} later reversed",
            if by_state.is_empty() {
                "none".to_owned()
            } else {
                by_state.join(", ")
            },
            self.on_replaced_slate,
            self.undecided,
            self.from_point,
            self.reversed
        );
        let _ = writeln!(
            out,
            "of {} promotion(s) on the paired slate: {} executed the existing slate's best \
             option, {} a plan the agent offered, {} the agent's best option",
            self.promotions,
            self.executed_legacy_best,
            self.executed_offered_by_agent,
            self.executed_agent_best
        );
        let _ = writeln!(
            out,
            "engines: {}; models: {}",
            list(&self.engines),
            list(&self.models)
        );
        for (skill, versions) in &self.skills {
            let _ = writeln!(out, "skill {skill}: {}", list(versions));
        }
        out
    }
}

impl ShadowReport {
    /// The report as text: one block per record, then the counts, point records and joint
    /// records apart.
    #[must_use]
    pub fn table(&self) -> String {
        let rows: String = self.rows.iter().map(row_text).collect();
        if self.joint.records == 0 {
            return format!("{rows}\n{}", self.totals.text());
        }
        format!(
            "{rows}\npoint records\n{}\njoint records\n{}",
            self.totals.text(),
            self.joint.text()
        )
    }
}

/// The keys that have shadow records, from the admin introspection at `admin`: a subject's
/// IRI, or a portfolio's id for joint records. What a key holds is told by its records, not by
/// its shape.
///
/// # Errors
/// A message naming the URL when the endpoint cannot be reached or answers something else.
pub async fn subjects(http: &reqwest::Client, admin: &str) -> Result<Vec<EntityUri>, String> {
    let url = format!("{}/query", admin.trim_end_matches('/'));
    let query = format!(
        "SELECT service_key FROM state WHERE service_name = '{AGENT_SHADOW_OBJECT}' \
         ORDER BY service_key"
    );
    let failed = |why: String| format!("the Restate admin endpoint at {url}: {why}");
    let response = http
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(serde_json::json!({ "query": query }).to_string())
        .send()
        .await
        .map_err(|why| failed(why.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|why| failed(why.to_string()))?;
    if !status.is_success() {
        return Err(failed(format!("answered {status}: {text}")));
    }
    let rows = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|body| body.get("rows")?.as_array().cloned())
        .ok_or_else(|| failed(format!("answered without rows: {text}")))?;
    let keys: BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.get("service_key")?.as_str().map(str::to_owned))
        .collect();
    Ok(keys.into_iter().map(EntityUri).collect())
}

/// The shadow records under `keys` and, for each record that has a paired slate and no
/// decision, where its decision can be read: its subject's point record, or for a joint record
/// its portfolio's record. All are read over the ingress at `ingress`.
///
/// # Errors
/// A message naming the key whose records, point or portfolio could not be read.
pub async fn read(
    http: &reqwest::Client,
    ingress: &str,
    keys: &[EntityUri],
) -> Result<(Vec<ShadowRecord>, Sources), String> {
    let shadow = ShadowClient::new(http.clone(), ingress);
    let (mut records, mut sources) = (Vec::new(), Sources::default());
    for key in keys {
        let under_key = shadow
            .records_under(&key.0)
            .await
            .map_err(|why| format!("the shadow records of {}: {why}", key.0))?;
        for record in &under_key {
            if record.paired.is_none() || record.decision.is_some() {
                continue;
            }
            match &record.group {
                Some(group) if !sources.portfolios.contains_key(&group.portfolio) => {
                    let portfolio: Option<PortfolioRecord> = call_object::<(), _>(
                        http,
                        ingress,
                        [PORTFOLIO_OBJECT, &group.portfolio, "record"],
                        None,
                    )
                    .await
                    .map_err(|why| format!("the portfolio {}: {why}", group.portfolio))?;
                    if let Some(portfolio) = portfolio {
                        sources
                            .portfolios
                            .insert(group.portfolio.clone(), portfolio);
                    }
                }
                None if !sources.points.contains_key(&record.subject) => {
                    let point: Option<ActionPointRecord> = call_object::<(), _>(
                        http,
                        ingress,
                        [ACTION_POINT_OBJECT, &record.subject.0, "record"],
                        None,
                    )
                    .await
                    .map_err(|why| format!("the action point of {}: {why}", record.subject.0))?;
                    if let Some(point) = point {
                        sources.points.insert(record.subject.clone(), point);
                    }
                }
                _ => {}
            }
        }
        records.extend(under_key);
    }
    Ok((records, sources))
}

/// The report over every subject's shadow records.
///
/// # Errors
/// As [`subjects`] and [`read`]: no partial report is built.
pub async fn collect(
    http: &reqwest::Client,
    admin: &str,
    ingress: &str,
) -> Result<ShadowReport, String> {
    let keys = subjects(http, admin).await?;
    let (records, sources) = read(http, ingress, &keys).await?;
    Ok(build(&records, &sources))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod tests {
    use ekg_domain::{
        ActionPoint, ActionPointId, ActionType, ExecutionPlan, Plan, RankedPlan, Trace,
    };

    use super::*;
    use crate::shadow::{Decision, PairedJointSlate, PairedSlate};

    const SUBJECT: &str = "x:order/1";
    const ATTACHED: &str = "100";

    fn action(to: &str) -> ProposedAction {
        ProposedAction {
            action_type: ActionType("re_promise_date".to_owned()),
            target: EntityUri(SUBJECT.to_owned()),
            params: BTreeMap::from([("new_date".to_owned(), to.to_owned())]),
        }
    }

    fn score(rank_key: &str) -> Score {
        Score {
            metrics: BTreeMap::new(),
            rank_key: rank_key.to_owned(),
        }
    }

    /// A slate of `options`, each a rank key and the date its one action moves to.
    fn slate(options: &[(&str, &str)]) -> Slate {
        Slate {
            baseline: score("9"),
            options: options
                .iter()
                .map(|(rank_key, to)| RankedPlan {
                    plan: Plan {
                        actions: vec![action(to)],
                    },
                    score: score(rank_key),
                    recommended: false,
                    needs: Vec::new(),
                    omissions: Vec::new(),
                    trace: Trace::from_steps(Vec::new()),
                })
                .collect(),
            trace: Trace::from_steps(Vec::new()),
        }
    }

    pub(crate) fn record(legacy: &[(&str, &str)], agent: &[(&str, &str)]) -> ShadowRecord {
        ShadowRecord {
            run_id: "run-1".to_owned(),
            subject: EntityUri(SUBJECT.to_owned()),
            group: None,
            started_at: 10,
            ended_at: 20,
            outcome: ShadowOutcome::Completed,
            reason: None,
            slate: Some(slate(agent)),
            runs: Vec::new(),
            version: VersionTuple {
                engine: "0.1.0".to_owned(),
                model: "m/1".to_owned(),
                skills: BTreeMap::from([("propose-plan".to_owned(), "v1".to_owned())]),
                tools: Vec::new(),
            },
            paired: Some(Paired::Point(PairedSlate {
                action_point: ActionPointId("ap/1".to_owned()),
                point_seq: 1,
                slate: slate(legacy),
                attached_at: ATTACHED.to_owned(),
            })),
            paired_missing: None,
            decision: None,
            shown: None,
            epoch: None,
            rollout: None,
        }
    }

    pub(crate) fn decided(
        mut record: ShadowRecord,
        state: DecisionState,
        executed: &[&str],
    ) -> ShadowRecord {
        record.decision = Some(Decision {
            state,
            decided_at: 30,
            reviewer: Some("planner-1".to_owned()),
            actions: executed.iter().map(|to| action(to)).collect(),
            option_rank_key: None,
            via_portfolio: None,
            on_paired_slate: true,
            reversed_at: None,
        });
        record
    }

    pub(crate) const PORTFOLIO: &str = "portfolio/0123456789ab";

    /// `record` as a joint record of the portfolio at generation 2: the same two slates,
    /// paired on the portfolio.
    pub(crate) fn joint(mut record: ShadowRecord) -> ShadowRecord {
        let legacy = record.paired.take().map(|paired| paired.slate().clone());
        record.group = Some(ShadowGroup {
            portfolio: PORTFOLIO.to_owned(),
            generation: 2,
            members: vec![
                EntityUri(SUBJECT.to_owned()),
                EntityUri("x:mo/7".to_owned()),
            ],
        });
        record.paired = legacy.map(|slate| {
            Paired::Joint(PairedJointSlate {
                portfolio: PORTFOLIO.to_owned(),
                generation: 2,
                slate,
                attached_at: ATTACHED.to_owned(),
            })
        });
        record
    }

    /// The portfolio's own record at `generation`, with what happened to generation 2.
    fn portfolio(generation: u32, events: &[ekg_dynamic::PortfolioEvent]) -> PortfolioRecord {
        use ekg_dynamic::{
            PortfolioAuditEntry, PortfolioPromotion, PortfolioState, ReverseProgress,
        };
        let promoted = events.contains(&PortfolioEvent::Promoted);
        PortfolioRecord {
            id: Some(ekg_domain::PortfolioId(PORTFOLIO.to_owned())),
            members: Vec::new(),
            state: PortfolioState::Active,
            generation,
            audit: events
                .iter()
                .map(|event| PortfolioAuditEntry {
                    event: *event,
                    generation: 2,
                    reason: "r".to_owned(),
                    members: Vec::new(),
                    reviewer: None,
                    links: Vec::new(),
                })
                .collect(),
            slate: None,
            slate_generation: Some(2),
            promotion: promoted.then(|| PortfolioPromotion {
                generation: 2,
                rank_key: "2~b".to_owned(),
                reviewer: ekg_domain::ReviewerId("planner-1".to_owned()),
                rationale: "r".to_owned(),
                execution_plan: ExecutionPlan {
                    actions: vec![action("late")],
                    receipts: Vec::new(),
                },
                promoted_at: "30".to_owned(),
                progress: ReverseProgress::default(),
            }),
            slate_attached_at: Some(ATTACHED.to_owned()),
        }
    }

    #[test]
    fn a_joint_record_is_compared_like_a_points_and_names_its_portfolio_and_members() {
        let record = joint(decided(
            record(&[("1~a", "early"), ("2~b", "late")], &[("3~c", "late")]),
            DecisionState::Promoted,
            &["late"],
        ));
        let report = build(std::slice::from_ref(&record), &Sources::default());
        let row = &report.rows[0];
        assert_eq!(row.kind, Kind::Joint);
        let group = row.group.as_ref().expect("its portfolio");
        assert_eq!((group.portfolio.as_str(), group.generation), (PORTFOLIO, 2));
        assert_eq!(group.members.len(), 2);
        let compared = row.outcome.as_ref().expect("compared");
        assert_eq!(compared.comparison, Comparison::LegacyBetter);
        assert_eq!(
            compared.baseline.rank_key, "9",
            "the score of doing nothing"
        );
        // The decision beside both joint slates.
        let decision = compared.decision.as_ref().expect("a decision");
        assert_eq!(decision.state, DecisionState::Promoted);
        assert_eq!(
            decision.executed,
            Some(Executed {
                legacy_option: Some(2),
                agent_option: Some(1)
            })
        );
        let json = serde_json::to_value(row).expect("json");
        assert_eq!(json["kind"], "joint");
        let table = report.table();
        assert!(
            table.contains(&format!(
                "joint: {PORTFOLIO} generation 2 ({SUBJECT}, x:mo/7)"
            )),
            "{table}"
        );
    }

    #[test]
    fn a_joint_record_without_a_paired_slate_or_with_a_failed_run_is_listed_with_its_reason() {
        let mut unpaired = joint(record(&[], &[("1~c", "late")]));
        unpaired.paired = None;
        unpaired.paired_missing = Some("the joint slate arrived stale".to_owned());
        let mut failed = joint(record(&[("1~a", "early")], &[]));
        failed.outcome = ShadowOutcome::Failed;
        failed.reason = Some("model down".to_owned());
        failed.slate = None;
        let report = build(&[unpaired, failed], &Sources::default());
        assert_eq!(
            report.rows[0].outcome,
            Err(NotCompared::Unpaired(
                "the joint slate arrived stale".to_owned()
            ))
        );
        assert_eq!(
            report.rows[1].outcome,
            Err(NotCompared::AgentFailed("model down".to_owned()))
        );
        assert_eq!(
            (report.joint.records, report.joint.compared),
            (2, 0),
            "listed, not compared"
        );
        assert_eq!((report.joint.unpaired, report.joint.agent_failed), (1, 1));
    }

    #[test]
    fn point_records_and_joint_records_are_totalled_apart() {
        let point = || record(&[("1~a", "early")], &[("1~b", "early")]);
        let mut records = vec![point(), point(), point()];
        records.push(joint(point()));
        records.push(joint(decided(
            record(&[("1~a", "early")], &[("2~c", "late")]),
            DecisionState::Dismissed,
            &[],
        )));
        let report = build(&records, &Sources::default());
        assert_eq!((report.totals.records, report.totals.compared), (3, 3));
        assert_eq!((report.joint.records, report.joint.compared), (2, 2));
        assert_eq!(report.totals.same_plan, 3);
        assert_eq!((report.joint.same_plan, report.joint.legacy_better), (1, 1));
        assert_eq!(report.joint.decisions["dismissed"], 1);
        assert!(report.totals.decisions.is_empty());
        let table = report.table();
        assert!(
            table.contains("point records\n3 record(s): 3 compared")
                && table.contains("joint records\n2 record(s): 2 compared"),
            "{table}"
        );
        // Without joint records the table is the one it always was.
        let before = build(&records[..3], &Sources::default());
        assert!(!before.table().contains("joint records"));
        assert_eq!(before.totals, report.totals);
    }

    #[test]
    fn an_undecided_joint_record_reads_its_decision_from_the_portfolio() {
        use ekg_dynamic::PortfolioEvent as E;
        let undecided = joint(record(
            &[("1~a", "early"), ("2~b", "late")],
            &[("3~c", "late")],
        ));
        let read = |portfolio: PortfolioRecord| {
            let sources = Sources {
                points: BTreeMap::new(),
                portfolios: BTreeMap::from([(PORTFOLIO.to_owned(), portfolio)]),
            };
            let mut report = build(std::slice::from_ref(&undecided), &sources);
            (
                report.rows.remove(0).outcome.expect("compared"),
                report.joint,
            )
        };

        let (compared, totals) = read(portfolio(2, &[E::Promoted, E::PartiallyReversed]));
        let decision = compared.decision.expect("read from the portfolio");
        assert_eq!(decision.state, DecisionState::Promoted);
        assert!(decision.reversed && decision.from_point && decision.on_paired_slate);
        assert_eq!(
            decision.executed.map(|executed| executed.legacy_option),
            Some(Some(2))
        );
        assert_eq!(
            (totals.from_point, totals.reversed, totals.promotions),
            (1, 1, 1)
        );

        for (event, state) in [
            (E::Dismissed, DecisionState::Dismissed),
            (E::Superseded, DecisionState::Superseded),
        ] {
            let (compared, _) = read(portfolio(2, &[event]));
            assert_eq!(
                compared.decision.map(|decision| decision.state),
                Some(state)
            );
        }
        // Still open at its generation, and formed again without a decision on it.
        let (open, totals) = read(portfolio(2, &[E::SlateAttached]));
        assert_eq!(open.decision, None);
        assert!(!open.point_replaced);
        assert_eq!(totals.undecided, 1);
        let (replaced, _) = read(portfolio(3, &[E::SlateAttached]));
        assert_eq!(replaced.decision, None);
        assert!(replaced.point_replaced);

        // No portfolio could be read: undecided.
        let report = build(std::slice::from_ref(&undecided), &Sources::default());
        let compared = report.rows[0].outcome.as_ref().expect("compared");
        assert_eq!(compared.decision, None);
        assert!(report.table().contains("decision:      undecided"));
    }

    fn one(record: &ShadowRecord) -> Compared {
        only(record, None).expect("compared")
    }

    fn only(
        record: &ShadowRecord,
        point: Option<ActionPointRecord>,
    ) -> Result<Compared, NotCompared> {
        let points: BTreeMap<EntityUri, ActionPointRecord> = point
            .map(|point| (record.subject.clone(), point))
            .into_iter()
            .collect();
        let mut report = build(std::slice::from_ref(record), &points.into());
        report.rows.remove(0).outcome
    }

    #[test]
    fn the_same_best_plan_is_the_same_plan_whatever_the_keys() {
        let compared = one(&record(&[("2~a", "early")], &[("1~b", "early")]));
        assert_eq!(compared.comparison, Comparison::SamePlan);
        assert_eq!(compared.baseline.rank_key, "9");
        let legacy = compared.legacy_best.expect("legacy best");
        assert_eq!(legacy.actions, [action("early")]);
        assert_eq!(legacy.score.rank_key, "2~a");
        assert_eq!(
            compared.agent_best.expect("agent best").score.rank_key,
            "1~b"
        );
    }

    #[test]
    fn the_better_capability_key_ranks_better() {
        let agent = one(&record(&[("2~a", "late")], &[("1~z", "early")]));
        assert_eq!(agent.comparison, Comparison::AgentBetter);
        let legacy = one(&record(&[("1~z", "late")], &[("2~a", "early")]));
        assert_eq!(legacy.comparison, Comparison::LegacyBetter);
    }

    #[test]
    fn equal_capability_keys_with_different_plans_rank_equal() {
        // The digests after `~` differ and would order the two; they rank nothing.
        let compared = one(&record(&[("1~zzz", "late")], &[("1~aaa", "early")]));
        assert_eq!(compared.comparison, Comparison::EqualRank);
    }

    #[test]
    fn a_slate_without_options_is_worse_than_any_option_and_equal_to_another() {
        let empty_legacy = one(&record(&[], &[("5~a", "early")]));
        assert_eq!(empty_legacy.comparison, Comparison::AgentBetter);
        assert_eq!(empty_legacy.legacy_best, None);
        let empty_agent = one(&record(&[("5~a", "early")], &[]));
        assert_eq!(empty_agent.comparison, Comparison::LegacyBetter);
        assert_eq!(
            one(&record(&[], &[])).comparison,
            Comparison::EqualRank,
            "neither offers anything"
        );
        let table = build(&[record(&[], &[("5~a", "early")])], &Sources::default()).table();
        assert!(table.contains("existing path: no option"), "{table}");
    }

    #[test]
    fn an_unpaired_record_is_listed_with_its_reason_and_not_compared() {
        let mut unpaired = record(&[], &[("1~a", "early")]);
        unpaired.paired = None;
        unpaired.paired_missing = Some("the slate loop timed out".to_owned());
        assert_eq!(
            only(&unpaired, None),
            Err(NotCompared::Unpaired("the slate loop timed out".to_owned()))
        );
        unpaired.paired_missing = None;
        assert!(matches!(
            only(&unpaired, None),
            Err(NotCompared::Unpaired(why)) if why.contains("before slates were paired")
        ));
    }

    #[test]
    fn a_failed_or_aborted_agent_run_is_listed_with_its_reason_and_not_compared() {
        let mut failed = record(&[("1~a", "early")], &[]);
        failed.outcome = ShadowOutcome::Failed;
        failed.slate = None;
        failed.reason = Some("the runtime answered 502".to_owned());
        assert_eq!(
            only(&failed, None),
            Err(NotCompared::AgentFailed(
                "the runtime answered 502".to_owned()
            ))
        );
        let mut aborted = failed.clone();
        aborted.outcome = ShadowOutcome::Aborted;
        aborted.reason = Some("the mode was set to legacy".to_owned());
        assert_eq!(
            only(&aborted, None),
            Err(NotCompared::AgentAborted(
                "the mode was set to legacy".to_owned()
            ))
        );
    }

    #[test]
    fn a_promotion_is_set_against_both_slates() {
        let legacy = [("1~a", "early"), ("2~b", "late")];
        let agent = [("1~c", "late"), ("3~d", "later")];
        let executed = |to: &[&str]| -> Executed {
            one(&decided(
                record(&legacy, &agent),
                DecisionState::Promoted,
                to,
            ))
            .decision
            .expect("a decision")
            .executed
            .expect("a promotion")
        };
        // The existing slate's best option, which the agent did not offer.
        assert_eq!(
            executed(&["early"]),
            Executed {
                legacy_option: Some(1),
                agent_option: None
            }
        );
        // Its second option, which is the agent's best.
        assert_eq!(
            executed(&["late"]),
            Executed {
                legacy_option: Some(2),
                agent_option: Some(1)
            }
        );
        // A plan of neither slate's first place, offered by the agent second.
        assert_eq!(
            executed(&["later"]),
            Executed {
                legacy_option: None,
                agent_option: Some(2)
            }
        );
        let table = build(
            &[decided(
                record(&legacy, &agent),
                DecisionState::Promoted,
                &["late"],
            )],
            &Sources::default(),
        )
        .table();
        assert!(
            table.contains(
                "promoted; executed the existing slate's option 2 and the agent's best option"
            ),
            "{table}"
        );
    }

    #[test]
    fn an_abort_or_expiry_executes_nothing() {
        for state in [DecisionState::Aborted, DecisionState::Expired] {
            let decision = one(&decided(record(&[("1~a", "early")], &[]), state, &[]))
                .decision
                .expect("a decision");
            assert_eq!((decision.state, decision.executed), (state, None));
            assert!(!decision.from_point && !decision.reversed);
        }
    }

    #[test]
    fn a_decision_on_a_replaced_slate_and_a_reversal_are_shown() {
        let mut replaced = decided(
            record(&[("1~a", "early")], &[("1~b", "early")]),
            DecisionState::Promoted,
            &["early"],
        );
        let decision = replaced.decision.as_mut().expect("a decision");
        decision.on_paired_slate = false;
        decision.reversed_at = Some(40);
        let shown = one(&replaced).decision.expect("a decision");
        assert!(!shown.on_paired_slate);
        assert!(shown.reversed);
        let report = build(&[replaced], &Sources::default());
        assert_eq!(report.totals.on_replaced_slate, 1);
        assert_eq!(report.totals.reversed, 1);
        assert!(report.totals.decisions.is_empty(), "counted apart");
        assert_eq!(
            report.totals.promotions, 0,
            "no choice among the paired options"
        );
        let table = report.table();
        assert!(table.contains("later reversed"), "{table}");
        assert!(
            table.contains("on a slate attached after the shadow run"),
            "{table}"
        );
    }

    /// The subject's point record: the `opened`-th point, in `state`, having executed `to`.
    fn point(opened: u32, state: ActionPointState, to: Option<&str>) -> ActionPointRecord {
        ActionPointRecord {
            point: Some(ActionPoint {
                id: ActionPointId("ap/1".to_owned()),
                subject: EntityUri(SUBJECT.to_owned()),
                reason: "late".to_owned(),
                state,
                proposal: None,
            }),
            execution_plan: to.map(|to| ExecutionPlan {
                actions: vec![action(to)],
                receipts: Vec::new(),
            }),
            promoted_at: to.map(|_| "30".to_owned()),
            slate_attached_at: Some(ATTACHED.to_owned()),
            points_opened: opened,
            ..ActionPointRecord::default()
        }
    }

    #[test]
    fn an_undecided_record_shows_the_decision_of_its_own_point() {
        let undecided = record(&[("1~a", "early"), ("2~b", "late")], &[("1~c", "late")]);
        let promoted = only(
            &undecided,
            Some(point(1, ActionPointState::Promoted, Some("late"))),
        )
        .expect("compared");
        let decision = promoted.decision.expect("read from the point");
        assert!(decision.from_point && decision.on_paired_slate && !decision.reversed);
        assert_eq!(decision.state, DecisionState::Promoted);
        assert_eq!(
            decision.executed,
            Some(Executed {
                legacy_option: Some(2),
                agent_option: Some(1)
            })
        );
        assert!(!promoted.point_replaced);

        let reversed = only(
            &undecided,
            Some(point(1, ActionPointState::Reversed, Some("late"))),
        )
        .expect("compared")
        .decision
        .expect("read from the point");
        assert!(reversed.reversed && reversed.state == DecisionState::Promoted);

        let aborted = only(&undecided, Some(point(1, ActionPointState::Aborted, None)))
            .expect("compared")
            .decision
            .expect("read from the point");
        assert_eq!(
            (aborted.state, aborted.executed),
            (DecisionState::Aborted, None)
        );
    }

    #[test]
    fn an_undecided_record_of_a_point_still_open_stays_undecided() {
        let undecided = record(&[("1~a", "early")], &[("1~c", "late")]);
        for state in [ActionPointState::Proposed, ActionPointState::UnderReview] {
            let compared = only(&undecided, Some(point(1, state, None))).expect("compared");
            assert_eq!(compared.decision, None);
            assert!(!compared.point_replaced);
        }
        assert_eq!(one(&undecided).decision, None, "no point could be read");
    }

    #[test]
    fn an_undecided_record_of_a_replaced_point_says_so_and_takes_no_decision() {
        let undecided = record(&[("1~a", "early")], &[("1~c", "late")]);
        // The subject's second point was promoted; the record is of the first.
        let later = point(2, ActionPointState::Promoted, Some("early"));
        let compared = only(&undecided, Some(later.clone())).expect("compared");
        assert_eq!(compared.decision, None);
        assert!(compared.point_replaced);
        let points = BTreeMap::from([(undecided.subject.clone(), later)]);
        let report = build(&[undecided], &points.into());
        assert_eq!(report.totals.undecided, 1);
        assert!(
            report.table().contains("the point was replaced"),
            "{}",
            report.table()
        );
    }

    #[test]
    fn a_recorded_decision_is_not_read_from_the_point_again() {
        let aborted = decided(
            record(&[("1~a", "early")], &[("1~c", "late")]),
            DecisionState::Aborted,
            &[],
        );
        let decision = only(
            &aborted,
            Some(point(2, ActionPointState::Promoted, Some("early"))),
        )
        .expect("compared")
        .decision
        .expect("the recorded decision");
        assert_eq!(decision.state, DecisionState::Aborted);
        assert!(!decision.from_point);
    }

    /// Ten records: seven compared (five decided, two not), two unpaired, one failed.
    fn window() -> Vec<ShadowRecord> {
        let pair = [("1~a", "early"), ("2~b", "late")];
        let promoted = DecisionState::Promoted;
        let mut records = vec![
            // Same plan, promoted with the best option of both.
            decided(record(&pair, &[("1~x", "early")]), promoted, &["early"]),
            // The agent's better, promoted with the existing second option, the agent's best.
            decided(record(&pair, &[("0~x", "late")]), promoted, &["late"]),
            // The existing path's better, aborted.
            decided(
                record(&pair, &[("5~x", "later")]),
                DecisionState::Aborted,
                &[],
            ),
            // Equal rank, different plan, expired.
            decided(
                record(&pair, &[("1~x", "later")]),
                DecisionState::Expired,
                &[],
            ),
            // Promoted with a plan neither offered.
            decided(record(&pair, &[("0~x", "late")]), promoted, &["never"]),
            // Undecided.
            record(&pair, &[("0~x", "late")]),
            record(&pair, &[("0~x", "late")]),
        ];
        records[6].version.skills = BTreeMap::from([("propose-plan".to_owned(), "v2".to_owned())]);
        records[6].version.model = "m/2".to_owned();
        let mut unpaired = record(&pair, &[("1~x", "early")]);
        unpaired.paired = None;
        records.extend([unpaired.clone(), unpaired]);
        let mut failed = record(&pair, &[]);
        failed.outcome = ShadowOutcome::Failed;
        records.push(failed);
        records
    }

    #[test]
    fn the_totals_add_up() {
        let report = build(&window(), &Sources::default());
        let t = &report.totals;
        assert_eq!(
            (
                t.records,
                t.compared,
                t.unpaired,
                t.agent_failed,
                t.agent_aborted
            ),
            (10, 7, 2, 1, 0)
        );
        assert_eq!(
            (t.same_plan, t.agent_better, t.legacy_better, t.equal_rank),
            (1, 4, 1, 1)
        );
        assert_eq!(
            t.same_plan + t.agent_better + t.legacy_better + t.equal_rank,
            t.compared
        );
        assert_eq!(
            t.decisions,
            BTreeMap::from([
                ("promoted".to_owned(), 3),
                ("aborted".to_owned(), 1),
                ("expired".to_owned(), 1)
            ])
        );
        assert_eq!(
            t.decisions.values().sum::<u32>() + t.on_replaced_slate + t.undecided,
            t.compared
        );
        assert_eq!(
            (
                t.promotions,
                t.executed_legacy_best,
                t.executed_offered_by_agent,
                t.executed_agent_best
            ),
            (3, 1, 2, 2)
        );
    }

    #[test]
    fn the_report_names_every_version_and_prints_counts_only() {
        let report = build(&window(), &Sources::default());
        let t = &report.totals;
        assert_eq!(t.engines, BTreeSet::from(["0.1.0".to_owned()]));
        assert_eq!(
            t.models,
            BTreeSet::from(["m/1".to_owned(), "m/2".to_owned()])
        );
        assert_eq!(
            t.skills["propose-plan"],
            BTreeSet::from(["v1".to_owned(), "v2".to_owned()])
        );
        assert_eq!(report.rows[6].version.skills["propose-plan"], "v2");

        let table = report.table();
        assert!(
            table.contains("10 record(s): 7 compared, 2 unpaired, 1 with a failed"),
            "{table}"
        );
        assert!(table.contains("skill propose-plan: v1, v2"), "{table}");
        assert!(table.contains("skills propose-plan@v2"), "{table}");
        assert!(!table.contains('%'), "counts, never rates: {table}");
        let json = serde_json::to_string(&report).expect("json");
        assert_eq!(
            serde_json::from_str::<ShadowReport>(&json).expect("report"),
            report
        );
    }

    #[tokio::test]
    async fn an_unreachable_admin_endpoint_fails_with_its_url() {
        let http = reqwest::Client::new();
        let why = collect(&http, "http://127.0.0.1:1", "http://127.0.0.1:1")
            .await
            .expect_err("nothing listens there");
        assert!(why.contains("http://127.0.0.1:1/query"), "{why}");
    }

    #[test]
    fn the_report_says_which_slate_each_agent_mode_point_got_and_counts_both() {
        let shadow = record(&[("1~a", "early")], &[("1~x", "early")]);
        let mut shown = shadow.clone();
        shown.run_id = "run-2".to_owned();
        shown.shown = Some(Shown::Agent);
        let mut fallback = shadow.clone();
        fallback.run_id = "run-3".to_owned();
        fallback.outcome = ShadowOutcome::Failed;
        fallback.slate = None;
        fallback.reason = Some("model down".to_owned());
        fallback.shown = Some(Shown::Legacy {
            reason: "the agent's run failed: model down".to_owned(),
        });
        let report = build(&[shadow, shown, fallback], &Sources::default());
        let rows: Vec<Option<&Shown>> = report.rows.iter().map(|row| row.shown.as_ref()).collect();
        assert_eq!(rows[0], None, "a shadow run");
        assert_eq!(rows[1], Some(&Shown::Agent));
        assert!(matches!(rows[2], Some(Shown::Legacy { .. })));
        assert_eq!((report.totals.agent_shown, report.totals.fallback), (1, 1));
        assert_eq!(report.totals.records, 3);

        let table = report.table();
        assert_eq!(table.matches("  shown: ").count(), 2, "{table}");
        assert!(
            table.contains("  shown:         the agent's slate\n"),
            "{table}"
        );
        assert!(
            table.contains(
                "  shown:         the existing path's slate: the agent's run failed: model down"
            ),
            "{table}"
        );
        assert!(
            table.contains(
                "in agent mode: 1 point(s) got the agent's slate, 1 the existing path's instead"
            ),
            "{table}"
        );
        let json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["rows"][1]["shown"]["slate"], "agent");
        assert!(json["rows"][0].get("shown").is_none());
        assert_eq!(json["totals"]["agent_shown"], 1);
    }

    #[test]
    fn a_report_of_shadow_runs_alone_reads_as_before() {
        let report = build(
            &[record(&[("1~a", "early")], &[("1~x", "early")])],
            &Sources::default(),
        );
        let table = report.table();
        assert!(
            !table.contains("shown:") && !table.contains("in agent mode"),
            "{table}"
        );
        // A report written before the counts existed still reads.
        let mut json = serde_json::to_value(&report).expect("json");
        let totals = json["totals"].as_object_mut().expect("totals");
        totals.remove("agent_shown");
        totals.remove("fallback");
        let back: ShadowReport = serde_json::from_value(json).expect("an older report");
        assert_eq!(back, report);
    }
}
