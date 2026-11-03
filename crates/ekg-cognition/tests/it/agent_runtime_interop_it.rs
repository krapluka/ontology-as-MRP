//! Gated interop suite for the runtime's `agent` pipeline: the built runtime image
//! (`localhost/ekg-hayhooks:local`, from `just podman-build`) serves `POST /agent/run`; its
//! model is an in-test fake OpenAI provider that scripts the agent's tool calls, and its tools
//! are the engine's real gateway, served in-process with a run ledger and an in-memory skill
//! source.
//!
//! The scripted model calls `load_skill`, `ground_entity`, a `submit_plan` the catalog rejects,
//! then one it accepts. The suite asserts the runtime's answer (`submitted`, the steps, the
//! summed tokens, the model) and the gateway's ledger (the accepted plan and the one rejection).
//! A request without a bearer is refused with 401; a forged token answers `error` having read
//! nothing; and the runtime's logs never contain a token the suite sent.
//!
//! A second test runs the same script as the engine does, through the agent proposer and the
//! served runtime's reasoner, with both sides exporting spans: the engine's to an in-memory
//! exporter, the runtime's over OTLP/HTTP to a collector double. It asserts that they are one
//! trace, that the runtime's spans hang under the engine's run span, and that neither side's
//! spans carry the task, a tool's arguments or result, the task token or the model credential.
//!
//! A third test is a joint run: a profile composed of two, a token of two subjects, and a
//! scripted model that calls a tool of each member and hands in one plan across both catalogs.
//!
//! Ignored by default. A missing container runtime or image is reported through the shared
//! skip helper, so `just test-e2e` (which sets `EKG_E2E_REQUIRE_INFRA`) fails instead of
//! passing on a skip. Run with:
//!
//! ```text
//! just podman-build
//! cargo test -p ekg-cognition --test it agent_runtime_interop -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown)]

use crate::loud_skip;
use crate::runtime_contract_it::{collector, contains, hex_bytes};
use crate::spans;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::extract::State;
use axum::routing::post;
use ekg_cognition::skills::SkillBundle;
use ekg_cognition::tools::{
    ActionSpec, AgentTool, CallRecord, CauseKind, GatewayConfig, JoinedCatalog, ParamSpec,
    PlanCatalog, ProfileRegistry, ReadOnlyWorld, RunLedger, SkillPin, SkillSource, SubmitFinding,
    SubmitPlan, TaskGrant, TaskScope, TaskTokens, ToolError, ToolProfile, gateway_router,
};
use ekg_cognition::{
    AgentLimits, AgentProposer, Reasoner, ReasoningTask, RunRecord, RunRecorder, RunReport,
    RuntimeReasoner, SkillPins, TaskBuilder,
};
use ekg_domain::fakes::InMemoryWorldModel;
use ekg_domain::{
    CandidateProposer, EntityUri, ProposedAction, ReadContext, SourceRecord, SourceRef, WorldModel,
};
use opentelemetry::trace::FutureExt as _;
use serde_json::{Value, json};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// The image under test.
const IMAGE: (&str, &str) = ("localhost/ekg-hayhooks", "local");
/// How a container on this machine reaches a listener on the host (Podman).
const HOST_FROM_CONTAINER: &str = "host.containers.internal";
/// The agent's model, as the runtime is configured. Recognizable in its answer.
const AGENT_MODEL: &str = "interop-agent-model";
const PROFILE: &str = "interop";
const TRACE: &str = "5bf92f3577b34da6a3ce929d0e0e4737";
/// The runtime's model credential. Recognizable, so a leak of it can be searched for.
const MODEL_KEY: &str = "interop-dummy-key-MARKER-CREDENTIAL";
/// Text only a tool's result holds: a field of the order the agent grounds.
const RESULT_MARKER: &str = "MARKER-IN-A-TOOL-RESULT";
/// Text only the task holds.
const TASK_MARKER: &str = "MARKER-IN-THE-TASK-TEXT";
/// The usage every scripted reply reports.
const PROMPT_TOKENS: u64 = 100;
const COMPLETION_TOKENS: u64 = 20;

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
    ) -> Result<ProposedAction, Vec<String>> {
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

/// One skill version, served by version.
struct OneSkill(Arc<SkillBundle>);

#[async_trait]
impl SkillSource for OneSkill {
    async fn version(&self, version: &str) -> Result<Option<Arc<SkillBundle>>, String> {
        Ok((self.0.digest() == version).then(|| Arc::clone(&self.0)))
    }
}

/// The fake provider's state: the subject the script plans for, and how many chat requests it
/// has answered.
#[derive(Clone)]
struct Provider {
    subject: String,
    requests: Arc<AtomicUsize>,
}

/// An OpenAI chat completion carrying one tool call, or a plain text answer.
fn completion(call: Option<(&str, Value)>) -> Value {
    let (message, finish) = match call {
        Some((name, args)) => (
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{name}"),
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }]
            }),
            "tool_calls",
        ),
        None => (json!({ "role": "assistant", "content": "done" }), "stop"),
    };
    json!({
        "id": "chatcmpl-interop",
        "object": "chat.completion",
        "created": 0,
        "model": AGENT_MODEL,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": {
            "prompt_tokens": PROMPT_TOKENS,
            "completion_tokens": COMPLETION_TOKENS,
            "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS
        }
    })
}

