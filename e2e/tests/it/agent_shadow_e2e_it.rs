//! Gated live e2e for the agent's shadow rollout: one shadow slate with the real model, over
//! the seeded world, isolated from the operator.
//!
//! Like the slate suites, the slate step is composed here, in-process, over the live stack:
//!
//! - the seeded world model in GraphDB (read-only) and the real what-if simulator;
//! - the `AgentRollout` and `AgentShadow` objects the running backend serves (the governed
//!   mode and the shadow store), reached over the ingress;
//! - the live skill registry (the pins, and the gateway's skill source);
//! - the agent tool gateway with its run ledger, served in-process, and the built runtime image
//!   (`localhost/ekg-hayhooks:local`) in a container pointed at it, with the stack's model key.
//!
//! The legacy slate is a canned one attached to an in-test sink, so the suite never writes to
//! the system of record (a legacy plan on the live stack may auto-apply). The suite sets
//! `shadow` (one slate, a short window), runs one point's slates, and asserts isolation:
//!
//! - the point got the legacy slate only, and the live point's record is unchanged;
//! - every shadow plan target is in the task's scope;
//! - the shadow record is in the live store, with its runs and a version tuple with the pins,
//!   and carries the legacy slate the point got;
//! - a second claim is refused with `budget_exhausted`.
//!
//! Then the same step runs in `agent` mode, on a rollout of this run's own (the engine's is
//! never switched to `agent`), entered under a hand-built sign-off of the version the shadow
//! run reported:
//!
//! - the point carries the agent's slate, whose trace ends with the step naming the agent and
//!   its versions; the record says the agent's slate was shown and holds the legacy slate;
//! - with a reasoner that cannot be reached, the point carries the legacy slate, the record
//!   says why, and the rollout counted a failed run.
//!
//! A second test needs no model: a portfolio formed in a `PortfolioService` served here gets the
//! existing joint slate, and in `shadow` (on a rollout of this run's own) a scripted agent's
//! joint slate is recorded beside it, under the portfolio, with both joint slates and then the
//! decision on the portfolio. In `legacy` no record is written.
//!
//! It sets `legacy` in cleanup, even on failure, and prints the quality numbers (first-submit
//! acceptance, rejections, steps, tokens) without asserting them.
//!
//! Ignored by default. Skips if the stack, the runtime image or the model key is unavailable,
//! unless `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), when that fails the run.
//!
//! ```text
//! just podman-build && just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it agent_shadow_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::IngestionConfig;
use ekg_backend::drift::{RoutingSlateProposer, slate_guard};
use ekg_backend::promotion::{Criteria, ShadowEvidence, Verdict, Version};
use ekg_backend::propose::{self, AgentSeams, SUPPLY_RISK_PROFILE};
use ekg_backend::rollout::{
    Claim, Mode, RegisterRequest, RolloutClient, SetModeRequest, SignoffRequest,
};
use ekg_backend::shadow::{ShadowClient, ShadowOutcome, ShadowRecord, Shown};
use ekg_backend::skills::{DEFAULT_SKILL_CACHE_CAPACITY, RegistryClient, RegistrySource};
use ekg_backend::slates::{PointAttach, ShadowRun, ShadowStep, SlateSink, point_slates};
use ekg_cognition::tools::{GatewayConfig, RunLedger, TaskScope, TaskTokens, gateway_router};
use ekg_cognition::{
    DEFAULT_MAX_PLAN_ACTIONS, Reasoner, ReasoningTask, RunReport, RuntimeReasoner, Termination,
};
use ekg_domain::fakes::CannedSlateProposer;
use ekg_domain::{ActionPointId, EngineError, EntityUri, Slate, SlateProposer, WorldModel};
use ekg_kinetic::PolicyEngine;
use ekg_semantic::{GraphDbClient, GraphDbSimulator, GraphDbWorldModel, ontology};
use futures::FutureExt as _;
use serde_json::Value;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

/// The runtime image under test.
const IMAGE: (&str, &str) = ("localhost/ekg-hayhooks", "local");
/// How a container on this machine reaches a listener on the host (Podman).
const HOST_FROM_CONTAINER: &str = "host.containers.internal";
/// The seeded repository the stack reads.
const REPO: &str = "ekg";

/// When the in-test point says its slate was attached.
const ATTACHED_AT: &str = "1700000000";

