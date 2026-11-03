//! Hermetic suite for the agent proposer: the real tool gateway is served in-process over an
//! in-memory world, and a `ScriptedReasoner` plays the runtime's agent. It drives the gateway
//! over MCP with the token the proposer minted (`ground_entity`, `submit_plan` …) and reports a
//! scripted run. The suite asserts the plan of record comes from the gateway's run ledger, never
//! from the report.
//!
//! ```text
//! cargo test -p ekg-cognition --test it agent_proposer
//! ```

#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ekg_cognition::tools::{
    ActionSpec, CauseKind, Derived, Finding, FindingOrigin, FindingSource, GatewayConfig,
    JoinedCatalog, MemberState, ParamSpec, PlanCatalog, ProfileRegistry, ReadOnlyWorld, RunLedger,
    SkillPin, SubmitFinding, SubmitPlan, TaskTokens, ToolProfile, gateway_router, render_finding,
};
use ekg_cognition::{
    AgentLimits, AgentProposer, AgentSpec, BoundedAgent, GroundingExtension, GroupAgent,
    GroupAgentProposer, GroupSlateAgent, GroupTaskBuilder, MemberAnalysts, Reasoner, ReasoningTask,
    RunGate, RunRecord, RunRecorder, RunReport, RunRole, SkillPins, SlateAgent, TaskBuilder,
    Termination,
};
use ekg_domain::fakes::{DeterministicCognition, InMemorySimulator, InMemoryWorldModel};
use ekg_domain::{
    ActionType, CandidateProposer, EngineError, EntityUri, GroundedProposal,
    GroupCandidateProposer, GroupScorer, GroupSlateProposer, Plan, Projection, Proposal,
    ProposedAction, ReadContext, Result, Score, ScoredCandidate, Scorer, SlateProposer,
    SourceRecord, SourceRef, SubjectGroup, TraceTag, WorldModel,
};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

use opentelemetry::trace::FutureExt as _;

use crate::spans;

const SECRET: &[u8] = b"agent-proposer-suite-secret-32-bytes";
const PROFILE: &str = "planner";
/// The second capability's profile, and the two composed.
const PLANT: &str = "plant";
const JOINT: &str = "joint";
/// The analysts of the two capabilities' members.
const ANALYST: &str = "planner-analyst";
const PLANT_ANALYST: &str = "plant-analyst";

/// The related line the planner profile's hook names for the order.
struct LineHook {
    order: EntityUri,
    line: EntityUri,
}

#[async_trait]
impl GroundingExtension for LineHook {
    async fn related(&self, subject: &EntityUri) -> Result<Vec<EntityUri>> {
        Ok(if *subject == self.order {
            vec![self.line.clone()]
        } else {
            Vec::new()
        })
    }
}

/// `move_date` on a sale order (a `new_date`; a bare date is normalized to midnight).
struct MoveDate;

#[async_trait]
impl PlanCatalog for MoveDate {
    fn action_types(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            action_type: "move_date".to_owned(),
            description: "moves the order's date".to_owned(),
            target_model: Some("sale.order".to_owned()),
            params: vec![ParamSpec {
                name: "new_date".to_owned(),
                description: "the new date".to_owned(),
            }],
        }]
    }

    async fn check(
        &self,
        action: &ProposedAction,
        _world: &ReadOnlyWorld,
    ) -> std::result::Result<ProposedAction, Vec<String>> {
        if action.action_type.0 != "move_date" {
            return Err(vec![format!(
                "{:?} is not in the catalog",
                action.action_type.0
            )]);
        }
        let Some(date) = action.params.get("new_date") else {
            return Err(vec!["move_date is missing param \"new_date\"".to_owned()]);
        };
        let mut normalized = action.clone();
        if date.len() == 10 {
            normalized
                .params
                .insert("new_date".to_owned(), format!("{date} 00:00:00"));
        }
        Ok(normalized)
    }
}

/// `move_window` on a work order (a `start`).
struct MoveWindow;

#[async_trait]
impl PlanCatalog for MoveWindow {
    fn action_types(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            action_type: "move_window".to_owned(),
            description: "moves the work order's window".to_owned(),
            target_model: Some("mrp.workorder".to_owned()),
            params: vec![ParamSpec {
                name: "start".to_owned(),
                description: "the new start".to_owned(),
            }],
        }]
    }

    async fn check(
        &self,
        action: &ProposedAction,
        _world: &ReadOnlyWorld,
    ) -> std::result::Result<ProposedAction, Vec<String>> {
        if action.params.contains_key("start") {
            Ok(action.clone())
        } else {
            Err(vec!["move_window is missing param \"start\"".to_owned()])
        }
    }
}

/// One step of a scripted agent run.
#[derive(Clone)]
enum Step {
    /// `ground_entity` on the entity.
    Ground(EntityUri),
    /// `submit_plan` with these arguments.
    Submit(Value),
}

/// One scripted run: its steps, then the report it answers (an `Err` is a rate-limited run).
#[derive(Clone)]
struct Script {
    steps: Vec<Step>,
    report: std::result::Result<Termination, String>,
}

/// Plays the runtime's agent: per call, the next script (the last repeats), driven over MCP
/// with the task's token.
struct ScriptedReasoner {
    url: String,
    scripts: Vec<Script>,
    tasks: Mutex<Vec<ReasoningTask>>,
    /// The analysts' tasks, which an analyst answers apart from the scripts.
    analysts: Mutex<Vec<ReasoningTask>>,
    /// How many more runs of an analyst of the member end without a finding.
    failing: Mutex<BTreeMap<String, u32>>,
}

impl ScriptedReasoner {
    fn new(url: &str, scripts: Vec<Script>) -> Self {
        Self {
            url: url.to_owned(),
            scripts,
            tasks: Mutex::new(Vec::new()),
            analysts: Mutex::new(Vec::new()),
            failing: Mutex::new(BTreeMap::new()),
        }
    }

    /// The next `runs` analysts of `member` end at their step limit, without a finding.
    fn fail_analyst(&self, member: &EntityUri, runs: u32) {
        self.failing
            .lock()
            .expect("failing")
            .insert(member.0.clone(), runs);
    }

    fn analyst_tasks(&self) -> Vec<ReasoningTask> {
        self.analysts.lock().expect("analysts").clone()
    }

    /// Read the member, and hand in a finding citing what came back.
    async fn analyse(&self, task: &ReasoningTask) -> Result<RunReport> {
        self.analysts.lock().expect("analysts").push(task.clone());
        let first = task.task.lines().next().expect("a line");
        let member = first.trim_start_matches("Analyse ").trim_end_matches('.');
        let failing = {
            let mut failing = self.failing.lock().expect("failing");
            match failing.get_mut(member) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    true
                }
                _ => false,
            }
        };
        let report = |termination| RunReport {
            termination,
            steps: 3,
            input_tokens: 100,
            output_tokens: 20,
            model: "scripted-model".to_owned(),
            closing_answer: Some(ANALYST_WORDS.to_owned()),
        };
        if failing {
            return Ok(report(Termination::StepLimit));
        }
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.url.clone())
                .auth_header(task.token.clone()),
        );
        let client = ().serve(transport).await.expect("connect");
        let call = |tool: &str, args: Value| {
            let Value::Object(args) = args else {
                panic!("arguments must be an object");
            };
            CallToolRequestParams::new(tool.to_owned()).with_arguments(args)
        };
        let read = client
            .call_tool(call("ground_entity", json!({ "entity": member })))
            .await
            .expect("call");
        let grounding = read.structured_content.expect("a grounding");
        let edge = &grounding["edges"][0];
        let finding = json!({
            "member": member,
            "state": "at_risk",
            "causes": [{
                "kind": "late",
                "entities": [edge["subject"]],
                "evidence": [{
                    "subject": edge["subject"],
                    "predicate": edge["predicate"],
                    "object": edge["object"]
                }]
            }]
        });
        let handed_in = client
            .call_tool(call("submit_finding", finding))
            .await
            .expect("call");
        assert_ne!(handed_in.is_error, Some(true), "{:?}", handed_in.content);
        Ok(report(Termination::Submitted))
    }

    fn calls(&self) -> usize {
        self.tasks.lock().expect("tasks").len()
    }

    fn task(&self, index: usize) -> ReasoningTask {
        self.tasks.lock().expect("tasks")[index].clone()
    }
}