/// The task the scripted model answers in plain text at once, without a tool call.
const DECLINE_TASK: &str = "Plan for the order. Nothing can be done about it.";
/// The task the scripted model answers as an analyst.
const FINDING_TASK: &str = "Analyse the order.";
/// The task whose answer the scripted provider cuts off for length.
const CUT_OFF_TASK: &str = "Plan for the order, at length.";
/// The analyst profile of [`stage`].
const ANALYST_PROFILE: &str = "interop-analyst";

/// The scripted model: its next tool call follows from how many tool results it has seen.
async fn chat(State(provider): State<Provider>, Json(body): Json<Value>) -> Json<Value> {
    provider.requests.fetch_add(1, Ordering::SeqCst);
    let asked = |task: &str| {
        body["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|m| m["role"] == "user" && m["content"].to_string().contains(task))
        })
    };
    if asked(DECLINE_TASK) {
        return Json(completion(None));
    }
    if asked(CUT_OFF_TASK) {
        let mut cut_off = completion(None);
        cut_off["choices"][0]["finish_reason"] = json!("length");
        return Json(cut_off);
    }
    let seen = body["messages"].as_array().map_or(0, |messages| {
        messages.iter().filter(|m| m["role"] == "tool").count()
    });
    let subject = provider.subject.as_str();
    if asked(FINDING_TASK) {
        let finding = |kind: &str| {
            json!({
                "member": subject,
                "state": "on_plan",
                "causes": [{ "kind": kind, "entities": [], "evidence": [] }]
            })
        };
        return Json(completion(match seen {
            0 => Some(("ground_entity", json!({ "entity": subject }))),
            1 => Some(("submit_finding", finding("no_such_kind"))),
            2 => Some((
                "submit_finding",
                json!({ "member": subject, "state": "on_plan", "causes": [] }),
            )),
            _ => None,
        }));
    }
    let plan = |params: Value, explanation: &str| {
        json!({
            "actions": [{ "action_type": "move_date", "target": subject, "params": params }],
            "explanation": explanation
        })
    };
    Json(completion(match seen {
        0 => Some(("load_skill", json!({ "name": "propose-plan" }))),
        1 => Some(("ground_entity", json!({ "entity": subject }))),
        2 => Some(("submit_plan", plan(json!({}), "first try"))),
        3 => Some((
            "submit_plan",
            plan(json!({ "new_date": "2027-11-08" }), "Move the promise."),
        )),
        _ => None,
    }))
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
            .collect(),
    }
}

/// Serve `router` on `0.0.0.0` (reachable from a container); its port.
async fn serve(router: axum::Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    port
}

async fn logs(container: &ContainerAsync<GenericImage>) -> String {
    let out = container.stdout_to_vec().await.unwrap_or_default();
    let err = container.stderr_to_vec().await.unwrap_or_default();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    )
}

/// What a run reads and what it is graded from: an in-memory world with one late order, one
/// profile with `submit_plan`, a pinned skill, the gateway with a run ledger, and the scripted
/// provider.
struct Stage {
    world: Arc<InMemoryWorldModel>,
    subject: EntityUri,
    profile: Arc<ToolProfile>,
    /// The analyst's read side of `profile`.
    analyst: Arc<ToolProfile>,
    pin: SkillPin,
    tokens: Arc<TaskTokens>,
    ledger: Arc<RunLedger>,
    calls: Arc<Mutex<Vec<CallRecord>>>,
    requests: Arc<AtomicUsize>,
    gateway_port: u16,
    provider_port: u16,
}

