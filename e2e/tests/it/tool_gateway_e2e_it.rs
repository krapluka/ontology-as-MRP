//! Gated live e2e for the agent tool gateway: the containerized backend's gateway at
//! `http://localhost:8091/mcp`, over the seeded world model in GraphDB.
//!
//! A task token is minted here with the stack's shared `EKG_TOOL_TOKEN_SECRET` (written to
//! `.env` by `just podman-local-up`) for the seeded battery order under the `supply-risk`
//! profile, and the gateway is driven with the official `rmcp` client:
//!
//! - `ground_entity` on the order returns edges with Odoo provenance;
//! - the seeded trike order, which the battery order's scope does not reach, is refused;
//! - a request without a token gets a 401.
//!
//! A second token, for the seeded e-bike manufacturing order under the `process` profile, lists
//! the schedule of the shared Assembly Line: it includes the trike order's work order, and a
//! plan that targets that work order is refused.
//!
//! A third test serves a gateway of its own over the stack's GraphDB, with its spans exported
//! in-process: a tool call's store requests are spans under the call's span, and a read made
//! outside any span, as a monitor makes it, is none.
//!
//! Ignored by default. Skips if the gateway, Odoo or the secret is unavailable, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), when that fails the run.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it tool_gateway_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::propose::{
    PROCESS_PROFILE, SUPPLY_RISK_PROFILE, process_profile, supply_risk_profile,
};
use ekg_cognition::tools::{TaskGrant, TaskTokens, ToolProfile};
use ekg_domain::fakes::InMemoryWorldModel;
use ekg_domain::{EntityUri, WorldModel};
use ekg_semantic::ontology;
use odoo_client::Odoo;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

const TRACE: &str = "5c1e0b4f1a2d4e6f8a9b0c1d2e3f4a5b";

/// The Odoo id of a seeded record, by its seed external id.
fn seed_res_id(odoo: &Odoo, name: &str) -> Option<i64> {
    let rows = odoo
        .search_read(
            "ir.model.data",
            json!([["module", "=", "ekg_seed"], ["name", "=", name]]),
            json!(["res_id"]),
        )
        .ok()?;
    rows.as_array()?.first()?["res_id"].as_i64()
}

fn ground(entity: &EntityUri) -> CallToolRequestParams {
    let Value::Object(args) = json!({ "entity": entity.0 }) else {
        unreachable!("an object literal");
    };
    CallToolRequestParams::new("ground_entity").with_arguments(args)
}

/// A task token for `subject`, minted as the engine would with the stack's `secret`, for the
/// supply-risk profile (the world is not read to mint; the profile names its id and tools).
fn mint(secret: &str, subject: &EntityUri) -> String {
    let profile = supply_risk_profile(
        Arc::new(InMemoryWorldModel::default()) as Arc<dyn WorldModel>,
        ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS,
    );
    assert_eq!(profile.id(), SUPPLY_RISK_PROFILE);
    mint_for(
        secret,
        &profile,
        subject,
        &["ground_entity", "related_entities"],
    )
}

/// A task token for `subject` under `profile`, granting `tools`.
fn mint_for(secret: &str, profile: &ToolProfile, subject: &EntityUri, tools: &[&str]) -> String {
    TaskTokens::new(secret.trim().as_bytes())
        .expect("secret")
        .mint(
            profile,
            TaskGrant::new(
                vec![subject.clone()],
                tools.iter().map(|tool| (*tool).to_owned()).collect(),
                Duration::from_secs(120),
                TRACE,
            ),
        )
        .expect("mint")
}

fn call(tool: &'static str, args: Value) -> CallToolRequestParams {
    let Value::Object(args) = args else {
        unreachable!("an object literal");
    };
    CallToolRequestParams::new(tool).with_arguments(args)
}