#[async_trait]
impl Reasoner for ScriptedReasoner {
    async fn reason(&self, task: &ReasoningTask) -> Result<RunReport> {
        if task.task.starts_with("Analyse ") {
            return self.analyse(task).await;
        }
        let script = {
            let mut tasks = self.tasks.lock().expect("tasks");
            tasks.push(task.clone());
            let index = (tasks.len() - 1).min(self.scripts.len() - 1);
            self.scripts[index].clone()
        };
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.url.clone())
                .auth_header(task.token.clone()),
        );
        let client = ().serve(transport).await.expect("connect");
        for step in &script.steps {
            let (tool, args) = match step {
                Step::Ground(entity) => ("ground_entity", json!({ "entity": entity.0 })),
                Step::Submit(args) => ("submit_plan", args.clone()),
            };
            let Value::Object(args) = args else {
                panic!("arguments must be an object");
            };
            client
                .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(args))
                .await
                .expect("call");
        }
        let termination = script.report.map_err(EngineError::RateLimited)?;
        Ok(RunReport {
            termination,
            steps: u32::try_from(script.steps.len()).expect("steps") + 1,
            input_tokens: 1000,
            output_tokens: 200,
            model: "scripted-model".to_owned(),
            closing_answer: (termination == Termination::NoPlan).then(|| DECLINED.to_owned()),
        })
    }
}

/// What a scripted analyst says last, which must never reach a planner.
const ANALYST_WORDS: &str = "The analyst says to leave the purchase order alone.";

/// What the scripted agent says last when it ends without a plan.
const DECLINED: &str = "Nothing on order helps, so no plan is handed in.";

/// Pins one skill version written for the planner.
struct FixedPins;

#[async_trait]
impl SkillPins for FixedPins {
    async fn pins(&self, profile: &str) -> Result<Vec<SkillPin>> {
        Ok(vec![SkillPin {
            name: "propose-plan".to_owned(),
            version: "abc123".to_owned(),
            profiles: vec![profile.to_owned()],
        }])
    }
}

/// A gate that can be closed, counting how often it was asked.
#[derive(Default)]
struct Switch {
    closed: AtomicBool,
    asked: AtomicUsize,
}

#[async_trait]
impl RunGate for Switch {
    async fn proceed(&self) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        !self.closed.load(Ordering::SeqCst)
    }
}

/// Keeps every run record.
#[derive(Default)]
struct Records(Mutex<Vec<RunRecord>>);

impl RunRecorder for Records {
    fn record(&self, record: RunRecord) {
        self.0.lock().expect("records").push(record);
    }
}

impl Records {
    /// Every run but the analysts'.
    fn all(&self) -> Vec<RunRecord> {
        let runs = self.0.lock().expect("records");
        runs.iter()
            .filter(|run| matches!(run.role, None | Some(RunRole::Planner)))
            .cloned()
            .collect()
    }

    fn analysts(&self) -> Vec<RunRecord> {
        let runs = self.0.lock().expect("records");
        runs.iter()
            .filter(|run| run.role == Some(RunRole::Analyst))
            .cloned()
            .collect()
    }
}

fn record(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
    SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        },
        fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// A gateway on an ephemeral port over an in-memory world, sharing its run ledger.
/// The semantic layer of the stage: it decides the members it is told, and no other.
#[derive(Default)]
struct StageFindings {
    decided: Mutex<BTreeMap<EntityUri, MemberState>>,
    contradicted: Mutex<Vec<String>>,
}

impl StageFindings {
    fn decide(&self, member: &EntityUri, state: MemberState) {
        self.decided
            .lock()
            .expect("decided")
            .insert(member.clone(), state);
    }
}

#[async_trait]
impl FindingSource for StageFindings {
    async fn derive(&self, member: &EntityUri) -> Result<Derived> {
        let state = self.decided.lock().expect("decided").get(member).copied();
        Ok(state.map_or_else(
            || Derived::Undetermined("no rule names a cause".to_owned()),
            |state| {
                Derived::Decided(Finding {
                    member: member.clone(),
                    state,
                    causes: Vec::new(),
                    origin: FindingOrigin::Semantic,
                    dropped: Vec::new(),
                })
            },
        ))
    }

    async fn conflicts(&self, _member: &EntityUri, _claim: &Finding) -> Result<Vec<String>> {
        Ok(self.contradicted.lock().expect("contradicted").clone())
    }
}

struct Stage {
    url: String,
    world: Arc<InMemoryWorldModel>,
    profile: Arc<ToolProfile>,
    tokens: Arc<TaskTokens>,
    ledger: Arc<RunLedger>,
    hook: Arc<LineHook>,
    order: EntityUri,
    /// The two capabilities' profiles composed, the plant's hook, and the plant's records.
    joint: Arc<ToolProfile>,
    analyst: Arc<ToolProfile>,
    plant_analyst: Arc<ToolProfile>,
    findings: Arc<StageFindings>,
    plant_hook: Arc<LineHook>,
    mo: EntityUri,
    work_order: EntityUri,
}

fn read_context() -> ReadContext {
    ReadContext {
        role: "planner".to_owned(),
        purpose: "delivery-promise".to_owned(),
    }
}

/// The plant reads as procurement: it may see the supplier the planner may not.
fn plant_read_context() -> ReadContext {
    ReadContext {
        role: "procurement".to_owned(),
        purpose: "on-plan-schedule".to_owned(),
    }
}

/// A bounded agent reading under `read_context` with `hook`, proposing nothing of its own.
fn bounded_agent(
    world: &Arc<InMemoryWorldModel>,
    read_context: ReadContext,
    hook: &Arc<LineHook>,
) -> BoundedAgent {
    let spec = AgentSpec {
        read_context,
        build_prompt: Arc::new(|_, _| String::new()),
        parse_action: Arc::new(|_, subject, _| {
            Ok(vec![ProposedAction {
                action_type: ActionType("move_date".to_owned()),
                target: subject.clone(),
                params: BTreeMap::new(),
            }])
        }),
        extend_grounding: Some(Arc::clone(hook) as Arc<dyn GroundingExtension>),
    };
    BoundedAgent::new(
        Arc::clone(world) as Arc<dyn WorldModel>,
        Arc::new(DeterministicCognition),
        spec,
    )
}

impl Stage {
    #[allow(clippy::too_many_lines)] // one fixture: world, three profiles, gateway
    async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let world = Arc::new(InMemoryWorldModel::default());
        let uri =
            |model: &str, id: i64| InMemoryWorldModel::uri_of(&record(model, id, &[]).source_ref);
        let (order, line, product) = (
            uri("sale.order", 1),
            uri("purchase.order.line", 9),
            uri("product.product", 3),
        );
        for rec in [
            record(
                "sale.order",
                1,
                &[
                    ("state", "late"),
                    ("supplier", "CellTech"),
                    ("product", &product.0),
                ],
            ),
            record("purchase.order.line", 9, &[("date_planned", "2027-11-15")]),
            record("product.product", 3, &[("name", "Battery")]),
            record("sale.order", 99, &[("state", "on-time")]),
            record(
                "mrp.production",
                7,
                &[("state", "confirmed"), ("supplier", "CellTech")],
            ),
            record("mrp.workorder", 3, &[("date_start", "2027-11-09 08:00:00")]),
        ] {
            world.materialize(&rec).await.expect("materialize");
        }
        let (mo, work_order) = (uri("mrp.production", 7), uri("mrp.workorder", 3));
        let hook = Arc::new(LineHook {
            order: order.clone(),
            line,
        });
        let plant_hook = Arc::new(LineHook {
            order: mo.clone(),
            line: work_order.clone(),
        });
        let profile = ToolProfile::new(
            PROFILE,
            read_context(),
            Some(Arc::clone(&hook) as Arc<dyn GroundingExtension>),
        )
        .with_tool(Arc::new(SubmitPlan::new(Arc::new(MoveDate), 3)));
        let plant = ToolProfile::new(
            PLANT,
            plant_read_context(),
            Some(Arc::clone(&plant_hook) as Arc<dyn GroundingExtension>),
        )
        .with_tool(Arc::new(SubmitPlan::new(Arc::new(MoveWindow), 3)));
        let catalog = JoinedCatalog::new(vec![Arc::new(MoveDate), Arc::new(MoveWindow)])
            .expect("no verb twice");
        let joint = ToolProfile::composed(
            JOINT,
            &[&profile, &plant],
            Arc::new(|entity: &EntityUri| {
                let plant = entity
                    .model()
                    .is_some_and(|model| model.starts_with("mrp."));
                if plant { PLANT } else { PROFILE }.to_owned()
            }),
        )
        .expect("two members")
        .with_tool(Arc::new(SubmitPlan::new(Arc::new(catalog), 3)));
        let finding = || {
            Arc::new(SubmitFinding::new(vec![CauseKind::new(
                "late",
                "Something arrives late.",
            )]))
        };
        let analyst = ToolProfile::new(
            ANALYST,
            read_context(),
            Some(Arc::clone(&hook) as Arc<dyn GroundingExtension>),
        )
        .with_tool(finding());
        let plant_analyst = ToolProfile::new(
            PLANT_ANALYST,
            plant_read_context(),
            Some(Arc::clone(&plant_hook) as Arc<dyn GroundingExtension>),
        )
        .with_tool(finding());
        let mut registry = ProfileRegistry::new(Arc::clone(&world) as Arc<dyn WorldModel>);
        for profile in [profile, plant, joint, analyst, plant_analyst] {
            registry.register(profile).expect("register");
        }
        let registry = Arc::new(registry);
        let profile = registry.get(PROFILE).expect("profile");
        let joint = registry.get(JOINT).expect("joint");
        let analyst = registry.get(ANALYST).expect("analyst");
        let plant_analyst = registry.get(PLANT_ANALYST).expect("plant analyst");
        let tokens = Arc::new(TaskTokens::new(SECRET).expect("secret"));
        let ledger = Arc::new(RunLedger::default());
        let router = gateway_router(
            registry,
            Arc::clone(&tokens),
            GatewayConfig::default()
                .with_call_log(Arc::new(|_| {}))
                .with_run_ledger(Arc::clone(&ledger)),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        Self {
            url: format!("http://127.0.0.1:{port}/mcp"),
            world,
            profile,
            tokens,
            ledger,
            hook,
            order,
            joint,
            analyst,
            plant_analyst,
            findings: Arc::new(StageFindings::default()),
            plant_hook,
            mo,
            work_order,
        }
    }