async fn stage() -> Stage {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let world = Arc::new(InMemoryWorldModel::default());
    let order = record(
        "sale.order",
        1,
        &[
            ("state", "late"),
            ("supplier", "CellTech"),
            ("note", RESULT_MARKER),
        ],
    );
    world.materialize(&order).await.expect("materialize");
    let subject: EntityUri = InMemoryWorldModel::uri_of(&order.source_ref);
    let profile = ToolProfile::new(
        PROFILE,
        ReadContext {
            role: "planner".to_owned(),
            purpose: "interop".to_owned(),
        },
        None,
    )
    .with_tool(Arc::new(SubmitPlan::new(Arc::new(MoveDate), 3)));
    let analyst = ToolProfile::new(
        ANALYST_PROFILE,
        ReadContext {
            role: "planner".to_owned(),
            purpose: "interop".to_owned(),
        },
        None,
    )
    .with_tool(Arc::new(SubmitFinding::new(vec![CauseKind::new(
        "late",
        "Something arrives late.",
    )])));
    let bundle = Arc::new(
        SkillBundle::parse(
            "propose-plan",
            BTreeMap::from([(
                "SKILL.md".to_owned(),
                format!(
                    "---\nname: propose-plan\ndescription: Plan a remediation.\nprofiles: [{PROFILE}]\ntools: [submit_plan]\n---\nGround the subject, then submit_plan."
                ),
            )]),
        )
        .expect("skill"),
    );
    let pin = SkillPin {
        name: "propose-plan".to_owned(),
        version: bundle.digest(),
        profiles: vec![PROFILE.to_owned()],
    };
    let tokens = Arc::new(TaskTokens::random().expect("secret"));
    let mut registry = ProfileRegistry::new(Arc::clone(&world) as Arc<dyn WorldModel>);
    registry.register(profile).expect("register");
    registry.register(analyst).expect("register");
    let registry = Arc::new(registry);
    let profile = registry.get(PROFILE).expect("profile");
    let analyst = registry.get(ANALYST_PROFILE).expect("analyst");
    let ledger = Arc::new(RunLedger::default());
    let calls: Arc<Mutex<Vec<CallRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let log = {
        let calls = Arc::clone(&calls);
        Arc::new(move |record: &CallRecord| calls.lock().expect("log").push(record.clone()))
    };
    let gateway_port = serve(gateway_router(
        registry,
        Arc::clone(&tokens),
        GatewayConfig::default()
            .allow_host(HOST_FROM_CONTAINER)
            .with_call_log(log)
            .with_run_ledger(Arc::clone(&ledger))
            .with_skill_source(Arc::new(OneSkill(bundle))),
    ))
    .await;
    let provider = Provider {
        subject: subject.0.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
    };
    let requests = Arc::clone(&provider.requests);
    let provider_port = serve(
        axum::Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(provider),
    )
    .await;
    Stage {
        world,
        subject,
        profile,
        analyst,
        pin,
        tokens,
        ledger,
        calls,
        requests,
        gateway_port,
        provider_port,
    }
}