/// Keeps every slate it was handed (the point, as the legacy path attaches to it).
#[derive(Default)]
struct Point(std::sync::Mutex<Vec<Slate>>);

#[async_trait::async_trait]
impl SlateSink for Point {
    async fn point_has_slate(&self, _subject: &EntityUri) -> Option<bool> {
        Some(false)
    }
    async fn portfolio_current(&self, _id: &str, _generation: u32) -> Option<bool> {
        Some(true)
    }
    async fn attach_point_slate(&self, _subject: &EntityUri, slate: &Slate) -> bool {
        self.0.lock().expect("lock").push(slate.clone());
        true
    }
    async fn point_attach(&self, subject: &EntityUri) -> Option<PointAttach> {
        Some(PointAttach {
            action_point: ActionPointId(format!("action-point/{}", subject.0)),
            point_seq: 1,
            attached_at: ATTACHED_AT.to_owned(),
        })
    }
    async fn attach_portfolio_slate(
        &self,
        _id: &str,
        _generation: u32,
        _slate: &Slate,
    ) -> ekg_backend::slates::PortfolioAttach {
        ekg_backend::slates::PortfolioAttach::Landed
    }
    async fn portfolio_attach(&self, _id: &str, _generation: u32) -> Option<String> {
        None
    }
}

/// The live point's record of `subject` (its `slate` and `proposal` included).
async fn point_record(http: &reqwest::Client, ingress: &str, subject: &str) -> Option<Value> {
    let mut url = reqwest::Url::parse(ingress).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["ActionPointService", subject, "record"]);
    let body = http.post(url).send().await.ok()?.bytes().await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// A seeded supply-risk point's subject, from the live `/api`.
async fn seeded_subject(http: &reqwest::Client, api: &str) -> Option<EntityUri> {
    let body = http
        .get(format!("{api}/api/action-points"))
        .send()
        .await
        .ok()?
        .bytes()
        .await
        .ok()?;
    let points: Value = serde_json::from_slice(&body).ok()?;
    points.as_array()?.iter().find_map(|point| {
        let subject = point["subject"].as_str()?;
        (subject.contains("/sale.order/") && point["state"] != "promoted")
            .then(|| EntityUri(subject.to_owned()))
    })
}

/// The criteria the `agent`-mode part registers. They state no rollback bound, so a failed run
/// is counted and rolls nothing back.
const CRITERIA: &str = "\
version: 1
eval: { set_version: 2, min_trials: 3, min_passed: 14 }
shadow: { min_compared: 20 }
";

/// A runtime that cannot be reached.
struct Unreachable;

#[async_trait::async_trait]
impl Reasoner for Unreachable {
    async fn reason(&self, _task: &ReasoningTask) -> ekg_domain::Result<RunReport> {
        Err(EngineError::Unavailable(
            "the runtime is unreachable".to_owned(),
        ))
    }
}

/// The agent's slate proposer over `seams`, per run.
fn agent_slates(seams: AgentSeams) -> ekg_backend::slates::ShadowSlates {
    Arc::new(move |run: ShadowRun| {
        let (supply_risk, process) = propose::agent_slate_agents(&seams, &run.gate, &run.recorder);
        Arc::new(RoutingSlateProposer::new(
            Arc::new(supply_risk),
            Arc::new(process),
        )) as Arc<dyn SlateProposer>
    })
}

/// The agent's joint slate proposer over `seams`, per run.
fn agent_joint_slates(seams: AgentSeams) -> ekg_backend::slates::JointShadowSlates {
    Arc::new(move |run: ShadowRun| {
        Arc::new(ekg_backend::joint::agent_portfolio_slate_agent(
            &seams,
            &run.gate,
            &run.recorder,
        )) as Arc<dyn ekg_domain::GroupSlateProposer>
    })
}

/// Runs one point's slates with `step` on a point of the test's own; returns the slates the
/// point got and the run's record.
async fn run_point(
    step: &ShadowStep,
    world: &dyn WorldModel,
    subject: &EntityUri,
) -> (Vec<Slate>, ShadowRecord) {
    let point = Point::default();
    let handle = point_slates(
        &point,
        &CannedSlateProposer,
        &PolicyEngine::new("p/v1", std::collections::BTreeSet::new()),
        world,
        subject,
        Some(step),
    )
    .await
    .expect("an agent task");
    let record = handle
        .await
        .expect("agent task")
        .expect("the claim was granted and a record written");
    let attached = point.0.lock().expect("lock").clone();
    (attached, record)
}