    /// The order and the manufacturing order that builds it, as one group.
    fn group(&self) -> SubjectGroup {
        SubjectGroup {
            key: "portfolio/abc".to_owned(),
            members: vec![self.order.clone(), self.mo.clone()],
            links: vec!["the manufacturing order builds the sales order".to_owned()],
            worded: false,
        }
    }

    /// A group agent proposer over `reasoner` and the composed profile, building a task that
    /// names the members, the coupling and the plans already tried with their scores.
    fn group_proposer(&self, reasoner: Arc<dyn Reasoner>) -> GroupAgentProposer {
        let task: GroupTaskBuilder = Arc::new(|group, _grounding, findings, feedback| {
            let members: Vec<&str> = group.members.iter().map(|m| m.0.as_str()).collect();
            let tried: Vec<String> = feedback
                .iter()
                .map(|tried| {
                    let verbs: Vec<&str> = tried
                        .plan
                        .actions
                        .iter()
                        .map(|a| a.action_type.0.as_str())
                        .collect();
                    format!("{} -> {}", verbs.join("+"), tried.score.rank_key)
                })
                .collect();
            let found: String = findings.iter().map(|f| render_finding(f, 80)).collect();
            format!(
                "Members {}. Coupled: {}. Found: {found}. Tried: [{}].",
                members.join(", "),
                group.links.join("; "),
                tried.join("; ")
            )
        });
        let (analyst, plant_analyst) = (Arc::clone(&self.analyst), Arc::clone(&self.plant_analyst));
        GroupAgentProposer::new(
            Arc::clone(&self.world) as Arc<dyn WorldModel>,
            Arc::clone(&self.joint),
            Arc::clone(&self.tokens),
            reasoner,
            Arc::clone(&self.ledger),
            Arc::new(FixedPins),
            task,
            // Limits of its own, apart from a point's.
            AgentLimits {
                max_steps: 14,
                run_cap: Duration::from_secs(240),
            },
            MemberAnalysts {
                source: Arc::clone(&self.findings) as Arc<dyn FindingSource>,
                profile: Arc::new(move |member: &EntityUri| {
                    if member.model().is_some_and(|m| m.starts_with("mrp.")) {
                        Arc::clone(&plant_analyst)
                    } else {
                        Arc::clone(&analyst)
                    }
                }),
                task: Arc::new(|member, _grounding| format!("Analyse {}.", member.0)),
                limits: AgentLimits {
                    max_steps: 6,
                    run_cap: Duration::from_secs(90),
                },
            },
        )
    }

    /// The joint prompt path over the same members: each grounded by its own capability's
    /// bounded agent.
    fn group_agent(&self) -> GroupAgent {
        let planner = Arc::new(bounded_agent(&self.world, read_context(), &self.hook));
        let plant = Arc::new(bounded_agent(
            &self.world,
            plant_read_context(),
            &self.plant_hook,
        ));
        let order = self.order.clone();
        GroupAgent::new(
            Arc::new(move |member: &EntityUri| {
                if member.model().is_some_and(|m| m.starts_with("mrp.")) {
                    Arc::clone(&plant)
                } else {
                    Arc::clone(&planner)
                }
            }),
            Arc::new(DeterministicCognition),
            Arc::new(|_, _, _| String::new()),
            Arc::new(move |_, _| {
                Ok(vec![ProposedAction {
                    action_type: ActionType("move_date".to_owned()),
                    target: order.clone(),
                    params: BTreeMap::new(),
                }])
            }),
            3,
        )
    }

    /// A plan across both catalogs: the order's date and the work order's window.
    fn joint_plan(&self, date: &str, explanation: &str) -> Value {
        json!({
            "actions": [
                { "action_type": "move_date", "target": self.order.0, "params": { "new_date": date } },
                { "action_type": "move_window", "target": self.work_order.0, "params": { "start": "2027-11-07 08:00:00" } }
            ],
            "explanation": explanation
        })
    }

    /// An agent proposer over `reasoner`, building a task that names the subject and the
    /// plans already tried.
    fn proposer(&self, reasoner: Arc<dyn Reasoner>) -> AgentProposer {
        let task: TaskBuilder = Arc::new(|grounding, feedback| {
            format!(
                "Subject {}. Tried {}. Load the skill propose-plan and follow it.",
                grounding.entity.0,
                feedback.len()
            )
        });
        AgentProposer::new(
            Arc::clone(&self.world) as Arc<dyn WorldModel>,
            Arc::clone(&self.profile),
            Arc::clone(&self.tokens),
            reasoner,
            Arc::clone(&self.ledger),
            Arc::new(FixedPins),
            task,
            AgentLimits {
                max_steps: 8,
                run_cap: Duration::from_secs(60),
            },
        )
    }

    /// The bounded agent over the same world, read context and hook, for comparing groundings.
    fn bounded(&self) -> BoundedAgent {
        bounded_agent(&self.world, read_context(), &self.hook)
    }

    fn move_date(&self, date: &str, explanation: &str) -> Value {
        json!({
            "actions": [{ "action_type": "move_date", "target": self.order.0, "params": { "new_date": date } }],
            "explanation": explanation
        })
    }

    /// A submission `submit_plan` rejects (no `new_date`).
    fn defective(&self) -> Value {
        json!({
            "actions": [{ "action_type": "move_date", "target": self.order.0, "params": {} }],
            "explanation": "first try"
        })
    }
}

fn the_date(proposal: &GroundedProposal) -> &str {
    proposal.proposal.actions[0]
        .params
        .get("new_date")
        .map_or("", String::as_str)
}

