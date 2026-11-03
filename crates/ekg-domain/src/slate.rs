//! The ranked remediation slate: the cognition loop's output for an action point.
//! It is an ordered set of candidate [`Plan`]s the operator compares before
//! promoting one, plus a do-nothing baseline.
//!
//! The base fixes only the shape of a slate and names no metric: a [`Score`] is an
//! opaque container the capability fills, just as the [`Simulator`]'s projection
//! carries no outcome vocabulary. The metric meaning lives in the capability layer
//! (`ekg-semantic`).
//!
//! [`Simulator`]: crate::seams::Simulator
//! [`Plan`]: crate::proposal::Plan

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::grounding::Omission;
use crate::ids::EntityUri;
use crate::proposal::Plan;

/// A group of coupled subjects planned together by one slate, e.g. the members of a
/// portfolio of coupled action points.
///
/// `key` is an opaque identifier of the group (a portfolio id, say) and is never
/// parsed as an entity; `members` are the subjects the group plans for; `links` are
/// the human-readable reasons the members are coupled, in the order they were
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectGroup {
    /// The opaque key of the group (never an entity IRI).
    pub key: String,
    /// The subjects the group plans for, in a stable order.
    pub members: Vec<EntityUri>,
    /// Why the members are coupled, one human-readable reason per link.
    pub links: Vec<String>,
    /// Whether `links` are the capability's worded sentences, which name every member
    /// themselves, rather than the recorded reasons. Defaulted so older groups load.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worded: bool,
}

/// One coupling link between two subjects, as recorded when their group formed: its two
/// ends (ordered by IRI), the reason as found (never model-generated), and the
/// capability's opaque facts about it (what kind of link it is and what it names).
///
/// Only the capability that fills `facts` reads them, e.g. to word the link for an
/// operator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CouplingLink {
    /// The lesser end (by IRI).
    pub a: EntityUri,
    /// The greater end (by IRI).
    pub b: EntityUri,
    /// Why the two are coupled, as found.
    pub reason: String,
    /// The capability's facts about the link. Defaulted and skipped when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
}

/// An opaque score for one candidate (or the baseline), filled by the capability.
///
/// `metrics` is a bag the capability fills from its own rule over a [`Projection`]
/// (e.g. whether the drift signal cleared, a projected completion date, a projected
/// cost), and `rank_key` is the capability-defined ordering value the agent sorts on.
/// Smaller sorts first (best-first). Only the capability knows what a key means.
///
/// [`Projection`]: crate::seams::Projection
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    /// Capability-filled metrics, keyed by a capability-defined name. Opaque to the
    /// base.
    pub metrics: BTreeMap<String, String>,
    /// The capability-defined ordering value the agent ranks on; lexically smaller
    /// sorts first (best-first). Opaque to the base. On an admitted slate option the
    /// slate loop extends it with a digest of the option's plan, so it also identifies
    /// the option uniquely within its slate.
    pub rank_key: String,
}

/// A structured reason a candidate cannot be auto-applied and needs a human, e.g.
/// a policy predicate it fails. Carried so oversight sees why a plan waits, never
/// parsed from prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Need {
    /// A stable, capability-defined reason code.
    pub code: String,
    /// A human-readable detail for the reason.
    pub detail: String,
}

/// The kind of reasoning a [`TraceStep`] records: the tag surfaced for oversight.
///
/// The base fixes only the tag set; what a step's text says is the capability's. The
/// tags are the ones Plant Pulse shows under "how the agent got here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceTag {
    /// A candidate plan (the do-nothing baseline, an admitted option, or a rejected
    /// candidate) was projected onto a non-authoritative what-if and scored.
    Simulated,
    /// A traversal of the governed evidence subgraph the reasoning rested on.
    Graph,
    /// An open assumption forced by a grounding omission (a node or edge read-policy
    /// hid), carried structurally, never parsed from model prose.
    Assumption,
    /// A model generation attempt that yielded no scorable candidate (its failure
    /// class and reason), or a time budget that ended generation. Retained like a
    /// rejected candidate, never simulated.
    Generation,
}