/// The entity-IRI objects of a grounding's edges with `predicate`.
fn objects(grounding: &Value, predicate: &str) -> Vec<String> {
    grounding["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|edge| edge["predicate"] == json!(predicate))
        .filter_map(|edge| edge["object"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
#[ignore = "requires the live stack with the agent tool gateway (run with --ignored)"]
async fn a_process_task_reads_its_work_centres_schedule_and_cannot_target_another_orders_work() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let gateway = common::env_or("EKG_TOOL_GATEWAY_URL", "http://localhost:8091/mcp");
    let Some(secret) = std::env::var("EKG_TOOL_TOKEN_SECRET")
        .ok()
        .filter(|secret| !secret.trim().is_empty())
    else {
        common::infra_unavailable(
            "EKG_TOOL_TOKEN_SECRET unset (run `just podman-local-up`, then `set -a; source .env`)",
        );
        return;
    };
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let Ok(odoo) = Odoo::connect(
        &odoo_url,
        &common::env_or("ODOO_DB", "ekg"),
        &common::env_or("ODOO_USER", "admin"),
        &common::env_or("ODOO_API_KEY", "admin"),
    ) else {
        common::infra_unavailable(&format!("Odoo unreachable at {odoo_url}"));
        return;
    };
    let seeded = |model: &str, name: &str| {
        let id = seed_res_id(&odoo, name).unwrap_or_else(|| panic!("{name} seeded"));
        EntityUri(ontology::entity_iri(model, id))
    };
    let order = seeded("mrp.production", "mo_ebike");
    let own = seeded("mrp.workorder", "wo_ebike_assemble");
    let others = seeded("mrp.workorder", "wo_trike_assemble");
    let assembly = seeded("mrp.workcenter", "wc_assembly");

    let profile = process_profile(
        Arc::new(InMemoryWorldModel::default()) as Arc<dyn WorldModel>,
        ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS,
    );
    assert_eq!(profile.id(), PROCESS_PROFILE);
    let token = mint_for(
        &secret,
        &profile,
        &order,
        &["ground_entity", "list_work_centre_schedule", "submit_plan"],
    );
    let client = match ()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.clone()).auth_header(token),
        ))
        .await
    {
        Ok(client) => client,
        Err(why) => {
            common::infra_unavailable(&format!("tool gateway unreachable at {gateway}: {why}"));
            return;
        }
    };

    // The shared line's schedule: this order's work order and the trike order's.
    let listed = client
        .call_tool(call(
            "list_work_centre_schedule",
            json!({ "work_centre": assembly.0 }),
        ))
        .await
        .expect("call");
    assert_ne!(listed.is_error, Some(true), "{:?}", listed.content);
    let schedule = listed.structured_content.expect("a structured schedule");
    assert_eq!(schedule["work_centre"], json!(assembly.0));
    let work_orders = schedule["work_orders"].as_array().expect("work_orders");
    let iris: Vec<&str> = work_orders
        .iter()
        .filter_map(|wo| wo["entity"].as_str())
        .collect();
    assert!(iris.contains(&own.0.as_str()), "{iris:?}");
    assert!(iris.contains(&others.0.as_str()), "{iris:?}");
    let other = work_orders
        .iter()
        .find(|wo| wo["entity"] == json!(others.0))
        .expect("the other order's work order");
    for predicate in ["ekg:startedAt", "ekg:endedAt"] {
        assert_eq!(
            objects(other, predicate).len(),
            1,
            "its {predicate}: {other}"
        );
    }
    assert!(
        !objects(other, "ekg:part_of").contains(&order.0),
        "it belongs to another order: {other}"
    );

    // A work centre none of the order's work orders runs on is refused.
    let unknown = EntityUri(ontology::entity_iri("mrp.workcenter", 987_654));
    let refused = client
        .call_tool(call(
            "list_work_centre_schedule",
            json!({ "work_centre": unknown.0 }),
        ))
        .await
        .expect("call");
    assert_eq!(refused.is_error, Some(true), "{:?}", refused.content);

    // Read, not targeted: a plan that moves the other order's work order is refused.
    let plan = |target: &EntityUri| {
        call(
            "submit_plan",
            json!({
                "actions": [{
                    "action_type": "reschedule_work_order",
                    "target": target.0,
                    "params": { "new_date": "2031-01-06 08:00:00" }
                }],
                "explanation": "Move the work order out of the overlap."
            }),
        )
    };
    let rejected = client.call_tool(plan(&others)).await.expect("call");
    assert_eq!(rejected.is_error, Some(true), "{:?}", rejected.content);
    let message = serde_json::to_string(&rejected.content).expect("content");
    assert!(
        message.contains(&others.0) && message.contains("outside this task's scope"),
        "{message}"
    );
}