#[tokio::test]
async fn the_candidate_is_the_plan_the_ledger_recorded_as_accepted() {
    let stage = Stage::start().await;
    // The agent grounds, is rejected, fixes the plan and is accepted, and then reports a step
    // limit: what the runtime says does not decide the plan, the ledger does.
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![
                Step::Ground(stage.order.clone()),
                Step::Submit(stage.defective()),
                Step::Submit(stage.move_date("2027-11-08", "Move the promise.")),
            ],
            report: Ok(Termination::StepLimit),
        }],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);

    let candidate = proposer
        .propose_candidate(&stage.order, &[])
        .await
        .expect("a candidate");
    assert_eq!(candidate.proposal.explanation, "Move the promise.");
    assert_eq!(candidate.proposal.actions.len(), 1);
    assert_eq!(candidate.proposal.actions[0].target, stage.order);
    assert_eq!(
        the_date(&candidate),
        "2027-11-08 00:00:00",
        "normalized, as submit_plan returned it"
    );

    // Grounded as the bounded agent grounds the subject.
    let bounded = stage
        .bounded()
        .propose_candidate(&stage.order, &[])
        .await
        .expect("bounded");
    assert!(
        !candidate.grounding_omissions.is_empty(),
        "the supplier is hidden"
    );
    assert_eq!(candidate.grounding_omissions, bounded.grounding_omissions);
    assert_eq!(candidate.proposal.omissions, bounded.proposal.omissions);
    assert_eq!(candidate.grounding_edges, bounded.grounding_edges);
    assert_eq!(candidate.subject_members, bounded.subject_members);

    // The run record: the report's telemetry, the ledger's counts and defects, the pins.
    let task = reasoner.task(0);
    assert!(task.task.contains(&stage.order.0), "{}", task.task);
    assert_eq!(task.max_steps, 8);
    assert_eq!(task.run_cap, Duration::from_secs(60));
    let claims = stage
        .tokens
        .verify_now(&task.token)
        .expect("the minted token");
    assert_eq!(claims.profile, PROFILE);
    assert_eq!(claims.subjects, vec![stage.order.clone()]);
    assert_eq!(
        claims.tools,
        vec!["ground_entity", "related_entities", "submit_plan"]
    );
    assert_eq!(claims.trace_id, task.trace_id);
    let runs = records.all();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.termination, Termination::StepLimit);
    assert_eq!((run.input_tokens, run.output_tokens), (1000, 200));
    assert_eq!(run.model, "scripted-model");
    assert_eq!(run.trace_id, task.trace_id);
    assert_eq!(run.tool_calls["ground_entity"]["ok"], 1);
    assert_eq!(run.tool_calls["submit_plan"]["rejected"], 1);
    assert_eq!(run.tool_calls["submit_plan"]["ok"], 1);
    assert_eq!(run.rejections.len(), 1);
    assert!(
        run.rejections[0][0].contains("new_date"),
        "{:?}",
        run.rejections
    );
    assert_eq!(
        run.skills,
        BTreeMap::from([("propose-plan".to_owned(), "abc123".to_owned())])
    );
    assert_eq!(run.tools, claims.tools, "the tools the token allowed");
    assert!(stage.ledger.is_empty(), "the run was read once");
}

#[tokio::test]
async fn a_run_without_an_accepted_plan_is_refused() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![
            // Rejected, never fixed.
            Script {
                steps: vec![Step::Submit(stage.defective())],
                report: Ok(Termination::NoPlan),
            },
            // Claims to have submitted, but never called submit_plan.
            Script {
                steps: vec![Step::Ground(stage.order.clone())],
                report: Ok(Termination::Submitted),
            },
        ],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    for termination in ["no_plan", "submitted"] {
        let err = proposer
            .propose_candidate(&stage.order, &[])
            .await
            .expect_err("no accepted plan");
        assert!(
            matches!(&err, EngineError::InvalidProposal(why)
                if why == &format!("agent ended {termination} without an accepted plan")),
            "{err:?}"
        );
    }
    let runs = records.all();
    assert_eq!(runs.len(), 2, "both runs are recorded");
    assert_eq!(runs[0].rejections.len(), 1);
    assert_eq!(runs[1].tool_calls["ground_entity"]["ok"], 1);
    assert_eq!(
        runs[0].closing_answer.as_deref(),
        Some(DECLINED),
        "a run without a plan keeps what the agent said last"
    );
    assert_eq!(runs[1].closing_answer, None);
}

#[tokio::test]
async fn a_failed_run_keeps_its_class_and_is_recorded_as_error() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Ground(stage.order.clone())],
            report: Err("slow down".to_owned()),
        }],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    let err = proposer
        .propose_candidate(&stage.order, &[])
        .await
        .expect_err("rate limited");
    assert!(matches!(err, EngineError::RateLimited(_)), "{err:?}");
    let runs = records.all();
    assert_eq!(runs[0].termination, Termination::Error);
    assert_eq!(
        runs[0].tool_calls["ground_entity"]["ok"], 1,
        "the ledger still counts"
    );
}

#[tokio::test]
async fn a_stopped_gate_makes_no_reasoner_call() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.move_date("2027-11-08", "x"))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let gate = Arc::new(Switch::default());
    gate.closed.store(true, Ordering::SeqCst);
    let records = Arc::new(Records::default());
    let proposer = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_gate(Arc::clone(&gate) as Arc<dyn RunGate>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    let err = proposer
        .propose_candidate(&stage.order, &[])
        .await
        .expect_err("stopped");
    assert!(
        matches!(&err, EngineError::Unavailable(why) if why.contains("gate")),
        "{err:?}"
    );
    assert_eq!(gate.asked.load(Ordering::SeqCst), 1);
    assert_eq!(reasoner.calls(), 0, "no reasoner call");
    assert!(records.all().is_empty(), "no run to record");
    assert!(stage.ledger.is_empty(), "no token was used");
}

/// Ranks a plan by its first action's `new_date` (earlier is better); the empty plan last.
struct EarliestDate;

#[async_trait]
impl Scorer for EarliestDate {
    async fn score(&self, _projection: &dyn Projection) -> Result<Score> {
        Ok(Score {
            metrics: BTreeMap::new(),
            rank_key: "~".to_owned(),
        })
    }

    async fn score_plan(&self, plan: &Plan, projection: &dyn Projection) -> Result<Score> {
        let Some(date) = plan.actions.first().and_then(|a| a.params.get("new_date")) else {
            return self.score(projection).await;
        };
        Ok(Score {
            metrics: BTreeMap::from([("new_date".to_owned(), date.clone())]),
            rank_key: date.clone(),
        })
    }
}

/// Answers the given candidates in order (an `Err` is a refused generation).
struct Sequence {
    answers: Vec<std::result::Result<GroundedProposal, String>>,
    next: AtomicUsize,
}

#[async_trait]
impl CandidateProposer for Sequence {
    async fn propose_candidate(
        &self,
        _subject: &EntityUri,
        _feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        self.answers[index.min(self.answers.len() - 1)]
            .clone()
            .map_err(EngineError::InvalidProposal)
    }
}

#[tokio::test]
async fn an_agent_proposer_slate_ranks_like_the_bounded_agents_and_traces_a_refused_run() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![
            Script {
                steps: vec![Step::Submit(stage.move_date("2027-11-11", "later"))],
                report: Ok(Termination::Submitted),
            },
            Script {
                steps: vec![Step::Submit(stage.defective())],
                report: Ok(Termination::NoPlan),
            },
            Script {
                steps: vec![Step::Submit(stage.move_date("2027-11-08", "sooner"))],
                report: Ok(Termination::Submitted),
            },
        ],
    ));
    let simulator = Arc::new(InMemorySimulator::default());
    let agent_slate = SlateAgent::with_max_candidates(
        Arc::new(stage.proposer(reasoner as Arc<dyn Reasoner>)),
        Arc::clone(&simulator) as Arc<dyn ekg_domain::Simulator>,
        Arc::new(EarliestDate),
        3,
    )
    .propose_slate(&stage.order)
    .await
    .expect("agent slate");

    // The same candidates from a plain proposer, as the bounded agent would hand them over.
    let grounded = stage
        .bounded()
        .propose_candidate(&stage.order, &[])
        .await
        .expect("bounded");
    let candidate = |date: &str, explanation: &str| {
        let mut params = BTreeMap::new();
        params.insert("new_date".to_owned(), format!("{date} 00:00:00"));
        Ok(GroundedProposal {
            proposal: Proposal {
                explanation: explanation.to_owned(),
                actions: vec![ProposedAction {
                    action_type: ActionType("move_date".to_owned()),
                    target: stage.order.clone(),
                    params,
                }],
                omissions: grounded.grounding_omissions.clone(),
            },
            ..grounded.clone()
        })
    };
    let plain_slate = SlateAgent::with_max_candidates(
        Arc::new(Sequence {
            answers: vec![
                candidate("2027-11-11", "later"),
                Err("agent ended no_plan without an accepted plan".to_owned()),
                candidate("2027-11-08", "sooner"),
            ],
            next: AtomicUsize::new(0),
        }),
        simulator,
        Arc::new(EarliestDate),
        3,
    )
    .propose_slate(&stage.order)
    .await
    .expect("plain slate");

    assert_eq!(agent_slate, plain_slate, "the same slate, step for step");
    let dates: Vec<&str> = agent_slate
        .options
        .iter()
        .map(|o| o.plan.actions[0].params["new_date"].as_str())
        .collect();
    assert_eq!(dates, vec!["2027-11-08 00:00:00", "2027-11-11 00:00:00"]);
    assert!(agent_slate.options[0].recommended);
    assert!(
        agent_slate
            .trace
            .steps
            .iter()
            .any(|step| step.tag == TraceTag::Generation
                && step.rejected
                && step.text
                    == "attempt 2: refused — agent ended no_plan without an accepted plan"),
        "{:#?}",
        agent_slate.trace.steps
    );
}