/// One step of the agent's reasoning trace: a [`TraceTag`], a human-readable `text`,
/// and a `rejected` flag set when the step records a candidate that was tried and set
/// aside, so oversight sees what was considered and dropped.
///
/// `text` is opaque to the base. The loop fills it from neutral evidence (an opaque
/// [`Score`], a [`crate::grounding::Omission`], an evidence edge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    /// Which kind of reasoning this step records.
    pub tag: TraceTag,
    /// The human-readable line surfaced for oversight (opaque to the base).
    pub text: String,
    /// Whether this step describes a candidate that was tried and set aside.
    pub rejected: bool,
}

/// Summary counts over a [`Trace`], so oversight sees the shape of the reasoning at a
/// glance (N plans simulated, M rejected, K open assumptions). Derived from the steps
/// by [`Trace::from_steps`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceSummary {
    /// How many plans were simulated: every [`TraceTag::Simulated`] step, including
    /// the do-nothing baseline.
    pub simulated: u32,
    /// How many steps describe a candidate that was tried and set aside (`rejected`).
    pub rejected: u32,
    /// How many open-assumption steps ([`TraceTag::Assumption`]) the trace carries.
    pub open_assumptions: u32,
}

/// An ordered reasoning trace: the tagged steps the durable agent loop performed to
/// reach a slate, plus derived [`TraceSummary`] counts. Surfaced for human oversight
/// (EU AI Act Art. 14) and recorded in the audit trail (Art. 12).
///
/// The summary cannot disagree with the steps: [`Trace::from_steps`] derives it on
/// construction, and it is recomputed from the steps on deserialize (via
/// `#[serde(from)]`), so a persisted or forged `summary` is corrected on load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TraceWire")]
pub struct Trace {
    /// The reasoning steps, in the order the loop performed them.
    pub steps: Vec<TraceStep>,
    /// Counts derived from `steps` (see [`Trace::from_steps`]); recomputed on load.
    pub summary: TraceSummary,
}

/// The wire form [`Trace`] deserializes through: only `steps` are read, and any
/// incoming `summary` is ignored and recomputed. `summary` stays in the serialized
/// output (the derived `Serialize` on `Trace` emits it) for the frontend, which reads
/// the counts directly.
#[derive(Deserialize)]
struct TraceWire {
    #[serde(default)]
    steps: Vec<TraceStep>,
}

impl From<TraceWire> for Trace {
    fn from(wire: TraceWire) -> Self {
        Self::from_steps(wire.steps)
    }
}

impl Trace {
    /// Assemble a trace from its ordered steps, deriving the [`TraceSummary`] from them.
    #[must_use]
    pub fn from_steps(steps: Vec<TraceStep>) -> Self {
        let count = |predicate: fn(&TraceStep) -> bool| {
            u32::try_from(steps.iter().filter(|step| predicate(step)).count()).unwrap_or(u32::MAX)
        };
        let summary = TraceSummary {
            simulated: count(|step| step.tag == TraceTag::Simulated),
            rejected: count(|step| step.rejected),
            open_assumptions: count(|step| step.tag == TraceTag::Assumption),
        };
        Self { steps, summary }
    }
}

