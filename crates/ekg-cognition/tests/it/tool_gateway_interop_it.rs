//! Gated interop suite for the agent tool gateway: the built runtime image
//! (`localhost/ekg-hayhooks:local`, from `just podman-build`) uses the engine's tool gateway
//! as a Haystack `MCPToolset`, as an agent in the runtime does.
//!
//! The gateway runs in-process on `0.0.0.0` over an in-memory world with a test profile. A
//! container from the runtime image runs a short Python check that reaches it through
//! `host.containers.internal` (a non-localhost `Host`, so the DNS-rebinding allow-list is
//! exercised): with a minted task token it lists only the token's tools and calls
//! `ground_entity`; without one, `warm_up()` fails and no tool is loaded.
//!
//! A second check serves a test profile with `submit_plan` over a fake catalog and an
//! in-memory skill source: in the image, a Haystack `SkillToolset` backed by the runtime's
//! `EngineSkillStore` lists and loads the token's pinned `propose-plan`, and `submit_plan`
//! called through the `MCPToolset` accepts a valid plan and raises `MCPInvocationError` naming
//! every defect of an invalid one. Without a token, no skill loads.
//!
//! Ignored by default. It reports a missing container runtime or image through the shared
//! loud-skip helper, so `just test-e2e` (which sets `EKG_E2E_REQUIRE_INFRA`) fails instead
//! of passing on a skip. Run with:
//!
//! ```text
//! just podman-build
//! cargo test -p ekg-cognition --test it tool_gateway_interop -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown)]

use crate::loud_skip;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ekg_cognition::skills::SkillBundle;
use ekg_cognition::tools::{
    ActionSpec, CallRecord, GatewayConfig, ParamSpec, PlanCatalog, ProfileRegistry, ReadOnlyWorld,
    SkillPin, SkillRecord, SkillSource, SubmitPlan, TaskGrant, TaskTokens, ToolProfile,
    gateway_router,
};
use ekg_domain::fakes::InMemoryWorldModel;
use ekg_domain::{EntityUri, ProposedAction, ReadContext, SourceRecord, SourceRef, WorldModel};
use serde_json::Value;
use testcontainers::core::WaitFor;
use testcontainers::core::wait::ExitWaitStrategy;
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

/// The image under test.
const IMAGE: (&str, &str) = ("localhost/ekg-hayhooks", "local");
/// How a container on this machine reaches a listener on the host (Podman).
const HOST_FROM_CONTAINER: &str = "host.containers.internal";
const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";

/// The runtime-side check: a toolset per run, with the run's token, closed after it, then
/// a toolset with no token at all.
const CHECK: &str = r#"
import json, os
from haystack_integrations.tools.mcp import MCPToolset, StreamableHttpServerInfo

url, token, subject = os.environ["GATEWAY_URL"], os.environ["TASK_TOKEN"], os.environ["SUBJECT"]
out = {}
toolset = MCPToolset(StreamableHttpServerInfo(url=url, token=token), eager_connect=False)
toolset.warm_up()
try:
    out["tools"] = sorted(tool.name for tool in toolset)
    ground = next(tool for tool in toolset if tool.name == "ground_entity")
    result = ground.invoke(entity=subject)
    out["ground"] = result if isinstance(result, str) else json.dumps(result)
finally:
    toolset.close()
try:
    anonymous = MCPToolset(StreamableHttpServerInfo(url=url), eager_connect=False)
    anonymous.warm_up()
    out["no_token"] = "connected"
    out["no_token_tools"] = [tool.name for tool in anonymous]
except Exception as error:
    out["no_token"] = "refused:" + type(error).__name__
print("INTEROP-RESULT " + json.dumps(out), flush=True)
"#;

