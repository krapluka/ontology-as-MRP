//! The eval loop: each case in its own world, `k` trials each, under a token bound.
//!
//! Building a case's world and the arm that runs in it is the caller's ([`CaseEnv`]); the loop
//! only orders the work, counts tokens, grades, and makes sure a case's world is closed again
//! whatever its trials did.
//!
//! Cases run one after another. The trials of a case may run at the same time, and a trial
//! that errors is attempted again: an errored attempt carries no grade, so nothing that was
//! graded is ever run twice.
//!
//! Each attempt is a trace of its own, named by its case, that holds the arm's run. A run
//! can be marked as an [`Experiment`]: each attempt's trace then also says which experiment
//! run and which case it is, in the attributes a trace backend groups traces by.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use ekg_domain::{EntityUri, Plan, Proposal, Score};
use futures::StreamExt as _;
use opentelemetry::KeyValue;
use opentelemetry::trace::{FutureExt as _, SpanKind};

use super::grade::{Arm, EngineChecks, Grade, TrialEnd, grade, violations};
use super::report::{
    CaseResult, ExperimentRef, GradeKind, Narrowing, ReportedPlan, RunSettings, TrialResult,
    VersionTuple,
};
use super::set::{Case, Outcome, PlansFor, ResolvedCase};
use crate::telemetry;

/// Opens a case: builds its world and what a trial in it needs.
#[async_trait]
pub trait CaseEnv: Send + Sync {
    /// Open `case`.
    ///
    /// # Errors
    /// A message when the case's world cannot be built.
    async fn open(&self, case: &Case) -> Result<Box<dyn OpenCase>, String>;
}

/// A case with its world built, until it is closed.
#[async_trait]
pub trait OpenCase: Send + Sync {
    /// The case with its entities as IRIs.
    fn resolved(&self) -> &ResolvedCase;

    /// Whether `entity` is in the scope of a task for the case's subject, or for its members.
    fn in_scope(&self, entity: &EntityUri) -> bool;

    /// The action types of the profile's catalog.
    fn action_types(&self) -> &[String];

    /// The most actions a plan may have.
    fn max_actions(&self) -> usize;

    /// The arm that runs in this world.
    fn arm(&self) -> &dyn Arm;

    /// Simulate `plan` in this world and score it. The empty plan scores doing nothing.
    ///
    /// # Errors
    /// A message when the plan cannot be simulated or scored.
    async fn score(&self, plan: &Plan) -> Result<Score, String>;

    /// Remove the case's world.
    async fn close(self: Box<Self>);
}

/// The bounds of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    /// The trials per case (`k`), at least one.
    pub trials: u32,
    /// The tokens after which no further trial, and no further attempt of one, starts.
    pub max_tokens: u64,
    /// The further attempts of a trial that errored. Zero runs every trial once.
    pub retries: u32,
    /// The most trials of one case in flight at once, at least one.
    pub concurrency: u32,
    /// The eval set's version, which each attempt's trace names.
    pub set_version: u32,
}

/// The experiment run a run's traces are marked as: one per arm of a run. It carries what the
/// run is measured with, as names, versions and counts, and nothing of a case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Experiment {
    /// The experiment run's id, unique to this arm of this run.
    pub id: String,
    /// Its name, as a reader tells runs apart.
    pub name: String,
    /// What groups the runs of one eval set version. A name, not a stored dataset.
    pub dataset: String,
    /// What the run is measured with, by name.
    pub metadata: BTreeMap<String, String>,
}

impl Experiment {
    /// The experiment run of `arm` in the run started at `started`, measured with `version`
    /// under `settings`, narrowed to `narrowing`. `version.model` is left out when empty: an
    /// agent's model is known only once a run reported it, and each trace then names its own.
    #[must_use]
    pub fn of(
        arm: &str,
        started: &str,
        version: &VersionTuple,
        settings: RunSettings,
        narrowing: &Narrowing,
    ) -> Self {
        let mut metadata = BTreeMap::from([
            ("arm".to_owned(), arm.to_owned()),
            ("engine_version".to_owned(), version.engine.clone()),
            ("set_version".to_owned(), version.set_version.to_string()),
            ("set_digest".to_owned(), version.set_digest.clone()),
            ("k".to_owned(), version.trials.to_string()),
            ("max_steps".to_owned(), version.max_steps.to_string()),
            (
                "analyst_max_steps".to_owned(),
                version.analyst_max_steps.to_string(),
            ),
            ("planners".to_owned(), version.planners.to_string()),
            (
                "planner_reads".to_owned(),
                version.planner_reads.to_string(),
            ),
            (
                "joint_max_steps".to_owned(),
                version.joint_max_steps.to_string(),
            ),
            ("concurrency".to_owned(), settings.concurrency.to_string()),
            ("retries".to_owned(), settings.retries.to_string()),
            ("narrowed".to_owned(), (!narrowing.is_empty()).to_string()),
        ]);
        if !version.model.is_empty() {
            metadata.insert("model".to_owned(), version.model.clone());
        }
        for (name, digest) in &version.skills {
            metadata.insert(format!("skill.{name}"), digest.clone());
        }
        for (profile, tools) in &version.tools {
            metadata.insert(format!("tools.{profile}"), tools.join(", "));
        }
        for (kind, names) in [
            ("case", &narrowing.cases),
            ("category", &narrowing.categories),
            ("profile", &narrowing.profiles),
        ] {
            if !names.is_empty() {
                metadata.insert(format!("narrowed_to.{kind}"), names.join(", "));
            }
        }
        Self {
            id: format!("{started}-{arm}"),
            name: format!("v{} {arm} {started}", version.set_version),
            dataset: format!("agent-evals-v{}", version.set_version),
            metadata,
        }
    }