/// One ranked candidate on a slate: a governed [`Plan`], its opaque [`Score`], a
/// `recommended` flag, the `needs` reasons it cannot be auto-applied, and the
/// omissions of the grounding it was built from.
///
/// Like a single [`Proposal`], a candidate discloses its grounding's `omissions`
/// structurally, so a plan the operator compares is never shown as complete when
/// facts were hidden.
///
/// [`Plan`]: crate::proposal::Plan
/// [`Proposal`]: crate::proposal::Proposal
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedPlan {
    /// The candidate plan (one or more governed actions, promoted as one step).
    pub plan: Plan,
    /// The opaque, capability-filled score for this candidate.
    pub score: Score,
    /// Whether this candidate is the recommended option. At most one candidate on a
    /// slate is recommended (the agent's choice; the base only carries the flag).
    pub recommended: bool,
    /// Structured reasons this candidate cannot be auto-applied (empty if none).
    pub needs: Vec<Need>,
    /// The omissions recorded by the grounding this candidate was built from.
    pub omissions: Vec<Omission>,
    /// The reasoning-trace slice for this candidate: the graph, assumption, and
    /// simulated steps specific to this plan (a subset of [`Slate::trace`]).
    /// Defaulted so a [`RankedPlan`] persisted without a trace still deserializes.
    #[serde(default)]
    pub trace: Trace,
}

/// One already-tried candidate fed back into the revise stage of the slate loop:
/// a candidate [`Plan`] paired with the opaque [`Score`] it was observed to earn.
///
/// A proposer uses a slice of these to steer the next candidate away from what was
/// already tried and toward a better `rank_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoredCandidate {
    /// The candidate plan that was tried.
    pub plan: Plan,
    /// The opaque score it earned under projection.
    pub score: Score,
}

/// A ranked remediation slate for one action point: an ordered set of candidate
/// [`RankedPlan`]s (best-first) plus a do-nothing baseline [`Score`].
///
/// The baseline is the score of taking no action, the reference every option is
/// compared against, so doing nothing is always an explicit, scored choice. Producing
/// a slate commits nothing: every option is a proposal, and a separate human promotion
/// of one chosen plan remains the only path to a write-back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slate {
    /// The do-nothing baseline score (the outcome if no action is taken).
    pub baseline: Score,
    /// The candidate plans, ordered best-first. May be empty: a slate with no
    /// admissible candidate that beats the baseline carries only the baseline.
    pub options: Vec<RankedPlan>,
    /// The reasoning trace covering the whole loop: the do-nothing baseline, every
    /// candidate (including rejected ones, flagged `rejected`), and the assumptions
    /// carried. Surfaced for oversight and audited.
    /// Defaulted so a [`Slate`] persisted without a trace (in `ActionPointRecord.slate`)
    /// still deserializes.
    #[serde(default)]
    pub trace: Trace,
}

impl Slate {
    /// The recommended option, if the agent flagged one. At most one option is
    /// recommended; this returns the first so flagged.
    #[must_use]
    pub fn recommended(&self) -> Option<&RankedPlan> {
        self.options.iter().find(|option| option.recommended)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::{Trace, TraceStep, TraceTag};

    fn step(tag: TraceTag, rejected: bool) -> TraceStep {
        TraceStep {
            tag,
            text: "why".to_owned(),
            rejected,
        }
    }

    #[test]
    fn from_steps_derives_summary_counts_that_cannot_drift() {
        let trace = Trace::from_steps(vec![
            step(TraceTag::Simulated, false), // baseline
            step(TraceTag::Graph, false),
            step(TraceTag::Assumption, false),
            step(TraceTag::Simulated, false), // an admitted candidate
            step(TraceTag::Simulated, true),  // a rejected candidate
        ]);
        assert_eq!(trace.summary.simulated, 3);
        assert_eq!(trace.summary.rejected, 1);
        assert_eq!(trace.summary.open_assumptions, 1);
    }

    #[test]
    fn a_forged_summary_is_recomputed_from_the_steps_on_load() {
        // A trace whose persisted or forged `summary` disagrees with its `steps` loads
        // with the summary recomputed from the steps. Here the steps hold 1 simulated,
        // 1 rejected and 0 assumptions, but the summary lies.
        let forged = r#"{
            "steps": [
                { "tag": "simulated", "text": "a", "rejected": false },
                { "tag": "simulated", "text": "b", "rejected": true }
            ],
            "summary": { "simulated": 99, "rejected": 0, "open_assumptions": 42 }
        }"#;
        let trace: Trace = serde_json::from_str(forged).expect("deserialize");
        assert_eq!(trace.summary.simulated, 2, "recomputed, not the forged 99");
        assert_eq!(trace.summary.rejected, 1, "recomputed, not the forged 0");
        assert_eq!(
            trace.summary.open_assumptions, 0,
            "recomputed, not the forged 42"
        );
    }

