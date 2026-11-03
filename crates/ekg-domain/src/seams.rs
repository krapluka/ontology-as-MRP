//! The async trait seams every external effect flows through.
//!
//! Each seam has a real driver in a layer crate and an in-memory fake (see
//! [`crate::fakes`]), so the whole governed loop runs hermetically in tests.
//! `async_trait` keeps them dyn-safe for runtime wiring.
//!
//! No method here names a use case. Risk detection is a capability rule (a SPARQL
//! query the monitor runs through [`WorldModel::select`]); explanation is the bounded
//! agent's job, built on [`CognitionProvider::generate`].

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::action_point::{ExecutionPlan, ExecutionReceipt, PromotedAction, PromotedPlan};
use crate::error::{EngineError, Result};
use crate::events::RdfChanged;
use crate::grounding::{Grounding, ReadContext};
use crate::ids::{EntityUri, Revision};
use crate::proposal::{GroundedProposal, Plan};
use crate::slate::{Score, ScoredCandidate, Slate, SubjectGroup};
use crate::source::{SourceRecord, SourceRef};

/// Pulls authoritative records from a source system of record (fat-pull).
///
/// Real driver: Odoo JSON-RPC. Fake: fixture replay.
#[async_trait]
pub trait SourceIngestor: Send + Sync {
    /// Fat-pull the authoritative record for a reference.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if the source has no such record;
    /// [`crate::EngineError::Backend`] on transport failure.
    async fn pull(&self, source_ref: &SourceRef) -> Result<SourceRecord>;

    /// List references changed since a watermark (reconciliation sweep).
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] on transport failure.
    async fn changed_since(&self, watermark: &str) -> Result<Vec<SourceRef>>;
}

/// The governed RDF world model: materialize, retract, ground, and query.
///
/// Real driver: GraphDB via SPARQL/SHACL. Fake: in-memory map.
#[async_trait]
pub trait WorldModel: Send + Sync {
    /// Materialize a source record into its named graph with provenance,
    /// returning the entity's new revision.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the store rejects the write.
    async fn materialize(&self, record: &SourceRecord) -> Result<Revision>;

    /// Retract an entity's graph (its source record no longer exists).
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if the entity is unknown.
    async fn retract(&self, entity: &EntityUri) -> Result<Revision>;

    /// Build a grounding (evidence subgraph + recorded omissions) for an entity,
    /// filtered by the caller's read context.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if the entity is unknown.
    async fn ground(&self, entity: &EntityUri, ctx: &ReadContext) -> Result<Grounding>;

    /// Run a SPARQL SELECT and return solution rows, each a map from variable name
    /// to term string. A capability runs its own rules through this without the seam
    /// knowing about them.
    ///
    /// This is the raw query path for the deterministic monitor, which sees the
    /// complete world model. It is not read-policy filtered: anything an agent or
    /// user sees must go through [`WorldModel::ground`], which enforces policy and
    /// records omissions.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the query fails.
    async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>>;
}

/// Publishes `RdfChanged` events onto the fan-out backbone.
///
/// Real driver: NATS JetStream. Fake: in-process capture.
#[async_trait]
pub trait RdfChangePublisher: Send + Sync {
    /// Publish one change event on `ekg.rdf.v1.<model>.<kind>`.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] on transport failure.
    async fn publish(&self, event: &RdfChanged) -> Result<()>;
}

/// The provider-neutral cognition boundary (generate and embed).
///
/// Real driver: Hayhooks behind the OpenAI-compatible endpoint. It takes a prompt or
/// text and returns text or vectors and never sees domain types, so the model stays
/// out of the truth path and any provider is swappable. Higher-level behaviour (e.g.
/// explaining a grounding) is built on top in `ekg-cognition`.
#[async_trait]
pub trait CognitionProvider: Send + Sync {
    /// Generate text from a prompt.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the provider is unavailable.
    async fn generate(&self, prompt: &str) -> Result<String>;

