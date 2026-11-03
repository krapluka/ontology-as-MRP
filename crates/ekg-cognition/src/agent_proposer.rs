//! The agent proposer: a [`CandidateProposer`] that runs a tool-using agent through the
//! [`Reasoner`] seam, so the slate loop, the simulator, the scorer and the attach gate need
//! not know an agent is behind it.
//!
//! The subject is grounded under the profile's read context and hook, as the bounded agent
//! does, so a candidate carries the same omissions, edges and members. Each run gets a task
//! token naming the profile's tools and the skill pins active at that moment, under a fresh
//! trace id. The plan is the one `submit_plan` accepted in the gateway's [`RunLedger`], never
//! the reasoner's report.
//!
//! A run is one span: it gives the task its trace id, the runtime's spans continue it, and
//! the gateway hangs the task's calls under it. When no trace is exported the ids are random.
//!
//! A run that ends without an accepted plan yields no candidate: an
//! [`EngineError::InvalidProposal`] the slate loop traces as a refused generation.
//!
//! [`GroupAgentProposer`] does this for a group of coupled subjects around the semantic layer. A
//! [`FindingSource`] derives each member's finding; an investigator reads under that member's own
//! profile only where the semantic layer cannot decide, and what it claims gives way to what the
//! semantic layer holds. A slate keeps the findings. With no cause a plan could reach, no model
//! runs. Otherwise each round runs planners side by side, one per strategy, under the profile
//! composed of the members' own and a token naming every member. So reading happens once however
//! many plans a slate tries, no conclusion passes between agents, and the scorer chooses.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ekg_domain::{
    CandidateProposer, EngineError, EntityUri, GroundedProposal, Grounding, GroupCandidateProposer,
    Proposal, Result, ScoredCandidate, SubjectGroup, WorldModel,
};
use opentelemetry::trace::{FutureExt, SpanKind};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use serde::{Deserialize, Serialize};

use crate::agent::GroundedSubject;
use crate::evidence::{evidenced_members, merge_grounding};
use crate::reasoner::{Reasoner, ReasoningTask, Termination};
use crate::telemetry;
use crate::tools::{
    AcceptedPlan, Derived, Finding, FindingOrigin, FindingSource, MAX_TOKEN_TTL, MemberState,
    RunLedger, SUBMIT_PLAN, SkillPin, TaskClaims, TaskGrant, TaskRun, TaskTokens, ToolProfile,
    now_secs,
};

/// Builds the agent's task text from the subject's grounding and the plans already tried (the
/// revise feedback): the subject, what drifted, the plans tried and the skills to load.
/// Supplied by the capability.
pub type TaskBuilder = Arc<dyn Fn(&Grounding, &[ScoredCandidate]) -> String + Send + Sync>;

/// Builds a planner's task text for a group: its members and why they are coupled, their merged
/// grounding, the kept findings, the plans already tried and the skills to load. Supplied by the
/// capability layer.
pub type GroupTaskBuilder =
    Arc<dyn Fn(&SubjectGroup, &Grounding, &[Finding], &[ScoredCandidate]) -> String + Send + Sync>;

/// Builds an analyst's task text from its member and that member's grounding. Supplied by the
/// capability layer.
pub type AnalystTaskBuilder = Arc<dyn Fn(&EntityUri, &Grounding) -> String + Send + Sync>;

/// Says which analyst profile reads a member. Supplied by the capability layer.
pub type AnalystRouter = Arc<dyn Fn(&EntityUri) -> Arc<ToolProfile> + Send + Sync>;

/// Resolves the skill versions currently active for a profile.
#[async_trait]
pub trait SkillPins: Send + Sync {
    /// The pins for a task reading through `profile`.
    ///
    /// # Errors
    /// The registry could not be read.
    async fn pins(&self, profile: &str) -> Result<Vec<SkillPin>>;
}

/// Whether an agent run may go on: asked before every candidate. `false` stops the run
/// before the reasoner is called (the kill switch).
#[async_trait]
pub trait RunGate: Send + Sync {
    /// Whether to start the next candidate's run.
    async fn proceed(&self) -> bool;
}