    #[test]
    fn a_legacy_slate_without_a_trace_still_deserializes() {
        use super::Slate;
        // A slate persisted without a trace is nested inside `ActionPointRecord.slate`
        // as Restate JSON with no `trace` key on the slate or its options.
        // `#[serde(default)]` must fill both with an empty trace.
        let legacy = r#"{
            "baseline": { "metrics": {}, "rank_key": "00001" },
            "options": [
                {
                    "plan": { "actions": [] },
                    "score": { "metrics": {}, "rank_key": "00000" },
                    "recommended": true,
                    "needs": [],
                    "omissions": []
                }
            ]
        }"#;
        let slate: Slate = serde_json::from_str(legacy).expect("legacy slate deserializes");
        assert!(slate.trace.steps.is_empty(), "slate trace defaults empty");
        assert!(
            slate.options[0].trace.steps.is_empty(),
            "option trace defaults empty"
        );
        assert_eq!(slate.trace.summary, super::TraceSummary::default());
    }

    #[test]
    fn a_persisted_trace_without_generation_steps_still_decodes() {
        // A persisted trace that holds no `generation` step loads unchanged.
        let persisted = r#"{
            "steps": [
                { "tag": "simulated", "text": "baseline", "rejected": false },
                { "tag": "graph", "text": "read", "rejected": false },
                { "tag": "assumption", "text": "hidden", "rejected": false }
            ],
            "summary": { "simulated": 1, "rejected": 0, "open_assumptions": 1 }
        }"#;
        let trace: Trace = serde_json::from_str(persisted).expect("persisted trace decodes");
        assert_eq!(trace.steps.len(), 3);
        assert!(trace.steps.iter().all(|s| s.tag != TraceTag::Generation));
    }

    #[test]
    fn a_generation_step_round_trips_and_counts_as_rejected_not_simulated() {
        let trace = Trace::from_steps(vec![
            step(TraceTag::Simulated, false), // baseline
            step(TraceTag::Generation, true), // a failed attempt, never projected
        ]);
        assert_eq!(
            trace.summary.simulated, 1,
            "a generation step is not simulated"
        );
        assert_eq!(trace.summary.rejected, 1, "a generation step is set aside");
        let json = serde_json::to_string(&trace).expect("serialize");
        assert!(json.contains("\"generation\""), "{json}");
        let back: Trace = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, trace);
    }

    #[test]
    fn a_subject_group_round_trips_through_serde() {
        use super::SubjectGroup;
        use crate::ids::EntityUri;
        let group = SubjectGroup {
            key: "portfolio/0123456789ab".to_owned(),
            members: vec![
                EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
                EntityUri("https://ekg.example/id/mrp.production/1".to_owned()),
            ],
            links: vec!["linked: MO/1 fulfils SO/1".to_owned()],
            worded: false,
        };
        let json = serde_json::to_string(&group).expect("serialize");
        assert!(
            json.contains("\"key\":\"portfolio/0123456789ab\""),
            "{json}"
        );
        let back: SubjectGroup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, group);
    }

    #[test]
    fn trace_tags_serialize_snake_case() {
        let trace = Trace::from_steps(vec![
            step(TraceTag::Simulated, true),
            step(TraceTag::Graph, false),
            step(TraceTag::Assumption, false),
        ]);
        let json = serde_json::to_string(&trace).expect("serialize");
        assert!(json.contains("\"simulated\""));
        assert!(json.contains("\"graph\""));
        assert!(json.contains("\"assumption\""));
        assert!(json.contains("\"rejected\":true"));
        assert!(json.contains("\"open_assumptions\":1"));
        let back: Trace = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, trace);
    }
}