/// A gateway in-process on `0.0.0.0` over an in-memory order whose supplier the test
/// profile's role may not see: its port, the order, a token allowing only `ground_entity`,
/// and the gateway's call log.
async fn serve_gateway() -> (u16, EntityUri, String, Arc<Mutex<Vec<CallRecord>>>) {
    // An order whose supplier the test profile's role may not see.
    let world = Arc::new(InMemoryWorldModel::default());
    let order = SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 7,
        },
        fields: BTreeMap::from([
            ("state".to_owned(), "late".to_owned()),
            ("supplier".to_owned(), "CellTech".to_owned()),
        ]),
    };
    world.materialize(&order).await.expect("materialize");
    let subject = InMemoryWorldModel::uri_of(&order.source_ref);

    let profile = ToolProfile::new(
        "interop",
        ReadContext {
            role: "planner".to_owned(),
            purpose: "interop".to_owned(),
        },
        None,
    );
    let tokens = Arc::new(TaskTokens::random().expect("secret"));
    let token = tokens
        .mint(
            &profile,
            TaskGrant::new(
                vec![subject.clone()],
                vec!["ground_entity".to_owned()],
                Duration::from_secs(300),
                TRACE,
            ),
        )
        .expect("mint");
    let mut registry = ProfileRegistry::new(Arc::clone(&world) as Arc<dyn WorldModel>);
    registry.register(profile).expect("register");

    let calls: Arc<Mutex<Vec<CallRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let log = {
        let calls = Arc::clone(&calls);
        Arc::new(move |record: &CallRecord| calls.lock().expect("log").push(record.clone()))
    };
    let router = gateway_router(
        Arc::new(registry),
        tokens,
        GatewayConfig::default()
            .allow_host(HOST_FROM_CONTAINER)
            .with_call_log(log),
    );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });

    (port, subject, token, calls)
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_haystack_toolset_in_the_runtime_image_lists_and_calls_the_gateway() {
    let (port, subject, token, calls) = serve_gateway().await;
    let request = GenericImage::new(IMAGE.0, IMAGE.1)
        .with_entrypoint("python")
        .with_wait_for(WaitFor::exit(ExitWaitStrategy::new()))
        .with_cmd(["-c", CHECK])
        .with_env_var(
            "GATEWAY_URL",
            format!("http://{HOST_FROM_CONTAINER}:{port}/mcp"),
        )
        .with_env_var("TASK_TOKEN", token)
        .with_env_var("SUBJECT", subject.0.clone())
        .with_startup_timeout(Duration::from_secs(120));
    let container = match request.start().await {
        Ok(container) => container,
        Err(why) => {
            loud_skip::infra_unavailable(&format!(
                "no container runtime or no {}:{} image (run `just podman-build`): {why}",
                IMAGE.0, IMAGE.1
            ));
            return;
        }
    };
    let stdout =
        String::from_utf8_lossy(&container.stdout_to_vec().await.unwrap_or_default()).into_owned();
    let stderr =
        String::from_utf8_lossy(&container.stderr_to_vec().await.unwrap_or_default()).into_owned();
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("INTEROP-RESULT "))
        .unwrap_or_else(|| {
            panic!("no result from the runtime check\nstdout:\n{stdout}\nstderr:\n{stderr}")
        });
    let out: Value = serde_json::from_str(line).expect("result json");

    // With the token: only the token's tools, and a grounding of the subject.
    assert_eq!(out["tools"], serde_json::json!(["ground_entity"]), "{out}");
    let result: Value =
        serde_json::from_str(out["ground"].as_str().expect("ground result")).expect("json");
    let grounding = &result["structuredContent"];
    assert_eq!(
        grounding["entity"],
        serde_json::json!(subject.0),
        "{result}"
    );
    assert!(
        grounding["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .all(|edge| edge["source"]["system"] == "odoo"),
        "{result}"
    );
    assert_eq!(
        grounding["omissions"][0]["type_iri"], "ekg:Supplier",
        "the planner's hidden supplier is disclosed: {result}"
    );

    // Without a token: refused, nothing loaded.
    assert!(
        out["no_token"]
            .as_str()
            .is_some_and(|outcome| outcome.starts_with("refused:")),
        "{out}"
    );

    // The gateway saw one call, under the task's trace.
    let calls = calls.lock().expect("log").clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].trace_id, TRACE);
    assert_eq!(calls[0].tool, "ground_entity");
    assert_eq!(calls[0].outcome, "ok");
}

/// The runtime-side skill check: a `SkillToolset` over the engine's skill store with the run's
/// token, `submit_plan` through the `MCPToolset`, then the store without (or with a bad) token.
const SKILLS_CHECK: &str = r#"
import json, os, sys
sys.path.insert(0, "/opt/ekg-common")
from ekg_skills import EngineSkillStore
from haystack.tools import SkillToolset
from haystack_integrations.tools.mcp import MCPToolset, StreamableHttpServerInfo