#[tokio::test]
async fn the_eval_arm_maps_each_way_a_run_ends() {
    use ekg_cognition::eval::{AgentArm, Arm, PlansFor, TrialEnd};

    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![
            Script {
                steps: vec![
                    Step::Submit(stage.defective()),
                    Step::Submit(stage.move_date("2027-11-08", "The line is late.")),
                ],
                report: Ok(Termination::Submitted),
            },
            Script {
                steps: vec![Step::Ground(stage.order.clone())],
                report: Ok(Termination::NoPlan),
            },
            Script {
                steps: vec![Step::Submit(stage.defective())],
                report: Ok(Termination::StepLimit),
            },
            Script {
                steps: vec![],
                report: Err("slow down".to_owned()),
            },
            // Claims a plan the gateway never accepted.
            Script {
                steps: vec![Step::Ground(stage.order.clone())],
                report: Ok(Termination::Submitted),
            },
        ],
    ));
    let arm = AgentArm::new(stage.proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>));
    assert_eq!(arm.name(), "agent");
    let order = PlansFor::Subject(stage.order.clone());

    let submitted = arm.trial(&order, &[]).await;
    let TrialEnd::Submitted(proposal) = &submitted.end else {
        panic!("{:?}", submitted.end);
    };
    assert_eq!(proposal.explanation, "The line is late.");
    let run = submitted.run.expect("the run's record");
    assert_eq!(run.rejections.len(), 1, "the rejected first submission");
    assert_eq!((run.input_tokens, run.output_tokens), (1000, 200));

    let declined = arm.trial(&order, &[]).await;
    assert_eq!(declined.end, TrialEnd::Declined);
    let run = declined.run.expect("record");
    assert_eq!(
        run.termination,
        Termination::NoPlan,
        "each trial carries its own run, not the one before"
    );
    assert_eq!(run.closing_answer.as_deref(), Some(DECLINED));

    assert_eq!(arm.trial(&order, &[]).await.end, TrialEnd::Exhausted);

    let errored = arm.trial(&order, &[]).await;
    assert!(
        matches!(&errored.end, TrialEnd::Errored(why) if why.contains("slow down")),
        "{:?}",
        errored.end
    );

    let unrecorded = arm.trial(&order, &[]).await;
    assert!(
        matches!(&unrecorded.end, TrialEnd::Errored(why) if why.contains("did not record")),
        "{:?}",
        unrecorded.end
    );
}

/// Holds every run until `runs` of them have started, so they are in flight together.
struct Together {
    inner: Arc<ScriptedReasoner>,
    started: tokio::sync::Barrier,
}

#[async_trait]
impl Reasoner for Together {
    async fn reason(&self, task: &ReasoningTask) -> Result<RunReport> {
        self.started.wait().await;
        self.inner.reason(task).await
    }
}

#[tokio::test]
async fn runs_at_the_same_time_for_one_subject_each_keep_their_own_record_and_plan() {
    use ekg_cognition::eval::{AgentArm, Arm, PlansFor, TrialEnd};

    let stage = Stage::start().await;
    // Three runs that differ in what they call and in the plan they hand in.
    let scripted = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![
            Script {
                steps: vec![
                    Step::Ground(stage.order.clone()),
                    Step::Submit(stage.move_date("2027-11-08", "one")),
                ],
                report: Ok(Termination::Submitted),
            },
            Script {
                steps: vec![
                    Step::Submit(stage.defective()),
                    Step::Submit(stage.move_date("2027-11-09", "two")),
                ],
                report: Ok(Termination::Submitted),
            },
            Script {
                steps: vec![
                    Step::Ground(stage.order.clone()),
                    Step::Ground(stage.order.clone()),
                    Step::Submit(stage.defective()),
                    Step::Submit(stage.defective()),
                    Step::Submit(stage.move_date("2027-11-10", "three")),
                ],
                report: Ok(Termination::Submitted),
            },
        ],
    ));
    let reasoner = Arc::new(Together {
        inner: Arc::clone(&scripted),
        started: tokio::sync::Barrier::new(3),
    });
    let arm = AgentArm::new(stage.proposer(reasoner as Arc<dyn Reasoner>));
    let order = PlansFor::Subject(stage.order.clone());

    let (a, b, c) = tokio::join!(
        arm.trial(&order, &[]),
        arm.trial(&order, &[]),
        arm.trial(&order, &[]),
    );

    // Which trial got which script is not fixed; that each kept its own run is.
    let mut seen = Vec::new();
    let mut traces = Vec::new();
    for trial in [a, b, c] {
        let TrialEnd::Submitted(proposal) = &trial.end else {
            panic!("{:?}", trial.end);
        };
        let run = trial.run.expect("the run's record");
        let date = proposal.actions[0].params["new_date"].as_str();
        let grounds = run
            .tool_calls
            .get("ground_entity")
            .map_or(0, |calls| calls["ok"]);
        let expected = match proposal.explanation.as_str() {
            "one" => ("2027-11-08 00:00:00", 1, 0),
            "two" => ("2027-11-09 00:00:00", 0, 1),
            "three" => ("2027-11-10 00:00:00", 2, 2),
            other => panic!("an unknown plan: {other}"),
        };
        assert_eq!(
            (date, grounds, run.rejections.len()),
            expected,
            "{}",
            proposal.explanation
        );
        assert_eq!(run.tool_calls["submit_plan"]["ok"], 1);
        seen.push(proposal.explanation.clone());
        traces.push(run.trace_id);
    }
    seen.sort();
    assert_eq!(seen, ["one", "three", "two"]);

    // One token per run, each traced as its own run's record.
    let mut tokens = Vec::new();
    for index in 0..scripted.calls() {
        let task = scripted.task(index);
        let claims = stage.tokens.verify_now(&task.token).expect("token");
        assert_eq!(claims.subjects, vec![stage.order.clone()]);
        assert!(traces.contains(&task.trace_id), "{}", task.trace_id);
        tokens.push(claims.jti);
    }
    tokens.sort();
    tokens.dedup();
    traces.sort();
    traces.dedup();
    assert_eq!((tokens.len(), traces.len()), (3, 3));
    assert!(stage.ledger.is_empty(), "every run was read once");
}

/// A reasoner whose runtime cannot be reached.
struct Down;

#[async_trait]
impl Reasoner for Down {
    async fn reason(&self, _task: &ReasoningTask) -> Result<RunReport> {
        Err(EngineError::Unavailable("reasoner: no route".to_owned()))
    }
}

