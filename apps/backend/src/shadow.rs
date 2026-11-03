//! Shadow records: what each shadow run of the agent produced, kept apart from everything the
//! operator sees.
//!
//! A shadow record holds the agent's slate (options, scores and trace), one [`RunRecord`] per
//! candidate attempt, and the version tuple it ran with, keyed by subject. So that it can be
//! compared on its own, it also holds the existing path's slate of the same step and, once the
//! point is promoted, aborted or expired, that decision. The decision arrives later, from the
//! action-point object, and only on the records of that point.
//!
//! A joint shadow run plans for a portfolio. Its record names the portfolio, the generation
//! and the members ([`ShadowGroup`]), is kept under the portfolio's id, pairs with the joint
//! slate recorded for that generation, and takes the decision on the portfolio from the
//! portfolio object.
//!
//! In `agent` mode the run's slate may be the one the point got. The record then says which
//! slate was shown ([`Shown`]), and a reversed promotion of an agent-shown slate is reported to
//! the rollout the run was claimed from, which may roll the mode back.
//!
//! The records live in their own `AgentShadow` object, keyed by subject, or by portfolio for a
//! joint run. No GraphQL resolver,
//! `/api` route or attach path reads it (the operator's surfaces read `ActionPoint` and
//! `Portfolio` only), so a shadow slate cannot leak into the inbox. Each subject keeps its
//! last [`MAX_SHADOW_RECORDS`] records. Operators read them with Restate's state
//! introspection (`just agent-shadow-list`); the shared `records` handler serves the engine's
//! own reads. Shadow output is not a governed fact, so it never reaches the world model.

use std::collections::BTreeMap;

use ekg_cognition::RunRecord;
use ekg_domain::{ActionPointId, ActionPointState, EntityUri, ProposedAction, Slate};
use ekg_dynamic::{PointOutcome, PortfolioEvent, PortfolioOutcome};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::rollout::{AGENT_ROLLOUT_KEY, AGENT_ROLLOUT_OBJECT, IngressError, call_object};

/// The registered name of the shadow object.
pub const AGENT_SHADOW_OBJECT: &str = "AgentShadow";
/// How many records a subject keeps (the newest).
pub const MAX_SHADOW_RECORDS: usize = 20;

/// Restate state key of a subject's records.
const RECORDS: &str = "records";

/// How a shadow run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowOutcome {
    /// The agent's slate was produced.
    Completed,
    /// The kill switch stopped it before a candidate.
    Aborted,
    /// The slate could not be produced (or the guard abandoned it).
    Failed,
}

/// What a shadow run ran with: the engine's own version, the pinned skills and the profile's
/// tools (engine-side), and the model the runtime reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionTuple {
    /// The engine version (`CARGO_PKG_VERSION`).
    pub engine: String,
    /// The served model, as the runtime reported (empty when no run reported one).
    pub model: String,
    /// The pinned skill versions, by name.
    pub skills: BTreeMap<String, String>,
    /// The tools the profile offered.
    pub tools: Vec<String>,
}

/// The slate the existing path attached to the point a shadow run ran beside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedSlate {
    /// The point it was attached to.
    pub action_point: ActionPointId,
    /// Which of the subject's points that is, counted from 1 (they share one id).
    pub point_seq: u32,
    /// The slate as attached, with its policy needs.
    pub slate: Slate,
    /// When it was attached, as the point's record holds it.
    pub attached_at: String,
}

/// The joint slate the existing path produced for the portfolio a joint shadow run ran beside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedJointSlate {
    /// The portfolio it was produced for.
    pub portfolio: String,
    /// The generation it was produced for.
    pub generation: u32,
    /// The slate as produced, with its policy needs.
    pub slate: Slate,
    /// When the portfolio's joint slate was attached, as the portfolio's record holds it.
    pub attached_at: String,
}

/// The existing path's slate of the same step as a shadow run: a point's, or a portfolio's
/// joint slate. A point's form is written as it always was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Paired {
    /// A point's slate.
    Point(PairedSlate),
    /// A portfolio's joint slate.
    Joint(PairedJointSlate),
}

impl Paired {
    /// The paired slate.
    #[must_use]
    pub fn slate(&self) -> &Slate {
        match self {
            Self::Point(paired) => &paired.slate,
            Self::Joint(paired) => &paired.slate,
        }
    }

    /// When the slate the point or portfolio carried was attached.
    #[must_use]
    pub fn attached_at(&self) -> &str {
        match self {
            Self::Point(paired) => &paired.attached_at,
            Self::Joint(paired) => &paired.attached_at,
        }
    }

    /// The point form, if this is one.
    #[must_use]
    pub fn as_point(&self) -> Option<&PairedSlate> {
        match self {
            Self::Point(paired) => Some(paired),
            Self::Joint(_) => None,
        }
    }
}

impl From<PairedSlate> for Paired {
    fn from(paired: PairedSlate) -> Self {
        Self::Point(paired)
    }
}

/// The portfolio a joint shadow run planned for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowGroup {
    /// The portfolio's id.
    pub portfolio: String,
    /// The generation the run was produced for.
    pub generation: u32,
    /// The members it planned for.
    pub members: Vec<EntityUri>,
}

/// How a point or a portfolio ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    /// A plan was promoted and written back.
    Promoted,
    /// The stop control ended it.
    Aborted,
    /// Nobody acted before the escalation timer.
    Expired,
    /// An operator dismissed the portfolio's plan.
    Dismissed,
    /// The portfolio stopped being current before anyone decided it.
    Superseded,
}