/// Start the runtime over `stage`: its model is the fake provider, its tools the gateway.
/// `None` (after a loud skip) when there is no container runtime or no image.
async fn runtime(stage: &Stage, extra: &[(&str, String)]) -> Option<ContainerAsync<GenericImage>> {
    let mut request = GenericImage::new(IMAGE.0, IMAGE.1)
        .with_exposed_port(1416.tcp())
        .with_wait_for(WaitFor::message_on_either_std(
            "Application startup complete",
        ))
        .with_env_var("MODEL_API_KEY", MODEL_KEY)
        .with_env_var(
            "MODEL_BASE_URL",
            format!("http://{HOST_FROM_CONTAINER}:{}/v1", stage.provider_port),
        )
        .with_env_var("AGENT_MODEL", AGENT_MODEL)
        .with_env_var(
            "EKG_TOOL_GATEWAY_URL",
            format!("http://{HOST_FROM_CONTAINER}:{}", stage.gateway_port),
        )
        .with_env_var("HAYHOOKS_DURABLE_MODE", "false")
        .with_env_var("HAYSTACK_CONTENT_TRACING_ENABLED", "false")
        .with_startup_timeout(Duration::from_secs(90));
    for (key, value) in extra {
        request = request.with_env_var(*key, value.clone());
    }
    match request.start().await {
        Ok(container) => Some(container),
        Err(why) => {
            loud_skip::infra_unavailable(&format!(
                "no container runtime or no {}:{} image (run `just podman-build`): {why}",
                IMAGE.0, IMAGE.1
            ));
            None
        }
    }
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
#[allow(clippy::too_many_lines)] // one runtime container, every runtime promise in order
async fn the_runtime_agent_submits_through_the_gateway_and_reports_its_run() {
    let stage = stage().await;
    let Stage {
        ledger,
        calls,
        requests,
        ..
    } = &stage;
    let mint = || {
        stage
            .tokens
            .mint(
                &stage.profile,
                TaskGrant::new(
                    vec![stage.subject.clone()],
                    vec![
                        "ground_entity".to_owned(),
                        "related_entities".to_owned(),
                        "submit_plan".to_owned(),
                    ],
                    Duration::from_secs(300),
                    TRACE,
                )
                .with_skills(vec![stage.pin.clone()]),
            )
            .expect("mint")
    };
    let token = mint();
    let jti = stage.tokens.verify_now(&token).expect("verify").jti;
    let Some(container) = runtime(&stage, &[]).await else {
        return;
    };
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(1416.tcp())
        .await
        .expect("mapped 1416");
    let url = format!("http://{host}:{port}/agent/run");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("client");
    let run_task = |bearer: Option<String>, task: &str| {
        let mut request = http
            .post(&url)
            .header("content-type", "application/json")
            .body(json!({ "task": task, "max_steps": 6 }).to_string());
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        request.send()
    };
    let run = |bearer: Option<String>| run_task(bearer, "Plan for the order. Load propose-plan.");

    // A run with the task's token: submitted on the second submission.
    let response = run(Some(token.clone())).await.expect("run");
    assert!(response.status().is_success(), "{}", response.status());
    let body: Value = serde_json::from_str(&response.text().await.expect("body")).expect("json");
    let report = &body["result"];
    let container_logs = logs(&container).await;
    assert_eq!(
        report["termination"], "submitted",
        "{body}\n{container_logs}"
    );
    assert_eq!(report["steps"], 4, "{body}");
    assert_eq!(report["input_tokens"], 4 * PROMPT_TOKENS, "{body}");
    assert_eq!(report["output_tokens"], 4 * COMPLETION_TOKENS, "{body}");
    assert_eq!(report["model"], AGENT_MODEL, "{body}");
    assert!(
        report.get("closing_answer").is_none(),
        "a submitted run has its plan, not a closing answer: {body}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "one model call per step"
    );

    // The gateway's ledger: what the run did, and the plan it handed in.
    let ran = ledger.take(&jti).expect("the run's ledger record");
    assert_eq!(ran.calls("ground_entity", "ok"), 1, "{ran:?}");
    assert_eq!(ran.calls("submit_plan", "rejected"), 1, "{ran:?}");
    assert_eq!(ran.calls("submit_plan", "ok"), 1, "{ran:?}");
    assert_eq!(ran.rejections.len(), 1);
    assert!(ran.rejections[0][0].contains("new_date"), "{ran:?}");
    let plan = ran.accepted.expect("the accepted plan");
    assert_eq!(plan.explanation, "Move the promise.");
    assert_eq!(
        plan.actions[0].params.get("new_date").map(String::as_str),
        Some("2027-11-08 00:00:00")
    );

    // A run the model ends in plain text: no plan, and the answer says what it said last.
    let response = run_task(Some(mint()), DECLINE_TASK).await.expect("run");
    let body: Value = serde_json::from_str(&response.text().await.expect("body")).expect("json");
    assert_eq!(body["result"]["termination"], "no_plan", "{body}");
    assert_eq!(body["result"]["closing_answer"], "done", "{body}");
    let model_calls = requests.load(Ordering::SeqCst);
    assert_eq!(model_calls, 5, "the declined run took one model call");

    // No bearer: refused before anything runs.
    let calls_before = calls.lock().expect("log").len();
    let refused = run(None).await.expect("run");
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);

    // A forged token: `error`, and nothing was read (no tool call, no model call).
    let forged = "v1.forged-interop-token.mac";
    let response = run(Some(forged.to_owned())).await.expect("run");
    let body: Value = serde_json::from_str(&response.text().await.expect("body")).expect("json");
    assert_eq!(body["result"]["termination"], "error", "{body}");
    assert_eq!(
        calls.lock().expect("log").len(),
        calls_before,
        "nothing read"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        model_calls,
        "no model call"
    );

    // The runtime's logs never carry a token the suite sent.
    let container_logs = logs(&container).await;
    for secret in [token.as_str(), forged] {
        assert!(
            !container_logs.contains(secret),
            "a task token reached the runtime's logs"
        );
    }
}

