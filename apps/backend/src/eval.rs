//! The offline eval of the tool-using agent: each case of the eval set in a throwaway world,
//! the agent run through the real gateway and runtime, graded by the simulator and the
//! capability's scorer. The bounded prompt agent, which produces today's proposals, runs on
//! the same cases as the legacy arm. A joint case runs the agent once for all its members under
//! the portfolio profile, and the joint prompt as the legacy arm, graded by the portfolio's
//! scorer.
//!
//! The neutral parts (the set, the graders, pass^k) are
//! [`ekg_cognition::eval`]. This module is the wiring only the backend can do: which
//! capability a case's profile is, the world a case's records make, and the command line of
//! the `agent-eval` binary.
//!
//! An eval reads no live state. Its worlds are repositories of their own (`eval_*`), read
//! under the checked-in default read policy; the stack's repository, the action points, the
//! rollout mode and the shadow store are never touched, and nothing is written back anywhere.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use ekg_cognition::eval::{
    AgentArm, Arm, Case, CaseEnv, EvalSet, Lock, Narrowing, OpenCase, PlansFor, PromptArm, Report,
    ResolvedCase, Trial, VersionTuple,
};
use ekg_cognition::tools::{
    GatewayConfig, PlanCatalog, ProfileRegistry, RunLedger, TaskScope, TaskTokens, ToolProfile,
    gateway_router,
};
use ekg_cognition::{
    AgentLimits, AgentProposer, BoundedAgent, CognitionRoute, GroupAgent, GroupAgentProposer,
    GroupTaskBuilder, MemberAnalysts, Reasoner, TaskBuilder,
};
use ekg_domain::{
    CandidateProposer, CognitionProvider, EngineError, EntityUri, Grounding,
    GroupCandidateProposer, GroupScorer, Plan, ReadContext, ReadPolicy, Result as EngineResult,
    Revision, Score, ScoredCandidate, Scorer, Simulator, SourceRecord, SubjectGroup, WorldModel,
};
use ekg_semantic::fixture::{self, FixtureWorld};
use ekg_semantic::{
    GraphDbClient, GraphDbSimulator, ProcessContentionScorer, SupplyRiskScorer, ontology,
};
use tokio::sync::Semaphore;

use crate::IngestionConfig;
use crate::joint::{self, PortfolioScorer};
use crate::propose::{
    self, PORTFOLIO_PROFILE, PROCESS_ANALYST_PROFILE, PROCESS_PROFILE, SUPPLY_RISK_ANALYST_PROFILE,
    SUPPLY_RISK_PROFILE,
};
use crate::skills::DirSkills;

/// The prefix of every eval world's repository.
pub const EVAL_REPO_PREFIX: &str = "eval_";
/// How old an eval repository must be before a new run reclaims it: longer than any case
/// holds its world.
pub const STALE_EVAL_REPO_AGE: Duration = Duration::from_secs(3600);
/// The source system a case's records are attributed to.
const SOURCE_SYSTEM: &str = "odoo";

/// The trials per case when none is given.
pub const DEFAULT_TRIALS: u32 = 3;
/// The token bound when none is given.
pub const DEFAULT_MAX_TOKENS: u64 = 2_000_000;
/// The further attempts of an errored trial when none is given.
pub const DEFAULT_RETRIES: u32 = 2;
/// The trials of a case in flight at once when none is given.
pub const DEFAULT_CONCURRENCY: u32 = 3;
/// The repositories a GraphDB server holds when none is given: what GraphDB Free allows.
pub const DEFAULT_MAX_REPOSITORIES: usize = 5;
/// The port the eval's gateway listens on when none is given.
pub const DEFAULT_GATEWAY_PORT: u16 = 8791;
/// Where the eval reaches the runtime when no URL is given.
pub const DEFAULT_RUNTIME_URL: &str = "http://localhost:1417";

/// The engine's settings as checked in, with only `passthrough` taken from `env`. The read
/// policy in particular is the default one, so a case that hides a fact hides the same fact
/// on every machine.
///
/// # Errors
/// The name of a passed-through variable whose value does not parse.
pub fn engine_defaults(
    env: &BTreeMap<String, String>,
    passthrough: &[&str],
) -> Result<IngestionConfig, String> {
    // The two settings the engine cannot start without; an eval uses neither.
    let mut vars: BTreeMap<String, String> = [("EKG_WEBHOOK_SECRET", "-"), ("ODOO_API_KEY", "-")]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    for key in passthrough {
        if let Some(value) = env.get(*key).filter(|value| !value.trim().is_empty()) {
            vars.insert((*key).to_owned(), value.clone());
        }
    }
    IngestionConfig::from_map(&vars)
}

/// The world model the gateway and the agent read: whichever case is open. Read-only: an
/// eval materializes a case's records before it opens the case, never through this.
#[derive(Default)]
pub struct SwitchWorld {
    current: RwLock<Option<Arc<dyn WorldModel>>>,
}

impl SwitchWorld {
    fn set(&self, world: Option<Arc<dyn WorldModel>>) {
        match self.current.write() {
            Ok(mut current) => *current = world,
            Err(poisoned) => *poisoned.into_inner() = world,
        }
    }

    fn current(&self) -> EngineResult<Arc<dyn WorldModel>> {
        let current = match self.current.read() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        current.ok_or_else(|| EngineError::Backend("no eval case is open".to_owned()))
    }
}

#[async_trait]
impl WorldModel for SwitchWorld {
    async fn materialize(&self, _record: &SourceRecord) -> EngineResult<Revision> {
        Err(EngineError::Backend(
            "an eval's world is read-only".to_owned(),
        ))
    }

    async fn retract(&self, _entity: &EntityUri) -> EngineResult<Revision> {
        Err(EngineError::Backend(
            "an eval's world is read-only".to_owned(),
        ))
    }

    async fn ground(&self, entity: &EntityUri, ctx: &ReadContext) -> EngineResult<Grounding> {
        self.current()?.ground(entity, ctx).await
    }

    async fn select(&self, sparql: &str) -> EngineResult<Vec<BTreeMap<String, String>>> {
        self.current()?.select(sparql).await
    }
}

/// A provider that keeps its last reply, so a refused reply can be told from one that
/// proposed nothing. One per trial: trials that run at the same time must not read each
/// other's reply.
pub struct LastReply {
    inner: Arc<dyn CognitionProvider>,
    last: Mutex<Option<String>>,
}