    /// Embed text for the (non-authoritative) vector projection.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the provider is unavailable.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

/// Commits governed actions to a system of record, and reverses them.
///
/// Real driver: Odoo JSON-RPC write-back. It accepts a [`PromotedAction`], never a
/// bare proposal, so a write-back cannot be expressed without a human promotion.
/// The concrete verbs are defined by the kinetic layer.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Execute a promoted governed action (the write-back), returning a receipt.
    /// The receipt records the source revision written against, for a later
    /// conditional reverse.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the write-back fails.
    async fn execute(&self, promoted: &PromotedAction) -> Result<ExecutionReceipt>;

    /// Apply the inverse of a previously executed action. Implementations must
    /// reverse conditionally: if the source has advanced past
    /// [`ExecutionReceipt::source_revision`], refuse rather than clobber the
    /// newer change.
    ///
    /// # Errors
    /// [`crate::EngineError::NotReversible`] if the receipt is not reversible;
    /// [`crate::EngineError::ReverseConflict`] if the source advanced past the
    /// executed revision; [`crate::EngineError::Backend`] if the write-back fails.
    async fn reverse(&self, receipt: &ExecutionReceipt) -> Result<()>;

    /// Execute a promoted plan: apply each action in listed order through
    /// [`ActionExecutor::execute`], using the plan's shared promotion and basis
    /// revision. All-or-nothing: when action `i` fails, the already-committed actions
    /// `i-1` down to `0` are reversed in that order and the original failure is
    /// returned.
    ///
    /// This default is built only on `execute` and `reverse`, so executors do not
    /// reimplement it.
    ///
    /// # Errors
    /// The original per-action error on a mid-plan failure whose rollback completed;
    /// [`crate::EngineError::PlanPartiallyApplied`] if a rollback reverse was itself
    /// refused because the source advanced mid-plan.
    async fn execute_plan(&self, promoted: &PromotedPlan) -> Result<ExecutionPlan> {
        let promoted_actions = promoted.promoted_actions();
        let mut receipts: Vec<ExecutionReceipt> = Vec::with_capacity(promoted_actions.len());
        for promoted_action in &promoted_actions {
            match self.execute(promoted_action).await {
                Ok(receipt) => receipts.push(receipt),
                Err(failure) => {
                    // Roll the already-committed actions back in reverse order. A
                    // refused reverse means a concurrent edit advanced the source past
                    // the watermark just written, so the plan is partially applied.
                    let mut unreversed: Vec<String> = Vec::new();
                    for receipt in receipts.iter().rev() {
                        if let Err(rollback_err) = self.reverse(receipt).await {
                            unreversed.push(format!(
                                "{} ({rollback_err})",
                                describe_action(&receipt.action)
                            ));
                        }
                    }
                    if unreversed.is_empty() {
                        return Err(failure);
                    }
                    return Err(EngineError::PlanPartiallyApplied {
                        failure: failure.to_string(),
                        unreversed: unreversed.len(),
                        details: unreversed.join("; "),
                    });
                }
            }
        }
        Ok(ExecutionPlan {
            actions: promoted.plan.actions.clone(),
            receipts,
        })
    }