/// The pinned skill of the stage.
struct StagePins(SkillPin);

#[async_trait]
impl SkillPins for StagePins {
    async fn pins(&self, _profile: &str) -> ekg_domain::Result<Vec<SkillPin>> {
        Ok(vec![self.0.clone()])
    }
}

/// The served runtime's reasoner, keeping the task it was given.
struct Kept {
    runtime: RuntimeReasoner,
    task: Mutex<Option<ReasoningTask>>,
}

#[async_trait]
impl Reasoner for Kept {
    async fn reason(&self, task: &ReasoningTask) -> ekg_domain::Result<RunReport> {
        *self.task.lock().expect("task") = Some(task.clone());
        self.runtime.reason(task).await
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<RunRecord>>);

impl RunRecorder for Records {
    fn record(&self, record: RunRecord) {
        self.0.lock().expect("records").push(record);
    }
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
#[allow(clippy::too_many_lines)] // one runtime container, one run, both sides' spans
async fn an_agent_run_is_one_trace_across_the_engine_and_the_runtime_without_content() {
    spans::on();
    let stage = stage().await;
    let collector = collector().await;
    let extra = [
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            format!("http://{HOST_FROM_CONTAINER}:{}", collector.port),
        ),
        // The collector double takes HTTP only, as the trace backend does.
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf".to_owned()),
        ("OTEL_SERVICE_NAME", "ekg-cognition-runtime".to_owned()),
        // Export promptly instead of the 5 s default batch delay.
        ("OTEL_BSP_SCHEDULE_DELAY", "200".to_owned()),
    ];
    let Some(container) = runtime(&stage, &extra).await else {
        return;
    };
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(1416.tcp())
        .await
        .expect("mapped 1416");

    // The engine's side: the agent proposer over the served runtime.
    let reasoner = Arc::new(Kept {
        runtime: RuntimeReasoner::new(
            &format!("http://{host}:{port}"),
            std::num::NonZeroUsize::MIN,
        )
        .expect("reasoner"),
        task: Mutex::new(None),
    });
    let records = Arc::new(Records::default());
    let task: TaskBuilder =
        Arc::new(|_, _| format!("Plan for the order. Load propose-plan. {TASK_MARKER}"));
    let proposer = AgentProposer::new(
        Arc::clone(&stage.world) as Arc<dyn WorldModel>,
        Arc::clone(&stage.profile),
        Arc::clone(&stage.tokens),
        Arc::clone(&reasoner) as Arc<dyn Reasoner>,
        Arc::clone(&stage.ledger),
        Arc::new(StagePins(stage.pin.clone())),
        task,
        AgentLimits {
            max_steps: 6,
            run_cap: Duration::from_secs(120),
        },
    )
    .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
    let candidate = proposer
        .propose_candidate(&stage.subject, &[])
        .with_context(opentelemetry::Context::new())
        .await;
    let container_logs = logs(&container).await;
    let candidate = candidate.unwrap_or_else(|why| panic!("{why}\n{container_logs}"));
    assert_eq!(candidate.proposal.explanation, "Move the promise.");

    // The engine's spans: the run, and under it every call through the gateway.
    let record = records.0.lock().expect("records").remove(0);
    let trace = record.trace_id.clone();
    let engine = spans::of(&trace);
    let run = spans::named(&engine, &format!("invoke_agent {PROFILE}"));
    assert_eq!(
        spans::attr(run, "ekg.agent.termination").as_deref(),
        Some("submitted")
    );
    assert_eq!(
        spans::attr(run, "gen_ai.response.model").as_deref(),
        Some(AGENT_MODEL)
    );
    let run_span = run.span_context.span_id();
    let tools: Vec<_> = engine
        .iter()
        .filter(|span| span.name.starts_with("execute_tool "))
        .collect();
    assert_eq!(tools.len(), 3, "{}", spans::text(&engine));
    let _ = spans::named(&engine, "read_skill propose-plan");