impl LastReply {
    /// Wrap `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn CognitionProvider>) -> Self {
        Self {
            inner,
            last: Mutex::new(None),
        }
    }

    fn last(&self) -> Option<String> {
        match self.last.lock() {
            Ok(last) => last.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Whether the last reply proposed no action at all. A reply that was never given
    /// proposed nothing to decline with.
    #[must_use]
    pub fn declined(&self) -> bool {
        self.last()
            .is_some_and(|reply| propose::action_blocks(&reply).is_empty())
    }
}

#[async_trait]
impl CognitionProvider for LastReply {
    async fn generate(&self, prompt: &str) -> EngineResult<String> {
        let reply = self.inner.generate(prompt).await?;
        match self.last.lock() {
            Ok(mut last) => *last = Some(reply.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(reply.clone()),
        }
        Ok(reply)
    }

    async fn embed(&self, text: &str) -> EngineResult<Vec<f32>> {
        self.inner.embed(text).await
    }
}

/// Builds a profile's bounded prompt agent over a world, a provider and a plan bound.
type LegacyAgent = fn(Arc<dyn WorldModel>, Arc<dyn CognitionProvider>, usize) -> BoundedAgent;

/// Builds the proposer of one legacy trial over that trial's provider: a subject's, or a
/// group's for a joint case.
enum LegacyProposer {
    Subject(Over<dyn CandidateProposer>),
    Group(Over<dyn GroupCandidateProposer>),
}

/// Builds a proposer of kind `P` over one trial's provider.
type Over<P> = Box<dyn Fn(Arc<dyn CognitionProvider>) -> Arc<P> + Send + Sync>;

/// The legacy arm of a case: every trial gets a proposer and a [`LastReply`] of its own over
/// the one provider.
struct LegacyArm {
    provider: Arc<dyn CognitionProvider>,
    proposer: LegacyProposer,
}

#[async_trait]
impl Arm for LegacyArm {
    fn name(&self) -> &'static str {
        ArmKind::Legacy.name()
    }

    async fn trial(&self, plans_for: &PlansFor, tried: &[ScoredCandidate]) -> Trial {
        let last = Arc::new(LastReply::new(Arc::clone(&self.provider)));
        let provider = Arc::clone(&last) as Arc<dyn CognitionProvider>;
        let declined = Arc::new(move || last.declined());
        let arm = match &self.proposer {
            LegacyProposer::Subject(build) => PromptArm::new(build(provider), declined),
            LegacyProposer::Group(build) => PromptArm::for_group(build(provider), declined),
        };
        arm.trial(plans_for, tried).await
    }
}

/// Run `work` under one of `permits`.
async fn bounded<T>(permits: &Semaphore, work: impl Future<Output = T>) -> Result<T, String> {
    let _permit = permits
        .acquire()
        .await
        .map_err(|why| format!("the scoring permits are closed: {why}"))?;
    Ok(work.await)
}

/// The trials of a case that may be in flight at once on a server that holds `limit`
/// repositories and has `existing` ones: `asked`, or fewer when the server has no room for
/// them. A case in flight needs its world and one simulation per plan being scored.
///
/// # Errors
/// A message naming the limit and the repositories that occupy the server, when there is no
/// room for a case's world and one simulation.
pub fn trial_bound(limit: usize, existing: &[String], asked: u32) -> Result<u32, String> {
    let room = limit.saturating_sub(existing.len());
    if room < 2 {
        return Err(format!(
            "the world-model server holds at most {limit} repositories and has {}: {}; a case \
             needs two free (its world and one simulation)",
            existing.len(),
            existing.join(", ")
        ));
    }
    let simulations = u32::try_from(room - 1).unwrap_or(u32::MAX);
    Ok(asked.clamp(1, simulations))
}

/// The repositories the GraphDB server holds: `EKG_GRAPHDB_MAX_REPOSITORIES`, or what GraphDB
/// Free allows.
///
/// # Errors
/// A message when the setting is not a positive number.
pub fn max_repositories(env: &BTreeMap<String, String>) -> Result<usize, String> {
    const KEY: &str = "EKG_GRAPHDB_MAX_REPOSITORIES";
    match env.get(KEY).filter(|value| !value.trim().is_empty()) {
        Some(value) => positive(KEY, value),
        None => Ok(DEFAULT_MAX_REPOSITORIES),
    }
}

/// Drop the eval repositories a killed run left behind on `server`, returning their ids.
///
/// # Errors
/// A message when the server's repositories cannot be listed.
pub async fn sweep_stale(server: &GraphDbClient) -> Result<Vec<String>, String> {
    fixture::sweep_stale(server, EVAL_REPO_PREFIX, STALE_EVAL_REPO_AGE)
        .await
        .map_err(|e| e.to_string())
}

/// `config` for the legacy arm: its calls go through the runtime at `runtime`, as many at
/// once as trials are in flight.
#[must_use]
pub fn legacy_config(config: &IngestionConfig, runtime: &str, concurrency: u32) -> IngestionConfig {
    let mut routed = config.clone();
    routed.cognition_route = CognitionRoute::Runtime {
        url: runtime.to_owned(),
    };
    routed.cognition_limits.max_concurrent = permits(concurrency);
    routed
}

/// One permit per trial in flight.
#[must_use]
pub fn permits(concurrency: u32) -> NonZeroUsize {
    usize::try_from(concurrency)
        .ok()
        .and_then(NonZeroUsize::new)
        .unwrap_or(NonZeroUsize::MIN)
}

/// What a profile's cases run with: its catalog, and what its trials plan for.
struct Capability {
    profile: Arc<ToolProfile>,
    action_types: Vec<String>,
    plans: Plans,
}

/// What a capability's trials run and are scored with.
enum Plans {
    /// One subject: the capability's task text, prompt agent and scorer.
    Subject {
        task: TaskBuilder,
        legacy: LegacyAgent,
        scorer: Arc<dyn Scorer>,
    },
    /// A portfolio's members at once: the joint task text and the portfolio's scorer. Its
    /// legacy arm is the joint prompt.
    Group {
        task: GroupTaskBuilder,
        analysts: MemberAnalysts,
        scorer: Arc<dyn GroupScorer>,
    },
}

fn action_types(catalog: &dyn PlanCatalog) -> Vec<String> {
    catalog
        .action_types()
        .into_iter()
        .map(|spec| spec.action_type)
        .collect()
}

/// The profile ids an eval case may run under.
pub const EVAL_PROFILES: [&str; 3] = [SUPPLY_RISK_PROFILE, PROCESS_PROFILE, PORTFOLIO_PROFILE];
/// Those of [`EVAL_PROFILES`] a joint case runs under: the ones composed of the others.
pub const EVAL_JOINT_PROFILES: [&str; 1] = [PORTFOLIO_PROFILE];

/// The eval's environment: one gateway and one agent wiring over a [`SwitchWorld`], and a
/// throwaway world per case.
pub struct EvalEnv {
    server: GraphDbClient,
    switch: Arc<SwitchWorld>,
    world: Arc<dyn WorldModel>,
    profiles: Arc<ProfileRegistry>,
    tokens: Arc<TaskTokens>,
    ledger: Arc<RunLedger>,
    skills: Arc<DirSkills>,
    reasoner: Arc<dyn Reasoner>,
    limits: AgentLimits,
    /// The limits of a joint case's run.
    joint_limits: AgentLimits,
    analyst_limits: AgentLimits,
    planners: usize,
    planner_reads: bool,
    /// The most characters of a record's text a tool hands on.
    text_max_chars: usize,
    max_plan_actions: usize,
    read_policy: ReadPolicy,
    /// The legacy arm's provider, when this environment builds that arm instead of the
    /// agent's.
    legacy: Option<Arc<dyn CognitionProvider>>,
    /// Bounds the plans being scored at once, each of which holds a simulation repository.
    scoring: Arc<Semaphore>,
}

impl EvalEnv {
    /// Wire the environment: worlds on `server`'s GraphDB, the agent through `reasoner`, the
    /// skills of `skills_dir` as they are on disk, and `config`'s read policy, plan bound and
    /// run limits: a joint case's run gets the limits of a portfolio's, any other a point's.
    ///
    /// # Errors
    /// A message when the profiles, the token secret or the skills cannot be set up.
    pub fn new(
        server: GraphDbClient,
        reasoner: Arc<dyn Reasoner>,
        skills_dir: &std::path::Path,
        config: &IngestionConfig,
    ) -> Result<Self, String> {
        let switch = Arc::new(SwitchWorld::default());
        let world = Arc::clone(&switch) as Arc<dyn WorldModel>;
        let profiles =
            propose::tool_profiles(&world, config.max_plan_actions).map_err(|e| e.to_string())?;
        let skills = DirSkills::read(skills_dir, &profiles.offered_tools())?;
        Ok(Self {
            server,
            switch,
            world,
            profiles: Arc::new(profiles),
            tokens: Arc::new(TaskTokens::random().map_err(|e| e.to_string())?),
            ledger: Arc::new(RunLedger::default()),
            skills: Arc::new(skills),
            reasoner,
            limits: config.agent_limits(),
            joint_limits: config.agent_joint_limits(),
            analyst_limits: config.agent_analyst_limits(),
            planners: config.agent_planners,
            planner_reads: config.agent_planner_reads,
            text_max_chars: config.agent_text_max_chars,
            max_plan_actions: config.max_plan_actions,
            read_policy: config.read_policy(),
            legacy: None,
            scoring: Arc::new(Semaphore::new(1)),
        })
    }

    /// The same environment, scoring up to `concurrency` plans at once: as many as trials
    /// are in flight.
    #[must_use]
    pub fn scoring_at_most(mut self, concurrency: u32) -> Self {
        self.scoring = Arc::new(Semaphore::new(permits(concurrency).get()));
        self
    }

    /// The same environment, building the legacy arm: each case's trials are one call of
    /// `provider` through the profile's bounded prompt agent.
    #[must_use]
    pub fn with_legacy_arm(mut self, provider: Arc<dyn CognitionProvider>) -> Self {
        self.legacy = Some(provider);
        self
    }

    /// The arm this environment builds for a case.
    #[must_use]
    pub fn arm(&self) -> ArmKind {
        if self.legacy.is_some() {
            ArmKind::Legacy
        } else {
            ArmKind::Agent
        }
    }

    /// The tool gateway the runtime reads through, also reachable as `allowed_host`.
    pub fn gateway(&self, allowed_host: &str) -> axum::Router {
        gateway_router(
            Arc::clone(&self.profiles),
            Arc::clone(&self.tokens),
            // Every call is in the run ledger and so in the report; a line per call on the
            // terminal would bury the per-case results.
            GatewayConfig::default()
                .allow_host(allowed_host)
                .with_call_log(Arc::new(|_| {}))
                .with_skill_log(Arc::new(|_| {}))
                .with_run_ledger(Arc::clone(&self.ledger))
                .with_text_bound(self.text_max_chars)
                .with_skill_source(Arc::clone(&self.skills) as _),
        )
    }

    /// What a run of `set` with `trials` per case is measured with; `model` is what the
    /// runtime reported for the agent, and what was configured for the legacy arm. The legacy
    /// arm has no skills, tools or step limit to name.
    #[must_use]
    pub fn version(&self, set: &EvalSet, trials: u32, model: &str) -> VersionTuple {
        let lock = Lock::of(set);
        if self.legacy.is_some() {
            return VersionTuple {
                engine: env!("CARGO_PKG_VERSION").to_owned(),
                model: model.to_owned(),
                skills: BTreeMap::new(),
                tools: BTreeMap::new(),
                set_version: lock.version,
                set_digest: lock.digest,
                trials,
                max_steps: 0,
                joint_max_steps: 0,
                analyst_max_steps: 0,
                planners: 0,
                planner_reads: false,
            };
        }
        VersionTuple {
            engine: env!("CARGO_PKG_VERSION").to_owned(),
            model: model.to_owned(),
            skills: self.skills.versions(),
            tools: EVAL_PROFILES
                .iter()
                .filter_map(|id| self.profiles.get(id))
                .map(|profile| {
                    (
                        profile.id().to_owned(),
                        profile
                            .tool_names()
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                    )
                })
                .collect(),
            set_version: lock.version,
            set_digest: lock.digest,
            trials,
            max_steps: self.limits.max_steps,
            joint_max_steps: self.joint_limits.max_steps,
            analyst_max_steps: self.analyst_limits.max_steps,
            planners: u32::try_from(self.planners).unwrap_or(u32::MAX),
            planner_reads: self.planner_reads,
        }
    }

    fn capability(&self, profile: &str) -> Result<Capability, String> {
        let registered = |id: &str| {
            self.profiles
                .get(id)
                .ok_or_else(|| format!("profile {id:?} is not registered"))
        };
        match profile {
            SUPPLY_RISK_PROFILE => Ok(Capability {
                profile: registered(profile)?,
                action_types: action_types(&propose::supply_risk_catalog()),
                plans: Plans::Subject {
                    task: Arc::new(propose::supply_risk_task),
                    legacy: propose::supply_risk_agent,
                    scorer: Arc::new(SupplyRiskScorer::new()),
                },
            }),
            PROCESS_PROFILE => Ok(Capability {
                profile: registered(profile)?,
                action_types: action_types(&propose::process_catalog_for_plans()),
                plans: Plans::Subject {
                    task: Arc::new(propose::process_task),
                    legacy: propose::process_agent,
                    scorer: Arc::new(ProcessContentionScorer::new()),
                },
            }),
            PORTFOLIO_PROFILE => Ok(Capability {
                profile: registered(profile)?,
                // Both members' catalogs, as the profile's `submit_plan` holds them.
                action_types: action_types(&propose::supply_risk_catalog())
                    .into_iter()
                    .chain(action_types(&propose::process_catalog_for_plans()))
                    .collect(),
                plans: Plans::Group {
                    task: {
                        let text_bound = self.text_max_chars;
                        Arc::new(move |group: &_, grounding: &_, findings: &_, tried: &_| {
                            propose::portfolio_task(group, grounding, findings, tried, text_bound)
                        })
                    },
                    analysts: propose::member_analysts(
                        Arc::new(propose::SemanticFindings::new(
                            Arc::clone(&self.world),
                            registered(SUPPLY_RISK_ANALYST_PROFILE)?,
                            registered(PROCESS_ANALYST_PROFILE)?,
                            self.text_max_chars,
                        )),
                        registered(SUPPLY_RISK_ANALYST_PROFILE)?,
                        registered(PROCESS_ANALYST_PROFILE)?,
                        self.analyst_limits,
                    ),
                    // Ranked by the monitor's own detection rules, as a portfolio's joint slate.
                    scorer: Arc::new(PortfolioScorer::new(crate::drift::monitor_rules())),
                },
            }),
            other => Err(format!("no capability runs under the profile {other:?}")),
        }
    }

    /// What the semantic layer says about `member` in the case that is open.
    ///
    /// # Errors
    /// The analysts' profiles are not registered, or the world could not be read.
    pub async fn derive(
        &self,
        member: &EntityUri,
    ) -> Result<ekg_cognition::tools::Derived, String> {
        let registered = |id: &str| {
            self.profiles
                .get(id)
                .ok_or_else(|| format!("tool profile {id:?} is not registered"))
        };
        let source = propose::SemanticFindings::new(
            Arc::clone(&self.world),
            registered(SUPPLY_RISK_ANALYST_PROFILE)?,
            registered(PROCESS_ANALYST_PROFILE)?,
            self.text_max_chars,
        );
        ekg_cognition::tools::FindingSource::derive(&source, member)
            .await
            .map_err(|e| e.to_string())
    }

    /// The subjects the monitor's rules flag in the case that is open, as the monitor would
    /// seat them.
    ///
    /// # Errors
    /// The world could not be read.
    pub async fn flagged(&self) -> Result<Vec<EntityUri>, String> {
        let rules = crate::drift::monitor_rules();
        let verdicts = ekg_dynamic::monitor::verdicts(self.world.as_ref(), &rules)
            .await
            .map_err(|e| e.to_string())?;
        Ok(ekg_dynamic::monitor::flagged_subjects(&verdicts))
    }

    /// The limits of an agent run for a case planned as `plans`: a joint case's planner runs
    /// under a portfolio's.
    fn limits_for(&self, plans: &Plans) -> AgentLimits {
        match plans {
            Plans::Subject { .. } => self.limits,
            Plans::Group { .. } => self.joint_limits,
        }
    }

    /// The arm for a case of `capability`, in whichever world is open.
    fn build_arm(&self, capability: &Capability) -> Box<dyn Arm> {
        let (world, max_actions) = (Arc::clone(&self.world), self.max_plan_actions);
        let limits = self.limits_for(&capability.plans);
        match (&self.legacy, &capability.plans) {
            (None, Plans::Subject { task, .. }) => Box::new(AgentArm::new(AgentProposer::new(
                world,
                Arc::clone(&capability.profile),
                Arc::clone(&self.tokens),
                Arc::clone(&self.reasoner),
                Arc::clone(&self.ledger),
                Arc::clone(&self.skills) as _,
                Arc::clone(task),
                limits,
            ))),
            (None, Plans::Group { task, analysts, .. }) => Box::new(AgentArm::for_group(
                GroupAgentProposer::new(
                    world,
                    Arc::clone(&capability.profile),
                    Arc::clone(&self.tokens),
                    Arc::clone(&self.reasoner),
                    Arc::clone(&self.ledger),
                    Arc::clone(&self.skills) as _,
                    Arc::clone(task),
                    limits,
                    analysts.clone(),
                )
                .with_strategies(propose::planner_strategies(self.planners))
                .with_planner_reads(self.planner_reads),
            )),
            (Some(provider), Plans::Subject { legacy, .. }) => {
                let legacy = *legacy;
                Box::new(LegacyArm {
                    provider: Arc::clone(provider),
                    proposer: LegacyProposer::Subject(Box::new(move |provider| {
                        Arc::new(legacy(Arc::clone(&world), provider, max_actions))
                    })),
                })
            }
            // The joint prompt path, as a portfolio's joint slate is drafted today.
            (Some(provider), Plans::Group { .. }) => Box::new(LegacyArm {
                provider: Arc::clone(provider),
                proposer: LegacyProposer::Group(Box::new(move |provider| {
                    Arc::new(joint_prompt_agent(&world, &provider, max_actions))
                })),
            }),
        }
    }
}

/// The joint prompt agent over `world` and `provider`: each member grounded by its own
/// capability's bounded agent, one joint prompt, one reply.
fn joint_prompt_agent(
    world: &Arc<dyn WorldModel>,
    provider: &Arc<dyn CognitionProvider>,
    max_actions: usize,
) -> GroupAgent {
    let agent =
        |build: LegacyAgent| Arc::new(build(Arc::clone(world), Arc::clone(provider), max_actions));
    GroupAgent::new(
        joint::member_router(
            agent(propose::supply_risk_agent),
            agent(propose::process_agent),
        ),
        Arc::clone(provider),
        Arc::new(joint::group_prompt),
        Arc::new(joint::group_parse),
        max_actions,
    )
}

/// What an open case's plans are scored with.
enum CaseScorer {
    Subject(Arc<dyn Scorer>),
    Group(Arc<dyn GroupScorer>, SubjectGroup),
}

#[async_trait]
impl CaseEnv for EvalEnv {
    async fn open(&self, case: &Case) -> Result<Box<dyn OpenCase>, String> {
        let capability = self.capability(&case.profile)?;
        // What the case plans for must be what its profile plans for, before a world is built.
        let resolved = case.resolve(&ontology::entity_iri);
        let arm = self.build_arm(&capability);
        let scorer = match (capability.plans, &resolved.plans_for) {
            (Plans::Subject { scorer, .. }, PlansFor::Subject(_)) => CaseScorer::Subject(scorer),
            (Plans::Group { scorer, .. }, PlansFor::Group(group)) => {
                CaseScorer::Group(scorer, group.clone())
            }
            (Plans::Subject { .. }, PlansFor::Group(_)) => {
                return Err(format!(
                    "the profile {:?} plans for one subject, the case names members",
                    case.profile
                ));
            }
            (Plans::Group { .. }, PlansFor::Subject(_)) => {
                return Err(format!(
                    "the profile {:?} plans for members, the case names one subject",
                    case.profile
                ));
            }
        };
        let contract = || ontology::supply_risk_contract().map_err(|e| e.to_string());
        let records: Vec<SourceRecord> = case
            .records
            .iter()
            .map(|record| record.source_record(SOURCE_SYSTEM))
            .collect();
        let fixture = FixtureWorld::load(&self.server, EVAL_REPO_PREFIX, contract()?, &records)
            .await
            .map_err(|e| e.to_string())?;
        // From here on the fixture is owned by the open case: closing it, or dropping it on
        // an early return below, removes the repository.
        let model = fixture
            .world_model(contract()?, self.read_policy.clone())
            .map_err(|e| e.to_string())?;
        let simulator = GraphDbSimulator::new(fixture.client().map_err(|e| e.to_string())?);
        self.switch.set(Some(Arc::new(model)));
        let mut open = EvalCase {
            resolved,
            scope: None,
            arm,
            action_types: capability.action_types,
            max_actions: self.max_plan_actions,
            simulator,
            scorer,
            fixture: Some(fixture),
            switch: Arc::clone(&self.switch),
            scoring: Arc::clone(&self.scoring),
        };
        // The scope of a task for the case's subject, or for its members under the routed
        // reader: each member's own scope, together.
        let reader = capability.profile.reader(Arc::clone(&self.world));
        match TaskScope::resolve(open.resolved.plans_for.subjects(), &reader).await {
            Ok(scope) => open.scope = Some(scope),
            Err(why) => {
                Box::new(open).close().await;
                return Err(format!("the case's scope could not be resolved: {why}"));
            }
        }
        Ok(Box::new(open))
    }
}

/// One open case: its world, and what a trial in it runs and is graded with.
struct EvalCase {
    resolved: ResolvedCase,
    scope: Option<TaskScope>,
    action_types: Vec<String>,
    max_actions: usize,
    arm: Box<dyn Arm>,
    simulator: GraphDbSimulator,
    scorer: CaseScorer,
    fixture: Option<FixtureWorld>,
    switch: Arc<SwitchWorld>,
    scoring: Arc<Semaphore>,
}

#[async_trait]
impl OpenCase for EvalCase {
    fn resolved(&self) -> &ResolvedCase {
        &self.resolved
    }

    fn in_scope(&self, entity: &EntityUri) -> bool {
        self.scope
            .as_ref()
            .is_some_and(|scope| scope.contains(entity))
    }

    fn action_types(&self) -> &[String] {
        &self.action_types
    }

    fn max_actions(&self) -> usize {
        self.max_actions
    }

    fn arm(&self) -> &dyn Arm {
        self.arm.as_ref()
    }

    async fn score(&self, plan: &Plan) -> Result<Score, String> {
        // The projection is a repository of its own until it is dropped at the end of this.
        bounded(&self.scoring, async {
            let projection = self
                .simulator
                .project(plan)
                .await
                .map_err(|e| e.to_string())?;
            let scored = match &self.scorer {
                CaseScorer::Subject(scorer) => scorer.score_plan(plan, projection.as_ref()).await,
                CaseScorer::Group(scorer, group) => {
                    scorer
                        .score_group_plan(plan, projection.as_ref(), group)
                        .await
                }
            };
            scored.map_err(|e| e.to_string())
        })
        .await?
    }

    async fn close(mut self: Box<Self>) {
        self.switch.set(None);
        if let Some(fixture) = self.fixture.take() {
            fixture.discard().await;
        }
    }
}

/// The arm an environment builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmKind {
    /// The tool-using agent.
    Agent,
    /// The bounded prompt agent.
    Legacy,
}