    /// What a report says of this experiment run.
    #[must_use]
    pub fn reference(&self) -> ExperimentRef {
        ExperimentRef {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    /// The attributes that mark the trace rooted at `root_span` as `case`'s item of this
    /// experiment run, for trial `n`'s `attempt`, graded `grade`.
    fn item(
        &self,
        case: &Case,
        root_span: String,
        n: u32,
        attempt: i64,
        result: &TrialResult,
    ) -> Vec<KeyValue> {
        let mut attributes = vec![
            KeyValue::new("langfuse.experiment.id", self.id.clone()),
            KeyValue::new("langfuse.experiment.name", self.name.clone()),
            KeyValue::new("langfuse.experiment.dataset.id", self.dataset.clone()),
            KeyValue::new("langfuse.experiment.item.id", case.id.clone()),
            KeyValue::new("langfuse.experiment.item.root_observation_id", root_span),
            KeyValue::new(
                "langfuse.experiment.item.expected_output",
                match case.expect.outcome {
                    Outcome::Plan => "plan",
                    Outcome::Abstain => "no_plan",
                },
            ),
            KeyValue::new(
                "langfuse.experiment.item.metadata.category",
                case.category.clone(),
            ),
            KeyValue::new(
                "langfuse.experiment.item.metadata.profile",
                case.profile.clone(),
            ),
            KeyValue::new("langfuse.experiment.item.metadata.trial", i64::from(n)),
            KeyValue::new("langfuse.experiment.item.metadata.attempt", attempt),
            KeyValue::new("langfuse.experiment.item.metadata.end", result.end.clone()),
            KeyValue::new("langfuse.observation.input", case.id.clone()),
            KeyValue::new("langfuse.observation.output", grade_name(result.grade)),
        ];
        for (key, value) in &self.metadata {
            attributes.push(KeyValue::new(
                format!("langfuse.experiment.metadata.{key}"),
                value.clone(),
            ));
        }
        // The model a run reported, when the experiment could not name one beforehand.
        if let Some(model) = result
            .run
            .as_ref()
            .map(|run| &run.model)
            .filter(|model| !model.is_empty())
        {
            attributes.push(KeyValue::new(
                "langfuse.experiment.item.metadata.model",
                model.clone(),
            ));
            if !self.metadata.contains_key("model") {
                attributes.push(KeyValue::new(
                    "langfuse.experiment.metadata.model",
                    model.clone(),
                ));
            }
        }
        attributes
    }
}

/// The grade of an attempt, as its trace names it.
fn grade_name(grade: GradeKind) -> &'static str {
    match grade {
        GradeKind::Pass => "passed",
        GradeKind::Fail => "failed",
        GradeKind::Violation => "violation",
        GradeKind::Errored => "error",
    }
}

/// A case none of whose trials could be graded, for `why`.
fn unopened(case: &Case, trials: u32, why: &str) -> CaseResult {
    CaseResult {
        id: case.id.clone(),
        category: case.category.clone(),
        profile: case.profile.clone(),
        baseline: None,
        trials: (1..=trials)
            .map(|n| {
                TrialResult::new(
                    n,
                    "errored",
                    Grade::Errored(vec![why.to_owned()]),
                    None,
                    None,
                    None,
                )
            })
            .collect(),
        not_run: 0,
    }
}

/// Run one attempt of a trial in `open` and grade it, adding its tokens to `tokens`.
async fn one_attempt(
    open: &dyn OpenCase,
    n: u32,
    baseline: &Score,
    tokens: &AtomicU64,
) -> TrialResult {
    let resolved = open.resolved();
    let trial = open.arm().trial(&resolved.plans_for, &resolved.tried).await;
    for run in trial.run.iter().chain(&trial.other_runs) {
        tokens.fetch_add(
            run.input_tokens.saturating_add(run.output_tokens),
            Ordering::SeqCst,
        );
    }
    let mut end = trial.end;
    let mut score = None;
    // A slate shows a round's plans, so a forbidden action counts in any of them.
    let mut forbidden = Vec::new();
    let mut other_plans = Vec::new();
    let mut set_aside = 0;
    if let TrialEnd::Submitted(proposal) = &end {
        let mut round = vec![proposal.clone()];
        round.extend(trial.candidates);
        // The scorer chooses among a round's plans, as it ranks them on a slate.
        let mut best: Option<(Proposal, Score)> = None;
        let mut failed = None;
        let reported = |proposal: &Proposal| ReportedPlan {
            actions: proposal.actions.clone(),
            explanation: proposal.explanation.clone(),
        };
        for proposal in round {
            forbidden.extend(violations(resolved, &proposal));
            let plan = Plan {
                actions: proposal.actions.clone(),
            };
            match open.score(&plan).await {
                Ok(scored)
                    if best
                        .as_ref()
                        .is_none_or(|(_, b)| scored.rank_key < b.rank_key) =>
                {
                    if let Some((beaten, _)) = best.replace((proposal, scored)) {
                        other_plans.push(reported(&beaten));
                    }
                }
                Ok(_) => other_plans.push(reported(&proposal)),
                Err(why) => {
                    other_plans.push(reported(&proposal));
                    failed = Some(why);
                }
            }
        }
        match (best, failed) {
            // A slate admits only a plan that beats doing nothing, so a joint round with none
            // shows no option.
            (Some((proposal, scored)), _)
                if matches!(resolved.plans_for, PlansFor::Group(_))
                    && resolved.expect.beats_baseline.unwrap_or(true)
                    && scored.rank_key >= baseline.rank_key =>
            {
                other_plans.push(reported(&proposal));
                set_aside = other_plans.len();
                end = TrialEnd::Declined;
            }
            (Some((proposal, scored)), _) => {
                end = TrialEnd::Submitted(proposal);
                score = Some(scored);
            }
            (None, why) => {
                end = TrialEnd::Errored(format!(
                    "the plan could not be scored: {}",
                    why.unwrap_or_default()
                ));
            }
        }
    }
    let in_scope = |entity: &EntityUri| open.in_scope(entity);
    let graded = grade(
        resolved,
        &end,
        score.as_ref(),
        baseline,
        &EngineChecks {
            in_scope: &in_scope,
            action_types: open.action_types(),
            max_actions: open.max_actions(),
        },
    );
    let plan = match &end {
        TrialEnd::Submitted(proposal) => Some(ReportedPlan {
            actions: proposal.actions.clone(),
            explanation: proposal.explanation.clone(),
        }),
        _ => None,
    };
    let graded = if forbidden.is_empty() {
        graded
    } else {
        Grade::Violation(forbidden)
    };
    TrialResult::new(n, end.name(), graded, plan, score, trial.run)
        .with_other_runs(trial.other_runs)
        .with_other_plans(other_plans)
        .with_set_aside(set_aside)
}

/// Run trial `n` in `open`: an attempt, and while it errors, attempts are left and the token
/// bound is not reached, another. A graded attempt stands, whatever its grade. The result is
/// the last attempt's, with the reason of every attempt that errored before it.
async fn one_trial(
    open: &dyn OpenCase,
    (case, experiment): (&Case, Option<&Experiment>),
    n: u32,
    baseline: &Score,
    limits: RunLimits,
    tokens: &AtomicU64,
) -> TrialResult {
    let mut errored = Vec::new();
    loop {
        // A root span, attached to this attempt's future only: trials of a case run at the
        // same time, and each attempt is a trace of its own.
        let span = telemetry::root(format!("eval_trial {}", case.id), SpanKind::Internal);
        let mut result = one_attempt(open, n, baseline, tokens)
            .with_context(span.clone())
            .await;
        let attempt = i64::try_from(errored.len())
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let graded = result.grade != GradeKind::Errored;
        let mut attributes = vec![
            KeyValue::new("ekg.eval.case", case.id.clone()),
            KeyValue::new("ekg.eval.trial", i64::from(n)),
            KeyValue::new("ekg.eval.attempt", attempt),
            KeyValue::new("ekg.eval.arm", open.arm().name()),
            KeyValue::new("ekg.eval.set_version", i64::from(limits.set_version)),
            KeyValue::new("ekg.eval.grade", grade_name(result.grade)),
            KeyValue::new("ekg.eval.end", result.end.clone()),
        ];
        if let (Some(experiment), Some(root)) = (experiment, telemetry::span_id(&span)) {
            attributes.extend(experiment.item(case, root, n, attempt, &result));
        }
        telemetry::finish(&span, attributes, graded, None);
        let attempts_left = u32::try_from(errored.len()).unwrap_or(u32::MAX) < limits.retries;
        if result.grade != GradeKind::Errored
            || !attempts_left
            || tokens.load(Ordering::SeqCst) >= limits.max_tokens
        {
            result.errored_attempts = errored;
            return result;
        }
        errored.push(result.reasons.join("; "));
    }
}

/// Run `cases` one after another, `limits.trials` trials each and up to `limits.concurrency`
/// of a case at once, starting no trial and no further attempt once the tokens used reach
/// `limits.max_tokens`. Trials in flight when the bound is reached end and are graded. A
/// case's trials are in the order of their numbers, whichever ended first. Returns each
/// case's result and the tokens used.
///
/// A case whose world cannot be built, or whose do-nothing score cannot be computed, has all
/// its trials errored. Every opened case is closed.
pub async fn run_cases(
    env: &dyn CaseEnv,
    cases: &[&Case],
    limits: RunLimits,
) -> (Vec<CaseResult>, u64) {
    run_cases_as(env, cases, limits, None).await
}

/// [`run_cases`], with each attempt's trace marked as an item of `experiment`.
pub async fn run_cases_as(
    env: &dyn CaseEnv,
    cases: &[&Case],
    limits: RunLimits,
    experiment: Option<&Experiment>,
) -> (Vec<CaseResult>, u64) {
    let k = limits.trials.max(1);
    let in_flight = usize::try_from(limits.concurrency.max(1)).unwrap_or(1);
    let tokens = AtomicU64::new(0);
    let spent = || tokens.load(Ordering::SeqCst) >= limits.max_tokens;
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        if spent() {
            results.push(CaseResult {
                id: case.id.clone(),
                category: case.category.clone(),
                profile: case.profile.clone(),
                baseline: None,
                trials: Vec::new(),
                not_run: k,
            });
            continue;
        }
        let open = match env.open(case).await {
            Ok(open) => open,
            Err(why) => {
                results.push(unopened(
                    case,
                    k,
                    &format!("the case's world could not be built: {why}"),
                ));
                continue;
            }
        };
        let baseline = match open
            .score(&Plan {
                actions: Vec::new(),
            })
            .await
        {
            Ok(baseline) => baseline,
            Err(why) => {
                open.close().await;
                results.push(unopened(
                    case,
                    k,
                    &format!("doing nothing could not be scored: {why}"),
                ));
                continue;
            }
        };
        let (case_open, tokens, spent) = (open.as_ref(), &tokens, &spent);
        let (baseline_ref, marked) = (&baseline, (*case, experiment));
        // `buffered` yields in the order the trials were started, not the order they ended.
        let ran: Vec<Option<TrialResult>> = futures::stream::iter(1..=k)
            .map(|n| async move {
                if spent() {
                    return None;
                }
                Some(one_trial(case_open, marked, n, baseline_ref, limits, tokens).await)
            })
            .buffered(in_flight)
            .collect()
            .await;
        let trials: Vec<TrialResult> = ran.into_iter().flatten().collect();
        let not_run = k.saturating_sub(u32::try_from(trials.len()).unwrap_or(u32::MAX));
        open.close().await;
        results.push(CaseResult {
            id: case.id.clone(),
            category: case.category.clone(),
            profile: case.profile.clone(),
            baseline: Some(baseline),
            trials,
            not_run,
        });
    }
    (results, tokens.load(Ordering::SeqCst))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use ekg_domain::{ActionType, Proposal, ProposedAction, ScoredCandidate};