    // The runtime's spans arrive over HTTP, in the same trace, under the engine's run span.
    let (trace_bytes, run_bytes) = (hex_bytes(&trace), hex_bytes(&run_span.to_string()));
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let runtime_spans = loop {
        let all = collector.exported();
        if contains(&all, &trace_bytes)
            && contains(&all, &run_bytes)
            && contains(&all, b"haystack.agent.run")
        {
            break all;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the runtime exported no span of trace {trace} under the run span {run_span} \
             ({} bytes exported)\n{container_logs}",
            all.len()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(contains(&runtime_spans, b"ekg-cognition-runtime"));

    // A tool span hangs under the runtime's tool step when the runtime's MCP client sends the
    // trace context, and under the run span when it does not. Either way it is in the trace.
    let propagated = tools
        .iter()
        .filter(|span| span.parent_span_id != run_span)
        .count();
    for span in &tools {
        assert!(
            span.parent_span_id == run_span
                || contains(&runtime_spans, &hex_bytes(&span.parent_span_id.to_string())),
            "a tool span's parent is neither the run span nor a span of the runtime"
        );
    }
    println!(
        "the runtime's MCP calls carried a trace context on {propagated} of {} tool calls",
        tools.len()
    );

    // Neither side's spans carry content or a credential.
    let token = reasoner
        .task
        .lock()
        .expect("task")
        .as_ref()
        .expect("the task the runtime was given")
        .token
        .clone();
    let engine_text = spans::text(&engine);
    for (what, content) in [
        ("the task text", TASK_MARKER),
        ("a tool argument", "Move the promise."),
        ("a tool argument", stage.subject.0.as_str()),
        ("a tool result", RESULT_MARKER),
        ("the task token", token.as_str()),
        ("the model credential", MODEL_KEY),
    ] {
        assert!(
            !engine_text.contains(content),
            "{what} is in the engine's spans:\n{engine_text}"
        );
        assert!(
            !contains(&runtime_spans, content.as_bytes()),
            "{what} is in the runtime's spans"
        );
    }
}

// --- a joint run under a composed profile ---

const JOINT_PROFILE: &str = "interop-joint";
const PLANT_PROFILE: &str = "interop-plant";

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
    ) -> Result<ProposedAction, Vec<String>> {
        if action.params.contains_key("start") {
            Ok(action.clone())
        } else {
            Err(vec!["move_window is missing param \"start\"".to_owned()])
        }
    }
}

/// A member's own tool: it takes nothing and answers its name.
struct Named(&'static str);

#[async_trait]
impl AgentTool for Named {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "Answers its own name."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn call(
        &self,
        _args: Value,
        _world: &ReadOnlyWorld,
        _scope: &TaskScope,
    ) -> Result<Value, ToolError> {
        Ok(json!({ "tool": self.0 }))
    }
}

/// The joint script's state: the two targets, and how many chat requests it has answered.
#[derive(Clone)]
struct JointProvider {
    order: String,
    work_order: String,
    requests: Arc<AtomicUsize>,
}

/// The scripted model of a joint run: a tool of each member, then one plan across both.
async fn joint_chat(State(provider): State<JointProvider>, Json(body): Json<Value>) -> Json<Value> {
    provider.requests.fetch_add(1, Ordering::SeqCst);
    let seen = body["messages"].as_array().map_or(0, |messages| {
        messages.iter().filter(|m| m["role"] == "tool").count()
    });
    Json(completion(match seen {
        0 => Some(("list_lines", json!({}))),
        1 => Some(("list_centres", json!({}))),
        2 => Some((
            "submit_plan",
            json!({
                "actions": [
                    { "action_type": "move_date", "target": provider.order, "params": { "new_date": "2027-11-08" } },
                    { "action_type": "move_window", "target": provider.work_order, "params": { "start": "2027-11-07 08:00:00" } }
                ],
                "explanation": "Move the promise and the window."
            }),
        )),
        _ => None,
    }))
}

/// The manufacturing order's work order, as the plant's hook relates it.
struct WorkOrders {
    mo: EntityUri,
    work_order: EntityUri,
}

#[async_trait]
impl ekg_cognition::GroundingExtension for WorkOrders {
    async fn related(&self, subject: &EntityUri) -> ekg_domain::Result<Vec<EntityUri>> {
        Ok(if *subject == self.mo {
            vec![self.work_order.clone()]
        } else {
            Vec::new()
        })
    }
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn an_accepted_finding_ends_a_run_and_a_cut_off_answer_is_no_decline() {
    let stage = stage().await;
    let Some(container) = runtime(&stage, &[]).await else {
        return;
    };
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(1416.tcp())
        .await
        .expect("mapped 1416");
    let url = format!("http://{host}:{port}/agent/run");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("client");
    let run = |bearer: String, task: &'static str| {
        http.post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(json!({ "task": task, "max_steps": 6 }).to_string())
            .send()
    };
    let token = stage
        .tokens
        .mint(
            &stage.analyst,
            TaskGrant::new(
                vec![stage.subject.clone()],
                vec!["ground_entity".to_owned(), "submit_finding".to_owned()],
                Duration::from_secs(300),
                TRACE,
            ),
        )
        .expect("mint");
    let jti = stage.tokens.verify_now(&token).expect("verify").jti;

    let response = run(token.clone(), FINDING_TASK).await.expect("run");
    assert!(response.status().is_success(), "{}", response.status());
    let body: Value = serde_json::from_str(&response.text().await.expect("body")).expect("json");
    assert_eq!(body["result"]["termination"], "submitted", "{body}");

    let task = stage.ledger.take(&jti).expect("the task's record");
    assert_eq!(task.calls("submit_finding", "rejected"), 1, "the bad kind");
    assert_eq!(task.calls("submit_finding", "ok"), 1, "the run ended on it");
    assert_eq!(task.finding.expect("a finding").member, stage.subject);

    let response = run(token, CUT_OFF_TASK).await.expect("run");
    assert!(response.status().is_success(), "{}", response.status());
    let body: Value = serde_json::from_str(&response.text().await.expect("body")).expect("json");
    assert_eq!(body["result"]["termination"], "length", "{body}");
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
#[allow(clippy::too_many_lines)] // one runtime container, one joint run
async fn a_joint_run_calls_a_tool_of_each_member_and_submits_one_plan_across_both_catalogs() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let world = Arc::new(InMemoryWorldModel::default());
    let records = [
        record("sale.order", 1, &[("state", "late")]),
        record("mrp.production", 7, &[("state", "confirmed")]),
        record("mrp.workorder", 3, &[("state", "ready")]),
    ];
    for rec in &records {
        world.materialize(rec).await.expect("materialize");
    }
    let uri = |index: usize| InMemoryWorldModel::uri_of(&records[index].source_ref);
    let (order, mo, work_order) = (uri(0), uri(1), uri(2));
    let context = |role: &str| ReadContext {
        role: role.to_owned(),
        purpose: "interop".to_owned(),
    };
    let orders = ToolProfile::new(PROFILE, context("planner"), None)
        .with_tool(Arc::new(Named("list_lines")))
        .with_tool(Arc::new(SubmitPlan::new(Arc::new(MoveDate), 3)));
    let plant = ToolProfile::new(
        PLANT_PROFILE,
        context("scheduler"),
        Some(Arc::new(WorkOrders {
            mo: mo.clone(),
            work_order: work_order.clone(),
        })),
    )
    .with_tool(Arc::new(Named("list_centres")))
    .with_tool(Arc::new(SubmitPlan::new(Arc::new(MoveWindow), 3)));
    let catalog =
        JoinedCatalog::new(vec![Arc::new(MoveDate), Arc::new(MoveWindow)]).expect("no verb twice");
    let joint = ToolProfile::composed(
        JOINT_PROFILE,
        &[&orders, &plant],
        Arc::new(|entity: &EntityUri| {
            let plant = entity
                .model()
                .is_some_and(|model| model.starts_with("mrp."));
            if plant { PLANT_PROFILE } else { PROFILE }.to_owned()
        }),
    )
    .expect("two members")
    .with_tool(Arc::new(SubmitPlan::new(Arc::new(catalog), 3)));
    let mut registry = ProfileRegistry::new(Arc::clone(&world) as Arc<dyn WorldModel>);
    for profile in [orders, plant, joint] {
        registry.register(profile).expect("register");
    }
    let registry = Arc::new(registry);
    let profile = registry.get(JOINT_PROFILE).expect("joint");
    // The runtime lists the token's skills before it runs: one skill written for the joint
    // profile.
    let bundle = Arc::new(
        SkillBundle::parse(
            "plan-portfolio",
            BTreeMap::from([(
                "SKILL.md".to_owned(),
                format!(
                    "---\nname: plan-portfolio\ndescription: Plan for coupled members.\nprofiles: [{JOINT_PROFILE}]\ntools: [submit_plan]\n---\nRead every member, then submit one plan."
                ),
            )]),
        )
        .expect("skill"),
    );
    let pin = SkillPin {
        name: "plan-portfolio".to_owned(),
        version: bundle.digest(),
        profiles: vec![JOINT_PROFILE.to_owned()],
    };
    let tokens = Arc::new(TaskTokens::random().expect("secret"));
    let ledger = Arc::new(RunLedger::default());
    let gateway_port = serve(gateway_router(
        registry,
        Arc::clone(&tokens),
        GatewayConfig::default()
            .allow_host(HOST_FROM_CONTAINER)
            .with_call_log(Arc::new(|_| {}))
            .with_run_ledger(Arc::clone(&ledger))
            .with_skill_source(Arc::new(OneSkill(bundle))),
    ))
    .await;
    let provider = JointProvider {
        order: order.0.clone(),
        work_order: work_order.0.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
    };
    let requests = Arc::clone(&provider.requests);
    let provider_port = serve(
        axum::Router::new()
            .route("/v1/chat/completions", post(joint_chat))
            .with_state(provider),
    )
    .await;

    // One token for both members, with every tool the composed profile offers.
    let tools: Vec<String> = profile
        .tool_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        tools,
        [
            "ground_entity",
            "related_entities",
            "list_lines",
            "list_centres",
            "submit_plan"
        ]
    );
    let token = tokens
        .mint(
            &profile,
            TaskGrant::new(
                vec![order.clone(), mo.clone()],
                tools,
                Duration::from_secs(300),
                TRACE,
            )
            .with_skills(vec![pin.clone()]),
        )
        .expect("mint");
    let jti = tokens.verify_now(&token).expect("verify").jti;

    let stage = Stage {
        world,
        subject: order.clone(),
        analyst: Arc::clone(&profile),
        profile,
        pin,
        tokens,
        ledger: Arc::clone(&ledger),
        calls: Arc::new(Mutex::new(Vec::new())),
        requests: Arc::clone(&requests),
        gateway_port,
        provider_port,
    };
    let Some(container) = runtime(&stage, &[]).await else {
        return;
    };
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(1416.tcp())
        .await
        .expect("mapped 1416");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("client");
    let response = http
        .post(format!("http://{host}:{port}/agent/run"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(json!({ "task": "Plan for both members.", "max_steps": 6 }).to_string())
        .send()
        .await
        .expect("run");
    assert!(response.status().is_success(), "{}", response.status());
    let body: Value = serde_json::from_str(&response.text().await.expect("body")).expect("json");
    let container_logs = logs(&container).await;
    assert_eq!(
        body["result"]["termination"], "submitted",
        "{body}\n{container_logs}"
    );
    assert_eq!(body["result"]["steps"], 3, "{body}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "one model call per step"
    );

    // The ledger: a tool of each member was called, and one plan spans both catalogs.
    let ran = ledger.take(&jti).expect("the run's ledger record");
    assert_eq!(ran.calls("list_lines", "ok"), 1, "{ran:?}");
    assert_eq!(ran.calls("list_centres", "ok"), 1, "{ran:?}");
    assert_eq!(ran.calls("submit_plan", "ok"), 1, "{ran:?}");
    assert!(ran.rejections.is_empty(), "{ran:?}");
    let plan = ran.accepted.expect("the accepted plan");
    let verbs: Vec<&str> = plan
        .actions
        .iter()
        .map(|action| action.action_type.0.as_str())
        .collect();
    assert_eq!(verbs, ["move_date", "move_window"]);
    assert_eq!(plan.actions[0].target, order);
    assert_eq!(
        plan.actions[1].target, work_order,
        "in the plant member's scope"
    );
    assert_eq!(
        plan.actions[0].params.get("new_date").map(String::as_str),
        Some("2027-11-08 00:00:00"),
        "normalized by its own catalog"
    );
}