impl ArmKind {
    /// The arm's name in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Legacy => "legacy",
        }
    }
}

/// The arms a run covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arms {
    /// The tool-using agent alone.
    Agent,
    /// The bounded prompt agent alone.
    Legacy,
    /// The legacy arm, then the agent, as one paired report.
    Both,
}

impl Arms {
    fn parse(name: &str, value: &str) -> Result<Self, String> {
        match value.trim() {
            "agent" => Ok(Self::Agent),
            "legacy" => Ok(Self::Legacy),
            "both" => Ok(Self::Both),
            other => Err(format!(
                "{name} must be agent, legacy or both, got {other:?}"
            )),
        }
    }

    /// The passes of a run, in the order they run: the legacy arm is cheap and fails fast on
    /// a broken setup before the agent's tokens are spent.
    #[must_use]
    pub fn passes(self) -> &'static [ArmKind] {
        match self {
            Self::Agent => &[ArmKind::Agent],
            Self::Legacy => &[ArmKind::Legacy],
            Self::Both => &[ArmKind::Legacy, ArmKind::Agent],
        }
    }
}

/// The `agent-eval run` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// The eval set's directory.
    pub set: PathBuf,
    /// The skill directory.
    pub skills: PathBuf,
    /// The trials per case.
    pub trials: u32,
    /// The token bound.
    pub max_tokens: u64,
    /// The further attempts of a trial that errored.
    pub retries: u32,
    /// The trials of a case in flight at once, before the server's room is taken into account.
    pub concurrency: u32,
    /// The arms to run.
    pub arms: Arms,
    /// Only these cases, when any is named.
    pub cases: Vec<String>,
    /// Only these categories, when any is named.
    pub categories: Vec<String>,
    /// Only the cases of these profiles, when any is named.
    pub profiles: Vec<String>,
    /// Where the report goes; a timestamped file under `.artifacts/agent-eval/` when unset.
    pub report: Option<PathBuf>,
}