/// Where each candidate's [`RunRecord`] goes.
pub trait RunRecorder: Send + Sync {
    /// Keep `record`.
    fn record(&self, record: RunRecord);
}

/// One candidate's agent run, as the engine records it: how it ended (from the reasoner), what
/// it did through the gateway (from the ledger), and the skill pins it ran with (from the minted
/// token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// How the run ended.
    pub termination: Termination,
    /// The agent steps it took.
    pub steps: u32,
    /// The model's input tokens.
    pub input_tokens: u64,
    /// The model's output tokens.
    pub output_tokens: u64,
    /// The model that served it, as the runtime reported (empty when the run failed).
    pub model: String,
    /// The run's wall time as the engine measured it, in milliseconds. `#[serde(default)]`, so
    /// a stored record without the field still reads.
    #[serde(default)]
    pub elapsed_ms: u64,
    /// The count of tool calls per tool and outcome, from the gateway's ledger.
    pub tool_calls: BTreeMap<String, BTreeMap<String, u32>>,
    /// The defects of every rejected `submit_plan`, from the gateway's ledger.
    pub rejections: Vec<Vec<String>>,
    /// The skill versions the run's token pinned, by name.
    pub skills: BTreeMap<String, String>,
    /// The tools the run's token allowed (the profile's). `#[serde(default)]`, as above.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The run's trace id.
    pub trace_id: String,
    /// What the agent said last, when the run ended without a plan. Left out when there is
    /// none, so a stored record without the field still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closing_answer: Option<String>,
    /// What the run did for a group. Left out for a point's run, so a stored record without the
    /// field still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RunRole>,
    /// The finding an analyst handed in. Left out otherwise, as above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<Finding>,
}

/// What a run of a group's slate did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    /// It read one member and handed in a finding.
    Analyst,
    /// It planned across the members from their findings.
    Planner,
    /// No run: the semantic layer's finding about a member, recorded with the slate's runs.
    Semantic,
}

/// The per-run limits of an agent proposer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLimits {
    /// The most agent steps one run may take.
    pub max_steps: u32,
    /// How long one run may take before it fails as unavailable.
    pub run_cap: Duration,
}

impl AgentLimits {
    /// How long a run's task token lives: the run's cap and a margin, so the token never
    /// expires under a run that is inside its cap (clamped by the token issuer).
    #[must_use]
    pub fn token_ttl(&self) -> Duration {
        self.run_cap + TOKEN_MARGIN
    }

    /// The longest run cap a task token can outlive.
    #[must_use]
    pub fn longest_run_cap() -> Duration {
        MAX_TOKEN_TTL.saturating_sub(TOKEN_MARGIN)
    }
}

/// How much longer than its run a task token lives.
const TOKEN_MARGIN: Duration = Duration::from_secs(30);

/// A [`RunGate`] that always proceeds.
pub struct AlwaysProceed;

#[async_trait]
impl RunGate for AlwaysProceed {
    async fn proceed(&self) -> bool {
        true
    }
}

/// A [`RunRecorder`] that keeps nothing.
pub struct DiscardRuns;

impl RunRecorder for DiscardRuns {
    fn record(&self, _record: RunRecord) {}
}

/// What an agent run is made of, whoever it plans for: the profile its task reads through and
/// the seams a run composes. Shared by the proposer of a point and the proposer of a group.
#[derive(Clone)]
struct AgentRunner {
    world: Arc<dyn WorldModel>,
    profile: Arc<ToolProfile>,
    tokens: Arc<TaskTokens>,
    reasoner: Arc<dyn Reasoner>,
    ledger: Arc<RunLedger>,
    pins: Arc<dyn SkillPins>,
    limits: AgentLimits,
    gate: Arc<dyn RunGate>,
    recorder: Arc<dyn RunRecorder>,
}