/// Whether `slate` is the canned legacy slate for `subject`.
async fn is_legacy(slate: &Slate, subject: &EntityUri) -> bool {
    let legacy = CannedSlateProposer
        .propose_slate(subject)
        .await
        .expect("canned");
    slate.options.len() == legacy.options.len()
        && slate
            .options
            .iter()
            .zip(&legacy.options)
            .all(|(got, want)| got.plan == want.plan)
}

/// The `agent`-mode part: on `own`, a rollout of this run's, entered under a hand-built
/// sign-off of `version`.
async fn agent_mode_shows_the_agents_slate_or_falls_back(
    own: &RolloutClient,
    step: &ShadowStep,
    unreachable: &ShadowStep,
    world: &dyn WorldModel,
    subject: &EntityUri,
    version: Version,
) {
    let by = |who: &str| format!("agent_shadow_e2e_it-{who}");
    own.register_criteria(&RegisterRequest {
        document: CRITERIA.to_owned(),
        by: by("registrant"),
        reason: "agent_shadow_e2e_it: agent mode".to_owned(),
    })
    .await
    .expect("registered");
    let entered = SetModeRequest {
        mode: Mode::Agent {
            until: ekg_cognition::tools::now_secs().expect("now") + 20 * 60,
            max_slates: 2,
            max_tokens: 2_000_000,
        },
        by: by("registrant"),
        reason: "agent_shadow_e2e_it: one agent slate".to_owned(),
    };
    assert!(
        own.set_mode(&entered).await.is_err(),
        "the mode is refused without a sign-off"
    );
    own.sign_off(&SignoffRequest {
        verdict: Verdict {
            criteria: Some(Criteria::digest_of(CRITERIA)),
            met: true,
            checks: Vec::new(),
            eval_missing: None,
            shadow: ShadowEvidence::default(),
            eval_version: Some(version.clone()),
            shadow_versions: Vec::new(),
        },
        report: "hand-built".to_owned(),
        by: by("signer"),
        reason: "agent_shadow_e2e_it: hand-built evidence".to_owned(),
    })
    .await
    .expect("signed off");
    let epoch = own.set_mode(&entered).await.expect("agent").epoch();

    // --- the agent's slate is the point's ---
    let (attached, record) = run_point(step, world, subject).await;
    assert_eq!(attached.len(), 1, "exactly one slate on the point");
    assert_eq!(record.epoch, Some(epoch));
    assert_eq!(record.rollout.as_deref(), Some(own.key()));
    let paired = record
        .paired
        .as_ref()
        .expect("the legacy slate is recorded");
    assert!(is_legacy(paired.slate(), subject).await);
    let ran = record
        .runs
        .iter()
        .any(|run| run.termination != Termination::Error);
    match record
        .shown
        .clone()
        .expect("an agent-mode record says what was shown")
    {
        Shown::Agent => {
            let step = attached[0].trace.steps.last().expect("the added step");
            assert_eq!(step.tag, ekg_domain::TraceTag::Assumption);
            assert!(
                step.text.contains("produced by the tool-using agent")
                    && step.text.contains(&version.model)
                    && step.text.contains("propose-plan@"),
                "{}",
                step.text
            );
            // The point carries the agent's options, with the policy's needs.
            let agent = record.slate.as_ref().expect("the agent's slate");
            assert_eq!(attached[0].options.len(), agent.options.len());
            for (got, want) in attached[0].options.iter().zip(&agent.options) {
                assert_eq!(got.plan, want.plan, "the agent's plan, not the legacy one");
                assert!(!got.needs.is_empty(), "nothing is allowlisted here");
            }
            println!(
                "agent mode: the point got the agent's slate ({} option(s))",
                agent.options.len()
            );
        }
        Shown::Legacy { reason } => {
            // The real model may fail a run; then the point must carry the legacy slate.
            assert!(
                record.outcome != ShadowOutcome::Completed || !ran,
                "a completed run of the signed version was not shown: {reason}"
            );
            assert!(is_legacy(&attached[0], subject).await);
            println!("agent mode: the point got the legacy slate: {reason}");
        }
    }

    // --- a runtime that cannot be reached ---
    let (attached, record) = run_point(unreachable, world, subject).await;
    assert_eq!(attached.len(), 1, "exactly one slate on the point");
    assert!(is_legacy(&attached[0], subject).await, "the legacy slate");
    let shown = record.shown.clone().expect("what was shown");
    assert!(
        matches!(&shown, Shown::Legacy { reason } if reason.starts_with("the agent's run failed")),
        "{shown:?}"
    );
    assert!(record.paired.is_some(), "the slate the point got");
    let state = own.get().await.expect("state");
    // A failed run is counted; had the first run rolled the mode back, the report is stale.
    assert!(
        state.agent_failures == 1 || state.mode == Mode::Legacy,
        "{state:?}"
    );
}

