//! The slate agent: the propose, simulate, observe, revise loop that turns an action
//! point into a ranked remediation slate.
//!
//! It composes three neutral seams and names no use case:
//!
//! - a [`CandidateProposer`] (the bounded agent) generates candidate plans;
//! - a [`Simulator`] projects each candidate onto a non-authoritative what-if;
//! - an injected [`Scorer`] runs the capability's own rule over each projection to
//!   fill an opaque [`Score`].
//!
//! Each iteration feeds the candidates already tried and their scores
//! ([`ScoredCandidate`]) back into the proposer, so it revises toward a distinct,
//! better-ranked candidate instead of re-sampling the same one. The empty plan is scored
//! as the do-nothing baseline and only candidates that beat it are admitted, ranked
//! best-first by `rank_key`. An admitted option's `rank_key` is the capability's key plus
//! a digest of the plan's actions, so it is unique within the slate and identifies the
//! option. Each option carries its candidate's grounding omissions.
//!
//! Generation is bounded by `max_candidates` and by a time budget. A failed attempt is
//! traced as a rejected Generation step with its failure class (`rate_limited`,
//! `unavailable`, `refused`) and the loop moves on. When the budget is spent the loop
//! returns the baseline plus whatever it already admitted.
//!
//! Producing a slate commits nothing: every candidate is a proposal, and a human
//! promoting one chosen plan is the only path to a governed write-back.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ekg_domain::{
    CandidateProposer, EngineError, EntityUri, EvidenceEdge, GroundedProposal,
    GroupCandidateProposer, GroupScorer, GroupSlateProposer, Omission, OmissionKind, Plan,
    RankedPlan, Result, Score, ScoredCandidate, Scorer, Simulator, Slate, SlateProposer,
    SubjectGroup, Trace, TraceStep, TraceTag,
};

/// The default bound on candidate generation when none is given: enough for the
/// demo's reschedule/reassign fan-out.
pub const DEFAULT_MAX_CANDIDATES: usize = 4;

/// The default time budget for generating a slate's candidates: room for a burst of
/// fresh points to queue for the model behind the process-wide call limit.
pub const DEFAULT_SLATE_BUDGET: Duration = Duration::from_secs(150);

/// Cap on how many per-edge Graph steps a single candidate contributes, so a large
/// grounding cannot bloat the trace; any surplus is summarized in one trailing step.
const MAX_GRAPH_STEPS_PER_CANDIDATE: usize = 6;

/// Cap on how many characters of a failure reason a Generation step carries, so a
/// verbose endpoint body cannot bloat the trace.
const MAX_REASON_CHARS: usize = 200;

/// Classify a failed generation attempt into its failure class and reason: a load
/// refusal is `rate_limited`; a refused answer (unparseable, abstained, or refused by
/// the governed catalog) is `refused`; every other failure of the call (a timeout, a
/// transport or server error) is `unavailable`.
fn failure_class(err: &EngineError) -> (&'static str, String) {
    match err {
        EngineError::RateLimited(why) => ("rate_limited", why.clone()),
        EngineError::InvalidProposal(why) => ("refused", why.clone()),
        EngineError::Unavailable(why) | EngineError::Backend(why) => ("unavailable", why.clone()),
        other => ("unavailable", other.to_string()),
    }
}