impl AgentRunner {
    /// The tools a task may use: every tool the profile offers.
    fn tools(&self) -> Vec<String> {
        self.profile
            .tool_names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The kill switch: a stopped gate starts no run.
    async fn admit(&self) -> Result<()> {
        if self.gate.proceed().await {
            Ok(())
        } else {
            Err(EngineError::Unavailable(
                "agent run stopped: the rollout gate is closed".to_owned(),
            ))
        }
    }

    /// Mint the task's token: `tools` and `pins` over `subjects`, traced as `trace_id`. Returns
    /// the token and its claims.
    fn mint(
        &self,
        subjects: Vec<EntityUri>,
        tools: Vec<String>,
        pins: Vec<SkillPin>,
        trace_id: &str,
    ) -> Result<(String, TaskClaims)> {
        let grant =
            TaskGrant::new(subjects, tools, self.limits.token_ttl(), trace_id).with_skills(pins);
        let token = self
            .tokens
            .mint(&self.profile, grant)
            .map_err(|e| EngineError::Backend(format!("agent: task token: {e}")))?;
        let claims = self
            .tokens
            .verify_now(&token)
            .map_err(|e| EngineError::Backend(format!("agent: task token: {e}")))?;
        Ok((token, claims))
    }

    /// One agent run for `subjects` with the task text `task`: the plan `submit_plan` accepted
    /// for the run's token. The run is recorded however it ends.
    ///
    /// # Errors
    /// The reasoner's failure; [`EngineError::InvalidProposal`] for a run that ended without an
    /// accepted plan.
    async fn run(&self, subjects: Vec<EntityUri>, task: String) -> Result<AcceptedPlan> {
        let (termination, handed_in) = self.execute(subjects, task, self.tools(), None).await?;
        // The plan of record is the one `submit_plan` accepted, never the runtime's word.
        handed_in.accepted.ok_or_else(|| {
            EngineError::InvalidProposal(format!(
                "agent ended {termination} without an accepted plan"
            ))
        })
    }

    /// One agent run granted `tools`: how it ended, and what it handed in by the gateway's own
    /// record. The run is recorded however it ends, as `role`.
    async fn execute(
        &self,
        subjects: Vec<EntityUri>,
        task: String,
        tools: Vec<String>,
        role: Option<RunRole>,
    ) -> Result<(Termination, HandedIn)> {
        // Before the token: a token minted now would spend its lifetime waiting.
        let permit = self.reasoner.admit().await?;
        let pins = self.pins.pins(self.profile.id()).await?;
        let skills: BTreeMap<String, String> = pins
            .iter()
            .map(|pin| (pin.name.clone(), pin.version.clone()))
            .collect();
        // The run's span, under whatever the caller is traced by. It is opened before the
        // token is minted, because the token carries the trace id the span decides.
        let run = telemetry::child(
            &Context::current(),
            format!("invoke_agent {}", self.profile.id()),
            SpanKind::Client,
        );
        let (trace_id, parent_span_id) = telemetry::task_ids(&run)?;
        let (token, claims) = self.mint(subjects, tools.clone(), pins, &trace_id)?;
        let jti = claims.jti.clone();
        if let Some(span) = telemetry::span_context(&run) {
            self.ledger.open(&claims, span, now_secs().unwrap_or(0));
        }
        let task = ReasoningTask {
            task,
            token,
            trace_id: trace_id.clone(),
            parent_span_id,
            max_steps: self.limits.max_steps,
            run_cap: self.limits.run_cap,
            permit,
        };
        let started = std::time::Instant::now();
        let report = self.reasoner.reason(&task).with_context(run.clone()).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // What the task did, from the gateway's own record. `take` removes the entry, so read
        // it once here, whatever the report says.
        let TaskRun {
            tool_calls,
            rejections,
            accepted,
            finding,
            ..
        } = self.ledger.take(&jti).unwrap_or_default();
        let (termination, steps, input_tokens, output_tokens, model, closing_answer) = match &report
        {
            Ok(report) => (
                report.termination,
                report.steps,
                report.input_tokens,
                report.output_tokens,
                report.model.clone(),
                report.closing_answer.clone(),
            ),
            Err(_) => (Termination::Error, 0, 0, 0, String::new(), None),
        };
        let record = RunRecord {
            termination,
            steps,
            input_tokens,
            output_tokens,
            model,
            elapsed_ms,
            tool_calls,
            rejections,
            skills,
            tools,
            trace_id,
            closing_answer,
            role,
            finding: finding.clone(),
        };
        telemetry::finish(
            &run,
            run_attributes(self.profile.id(), self.limits.max_steps, &record),
            report.is_ok(),
            report.as_ref().err().map(error_class),
        );
        self.recorder.record(record);
        report?;
        Ok((termination, HandedIn { accepted, finding }))
    }
}

/// What a run handed in, by the gateway's own record.
struct HandedIn {
    accepted: Option<AcceptedPlan>,
    finding: Option<Finding>,
}

/// The candidate an accepted `plan` makes over `grounding`.
fn candidate(
    plan: AcceptedPlan,
    grounding: Grounding,
    subject_members: Vec<EntityUri>,
) -> GroundedProposal {
    GroundedProposal {
        proposal: Proposal {
            explanation: plan.explanation,
            actions: plan.actions,
            omissions: grounding.omissions.clone(),
        },
        grounding_omissions: grounding.omissions,
        grounding_edges: grounding.edges,
        subject_members,
    }
}

/// A capability-neutral [`CandidateProposer`] backed by a tool-using agent run.
#[derive(Clone)]
pub struct AgentProposer {
    runner: AgentRunner,
    task: TaskBuilder,
}

impl AgentProposer {
    /// Wire the proposer: it grounds `world` and mints tokens for `profile` with `tokens`, runs
    /// `reasoner`, reads the plan from `ledger` (the gateway's), pins skills from `pins` and
    /// builds the task with `task`. It proceeds always and records nothing until
    /// [`Self::with_gate`] and [`Self::with_recorder`] say otherwise.
    #[allow(clippy::too_many_arguments)] // the seams a run composes, each injected
    #[must_use]
    pub fn new(
        world: Arc<dyn WorldModel>,
        profile: Arc<ToolProfile>,
        tokens: Arc<TaskTokens>,
        reasoner: Arc<dyn Reasoner>,
        ledger: Arc<RunLedger>,
        pins: Arc<dyn SkillPins>,
        task: TaskBuilder,
        limits: AgentLimits,
    ) -> Self {
        Self {
            runner: AgentRunner {
                world,
                profile,
                tokens,
                reasoner,
                ledger,
                pins,
                limits,
                gate: Arc::new(AlwaysProceed),
                recorder: Arc::new(DiscardRuns),
            },
            task,
        }
    }