    /// Reverse an executed plan: undo its receipts in reverse order, each under the
    /// per-action source-watermark guard. Best-effort per action: an action whose
    /// source changed after its write is refused rather than clobbered, the others
    /// are still reversed, and the refusals are reported together.
    ///
    /// # Errors
    /// [`crate::EngineError::PlanReverseIncomplete`] if one or more actions could not
    /// be reversed (each refused by its watermark guard); the remaining actions are
    /// still reversed.
    async fn reverse_plan(&self, executed: &ExecutionPlan) -> Result<()> {
        let outcome = self
            .reverse_plan_remaining(executed, &BTreeSet::new())
            .await?;
        if outcome.refused.is_empty() {
            return Ok(());
        }
        Err(EngineError::PlanReverseIncomplete {
            total: executed.receipts.len(),
            unreversed: outcome.refused.len(),
            details: outcome
                .refused
                .iter()
                .map(|(_, reason)| reason.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        })
    }

    /// Reverse the receipts of an executed plan that are not in `already` (the indices
    /// an earlier, partial reverse undid), in reverse order, each under its
    /// source-watermark guard. A refusal does not stop the others: the outcome names
    /// every receipt reversed and every one refused, with why, so a caller can record
    /// the progress and a later retry resumes from it.
    ///
    /// # Errors
    /// [`crate::EngineError::Invariant`] if `already` names a receipt the plan does not
    /// have (progress recorded over another plan). A refused receipt is not an error:
    /// it is reported in [`ReverseOutcome::refused`].
    async fn reverse_plan_remaining(
        &self,
        executed: &ExecutionPlan,
        already: &BTreeSet<usize>,
    ) -> Result<ReverseOutcome> {
        let total = executed.receipts.len();
        if let Some(stray) = already.iter().find(|&&index| index >= total) {
            return Err(EngineError::Invariant(format!(
                "reverse progress names receipt {stray}, but the plan has {total}"
            )));
        }
        let mut outcome = ReverseOutcome::default();
        for (index, receipt) in executed.receipts.iter().enumerate().rev() {
            if already.contains(&index) {
                continue;
            }
            match self.reverse(receipt).await {
                Ok(()) => outcome.reversed.push(index),
                Err(err) => outcome.refused.push((
                    index,
                    format!("{} ({err})", describe_action(&receipt.action)),
                )),
            }
        }
        Ok(outcome)
    }
}

/// What one [`ActionExecutor::reverse_plan_remaining`] call did, by receipt index into the
/// executed plan: the receipts it reversed and the ones it could not, each with why. Both
/// lists are in the order they were attempted (reverse plan order).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseOutcome {
    /// The receipts reversed by this call.
    pub reversed: Vec<usize>,
    /// The receipts refused by this call, each with the action and the refusal.
    pub refused: Vec<(usize, String)>,
}

/// Describe an action for a plan-level error message: its verb and target, never
/// its parameters (which may carry sensitive values).
fn describe_action(action: &crate::proposal::ProposedAction) -> String {
    format!("{} on {}", action.action_type.0, action.target)
}

/// Produces a grounded proposal for an action point's subject: the propose stage of
/// the loop. Real impl: the bounded agent (`ekg-cognition`), which grounds via
/// [`WorldModel::ground`] and generates via [`CognitionProvider::generate`]. Fake: a
/// canned proposal.
///
/// The proposer never writes to the world model or a system of record. It returns
/// the grounding's omissions alongside the proposal so disclosure can be enforced
/// independently of the proposal's own field.
#[async_trait]
pub trait Proposer: Send + Sync {
    /// Ground `subject` and produce a candidate proposal.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if the subject is unknown;
    /// [`crate::EngineError::Backend`] if grounding or generation fails;
    /// [`crate::EngineError::InvalidProposal`] if a well-formed proposal cannot be
    /// produced from the model response.
    async fn propose(&self, subject: &EntityUri) -> Result<GroundedProposal>;
}