/// Truncate a failure reason to [`MAX_REASON_CHARS`] on a character boundary.
fn truncate_reason(reason: &str) -> String {
    let mut chars = reason.chars();
    let head: String = chars.by_ref().take(MAX_REASON_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Trace (and log) a generation attempt that yielded no candidate as a rejected
/// Generation step: `attempt {n}: {class} — {reason}`. Nothing was projected, so it is
/// not a Simulated step. It is kept like a rejected candidate so oversight sees why a
/// slate carries fewer options than attempts.
fn generation_step(label: &str, attempt: usize, class: &str, reason: &str) -> TraceStep {
    let reason = truncate_reason(reason);
    eprintln!("warning: slate {label}: attempt {attempt} {class}: {reason}");
    TraceStep {
        tag: TraceTag::Generation,
        text: format!("attempt {attempt}: {class} — {reason}"),
        rejected: true,
    }
}

/// Render an opaque [`Score`] into a human-readable trace line by joining its metric
/// strings. The loop names no metric itself.
fn render_score(score: &Score) -> String {
    if score.metrics.is_empty() {
        return format!("rank {}", score.rank_key);
    }
    score
        .metrics
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A Simulated step for `score`: the capability's wording after `worded_head` ("Do
/// nothing", "Candidate") when its scorer words one, else the [`render_score`] listing
/// after `listing_head`.
fn simulated_step(
    seams: &dyn LoopSeams,
    (worded_head, listing_head): (&str, &str),
    score: &Score,
    rejected: bool,
) -> TraceStep {
    let text = match seams.describe(score) {
        Some(wording) => format!("{worded_head}: {wording}"),
        None => format!("{listing_head}: {}", render_score(score)),
    };
    TraceStep {
        tag: TraceTag::Simulated,
        text,
        rejected,
    }
}

/// The heads of the baseline's Simulated step (worded, listing).
const BASELINE_HEADS: (&str, &str) = ("Do nothing", "do-nothing baseline");
/// The heads of a candidate's Simulated step (worded, listing).
const CANDIDATE_HEADS: (&str, &str) = ("Candidate", "simulated candidate");

/// Build the Graph steps for a candidate from the evidence edges it grounded on. Bounded
/// by [`MAX_GRAPH_STEPS_PER_CANDIDATE`]; a surplus becomes one summary step.
fn graph_steps(edges: &[EvidenceEdge]) -> Vec<TraceStep> {
    let mut steps: Vec<TraceStep> = edges
        .iter()
        .take(MAX_GRAPH_STEPS_PER_CANDIDATE)
        .map(|edge| TraceStep {
            tag: TraceTag::Graph,
            text: format!("read {} {} {}", edge.subject.0, edge.predicate, edge.object),
            rejected: false,
        })
        .collect();
    if edges.len() > MAX_GRAPH_STEPS_PER_CANDIDATE {
        steps.push(TraceStep {
            tag: TraceTag::Graph,
            text: format!(
                "and {} further evidence edge(s)",
                edges.len() - MAX_GRAPH_STEPS_PER_CANDIDATE
            ),
            rejected: false,
        });
    }
    steps
}

/// Build the Assumption steps for a candidate from the omissions its grounding
/// recorded: each hidden node/edge becomes an open-assumption step, rendered from the
/// structured [`Omission`], never parsed from model prose.
fn assumption_steps(omissions: &[Omission]) -> Vec<TraceStep> {
    omissions
        .iter()
        .map(|omission| {
            let kind = match omission.kind {
                OmissionKind::Node => "node",
                OmissionKind::Edge => "edge",
            };
            TraceStep {
                tag: TraceTag::Assumption,
                text: format!(
                    "assumed away {} hidden {kind}(s) of type {} (read-policy {})",
                    omission.count, omission.type_iri, omission.policy_id.0
                ),
                rejected: false,
            }
        })
        .collect()
}

/// A stable digest of a plan's actions: [`ekg_domain::short_digest`] (12 lowercase hex
/// characters of FNV-1a 64) over the plan's canonical form, which is each action's verb,
/// target and params (params ordered, as the map keeps them) serialized as JSON, in plan
/// order.
fn plan_digest(plan: &Plan) -> String {
    // Serializing owned strings and a BTreeMap cannot fail; an empty form would still
    // hash deterministically.
    let canonical = serde_json::to_vec(&plan.actions).unwrap_or_default();
    ekg_domain::short_digest(&canonical)
}

/// The unique `rank_key` of an admitted option: the capability's key, then `~` and the
/// plan digest. `~` sorts after every digit and `:`, so options still order best-first by
/// the capability's key, and equal scores are ordered deterministically by the digest.
fn option_rank_key(capability_key: &str, digest: &str) -> String {
    format!("{capability_key}~{digest}")
}

/// The rejected Simulated step recording that a candidate duplicates an option already
/// admitted.
fn duplicate_step(plan: &Plan) -> TraceStep {
    TraceStep {
        tag: TraceTag::Simulated,
        text: format!(
            "simulated candidate {} set aside: duplicate of an admitted option",
            describe_plan(plan)
        ),
        rejected: true,
    }
}

/// Name a plan's actions for a trace line: each verb and target, as carried.
fn describe_plan(plan: &Plan) -> String {
    plan.actions
        .iter()
        .map(|action| format!("{} {}", action.action_type.0, action.target.0))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// The slate agent over its three seams. It names no capability.
#[derive(Clone)]
pub struct SlateAgent {
    proposer: Arc<dyn CandidateProposer>,
    simulator: Arc<dyn Simulator>,
    scorer: Arc<dyn Scorer>,
    max_candidates: usize,
    budget: Duration,
}

impl SlateAgent {
    /// Wire the agent over the propose / simulate / observe seams with the default
    /// candidate bound.
    #[must_use]
    pub fn new(
        proposer: Arc<dyn CandidateProposer>,
        simulator: Arc<dyn Simulator>,
        scorer: Arc<dyn Scorer>,
    ) -> Self {
        Self::with_max_candidates(proposer, simulator, scorer, DEFAULT_MAX_CANDIDATES)
    }

    /// Wire the agent with an explicit bound on how many candidates it will generate.
    /// A bound of 0 is raised to 1.
    #[must_use]
    pub fn with_max_candidates(
        proposer: Arc<dyn CandidateProposer>,
        simulator: Arc<dyn Simulator>,
        scorer: Arc<dyn Scorer>,
        max_candidates: usize,
    ) -> Self {
        Self {
            proposer,
            simulator,
            scorer,
            max_candidates: max_candidates.max(1),
            budget: DEFAULT_SLATE_BUDGET,
        }
    }

    /// Bound candidate generation by `budget` (default [`DEFAULT_SLATE_BUDGET`]): no new
    /// attempt starts once it is spent. A generation already in flight may finish; the
    /// cognition boundary's own timeouts bound it.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Simulate `plan` and score the projection with [`Scorer::score_plan`], which also
    /// sees the plan.
    async fn score_plan(&self, plan: &Plan) -> Result<Score> {
        let projection = self.simulator.project(plan).await?;
        self.scorer.score_plan(plan, projection.as_ref()).await
    }
}

/// The seams one run of the slate loop drives: generate a candidate from the revise
/// feedback, and score a plan. A point slate binds them to one subject, a group slate
/// to a group.
#[async_trait]
trait LoopSeams: Send + Sync {
    /// Generate the next candidate, revising away from `feedback`.
    async fn propose(&self, feedback: &[ScoredCandidate]) -> Result<GroundedProposal>;
    /// Project `plan` and score the projection.
    async fn score(&self, plan: &Plan) -> Result<Score>;
    /// The capability's wording of a score these seams produced, if it words one.
    fn describe(&self, score: &Score) -> Option<String>;

    /// One round of at most `room` candidates: one, unless the seams can draft several at once.
    async fn propose_round(
        &self,
        feedback: &[ScoredCandidate],
        room: usize,
    ) -> Vec<Result<GroundedProposal>> {
        let _ = room;
        vec![self.propose(feedback).await]
    }
}

/// The propose, simulate, observe, revise loop shared by the point and group slates.
/// `opening` steps open the slate-level trace ahead of the baseline (none for a point
/// slate); `label` names the run in logs.
#[allow(clippy::too_many_lines)] // the one loop: each outcome of a round handled in order
async fn run_loop(
    seams: &dyn LoopSeams,
    opening: Vec<TraceStep>,
    label: &str,
    max_candidates: usize,
    budget: Duration,
) -> Result<Slate> {
    let started = tokio::time::Instant::now();
    // The do-nothing baseline: score the empty plan. Without it there is nothing to rank
    // against, so a failure here fails the slate.
    let baseline = seams
        .score(&Plan {
            actions: Vec::new(),
        })
        .await?;

    // The caller's opening steps (a group's Linked step) come first, then the baseline as
    // a Simulated step.
    let mut slate_steps = opening;
    slate_steps.push(simulated_step(seams, BASELINE_HEADS, &baseline, false));

    let mut options: Vec<RankedPlan> = Vec::new();
    // The revise feedback: every scored attempt so far (plan + opaque score).
    let mut tried: Vec<ScoredCandidate> = Vec::new();
    // The candidates that could not be projected or scored (traced as rejected).
    let mut unscorable: Vec<Plan> = Vec::new();
    // The plan digest of every admitted option, so a duplicate plan is set aside.
    let mut admitted: Vec<String> = Vec::new();
    let mut attempt = 0;
    'rounds: while attempt < max_candidates {
        // Once the time budget is spent, stop generating and keep what was admitted,
        // saying so in the trace.
        if started.elapsed() >= budget {
            let reason =
                format!("the {budget:?} time budget ended generation after {attempt} attempt(s)");
            slate_steps.push(generation_step(label, attempt + 1, "budget", &reason));
            break;
        }
        let round = seams.propose_round(&tried, max_candidates - attempt).await;
        for outcome in round {
            attempt += 1;
            // A failed generation uses up this attempt but does not end the loop. The revise
            // feedback is unchanged: it carries only scored candidates.
            let grounded = match outcome {
                Ok(grounded) => grounded,
                // Nothing a plan could reach: the slate ends without asking again.
                Err(EngineError::NothingToDo(why)) => {
                    slate_steps.push(TraceStep {
                        tag: TraceTag::Generation,
                        text: format!("declined: {why}"),
                        rejected: false,
                    });
                    break 'rounds;
                }
                Err(err) => {
                    let (class, reason) = failure_class(&err);
                    slate_steps.push(generation_step(label, attempt, class, &reason));
                    continue;
                }
            };
            let plan = Plan {
                actions: grounded.proposal.actions,
            };
            // A plan that duplicates an admitted option is traced as set aside.
            let digest = plan_digest(&plan);
            if admitted.contains(&digest) {
                slate_steps.push(duplicate_step(&plan));
                continue;
            }
            // Skip a repeated candidate (the feedback already carries it, or it was already
            // traced as unscorable). The attempt still counts.
            if tried.iter().any(|prior| prior.plan == plan) || unscorable.contains(&plan) {
                continue;
            }
            // A candidate that cannot be projected or scored (e.g. a malformed param the
            // simulator rejects) is set aside and traced as rejected with its reason. It does
            // not fail the slate.
            let score = match seams.score(&plan).await {
                Ok(score) => score,
                Err(err) => {
                    slate_steps.push(TraceStep {
                        tag: TraceTag::Simulated,
                        text: format!(
                            "candidate {} could not be scored: {err}",
                            describe_plan(&plan)
                        ),
                        rejected: true,
                    });
                    unscorable.push(plan);
                    continue;
                }
            };
            // Record this attempt for the next revision.
            tried.push(ScoredCandidate {
                plan: plan.clone(),
                score: score.clone(),
            });

            // A candidate is admitted only if it strictly beats doing nothing. One that scores
            // no better than the baseline (e.g. a no-op verb) is rejected but stays in the
            // trace, so oversight sees what was tried and set aside.
            let rejected = score.rank_key >= baseline.rank_key;

            // The candidate's reasoning steps: the graph facts it grounded on, the
            // assumptions its omissions force, then the simulated score. The per-candidate
            // trace and the slate-level trace share these steps.
            let mut candidate_steps = graph_steps(&grounded.grounding_edges);
            candidate_steps.extend(assumption_steps(&grounded.grounding_omissions));
            candidate_steps.push(simulated_step(seams, CANDIDATE_HEADS, &score, rejected));

            // Every candidate, admitted or rejected, goes into the slate-level trace in the
            // order it was tried.
            slate_steps.extend(candidate_steps.iter().cloned());

            if !rejected {
                // The option's key identifies it within the slate: the capability's key
                // (which decided admission above) plus the plan digest.
                let score = Score {
                    rank_key: option_rank_key(&score.rank_key, &digest),
                    ..score
                };
                admitted.push(digest);
                options.push(RankedPlan {
                    plan,
                    score,
                    recommended: false,
                    // Policy `needs[]` are not evaluated here.
                    needs: Vec::new(),
                    omissions: grounded.grounding_omissions,
                    trace: Trace::from_steps(candidate_steps),
                });
            }
        }
    }

    // Best first.
    options.sort_by(|a, b| a.score.rank_key.cmp(&b.score.rank_key));

    // The best-ranked option is the recommended one. There is none when the slate carries
    // only the baseline.
    if let Some(best) = options.first_mut() {
        best.recommended = true;
    }

    Ok(Slate {
        baseline,
        options,
        trace: Trace::from_steps(slate_steps),
    })
}

/// A point slate's loop seams: the candidate proposer and scorer over one subject.
struct PointSeams<'a> {
    agent: &'a SlateAgent,
    subject: &'a EntityUri,
}

#[async_trait]
impl LoopSeams for PointSeams<'_> {
    async fn propose(&self, feedback: &[ScoredCandidate]) -> Result<GroundedProposal> {
        self.agent
            .proposer
            .propose_candidate(self.subject, feedback)
            .await
    }

    async fn score(&self, plan: &Plan) -> Result<Score> {
        self.agent.score_plan(plan).await
    }

    fn describe(&self, score: &Score) -> Option<String> {
        self.agent.scorer.describe(score)
    }
}

#[async_trait]
impl SlateProposer for SlateAgent {
    async fn propose_slate(&self, subject: &EntityUri) -> Result<Slate> {
        let seams = PointSeams {
            agent: self,
            subject,
        };
        run_loop(
            &seams,
            Vec::new(),
            &subject.0,
            self.max_candidates,
            self.budget,
        )
        .await
    }
}

/// The group slate agent: the same loop as [`SlateAgent`] over a [`SubjectGroup`], giving
/// one slate whose plans may act across all the members. Its trace opens with a Graph
/// "Linked" step naming the members and why they are coupled.
#[derive(Clone)]
pub struct GroupSlateAgent {
    proposer: Arc<dyn GroupCandidateProposer>,
    simulator: Arc<dyn Simulator>,
    scorer: Arc<dyn GroupScorer>,
    max_candidates: usize,
    budget: Duration,
}

impl GroupSlateAgent {
    /// Wire the agent over the group propose / simulate / observe seams with the
    /// default candidate bound.
    #[must_use]
    pub fn new(
        proposer: Arc<dyn GroupCandidateProposer>,
        simulator: Arc<dyn Simulator>,
        scorer: Arc<dyn GroupScorer>,
    ) -> Self {
        Self::with_max_candidates(proposer, simulator, scorer, DEFAULT_MAX_CANDIDATES)
    }

    /// Wire the agent with an explicit bound on how many candidates it will generate.
    #[must_use]
    pub fn with_max_candidates(
        proposer: Arc<dyn GroupCandidateProposer>,
        simulator: Arc<dyn Simulator>,
        scorer: Arc<dyn GroupScorer>,
        max_candidates: usize,
    ) -> Self {
        Self {
            proposer,
            simulator,
            scorer,
            max_candidates: max_candidates.max(1),
            budget: DEFAULT_SLATE_BUDGET,
        }
    }

    /// Bound candidate generation by `budget` (default [`DEFAULT_SLATE_BUDGET`]).
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }
}