    /// Ask `gate` before every candidate.
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn RunGate>) -> Self {
        self.runner.gate = gate;
        self
    }

    /// Record each candidate's run with `recorder`.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn RunRecorder>) -> Self {
        self.runner.recorder = recorder;
        self
    }

    /// The profile the proposer's tasks read through.
    #[must_use]
    pub fn profile(&self) -> &ToolProfile {
        &self.runner.profile
    }
}

/// The class of a failed run, as its span names it.
fn error_class(err: &EngineError) -> &'static str {
    match err {
        EngineError::RateLimited(_) => "rate_limited",
        EngineError::Unavailable(_) => "unavailable",
        _ => "error",
    }
}

/// What a run's span says about it: how it ended, its counts, the model, and the versions it
/// ran with. Names and numbers only.
fn run_attributes(profile: &str, max_steps: u32, record: &RunRecord) -> Vec<KeyValue> {
    let pins: Vec<StringValue> = record
        .skills
        .iter()
        .map(|(name, version)| format!("{name}@{version}").into())
        .collect();
    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let mut attributes = vec![
        KeyValue::new("gen_ai.operation.name", "invoke_agent"),
        KeyValue::new("gen_ai.agent.name", profile.to_owned()),
        KeyValue::new("gen_ai.usage.input_tokens", count(record.input_tokens)),
        KeyValue::new("gen_ai.usage.output_tokens", count(record.output_tokens)),
        KeyValue::new("ekg.agent.termination", record.termination.to_string()),
        KeyValue::new("ekg.agent.steps", i64::from(record.steps)),
        KeyValue::new("ekg.agent.max_steps", i64::from(max_steps)),
        KeyValue::new(
            "ekg.agent.rejections",
            count(record.rejections.len() as u64),
        ),
        KeyValue::new("ekg.engine.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("ekg.skill.pins", Value::Array(Array::String(pins))),
    ];
    if !record.model.is_empty() {
        attributes.push(KeyValue::new("gen_ai.response.model", record.model.clone()));
    }
    attributes
}

#[async_trait]
impl CandidateProposer for AgentProposer {
    async fn propose_candidate(
        &self,
        subject: &EntityUri,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        self.runner.admit().await?;
        // The same grounding as the bounded agent: same omissions, edges and members.
        let GroundedSubject {
            grounding,
            extended,
        } = self
            .runner
            .profile
            .reader(Arc::clone(&self.runner.world))
            .ground_extended(subject)
            .await?;
        let task = (self.task)(&grounding, feedback);
        let plan = self.runner.run(vec![subject.clone()], task).await?;
        let subject_members = evidenced_members(extended, &grounding);
        Ok(candidate(plan, grounding, subject_members))
    }
}

/// How a group's members are analysed: the semantic layer, and for what it cannot decide the
/// profile an investigator reads under, its task and its limits. Supplied by the capability
/// layer, so the proposer names no capability.
#[derive(Clone)]
pub struct MemberAnalysts {
    /// What derives findings from the semantic layer.
    pub source: Arc<dyn FindingSource>,
    /// The analyst profile of a member.
    pub profile: AnalystRouter,
    /// The analyst's task.
    pub task: AnalystTaskBuilder,
    /// The limits of an analyst run.
    pub limits: AgentLimits,
}

/// A capability-neutral [`GroupCandidateProposer`] backed by tool-using agent runs: the analysts
/// of the members a slate has no finding for, then one planner across them. Its profile is
/// composed of the members' own, so each member is grounded under its own capability's read
/// context, as the joint prompt path grounds it.
///
/// A proposer keeps its findings, so one instance serves one slate; [`Self::fresh`] gives a
/// clone none of them.
#[derive(Clone)]
pub struct GroupAgentProposer {
    runner: AgentRunner,
    analysts: MemberAnalysts,
    task: GroupTaskBuilder,
    findings: Arc<Mutex<BTreeMap<EntityUri, Finding>>>,
    strategies: Vec<String>,
    planner_reads: bool,
}

impl GroupAgentProposer {
    /// Wire the proposer as [`AgentProposer::new`], over a composed `profile`, a planner's task
    /// built for the group, and the group's `analysts`.
    #[allow(clippy::too_many_arguments)] // the seams a run composes, each injected
    #[must_use]
    pub fn new(
        world: Arc<dyn WorldModel>,
        profile: Arc<ToolProfile>,
        tokens: Arc<TaskTokens>,
        reasoner: Arc<dyn Reasoner>,
        ledger: Arc<RunLedger>,
        pins: Arc<dyn SkillPins>,
        task: GroupTaskBuilder,
        limits: AgentLimits,
        analysts: MemberAnalysts,
    ) -> Self {
        Self {
            runner: AgentRunner {
                world,
                profile,
                tokens,
                reasoner,
                ledger,
                pins,
                limits,
                gate: Arc::new(AlwaysProceed),
                recorder: Arc::new(DiscardRuns),
            },
            analysts,
            task,
            findings: Arc::new(Mutex::new(BTreeMap::new())),
            strategies: Vec::new(),
            planner_reads: false,
        }
    }

    /// Run one planner per strategy in a round, instead of one planner with none.
    #[must_use]
    pub fn with_strategies(mut self, strategies: Vec<String>) -> Self {
        self.strategies = strategies;
        self
    }

    /// Give planners the composed profile's read tools as well.
    #[must_use]
    pub fn with_planner_reads(mut self, reads: bool) -> Self {
        self.planner_reads = reads;
        self
    }

    /// How many planners a full round runs.
    #[must_use]
    pub fn planners(&self) -> usize {
        self.strategies.len().max(1)
    }

    /// A proposer like this one that holds no finding, for a slate of its own.
    #[must_use]
    pub fn fresh(mut self) -> Self {
        self.findings = Arc::new(Mutex::new(BTreeMap::new()));
        self
    }

    /// Ask `gate` before every candidate.
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn RunGate>) -> Self {
        self.runner.gate = gate;
        self
    }

    /// Record each candidate's run with `recorder`.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn RunRecorder>) -> Self {
        self.runner.recorder = recorder;
        self
    }

    /// The profile the proposer's tasks read through.
    #[must_use]
    pub fn profile(&self) -> &ToolProfile {
        &self.runner.profile
    }

    /// The finding kept for `member`.
    fn kept(&self, member: &EntityUri) -> Option<Finding> {
        self.findings.lock().ok()?.get(member).cloned()
    }

    /// Keep `finding` for `member`.
    fn keep(&self, member: &EntityUri, finding: Finding) {
        if let Ok(mut findings) = self.findings.lock() {
            findings.insert(member.clone(), finding);
        }
    }

    /// Record `finding` with the slate's runs, as the engine's.
    fn record_finding(&self, finding: &Finding) {
        self.runner.recorder.record(RunRecord {
            termination: Termination::Submitted,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            model: String::new(),
            elapsed_ms: 0,
            tool_calls: BTreeMap::new(),
            rejections: Vec::new(),
            skills: BTreeMap::new(),
            tools: Vec::new(),
            trace_id: String::new(),
            closing_answer: None,
            role: Some(RunRole::Semantic),
            finding: Some(finding.clone()),
        });
    }

    /// Run the investigator of `member`, which the semantic layer could not decide for the
    /// reason `why`, and keep what it claims and the semantic layer does not contradict. Fails
    /// when it handed in nothing, naming the member and how the run ended.
    async fn investigate(&self, member: &EntityUri, why: &str) -> Result<()> {
        self.runner.admit().await?;
        let profile = (self.analysts.profile)(member);
        let analyst = AgentRunner {
            profile,
            limits: self.analysts.limits,
            ..self.runner.clone()
        };
        let GroundedSubject { grounding, .. } = analyst
            .profile
            .reader(Arc::clone(&analyst.world))
            .ground_extended(member)
            .await?;
        let task = format!(
            "{}\n\nThe records alone do not settle this member: {why}",
            (self.analysts.task)(member, &grounding)
        );
        let tools = analyst.tools();
        let (termination, handed_in) = analyst
            .execute(vec![member.clone()], task, tools, Some(RunRole::Analyst))
            .await?;
        let Some(mut finding) = handed_in.finding else {
            return Err(EngineError::InvalidProposal(format!(
                "the investigator of {} ended {termination} without a finding",
                member.0
            )));
        };
        // Decided here, not left to a planner: a claim the records contradict is never shown.
        let conflicts = self.analysts.source.conflicts(member, &finding).await?;
        if !conflicts.is_empty() {
            let (dropped, kept) = std::mem::take(&mut finding.causes)
                .into_iter()
                .partition(|cause| conflicts.contains(&cause.kind));
            finding.causes = kept;
            finding.dropped = dropped;
            self.record_finding(&finding);
        }
        self.keep(member, finding);
        Ok(())
    }

    /// What is known about `member`, from the semantic layer or, where it cannot decide, from
    /// an investigator.
    async fn find(&self, member: &EntityUri) -> Result<()> {
        match self.analysts.source.derive(member).await? {
            Derived::Decided(finding) => {
                self.record_finding(&finding);
                self.keep(member, finding);
                Ok(())
            }
            Derived::Undetermined(why) => self.investigate(member, &why).await,
        }
    }

    /// Every member's finding, in member order: the kept ones, and the rest found side by side.
    async fn findings_of(&self, members: &[EntityUri]) -> Result<Vec<Finding>> {
        let missing = members.iter().filter(|member| self.kept(member).is_none());
        let outcomes = futures::future::join_all(missing.map(|member| self.find(member))).await;
        outcomes.into_iter().collect::<Result<Vec<()>>>()?;
        members
            .iter()
            .map(|member| {
                self.kept(member).ok_or_else(|| {
                    EngineError::InvalidProposal(format!("{} has no finding", member.0))
                })
            })
            .collect()
    }

    /// The tools a planner is granted.
    fn planner_tools(&self) -> Vec<String> {
        if self.planner_reads {
            self.runner.tools()
        } else {
            vec![SUBMIT_PLAN.to_owned()]
        }
    }

    /// One planner run: the plan `submit_plan` accepted for its token.
    async fn plan(&self, group: &SubjectGroup, task: String) -> Result<AcceptedPlan> {
        // The kill switch may have been set while the findings were read.
        self.runner.admit().await?;
        let (termination, handed_in) = self
            .runner
            .execute(
                group.members.clone(),
                task,
                self.planner_tools(),
                Some(RunRole::Planner),
            )
            .await?;
        // `submit_plan` refused any target outside the task's scope, which is these members.
        handed_in.accepted.ok_or_else(|| {
            EngineError::InvalidProposal(format!(
                "agent ended {termination} without an accepted plan"
            ))
        })
    }

    /// The strategies of a round of at most `room` planners, continuing where the plans already
    /// tried left off, so that a later round does not repeat the first's opening.
    fn round_strategies(&self, tried: usize, room: usize) -> Vec<Option<&str>> {
        if self.strategies.is_empty() {
            return vec![None];
        }
        (0..room.min(self.strategies.len()))
            .map(|n| {
                self.strategies
                    .get((tried + n) % self.strategies.len())
                    .map(String::as_str)
            })
            .collect()
    }

    /// Ground every member as its own capability grounds it, and merge. The merged grounding is
    /// about the first member; its edges carry their own subjects.
    async fn evidence(&self, group: &SubjectGroup) -> Result<(Grounding, Vec<EntityUri>)> {
        let Some(first) = group.members.first() else {
            return Err(EngineError::InvalidProposal(format!(
                "group {} has no members",
                group.key
            )));
        };
        let reader = self.runner.profile.reader(Arc::clone(&self.runner.world));
        let mut merged = Grounding {
            entity: first.clone(),
            edges: Vec::new(),
            omissions: Vec::new(),
        };
        let mut members: Vec<EntityUri> = Vec::new();
        for member in &group.members {
            let grounded = reader.ground_extended(member).await?;
            if !members.contains(member) {
                members.push(member.clone());
            }
            for entity in evidenced_members(grounded.extended, &grounded.grounding) {
                if !members.contains(&entity) {
                    members.push(entity);
                }
            }
            merge_grounding(&mut merged, grounded.grounding);
        }
        Ok((merged, members))
    }

    /// The findings, past the gate: an error when no member has a cause a plan could reach.
    async fn reachable_findings(&self, group: &SubjectGroup) -> Result<Vec<Finding>> {
        let findings = self.findings_of(&group.members).await?;
        if findings.iter().any(|f| f.state == MemberState::AtRisk) {
            return Ok(findings);
        }
        let states = findings
            .iter()
            .map(|finding| {
                let from = match finding.origin {
                    FindingOrigin::Semantic => "records",
                    FindingOrigin::Claim => "claim",
                };
                format!("{} {:?} ({from})", finding.member.0, finding.state)
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(EngineError::NothingToDo(format!(
            "no member has a cause an action reaches: {states}"
        )))
    }
}

#[async_trait]
impl GroupCandidateProposer for GroupAgentProposer {
    async fn propose_group_candidate(
        &self,
        group: &SubjectGroup,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        self.propose_group_round(group, feedback, 1)
            .await
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                Err(EngineError::InvalidProposal(
                    "a round drafted no candidate".to_owned(),
                ))
            })
    }

    async fn propose_group_round(
        &self,
        group: &SubjectGroup,
        feedback: &[ScoredCandidate],
        room: usize,
    ) -> Vec<Result<GroundedProposal>> {
        if let Err(stopped) = self.runner.admit().await {
            return vec![Err(stopped)];
        }
        let (merged, members) = match self.evidence(group).await {
            Ok(evidence) => evidence,
            Err(failed) => return vec![Err(failed)],
        };
        let findings = match self.reachable_findings(group).await {
            Ok(findings) => findings,
            Err(failed) => return vec![Err(failed)],
        };
        let task = (self.task)(group, &merged, &findings, feedback);
        let planners = self
            .round_strategies(feedback.len(), room)
            .into_iter()
            .map(|strategy| {
                let task = match strategy {
                    Some(strategy) => format!("Your approach: {strategy}\n\n{task}"),
                    None => task.clone(),
                };
                self.plan(group, task)
            });
        futures::future::join_all(planners)
            .await
            .into_iter()
            .map(|plan| plan.map(|plan| candidate(plan, merged.clone(), members.clone())))
            .collect()
    }
}