#[tokio::test]
#[ignore = "requires the live stack with the agent tool gateway (run with --ignored)"]
async fn the_live_gateway_grounds_a_seeded_order_and_refuses_what_is_out_of_scope() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let gateway = common::env_or("EKG_TOOL_GATEWAY_URL", "http://localhost:8091/mcp");
    let Some(secret) = std::env::var("EKG_TOOL_TOKEN_SECRET")
        .ok()
        .filter(|secret| !secret.trim().is_empty())
    else {
        common::infra_unavailable(
            "EKG_TOOL_TOKEN_SECRET unset (run `just podman-local-up`, then `set -a; source .env`)",
        );
        return;
    };
    let http = reqwest::Client::new();
    let anonymous = match http
        .post(&gateway)
        .header("content-type", "application/json")
        .body("{}")
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(response) => response,
        Err(why) => {
            common::infra_unavailable(&format!("tool gateway unreachable at {gateway}: {why}"));
            return;
        }
    };
    // No token: refused before MCP sees the request.
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(
        anonymous
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer")),
        "{:?}",
        anonymous.headers()
    );

    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let Ok(odoo) = Odoo::connect(
        &odoo_url,
        &common::env_or("ODOO_DB", "ekg"),
        &common::env_or("ODOO_USER", "admin"),
        &common::env_or("ODOO_API_KEY", "admin"),
    ) else {
        common::infra_unavailable(&format!("Odoo unreachable at {odoo_url}"));
        return;
    };
    let battery = EntityUri(ontology::entity_iri(
        "sale.order",
        seed_res_id(&odoo, "so_velomax").expect("battery sales order seeded"),
    ));
    let trike = EntityUri(ontology::entity_iri(
        "sale.order",
        seed_res_id(&odoo, "so_citylogix").expect("trike sales order seeded"),
    ));

    let token = mint(&secret, &battery);
    let client = ()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.clone()).auth_header(token),
        ))
        .await
        .expect("connect with the task token");

    let mut tools: Vec<String> = client
        .list_all_tools()
        .await
        .expect("list")
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    tools.sort();
    assert_eq!(tools, vec!["ground_entity", "related_entities"]);

    // The subject's grounding: visible edges, each traced to its Odoo record and revision.
    let result = client.call_tool(ground(&battery)).await.expect("call");
    assert_ne!(result.is_error, Some(true), "{:?}", result.content);
    let grounding = result.structured_content.expect("structured grounding");
    assert_eq!(grounding["entity"], json!(battery.0));
    let edges = grounding["edges"].as_array().expect("edges");
    assert!(!edges.is_empty(), "{grounding}");
    for edge in edges {
        assert_eq!(edge["source"]["system"], json!("odoo"), "{edge}");
        assert!(edge["source"]["id"].is_i64(), "{edge}");
        assert!(edge["revision"].is_u64(), "{edge}");
    }

    // Another seeded order is outside this task's scope: refused, naming it.
    let refused = client.call_tool(ground(&trike)).await.expect("call");
    assert_eq!(refused.is_error, Some(true), "{:?}", refused.content);
    let message = serde_json::to_string(&refused.content).expect("content");
    assert!(message.contains(&trike.0), "{message}");
}