#[tokio::test]
async fn a_run_is_a_span_that_gives_the_task_its_trace() {
    spans::on();
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![
                Step::Ground(stage.order.clone()),
                Step::Submit(stage.defective()),
                Step::Submit(stage.move_date("2027-11-08", "Move the promise.")),
            ],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    // A root context: whatever another test left current on this thread is not the parent.
    proposer
        .propose_candidate(&stage.order, &[])
        .with_context(opentelemetry::Context::new())
        .await
        .expect("a candidate");

    let record = &records.all()[0];
    let exported = spans::of(&record.trace_id);
    let run = spans::named(&exported, "invoke_agent planner");
    assert_eq!(run.span_context.trace_id().to_string(), record.trace_id);
    let task = reasoner.task(0);
    assert_eq!(task.trace_id, record.trace_id);
    assert_eq!(
        task.parent_span_id,
        run.span_context.span_id().to_string(),
        "the runtime continues the run span"
    );
    for (key, value) in [
        ("gen_ai.operation.name", "invoke_agent"),
        ("gen_ai.agent.name", PROFILE),
        ("gen_ai.response.model", "scripted-model"),
        ("gen_ai.usage.input_tokens", "1000"),
        ("gen_ai.usage.output_tokens", "200"),
        ("ekg.agent.termination", "submitted"),
        ("ekg.agent.steps", "4"),
        ("ekg.agent.max_steps", "8"),
        ("ekg.agent.rejections", "1"),
        ("ekg.skill.pins", "[\"propose-plan@abc123\"]"),
    ] {
        assert_eq!(spans::attr(run, key).as_deref(), Some(value), "{key}");
    }
    assert!(spans::attr(run, "ekg.engine.version").is_some());
    assert!(!spans::is_error(run));

    // The scripted runtime sends no trace context, so each call hangs under the run span.
    let calls: Vec<_> = exported
        .iter()
        .filter(|span| span.name.starts_with("execute_tool "))
        .collect();
    assert_eq!(calls.len(), 3, "{}", spans::text(&exported));
    for call in &calls {
        assert_eq!(call.parent_span_id, run.span_context.span_id());
    }
    let ground = spans::named(&exported, "execute_tool ground_entity");
    assert_eq!(
        spans::attr(ground, "ekg.tool.outcome").as_deref(),
        Some("ok")
    );
    assert_eq!(
        spans::attr(ground, "gen_ai.tool.name").as_deref(),
        Some("ground_entity")
    );
    let outcomes: Vec<(String, bool)> = calls
        .iter()
        .filter(|span| span.name == "execute_tool submit_plan")
        .map(|span| {
            (
                spans::attr(span, "ekg.tool.outcome").expect("outcome"),
                spans::is_error(span),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![("rejected".to_owned(), true), ("ok".to_owned(), false)]
    );

    // Names, versions, outcomes and counts only.
    let text = spans::text(&exported);
    for content in [
        stage.order.0.as_str(),
        "Move the promise.",
        "2027-11-08",
        task.token.as_str(),
        "Load the skill",
    ] {
        assert!(!text.contains(content), "{content:?} in:\n{text}");
    }
}

#[tokio::test]
async fn a_run_that_is_not_exported_still_names_a_trace() {
    spans::on();
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![
                Step::Ground(stage.order.clone()),
                Step::Submit(stage.move_date("2027-11-08", "Move the promise.")),
            ],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    proposer
        .propose_candidate(&stage.order, &[])
        .with_context(spans::unsampled())
        .await
        .expect("a candidate");

    let record = &records.all()[0];
    assert_eq!(record.trace_id.len(), 32);
    assert!(record.trace_id.chars().all(|c| c.is_ascii_hexdigit()));
    let task = reasoner.task(0);
    assert_eq!(task.trace_id, record.trace_id);
    assert_eq!(task.parent_span_id.len(), 16);
    assert!(
        spans::of(&record.trace_id).is_empty(),
        "nothing is exported"
    );
    assert_eq!(record.tool_calls["ground_entity"]["ok"], 1);
    assert_eq!(record.tool_calls["submit_plan"]["ok"], 1);
    assert!(stage.ledger.is_empty());
}

#[tokio::test]
async fn a_failed_run_is_an_error_span_with_its_class() {
    spans::on();
    let stage = Stage::start().await;
    let limited: Arc<dyn Reasoner> = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![],
            report: Err("slow down".to_owned()),
        }],
    ));
    for (reasoner, class) in [
        (limited, "rate_limited"),
        (Arc::new(Down) as _, "unavailable"),
    ] {
        let records = Arc::new(Records::default());
        let proposer = stage
            .proposer(reasoner)
            .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
        proposer
            .propose_candidate(&stage.order, &[])
            .with_context(opentelemetry::Context::new())
            .await
            .expect_err("the run failed");
        let exported = spans::of(&records.all()[0].trace_id);
        let run = spans::named(&exported, "invoke_agent planner");
        assert!(spans::is_error(run), "{class}");
        assert_eq!(spans::attr(run, "error.type").as_deref(), Some(class));
        assert_eq!(
            spans::attr(run, "ekg.agent.termination").as_deref(),
            Some("error")
        );
        assert!(spans::attr(run, "gen_ai.response.model").is_none());
    }
}

#[tokio::test]
async fn an_eval_trials_run_span_is_a_child_of_the_span_it_runs_under() {
    use ekg_cognition::eval::{AgentArm, Arm, PlansFor};
    use opentelemetry::trace::{TraceContextExt as _, Tracer as _};

    spans::on();
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(
                stage.move_date("2027-11-08", "The line is late."),
            )],
            report: Ok(Termination::Submitted),
        }],
    ));
    let arm = AgentArm::new(stage.proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>));
    let tracer = opentelemetry::global::tracer("suite");
    let trial = opentelemetry::Context::new().with_span(tracer.start("eval_trial a-case"));
    let (trace, trial_span) = {
        let span = trial.span();
        let context = span.span_context();
        (context.trace_id().to_string(), context.span_id())
    };
    let ran = arm
        .trial(&PlansFor::Subject(stage.order.clone()), &[])
        .with_context(trial.clone())
        .await;
    trial.span().end();

    assert_eq!(ran.run.expect("the run").trace_id, trace);
    let exported = spans::of(&trace);
    let run = spans::named(&exported, "invoke_agent planner");
    assert_eq!(run.parent_span_id, trial_span);
    let call = spans::named(&exported, "execute_tool submit_plan");
    assert_eq!(call.parent_span_id, run.span_context.span_id());
}

#[tokio::test]
async fn a_group_candidate_is_one_accepted_plan_across_both_catalogs() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);

    let candidate = proposer
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect("a candidate");
    assert_eq!(candidate.proposal.explanation, "Move both.");
    let verbs: Vec<&str> = candidate
        .proposal
        .actions
        .iter()
        .map(|a| a.action_type.0.as_str())
        .collect();
    assert_eq!(verbs, vec!["move_date", "move_window"]);
    assert_eq!(the_date(&candidate), "2027-11-08 00:00:00", "normalized");
    assert_eq!(candidate.proposal.actions[1].target, stage.work_order);

    // One token for the whole group, under the composed profile.
    let task = reasoner.task(0);
    assert!(
        task.task.contains(&stage.order.0) && task.task.contains(&stage.mo.0),
        "{}",
        task.task
    );
    assert!(
        task.task.contains("builds the sales order"),
        "{}",
        task.task
    );
    let claims = stage.tokens.verify_now(&task.token).expect("the token");
    assert_eq!(claims.profile, JOINT);
    assert_eq!(claims.subjects, vec![stage.order.clone(), stage.mo.clone()]);
    assert_eq!(claims.tools, vec!["submit_plan"], "a planner reads nothing");
    // The run record names the tool the planner got and the composed profile's pins.
    let runs = records.all();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].tools, claims.tools);
    assert_eq!(
        runs[0].skills,
        BTreeMap::from([("propose-plan".to_owned(), "abc123".to_owned())])
    );
    assert_eq!(runs[0].trace_id, task.trace_id);
    assert!(stage.ledger.is_empty(), "the run was read once");
}

#[tokio::test]
async fn a_group_run_carries_the_limits_its_proposer_was_built_with() {
    spans::on();
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    let proposer = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    proposer
        .propose_group_candidate(&stage.group(), &[])
        .with_context(opentelemetry::Context::new())
        .await
        .expect("a candidate");

    let task = reasoner.task(0);
    assert_eq!(task.max_steps, 14);
    assert_eq!(task.run_cap, Duration::from_secs(240));
    // The token outlives the run's cap.
    let claims = stage.tokens.verify_now(&task.token).expect("the token");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    assert!(
        (260..=270).contains(&(claims.exp - now)),
        "the cap and its margin: {}",
        claims.exp - now
    );
    let exported = spans::of(&records.all()[0].trace_id);
    let run = spans::named(&exported, &format!("invoke_agent {JOINT}"));
    assert_eq!(
        spans::attr(run, "ekg.agent.max_steps").as_deref(),
        Some("14"),
        "the limit the run was under, not a point's"
    );
}