fn set_mode(mode: Mode, reason: &str) -> SetModeRequest {
    SetModeRequest {
        mode,
        by: "agent_shadow_e2e_it".to_owned(),
        reason: reason.to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the running stack, the runtime image and a model key (run with --ignored)"]
async fn one_shadow_slate_runs_isolated_recorded_and_bounded() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let api = common::env_or("EKG_BACKEND_PUBLIC_URL", "http://localhost:8090");
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let http = reqwest::Client::new();
    let model_key = std::env::var("MODEL_API_KEY").unwrap_or_default();
    if model_key.trim().is_empty() {
        common::infra_unavailable("MODEL_API_KEY is unset: the shadow suite needs the real model");
        return;
    }
    let Ok(config) = IngestionConfig::from_env() else {
        common::infra_unavailable("the stack's config is not in the environment (source .env)");
        return;
    };
    let rollout = RolloutClient::new(http.clone(), ingress.clone());
    if rollout.get().await.is_err() {
        common::infra_unavailable(&format!("AgentRollout unreachable over {ingress}"));
        return;
    }
    let Some(subject) = seeded_subject(&http, &api).await else {
        common::infra_unavailable(&format!("no seeded supply-risk point at {api}"));
        return;
    };

    // The seeded world, read-only, under the stack's read policy; the what-if simulator.
    let world = Arc::new(GraphDbWorldModel::new_with_read_policy(
        GraphDbClient::new(common::graphdb_url(), REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
        config.read_policy(),
    )) as Arc<dyn WorldModel>;
    let simulator = Arc::new(GraphDbSimulator::new(
        GraphDbClient::new(common::graphdb_url(), REPO).expect("sim client"),
    )) as Arc<dyn ekg_domain::Simulator>;

    // The gateway with its ledger and the live skill registry, in-process.
    let profiles =
        Arc::new(propose::tool_profiles(&world, DEFAULT_MAX_PLAN_ACTIONS).expect("profiles"));
    let tokens = Arc::new(TaskTokens::random().expect("secret"));
    let ledger = Arc::new(RunLedger::default());
    let registry = RegistryClient::new(http.clone(), ingress.clone());
    let router = gateway_router(
        Arc::clone(&profiles),
        Arc::clone(&tokens),
        GatewayConfig::default()
            .allow_host(HOST_FROM_CONTAINER)
            .with_run_ledger(Arc::clone(&ledger))
            .with_skill_source(Arc::new(RegistrySource::new(
                registry.clone(),
                DEFAULT_SKILL_CACHE_CAPACITY,
            ))),
    );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind");
    let gateway_port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });

    // The runtime, with the stack's model, reading through that gateway.
    let mut request = GenericImage::new(IMAGE.0, IMAGE.1)
        .with_exposed_port(1416.tcp())
        .with_wait_for(WaitFor::message_on_either_std(
            "Application startup complete",
        ))
        .with_env_var("MODEL_API_KEY", model_key)
        .with_env_var(
            "EKG_TOOL_GATEWAY_URL",
            format!("http://{HOST_FROM_CONTAINER}:{gateway_port}"),
        )
        .with_env_var("HAYHOOKS_DURABLE_MODE", "false")
        .with_env_var("HAYSTACK_CONTENT_TRACING_ENABLED", "false")
        .with_startup_timeout(Duration::from_secs(120));
    for var in ["MODEL_BASE_URL", "CHAT_MODEL", "AGENT_MODEL"] {
        if let Some(value) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
            request = request.with_env_var(var, value);
        }
    }
    let container = match request.start().await {
        Ok(container) => container,
        Err(why) => {
            common::infra_unavailable(&format!(
                "no {}:{} runtime image (run `just podman-build`): {why}",
                IMAGE.0, IMAGE.1
            ));
            return;
        }
    };
    let runtime = format!(
        "http://{}:{}",
        container.get_host().await.expect("host"),
        container
            .get_host_port_ipv4(1416.tcp())
            .await
            .expect("mapped 1416")
    );

    let (limits, joint_limits) = (config.agent_limits(), config.agent_joint_limits());
    let seams = AgentSeams {
        world: Arc::clone(&world),
        simulator,
        supply_risk: profiles.get(SUPPLY_RISK_PROFILE).expect("supply-risk"),
        process: profiles.get(propose::PROCESS_PROFILE).expect("process"),
        portfolio: profiles.get(propose::PORTFOLIO_PROFILE).expect("portfolio"),
        rules: ekg_backend::drift::monitor_rules(),
        tokens,
        reasoner: Arc::new(
            RuntimeReasoner::new(&runtime, std::num::NonZeroUsize::MIN).expect("reasoner"),
        ),
        ledger,
        pins: Arc::new(registry),
        limits,
        joint_limits,
        analyst_limits: config.agent_analyst_limits(),
        supply_risk_analyst: profiles
            .get(propose::SUPPLY_RISK_ANALYST_PROFILE)
            .expect("supply-risk analyst"),
        process_analyst: profiles
            .get(propose::PROCESS_ANALYST_PROFILE)
            .expect("process analyst"),
        findings: Arc::new(propose::SemanticFindings::new(
            Arc::clone(&world),
            profiles
                .get(propose::SUPPLY_RISK_ANALYST_PROFILE)
                .expect("supply-risk analyst"),
            profiles
                .get(propose::PROCESS_ANALYST_PROFILE)
                .expect("process analyst"),
            config.agent_text_max_chars,
        )),
        planner_strategies: propose::planner_strategies(config.agent_planners),
        planner_reads: config.agent_planner_reads,
        text_max_chars: config.agent_text_max_chars,
        budget: config.slate_budget,
    };
    let step = ShadowStep {
        rollout: Arc::new(rollout.clone()),
        store: Arc::new(ShadowClient::new(http.clone(), ingress.clone())),
        slates: agent_slates(seams.clone()),
        joint: agent_joint_slates(seams.clone()),
        permits: Arc::new(tokio::sync::Semaphore::new(1)),
        guard: slate_guard(config.slate_budget, limits.run_cap),
        joint_guard: slate_guard(
            config.slate_budget,
            config.agent_analyst_limits().run_cap + joint_limits.run_cap,
        ),
    };
    // A rollout of this run's own for the `agent` mode: the engine's is never switched to it.
    let own = RolloutClient::new(http.clone(), ingress.clone())
        .under(format!("agent-shadow-e2e-{}", common::unique_base()));
    let agent_step = ShadowStep {
        rollout: Arc::new(own.clone()),
        ..step.clone()
    };
    let unreachable_step = ShadowStep {
        slates: agent_slates(AgentSeams {
            reasoner: Arc::new(Unreachable),
            ..seams
        }),
        ..agent_step.clone()
    };

    let body = async {
        rollout
            .set_mode(&set_mode(
                Mode::Shadow {
                    until: ekg_cognition::tools::now_secs().expect("now") + 20 * 60,
                    max_slates: 1,
                    max_tokens: 2_000_000,
                },
                "agent_shadow_e2e_it: one shadow slate",
            ))
            .await
            .expect("shadow");

        let point = Point::default();
        let handle = point_slates(
            &point,
            &CannedSlateProposer,
            &PolicyEngine::new("p/v1", std::collections::BTreeSet::new()),
            world.as_ref(),
            &subject,
            Some(&step),
        )
        .await
        .expect("a shadow task");
        let record = handle
            .await
            .expect("shadow task")
            .expect("the claim was granted and a record written");

        // Isolation: the point got the legacy slate only; the live point is unchanged.
        let attached = point.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1, "one slate on the point");
        let legacy = CannedSlateProposer
            .propose_slate(&subject)
            .await
            .expect("canned");
        assert_eq!(attached[0].options.len(), legacy.options.len());
        for (got, want) in attached[0].options.iter().zip(&legacy.options) {
            assert_eq!(got.plan, want.plan, "the legacy plan, not the agent's");
        }
        // No trace of the shadow run on the live point (the stack may attach its own slate
        // meanwhile; the shadow one never lands there).
        let live = point_record(&http, &ingress, &subject.0)
            .await
            .map(|record| record.to_string())
            .unwrap_or_default();
        for run in &record.runs {
            assert!(
                !live.contains(&run.trace_id),
                "a shadow run reached the live point"
            );
        }
        if let Some(slate) = record.slate.as_ref().filter(|s| !s.options.is_empty()) {
            let shadow = serde_json::to_value(slate).expect("json");
            assert!(
                point_record(&http, &ingress, &subject.0)
                    .await
                    .is_none_or(|record| record["slate"] != shadow),
                "the shadow slate reached the live point"
            );
        }

        // Every shadow plan target is in the task's scope.
        let profile = step_profile(&profiles);
        let scope = TaskScope::resolve(
            std::slice::from_ref(&subject),
            &profile.reader(Arc::clone(&world)),
        )
        .await
        .expect("scope");
        if let Some(slate) = &record.slate {
            for option in &slate.options {
                for action in &option.plan.actions {
                    assert!(
                        scope.contains(&action.target),
                        "{} is outside the task's scope",
                        action.target
                    );
                }
            }
        }

        // The record is complete and in the live store.
        assert!(
            matches!(
                record.outcome,
                ShadowOutcome::Completed | ShadowOutcome::Failed
            ),
            "{record:?}"
        );
        assert!(!record.runs.is_empty(), "every candidate attempt recorded");
        assert!(
            record.version.skills.contains_key("propose-plan"),
            "{:?}",
            record.version
        );
        assert!(record.version.tools.iter().any(|t| t == "submit_plan"));
        assert_eq!(record.version.engine, env!("CARGO_PKG_VERSION"));
        // It carries the slate the point got, apart from the agent's.
        let paired = record.paired.as_ref().expect("the paired slate");
        assert_eq!(*paired.slate(), attached[0], "the slate as attached");
        assert_eq!(paired.attached_at(), ATTACHED_AT);
        assert_eq!(record.paired_missing, None);
        let stored = ShadowClient::new(http.clone(), ingress.clone())
            .records(&subject)
            .await
            .expect("records");
        assert!(
            stored
                .iter()
                .any(|r| r.run_id == record.run_id && r.paired == record.paired),
            "the record is in the live shadow store, with its paired slate"
        );

        // The budget: a second claim is refused, and the mode says why.
        assert_eq!(
            rollout
                .claim_slate(ekg_backend::rollout::SlateKind::Point)
                .await
                .expect("claim"),
            Claim::Refused {
                reason: "budget_exhausted".to_owned()
            }
        );

        // Quality, reported but not asserted.
        let first_submit = record
            .runs
            .iter()
            .filter(|r| r.termination == Termination::Submitted && r.rejections.is_empty())
            .count();
        let rejections: usize = record.runs.iter().map(|r| r.rejections.len()).sum();
        let steps: u32 = record.runs.iter().map(|r| r.steps).sum();
        let tokens: u64 = record
            .runs
            .iter()
            .map(|r| r.input_tokens + r.output_tokens)
            .sum();
        println!(
            "agent shadow quality for {}: {} run(s), {first_submit} accepted on first submit, \
             {rejections} rejection(s), {steps} step(s), {tokens} token(s), {} option(s), \
             model {:?}, outcome {:?}",
            subject.0,
            record.runs.len(),
            record.slate.as_ref().map_or(0, |s| s.options.len()),
            record.version.model,
            record.outcome
        );
        for run in &record.runs {
            println!(
                "  run {}: {} after {} step(s) in {:.1}s",
                run.trace_id,
                run.termination,
                run.steps,
                Duration::from_millis(run.elapsed_ms).as_secs_f64()
            );
        }
        for step in record.slate.iter().flat_map(|s| &s.trace.steps) {
            if step.tag == ekg_domain::TraceTag::Generation {
                println!("  {}", step.text);
            }
        }

        // The `agent` mode needs the version the evidence is of: the one this run reported.
        if record.version.model.is_empty() {
            println!("agent mode: skipped, the shadow run reported no model to sign for");
        } else {
            let version = Version {
                engine: record.version.engine.clone(),
                model: record.version.model.clone(),
                skills: record.version.skills.clone(),
            };
            agent_mode_shows_the_agents_slate_or_falls_back(
                &own,
                &agent_step,
                &unreachable_step,
                world.as_ref(),
                &subject,
                version,
            )
            .await;
        }
    };
    let outcome = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    // Cleanup, even on failure: the kill switch.
    let cleanup = rollout
        .set_mode(&set_mode(Mode::Legacy, "agent_shadow_e2e_it: cleanup"))
        .await;
    let _ = own
        .set_mode(&set_mode(Mode::Legacy, "agent_shadow_e2e_it: cleanup"))
        .await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    cleanup.expect("legacy set in cleanup");
}