/// What was decided on the point or the portfolio a shadow record is paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// How the point ended.
    pub state: DecisionState,
    /// When, in seconds since the Unix epoch.
    pub decided_at: u64,
    /// Who decided, as the point's audit trail holds it: an operator, a policy's own
    /// reviewer id, or `None` for the timer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// The executed actions of a promotion, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ProposedAction>,
    /// The option of the paired slate with exactly these actions, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_rank_key: Option<String>,
    /// The portfolio the point was promoted through, when not on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_portfolio: Option<String>,
    /// Whether the point still carried the paired slate. `false` means the slate was
    /// replaced after the shadow run, so the decision is no choice among the paired options.
    pub on_paired_slate: bool,
    /// When the promotion was reversed, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reversed_at: Option<u64>,
}

/// Which slate the point of an `agent`-mode run got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "slate", rename_all = "snake_case")]
pub enum Shown {
    /// The agent's slate.
    Agent,
    /// The existing path's slate, because the agent's could not be shown.
    Legacy {
        /// Why.
        reason: String,
    },
}

/// One shadow run, as recorded for comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowRecord {
    /// A unique id of the run.
    pub run_id: String,
    /// The subject the run planned for; for a joint run, the first member.
    pub subject: EntityUri,
    /// The portfolio a joint run planned for. Absent for a point's run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<ShadowGroup>,
    /// When it started, in seconds since the Unix epoch.
    pub started_at: u64,
    /// When it ended, in seconds since the Unix epoch.
    pub ended_at: u64,
    /// How it ended.
    pub outcome: ShadowOutcome,
    /// Why, for an aborted or failed run.
    pub reason: Option<String>,
    /// The agent's slate, when it was produced.
    pub slate: Option<Slate>,
    /// One record per candidate attempt that ran the agent.
    pub runs: Vec<RunRecord>,
    /// What it ran with.
    pub version: VersionTuple,
    /// The existing path's slate for the same point or portfolio, when one was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired: Option<Paired>,
    /// Why there is no paired slate, when the existing path attached none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_missing: Option<String>,
    /// The decision on the paired point, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    /// Which slate the point got, for a run in `agent` mode. Absent for a shadow run: the point
    /// got the existing path's slate, as always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<Shown>,
    /// The rollout epoch the run's slate was claimed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    /// The key of the rollout the slate was claimed from, when it is not the engine's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<String>,
}

impl ShadowRecord {
    /// The key the record is kept under: the portfolio's id for a joint run, the subject
    /// otherwise.
    #[must_use]
    pub fn key(&self) -> &str {
        self.group
            .as_ref()
            .map_or(self.subject.0.as_str(), |group| group.portfolio.as_str())
    }
}

/// A reversed promotion of an agent-shown slate, as it is reported to the rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReversedAgentSlate {
    /// The key of the rollout the slate was claimed from.
    pub rollout: String,
    /// The epoch it was claimed in.
    pub epoch: u32,
}

/// What applying a point's outcome did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Whether any record changed.
    pub changed: bool,
    /// The reversals that landed on a record whose slate shown was the agent's.
    pub reversed: Vec<ReversedAgentSlate>,
}

/// Append `record` to `records`, keeping the newest [`MAX_SHADOW_RECORDS`].
pub fn apply_record(records: &mut Vec<ShadowRecord>, record: ShadowRecord) {
    records.push(record);
    if records.len() > MAX_SHADOW_RECORDS {
        let excess = records.len() - MAX_SHADOW_RECORDS;
        records.drain(..excess);
    }
}

/// Apply a point's `outcome` to the records paired with that point, and say whether any
/// changed. A promotion, abort or expiry sets the decision once; a reversal adds its time to
/// the decision already there. Records of another point, and records without a paired slate,
/// are left alone.
pub fn apply_outcome(records: &mut [ShadowRecord], outcome: &PointOutcome) -> bool {
    apply_point_outcome(records, outcome).changed
}

/// [`apply_outcome`], also naming each reversal that landed on a record whose slate shown was
/// the agent's. A redelivered reversal lands on nothing, so it is named once.
pub fn apply_point_outcome(records: &mut [ShadowRecord], outcome: &PointOutcome) -> Applied {
    let mut applied = Applied::default();
    for record in records.iter_mut() {
        let reversible = record
            .decision
            .as_ref()
            .is_some_and(|decision| decision.reversed_at.is_none());
        if !apply_to_record(record, outcome) {
            continue;
        }
        applied.changed = true;
        let reversed = outcome.state == ActionPointState::Reversed && reversible;
        if let (true, Some(Shown::Agent), Some(epoch)) = (reversed, &record.shown, record.epoch) {
            applied.reversed.push(ReversedAgentSlate {
                rollout: record
                    .rollout
                    .clone()
                    .unwrap_or_else(|| AGENT_ROLLOUT_KEY.to_owned()),
                epoch,
            });
        }
    }
    applied
}

/// Apply `outcome` to one record, and say whether it changed.
fn apply_to_record(record: &mut ShadowRecord, outcome: &PointOutcome) -> bool {
    let Some(paired) = record
        .paired
        .as_ref()
        .and_then(Paired::as_point)
        .filter(|paired| {
            paired.action_point == outcome.action_point && paired.point_seq == outcome.point_seq
        })
    else {
        return false;
    };
    let state = match outcome.state {
        ActionPointState::Promoted => DecisionState::Promoted,
        ActionPointState::Aborted => DecisionState::Aborted,
        ActionPointState::Expired => DecisionState::Expired,
        ActionPointState::Reversed => {
            let Some(decision) = record
                .decision
                .as_mut()
                .filter(|decision| decision.reversed_at.is_none())
            else {
                return false;
            };
            decision.reversed_at = Some(outcome.at);
            return true;
        }
        _ => return false,
    };
    if record.decision.is_some() {
        return false;
    }
    let option_rank_key = (state == DecisionState::Promoted)
        .then(|| {
            paired
                .slate
                .options
                .iter()
                .find(|option| option.plan.actions == outcome.actions)
                .map(|option| option.score.rank_key.clone())
        })
        .flatten();
    record.decision = Some(Decision {
        state,
        decided_at: outcome.at,
        reviewer: outcome.reviewer.as_ref().map(|reviewer| reviewer.0.clone()),
        actions: outcome.actions.clone(),
        option_rank_key,
        via_portfolio: outcome
            .promoted_via
            .as_ref()
            .map(|via| via.portfolio.0.clone()),
        on_paired_slate: outcome.slate_attached_at.as_deref() == Some(&paired.attached_at),
        reversed_at: None,
    });
    true
}