/// The opening Graph step of a group slate, taken as the group carries it: the
/// capability's worded sentences, which name every member themselves, or, for recorded
/// reasons, the members joined by " and ", then the reasons joined by "; ".
fn linked_step(group: &SubjectGroup) -> TraceStep {
    let members = group
        .members
        .iter()
        .map(|member| member.0.as_str())
        .collect::<Vec<_>>()
        .join(" and ");
    let text = if group.links.is_empty() {
        format!("Linked: {members}.")
    } else if group.worded {
        format!("Linked: {}", group.links.join(" "))
    } else {
        format!("Linked: {members}. {}", group.links.join("; "))
    };
    TraceStep {
        tag: TraceTag::Graph,
        text,
        rejected: false,
    }
}

/// A group slate's loop seams: the group candidate proposer and group scorer.
struct GroupSeams<'a> {
    agent: &'a GroupSlateAgent,
    group: &'a SubjectGroup,
}

#[async_trait]
impl LoopSeams for GroupSeams<'_> {
    async fn propose(&self, feedback: &[ScoredCandidate]) -> Result<GroundedProposal> {
        self.agent
            .proposer
            .propose_group_candidate(self.group, feedback)
            .await
    }

    async fn propose_round(
        &self,
        feedback: &[ScoredCandidate],
        room: usize,
    ) -> Vec<Result<GroundedProposal>> {
        self.agent
            .proposer
            .propose_group_round(self.group, feedback, room)
            .await
    }

    async fn score(&self, plan: &Plan) -> Result<Score> {
        let projection = self.agent.simulator.project(plan).await?;
        self.agent
            .scorer
            .score_group_plan(plan, projection.as_ref(), self.group)
            .await
    }

    fn describe(&self, score: &Score) -> Option<String> {
        self.agent.scorer.describe(score)
    }
}