#[tokio::test]
async fn a_group_run_without_an_accepted_plan_is_refused() {
    let stage = Stage::start().await;
    // A target no member evidences is outside the task's scope: `submit_plan` rejects it.
    let stranger = json!({
        "actions": [{
            "action_type": "move_date",
            "target": "https://ekg.example/id/sale.order/99",
            "params": { "new_date": "2027-11-08" }
        }],
        "explanation": "the wrong order"
    });
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stranger)],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    let error = stage
        .group_proposer(reasoner as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>)
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect_err("no accepted plan");
    assert!(
        matches!(&error, EngineError::InvalidProposal(why) if why.contains("without an accepted plan")),
        "{error}"
    );
    let runs = records.all();
    assert_eq!(runs[0].tool_calls["submit_plan"]["rejected"], 1);
    assert!(
        runs[0].rejections[0][0].contains("outside this task's scope"),
        "{:?}",
        runs[0].rejections
    );
}

#[tokio::test]
async fn an_empty_group_is_refused_before_any_run() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(&stage.url, Vec::new()));
    let records = Arc::new(Records::default());
    let empty = SubjectGroup {
        members: Vec::new(),
        ..stage.group()
    };
    let error = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>)
        .propose_group_candidate(&empty, &[])
        .await
        .expect_err("no members");
    assert!(error.to_string().contains("has no members"), "{error}");
    assert_eq!(reasoner.calls(), 0);
    assert!(records.all().is_empty());
}

#[tokio::test]
async fn a_stopped_gate_starts_no_group_run() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(&stage.url, Vec::new()));
    let gate = Arc::new(Switch::default());
    gate.closed.store(true, Ordering::SeqCst);
    let error = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_gate(Arc::clone(&gate) as Arc<dyn RunGate>)
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect_err("stopped");
    assert!(matches!(error, EngineError::Unavailable(_)), "{error}");
    assert_eq!(reasoner.calls(), 0);
}

#[tokio::test]
async fn a_group_candidate_is_grounded_like_the_joint_prompt_paths() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let group = stage.group();
    let agent = stage
        .group_proposer(reasoner as Arc<dyn Reasoner>)
        .propose_group_candidate(&group, &[])
        .await
        .expect("agent");
    let prompt = stage
        .group_agent()
        .propose_group_candidate(&group, &[])
        .await
        .expect("prompt path");
    assert_eq!(agent.grounding_omissions, prompt.grounding_omissions);
    assert_eq!(agent.proposal.omissions, prompt.proposal.omissions);
    assert_eq!(agent.grounding_edges, prompt.grounding_edges);
    assert_eq!(agent.subject_members, prompt.subject_members);
    // Each member under its own read context: the order's supplier is hidden and counted
    // once, the manufacturing order's is visible.
    let supplier_of = |entity: &EntityUri| {
        agent
            .grounding_edges
            .iter()
            .any(|e| e.subject == *entity && e.predicate == "ekg:supplier")
    };
    assert!(!supplier_of(&stage.order) && supplier_of(&stage.mo));
    assert_eq!(agent.grounding_omissions.len(), 1);
    assert_eq!(agent.grounding_omissions[0].count, 1);
    assert!(agent.subject_members.contains(&stage.work_order));
}

/// Ranks a group's plan by its first action's `new_date` (earlier is better); doing nothing
/// last.
struct EarliestGroupDate;

#[async_trait]
impl GroupScorer for EarliestGroupDate {
    async fn score_group(
        &self,
        _projection: &dyn Projection,
        _group: &SubjectGroup,
    ) -> Result<Score> {
        Ok(Score {
            metrics: BTreeMap::new(),
            rank_key: "~".to_owned(),
        })
    }

    async fn score_group_plan(
        &self,
        plan: &Plan,
        projection: &dyn Projection,
        group: &SubjectGroup,
    ) -> Result<Score> {
        let Some(date) = plan.actions.first().and_then(|a| a.params.get("new_date")) else {
            return self.score_group(projection, group).await;
        };
        Ok(Score {
            metrics: BTreeMap::from([("new_date".to_owned(), date.clone())]),
            rank_key: date.clone(),
        })
    }
}

#[tokio::test]
async fn a_group_slate_opens_linked_ranks_by_the_group_scorer_and_traces_a_refused_run() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![
            Script {
                steps: vec![Step::Submit(stage.joint_plan("2027-11-11", "later"))],
                report: Ok(Termination::Submitted),
            },
            Script {
                steps: vec![Step::Submit(stage.defective())],
                report: Ok(Termination::NoPlan),
            },
            Script {
                steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "sooner"))],
                report: Ok(Termination::Submitted),
            },
        ],
    ));
    let slate = GroupSlateAgent::with_max_candidates(
        Arc::new(stage.group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)),
        Arc::new(InMemorySimulator::default()),
        Arc::new(EarliestGroupDate),
        3,
    )
    .propose_group_slate(&stage.group())
    .await
    .expect("slate");

    let first = slate.trace.steps.first().expect("a step");
    assert_eq!(first.tag, TraceTag::Graph);
    assert!(first.text.starts_with("Linked: "), "{}", first.text);
    assert!(
        first.text.contains("builds the sales order"),
        "{}",
        first.text
    );
    let dates: Vec<&str> = slate
        .options
        .iter()
        .map(|o| o.plan.actions[0].params["new_date"].as_str())
        .collect();
    assert_eq!(dates, vec!["2027-11-08 00:00:00", "2027-11-11 00:00:00"]);
    assert!(slate.options[0].recommended);
    assert_eq!(slate.options[0].plan.actions.len(), 2, "both catalogs");
    assert!(
        slate
            .trace
            .steps
            .iter()
            .any(|step| step.tag == TraceTag::Generation
                && step.rejected
                && step.text
                    == "attempt 2: refused — agent ended no_plan without an accepted plan"),
        "{:#?}",
        slate.trace.steps
    );
    // The second candidate's task carries the first plan and what it scored.
    assert_eq!(reasoner.calls(), 3);
    let second = reasoner.task(1).task;
    assert!(
        second.contains("move_date+move_window -> 2027-11-11 00:00:00"),
        "{second}"
    );
    assert!(reasoner.task(0).task.contains("Tried: []"));
}

#[tokio::test]
async fn the_eval_arm_runs_analysts_and_one_planner_for_a_whole_group() {
    use ekg_cognition::eval::{AgentArm, Arm, PlansFor, TrialEnd};

    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![
            Script {
                steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
                report: Ok(Termination::Submitted),
            },
            Script {
                steps: Vec::new(),
                report: Ok(Termination::NoPlan),
            },
        ],
    ));
    let arm = AgentArm::for_group(stage.group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>));
    let group = PlansFor::Group(stage.group());

    let submitted = arm.trial(&group, &[]).await;
    let TrialEnd::Submitted(proposal) = &submitted.end else {
        panic!("a plan was expected, got {:?}", submitted.end);
    };
    assert_eq!(proposal.actions.len(), 2, "one plan across both members");
    let run = submitted.run.expect("the trial's run record");
    assert_eq!(run.tools, vec!["submit_plan"], "a planner reads nothing");
    assert_eq!(run.role, Some(RunRole::Planner));
    assert_eq!(submitted.other_runs.len(), 2, "one analyst per member");
    assert!(submitted.other_runs.iter().all(|run| run.finding.is_some()));
    assert!(run.skills.contains_key("propose-plan"));
    let claims = stage
        .tokens
        .verify_now(&reasoner.task(0).token)
        .expect("the token");
    assert_eq!(claims.subjects, stage.group().members);

    assert_eq!(arm.trial(&group, &[]).await.end, TrialEnd::Declined);
    // A group arm given a single subject is a wiring fault: not graded, and no run starts.
    let single = arm
        .trial(&PlansFor::Subject(stage.order.clone()), &[])
        .await;
    assert!(
        matches!(single.end, TrialEnd::Errored(_)),
        "{:?}",
        single.end
    );
    assert_eq!(reasoner.calls(), 2);
}

#[tokio::test]
async fn each_member_gets_an_analyst_under_its_own_profile_before_the_planner() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>)
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect("a candidate");

    let analysts = reasoner.analyst_tasks();
    assert_eq!(analysts.len(), 2);
    let mut seen = BTreeMap::new();
    for task in &analysts {
        let claims = stage.tokens.verify_now(&task.token).expect("the token");
        assert_eq!(claims.subjects.len(), 1, "one member per analyst");
        assert!(!claims.tools.contains(&"submit_plan".to_owned()));
        assert_eq!((task.max_steps, task.run_cap), (6, Duration::from_secs(90)));
        seen.insert(claims.subjects[0].clone(), claims.profile);
    }
    assert_eq!(seen[&stage.order], ANALYST);
    assert_eq!(seen[&stage.mo], PLANT_ANALYST);

    // The planner has the findings, from the recorded edges, and none of an analyst's words.
    let planner = reasoner.task(0).task;
    assert!(planner.contains("cause late"), "{planner}");
    assert!(planner.contains("at_risk"), "{planner}");
    assert!(!planner.contains(ANALYST_WORDS), "{planner}");
    assert_eq!(records.analysts().len(), 2);
    assert!(stage.ledger.is_empty(), "every run was read once");
}