/// Apply a portfolio's `outcome` to the joint records of that portfolio's generation. A
/// promotion, dismissal or supersession sets the decision once; an undo, in whole or in part,
/// adds its time to the decision already there, once. Records of another generation, and
/// point records, are left alone. Each undo that landed on a record whose slate shown was the
/// agent's is named.
pub fn apply_portfolio_outcome(
    records: &mut [ShadowRecord],
    outcome: &PortfolioOutcome,
) -> Applied {
    let mut applied = Applied::default();
    for record in records.iter_mut() {
        let of_generation = record.group.as_ref().is_some_and(|group| {
            group.portfolio == outcome.portfolio.0 && group.generation == outcome.generation
        });
        if !of_generation {
            continue;
        }
        let state = match outcome.event {
            PortfolioEvent::Promoted => DecisionState::Promoted,
            PortfolioEvent::Dismissed => DecisionState::Dismissed,
            PortfolioEvent::Superseded => DecisionState::Superseded,
            PortfolioEvent::PartiallyReversed | PortfolioEvent::Reversed => {
                let Some(decision) = record
                    .decision
                    .as_mut()
                    .filter(|decision| decision.reversed_at.is_none())
                else {
                    continue;
                };
                decision.reversed_at = Some(outcome.at);
                applied.changed = true;
                if let (Some(Shown::Agent), Some(epoch)) = (&record.shown, record.epoch) {
                    applied.reversed.push(ReversedAgentSlate {
                        rollout: record
                            .rollout
                            .clone()
                            .unwrap_or_else(|| AGENT_ROLLOUT_KEY.to_owned()),
                        epoch,
                    });
                }
                continue;
            }
            PortfolioEvent::Formed | PortfolioEvent::SlateAttached => continue,
        };
        if record.decision.is_some() {
            continue;
        }
        let paired = record.paired.as_ref();
        let option_rank_key = paired
            .filter(|_| state == DecisionState::Promoted)
            .and_then(|paired| {
                paired
                    .slate()
                    .options
                    .iter()
                    .find(|option| option.plan.actions == outcome.actions)
            })
            .map(|option| option.score.rank_key.clone());
        record.decision = Some(Decision {
            state,
            decided_at: outcome.at,
            reviewer: outcome.reviewer.as_ref().map(|reviewer| reviewer.0.clone()),
            actions: outcome.actions.clone(),
            option_rank_key,
            via_portfolio: None,
            on_paired_slate: paired.is_some_and(|paired| {
                outcome.slate_attached_at.as_deref() == Some(paired.attached_at())
            }),
            reversed_at: None,
        });
        applied.changed = true;
    }
    applied
}

/// The shadow object, keyed by subject, or by portfolio for joint records.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentShadow;

#[restate_sdk::object]
impl AgentShadow {
    /// Append a shadow record to the subject's list (the newest 20 are kept).
    #[handler]
    async fn record(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<ShadowRecord>,
    ) -> Result<(), HandlerError> {
        let mut records = ctx
            .get::<Json<Vec<ShadowRecord>>>(RECORDS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        apply_record(&mut records, record.into_inner());
        ctx.set(RECORDS, Json(records));
        Ok(())
    }

    /// Apply a point's outcome to the subject's records. A subject without a record of that
    /// point is left without state.
    #[handler]
    async fn decide(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<PointOutcome>,
    ) -> Result<(), HandlerError> {
        let mut records = ctx
            .get::<Json<Vec<ShadowRecord>>>(RECORDS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let applied = apply_point_outcome(&mut records, &outcome.into_inner());
        if applied.changed {
            ctx.set(RECORDS, Json(records));
        }
        // A durable one-way send, ordered after the state write; the rollout decides whether
        // the reversal rolls the mode back.
        for reversed in applied.reversed {
            ctx.request::<_, ()>(
                restate_sdk::context::RequestTarget::object(
                    AGENT_ROLLOUT_OBJECT,
                    reversed.rollout,
                    "report_reversal",
                ),
                Json(reversed.epoch),
            )
            .send();
        }
        Ok(())
    }

    /// Apply a portfolio's outcome to the joint records kept under it. A portfolio without a
    /// record of that generation is left without state.
    #[handler]
    async fn decide_portfolio(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<PortfolioOutcome>,
    ) -> Result<(), HandlerError> {
        let mut records = ctx
            .get::<Json<Vec<ShadowRecord>>>(RECORDS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let applied = apply_portfolio_outcome(&mut records, &outcome.into_inner());
        if applied.changed {
            ctx.set(RECORDS, Json(records));
        }
        // As for a point: the rollout decides whether the undo rolls the mode back.
        for reversed in applied.reversed {
            ctx.request::<_, ()>(
                restate_sdk::context::RequestTarget::object(
                    AGENT_ROLLOUT_OBJECT,
                    reversed.rollout,
                    "report_reversal",
                ),
                Json(reversed.epoch),
            )
            .send();
        }
        Ok(())
    }

    /// The subject's shadow records, oldest first. Shared: runs concurrently.
    #[handler]
    async fn records(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Vec<ShadowRecord>>, HandlerError> {
        Ok(Json(
            ctx.get::<Json<Vec<ShadowRecord>>>(RECORDS)
                .await?
                .map(Json::into_inner)
                .unwrap_or_default(),
        ))
    }
}

/// The shadow object over the Restate ingress (plain HTTP).
#[derive(Debug, Clone)]
pub struct ShadowClient {
    http: reqwest::Client,
    ingress: String,
}

impl ShadowClient {
    /// A client of the shadow object behind the ingress at `ingress`.
    #[must_use]
    pub fn new(http: reqwest::Client, ingress: impl Into<String>) -> Self {
        Self {
            http,
            ingress: ingress.into(),
        }
    }

    /// Record `record` under its subject, or under its portfolio for a joint run.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn record(&self, record: &ShadowRecord) -> Result<(), IngressError> {
        call_object(
            &self.http,
            &self.ingress,
            [AGENT_SHADOW_OBJECT, record.key(), "record"],
            Some(record),
        )
        .await
    }

    /// Apply `outcome` to the joint records of its portfolio, as the portfolio object does.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn decide_portfolio(&self, outcome: &PortfolioOutcome) -> Result<(), IngressError> {
        call_object(
            &self.http,
            &self.ingress,
            [
                AGENT_SHADOW_OBJECT,
                &outcome.portfolio.0,
                ekg_dynamic::PORTFOLIO_OUTCOME_HANDLER,
            ],
            Some(outcome),
        )
        .await
    }

    /// The shadow records kept under `key`: a subject's IRI, or a portfolio's id.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn records_under(&self, key: &str) -> Result<Vec<ShadowRecord>, IngressError> {
        call_object::<(), _>(
            &self.http,
            &self.ingress,
            [AGENT_SHADOW_OBJECT, key, "records"],
            None,
        )
        .await
    }

    /// Apply `outcome` to the records of `subject`, as the action-point object does.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn decide(
        &self,
        subject: &EntityUri,
        outcome: &PointOutcome,
    ) -> Result<(), IngressError> {
        call_object(
            &self.http,
            &self.ingress,
            [
                AGENT_SHADOW_OBJECT,
                &subject.0,
                ekg_dynamic::OUTCOME_HANDLER,
            ],
            Some(outcome),
        )
        .await
    }