    use super::super::grade::Trial;
    use super::super::report::GradeKind;
    use super::super::set::EvalSet;
    use super::super::set::tests::{JOINT_MANIFEST, MANIFEST, case, joint_case};
    use super::*;
    use crate::Termination;
    use crate::agent_proposer::RunRecord;

    /// Counts what the loop did to the environment.
    #[derive(Default)]
    struct Counts {
        opened: AtomicU32,
        closed: AtomicU32,
        trials: AtomicU32,
        in_flight: AtomicU32,
        most_in_flight: AtomicU32,
        /// What each run was asked to plan for.
        planned_for: std::sync::Mutex<Vec<PlansFor>>,
    }

    /// How the arm's runs end, by the number of the run across the environment, from 1.
    type Ends = Arc<dyn Fn(u32) -> TrialEnd + Send + Sync>;

    /// An arm that reports 100 tokens a run. A run yields `waits(run)` times before it ends,
    /// so a later run can end before an earlier one.
    struct FixedArm {
        counts: Arc<Counts>,
        end: Ends,
        waits: fn(u32) -> u32,
        candidates: Vec<Proposal>,
    }

    #[async_trait]
    impl Arm for FixedArm {
        fn name(&self) -> &'static str {
            "fixed"
        }

        async fn trial(&self, plans_for: &PlansFor, _tried: &[ScoredCandidate]) -> Trial {
            self.counts
                .planned_for
                .lock()
                .expect("planned for")
                .push(plans_for.clone());
            let run = self.counts.trials.fetch_add(1, Ordering::SeqCst) + 1;
            let in_flight = self.counts.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.counts
                .most_in_flight
                .fetch_max(in_flight, Ordering::SeqCst);
            for _ in 0..(self.waits)(run) {
                tokio::task::yield_now().await;
            }
            self.counts.in_flight.fetch_sub(1, Ordering::SeqCst);
            Trial {
                end: (self.end)(run),
                run: Some(RunRecord {
                    termination: Termination::Submitted,
                    steps: 1,
                    input_tokens: 60,
                    output_tokens: 40,
                    model: "m".to_owned(),
                    elapsed_ms: 1,
                    tool_calls: BTreeMap::new(),
                    rejections: Vec::new(),
                    skills: BTreeMap::new(),
                    tools: Vec::new(),
                    trace_id: "t".to_owned(),
                    closing_answer: None,
                    role: None,
                    finding: None,
                }),
                other_runs: Vec::new(),
                candidates: self.candidates.clone(),
            }
        }
    }

    struct Open {
        resolved: ResolvedCase,
        arm: FixedArm,
        types: Vec<String>,
        score_fails: bool,
    }

    #[async_trait]
    impl OpenCase for Open {
        fn resolved(&self) -> &ResolvedCase {
            &self.resolved
        }
        fn in_scope(&self, _entity: &EntityUri) -> bool {
            true
        }
        fn action_types(&self) -> &[String] {
            &self.types
        }
        fn max_actions(&self) -> usize {
            3
        }
        fn arm(&self) -> &dyn Arm {
            &self.arm
        }
        async fn score(&self, plan: &Plan) -> Result<Score, String> {
            if self.score_fails && !plan.actions.is_empty() {
                return Err("the simulator is down".to_owned());
            }
            // A group's score: each member an action targets is no longer at risk.
            let members = self.resolved.plans_for.subjects();
            let at_risk = members
                .iter()
                .filter(|member| !plan.actions.iter().any(|a| a.target == **member))
                .count();
            Ok(Score {
                metrics: BTreeMap::from([("members_at_risk".to_owned(), at_risk.to_string())]),
                // Scrapping helps nothing, so it ranks with doing nothing.
                rank_key: if plan.actions.iter().all(|a| a.action_type.0 == "scrap") {
                    "2"
                } else {
                    "1"
                }
                .to_owned(),
            })
        }
        async fn close(self: Box<Self>) {
            self.arm.counts.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Env {
        counts: Arc<Counts>,
        end: Ends,
        waits: fn(u32) -> u32,
        candidates: Vec<Proposal>,
        refuse: Option<&'static str>,
        score_fails: bool,
    }

    impl Env {
        /// Every run ends the same way.
        fn new(end: fn() -> TrialEnd) -> Self {
            Self::scripted(move |_| end())
        }

        /// Each run ends as `end` says for its number.
        fn scripted(end: impl Fn(u32) -> TrialEnd + Send + Sync + 'static) -> Self {
            Self {
                counts: Arc::new(Counts::default()),
                end: Arc::new(end),
                waits: |_| 1,
                candidates: Vec::new(),
                refuse: None,
                score_fails: false,
            }
        }

        fn runs(&self) -> u32 {
            self.counts.trials.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CaseEnv for Env {
        async fn open(&self, case: &Case) -> Result<Box<dyn OpenCase>, String> {
            if self.refuse == Some(case.id.as_str()) {
                return Err("the write shapes refused a record".to_owned());
            }
            self.counts.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Open {
                resolved: case.resolve(&|model, id| format!("x:{model}/{id}")),
                arm: FixedArm {
                    counts: Arc::clone(&self.counts),
                    end: Arc::clone(&self.end),
                    waits: self.waits,
                    candidates: self.candidates.clone(),
                },
                types: vec!["move".to_owned()],
                score_fails: self.score_fails,
            }))
        }
    }

    fn plan() -> TrialEnd {
        TrialEnd::Submitted(Proposal {
            explanation: "Moved.".to_owned(),
            actions: vec![ProposedAction {
                action_type: ActionType("move".to_owned()),
                target: EntityUri("x:sale.order/1".to_owned()),
                params: BTreeMap::new(),
            }],
            omissions: Vec::new(),
        })
    }

    fn set() -> EvalSet {
        let plan = "expect: { outcome: plan }\n";
        EvalSet::parse(
            MANIFEST,
            [case("a", "late", plan), case("b", "late", plan)]
                .into_iter()
                .collect(),
        )
        .expect("set")
    }

    #[tokio::test]
    async fn a_joint_case_is_one_plan_for_the_group_graded_against_the_groups_score() {
        // A plan that acts on one member only: it ranks ahead of doing nothing, and leaves the
        // other member at risk.
        let env = Env::new(plan);
        let bounded = "expect: { outcome: plan, metrics: { members_at_risk: { at_most: 0 } } }\n";
        let set = EvalSet::parse(
            JOINT_MANIFEST,
            [
                joint_case("free", "joint", "expect: { outcome: plan }\n"),
                joint_case("strict", "joint", bounded),
            ]
            .into_iter()
            .collect(),
        )
        .expect("set");
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (results, _) = run_cases(&env, &cases, limits(1, u64::MAX)).await;

        let planned = env.counts.planned_for.lock().expect("planned for");
        let PlansFor::Group(group) = &planned[0] else {
            panic!(
                "the arm was asked to plan for a group, got {:?}",
                planned[0]
            );
        };
        assert_eq!(group.key, "case/free");
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.links, ["MO/2 builds S1"]);

        assert_eq!(results[0].id, "free", "a joint case is named by its id");
        assert_eq!(results[0].trials[0].grade, GradeKind::Pass);
        assert_eq!(results[1].trials[0].grade, GradeKind::Fail);
        assert_eq!(
            results[1].trials[0].reasons,
            ["metrics: members_at_risk is 1, above 0"]
        );
        assert_eq!(
            results[1].baseline.as_ref().expect("baseline").metrics["members_at_risk"],
            "2",
            "doing nothing is scored for the group"
        );
    }

    /// One trial at a time, each run once.
    fn limits(trials: u32, max_tokens: u64) -> RunLimits {
        RunLimits {
            trials,
            max_tokens,
            retries: 0,
            concurrency: 1,
            set_version: 1,
        }
    }

    fn retrying(trials: u32, retries: u32) -> RunLimits {
        RunLimits {
            retries,
            ..limits(trials, 10_000)
        }
    }

    fn at_once(trials: u32, concurrency: u32, max_tokens: u64) -> RunLimits {
        RunLimits {
            concurrency,
            ..limits(trials, max_tokens)
        }
    }

    fn down() -> TrialEnd {
        TrialEnd::Errored("the answer stalled for 30s".to_owned())
    }

    /// A plan with an action the test set forbids nowhere but the catalog does not hold, so
    /// the engine's own check fails it.
    fn off_catalog() -> TrialEnd {
        let TrialEnd::Submitted(mut proposal) = plan() else {
            return TrialEnd::Declined;
        };
        proposal.actions[0].action_type = ActionType("scrap".to_owned());
        TrialEnd::Submitted(proposal)
    }

    async fn one_case(env: &Env, limits: RunLimits) -> (CaseResult, u64) {
        let set = set();
        let cases: Vec<&Case> = set.cases.iter().take(1).collect();
        let (mut results, tokens) = run_cases(env, &cases, limits).await;
        (results.remove(0), tokens)
    }

    #[tokio::test]
    async fn a_joint_round_with_no_plan_that_beats_doing_nothing_is_declined() {
        let set = EvalSet::parse(
            JOINT_MANIFEST,
            [joint_case(
                "none",
                "joint",
                "expect: { outcome: abstain }\nfutile:\n  - actions:\n      - { action_type: move, target: { model: sale.order, id: 1 } }\n",
            )]
            .into_iter()
            .collect(),
        )
        .expect("a valid set");
        let mut env = Env::new(off_catalog);
        let TrialEnd::Submitted(idle) = off_catalog() else {
            panic!("a plan");
        };
        env.candidates = vec![idle];
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (results, _) = run_cases(&env, &cases, limits(1, 10_000)).await;
        let trial = &results[0].trials[0];
        assert_eq!((trial.end.as_str(), trial.set_aside), ("declined", 2));
        assert_eq!(trial.grade, GradeKind::Pass, "{trial:?}");
    }

    #[tokio::test]
    async fn every_case_runs_k_trials_in_its_own_world_and_is_closed() {
        let (set, env) = (set(), Env::new(plan));
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (results, tokens) = run_cases(&env, &cases, limits(3, 10_000)).await;
        assert_eq!(tokens, 600);
        assert_eq!(env.counts.opened.load(Ordering::SeqCst), 2);
        assert_eq!(env.counts.closed.load(Ordering::SeqCst), 2);
        assert_eq!(env.counts.trials.load(Ordering::SeqCst), 6);
        assert!(results.iter().all(|case| case.passed_at(3)), "{results:?}");
        let first = &results[0].trials[0];
        assert_eq!(first.end, "submitted");
        assert_eq!(first.plan.as_ref().expect("plan").explanation, "Moved.");
        assert_eq!(first.score.as_ref().expect("score").rank_key, "1");
        assert_eq!(
            results[0].baseline.as_ref().expect("baseline").rank_key,
            "2"
        );
    }

    #[tokio::test]
    async fn the_token_bound_starts_no_trial_once_it_is_reached() {
        let (set, env) = (set(), Env::new(plan));
        let cases: Vec<&Case> = set.cases.iter().collect();
        // 100 tokens a trial: the bound of 400 is reached after the fourth.
        let (results, tokens) = run_cases(&env, &cases, limits(3, 400)).await;
        assert_eq!(tokens, 400);
        assert_eq!(env.counts.trials.load(Ordering::SeqCst), 4);
        assert_eq!((results[0].trials.len(), results[0].not_run), (3, 0));
        assert_eq!((results[1].trials.len(), results[1].not_run), (1, 2));
        assert_eq!(
            env.counts.closed.load(Ordering::SeqCst),
            2,
            "the open case is still closed"
        );

        // A bound already spent before a case opens builds no world for it.
        let env = Env::new(plan);
        let (results, _) = run_cases(&env, &cases, limits(3, 300)).await;
        assert_eq!(env.counts.opened.load(Ordering::SeqCst), 1);
        assert_eq!((results[1].trials.len(), results[1].not_run), (0, 3));
    }

    #[tokio::test]
    async fn a_case_whose_world_cannot_be_built_is_errored_and_the_run_goes_on() {
        let set = set();
        let mut env = Env::new(plan);
        env.refuse = Some("a");
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (results, _) = run_cases(&env, &cases, limits(2, 10_000)).await;
        assert_eq!(results[0].errored(), 2);
        assert!(
            results[0].trials[0].reasons[0].contains("write shapes"),
            "{results:?}"
        );
        assert!(results[1].passed_at(2));
    }

    #[tokio::test]
    async fn a_trial_is_graded_on_the_best_plan_of_its_round() {
        let set = set();
        let TrialEnd::Submitted(best) = plan() else {
            panic!("a plan");
        };
        let mut env = Env::new(|| {
            TrialEnd::Submitted(Proposal {
                explanation: "nothing moves".to_owned(),
                actions: Vec::new(),
                omissions: Vec::new(),
            })
        });
        env.candidates = vec![best.clone()];
        let cases: Vec<&Case> = set.cases.iter().take(1).collect();
        let (results, _) = run_cases(&env, &cases, limits(1, 10_000)).await;
        let reported = results[0].trials[0].plan.as_ref().expect("a plan");
        assert_eq!(reported.explanation, best.explanation);
    }

    #[tokio::test]
    async fn a_forbidden_action_in_a_lower_ranked_plan_is_a_violation() {
        let forbid = "expect: { outcome: plan, forbid: { actions: [notify] } }\n";
        let set = EvalSet::parse(MANIFEST, [case("a", "late", forbid)].into_iter().collect())
            .expect("set");
        let TrialEnd::Submitted(clean) = plan() else {
            panic!("a plan");
        };
        let mut env = Env::new(plan);
        env.candidates = vec![Proposal {
            actions: vec![ProposedAction {
                action_type: ActionType("notify".to_owned()),
                target: clean.actions[0].target.clone(),
                params: BTreeMap::new(),
            }],
            ..clean
        }];
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (results, _) = run_cases(&env, &cases, limits(1, 10_000)).await;
        assert_eq!(results[0].trials[0].grade, GradeKind::Violation);
    }

    #[tokio::test]
    async fn a_plan_that_cannot_be_scored_is_errored_not_failed() {
        let set = set();
        let mut env = Env::new(plan);
        env.score_fails = true;
        let cases: Vec<&Case> = set.cases.iter().take(1).collect();
        let (results, _) = run_cases(&env, &cases, limits(1, 10_000)).await;
        let trial = &results[0].trials[0];
        assert_eq!(trial.grade, GradeKind::Errored);
        assert!(trial.reasons[0].contains("simulator is down"), "{trial:?}");
        assert_eq!(env.counts.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_run_without_a_plan_fails_a_case_that_expects_one() {
        let (set, env) = (set(), Env::new(|| TrialEnd::Exhausted));
        let cases: Vec<&Case> = set.cases.iter().take(1).collect();
        let (results, _) = run_cases(&env, &cases, limits(1, 10_000)).await;
        assert_eq!(results[0].trials[0].grade, GradeKind::Fail);
        assert_eq!(results[0].trials[0].end, "exhausted");
    }

    #[tokio::test]
    async fn an_errored_trial_is_attempted_again_and_carries_the_grade_of_the_attempt_that_ran() {
        // The first run errors, every later one hands in the plan.
        let env = Env::scripted(|run| if run == 1 { down() } else { plan() });
        let (case, tokens) = one_case(&env, retrying(2, 2)).await;
        assert_eq!(env.runs(), 3, "one further attempt, then trial 2");
        assert_eq!(tokens, 300, "the errored attempt's tokens count");
        assert!(case.passed_at(2), "{case:?}");
        assert_eq!(
            case.trials[0].errored_attempts,
            ["the answer stalled for 30s"]
        );
        assert_eq!(case.trials[0].end, "submitted");
        assert!(case.trials[1].errored_attempts.is_empty());
    }

    #[tokio::test]
    async fn a_trial_that_errors_on_every_attempt_is_errored_with_the_earlier_reasons() {
        let env = Env::new(down);
        let (case, _) = one_case(&env, retrying(1, 2)).await;
        assert_eq!(env.runs(), 3);
        let trial = &case.trials[0];
        assert_eq!(trial.grade, GradeKind::Errored);
        assert_eq!(trial.reasons, ["the answer stalled for 30s"]);
        assert_eq!(trial.errored_attempts.len(), 2);
        assert_eq!((case.errored(), case.failed()), (1, 0));
    }

    #[tokio::test]
    async fn a_plan_that_cannot_be_scored_is_attempted_again() {
        let mut env = Env::new(plan);
        env.score_fails = true;
        let (case, _) = one_case(&env, retrying(1, 1)).await;
        assert_eq!(env.runs(), 2);
        assert_eq!(case.trials[0].grade, GradeKind::Errored);
        assert!(
            case.trials[0].errored_attempts[0].contains("could not be scored"),
            "{case:?}"
        );
    }

    #[tokio::test]
    async fn a_graded_trial_is_never_repeated() {
        // A fail (no plan where one is expected, and a plan the engine's checks refuse) and a
        // rejection each stand after one run.
        for (end, name) in [
            (TrialEnd::Exhausted, "exhausted"),
            (off_catalog(), "submitted"),
            (TrialEnd::Rejected("no such verb".to_owned()), "rejected"),
        ] {
            let env = Env::scripted(move |_| end.clone());
            let (case, _) = one_case(&env, retrying(1, 2)).await;
            assert_eq!(env.runs(), 1, "{name}");
            let trial = &case.trials[0];
            assert_eq!((trial.grade, trial.end.as_str()), (GradeKind::Fail, name));
            assert!(trial.errored_attempts.is_empty(), "{name}");
            assert!(!case.passed_at(1), "{name}");
        }
    }

    #[tokio::test]
    async fn a_violation_is_never_repeated() {
        let forbid = "expect: { outcome: plan, forbid: { actions: [move] } }\n";
        let set = EvalSet::parse(MANIFEST, [case("a", "late", forbid)].into_iter().collect())
            .expect("set");
        let cases: Vec<&Case> = set.cases.iter().collect();
        let env = Env::new(plan);
        let (results, _) = run_cases(&env, &cases, retrying(1, 2)).await;
        assert_eq!(env.runs(), 1);
        assert_eq!(results[0].trials[0].grade, GradeKind::Violation);
        assert_eq!(results[0].violating(), 1);
    }

    #[tokio::test]
    async fn no_further_attempt_is_made_when_none_is_allowed() {
        let env = Env::new(down);
        let (case, _) = one_case(&env, retrying(1, 0)).await;
        assert_eq!(env.runs(), 1);
        assert_eq!(case.trials[0].grade, GradeKind::Errored);
        assert!(case.trials[0].errored_attempts.is_empty());
    }

    #[tokio::test]
    async fn the_token_bound_stops_a_further_attempt() {
        // 100 tokens a run: the first attempt already reaches the bound.
        let env = Env::new(down);
        let limits = RunLimits {
            retries: 2,
            ..limits(2, 100)
        };
        let (case, tokens) = one_case(&env, limits).await;
        assert_eq!((env.runs(), tokens), (1, 100));
        assert_eq!(case.trials[0].grade, GradeKind::Errored);
        assert!(case.trials[0].errored_attempts.is_empty());
        assert_eq!((case.trials.len(), case.not_run), (1, 1));
    }

    #[tokio::test]
    async fn the_report_is_the_same_whatever_the_concurrency() {
        // The first and the fifth run find no plan, wherever they run.
        let ends = |run: u32| {
            if run % 4 == 1 {
                TrialEnd::Exhausted
            } else {
                plan()
            }
        };
        let set = set();
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (one, three) = (Env::scripted(ends), Env::scripted(ends));
        let sequential = run_cases(&one, &cases, at_once(3, 1, 10_000)).await;
        let concurrent = run_cases(&three, &cases, at_once(3, 3, 10_000)).await;
        assert_eq!(sequential, concurrent);
        assert_eq!(one.counts.most_in_flight.load(Ordering::SeqCst), 1);
        assert_eq!(three.counts.most_in_flight.load(Ordering::SeqCst), 3);
        assert_eq!(
            three.counts.closed.load(Ordering::SeqCst),
            2,
            "one case at a time, each closed"
        );
    }

    #[tokio::test]
    async fn no_more_trials_are_in_flight_than_the_bound() {
        let env = Env::new(plan);
        let (case, _) = one_case(&env, at_once(5, 2, 10_000)).await;
        assert_eq!(env.runs(), 5);
        assert_eq!(env.counts.most_in_flight.load(Ordering::SeqCst), 2);
        assert!(case.passed_at(5));
    }

    #[tokio::test]
    async fn trials_are_reported_in_the_order_of_their_numbers_not_of_their_ending() {
        // The first run takes longest and finds no plan; the two after it end before it.
        let mut env = Env::scripted(|run| {
            if run == 1 {
                TrialEnd::Exhausted
            } else {
                plan()
            }
        });
        env.waits = |run| if run == 1 { 20 } else { 1 };
        let (case, _) = one_case(&env, at_once(3, 3, 10_000)).await;
        let order: Vec<(u32, &str)> = case
            .trials
            .iter()
            .map(|trial| (trial.trial, trial.end.as_str()))
            .collect();
        assert_eq!(
            order,
            [(1, "exhausted"), (2, "submitted"), (3, "submitted")]
        );
    }

    #[tokio::test]
    async fn the_token_bound_lets_trials_in_flight_end_and_starts_no_other() {
        // 100 tokens a run and a bound of 100: two trials start before any has reported.
        let env = Env::new(plan);
        let (case, tokens) = one_case(&env, at_once(4, 2, 100)).await;
        assert_eq!((env.runs(), tokens), (2, 200));
        assert_eq!(env.counts.most_in_flight.load(Ordering::SeqCst), 2);
        assert_eq!((case.passed(), case.not_run), (2, 2));
        assert_eq!(env.counts.closed.load(Ordering::SeqCst), 1);
    }

    /// One case under an id of its own, so its spans can be told from another test's.
    fn one(id: &str) -> EvalSet {
        let plan = "expect: { outcome: plan }\n";
        EvalSet::parse(MANIFEST, [case(id, "late", plan)].into_iter().collect()).expect("set")
    }

    #[tokio::test]
    async fn every_trial_is_a_trace_of_its_own_naming_its_case() {
        use crate::telemetry::testing::{attr, on, with};

        on();
        let set = one("span-three-trials");
        let cases: Vec<&Case> = set.cases.iter().collect();
        let env = Env::new(plan);
        let limits = RunLimits {
            set_version: 7,
            ..at_once(3, 3, 10_000)
        };
        let (results, _) = run_cases(&env, &cases, limits).await;
        assert!(results[0].passed_at(3));

        let spans = with("ekg.eval.case", "span-three-trials");
        assert_eq!(spans.len(), 3, "one per trial");
        let mut traces: Vec<String> = spans
            .iter()
            .map(|span| span.span_context.trace_id().to_string())
            .collect();
        traces.sort();
        traces.dedup();
        assert_eq!(traces.len(), 3, "each trial is a trace of its own");
        let mut trials: Vec<String> = spans
            .iter()
            .map(|span| attr(span, "ekg.eval.trial").expect("trial"))
            .collect();
        trials.sort();
        assert_eq!(trials, ["1", "2", "3"]);
        for span in &spans {
            assert_eq!(span.name, "eval_trial span-three-trials");
            assert_eq!(
                span.parent_span_id,
                opentelemetry::trace::SpanId::INVALID,
                "a root: no span of the run is around it"
            );
            for (key, value) in [
                ("ekg.eval.attempt", "1"),
                ("ekg.eval.arm", "fixed"),
                ("ekg.eval.set_version", "7"),
                ("ekg.eval.grade", "passed"),
                ("ekg.eval.end", "submitted"),
            ] {
                assert_eq!(attr(span, key).as_deref(), Some(value), "{key}");
            }
        }
    }

    #[tokio::test]
    async fn a_retried_trial_is_one_trace_per_attempt() {
        use opentelemetry::trace::Status;

        use crate::telemetry::testing::{attr, on, with};

        on();
        let set = one("span-retried");
        let cases: Vec<&Case> = set.cases.iter().collect();
        // The first run errors, the second submits.
        let env = Env::scripted(|run| if run == 1 { down() } else { plan() });
        let (results, _) = run_cases(&env, &cases, retrying(1, 2)).await;
        assert_eq!(results[0].trials[0].grade, GradeKind::Pass);

        let spans = with("ekg.eval.case", "span-retried");
        assert_eq!(spans.len(), 2, "one per attempt");
        assert_ne!(
            spans[0].span_context.trace_id(),
            spans[1].span_context.trace_id()
        );
        let seen: Vec<(String, String, bool)> = spans
            .iter()
            .map(|span| {
                (
                    attr(span, "ekg.eval.attempt").expect("attempt"),
                    attr(span, "ekg.eval.grade").expect("grade"),
                    matches!(span.status, Status::Error { .. }),
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                ("1".to_owned(), "error".to_owned(), true),
                ("2".to_owned(), "passed".to_owned(), false)
            ]
        );
    }

    fn experiment(arm: &str, narrowing: &Narrowing) -> Experiment {
        let version = VersionTuple {
            engine: "0.1.0".to_owned(),
            model: String::new(),
            skills: BTreeMap::from([("propose-plan".to_owned(), "abc123".to_owned())]),
            tools: BTreeMap::from([(
                "alpha".to_owned(),
                vec!["ground_entity".to_owned(), "submit_plan".to_owned()],
            )]),
            set_version: 7,
            set_digest: "sha256:set".to_owned(),
            trials: 2,
            max_steps: 12,
            joint_max_steps: 8,
            analyst_max_steps: 10,
            planners: 3,
            planner_reads: false,
        };
        let settings = RunSettings {
            concurrency: 2,
            retries: 1,
        };
        Experiment::of(arm, "20271014T120000Z", &version, settings, narrowing)
    }

    #[test]
    fn an_experiment_carries_what_the_run_is_measured_with() {
        let whole = experiment("agent", &Narrowing::default());
        assert_eq!(whole.id, "20271014T120000Z-agent");
        assert_eq!(whole.name, "v7 agent 20271014T120000Z");
        assert_eq!(whole.dataset, "agent-evals-v7");
        for (key, value) in [
            ("arm", "agent"),
            ("engine_version", "0.1.0"),
            ("set_version", "7"),
            ("set_digest", "sha256:set"),
            ("k", "2"),
            ("max_steps", "12"),
            ("joint_max_steps", "8"),
            ("analyst_max_steps", "10"),
            ("planners", "3"),
            ("planner_reads", "false"),
            ("concurrency", "2"),
            ("retries", "1"),
            ("narrowed", "false"),
            ("skill.propose-plan", "abc123"),
            ("tools.alpha", "ground_entity, submit_plan"),
        ] {
            assert_eq!(
                whole.metadata.get(key).map(String::as_str),
                Some(value),
                "{key}"
            );
        }
        assert!(
            !whole.metadata.contains_key("model"),
            "not known before a run"
        );
        assert_eq!(whole.reference().id, whole.id);

        let narrowed = experiment(
            "legacy",
            &Narrowing {
                profiles: vec!["alpha".to_owned()],
                ..Narrowing::default()
            },
        );
        assert_ne!(narrowed.id, whole.id, "one experiment run per arm");
        assert_eq!(narrowed.metadata["narrowed"], "true");
        assert_eq!(narrowed.metadata["narrowed_to.profile"], "alpha");
    }

    #[tokio::test]
    async fn a_marked_run_makes_each_attempt_an_item_of_its_experiment() {
        use crate::telemetry::testing::{attr, on, with};

        on();
        let set = one("span-marked");
        let cases: Vec<&Case> = set.cases.iter().collect();
        let env = Env::new(plan);
        let marked = experiment("agent", &Narrowing::default());
        let limits = RunLimits {
            set_version: 7,
            ..at_once(2, 2, 10_000)
        };
        let (results, _) = run_cases_as(&env, &cases, limits, Some(&marked)).await;
        assert!(results[0].passed_at(2));

        let spans = with("ekg.eval.case", "span-marked");
        assert_eq!(spans.len(), 2);
        for span in &spans {
            for (key, value) in [
                ("langfuse.experiment.id", marked.id.as_str()),
                ("langfuse.experiment.name", marked.name.as_str()),
                ("langfuse.experiment.dataset.id", "agent-evals-v7"),
                ("langfuse.experiment.item.id", "span-marked"),
                ("langfuse.experiment.item.expected_output", "plan"),
                ("langfuse.experiment.item.metadata.category", "late"),
                ("langfuse.experiment.item.metadata.attempt", "1"),
                ("langfuse.experiment.item.metadata.end", "submitted"),
                ("langfuse.experiment.item.metadata.model", "m"),
                ("langfuse.experiment.metadata.model", "m"),
                ("langfuse.experiment.metadata.k", "2"),
                ("langfuse.experiment.metadata.set_digest", "sha256:set"),
                ("langfuse.experiment.metadata.narrowed", "false"),
                ("langfuse.observation.input", "span-marked"),
                ("langfuse.observation.output", "passed"),
            ] {
                assert_eq!(attr(span, key).as_deref(), Some(value), "{key}");
            }
            assert_eq!(
                attr(span, "langfuse.experiment.item.root_observation_id"),
                Some(span.span_context.span_id().to_string()),
                "the item's root is the trace's own root span"
            );
            // Ids, names and grades: nothing of the case's world or of the plan.
            let text = format!("{:?}", span.attributes);
            for content in ["Moved.", "x:sale.order", "sale.order"] {
                assert!(!text.contains(content), "{content:?} in {text}");
            }
        }
        let mut trials: Vec<String> = spans
            .iter()
            .map(|span| attr(span, "langfuse.experiment.item.metadata.trial").expect("trial"))
            .collect();
        trials.sort();
        assert_eq!(trials, ["1", "2"]);
    }

    #[tokio::test]
    async fn an_unmarked_run_carries_no_experiment() {
        use crate::telemetry::testing::{on, with};

        on();
        let set = one("span-unmarked");
        let cases: Vec<&Case> = set.cases.iter().collect();
        let (_, _) = run_cases(&Env::new(plan), &cases, limits(1, 10_000)).await;
        let spans = with("ekg.eval.case", "span-unmarked");
        assert_eq!(spans.len(), 1);
        assert!(
            !spans[0]
                .attributes
                .iter()
                .any(|kv| kv.key.as_str().starts_with("langfuse.")),
            "{:?}",
            spans[0].attributes
        );
    }
}