impl RunArgs {
    /// Whether the run leaves cases out.
    #[must_use]
    pub fn narrowed(&self) -> bool {
        !self.narrowing().is_empty()
    }

    /// What the run is narrowed to.
    #[must_use]
    pub fn narrowing(&self) -> Narrowing {
        Narrowing {
            cases: self.cases.clone(),
            categories: self.categories.clone(),
            profiles: self.profiles.clone(),
        }
    }
}

/// An `agent-eval` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the set.
    Run(RunArgs),
    /// Renew the set's lock.
    Lock {
        /// The eval set's directory.
        set: PathBuf,
    },
}

/// The command line's usage.
pub const USAGE: &str = "usage: agent-eval run [--set <dir>] [--skills <dir>] [--trials <k>] \
                         [--max-tokens <n>] [--retries <n>] [--concurrency <n>] \
                         [--arm agent|legacy|both] [--case <id>]... \
                         [--category <name>]... [--profile <id>]... \
                         [--report <path>] | \
                         agent-eval lock [--set <dir>]";

fn positive<T>(flag: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + Default,
{
    value
        .trim()
        .parse::<T>()
        .ok()
        .filter(|parsed| *parsed > T::default())
        .ok_or_else(|| format!("{flag} must be a positive number, got {value:?}"))
}

fn count(flag: &str, value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{flag} must be a number, zero or more, got {value:?}"))
}

/// Parse the command line (without the program name). `env` supplies the defaults a flag
/// overrides: `EKG_AGENT_EVAL_TRIALS`, `EKG_AGENT_EVAL_MAX_TOKENS`, `EKG_AGENT_EVAL_RETRIES`,
/// `EKG_AGENT_EVAL_CONCURRENCY` and `EKG_AGENT_EVAL_ARM`.
/// `set` and `skills` are the directories used when no flag names one.
///
/// # Errors
/// A message naming the flag or value that is wrong.
pub fn parse_args(
    args: &[String],
    env: &BTreeMap<String, String>,
    set: PathBuf,
    skills: PathBuf,
) -> Result<Command, String> {
    let mut args = args.iter().map(String::as_str);
    let command = args.next().ok_or_else(|| USAGE.to_owned())?;
    let from_env = |key: &str| env.get(key).filter(|value| !value.trim().is_empty());
    let mut run = RunArgs {
        set,
        skills,
        trials: match from_env("EKG_AGENT_EVAL_TRIALS") {
            Some(value) => positive("EKG_AGENT_EVAL_TRIALS", value)?,
            None => DEFAULT_TRIALS,
        },
        max_tokens: match from_env("EKG_AGENT_EVAL_MAX_TOKENS") {
            Some(value) => positive("EKG_AGENT_EVAL_MAX_TOKENS", value)?,
            None => DEFAULT_MAX_TOKENS,
        },
        retries: match from_env("EKG_AGENT_EVAL_RETRIES") {
            Some(value) => count("EKG_AGENT_EVAL_RETRIES", value)?,
            None => DEFAULT_RETRIES,
        },
        concurrency: match from_env("EKG_AGENT_EVAL_CONCURRENCY") {
            Some(value) => positive("EKG_AGENT_EVAL_CONCURRENCY", value)?,
            None => DEFAULT_CONCURRENCY,
        },
        arms: match from_env("EKG_AGENT_EVAL_ARM") {
            Some(value) => Arms::parse("EKG_AGENT_EVAL_ARM", value)?,
            None => Arms::Agent,
        },
        cases: Vec::new(),
        categories: Vec::new(),
        profiles: Vec::new(),
        report: None,
    };
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match (command, flag) {
            (_, "--set") => run.set = PathBuf::from(value()?),
            ("run", "--skills") => run.skills = PathBuf::from(value()?),
            ("run", "--trials") => run.trials = positive(flag, value()?)?,
            ("run", "--max-tokens") => run.max_tokens = positive(flag, value()?)?,
            ("run", "--retries") => run.retries = count(flag, value()?)?,
            ("run", "--concurrency") => run.concurrency = positive(flag, value()?)?,
            ("run", "--arm") => run.arms = Arms::parse(flag, value()?)?,
            ("run", "--case") => run.cases.push(value()?.to_owned()),
            ("run", "--category") => run.categories.push(value()?.to_owned()),
            ("run", "--profile") => run.profiles.push(value()?.to_owned()),
            ("run", "--report") => run.report = Some(PathBuf::from(value()?)),
            _ => return Err(format!("unknown argument {flag:?}\n{USAGE}")),
        }
    }
    match command {
        "lock" => Ok(Command::Lock { set: run.set }),
        "run" => Ok(Command::Run(run)),
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    }
}