    /// The shadow records of `subject`.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn records(&self, subject: &EntityUri) -> Result<Vec<ShadowRecord>, IngressError> {
        call_object::<(), _>(
            &self.http,
            &self.ingress,
            [AGENT_SHADOW_OBJECT, &subject.0, "records"],
            None,
        )
        .await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ekg_cognition::Termination;
    use ekg_domain::{Score, Trace};

    use super::*;

    fn shadow_record(run: usize) -> ShadowRecord {
        ShadowRecord {
            run_id: format!("run-{run}"),
            subject: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            group: None,
            started_at: 10,
            ended_at: 20,
            outcome: ShadowOutcome::Completed,
            reason: None,
            slate: Some(Slate {
                baseline: Score {
                    metrics: BTreeMap::new(),
                    rank_key: "9".to_owned(),
                },
                options: Vec::new(),
                trace: Trace::from_steps(Vec::new()),
            }),
            runs: vec![RunRecord {
                termination: Termination::Submitted,
                steps: 4,
                input_tokens: 400,
                output_tokens: 80,
                model: "m".to_owned(),
                elapsed_ms: 31_000,
                tool_calls: BTreeMap::from([(
                    "submit_plan".to_owned(),
                    BTreeMap::from([("ok".to_owned(), 1), ("rejected".to_owned(), 1)]),
                )]),
                rejections: vec![vec!["missing new_date".to_owned()]],
                skills: BTreeMap::from([("propose-plan".to_owned(), "abc".to_owned())]),
                tools: vec!["ground_entity".to_owned(), "submit_plan".to_owned()],
                trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_owned(),
                closing_answer: None,
                role: None,
                finding: None,
            }],
            version: VersionTuple {
                engine: "0.1.0".to_owned(),
                model: "m".to_owned(),
                skills: BTreeMap::from([("propose-plan".to_owned(), "abc".to_owned())]),
                tools: vec!["ground_entity".to_owned(), "submit_plan".to_owned()],
            },
            paired: None,
            paired_missing: None,
            decision: None,
            shown: None,
            epoch: None,
            rollout: None,
        }
    }

    #[test]
    fn a_subject_keeps_its_newest_twenty_records() {
        let mut records = Vec::new();
        for run in 0..25 {
            apply_record(&mut records, shadow_record(run));
        }
        assert_eq!(records.len(), MAX_SHADOW_RECORDS);
        assert_eq!(records[0].run_id, "run-5", "the oldest five were dropped");
        assert_eq!(records[19].run_id, "run-24");
    }

    #[test]
    fn a_run_record_stored_before_its_newer_fields_still_reads() {
        let mut json = serde_json::to_value(shadow_record(1)).expect("json");
        let run = json["runs"][0].as_object_mut().expect("run");
        run.remove("elapsed_ms");
        run.remove("tools");
        let back: ShadowRecord = serde_json::from_value(json).expect("an older record reads");
        assert_eq!(back.runs[0].elapsed_ms, 0);
        assert!(back.runs[0].tools.is_empty());
    }

    #[test]
    fn a_joint_record_keeps_its_analysts_runs_and_their_findings() {
        let finding = ekg_cognition::tools::Finding {
            member: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            state: ekg_cognition::tools::MemberState::OnPlan,
            causes: Vec::new(),
            origin: ekg_cognition::tools::FindingOrigin::Semantic,
            dropped: Vec::new(),
        };
        let mut record = shadow_record(1);
        let planner = record.runs[0].clone();
        record.runs = vec![
            RunRecord {
                role: Some(ekg_cognition::RunRole::Analyst),
                finding: Some(finding.clone()),
                ..planner.clone()
            },
            RunRecord {
                role: Some(ekg_cognition::RunRole::Planner),
                ..planner
            },
        ];
        let json = serde_json::to_value(&record).expect("json");
        assert_eq!(json["runs"][0]["role"], "analyst");
        assert_eq!(json["runs"][0]["finding"]["state"], "on_plan");
        assert!(json["runs"][1].get("finding").is_none());
        let back: ShadowRecord = serde_json::from_value(json).expect("reads");
        assert_eq!(back.runs[0].finding, Some(finding));
    }