base, token, subject = os.environ["GATEWAY_BASE"], os.environ["TASK_TOKEN"], os.environ["SUBJECT"]
stranger = os.environ["STRANGER"]
out = {}
skills = SkillToolset(EngineSkillStore(base, token))
skills.warm_up()
load = next(tool for tool in skills if tool.name == "load_skill")
read = next(tool for tool in skills if tool.name == "read_skill_file")
out["skills"] = sorted(skills.skills)
out["load_description"] = load.description
out["loaded"] = load.invoke(name="propose-plan")
out["file"] = read.invoke(name="propose-plan", path="reference/catalog.md")
try:
    load.invoke(name="explain-plan")
    out["unpinned"] = "loaded"
except Exception as error:
    out["unpinned"] = "refused:" + type(error).__name__ + ":" + str(error)
out["to_dict"] = EngineSkillStore(base, token).to_dict()

toolset = MCPToolset(StreamableHttpServerInfo(url=base + "/mcp", token=token), eager_connect=False)
toolset.warm_up()
try:
    submit = next(tool for tool in toolset if tool.name == "submit_plan")
    accepted = submit.invoke(
        actions=[{"action_type": "move_date", "target": subject, "params": {"new_date": "2027-11-08"}}],
        explanation="Move the promise to the new arrival.",
    )
    out["accepted"] = accepted if isinstance(accepted, str) else json.dumps(accepted)
    try:
        submit.invoke(
            actions=[
                {"action_type": "cancel_order", "target": subject, "params": {}},
                {"action_type": "notify", "target": stranger, "params": {"message": "x"}},
            ],
            explanation="two defects",
        )
        out["rejected"] = "accepted"
    except Exception as error:
        # Called directly, Haystack's Tool wraps the MCP error; record the whole cause chain.
        chain, cause = [], error
        while cause is not None:
            chain.append(type(cause).__name__)
            cause = cause.__cause__
        out["rejected"] = ">".join(chain) + ":" + str(error)
finally:
    toolset.close()

try:
    EngineSkillStore(base, "")
    out["no_token"] = "constructed"
except ValueError as error:
    out["no_token"] = "refused:ValueError"
try:
    SkillToolset(EngineSkillStore(base, "v1.forged.token")).warm_up()
    out["bad_token"] = "loaded"
except Exception as error:
    out["bad_token"] = "refused:" + type(error).__name__ + ":" + str(error)
print("INTEROP-RESULT " + json.dumps(out), flush=True)
"#;

/// `move_date` on a sale order (a `new_date`) and `notify` on any entity (a `message`).
struct FakeCatalog;

#[async_trait]
impl PlanCatalog for FakeCatalog {
    fn action_types(&self) -> Vec<ActionSpec> {
        let spec = |verb: &str, model: Option<&str>, param: &str| ActionSpec {
            action_type: verb.to_owned(),
            description: format!("the {verb} verb"),
            target_model: model.map(str::to_owned),
            params: vec![ParamSpec {
                name: param.to_owned(),
                description: param.to_owned(),
            }],
        };
        vec![
            spec("move_date", Some("sale.order"), "new_date"),
            spec("notify", None, "message"),
        ]
    }

    async fn check(
        &self,
        action: &ProposedAction,
        _world: &ReadOnlyWorld,
    ) -> Result<ProposedAction, Vec<String>> {
        if self
            .action_types()
            .iter()
            .any(|spec| spec.action_type == action.action_type.0)
        {
            Ok(action.clone())
        } else {
            Err(vec![format!(
                "action type {:?} is not in this profile's catalog",
                action.action_type.0
            )])
        }
    }
}

/// Skill versions in memory, by version.
struct MemorySkills(BTreeMap<String, Arc<SkillBundle>>);

#[async_trait]
impl SkillSource for MemorySkills {
    async fn version(&self, version: &str) -> Result<Option<Arc<SkillBundle>>, String> {
        Ok(self.0.get(version).cloned())
    }
}

/// A skill for the `interop` profile, with one bundled file.
fn skill(name: &str, body: &str) -> SkillBundle {
    SkillBundle::parse(
        name,
        BTreeMap::from([
            (
                "SKILL.md".to_owned(),
                format!(
                    "---\ndescription: {name} for the interop check.\nname: {name}\nprofiles: [interop]\ntools: [submit_plan]\n---\n{body}\n"
                ),
            ),
            ("reference/catalog.md".to_owned(), "move_date, notify\n".to_owned()),
        ]),
    )
    .expect("skill")
}