#[tokio::test]
async fn later_attempts_of_a_slate_reuse_the_findings() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let proposer = stage.group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>);
    for _ in 0..3 {
        proposer
            .propose_group_candidate(&stage.group(), &[])
            .await
            .expect("a candidate");
    }
    assert_eq!((reasoner.analyst_tasks().len(), reasoner.calls()), (2, 3));

    // A slate of its own reads the members again.
    proposer
        .fresh()
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect("a candidate");
    assert_eq!(reasoner.analyst_tasks().len(), 4);
}

#[tokio::test]
async fn a_failed_analyst_is_run_again_alone_and_no_planner_runs_before_it_hands_in() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    reasoner.fail_analyst(&stage.mo, 1);
    let records = Arc::new(Records::default());
    let proposer = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);

    let error = proposer
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect_err("a member has no finding");
    assert!(
        matches!(&error, EngineError::InvalidProposal(why)
            if why.contains(&stage.mo.0) && why.contains("step_limit")),
        "{error}"
    );
    assert_eq!(reasoner.calls(), 0, "no planner without every finding");
    assert_eq!(records.analysts().len(), 2, "recorded however it ended");

    proposer
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect("a candidate");
    assert_eq!(reasoner.analyst_tasks().len(), 3, "only the missing one");
    assert_eq!(reasoner.calls(), 1);
}

/// Closes once this many more runs have started.
struct ClosesAfter(Mutex<usize>);

#[async_trait]
impl RunGate for ClosesAfter {
    async fn proceed(&self) -> bool {
        let mut left = self.0.lock().expect("left");
        let open = *left > 0;
        *left = left.saturating_sub(1);
        open
    }
}

#[tokio::test]
async fn a_gate_closed_between_the_analysts_and_the_planner_stops_the_planner() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(&stage.url, Vec::new()));
    // The group's own admission and one for each analyst.
    let gate = Arc::new(ClosesAfter(Mutex::new(3)));
    let error = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_gate(gate as Arc<dyn RunGate>)
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect_err("stopped");
    assert!(matches!(error, EngineError::Unavailable(_)), "{error}");
    assert_eq!(reasoner.analyst_tasks().len(), 2);
    assert_eq!(reasoner.calls(), 0);
}

#[tokio::test]
async fn an_analyst_without_a_finding_ends_a_trial_as_exhausted_not_declined() {
    use ekg_cognition::eval::{AgentArm, Arm, PlansFor, TrialEnd};

    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(&stage.url, Vec::new()));
    reasoner.fail_analyst(&stage.order, 4);
    let arm = AgentArm::for_group(stage.group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>));
    let trial = arm.trial(&PlansFor::Group(stage.group()), &[]).await;
    assert_eq!(trial.end, TrialEnd::Exhausted);
    assert!(trial.run.is_none(), "no planner ran");
    assert_eq!(
        trial.other_runs.len(),
        5,
        "one attempt each, and the other member's"
    );
}

#[tokio::test]
async fn a_planner_cut_off_for_length_is_exhausted_and_findings_do_not_cross_trials() {
    use ekg_cognition::eval::{AgentArm, Arm, PlansFor, TrialEnd};

    let stage = Stage::start().await;
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: Vec::new(),
            report: Ok(Termination::Length),
        }],
    ));
    let arm = AgentArm::for_group(stage.group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>));
    let group = PlansFor::Group(stage.group());
    assert_eq!(arm.trial(&group, &[]).await.end, TrialEnd::Exhausted);
    assert_eq!(arm.trial(&group, &[]).await.end, TrialEnd::Exhausted);
    assert_eq!(reasoner.analyst_tasks().len(), 4, "each trial reads again");
}

/// Keeps a run waiting for its permit, and notes how long its token has left when it starts.
struct Queued {
    tokens: Arc<TaskTokens>,
    wait: Duration,
    left: Mutex<Option<(u64, Duration)>>,
}

#[async_trait]
impl Reasoner for Queued {
    async fn reason(&self, task: &ReasoningTask) -> Result<RunReport> {
        let exp = self
            .tokens
            .verify_now(&task.token)
            .expect("a live token")
            .exp;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        *self.left.lock().expect("left") = Some((exp - now, task.run_cap));
        Err(EngineError::Unavailable("not run".to_owned()))
    }

    async fn admit(&self) -> Result<Option<Arc<tokio::sync::OwnedSemaphorePermit>>> {
        tokio::time::sleep(self.wait).await;
        Ok(None)
    }
}

#[tokio::test]
async fn a_token_is_minted_after_the_wait_for_a_permit() {
    let stage = Stage::start().await;
    let reasoner = Arc::new(Queued {
        tokens: Arc::clone(&stage.tokens),
        wait: Duration::from_millis(2100),
        left: Mutex::new(None),
    });
    let _ = stage
        .proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .propose_candidate(&stage.order, &[])
        .await;
    let (left, cap) = reasoner
        .left
        .lock()
        .expect("left")
        .expect("the run started");
    assert!(
        left + 1 >= cap.as_secs() + 30,
        "the wait came out of the token's life: {left} s left of a {cap:?} run"
    );
}

#[tokio::test]
async fn a_decided_group_runs_no_investigator_and_a_round_runs_each_strategy() {
    let stage = Stage::start().await;
    stage.findings.decide(&stage.order, MemberState::AtRisk);
    stage.findings.decide(&stage.mo, MemberState::OnPlan);
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let round = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_strategies(vec![
            "first".to_owned(),
            "second".to_owned(),
            "third".to_owned(),
        ])
        .propose_group_round(&stage.group(), &[], 2)
        .await;
    assert_eq!(round.len(), 2, "no more than the slate's room");
    assert!(round.iter().all(Result::is_ok));
    assert!(reasoner.analyst_tasks().is_empty());
    let mut approaches: Vec<String> = (0..2)
        .map(|n| {
            reasoner
                .task(n)
                .task
                .lines()
                .next()
                .expect("a line")
                .to_owned()
        })
        .collect();
    approaches.sort();
    assert_eq!(
        approaches,
        ["Your approach: first", "Your approach: second"]
    );
}

#[tokio::test]
async fn nothing_a_plan_could_reach_ends_before_a_model() {
    let stage = Stage::start().await;
    stage.findings.decide(&stage.order, MemberState::Immovable);
    stage.findings.decide(&stage.mo, MemberState::OnPlan);
    let reasoner = Arc::new(ScriptedReasoner::new(&stage.url, Vec::new()));
    let error = stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect_err("nothing to do");
    assert!(matches!(error, EngineError::NothingToDo(_)), "{error}");
    assert_eq!(reasoner.calls(), 0);
}

#[tokio::test]
async fn a_claim_the_records_contradict_never_reaches_a_planner() {
    let stage = Stage::start().await;
    stage.findings.decide(&stage.mo, MemberState::AtRisk);
    stage
        .findings
        .contradicted
        .lock()
        .expect("contradicted")
        .push("late".to_owned());
    let reasoner = Arc::new(ScriptedReasoner::new(
        &stage.url,
        vec![Script {
            steps: vec![Step::Submit(stage.joint_plan("2027-11-08", "Move both."))],
            report: Ok(Termination::Submitted),
        }],
    ));
    let records = Arc::new(Records::default());
    stage
        .group_proposer(Arc::clone(&reasoner) as Arc<dyn Reasoner>)
        .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>)
        .propose_group_candidate(&stage.group(), &[])
        .await
        .expect("a candidate");
    assert_eq!(
        reasoner.analyst_tasks().len(),
        1,
        "only the undecided member"
    );
    assert!(!reasoner.task(0).task.contains("cause late"));
    let recorded = records.0.lock().expect("records");
    assert!(recorded.iter().any(|run| {
        run.role == Some(RunRole::Semantic)
            && run.finding.as_ref().is_some_and(|f| f.dropped.len() == 1)
    }));
}