    #[test]
    fn a_run_record_stored_without_a_role_reads_as_a_points() {
        let mut json = serde_json::to_value(shadow_record(1)).expect("json");
        assert!(
            json["runs"][0].get("role").is_none(),
            "left out for a point"
        );
        json["runs"][0]
            .as_object_mut()
            .expect("run")
            .remove("finding");
        let back: ShadowRecord = serde_json::from_value(json).expect("reads");
        assert_eq!(back.runs[0].role, None);
    }

    #[test]
    fn a_shadow_record_round_trips_as_snake_case_json() {
        let record = shadow_record(1);
        let json = serde_json::to_value(&record).expect("json");
        assert_eq!(json["outcome"], "completed");
        assert_eq!(json["runs"][0]["termination"], "submitted");
        assert_eq!(json["runs"][0]["tool_calls"]["submit_plan"]["rejected"], 1);
        assert_eq!(json["runs"][0]["rejections"][0][0], "missing new_date");
        assert_eq!(json["version"]["skills"]["propose-plan"], "abc");
        let back: ShadowRecord = serde_json::from_value(json).expect("back");
        assert_eq!(back, record);
    }

    // --- the paired slate and the decision ---

    const POINT: &str = "action-point/sale.order/1";
    const ATTACHED_AT: &str = "1700000000";
    /// The point the outcomes in these tests are of: the subject's second.
    const SEQ: u32 = 2;

    fn action(verb: &str) -> ProposedAction {
        ProposedAction {
            action_type: ekg_domain::ActionType(verb.to_owned()),
            target: EntityUri("https://ekg.example/id/purchase.order.line/7".to_owned()),
            params: BTreeMap::from([("new_date".to_owned(), "2027-11-08".to_owned())]),
        }
    }

    fn option(rank_key: &str, verb: &str) -> ekg_domain::RankedPlan {
        ekg_domain::RankedPlan {
            plan: ekg_domain::Plan {
                actions: vec![action(verb)],
            },
            score: Score {
                metrics: BTreeMap::new(),
                rank_key: rank_key.to_owned(),
            },
            recommended: false,
            needs: Vec::new(),
            omissions: Vec::new(),
            trace: Trace::from_steps(Vec::new()),
        }
    }

    /// A record paired with the subject's point number `point_seq`, whose slate offers
    /// `expedite` and `split`.
    fn paired_record(run: usize, point_seq: u32) -> ShadowRecord {
        let mut record = shadow_record(run);
        record.paired = Some(Paired::Point(PairedSlate {
            action_point: ActionPointId(POINT.to_owned()),
            point_seq,
            slate: Slate {
                baseline: Score {
                    metrics: BTreeMap::new(),
                    rank_key: "9".to_owned(),
                },
                options: vec![option("1~aa", "expedite"), option("2~bb", "split")],
                trace: Trace::from_steps(Vec::new()),
            },
            attached_at: ATTACHED_AT.to_owned(),
        }));
        record
    }

    const PORTFOLIO: &str = "portfolio/0123456789ab";

    /// A joint record of the portfolio at `generation`, paired with a joint slate that offers
    /// `expedite` and `split`.
    fn joint_record(run: usize, generation: u32) -> ShadowRecord {
        let mut record = paired_record(run, SEQ);
        let slate = record.paired.take().expect("paired").slate().clone();
        record.group = Some(ShadowGroup {
            portfolio: PORTFOLIO.to_owned(),
            generation,
            members: vec![
                record.subject.clone(),
                EntityUri("https://ekg.example/id/mrp.production/7".to_owned()),
            ],
        });
        record.paired = Some(Paired::Joint(PairedJointSlate {
            portfolio: PORTFOLIO.to_owned(),
            generation,
            slate,
            attached_at: ATTACHED_AT.to_owned(),
        }));
        record
    }

    fn portfolio_outcome(event: PortfolioEvent, verb: Option<&str>) -> PortfolioOutcome {
        PortfolioOutcome {
            portfolio: ekg_domain::PortfolioId(PORTFOLIO.to_owned()),
            generation: 2,
            event,
            at: 500,
            reviewer: (event != PortfolioEvent::Superseded)
                .then(|| ekg_domain::ReviewerId("planner-1".to_owned())),
            actions: verb.map(action).into_iter().collect(),
            rank_key: verb.map(|_| "2~bb".to_owned()),
            slate_attached_at: Some(ATTACHED_AT.to_owned()),
        }
    }