/// The model credential the runtime will call the provider with, by the name of the setting
/// that holds it.
///
/// # Errors
/// A message naming the settings when neither is set: an eval calls the real model.
pub fn model_credential(env: &BTreeMap<String, String>) -> Result<&'static str, String> {
    ["AGENT_MODEL_API_KEY", "MODEL_API_KEY"]
        .into_iter()
        .find(|key| env.get(*key).is_some_and(|value| !value.trim().is_empty()))
        .ok_or_else(|| {
            "MODEL_API_KEY is not set (nor AGENT_MODEL_API_KEY): an eval runs the real model"
                .to_owned()
        })
}

/// The cases of `set` a run with `args` covers.
///
/// # Errors
/// A message naming a case id or category the set does not have, or a profile none of its
/// cases has.
pub fn select_cases<'s>(set: &'s EvalSet, args: &RunArgs) -> Result<Vec<&'s Case>, String> {
    for id in &args.cases {
        if !set.cases.iter().any(|case| &case.id == id) {
            return Err(format!("the eval set has no case {id:?}"));
        }
    }
    for name in &args.categories {
        if !set.manifest.categories.iter().any(|c| &c.name == name) {
            return Err(format!("the eval set has no category {name:?}"));
        }
    }
    for name in &args.profiles {
        if !set.cases.iter().any(|case| &case.profile == name) {
            return Err(format!("the eval set has no case of the profile {name:?}"));
        }
    }
    Ok(set
        .cases
        .iter()
        .filter(|case| {
            !args.narrowed()
                || args.cases.contains(&case.id)
                || args.categories.contains(&case.category)
                || args.profiles.contains(&case.profile)
        })
        .collect())
}

/// The last line of a narrowed run: the cases of `set` that `selected` leaves out, and what
/// the report therefore is not. `None` when the run covered the whole set.
#[must_use]
pub fn left_out(set: &EvalSet, selected: &[&Case]) -> Option<String> {
    let skipped: Vec<&str> = set
        .cases
        .iter()
        .filter(|case| !selected.iter().any(|ran| ran.id == case.id))
        .map(|case| case.id.as_str())
        .collect();
    (!skipped.is_empty()).then(|| {
        format!(
            "narrowed run: {} of the set's {} cases were left out ({}); the report is no \
             evidence for a promotion verdict",
            skipped.len(),
            set.cases.len(),
            skipped.join(", ")
        )
    })
}