/// Generates a candidate proposal for the revise stage of the slate loop. Like
/// [`Proposer`], but informed by the candidates already tried and the opaque scores
/// they earned, so it can steer toward a distinct, better-ranked candidate.
///
/// With empty feedback this is the initial propose. The real impl (the bounded
/// agent) folds the feedback into its prompt. It only reads and proposes.
#[async_trait]
pub trait CandidateProposer: Send + Sync {
    /// Ground `subject` and produce a candidate, revising away from `feedback` (the
    /// plans already tried and their opaque scores). Empty feedback means the first
    /// propose.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if the subject is unknown;
    /// [`crate::EngineError::Backend`] if grounding or generation fails;
    /// [`crate::EngineError::InvalidProposal`] if a well-formed proposal cannot be
    /// produced from the model response.
    async fn propose_candidate(
        &self,
        subject: &EntityUri,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal>;
}

/// Projects a hypothetical [`Plan`] onto a non-authoritative what-if read model of
/// the world and hands back a queryable handle. This is the simulate stage of the
/// loop.
///
/// `project` takes the opaque plan and returns a [`Projection`]. The rule that reads
/// the outcome from the projection is the capability's own (the same SPARQL its
/// monitor runs via [`WorldModel::select`]), so the seam knows nothing of delivery,
/// cost or risk.
///
/// A projection commits nothing and requires no promotion. Implementations must not
/// publish an [`RdfChanged`] event, must not mutate any authoritative named graph or
/// system of record, and must discard the projected state after use (real driver: an
/// ephemeral GraphDB sim graph, dropped on every path).
#[async_trait]
pub trait Simulator: Send + Sync {
    /// Project `plan` onto a fresh non-authoritative read model and return a handle
    /// the caller queries with its own rule.
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the projection cannot be built.
    async fn project(&self, plan: &Plan) -> Result<Box<dyn Projection>>;
}

/// Scores a hypothetical by running the capability's own rules over a
/// [`Projection`]. This is the observe stage of the slate loop.
///
/// The seam names no metric. A scorer runs the capability's queries via
/// [`Projection::select`] (e.g. whether the drift signal cleared, a projected
/// completion date, a projected cost) to fill an opaque [`Score`]. The concrete
/// scorers live in `ekg-semantic`.
///
/// Scoring only reads a projection. A scorer must not mutate any authoritative state
/// or a system of record.
#[async_trait]
pub trait Scorer: Send + Sync {
    /// Score one projected hypothetical, returning an opaque [`Score`].
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if a capability query against the projection
    /// fails.
    async fn score(&self, projection: &dyn Projection) -> Result<Score>;

    /// Score `projection` knowing the [`Plan`] it was projected from, so the capability's
    /// rank may read what the plan itself carries (not only the world it leaves). The
    /// default ignores the plan and calls [`Scorer::score`].
    ///
    /// # Errors
    /// As [`Scorer::score`].
    async fn score_plan(&self, _plan: &Plan, projection: &dyn Projection) -> Result<Score> {
        self.score(projection).await
    }

    /// The capability's operator wording of a [`Score`] this scorer produced: the text a
    /// simulated reasoning-trace step carries. `None` (the default) when the capability
    /// words no score; the caller then keeps its neutral listing.
    fn describe(&self, _score: &Score) -> Option<String> {
        None
    }
}

/// Produces a ranked remediation [`Slate`] for an action point's subject: the whole
/// propose, simulate, observe, revise loop behind one seam.
///
/// Real impl (`ekg-cognition`'s `SlateAgent`): generate candidate [`Plan`]s with a
/// bounded fan-out, simulate each ([`Simulator::project`]), observe each (an injected
/// [`Scorer`]), rank by [`Score`] `rank_key`, flag one `recommended`, and score the
/// empty plan as the do-nothing baseline. A fake lives in [`crate::fakes`].
///
/// Producing a slate commits nothing: every candidate is a proposal, and a separate
/// human promotion of one chosen plan remains the only path to a governed
/// write-back. The loop is a Restate durable orchestration; the model stays a
/// read-only step.
#[async_trait]
pub trait SlateProposer: Send + Sync {
    /// Produce a ranked slate for `subject`, committing nothing.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if the subject is unknown;
    /// [`crate::EngineError::Backend`] if grounding, generation, projection, or
    /// scoring fails.
    async fn propose_slate(&self, subject: &EntityUri) -> Result<Slate>;
}

/// Generates one candidate plan for a group of coupled subjects: the propose and
/// revise stage of a group slate. The group analogue of [`CandidateProposer`]: it
/// grounds every member (each under its own read context), asks for one plan that may
/// act across the members, and revises away from `feedback`.
///
/// The group's key is opaque and the seam names no coupling rule. It only reads and
/// proposes. The returned proposal's `subject_members` is the union of what every
/// member's grounding references, so each action targets an entity some member saw.
/// references, so each action targets an entity some member saw.
#[async_trait]
pub trait GroupCandidateProposer: Send + Sync {
    /// Ground every member of `group` and produce one candidate plan, revising away
    /// from `feedback`. Empty feedback means the first propose.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if a member is unknown;
    /// [`crate::EngineError::Backend`] if grounding or generation fails;
    /// [`crate::EngineError::InvalidProposal`] if a well-formed plan cannot be produced
    /// from the model response.
    async fn propose_group_candidate(
        &self,
        group: &SubjectGroup,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal>;

    /// One round of at most `room` candidates, each with its own outcome: a failed one does not
    /// fail the others. One candidate unless a proposer has several ways to draft.
    async fn propose_group_round(
        &self,
        group: &SubjectGroup,
        feedback: &[ScoredCandidate],
        room: usize,
    ) -> Vec<Result<GroundedProposal>> {
        let _ = room;
        vec![self.propose_group_candidate(group, feedback).await]
    }
}

/// Scores one projected hypothetical for a group: the observe stage of a group
/// slate. The group analogue of [`Scorer`]: it receives the group so the rank can
/// weigh how the plan leaves each member, not only the world as a whole.
///
/// The returned [`Score`] is opaque. Scoring only reads the projection.
#[async_trait]
pub trait GroupScorer: Send + Sync {
    /// Score `projection` for `group`, returning an opaque [`Score`].
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if a query against the projection fails.
    async fn score_group(&self, projection: &dyn Projection, group: &SubjectGroup)
    -> Result<Score>;