#[async_trait]
impl GroupSlateProposer for GroupSlateAgent {
    async fn propose_group_slate(&self, group: &SubjectGroup) -> Result<Slate> {
        let seams = GroupSeams { agent: self, group };
        run_loop(
            &seams,
            vec![linked_step(group)],
            &group.key,
            self.max_candidates,
            self.budget,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use ekg_domain::fakes::InMemorySimulator;
    use ekg_domain::{
        ActionType, CandidateProposer, EntityUri, EvidenceEdge, GroundedProposal, GroupScorer,
        Omission, OmissionKind, PolicyId, Projection, Proposal, ProposedAction, Result, Revision,
        Score, ScoredCandidate, Scorer, Simulator, Slate, SourceRef, SubjectGroup, TraceTag,
    };

    use super::{SlateAgent, SlateProposer};

    /// Build a grounded candidate for `verb`: an action on the subject with a
    /// `signal=off` param (a clearing verb flips the seeded `signal=on`; a `noop`
    /// changes nothing). Carries one omission so the slate's disclosure invariant is
    /// exercised.
    fn candidate(subject: &EntityUri, verb: &str) -> GroundedProposal {
        let action = ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: subject.clone(),
            params: if verb == "noop" {
                BTreeMap::new()
            } else {
                BTreeMap::from([("signal".to_owned(), "off".to_owned())])
            },
        };
        GroundedProposal {
            proposal: Proposal {
                explanation: format!("[test] {verb}"),
                actions: vec![action],
                omissions: Vec::new(),
            },
            grounding_omissions: vec![Omission {
                kind: OmissionKind::Node,
                type_iri: "ekg:Supplier".to_owned(),
                count: 1,
                policy_id: PolicyId("demo".to_owned()),
            }],
            // One evidence edge so the loop emits a Graph trace step.
            grounding_edges: vec![EvidenceEdge {
                subject: subject.clone(),
                predicate: "ekg:signal".to_owned(),
                object: "on".to_owned(),
                source: SourceRef {
                    system: "odoo".to_owned(),
                    model: "mrp.production".to_owned(),
                    id: 1,
                },
                revision: Revision(1),
            }],
            subject_members: Vec::new(),
        }
    }

    /// A proposer that yields a scripted sequence of verbs, indexed by how many
    /// candidates have already been tried (`feedback.len()`), so a test also shows that
    /// the revise feedback reaches each call. Past the end it repeats the last verb
    /// (which the loop then dedups).
    struct ScriptedProposer {
        verbs: Vec<&'static str>,
    }

    impl ScriptedProposer {
        fn new(verbs: Vec<&'static str>) -> Self {
            Self { verbs }
        }
    }

    #[async_trait]
    impl CandidateProposer for ScriptedProposer {
        async fn propose_candidate(
            &self,
            subject: &EntityUri,
            feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let verb = *self
                .verbs
                .get(feedback.len())
                .unwrap_or(&self.verbs[self.verbs.len() - 1]);
            Ok(candidate(subject, verb))
        }
    }

    /// A proposer that always errors: an abstaining generator (no candidate).
    struct AbstainingProposer;
    #[async_trait]
    impl CandidateProposer for AbstainingProposer {
        async fn propose_candidate(
            &self,
            _subject: &EntityUri,
            _feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            Err(ekg_domain::EngineError::InvalidProposal(
                "abstain".to_owned(),
            ))
        }
    }

    /// A proposer that revises from the observed feedback: while nothing has been tried
    /// it proposes a `noop` (which will not clear the signal); once it sees that the
    /// tried candidate did not clear the signal (`flagged > 0`), it revises to a
    /// clearing `reschedule_work_order`.
    struct RevisingProposer;
    #[async_trait]
    impl CandidateProposer for RevisingProposer {
        async fn propose_candidate(
            &self,
            subject: &EntityUri,
            feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let last_cleared = feedback
                .last()
                .is_some_and(|c| c.score.metrics.get("flagged").map(String::as_str) == Some("0"));
            // Nothing tried yet, or the last attempt is still flagged.
            let verb = if feedback.is_empty() || !last_cleared {
                if feedback.is_empty() {
                    "noop"
                } else {
                    "reschedule_work_order"
                }
            } else {
                "reschedule_work_order"
            };
            Ok(candidate(subject, verb))
        }
    }

    /// The subject and its seeded `signal=on` base state.
    fn subject() -> EntityUri {
        EntityUri("https://ekg.example/id/mrp.production/1".to_owned())
    }

    fn flagged_sim() -> Arc<InMemorySimulator> {
        Arc::new(InMemorySimulator::default().with_entity(
            subject(),
            BTreeMap::from([("signal".to_owned(), "on".to_owned())]),
        ))
    }

    /// A scorer that flags a projected row whose `signal` is still `on`; fewer flags
    /// rank first (mirrors the domain `FakeScorer`).
    struct SignalScorer;
    #[async_trait]
    impl Scorer for SignalScorer {
        async fn score(&self, projection: &dyn Projection) -> Result<Score> {
            let rows = projection.select("<rule>").await?;
            let flagged = rows
                .iter()
                .filter(|r| r.get("signal").map(String::as_str) == Some("on"))
                .count();
            Ok(Score {
                metrics: BTreeMap::from([("flagged".to_owned(), flagged.to_string())]),
                rank_key: format!("{flagged:06}"),
            })
        }
    }

    /// [`SignalScorer`] with a capability wording of its score.
    struct DescribingScorer;
    #[async_trait]
    impl Scorer for DescribingScorer {
        async fn score(&self, projection: &dyn Projection) -> Result<Score> {
            SignalScorer.score(projection).await
        }
        fn describe(&self, score: &Score) -> Option<String> {
            score
                .metrics
                .get("flagged")
                .map(|n| format!("{n} still flagged"))
        }
    }

    /// The Simulated steps' texts of `slate`, with their rejected flag.
    fn simulated(slate: &Slate) -> Vec<(String, bool)> {
        slate
            .trace
            .steps
            .iter()
            .filter(|s| s.tag == TraceTag::Simulated)
            .map(|s| (s.text.clone(), s.rejected))
            .collect()
    }

    #[tokio::test]
    async fn a_describing_scorer_words_the_simulated_steps() {
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["reschedule_work_order", "noop"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(DescribingScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        let steps = simulated(&slate);
        assert_eq!(
            steps[..3],
            [
                ("Do nothing: 1 still flagged".to_owned(), false),
                ("Candidate: 0 still flagged".to_owned(), false),
                ("Candidate: 1 still flagged".to_owned(), true),
            ]
        );
        assert!(steps.iter().all(|(text, _)| !text.contains('=')));
        // A repeat of an admitted plan keeps its "set aside" text.
        assert!(
            steps[3..]
                .iter()
                .all(|(text, rejected)| *rejected && text.contains("set aside: duplicate"))
        );
        // The per-candidate trace carries the same worded step.
        assert!(
            slate.options[0]
                .trace
                .steps
                .iter()
                .any(|s| s.text == "Candidate: 0 still flagged")
        );
    }

    #[tokio::test]
    async fn a_scorer_that_words_nothing_keeps_the_listing() {
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["reschedule_work_order"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert_eq!(
            simulated(&slate)[..2],
            [
                ("do-nothing baseline: flagged=1".to_owned(), false),
                ("simulated candidate: flagged=0".to_owned(), false),
            ]
        );
    }

    #[tokio::test]
    async fn a_clearing_candidate_outranks_the_baseline_and_is_recommended() {
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["reschedule_work_order"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");

        // Baseline sees the signal (flagged=1); the candidate clears it (flagged=0).
        assert_eq!(
            slate.baseline.metrics.get("flagged").map(String::as_str),
            Some("1")
        );
        assert_eq!(slate.options.len(), 1);
        let best = &slate.options[0];
        assert_eq!(
            best.score.metrics.get("flagged").map(String::as_str),
            Some("0")
        );
        assert!(best.recommended, "the clearing candidate is recommended");
        assert!(best.score.rank_key < slate.baseline.rank_key);
        // The candidate discloses its grounding's omissions.
        assert!(!best.omissions.is_empty());
    }

    #[tokio::test]
    async fn options_are_ranked_best_first_with_exactly_one_recommended() {
        // Two distinct verbs, both clearing, give two options.
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec![
                "reschedule_work_order",
                "reassign_work_center",
            ])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert_eq!(slate.options.len(), 2);
        assert!(
            slate
                .options
                .windows(2)
                .all(|w| w[0].score.rank_key <= w[1].score.rank_key),
            "best-first"
        );
        assert_eq!(slate.options.iter().filter(|o| o.recommended).count(), 1);
        assert!(slate.options[0].recommended);
    }

    #[tokio::test]
    async fn a_noop_candidate_that_does_not_beat_doing_nothing_yields_baseline_only() {
        // The only candidate is a no-op, which does not beat the baseline, so the slate
        // carries just the baseline, with no recommended option.
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["noop"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert!(slate.options.is_empty(), "no candidate beats doing nothing");
        assert!(slate.recommended().is_none());
        // The baseline is still scored and present.
        assert_eq!(
            slate.baseline.metrics.get("flagged").map(String::as_str),
            Some("1")
        );
    }

    #[tokio::test]
    async fn an_abstaining_generator_yields_baseline_only_with_every_attempt_traced() {
        let agent = SlateAgent::new(
            Arc::new(AbstainingProposer),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert!(slate.options.is_empty());
        assert!(slate.recommended().is_none());
        // An abstention does not end generation: every attempt is spent, and each is
        // traced as a refused Generation step.
        let generation: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| s.tag == TraceTag::Generation)
            .collect();
        assert_eq!(generation.len(), super::DEFAULT_MAX_CANDIDATES);
        assert!(
            generation
                .iter()
                .all(|s| s.rejected && s.text.contains("refused — abstain"))
        );
    }

    /// One scripted outcome of a generation attempt.
    #[derive(Clone, Copy)]
    enum Attempt {
        Refused,
        RateLimited,
        Verb(&'static str),
    }

    /// A proposer that plays a scripted sequence of attempt outcomes by call count,
    /// repeating the last one past the end.
    struct FallibleProposer {
        script: Vec<Attempt>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FallibleProposer {
        fn new(script: Vec<Attempt>) -> Self {
            Self {
                script,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl CandidateProposer for FallibleProposer {
        async fn propose_candidate(
            &self,
            subject: &EntityUri,
            _feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match self.script[n.min(self.script.len() - 1)] {
                Attempt::Refused => Err(ekg_domain::EngineError::InvalidProposal(
                    "the response carried no parseable action".to_owned(),
                )),
                Attempt::RateLimited => Err(ekg_domain::EngineError::RateLimited(
                    "cognition endpoint 429 Too Many Requests".to_owned(),
                )),
                Attempt::Verb(verb) => Ok(candidate(subject, verb)),
            }
        }
    }

    #[tokio::test]
    async fn a_refused_attempt_does_not_end_the_slate() {
        // The first answer is refused; the second is a clearing candidate. The slate
        // carries that candidate, and the trace retains the refused attempt.
        let agent = SlateAgent::with_max_candidates(
            Arc::new(FallibleProposer::new(vec![
                Attempt::Refused,
                Attempt::Verb("reschedule_work_order"),
            ])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
            2,
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert_eq!(slate.options.len(), 1, "the second attempt is admitted");
        let generation: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| s.tag == TraceTag::Generation)
            .collect();
        assert_eq!(generation.len(), 1, "{:?}", slate.trace.steps);
        assert!(generation[0].rejected);
        assert_eq!(
            generation[0].text,
            "attempt 1: refused — the response carried no parseable action"
        );
    }

    #[tokio::test]
    async fn a_slate_whose_every_attempt_is_rate_limited_says_why() {
        let agent = SlateAgent::with_max_candidates(
            Arc::new(FallibleProposer::new(vec![Attempt::RateLimited])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
            3,
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert!(slate.options.is_empty(), "baseline only");
        let generation: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| s.tag == TraceTag::Generation)
            .collect();
        assert_eq!(generation.len(), 3);
        for (n, step) in generation.iter().enumerate() {
            assert!(step.rejected);
            assert!(
                step.text
                    .starts_with(&format!("attempt {}: rate_limited — ", n + 1)),
                "{}",
                step.text
            );
        }
        // Rejected counts the failed attempts; simulated counts only the baseline.
        assert_eq!(slate.trace.summary.rejected, 3);
        assert_eq!(slate.trace.summary.simulated, 1);
    }

    /// A proposer whose first answer is immediate and clearing, and whose second takes
    /// `slow` (a generation in flight when the budget runs out) and is a no-op.
    struct SlowSecondProposer {
        slow: std::time::Duration,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl CandidateProposer for SlowSecondProposer {
        async fn propose_candidate(
            &self,
            subject: &EntityUri,
            _feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 {
                return Ok(candidate(subject, "reschedule_work_order"));
            }
            tokio::time::sleep(self.slow).await;
            Ok(candidate(subject, "noop"))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_time_budget_keeps_what_was_admitted() {
        use std::time::Duration;
        // A 60 s budget; the second generation takes 90 s, so the budget runs out while
        // it is in flight. It may finish; no third attempt starts.
        let proposer = Arc::new(SlowSecondProposer {
            slow: Duration::from_secs(90),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = SlateAgent::new(
            Arc::clone(&proposer) as Arc<dyn CandidateProposer>,
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        )
        .with_budget(Duration::from_secs(60));
        let slate = agent.propose_slate(&subject()).await.expect("slate");

        assert_eq!(slate.options.len(), 1, "the admitted candidate is kept");
        assert!(slate.options[0].recommended);
        assert_eq!(
            proposer.calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "no attempt starts once the budget is spent"
        );
        let budget: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| s.tag == TraceTag::Generation)
            .collect();
        assert_eq!(budget.len(), 1, "{:?}", slate.trace.steps);
        assert!(budget[0].rejected);
        assert!(
            budget[0]
                .text
                .starts_with("attempt 3: budget — the 60s time budget ended generation"),
            "{}",
            budget[0].text
        );
    }

    #[test]
    fn failures_are_classified_and_their_reasons_truncated() {
        use ekg_domain::EngineError;
        let class = |err: EngineError| super::failure_class(&err).0;
        assert_eq!(
            class(EngineError::RateLimited(String::new())),
            "rate_limited"
        );
        assert_eq!(
            class(EngineError::Unavailable(String::new())),
            "unavailable"
        );
        assert_eq!(class(EngineError::Backend(String::new())), "unavailable");
        assert_eq!(
            class(EngineError::InvalidProposal(String::new())),
            "refused"
        );

        let long = "é".repeat(super::MAX_REASON_CHARS + 50);
        let short = super::truncate_reason(&long);
        assert_eq!(short.chars().count(), super::MAX_REASON_CHARS + 1);
        assert!(short.ends_with('…'));
        assert_eq!(super::truncate_reason("brief"), "brief");
    }

    #[tokio::test]
    async fn the_agent_revises_toward_a_clearing_candidate_after_observing_a_failed_one() {
        // The proposer first tries a no-op (won't clear). Only after seeing in the fed-back
        // score that it did not clear does it revise to a clearing plan. So a slate with a
        // clearing recommended option can only arise if the loop passes the prior score
        // back.
        let agent = SlateAgent::new(
            Arc::new(RevisingProposer),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        let best = slate
            .recommended()
            .expect("revision produced a recommended option");
        assert_eq!(
            best.score.metrics.get("flagged").map(String::as_str),
            Some("0"),
            "the revised candidate clears the signal"
        );
        assert!(best.score.rank_key < slate.baseline.rank_key);
        // The failed no-op never becomes an option (it does not beat the baseline).
        assert_eq!(slate.options.len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_candidate_is_retained_in_the_trace_not_dropped() {
        // A clearing verb (admitted) and a no-op (rejected) are both tried. The no-op is
        // absent from `options` but must appear in the slate trace as a rejected Simulated
        // step.
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["reschedule_work_order", "noop"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");

        // Only the clearing candidate is an option; the no-op is not.
        assert_eq!(slate.options.len(), 1);

        // The trace opens with the do-nothing baseline (a non-rejected Simulated step).
        assert_eq!(slate.trace.steps[0].tag, TraceTag::Simulated);
        assert!(!slate.trace.steps[0].rejected);

        // One rejected Simulated step is retained (the no-op).
        let rejected: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| s.rejected && s.tag == TraceTag::Simulated)
            .collect();
        assert_eq!(rejected.len(), 1, "the rejected candidate is retained");
        assert_eq!(slate.trace.summary.rejected, 1);

        // All three tag kinds are present (Graph from the evidence edge, Assumption from
        // the omission, Simulated from scoring).
        assert!(slate.trace.steps.iter().any(|s| s.tag == TraceTag::Graph));
        assert!(
            slate
                .trace
                .steps
                .iter()
                .any(|s| s.tag == TraceTag::Assumption)
        );
        assert!(slate.trace.summary.open_assumptions >= 1);
        assert!(slate.trace.summary.simulated >= 3); // baseline + 2 candidates
    }

    #[test]
    fn graph_steps_cap_summarizes_the_surplus_evidence_edges() {
        // A candidate grounded on more evidence edges than the cap emits the cap's worth of
        // per-edge Graph steps plus one summary step naming the surplus count.
        let edge = |n: i64| EvidenceEdge {
            subject: subject(),
            predicate: format!("ekg:p{n}"),
            object: n.to_string(),
            source: SourceRef {
                system: "odoo".to_owned(),
                model: "mrp.production".to_owned(),
                id: n,
            },
            revision: Revision(1),
        };
        let edges: Vec<EvidenceEdge> = (0..8).map(edge).collect();
        let steps = super::graph_steps(&edges);

        // Six per-edge steps + one surplus-summary step = seven.
        assert_eq!(steps.len(), super::MAX_GRAPH_STEPS_PER_CANDIDATE + 1);
        assert!(steps.iter().all(|s| s.tag == TraceTag::Graph));
        let summary = steps.last().expect("a summary step");
        assert_eq!(summary.tag, TraceTag::Graph);
        // The surplus (8 - 6 = 2) is named in the summary step.
        assert!(
            summary.text.contains('2'),
            "the summary names the surplus count: {}",
            summary.text
        );

        // At or below the cap, no summary step is added.
        let few = super::graph_steps(&edges[..3]);
        assert_eq!(few.len(), 3);
    }

    #[tokio::test]
    async fn each_admitted_option_carries_its_own_trace_slice_from_grounding() {
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["reschedule_work_order"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        let option = &slate.options[0];

        // The per-candidate trace is non-empty and carries this plan's own steps.
        assert!(!option.trace.steps.is_empty());
        // A Graph step derived from the candidate's evidence edge.
        assert!(option.trace.steps.iter().any(|s| s.tag == TraceTag::Graph));
        // An Assumption step derived from the candidate's omission.
        assert!(
            option
                .trace
                .steps
                .iter()
                .any(|s| s.tag == TraceTag::Assumption),
            "assumption step derives from the candidate's omissions"
        );
        // Its own Simulated step is not rejected (this candidate is admitted).
        assert!(
            option
                .trace
                .steps
                .iter()
                .any(|s| s.tag == TraceTag::Simulated && !s.rejected)
        );
        // The option still discloses its omissions structurally.
        assert!(!option.omissions.is_empty());
    }

    /// A proposer that yields a fixed sequence of verbs by call count (not feedback
    /// length), so an unscorable candidate, which earns no feedback, is followed by the
    /// next scripted one.
    struct SequenceProposer {
        verbs: Vec<&'static str>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl CandidateProposer for SequenceProposer {
        async fn propose_candidate(
            &self,
            subject: &EntityUri,
            _feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let verb = *self
                .verbs
                .get(n)
                .unwrap_or(&self.verbs[self.verbs.len() - 1]);
            Ok(candidate(subject, verb))
        }
    }

    /// A simulator that cannot project an `unscorable` action (as the real one refuses
    /// a missing or malformed param), and otherwise delegates.
    struct RefusingSimulator(Arc<InMemorySimulator>);

    #[async_trait]
    impl Simulator for RefusingSimulator {
        async fn project(&self, plan: &ekg_domain::Plan) -> Result<Box<dyn Projection>> {
            if plan.actions.iter().any(|a| a.action_type.0 == "unscorable") {
                return Err(ekg_domain::EngineError::Backend(
                    "action unscorable is missing param \"new_date\"".to_owned(),
                ));
            }
            self.0.project(plan).await
        }
    }

    #[tokio::test]
    async fn an_unscorable_candidate_is_traced_as_rejected_with_its_reason() {
        let agent = SlateAgent::new(
            Arc::new(SequenceProposer {
                verbs: vec!["unscorable", "unscorable", "reschedule_work_order"],
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(RefusingSimulator(flagged_sim())) as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");

        // The slate is still produced from the candidate that could be scored.
        assert_eq!(slate.options.len(), 1);
        assert!(slate.options[0].recommended);

        // The unscorable one is traced once (its repeat is deduplicated), as rejected,
        // naming the candidate and the reason.
        let rejected: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| {
                s.rejected && s.tag == TraceTag::Simulated && s.text.contains("could not be scored")
            })
            .collect();
        assert_eq!(rejected.len(), 1, "{:?}", slate.trace.steps);
        assert!(rejected[0].text.contains("could not be scored"));
        assert!(
            rejected[0]
                .text
                .contains("unscorable https://ekg.example/id/mrp.production/1")
        );
        assert!(rejected[0].text.contains("missing param"));
        // The fourth attempt repeats the admitted plan, which is set aside as a duplicate:
        // the only other rejected step.
        assert_eq!(slate.trace.summary.rejected, 2);
    }

    // --- unique option keys ---

    /// A second entity, also flagged, so the baseline flags two rows and a candidate can
    /// clear one (an equal middle score) or both (the best score).
    fn other() -> EntityUri {
        EntityUri("https://ekg.example/id/mrp.production/2".to_owned())
    }

    fn two_flagged_sim() -> Arc<InMemorySimulator> {
        Arc::new(
            InMemorySimulator::default()
                .with_entity(
                    subject(),
                    BTreeMap::from([("signal".to_owned(), "on".to_owned())]),
                )
                .with_entity(
                    other(),
                    BTreeMap::from([("signal".to_owned(), "on".to_owned())]),
                ),
        )
    }

    /// A grounded candidate whose plan clears the signal on each of `targets`.
    fn clearing(targets: &[EntityUri]) -> GroundedProposal {
        let mut grounded = candidate(&subject(), "reschedule_work_order");
        grounded.proposal.actions = targets
            .iter()
            .map(|target| ProposedAction {
                action_type: ActionType("reschedule_work_order".to_owned()),
                target: target.clone(),
                params: BTreeMap::from([("signal".to_owned(), "off".to_owned())]),
            })
            .collect();
        grounded
    }

    /// A proposer yielding a scripted sequence of plans by call count, repeating the last.
    struct PlanSequence {
        plans: Vec<Vec<EntityUri>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl PlanSequence {
        fn new(plans: Vec<Vec<EntityUri>>) -> Self {
            Self {
                plans,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl CandidateProposer for PlanSequence {
        async fn propose_candidate(
            &self,
            _subject: &EntityUri,
            _feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let targets = self
                .plans
                .get(n)
                .unwrap_or(&self.plans[self.plans.len() - 1]);
            Ok(clearing(targets))
        }
    }

    fn two_tier_agent() -> SlateAgent {
        SlateAgent::new(
            Arc::new(PlanSequence::new(vec![
                vec![subject()],
                vec![other()],
                vec![subject(), other()],
            ])),
            two_flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        )
    }

    #[tokio::test]
    async fn equally_scored_options_carry_distinct_keys_after_better_scores() {
        let slate = two_tier_agent()
            .propose_slate(&subject())
            .await
            .expect("slate");
        assert_eq!(
            slate.baseline.rank_key, "000002",
            "the baseline keeps its plain key"
        );
        let keys: Vec<&str> = slate
            .options
            .iter()
            .map(|o| o.score.rank_key.as_str())
            .collect();
        assert_eq!(keys.len(), 3, "{keys:?}");
        // Best first: the plan clearing both, then the two equal single clears.
        assert!(keys[0].starts_with("000000~"), "{keys:?}");
        assert!(keys[1].starts_with("000001~") && keys[2].starts_with("000001~"));
        assert_ne!(keys[1], keys[2], "equal scores, distinct keys");
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "strictly ordered: {keys:?}"
        );
        for key in &keys {
            let (_, digest) = key.split_once('~').expect("a digest suffix");
            assert_eq!(digest.len(), 12);
            assert!(
                digest
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }
        assert!(slate.options[0].recommended);
        assert_eq!(slate.options[0].plan.actions.len(), 2);
    }

    #[tokio::test]
    async fn a_duplicate_plan_is_admitted_once_and_traced_as_set_aside() {
        let agent = SlateAgent::new(
            Arc::new(PlanSequence::new(vec![vec![subject()]])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert_eq!(slate.options.len(), 1, "one option for the repeated plan");
        let set_aside: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| {
                s.text
                    .contains("set aside: duplicate of an admitted option")
            })
            .collect();
        assert_eq!(set_aside.len(), 3, "each of the three repeats is traced");
        assert!(
            set_aside
                .iter()
                .all(|s| s.rejected && s.tag == TraceTag::Simulated)
        );
    }

    #[tokio::test]
    async fn admission_is_judged_on_the_capability_key_alone() {
        // A no-op scores the same as the baseline: its unique key would sort after the
        // baseline's plain key either way, and it is still not admitted.
        let agent = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec!["noop", "reschedule_work_order"])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(SignalScorer),
        );
        let slate = agent.propose_slate(&subject()).await.expect("slate");
        assert_eq!(slate.options.len(), 1);
        assert_eq!(
            slate.options[0].plan.actions[0].action_type.0,
            "reschedule_work_order"
        );
        assert!(slate.options[0].score.rank_key.starts_with("000000~"));
    }

    #[tokio::test]
    async fn an_options_key_is_stable_across_runs_over_the_same_plan() {
        let first = two_tier_agent()
            .propose_slate(&subject())
            .await
            .expect("slate");
        let second = two_tier_agent()
            .propose_slate(&subject())
            .await
            .expect("slate");
        let keys = |slate: &ekg_domain::Slate| -> Vec<String> {
            slate
                .options
                .iter()
                .map(|o| o.score.rank_key.clone())
                .collect()
        };
        assert_eq!(keys(&first), keys(&second));
    }

    // --- the group slate (shared loop, opening Linked step) ---

    fn group() -> ekg_domain::SubjectGroup {
        ekg_domain::SubjectGroup {
            key: "portfolio/0123456789ab".to_owned(),
            members: vec![subject(), other()],
            links: vec![
                "linked: MO/1 fulfils SO/1".to_owned(),
                "linked: MO/1 and MO/2 contend for WC/1".to_owned(),
            ],
            worded: false,
        }
    }

    /// A group proposer playing a scripted sequence of outcomes by call count (`None` =
    /// a refused generation), repeating the last past the end.
    struct GroupSequence {
        plans: Vec<Option<Vec<EntityUri>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl GroupSequence {
        fn new(plans: Vec<Option<Vec<EntityUri>>>) -> Self {
            Self {
                plans,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ekg_domain::GroupCandidateProposer for GroupSequence {
        async fn propose_group_candidate(
            &self,
            _group: &ekg_domain::SubjectGroup,
            _feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match self
                .plans
                .get(n)
                .unwrap_or(&self.plans[self.plans.len() - 1])
            {
                Some(targets) => Ok(clearing(targets)),
                None => Err(ekg_domain::EngineError::InvalidProposal(
                    "unknown verb".to_owned(),
                )),
            }
        }
    }

    fn group_agent(plans: Vec<Option<Vec<EntityUri>>>) -> super::GroupSlateAgent {
        super::GroupSlateAgent::new(
            Arc::new(GroupSequence::new(plans)),
            two_flagged_sim() as Arc<dyn Simulator>,
            Arc::new(ekg_domain::fakes::FakeGroupScorer::flagging("signal", "on")),
        )
    }

    #[tokio::test]
    async fn a_group_slate_opens_with_a_linked_step_naming_every_member_and_link() {
        use ekg_domain::GroupSlateProposer;
        let slate = group_agent(vec![Some(vec![subject(), other()])])
            .propose_group_slate(&group())
            .await
            .expect("slate");
        let first = slate.trace.steps.first().expect("a first step");
        assert_eq!(first.tag, TraceTag::Graph);
        assert!(!first.rejected);
        assert!(first.text.starts_with("Linked: "), "{}", first.text);
        for member in &group().members {
            assert!(first.text.contains(&member.0), "{}", first.text);
        }
        for link in &group().links {
            assert!(first.text.contains(link.as_str()), "{}", first.text);
        }
        // The baseline follows the opening step, as on a point slate.
        assert_eq!(slate.trace.steps[1].tag, TraceTag::Simulated);
        assert!(slate.trace.steps[1].text.starts_with("do-nothing baseline"));
    }

    #[tokio::test]
    async fn a_worded_group_opens_with_its_sentences_and_no_member_list() {
        use ekg_domain::GroupSlateProposer;
        let worded = ekg_domain::SubjectGroup {
            links: vec![
                format!(
                    "{} and {} are booked on WC/1 at the same time.",
                    subject().0,
                    other().0
                ),
                format!("{} builds SO/1.", subject().0),
            ],
            worded: true,
            ..group()
        };
        let slate = group_agent(vec![Some(vec![subject(), other()])])
            .propose_group_slate(&worded)
            .await
            .expect("slate");
        let first = slate.trace.steps.first().expect("a first step");
        assert_eq!(first.tag, TraceTag::Graph);
        assert_eq!(
            first.text,
            format!("Linked: {} {}", worded.links[0], worded.links[1])
        );
        // The recorded-reason form keeps its member list.
        let recorded = group_agent(vec![Some(vec![subject(), other()])])
            .propose_group_slate(&group())
            .await
            .expect("slate");
        assert_eq!(
            recorded.trace.steps[0].text,
            format!(
                "Linked: {} and {}. {}",
                subject().0,
                other().0,
                group().links.join("; ")
            )
        );
    }

    /// The flagging group fake with a capability wording of its score.
    struct DescribingGroupScorer(ekg_domain::fakes::FakeGroupScorer);
    #[async_trait]
    impl GroupScorer for DescribingGroupScorer {
        async fn score_group(
            &self,
            projection: &dyn Projection,
            group: &SubjectGroup,
        ) -> Result<Score> {
            self.0.score_group(projection, group).await
        }
        fn describe(&self, score: &Score) -> Option<String> {
            Some(format!("{} worded", score.rank_key))
        }
    }

    #[tokio::test]
    async fn a_describing_group_scorer_words_the_simulated_steps() {
        use ekg_domain::GroupSlateProposer;
        let slate = super::GroupSlateAgent::new(
            Arc::new(GroupSequence::new(vec![Some(vec![subject(), other()])])),
            two_flagged_sim() as Arc<dyn Simulator>,
            Arc::new(DescribingGroupScorer(
                ekg_domain::fakes::FakeGroupScorer::flagging("signal", "on"),
            )),
        )
        .propose_group_slate(&group())
        .await
        .expect("slate");
        let steps = simulated(&slate);
        assert!(steps[0].0.starts_with("Do nothing: ") && steps[0].0.ends_with(" worded"));
        assert!(steps[1].0.starts_with("Candidate: "), "{steps:?}");
        assert!(
            steps.iter().all(|(text, _)| !text.contains('=')),
            "{steps:?}"
        );
    }

    /// [`SignalScorer`] whose plan-aware key leads with a penalty on any
    /// `reschedule_work_order` plan, a key the plain `score` cannot produce.
    struct PlanAwareScorer;
    #[async_trait]
    impl Scorer for PlanAwareScorer {
        async fn score(&self, projection: &dyn Projection) -> Result<Score> {
            SignalScorer.score(projection).await
        }
        async fn score_plan(
            &self,
            plan: &ekg_domain::Plan,
            projection: &dyn Projection,
        ) -> Result<Score> {
            let mut score = self.score(projection).await?;
            let penalty = plan
                .actions
                .iter()
                .any(|a| a.action_type.0 == "reschedule_work_order");
            score.rank_key = format!("plan{}:{}", u8::from(penalty), score.rank_key);
            Ok(score)
        }
    }

    #[tokio::test]
    async fn the_point_loop_ranks_by_the_plan_aware_key() {
        let slate = SlateAgent::new(
            Arc::new(ScriptedProposer::new(vec![
                "reschedule_work_order",
                "split_production",
            ])),
            flagged_sim() as Arc<dyn Simulator>,
            Arc::new(PlanAwareScorer),
        )
        .propose_slate(&subject())
        .await
        .expect("slate");
        assert!(
            slate.baseline.rank_key.starts_with("plan0:"),
            "{}",
            slate.baseline.rank_key
        );
        let verbs: Vec<&str> = slate
            .options
            .iter()
            .map(|o| o.plan.actions[0].action_type.0.as_str())
            .collect();
        assert_eq!(
            verbs,
            ["split_production"],
            "the penalised plan is not admitted"
        );
        assert!(slate.options[0].score.rank_key.starts_with("plan0:"));
    }

    /// The flagging group fake whose plan-aware key ends with how many actions the plan takes.
    struct PlanAwareGroupScorer(ekg_domain::fakes::FakeGroupScorer);
    #[async_trait]
    impl GroupScorer for PlanAwareGroupScorer {
        async fn score_group(
            &self,
            projection: &dyn Projection,
            group: &SubjectGroup,
        ) -> Result<Score> {
            self.0.score_group(projection, group).await
        }
        async fn score_group_plan(
            &self,
            plan: &ekg_domain::Plan,
            projection: &dyn Projection,
            group: &SubjectGroup,
        ) -> Result<Score> {
            let mut score = self.score_group(projection, group).await?;
            score.rank_key = format!("{}:n{}", score.rank_key, plan.actions.len());
            Ok(score)
        }
    }

    #[tokio::test]
    async fn the_group_loop_ranks_by_the_plan_aware_key() {
        use ekg_domain::GroupSlateProposer;
        let slate = super::GroupSlateAgent::new(
            Arc::new(GroupSequence::new(vec![Some(vec![subject(), other()])])),
            two_flagged_sim() as Arc<dyn Simulator>,
            Arc::new(PlanAwareGroupScorer(
                ekg_domain::fakes::FakeGroupScorer::flagging("signal", "on"),
            )),
        )
        .propose_group_slate(&group())
        .await
        .expect("slate");
        assert!(
            slate.baseline.rank_key.contains(":n0"),
            "{}",
            slate.baseline.rank_key
        );
        assert!(!slate.options.is_empty());
        assert!(
            slate
                .options
                .iter()
                .all(|o| o.score.rank_key.contains(":n2")),
            "{:?}",
            slate.options
        );
    }

    #[tokio::test]
    async fn a_group_slate_admits_only_candidates_beating_the_baseline() {
        use ekg_domain::GroupSlateProposer;
        // Clearing both members, one member, and nothing (an empty-target plan is a
        // no-op): only the first two beat the baseline (two members flagged).
        let slate = group_agent(vec![
            Some(vec![subject()]),
            Some(vec![subject(), other()]),
            Some(Vec::new()),
        ])
        .with_max_candidates_for_test(3)
        .propose_group_slate(&group())
        .await
        .expect("slate");
        assert_eq!(slate.options.len(), 2);
        assert!(
            slate
                .options
                .iter()
                .all(|o| o.score.rank_key < slate.baseline.rank_key)
        );
        // Clearing both ranks first and is recommended.
        assert_eq!(slate.options[0].plan.actions.len(), 2);
        assert!(slate.options[0].recommended);
        // The no-op is traced as rejected, not dropped.
        assert!(
            slate
                .trace
                .steps
                .iter()
                .any(|s| s.tag == TraceTag::Simulated && s.rejected)
        );
    }

    #[tokio::test]
    async fn a_group_slate_keeps_rank_keys_unique() {
        use ekg_domain::GroupSlateProposer;
        // Two distinct plans that both clear only `subject`: equal capability keys.
        let slate = group_agent(vec![
            Some(vec![subject()]),
            Some(vec![subject(), subject()]),
        ])
        .with_max_candidates_for_test(2)
        .propose_group_slate(&group())
        .await
        .expect("slate");
        assert_eq!(slate.options.len(), 2);
        assert_ne!(
            slate.options[0].score.rank_key,
            slate.options[1].score.rank_key
        );
        assert!(slate.options.iter().all(|o| o.score.rank_key.contains('~')));
    }

    #[tokio::test]
    async fn a_group_slate_traces_a_failed_generation() {
        use ekg_domain::GroupSlateProposer;
        let slate = group_agent(vec![None, Some(vec![subject(), other()])])
            .with_max_candidates_for_test(2)
            .propose_group_slate(&group())
            .await
            .expect("slate");
        let generation: Vec<_> = slate
            .trace
            .steps
            .iter()
            .filter(|s| s.tag == TraceTag::Generation)
            .collect();
        assert_eq!(generation.len(), 1);
        assert!(generation[0].rejected);
        assert!(
            generation[0]
                .text
                .contains("attempt 1: refused — unknown verb"),
            "{}",
            generation[0].text
        );
        // The loop moved on and still admitted the next candidate.
        assert_eq!(slate.options.len(), 1);
    }

    impl super::GroupSlateAgent {
        /// Rebuild with a different candidate bound (tests only).
        fn with_max_candidates_for_test(self, max: usize) -> Self {
            Self {
                max_candidates: max,
                ..self
            }
        }
    }
}