/// One line per arm that attempted a trial again, naming the trials.
#[must_use]
pub fn retried_summary(reports: &[Report]) -> Vec<String> {
    reports
        .iter()
        .filter_map(|report| {
            let trials: Vec<String> = report
                .cases
                .iter()
                .flat_map(|case| {
                    case.trials
                        .iter()
                        .filter(|trial| !trial.errored_attempts.is_empty())
                        .map(move |trial| format!("{} trial {}", case.id, trial.trial))
                })
                .collect();
            (!trials.is_empty()).then(|| {
                format!(
                    "{}: {} trial(s) errored and were attempted again: {}",
                    report.arm,
                    trials.len(),
                    trials.join(", ")
                )
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn parse(line: &str, env: &[(&str, &str)]) -> Result<Command, String> {
        let env = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        parse_args(
            &args(line),
            &env,
            PathBuf::from("set"),
            PathBuf::from("skills"),
        )
    }

    fn run(line: &str, env: &[(&str, &str)]) -> RunArgs {
        match parse(line, env).expect("parses") {
            Command::Run(run) => run,
            Command::Lock { .. } => panic!("a run was expected"),
        }
    }

    #[test]
    fn a_bare_run_takes_the_defaults() {
        let run = run("run", &[]);
        assert_eq!(
            (run.trials, run.max_tokens),
            (DEFAULT_TRIALS, DEFAULT_MAX_TOKENS)
        );
        assert!(!run.narrowed());
        assert_eq!(run.set, PathBuf::from("set"));
        assert_eq!(run.skills, PathBuf::from("skills"));
    }

    #[test]
    fn the_environment_sets_defaults_and_a_flag_overrides_them() {
        let env = [
            ("EKG_AGENT_EVAL_TRIALS", "5"),
            ("EKG_AGENT_EVAL_MAX_TOKENS", "900"),
        ];
        let from_env = run("run", &env);
        assert_eq!((from_env.trials, from_env.max_tokens), (5, 900));
        let flagged = run(
            "run --trials 2 --max-tokens 100 --set s --report r.json",
            &env,
        );
        assert_eq!((flagged.trials, flagged.max_tokens), (2, 100));
        assert_eq!(flagged.set, PathBuf::from("s"));
        assert_eq!(flagged.report, Some(PathBuf::from("r.json")));
    }

    #[test]
    fn a_wrong_value_names_its_flag() {
        for (line, needle) in [
            ("run --trials 0", "--trials"),
            ("run --max-tokens many", "--max-tokens"),
            ("run --case", "--case needs a value"),
            ("run --nonsense", "unknown argument"),
            ("walk", "unknown command"),
            ("run --gate", "unknown argument"),
            ("run --record-baseline", "unknown argument"),
            ("lock --trials 2", "unknown argument"),
        ] {
            let why = parse(line, &[]).expect_err(line);
            assert!(why.contains(needle), "{line}: {why}");
        }
        let why = parse("run", &[("EKG_AGENT_EVAL_TRIALS", "x")]).expect_err("bad env");
        assert!(why.contains("EKG_AGENT_EVAL_TRIALS"), "{why}");
    }

    #[test]
    fn a_run_is_narrowed_by_case_and_category() {
        let narrowed = run("run --case a --category late", &[]);
        assert!(narrowed.narrowed());
        assert_eq!(narrowed.cases, ["a"]);
        assert_eq!(narrowed.categories, ["late"]);
    }

    #[test]
    fn the_arm_defaults_to_the_agent_and_is_set_by_flag_or_environment() {
        assert_eq!(run("run", &[]).arms, Arms::Agent);
        for (value, arms) in [
            ("agent", Arms::Agent),
            ("legacy", Arms::Legacy),
            ("both", Arms::Both),
        ] {
            assert_eq!(run(&format!("run --arm {value}"), &[]).arms, arms);
            assert_eq!(run("run", &[("EKG_AGENT_EVAL_ARM", value)]).arms, arms);
        }
        let env = [("EKG_AGENT_EVAL_ARM", "legacy")];
        assert_eq!(
            run("run --arm both", &env).arms,
            Arms::Both,
            "the flag wins"
        );
        let why = parse("run --arm prompt", &[]).expect_err("unknown arm");
        assert!(why.contains("--arm") && why.contains("\"prompt\""), "{why}");
        let why = parse("run", &[("EKG_AGENT_EVAL_ARM", "x")]).expect_err("bad env");
        assert!(why.contains("EKG_AGENT_EVAL_ARM"), "{why}");
    }

    #[test]
    fn a_paired_run_runs_the_legacy_arm_first() {
        assert_eq!(Arms::Agent.passes(), [ArmKind::Agent]);
        assert_eq!(Arms::Legacy.passes(), [ArmKind::Legacy]);
        assert_eq!(Arms::Both.passes(), [ArmKind::Legacy, ArmKind::Agent]);
        assert_eq!(
            (ArmKind::Agent.name(), ArmKind::Legacy.name()),
            ("agent", "legacy")
        );
    }

    /// A provider that answers every prompt with the next of its replies.
    struct Replies(Mutex<Vec<EngineResult<String>>>);

    #[async_trait]
    impl CognitionProvider for Replies {
        async fn generate(&self, _prompt: &str) -> EngineResult<String> {
            self.0.lock().expect("replies").remove(0)
        }

        async fn embed(&self, _text: &str) -> EngineResult<Vec<f32>> {
            Ok(vec![0.5])
        }
    }

    #[tokio::test]
    async fn the_recorder_returns_the_reply_unchanged_and_keeps_the_latest() {
        let plan = "Late.\nACTION: expedite_po\nTARGET: x\n";
        let recorder = LastReply::new(Arc::new(Replies(Mutex::new(vec![
            Ok(plan.to_owned()),
            Ok("Nothing helps here.".to_owned()),
            Err(EngineError::Backend("down".to_owned())),
        ]))));
        assert!(!recorder.declined(), "no reply yet is not a decline");
        assert_eq!(recorder.generate("p").await.expect("reply"), plan);
        assert!(!recorder.declined(), "the reply proposed an action");
        assert_eq!(
            recorder.generate("p").await.expect("reply"),
            "Nothing helps here."
        );
        assert!(recorder.declined(), "the latest reply proposed none");
        assert!(recorder.generate("p").await.is_err());
        assert_eq!(recorder.last().as_deref(), Some("Nothing helps here."));
        assert_eq!(recorder.embed("t").await.expect("embedding"), [0.5]);
    }

    fn env(legacy: bool) -> EvalEnv {
        let config = engine_defaults(&BTreeMap::new(), &[]).expect("config");
        let server = GraphDbClient::new(config.graphdb_url.clone(), config.graphdb_repo.clone())
            .expect("client");
        let reasoner =
            ekg_cognition::RuntimeReasoner::new(DEFAULT_RUNTIME_URL, std::num::NonZeroUsize::MIN)
                .expect("reasoner");
        let skills = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../hayhooks/skills");
        let env = EvalEnv::new(server, Arc::new(reasoner), &skills, &config).expect("env");
        if legacy {
            env.with_legacy_arm(Arc::new(Replies(Mutex::new(Vec::new()))))
        } else {
            env
        }
    }

    #[test]
    fn a_joint_case_runs_under_a_portfolios_limits_and_any_other_under_a_points() {
        let env = env(false);
        for profile in EVAL_PROFILES {
            let capability = env.capability(profile).expect(profile);
            let limits = env.limits_for(&capability.plans);
            let expected = if EVAL_JOINT_PROFILES.contains(&profile) {
                (8, Duration::from_secs(120))
            } else {
                (12, Duration::from_secs(120))
            };
            assert_eq!((limits.max_steps, limits.run_cap), expected, "{profile}");
        }
    }

    #[tokio::test]
    async fn every_profile_builds_its_arm_and_an_unknown_profile_is_refused() {
        for (legacy, name) in [(true, "legacy"), (false, "agent")] {
            let env = env(legacy);
            assert_eq!(env.arm().name(), name);
            for profile in EVAL_PROFILES {
                let capability = env.capability(profile).expect(profile);
                assert_eq!(env.build_arm(&capability).name(), name, "{profile}");
                assert_eq!(
                    matches!(capability.plans, Plans::Group { .. }),
                    EVAL_JOINT_PROFILES.contains(&profile),
                    "{profile}"
                );
                assert_eq!(
                    !capability.profile.member_ids().is_empty(),
                    EVAL_JOINT_PROFILES.contains(&profile),
                    "a joint case runs under a composed profile: {profile}"
                );
            }
            let portfolio = env.capability(PORTFOLIO_PROFILE).expect("portfolio");
            assert_eq!(portfolio.action_types.len(), 7, "both members' catalogs");
            let Err(why) = env.capability("churn") else {
                panic!("an unknown profile was accepted");
            };
            assert!(why.contains("\"churn\""), "{why}");
        }
    }

    #[test]
    fn a_legacy_run_names_its_model_and_no_skills_tools_or_step_limit() {
        let manifest = "version: 1\nmin_cases_per_category: 1\ncategories: []\n";
        let set = EvalSet::parse(manifest, BTreeMap::new()).expect("set");
        let legacy = env(true).version(&set, 3, "vendor/chat");
        assert_eq!(legacy.model, "vendor/chat");
        assert!(legacy.skills.is_empty() && legacy.tools.is_empty());
        assert_eq!((legacy.max_steps, legacy.trials), (0, 3));
        assert_eq!(legacy.joint_max_steps, 0);
        let agent = env(false).version(&set, 3, "vendor/agent");
        assert_eq!((agent.max_steps, agent.joint_max_steps), (12, 8));
        assert!(!agent.skills.is_empty() && !agent.tools.is_empty());
        assert_eq!(
            agent.tools[PORTFOLIO_PROFILE],
            [
                "ground_entity",
                "related_entities",
                "list_work_centres",
                "list_work_centre_schedule",
                "submit_plan"
            ],
            "the joint profile's tools are part of what a run is measured with"
        );
        assert!(agent.skills.contains_key(propose::PLAN_PORTFOLIO_SKILL));
        assert_eq!(agent.set_digest, legacy.set_digest);
    }

    #[test]
    fn lock_takes_the_set_directory() {
        assert_eq!(
            parse("lock --set other", &[]),
            Ok(Command::Lock {
                set: PathBuf::from("other")
            })
        );
    }

    #[test]
    fn a_run_needs_a_model_credential() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let why = model_credential(&env(&[("MODEL_API_KEY", " ")])).expect_err("blank");
        assert!(why.contains("MODEL_API_KEY"), "{why}");
        assert_eq!(
            model_credential(&env(&[("MODEL_API_KEY", "k")])),
            Ok("MODEL_API_KEY")
        );
        assert_eq!(
            model_credential(&env(&[
                ("MODEL_API_KEY", "k"),
                ("AGENT_MODEL_API_KEY", "a")
            ])),
            Ok("AGENT_MODEL_API_KEY")
        );
    }

    #[test]
    fn cases_are_selected_by_id_or_category_and_an_unknown_one_is_named() {
        let case = |id: &str, category: &str| {
            (
                format!("cases/{id}.yaml"),
                format!(
                    "id: {id}\ncategory: {category}\nprofile: supply-risk\n\
                     subject: {{ model: sale.order, id: 1 }}\n\
                     records:\n  - {{ model: sale.order, id: 1 }}\n\
                     expect: {{ outcome: plan }}\nnotes: n\n\
                     reference:\n  actions:\n    \
                     - {{ action_type: expedite_po, target: {{ model: sale.order, id: 1 }} }}\n"
                ),
            )
        };
        let manifest = "version: 1\nmin_cases_per_category: 1\ncategories:\n  \
                        - { name: late, covers: x }\n  - { name: other, covers: y }\n";
        let files = [case("a", "late"), case("b", "late"), case("c", "other")];
        let set = EvalSet::parse(manifest, files.into_iter().collect()).expect("set");
        let ids = |line: &str| -> Vec<String> {
            select_cases(&set, &run(line, &[]))
                .expect("selected")
                .iter()
                .map(|case| case.id.clone())
                .collect()
        };
        assert_eq!(ids("run"), ["a", "b", "c"]);
        assert_eq!(ids("run --case b"), ["b"]);
        assert_eq!(ids("run --category late --case c"), ["a", "b", "c"]);
        let why = select_cases(&set, &run("run --case z", &[])).expect_err("unknown");
        assert!(why.contains("\"z\""), "{why}");
        let why = select_cases(&set, &run("run --category z", &[])).expect_err("unknown");
        assert!(why.contains("category \"z\""), "{why}");
    }

    #[test]
    fn the_engine_defaults_ignore_the_operators_read_policy() {
        let env: BTreeMap<String, String> = [
            ("EKG_READ_POLICY_SENSITIVE_EDGES", "nothing"),
            ("EKG_AGENT_MAX_STEPS", "20"),
            ("EKG_AGENT_JOINT_MAX_STEPS", "16"),
            ("EKG_AGENT_JOINT_RUN_SECS", "240"),
            ("EKG_AGENT_TEXT_MAX_CHARS", "40"),
            ("EKG_MAX_PLAN_ACTIONS", "9"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let passed = [
            "EKG_AGENT_MAX_STEPS",
            "EKG_AGENT_JOINT_MAX_STEPS",
            "EKG_AGENT_JOINT_RUN_SECS",
            "EKG_AGENT_TEXT_MAX_CHARS",
        ];
        let config = engine_defaults(&env, &passed).expect("config");
        assert_eq!(config.agent_max_steps, 20, "passed through");
        assert_eq!(config.agent_text_max_chars, 40, "passed through");
        let joint = config.agent_joint_limits();
        assert_eq!(
            (joint.max_steps, joint.run_cap),
            (16, Duration::from_secs(240)),
            "passed through"
        );
        assert_eq!(config.read_policy_sensitive_edges, ["unit_price"]);
        assert_eq!(
            config.max_plan_actions,
            ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS
        );
    }

    #[tokio::test]
    async fn the_switch_reads_the_open_world_only_and_never_writes() {
        use ekg_domain::fakes::InMemoryWorldModel;

        let switch = SwitchWorld::default();
        let closed = switch
            .select("SELECT * WHERE {}")
            .await
            .expect_err("no case");
        assert!(
            closed.to_string().contains("no eval case is open"),
            "{closed}"
        );

        switch.set(Some(Arc::new(InMemoryWorldModel::default())));
        switch.select("SELECT * WHERE {}").await.expect("delegates");
        let record = SourceRecord {
            source_ref: ekg_domain::SourceRef {
                system: "odoo".to_owned(),
                model: "sale.order".to_owned(),
                id: 1,
            },
            fields: BTreeMap::new(),
        };
        assert!(switch.materialize(&record).await.is_err(), "read-only");
        assert!(switch.retract(&EntityUri("x".to_owned())).await.is_err());

        switch.set(None);
        assert!(switch.select("SELECT * WHERE {}").await.is_err());
    }

    #[test]
    fn retries_default_to_two_and_take_a_flag_or_a_setting_zero_included() {
        assert_eq!(run("run", &[]).retries, DEFAULT_RETRIES);
        assert_eq!(DEFAULT_RETRIES, 2);
        let env = [("EKG_AGENT_EVAL_RETRIES", "4")];
        assert_eq!(run("run", &env).retries, 4);
        assert_eq!(run("run --retries 1", &env).retries, 1, "the flag wins");
        assert_eq!(run("run --retries 0", &[]).retries, 0);
        assert_eq!(run("run", &[("EKG_AGENT_EVAL_RETRIES", "0")]).retries, 0);
        for line in ["run --retries some", "run --retries -1"] {
            let why = parse(line, &[]).expect_err(line);
            assert!(why.contains("--retries"), "{why}");
        }
        let why = parse("run", &[("EKG_AGENT_EVAL_RETRIES", "x")]).expect_err("bad env");
        assert!(why.contains("EKG_AGENT_EVAL_RETRIES"), "{why}");
    }

    #[test]
    fn concurrency_defaults_to_three_and_takes_a_flag_or_a_setting_but_not_zero() {
        assert_eq!(run("run", &[]).concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(DEFAULT_CONCURRENCY, 3);
        let env = [("EKG_AGENT_EVAL_CONCURRENCY", "2")];
        assert_eq!(run("run", &env).concurrency, 2);
        assert_eq!(run("run --concurrency 1", &env).concurrency, 1);
        for line in ["run --concurrency 0", "run --concurrency many"] {
            let why = parse(line, &[]).expect_err(line);
            assert!(why.contains("--concurrency"), "{why}");
        }
        let why = parse("run", &[("EKG_AGENT_EVAL_CONCURRENCY", "0")]).expect_err("bad env");
        assert!(why.contains("EKG_AGENT_EVAL_CONCURRENCY"), "{why}");
    }

    fn repos(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("repo{i}")).collect()
    }

    #[test]
    fn the_trial_bound_is_what_the_server_has_room_for() {
        // A case's world and one simulation per trial in flight.
        assert_eq!(
            trial_bound(5, &repos(0), 3),
            Ok(3),
            "room for the full bound"
        );
        assert_eq!(trial_bound(5, &repos(1), 3), Ok(3));
        assert_eq!(trial_bound(5, &repos(2), 3), Ok(2), "lowered");
        assert_eq!(
            trial_bound(5, &repos(3), 3),
            Ok(1),
            "a case and one simulation"
        );
        assert_eq!(trial_bound(50, &repos(3), 3), Ok(3), "never raised");
        for existing in [4, 5, 9] {
            let why = trial_bound(5, &repos(existing), 3).expect_err("no room");
            assert!(why.contains("at most 5 repositories"), "{why}");
            assert!(why.contains("repo0, repo1, repo2, repo3"), "{why}");
        }
    }

    #[test]
    fn the_repository_limit_is_graphdb_frees_unless_set() {
        let env = |value: &str| -> BTreeMap<String, String> {
            [("EKG_GRAPHDB_MAX_REPOSITORIES".to_owned(), value.to_owned())]
                .into_iter()
                .collect()
        };
        assert_eq!(max_repositories(&BTreeMap::new()), Ok(5));
        assert_eq!(max_repositories(&env(" ")), Ok(5));
        assert_eq!(max_repositories(&env("40")), Ok(40));
        for wrong in ["0", "many"] {
            let why = max_repositories(&env(wrong)).expect_err(wrong);
            assert!(why.contains("EKG_GRAPHDB_MAX_REPOSITORIES"), "{why}");
        }
    }

    /// How many are at work at once, and the most that ever were.
    #[derive(Default)]
    struct AtOnce {
        now: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
    }

    impl AtOnce {
        async fn work(&self, yields: u32) {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.now.fetch_add(1, SeqCst) + 1;
            self.most.fetch_max(now, SeqCst);
            for _ in 0..yields {
                tokio::task::yield_now().await;
            }
            self.now.fetch_sub(1, SeqCst);
        }

        fn most(&self) -> usize {
            self.most.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn scoring_holds_no_more_plans_at_once_than_its_bound() {
        for (bound, most) in [(1, 1), (2, 2), (5, 4)] {
            let (permits, scorer) = (Semaphore::new(bound), AtOnce::default());
            let score = || bounded(&permits, scorer.work(3));
            let (a, b, c, d) = tokio::join!(score(), score(), score(), score());
            assert_eq!((a, b, c, d), (Ok(()), Ok(()), Ok(()), Ok(())));
            assert_eq!(scorer.most(), most, "a bound of {bound}");
        }
        let closed = Semaphore::new(1);
        closed.close();
        assert!(bounded(&closed, async {}).await.is_err());
    }

    #[test]
    fn each_arm_calls_its_model_as_often_at_once_as_trials_are_in_flight() {
        assert_eq!(permits(3).get(), 3);
        assert_eq!(permits(0).get(), 1);
        let config = engine_defaults(&BTreeMap::new(), &[]).expect("config");
        let routed = legacy_config(&config, "http://runtime:1", 3);
        assert_eq!(routed.cognition_limits.max_concurrent.get(), 3);
        assert_eq!(
            routed.cognition_route,
            CognitionRoute::Runtime {
                url: "http://runtime:1".to_owned()
            }
        );
        assert_eq!(routed.cognition_chat_model, config.cognition_chat_model);
    }

    /// Replies with a plan when asked about a `plan` subject and with none otherwise, after
    /// `plan` subjects waited longer.
    #[derive(Default)]
    struct BySubject(AtOnce);

    #[async_trait]
    impl CognitionProvider for BySubject {
        async fn generate(&self, prompt: &str) -> EngineResult<String> {
            let plan = prompt.contains("plan");
            self.0.work(if plan { 8 } else { 2 }).await;
            Ok(if plan {
                "Late.\nACTION: expedite_po\nTARGET: x\n".to_owned()
            } else {
                "Nothing helps here.".to_owned()
            })
        }

        async fn embed(&self, _text: &str) -> EngineResult<Vec<f32>> {
            Ok(Vec::new())
        }
    }

    /// Asks its provider about the subject and has the reply refused, once every reply of
    /// the trials in flight is in.
    struct Refusing(Arc<dyn CognitionProvider>);

    #[async_trait]
    impl CandidateProposer for Refusing {
        async fn propose_candidate(
            &self,
            subject: &EntityUri,
            _feedback: &[ScoredCandidate],
        ) -> EngineResult<ekg_domain::GroundedProposal> {
            self.0.generate(&subject.0).await?;
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            Err(EngineError::InvalidProposal("refused".to_owned()))
        }
    }

    #[tokio::test]
    async fn legacy_trials_at_the_same_time_each_judge_their_own_reply() {
        use ekg_cognition::eval::TrialEnd;

        let provider = Arc::new(BySubject::default());
        let arm = LegacyArm {
            provider: Arc::clone(&provider) as Arc<dyn CognitionProvider>,
            proposer: LegacyProposer::Subject(Box::new(|provider| Arc::new(Refusing(provider)))),
        };
        let (plan, none) = (
            PlansFor::Subject(EntityUri("x:plan".to_owned())),
            PlansFor::Subject(EntityUri("x:none".to_owned())),
        );
        let (a, b, c) = tokio::join!(
            arm.trial(&plan, &[]),
            arm.trial(&none, &[]),
            arm.trial(&plan, &[]),
        );
        assert_eq!(provider.0.most(), 3, "the provider is called three at once");
        let refused = TrialEnd::Rejected("refused".to_owned());
        assert_eq!(
            (a.end, b.end, c.end),
            (refused.clone(), TrialEnd::Declined, refused)
        );
    }

    fn profiled_set() -> EvalSet {
        let case = |id: &str, profile: &str| {
            (
                format!("cases/{id}.yaml"),
                format!(
                    "id: {id}\ncategory: late\nprofile: {profile}\n\
                     subject: {{ model: sale.order, id: 1 }}\n\
                     records:\n  - {{ model: sale.order, id: 1 }}\n\
                     expect: {{ outcome: plan }}\nnotes: n\n\
                     reference:\n  actions:\n    \
                     - {{ action_type: expedite_po, target: {{ model: sale.order, id: 1 }} }}\n"
                ),
            )
        };
        let manifest = "version: 1\nmin_cases_per_category: 1\ncategories:\n  \
                        - { name: late, covers: x }\n";
        let files = [
            case("a", "supply-risk"),
            case("b", "process"),
            case("c", "process"),
        ];
        EvalSet::parse(manifest, files.into_iter().collect()).expect("set")
    }

    #[test]
    fn a_run_is_narrowed_to_the_cases_of_a_profile_and_an_unknown_one_is_named() {
        let set = profiled_set();
        let ids = |line: &str| -> Vec<String> {
            select_cases(&set, &run(line, &[]))
                .expect("selected")
                .iter()
                .map(|case| case.id.clone())
                .collect()
        };
        assert_eq!(ids("run --profile process"), ["b", "c"]);
        assert_eq!(ids("run --profile supply-risk --case c"), ["a", "c"]);
        assert_eq!(
            ids("run --profile process --profile supply-risk"),
            ["a", "b", "c"]
        );
        let why = select_cases(&set, &run("run --profile churn", &[])).expect_err("unknown");
        assert!(why.contains("profile \"churn\""), "{why}");

        let narrowed = run("run --profile process", &[]);
        assert!(narrowed.narrowed());
        assert_eq!(narrowed.narrowing().profiles, ["process"]);
    }

    #[test]
    fn a_narrowed_run_ends_by_naming_the_cases_left_out_and_what_the_report_is_not() {
        let set = profiled_set();
        let whole = select_cases(&set, &run("run", &[])).expect("selected");
        assert_eq!(left_out(&set, &whole), None);

        let args = run("run --profile process", &[]);
        let selected = select_cases(&set, &args).expect("selected");
        let line = left_out(&set, &selected).expect("narrowed");
        assert!(
            line.contains("1 of the set's 3 cases were left out (a)"),
            "{line}"
        );
        assert!(
            line.contains("the report is no evidence for a promotion verdict"),
            "{line}"
        );

        // The report carries what was asked for, and reads without it.
        let version = env(false).version(&set, 1, "m");
        let report = Report::new("agent", version, Vec::new(), 0).narrowed_to(args.narrowing());
        let mut json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["narrowed_to"]["profiles"][0], "process");
        assert!(report.table().contains("narrowed to profile process"));
        json.as_object_mut().expect("object").remove("narrowed_to");
        let read: Report = serde_json::from_value(json).expect("report");
        assert_eq!(read.narrowed_to, None);
    }

    #[test]
    fn the_run_ends_by_naming_each_arms_retried_trials() {
        use ekg_cognition::eval::{CaseResult, Grade, TrialResult};

        let set = profiled_set();
        let report = |arm: &str, retried: &[(&str, u32)]| {
            let cases = ["a", "b"]
                .into_iter()
                .map(|id| CaseResult {
                    id: id.to_owned(),
                    category: "late".to_owned(),
                    profile: "process".to_owned(),
                    baseline: None,
                    trials: (1..=2)
                        .map(|n| {
                            let mut trial =
                                TrialResult::new(n, "submitted", Grade::Pass, None, None, None);
                            if retried.contains(&(id, n)) {
                                trial.errored_attempts = vec!["stalled".to_owned()];
                            }
                            trial
                        })
                        .collect(),
                    not_run: 0,
                })
                .collect();
            Report::new(arm, env(false).version(&set, 2, "m"), cases, 0)
        };
        assert!(retried_summary(&[report("agent", &[])]).is_empty());
        assert_eq!(
            retried_summary(&[report("legacy", &[]), report("agent", &[("b", 2)])]),
            ["agent: 1 trial(s) errored and were attempted again: b trial 2"]
        );
        assert_eq!(
            retried_summary(&[
                report("legacy", &[("a", 1)]),
                report("agent", &[("a", 2), ("b", 1)])
            ]),
            [
                "legacy: 1 trial(s) errored and were attempted again: a trial 1",
                "agent: 2 trial(s) errored and were attempted again: a trial 2, b trial 1"
            ]
        );
    }
}