/// The skills gateway: in-process on `0.0.0.0`, its profile offering `submit_plan`, its source
/// holding a pinned and an unpinned skill.
struct SkillsGateway {
    port: u16,
    subject: EntityUri,
    stranger: EntityUri,
    token: String,
    pinned: SkillBundle,
    calls: Arc<Mutex<Vec<CallRecord>>>,
    skill_calls: Arc<Mutex<Vec<SkillRecord>>>,
}

async fn serve_skills_gateway() -> SkillsGateway {
    // The order whose plan is submitted, and an order outside its scope.
    let world = Arc::new(InMemoryWorldModel::default());
    let mut uris = Vec::new();
    for id in [7, 99] {
        let order = SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: "sale.order".to_owned(),
                id,
            },
            fields: BTreeMap::from([("state".to_owned(), "late".to_owned())]),
        };
        world.materialize(&order).await.expect("materialize");
        uris.push(InMemoryWorldModel::uri_of(&order.source_ref));
    }
    let profile = ToolProfile::new(
        "interop",
        ReadContext {
            role: "planner".to_owned(),
            purpose: "interop".to_owned(),
        },
        None,
    )
    .with_tool(Arc::new(SubmitPlan::new(Arc::new(FakeCatalog), 4)));
    let (pinned, unpinned) = (
        skill("propose-plan", "Ground, then submit_plan."),
        skill("explain-plan", "Never served."),
    );
    let source = MemorySkills(
        [&pinned, &unpinned]
            .into_iter()
            .map(|bundle| (bundle.digest(), Arc::new(bundle.clone())))
            .collect(),
    );
    let tokens = Arc::new(TaskTokens::random().expect("secret"));
    let token = tokens
        .mint(
            &profile,
            TaskGrant::new(
                vec![uris[0].clone()],
                vec!["submit_plan".to_owned()],
                Duration::from_secs(300),
                TRACE,
            )
            .with_skills(vec![SkillPin {
                name: pinned.name().to_owned(),
                version: pinned.digest(),
                profiles: pinned.profiles().to_vec(),
            }]),
        )
        .expect("mint");
    let mut registry = ProfileRegistry::new(Arc::clone(&world) as Arc<dyn WorldModel>);
    registry.register(profile).expect("register");
    let (calls, skill_calls) = (
        Arc::new(Mutex::new(Vec::<CallRecord>::new())),
        Arc::new(Mutex::new(Vec::<SkillRecord>::new())),
    );
    let config = {
        let (calls, skill_calls) = (Arc::clone(&calls), Arc::clone(&skill_calls));
        GatewayConfig::default()
            .allow_host(HOST_FROM_CONTAINER)
            .with_call_log(Arc::new(move |r: &CallRecord| {
                calls.lock().expect("log").push(r.clone());
            }))
            .with_skill_source(Arc::new(source))
            .with_skill_log(Arc::new(move |r: &SkillRecord| {
                skill_calls.lock().expect("log").push(r.clone());
            }))
    };
    let router = gateway_router(Arc::new(registry), tokens, config);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    SkillsGateway {
        port,
        subject: uris[0].clone(),
        stranger: uris[1].clone(),
        token,
        pinned,
        calls,
        skill_calls,
    }
}

/// Run the skills check in the runtime image against `gw`: its result, or `None` after a loud
/// skip when there is no container runtime or image.
async fn run_skills_check(gw: &SkillsGateway) -> Option<Value> {
    let request = GenericImage::new(IMAGE.0, IMAGE.1)
        .with_entrypoint("python")
        .with_wait_for(WaitFor::exit(ExitWaitStrategy::new()))
        .with_cmd(["-c", SKILLS_CHECK])
        .with_env_var(
            "GATEWAY_BASE",
            format!("http://{HOST_FROM_CONTAINER}:{}", gw.port),
        )
        .with_env_var("TASK_TOKEN", gw.token.clone())
        .with_env_var("SUBJECT", gw.subject.0.clone())
        .with_env_var("STRANGER", gw.stranger.0.clone())
        .with_startup_timeout(Duration::from_secs(120));
    let container = match request.start().await {
        Ok(container) => container,
        Err(why) => {
            loud_skip::infra_unavailable(&format!(
                "no container runtime or no {}:{} image (run `just podman-build`): {why}",
                IMAGE.0, IMAGE.1
            ));
            return None;
        }
    };
    let stdout =
        String::from_utf8_lossy(&container.stdout_to_vec().await.unwrap_or_default()).into_owned();
    let stderr =
        String::from_utf8_lossy(&container.stderr_to_vec().await.unwrap_or_default()).into_owned();
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("INTEROP-RESULT "))
        .unwrap_or_else(|| {
            panic!("no result from the runtime check\nstdout:\n{stdout}\nstderr:\n{stderr}")
        });
    Some(serde_json::from_str(line).expect("result json"))
}