    /// Score `projection` for `group` knowing the [`Plan`] it was projected from. The
    /// group analogue of [`Scorer::score_plan`]. The default ignores the plan and calls
    /// [`GroupScorer::score_group`].
    ///
    /// # Errors
    /// As [`GroupScorer::score_group`].
    async fn score_group_plan(
        &self,
        _plan: &Plan,
        projection: &dyn Projection,
        group: &SubjectGroup,
    ) -> Result<Score> {
        self.score_group(projection, group).await
    }

    /// The capability's operator wording of a [`Score`] this scorer produced. The group
    /// analogue of [`Scorer::describe`]. `None` (the default) when the capability words
    /// no score.
    fn describe(&self, _score: &Score) -> Option<String> {
        None
    }
}

/// Produces one ranked remediation [`Slate`] for a group of coupled subjects: the
/// whole propose, simulate, observe, revise loop over the group behind one seam.
///
/// Real impl (`ekg-cognition`'s `GroupSlateAgent`) shares the point slate's loop,
/// admission rule and trace. Producing a slate commits nothing: every option is a
/// proposal, and a separate human promotion remains the only path to a governed
/// write-back.
#[async_trait]
pub trait GroupSlateProposer: Send + Sync {
    /// Produce a ranked slate for `group`, committing nothing.
    ///
    /// # Errors
    /// [`crate::EngineError::NotFound`] if a member is unknown;
    /// [`crate::EngineError::Backend`] if grounding, generation, projection, or
    /// scoring fails.
    async fn propose_group_slate(&self, group: &SubjectGroup) -> Result<Slate>;
}

/// A queryable handle over one hypothetical projection (see [`Simulator::project`]).
///
/// Its `select` mirrors [`WorldModel::select`], so a capability re-runs its own
/// unchanged rule against the projected state: an empty result means the drift signal
/// that rule detects has cleared under the plan. The handle owns the ephemeral
/// projection; dropping it (or the driver's own cleanup) discards the projected state.
#[async_trait]
pub trait Projection: Send + Sync {
    /// Run a SPARQL SELECT against the projected (hypothetical) state, returning
    /// solution rows in the same shape as [`WorldModel::select`].
    ///
    /// # Errors
    /// [`crate::EngineError::Backend`] if the query fails.
    async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>>;
}