/// The supply-risk profile the seeded subject routes to.
fn step_profile(
    profiles: &ekg_cognition::tools::ProfileRegistry,
) -> Arc<ekg_cognition::tools::ToolProfile> {
    profiles.get(SUPPLY_RISK_PROFILE).expect("supply-risk")
}

/// A slate step whose agent only drafts joint slates, scripted.
fn joint_shadow_step(rollout: &RolloutClient, http: &reqwest::Client, ingress: &str) -> ShadowStep {
    ShadowStep {
        rollout: Arc::new(rollout.clone()),
        store: Arc::new(ShadowClient::new(http.clone(), ingress.to_owned())),
        slates: Arc::new(|_run| panic!("no point is planned for here")),
        joint: common::scripted_joint_slates("scripted/1", "v1"),
        permits: Arc::new(tokio::sync::Semaphore::new(1)),
        guard: Duration::from_secs(20),
        joint_guard: Duration::from_secs(20),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn a_formed_portfolio_is_shadowed_in_shadow_mode_and_not_in_legacy() {
    use ekg_backend::rollout::AgentRollout;
    use ekg_backend::shadow::{AGENT_SHADOW_OBJECT, AgentShadow, DecisionState, Paired};
    use ekg_backend::slates::{IngressSink, PortfolioSlateOutcome, portfolio_slates};
    use ekg_domain::fakes::{CannedGroupSlateProposer, InMemoryWorldModel};
    use ekg_dynamic::{PortfolioEvent, PortfolioService};
    use restate_sdk::prelude::Endpoint;

    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let (service_addr, callback_url) = (common::service_addr(22), common::callback_url(22));
    let http = reqwest::Client::new();
    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        common::infra_unavailable(&format!("Restate admin unreachable at {admin_url}"));
        return;
    }
    let endpoint = Endpoint::builder()
        .bind(AgentRollout)
        .bind(AgentShadow)
        .bind(PortfolioService::default().with_outcome_listener(AGENT_SHADOW_OBJECT))
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    common::register(&http, &admin_url, &callback_url)
        .await
        .expect("register");

    let base = common::unique_base();
    // A rollout of this run's own: the engine's, under its fixed key, is never switched.
    let rollout = RolloutClient::new(http.clone(), ingress.clone())
        .under(format!("agent-shadow-joint-{base}"));
    let shadow = ShadowClient::new(http.clone(), ingress.clone());
    let step = joint_shadow_step(&rollout, &http, &ingress);
    let sink = IngressSink::new(http.clone(), ingress.clone());
    let policy = PolicyEngine::new("p/v1", std::collections::BTreeSet::new());
    let world = InMemoryWorldModel::default();
    let member =
        |model: &str, n: i64| EntityUri(format!("https://ekg.example/id/{model}/{}", base + n));

    let body = async {
        // --- shadow: the existing joint slate on the portfolio, the agent's recorded only ---
        rollout
            .set_mode(&set_mode(
                Mode::Shadow {
                    until: ekg_cognition::tools::now_secs().expect("now") + 20 * 60,
                    max_slates: 2,
                    max_tokens: 1_000_000,
                },
                "agent_shadow_e2e_it: one joint shadow slate",
            ))
            .await
            .expect("shadow");
        let formed = common::form_portfolio(
            &http,
            &ingress,
            &[member("sale.order", 1), member("mrp.production", 1)],
        )
        .await;
        let (outcome, handle) = portfolio_slates(
            &sink,
            &CannedGroupSlateProposer,
            &policy,
            &world,
            &formed,
            Some(&step),
        )
        .await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        let written = handle
            .expect("an agent task")
            .await
            .expect("the agent's task")
            .expect("a joint shadow record");

        let portfolio = common::portfolio_record(&http, &ingress, &formed.id.0).await;
        let on_portfolio = portfolio
            .slate
            .clone()
            .expect("the portfolio's joint slate");
        assert!(
            !common::is_scripted_agents(&on_portfolio),
            "the operator sees only the existing joint slate"
        );
        let attached = portfolio
            .audit
            .iter()
            .filter(|entry| entry.event == PortfolioEvent::SlateAttached)
            .count();
        assert_eq!(attached, 1, "one joint slate was attached");

        let records = shadow
            .records_under(&formed.id.0)
            .await
            .expect("the portfolio's shadow records");
        assert_eq!(records, vec![written.clone()], "kept under the portfolio");
        let record = &records[0];
        let group = record.group.as_ref().expect("it names its portfolio");
        assert_eq!(group.portfolio, formed.id.0);
        assert_eq!(group.generation, formed.generation);
        assert_eq!(group.members, formed.members);
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.shown, None, "a shadow run");
        assert_eq!(record.runs.len(), 1);
        let agent = record.slate.as_ref().expect("the agent's joint slate");
        assert!(common::is_scripted_agents(agent));
        let Some(Paired::Joint(paired)) = &record.paired else {
            panic!("the existing joint slate is paired: {:?}", record.paired);
        };
        assert_eq!(
            paired.slate, on_portfolio,
            "the slate the portfolio carries"
        );
        assert_eq!(
            Some(paired.attached_at.as_str()),
            portfolio.slate_attached_at.as_deref()
        );
        assert_eq!(
            (paired.portfolio.as_str(), paired.generation),
            (formed.id.0.as_str(), formed.generation)
        );
        let state = rollout.get().await.expect("state");
        assert_eq!(state.slates_started, 1, "it counted against the budget");
        assert_eq!(state.tokens_used, 120, "and so did its tokens");

        // --- the decision on the portfolio reaches the record ---
        common::post_object(
            &http,
            &ingress,
            ["PortfolioService", &formed.id.0, "dismiss"],
            Some(&serde_json::json!({
                "generation": formed.generation,
                "reviewer": "planner-1",
                "reason": "agent_shadow_e2e_it: not this plan",
            })),
        )
        .await
        .expect("dismissed");
        let decided = common::wait_until(Duration::from_secs(20), async || {
            shadow
                .records_under(&formed.id.0)
                .await
                .is_ok_and(|records| records.iter().any(|record| record.decision.is_some()))
        })
        .await;
        assert!(decided, "the portfolio's outcome reached the shadow store");
        let records = shadow.records_under(&formed.id.0).await.expect("records");
        let decision = records[0].decision.as_ref().expect("a decision");
        assert_eq!(decision.state, DecisionState::Dismissed);
        assert_eq!(decision.reviewer.as_deref(), Some("planner-1"));
        assert!(decision.on_paired_slate);

        // --- legacy: the joint slate as always, and no agent run ---
        rollout
            .set_mode(&set_mode(
                Mode::Legacy,
                "agent_shadow_e2e_it: the joint part in legacy",
            ))
            .await
            .expect("legacy");
        let formed = common::form_portfolio(
            &http,
            &ingress,
            &[member("sale.order", 2), member("mrp.production", 2)],
        )
        .await;
        let (outcome, handle) = portfolio_slates(
            &sink,
            &CannedGroupSlateProposer,
            &policy,
            &world,
            &formed,
            Some(&step),
        )
        .await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        let written = handle.expect("the claim's task").await.expect("joined");
        assert_eq!(written, None, "no agent run starts in legacy");
        let portfolio = common::portfolio_record(&http, &ingress, &formed.id.0).await;
        assert!(portfolio.slate.is_some(), "the existing joint slate");
        let records = shadow.records_under(&formed.id.0).await.expect("records");
        assert!(records.is_empty(), "{records:?}");
    };
    let outcome = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    let _ = rollout
        .set_mode(&set_mode(Mode::Legacy, "agent_shadow_e2e_it: cleanup"))
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