/// Serve a gateway of this process over the stack's GraphDB and read its exported spans.
#[tokio::test]
#[ignore = "requires the live stack's GraphDB and Odoo (run with --ignored)"]
async fn a_tool_calls_store_requests_are_spans_under_it_and_a_monitors_read_is_none() {
    use ekg_cognition::tools::{GatewayConfig, ProfileRegistry, RunLedger, gateway_router};
    use ekg_semantic::{GraphDbClient, GraphDbWorldModel};
    use opentelemetry::trace::{
        FutureExt as _, SpanContext, SpanId, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    const TRACED: &str = "6d2f1c5a2b3e4f708192a3b4c5d6e7f8";
    const RUN_SPAN: &str = "00000000000000d4";

    let _ = rustls::crypto::ring::default_provider().install_default();
    let graphdb = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let repo = common::env_or("GRAPHDB_REPO", "ekg");
    let client = GraphDbClient::new(&graphdb, &repo).expect("client");
    if let Err(why) = client.repository_ids().await {
        common::infra_unavailable(&format!("no GraphDB at {graphdb}: {why}"));
        return;
    }
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let Ok(odoo) = Odoo::connect(
        &odoo_url,
        &common::env_or("ODOO_DB", "ekg"),
        &common::env_or("ODOO_USER", "admin"),
        &common::env_or("ODOO_API_KEY", "admin"),
    ) else {
        common::infra_unavailable(&format!("Odoo unreachable at {odoo_url}"));
        return;
    };
    let battery = EntityUri(ontology::entity_iri(
        "sale.order",
        seed_res_id(&odoo, "so_velomax").expect("battery sales order seeded"),
    ));

    let exporter = InMemorySpanExporter::default();
    opentelemetry::global::set_tracer_provider(
        SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build(),
    );
    let world: Arc<dyn WorldModel> = Arc::new(GraphDbWorldModel::new(
        client,
        ontology::supply_risk_contract().expect("contract"),
    ));
    let mut registry = ProfileRegistry::new(Arc::clone(&world));
    registry
        .register(supply_risk_profile(Arc::clone(&world), 4))
        .expect("register");
    let registry = Arc::new(registry);
    let profile = registry.get(SUPPLY_RISK_PROFILE).expect("profile");
    let tokens = Arc::new(TaskTokens::random().expect("secret"));
    let token = tokens
        .mint(
            &profile,
            TaskGrant::new(
                vec![battery.clone()],
                vec!["ground_entity".to_owned()],
                Duration::from_secs(60),
                TRACED,
            ),
        )
        .expect("mint");
    // As the engine does before a run: note the run span the task's calls hang under.
    let ledger = Arc::new(RunLedger::default());
    ledger.open(
        &tokens.verify_now(&token).expect("verify"),
        SpanContext::new(
            TraceId::from_hex(TRACED).expect("trace"),
            SpanId::from_hex(RUN_SPAN).expect("span"),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        ),
        ekg_cognition::tools::now_secs().expect("clock"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let router = gateway_router(
        registry,
        tokens,
        GatewayConfig::default()
            .with_call_log(Arc::new(|_| {}))
            .with_run_ledger(ledger),
    );
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });

    let mcp = ()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp"))
                .auth_header(token),
        ))
        .await
        .expect("connect");
    let result = mcp.call_tool(ground(&battery)).await.expect("call");
    assert_ne!(result.is_error, Some(true), "{:?}", result.content);

    // A monitor's read: the same store, outside any span.
    world
        .select("SELECT ?s WHERE { ?s ?p ?o } LIMIT 1")
        .with_context(opentelemetry::Context::new())
        .await
        .expect("select");

    let spans = exporter.get_finished_spans().expect("spans");
    let traced: Vec<_> = spans
        .iter()
        .filter(|span| span.span_context.trace_id().to_string() == TRACED)
        .collect();
    let tool = traced
        .iter()
        .find(|span| span.name == "execute_tool ground_entity")
        .expect("the tool call's span");
    assert_eq!(tool.parent_span_id.to_string(), RUN_SPAN);
    let under_the_call: Vec<_> = traced
        .iter()
        .filter(|span| span.parent_span_id == tool.span_context.span_id())
        .collect();
    assert!(
        under_the_call
            .iter()
            .any(|span| span.name == "graphdb select"),
        "the call's store requests are spans under it: {:?}",
        traced.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );
    for span in &under_the_call {
        let text = format!("{:?}", span.attributes);
        assert!(text.contains(&repo), "{text}");
        assert!(
            !text.contains("SELECT") && !text.contains(&battery.0),
            "{text}"
        );
        assert!(tool.start_time <= span.start_time && span.end_time <= tool.end_time);
    }
    // A store request outside a span would be a trace of its own: a root span.
    assert!(
        !spans
            .iter()
            .any(|span| span.name.starts_with("graphdb ") && span.parent_span_id == SpanId::INVALID),
        "the read outside a span exported nothing"
    );
}