    #[test]
    fn a_joint_record_round_trips_under_its_portfolio_and_a_points_shape_is_unchanged() {
        let joint = joint_record(1, 2);
        assert_eq!(joint.key(), PORTFOLIO, "kept under the portfolio's id");
        let json = serde_json::to_value(&joint).expect("json");
        assert_eq!(json["group"]["portfolio"], PORTFOLIO);
        assert_eq!(json["group"]["generation"], 2);
        assert_eq!(json["group"]["members"].as_array().map(Vec::len), Some(2));
        assert_eq!(json["paired"]["portfolio"], PORTFOLIO);
        assert_eq!(json["paired"]["generation"], 2);
        assert!(json["paired"].get("action_point").is_none());
        assert_eq!(
            json["subject"], joint.subject.0,
            "the first member, for a reader that knows no group"
        );
        let back: ShadowRecord = serde_json::from_value(json).expect("back");
        assert_eq!(back, joint);

        // A point's record: no group, and the paired slate written as it always was.
        let point = paired_record(1, SEQ);
        assert_eq!(point.key(), point.subject.0);
        let json = serde_json::to_value(&point).expect("json");
        assert!(json.get("group").is_none());
        let paired: Vec<&str> = json["paired"]
            .as_object()
            .expect("paired")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            paired,
            ["action_point", "attached_at", "point_seq", "slate"]
        );
        let back: ShadowRecord = serde_json::from_value(json).expect("back");
        assert!(matches!(back.paired, Some(Paired::Point(_))));
        assert_eq!(back, point);
    }

    #[test]
    fn a_record_stored_before_joint_records_existed_reads_unchanged() {
        // As written before `group` and the joint form of `paired`: neither field is there.
        let mut json = serde_json::to_value(paired_record(1, SEQ)).expect("json");
        assert!(json.get("group").is_none());
        json["decision"] = serde_json::json!({
            "state": "expired", "decided_at": 500, "on_paired_slate": true
        });
        let back: ShadowRecord = serde_json::from_value(json).expect("an older record reads");
        assert_eq!(back.group, None);
        assert_eq!(
            back.paired
                .as_ref()
                .and_then(Paired::as_point)
                .map(|p| p.point_seq),
            Some(SEQ)
        );
        assert_eq!(
            back.decision.map(|decision| decision.state),
            Some(DecisionState::Expired)
        );
    }

    #[test]
    fn a_promoted_portfolio_sets_the_decision_with_the_matched_options_rank_key() {
        let mut records = vec![joint_record(1, 2)];
        let applied = apply_portfolio_outcome(
            &mut records,
            &portfolio_outcome(PortfolioEvent::Promoted, Some("split")),
        );
        assert!(applied.changed && applied.reversed.is_empty());
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(decision.state, DecisionState::Promoted);
        assert_eq!(decision.decided_at, 500);
        assert_eq!(decision.reviewer.as_deref(), Some("planner-1"));
        assert_eq!(decision.actions, [action("split")]);
        assert_eq!(decision.option_rank_key.as_deref(), Some("2~bb"));
        assert!(decision.on_paired_slate);
        assert_eq!(decision.via_portfolio, None);

        // Actions no option of the paired slate has: the decision stands, without a match.
        let mut records = vec![joint_record(1, 2)];
        apply_portfolio_outcome(
            &mut records,
            &portfolio_outcome(PortfolioEvent::Promoted, Some("re_promise")),
        );
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(decision.option_rank_key, None);
    }

    #[test]
    fn a_dismissed_and_a_superseded_portfolio_are_recorded_as_such() {
        for (event, state, reviewer) in [
            (
                PortfolioEvent::Dismissed,
                DecisionState::Dismissed,
                Some("planner-1"),
            ),
            (PortfolioEvent::Superseded, DecisionState::Superseded, None),
        ] {
            let mut records = vec![joint_record(1, 2)];
            assert!(apply_portfolio_outcome(&mut records, &portfolio_outcome(event, None)).changed);
            let decision = records[0].decision.clone().expect("a decision");
            assert_eq!(decision.state, state);
            assert_eq!(decision.reviewer.as_deref(), reviewer);
            assert!(decision.actions.is_empty() && decision.option_rank_key.is_none());
            // A second outcome does not replace the first.
            let again = portfolio_outcome(PortfolioEvent::Promoted, Some("split"));
            assert!(!apply_portfolio_outcome(&mut records, &again).changed);
            assert_eq!(records[0].decision.as_ref().map(|d| d.state), Some(state));
        }
        let json = serde_json::to_value(DecisionState::Superseded).expect("json");
        assert_eq!(json, "superseded");
    }

    #[test]
    fn an_outcome_of_another_generation_or_portfolio_is_ignored_and_so_is_a_point_record() {
        let mut records = vec![joint_record(1, 1), paired_record(2, SEQ)];
        let promoted = portfolio_outcome(PortfolioEvent::Promoted, Some("split"));
        assert!(!apply_portfolio_outcome(&mut records, &promoted).changed);
        assert!(records.iter().all(|record| record.decision.is_none()));

        let mut other = joint_record(1, 2);
        other.group.as_mut().expect("group").portfolio = "portfolio/ffff".to_owned();
        let mut records = vec![other];
        assert!(!apply_portfolio_outcome(&mut records, &promoted).changed);

        // And a point's outcome never lands on a joint record.
        let mut records = vec![joint_record(1, 2)];
        assert!(!apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split"))
        ));
    }

    #[test]
    fn an_undo_in_part_or_in_whole_adds_its_time_once_and_names_an_agent_shown_slate() {
        for first in [PortfolioEvent::PartiallyReversed, PortfolioEvent::Reversed] {
            let mut shown = joint_record(1, 2);
            shown.shown = Some(Shown::Agent);
            shown.epoch = Some(4);
            let mut legacy = joint_record(2, 2);
            legacy.shown = Some(Shown::Legacy {
                reason: "the agent's run failed".to_owned(),
            });
            legacy.epoch = Some(4);
            let mut records = vec![shown, legacy];
            // An undo before any promotion lands on nothing.
            assert!(
                !apply_portfolio_outcome(&mut records, &portfolio_outcome(first, None)).changed
            );
            apply_portfolio_outcome(
                &mut records,
                &portfolio_outcome(PortfolioEvent::Promoted, Some("split")),
            );
            let mut undo = portfolio_outcome(first, None);
            undo.at = 900;
            let applied = apply_portfolio_outcome(&mut records, &undo);
            assert!(applied.changed);
            assert_eq!(
                applied.reversed,
                [ReversedAgentSlate {
                    rollout: AGENT_ROLLOUT_KEY.to_owned(),
                    epoch: 4
                }],
                "only the record whose slate shown was the agent's"
            );
            for record in &records {
                let decision = record.decision.as_ref().expect("a decision");
                assert_eq!(decision.state, DecisionState::Promoted, "still promoted");
                assert_eq!(decision.actions, [action("split")]);
                assert_eq!(decision.reversed_at, Some(900));
            }
            // The rest of a partial undo, or a redelivery, is not counted again.
            let again = apply_portfolio_outcome(
                &mut records,
                &portfolio_outcome(PortfolioEvent::Reversed, None),
            );
            assert!(!again.changed && again.reversed.is_empty());
        }
    }

    #[test]
    fn a_joint_record_without_a_paired_slate_still_takes_the_decision() {
        let mut record = joint_record(1, 2);
        record.paired = None;
        record.paired_missing = Some("the joint slate could not be produced".to_owned());
        let mut records = vec![record];
        let promoted = portfolio_outcome(PortfolioEvent::Promoted, Some("split"));
        assert!(apply_portfolio_outcome(&mut records, &promoted).changed);
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(decision.option_rank_key, None);
        assert!(!decision.on_paired_slate);
    }

    fn outcome(state: ActionPointState, verb: Option<&str>) -> PointOutcome {
        PointOutcome {
            action_point: ActionPointId(POINT.to_owned()),
            point_seq: SEQ,
            state,
            at: 500,
            reviewer: (state != ActionPointState::Expired)
                .then(|| ekg_domain::ReviewerId("planner-1".to_owned())),
            actions: verb.map(action).into_iter().collect(),
            promoted_via: None,
            slate_attached_at: Some(ATTACHED_AT.to_owned()),
        }
    }

    #[test]
    fn a_record_with_its_paired_slate_and_decision_round_trips_as_snake_case_json() {
        let mut records = vec![paired_record(1, SEQ)];
        assert!(apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split"))
        ));
        let json = serde_json::to_value(&records[0]).expect("json");
        assert_eq!(json["paired"]["action_point"], POINT);
        assert_eq!(json["paired"]["point_seq"], SEQ);
        assert_eq!(json["paired"]["attached_at"], ATTACHED_AT);
        assert_eq!(json["decision"]["state"], "promoted");
        assert_eq!(json["decision"]["option_rank_key"], "2~bb");
        assert_eq!(json["decision"]["on_paired_slate"], true);
        let back: ShadowRecord = serde_json::from_value(json).expect("back");
        assert_eq!(back, records[0]);
    }

    #[test]
    fn a_record_stored_before_the_pairing_reads_with_none_of_it() {
        let json = serde_json::to_value(shadow_record(1)).expect("json");
        let stored = json.as_object().expect("a record");
        for field in [
            "paired",
            "paired_missing",
            "decision",
            "shown",
            "epoch",
            "rollout",
        ] {
            assert!(!stored.contains_key(field), "{field} is absent when unset");
        }
        let back: ShadowRecord = serde_json::from_value(json).expect("an older record reads");
        assert_eq!(back, shadow_record(1));
        assert_eq!(back.paired, None);
        assert_eq!(back.decision, None);
    }

    #[test]
    fn a_promotion_names_the_paired_option_with_the_executed_actions() {
        let mut records = vec![paired_record(1, SEQ)];
        assert!(apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split"))
        ));
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(decision.state, DecisionState::Promoted);
        assert_eq!(decision.decided_at, 500);
        assert_eq!(decision.reviewer.as_deref(), Some("planner-1"));
        assert_eq!(decision.actions, vec![action("split")]);
        assert_eq!(decision.option_rank_key.as_deref(), Some("2~bb"));
        assert_eq!(decision.via_portfolio, None);
        assert!(decision.on_paired_slate);
        assert_eq!(decision.reversed_at, None);
    }

    #[test]
    fn a_promotion_of_a_plan_that_is_no_paired_option_names_none() {
        let mut records = vec![paired_record(1, SEQ)];
        apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("cancel")),
        );
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(decision.actions, vec![action("cancel")]);
        assert_eq!(decision.option_rank_key, None);
    }

    #[test]
    fn a_promotion_through_a_portfolio_names_the_portfolio() {
        let mut records = vec![paired_record(1, SEQ)];
        let mut promoted = outcome(ActionPointState::Promoted, Some("expedite"));
        promoted.promoted_via = Some(ekg_dynamic::PromotedVia {
            portfolio: ekg_domain::PortfolioId("portfolio/0123456789ab".to_owned()),
            generation: 2,
            rank_key: "0~joint".to_owned(),
        });
        apply_outcome(&mut records, &promoted);
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(
            decision.via_portfolio.as_deref(),
            Some("portfolio/0123456789ab")
        );
    }

    #[test]
    fn an_abort_and_an_expiry_are_recorded_without_actions() {
        let mut aborted = vec![paired_record(1, SEQ)];
        assert!(apply_outcome(
            &mut aborted,
            &outcome(ActionPointState::Aborted, None)
        ));
        let decision = aborted[0].decision.clone().expect("a decision");
        assert_eq!(decision.state, DecisionState::Aborted);
        assert_eq!(decision.reviewer.as_deref(), Some("planner-1"));
        assert!(decision.actions.is_empty());
        assert_eq!(decision.option_rank_key, None);

        let mut expired = vec![paired_record(1, SEQ)];
        assert!(apply_outcome(
            &mut expired,
            &outcome(ActionPointState::Expired, None)
        ));
        let decision = expired[0].decision.clone().expect("a decision");
        assert_eq!(decision.state, DecisionState::Expired);
        assert_eq!(decision.reviewer, None);
    }

    #[test]
    fn a_reversal_is_added_to_the_promotion_and_keeps_its_fields() {
        let mut records = vec![paired_record(1, SEQ)];
        apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split")),
        );
        let mut reversed = outcome(ActionPointState::Reversed, None);
        reversed.at = 900;
        assert!(apply_outcome(&mut records, &reversed));
        let decision = records[0].decision.clone().expect("a decision");
        assert_eq!(decision.state, DecisionState::Promoted);
        assert_eq!(decision.actions, vec![action("split")]);
        assert_eq!(decision.reversed_at, Some(900));
        // A redelivered reversal changes nothing.
        reversed.at = 901;
        assert!(!apply_outcome(&mut records, &reversed));
        assert_eq!(
            records[0].decision.as_ref().and_then(|d| d.reversed_at),
            Some(900)
        );
    }

    #[test]
    fn a_reversal_without_a_decision_changes_nothing() {
        let mut records = vec![paired_record(1, SEQ)];
        assert!(!apply_outcome(
            &mut records,
            &outcome(ActionPointState::Reversed, None)
        ));
        assert_eq!(records[0].decision, None);
    }

    #[test]
    fn a_second_outcome_for_the_same_point_keeps_the_first_decision() {
        let mut records = vec![paired_record(1, SEQ)];
        apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split")),
        );
        assert!(!apply_outcome(
            &mut records,
            &outcome(ActionPointState::Aborted, None)
        ));
        assert_eq!(
            records[0].decision.as_ref().map(|d| d.state),
            Some(DecisionState::Promoted)
        );
    }

    #[test]
    fn records_of_another_point_and_unpaired_records_are_left_alone() {
        let mut records = vec![
            paired_record(1, SEQ - 1),
            shadow_record(2),
            paired_record(3, SEQ),
        ];
        assert!(apply_outcome(
            &mut records,
            &outcome(ActionPointState::Aborted, None)
        ));
        assert_eq!(records[0].decision, None, "an earlier point of the subject");
        assert_eq!(records[1].decision, None, "no paired slate");
        assert!(records[2].decision.is_some());
    }

    #[test]
    fn a_decision_on_a_slate_attached_later_says_so() {
        let mut records = vec![paired_record(1, SEQ)];
        let mut promoted = outcome(ActionPointState::Promoted, Some("expedite"));
        promoted.slate_attached_at = Some("1700009999".to_owned());
        apply_outcome(&mut records, &promoted);
        let decision = records[0].decision.clone().expect("a decision");
        assert!(!decision.on_paired_slate);
    }

    #[test]
    fn an_outcome_for_a_subject_without_records_changes_nothing() {
        let mut records = Vec::new();
        assert!(!apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split"))
        ));
        assert!(records.is_empty());
    }

    #[test]
    fn a_state_that_is_no_outcome_changes_nothing() {
        let mut records = vec![paired_record(1, SEQ)];
        assert!(!apply_outcome(
            &mut records,
            &outcome(ActionPointState::Proposed, None)
        ));
        assert_eq!(records[0].decision, None);
    }

    // --- which slate was shown ---

    /// A paired record of an `agent`-mode run claimed in epoch 4, promoted on `split`.
    fn promoted_record(shown: Option<Shown>, rollout: Option<&str>) -> ShadowRecord {
        let mut record = paired_record(1, SEQ);
        record.shown = shown;
        record.epoch = Some(4);
        record.rollout = rollout.map(str::to_owned);
        let mut records = vec![record];
        apply_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split")),
        );
        records.remove(0)
    }

    #[test]
    fn which_slate_was_shown_round_trips_as_snake_case_json() {
        let mut record = paired_record(1, SEQ);
        record.shown = Some(Shown::Agent);
        record.epoch = Some(4);
        let json = serde_json::to_value(&record).expect("json");
        assert_eq!(json["shown"], serde_json::json!({ "slate": "agent" }));
        assert_eq!(json["epoch"], 4);
        assert!(json.get("rollout").is_none(), "the engine's own rollout");
        assert_eq!(
            serde_json::from_value::<ShadowRecord>(json).expect("back"),
            record
        );
        let fallback = Shown::Legacy {
            reason: "the agent's run failed: model down".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&fallback).expect("json"),
            serde_json::json!({
                "slate": "legacy", "reason": "the agent's run failed: model down"
            })
        );
    }

    #[test]
    fn a_reversal_on_an_agent_shown_record_is_reported_once() {
        let mut records = vec![promoted_record(Some(Shown::Agent), None)];
        let promoted = apply_point_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split")),
        );
        assert_eq!(promoted, Applied::default(), "already decided");

        let reversed = outcome(ActionPointState::Reversed, None);
        let applied = apply_point_outcome(&mut records, &reversed);
        assert!(applied.changed);
        assert_eq!(
            applied.reversed,
            vec![ReversedAgentSlate {
                rollout: AGENT_ROLLOUT_KEY.to_owned(),
                epoch: 4
            }]
        );
        // Redelivered: nothing changes, and nothing is reported again.
        assert_eq!(
            apply_point_outcome(&mut records, &reversed),
            Applied::default()
        );

        // A run claimed from a rollout of its own reports there.
        let mut own = vec![promoted_record(Some(Shown::Agent), Some("suite"))];
        let applied = apply_point_outcome(&mut own, &reversed);
        assert_eq!(applied.reversed[0].rollout, "suite");
    }

    #[test]
    fn a_reversal_on_a_shadow_or_fallback_record_is_not_reported() {
        let fallback = Shown::Legacy {
            reason: "the agent's run failed".to_owned(),
        };
        for shown in [None, Some(fallback)] {
            let mut records = vec![promoted_record(shown, None)];
            let applied =
                apply_point_outcome(&mut records, &outcome(ActionPointState::Reversed, None));
            assert!(applied.changed, "the reversal is still recorded");
            assert!(applied.reversed.is_empty());
        }
        // A promotion of an agent-shown slate is no reversal.
        let mut record = paired_record(1, SEQ);
        record.shown = Some(Shown::Agent);
        record.epoch = Some(4);
        let mut records = vec![record];
        let applied = apply_point_outcome(
            &mut records,
            &outcome(ActionPointState::Promoted, Some("split")),
        );
        assert!(applied.changed && applied.reversed.is_empty());
    }
}