/// The skill toolset saw only the pinned skill and loaded its pinned version, not the
/// unpinned one; without (or with a bad) token, nothing loaded.
fn assert_skills_loaded(gw: &SkillsGateway, out: &Value) {
    assert_eq!(out["skills"], serde_json::json!(["propose-plan"]), "{out}");
    let description = out["load_description"].as_str().expect("description");
    assert!(
        description.contains("- propose-plan: propose-plan for the interop check."),
        "{description}"
    );
    assert!(!description.contains("explain-plan"), "{description}");
    let loaded = out["loaded"].as_str().expect("loaded");
    assert!(
        loaded.starts_with("Ground, then submit_plan.") && loaded.contains("reference/catalog.md"),
        "{loaded}"
    );
    assert_eq!(out["file"], "move_date, notify\n", "{out}");
    let unpinned = out["unpinned"].as_str().expect("unpinned");
    assert!(
        unpinned.starts_with("refused:") && unpinned.contains("explain-plan"),
        "{unpinned}"
    );
    assert!(!unpinned.contains("Never served"), "{unpinned}");
    // The token is never serialized.
    assert_eq!(
        out["to_dict"]["init_parameters"],
        serde_json::json!({ "url": format!("http://{HOST_FROM_CONTAINER}:{}", gw.port) }),
        "{out}"
    );
    assert_eq!(out["no_token"], "refused:ValueError", "{out}");
    let bad = out["bad_token"].as_str().expect("bad token");
    assert!(bad.starts_with("refused:") && bad.contains("401"), "{bad}");
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_skill_toolset_in_the_runtime_image_loads_pinned_skills_and_submit_plan_checks_plans() {
    let gw = serve_skills_gateway().await;
    let Some(out) = run_skills_check(&gw).await else {
        return;
    };
    assert_skills_loaded(&gw, &out);

    // submit_plan: a valid plan is accepted; an invalid one is an MCPInvocationError naming
    // every defect.
    let accepted: Value =
        serde_json::from_str(out["accepted"].as_str().expect("accepted")).expect("json");
    assert_eq!(
        accepted["structuredContent"]["accepted"], true,
        "{accepted}"
    );
    let rejected = out["rejected"].as_str().expect("rejected");
    let (chain, _) = rejected.split_once(':').expect("chain:message");
    assert!(
        chain.split('>').any(|class| class == "MCPInvocationError"),
        "{rejected}"
    );
    for defect in ["cancel_order", gw.stranger.0.as_str(), "2 defect(s)"] {
        assert!(
            rejected.contains(defect),
            "{defect} missing from {rejected}"
        );
    }

    // The gateway logged both plan submissions and each skill request under the task's trace.
    let calls = gw.calls.lock().expect("log").clone();
    let outcomes: Vec<(&str, &str)> = calls.iter().map(|c| (c.tool.as_str(), c.outcome)).collect();
    assert_eq!(
        outcomes,
        vec![("submit_plan", "ok"), ("submit_plan", "rejected")],
        "{calls:?}"
    );
    let skill_calls = gw.skill_calls.lock().expect("log").clone();
    assert!(
        skill_calls.iter().all(|r| r.trace_id == TRACE),
        "{skill_calls:?}"
    );
    let v12 = &gw.pinned.digest()[..12];
    assert!(
        skill_calls.iter().any(|r| r
            .to_string()
            .contains(&format!("skill=propose-plan@{v12} op=load outcome=ok"))),
        "{skill_calls:#?}"
    );
}
