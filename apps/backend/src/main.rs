//! `ekg-backend` binary: serves the public `/api` surface, the durable Restate ingest
//! worker, and the internal agent tool gateway.
//!
//! Two HTTP surfaces run side by side: the axum `/api` router Odoo posts the webhook to,
//! which authenticates and submits to the Restate ingress, and the Restate SDK endpoint
//! the runtime calls back into to run the durable ingest virtual object. The `/api`
//! router also carries the human promote gate, the action-point routes, the health and
//! readiness endpoints, and the generated OpenAPI contract.

// Docs mention product names (Axum, Odoo, Restate, GraphDB).
#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ekg_backend::action_point::{self, ApiState};
use ekg_backend::config::{EventBus, IngestionConfig};
use ekg_backend::drift;
use ekg_backend::health::{self, ReadyState};
use ekg_backend::ingest::{AppState, router};
use ekg_backend::nats::NatsPublisher;
use ekg_backend::openapi;
use ekg_backend::portfolio_command;
use ekg_backend::projection::{self, VectorSink};
use ekg_backend::propose;
use ekg_backend::publisher::InProcessPublisher;
use ekg_backend::qdrant::QdrantClient;
use ekg_backend::skills::{
    DEFAULT_SKILL_CACHE_CAPACITY, RegistryClient, RegistrySource, SkillRegistry,
};
use ekg_backend::submit::RestateSubmitter;
use ekg_backend::sweep::{Sweep, trigger_first_sweep};
use ekg_backend::worker::{DerivationStore, Derivations, Ingest};
use ekg_domain::{
    ActionExecutor, CognitionProvider, Proposer, RdfChangePublisher, RdfChanged, SourceIngestor,
    WorldModel,
};
use ekg_dynamic::{ActionPointService, DriftMonitorService, PortfolioService};
use ekg_kinetic::{
    CostWithinLimit, NoPromisedDateMove, OdooActionExecutor, PolicyEngine, PromotionPredicate,
    StructuralChange, SupplierConfirmed,
};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use restate_sdk::prelude::{Endpoint, HttpServer};

/// Buffer for the in-process fan-out. A consumer lagging past this drops events (they
/// re-derive from the store) instead of blocking the write path.
const PUBLISHER_CAPACITY: usize = 1024;

/// The id and version recorded as the promoter of any auto-promotion. The allowlist it
/// governs is `EKG_AUTOPROMOTE_ACTIONS` (empty by default).
const AUTO_PROMOTE_POLICY_ID: &str = "local-autopromote/v1";

/// How long after apply an auto-applied change may still be reversed via the governed
/// `undo` command: a display window anchored to the durable `promoted_at`. Eight hours is
/// roughly the rest of an operator's working day ("reversible until end of day").
const GRAPHQL_UNDO_WINDOW_SECS: u64 = 8 * 60 * 60;

/// The monitor bridge's event source, chosen once by `EKG_EVENT_BUS`. Both drivers feed
/// `drift`'s single `forward(...)`; only where events come from differs.
enum Bridge {
    /// Drain the in-process broadcast fan-out (the default, no broker).
    InProcess(tokio::sync::broadcast::Receiver<RdfChanged>),
    /// Drain a durable JetStream consumer over the NATS backbone. Boxed because the
    /// JetStream context dwarfs the broadcast receiver (clippy `large_enum_variant`).
    Nats(Box<async_nats::jetstream::Context>),
}

// `main` is the composition root: it wires every seam to its real driver and
// stands up the HTTP surfaces, so it is long but flat.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(
        IngestionConfig::from_env()
            .map_err(|var| format!("missing required ingestion config: {var}"))?,
    );
    // Trace export, when an endpoint is configured; nothing otherwise.
    let telemetry = ekg_backend::telemetry::Telemetry::from_settings(|key| std::env::var(key).ok());
    // Every what-if simulator in the process shares one projection limit; fix it before any
    // projection runs.
    ekg_semantic::simulator::set_max_concurrent_projections(config.sim_max_projections);
    // Reclaim the scratch repositories an earlier process left behind. Off the startup path:
    // a store that is slow or down delays nothing, and a failure is only logged.
    let orphan_sweep = ekg_semantic::GraphDbSimulator::new(GraphDbClient::new(
        config.graphdb_url.clone(),
        config.graphdb_repo.clone(),
    )?);
    let orphan_max_age = config.sim_orphan_max_age;
    tokio::spawn(async move {
        match orphan_sweep.sweep_stale(orphan_max_age).await {
            Ok(reclaimed) => {
                for repo in reclaimed {
                    println!("reclaimed stale simulator scratch repo {repo}");
                }
            }
            Err(err) => eprintln!("warning: simulator: the stale scratch repo sweep failed: {err}"),
        }
    });

    // Public /api surface: authenticate and submit accepted webhooks to Restate.
    let submitter = Arc::new(RestateSubmitter::new(config.restate_ingress_url.clone()));
    let state = AppState {
        config: Arc::clone(&config),
        submitter,
    };
    let api_addr = std::env::var("EKG_BACKEND_ADDR").unwrap_or_else(|_| "0.0.0.0:8090".to_owned());

    // Durable ingest worker and reconciliation sweep, sharing the same drivers.
    // Connecting to Odoo up front fails at startup if the source is unreachable.
    // One contract instance: its source scopes bound the ingestor and it projects for the world.
    let contract = ontology::supply_risk_contract()?;
    let odoo = Arc::new(
        OdooIngestor::connect(OdooConfig {
            base_url: config.odoo_url.clone(),
            db: config.odoo_db.clone(),
            login: config.odoo_user.clone(),
            password: config.odoo_api_key.clone(),
            system: "odoo".to_owned(),
            allowlist: config.allowlist.clone(),
            scopes: contract.source_scopes(),
        })
        .await?,
    );
    let world = Arc::new(GraphDbWorldModel::new_with_read_policy(
        GraphDbClient::new(config.graphdb_url.clone(), config.graphdb_repo.clone())?,
        contract,
        config.read_policy(),
    ));
    // The event backbone: the in-process broadcast (default) or the durable NATS
    // JetStream driver, both behind `RdfChangePublisher`. Each consumer's event source is
    // captured in a `Bridge` and drained below. Built before serving so no event is
    // missed; an unreachable NATS fails here at startup. The vector projection, when
    // enabled, gets its own source (a second broadcast receiver, or its own durable
    // JetStream consumer).
    let (publisher, monitor_bridge, projection_bridge): (
        Arc<dyn RdfChangePublisher>,
        Bridge,
        Option<Bridge>,
    ) = match config.event_bus {
        EventBus::InProcess => {
            let publisher = Arc::new(InProcessPublisher::new(PUBLISHER_CAPACITY));
            let monitor = Bridge::InProcess(publisher.subscribe());
            let projection = config
                .vector_projection
                .then(|| Bridge::InProcess(publisher.subscribe()));
            (
                publisher as Arc<dyn RdfChangePublisher>,
                monitor,
                projection,
            )
        }
        EventBus::Nats => {
            let publisher = Arc::new(NatsPublisher::connect(&config.nats_url).await?);
            let monitor = Bridge::Nats(Box::new(publisher.context()));
            let projection = config
                .vector_projection
                .then(|| Bridge::Nats(Box::new(publisher.context())));
            (
                publisher as Arc<dyn RdfChangePublisher>,
                monitor,
                projection,
            )
        }
    };

    // The capability's structural derivations (precedes, used, fulfills, capacity), run
    // by the worker after each materialization and by the sweep after each retraction,
    // before the change is announced.
    let derivations = Derivations::new(
        Arc::new(GraphDbClient::new(
            config.graphdb_url.clone(),
            config.graphdb_repo.clone(),
        )?) as Arc<dyn DerivationStore>,
        ontology::STRUCTURAL_DERIVATIONS,
    );
    let ingest = Ingest::new(
        Arc::clone(&odoo) as Arc<dyn SourceIngestor>,
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher),
    )
    .with_derivations(derivations.clone());
    let sweep = Sweep::new(
        Arc::clone(&odoo),
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher),
        config.allowlist.clone(),
        config.sweep_interval_secs,
    )
    .with_derivations(derivations);
    // The dynamic layer: the durable drift monitor and the action-point virtual object
    // it opens on drift. Detection is deterministic; no model is in this path. The
    // monitor evaluates whatever rule list it is handed: supply-risk plus the two
    // process-drift rules (schedule infeasibility and resource contention), unioned per
    // subject, in the priority order documented on `drift::monitor_rules`. The read-model
    // tabs below share the same rule set, so their at-risk markers match the monitor's
    // current verdict.
    //
    // One cognition client for the process: the proposal agents, the slate agents and
    // the vector projection all use clones of it, so its generation permit pool bounds the
    // model calls process-wide.
    let cognition = Arc::new(propose::provider(&config)?) as Arc<dyn CognitionProvider>;
    let rules = drift::monitor_rules();
    // The monitor also groups coupled points awaiting oversight into durable portfolios by
    // the capability's injected coupling rules; the portfolio object records each group.
    // Grouping commits nothing.
    let monitor = DriftMonitorService::new(
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&rules),
    )
    .with_coupling(drift::coupling_rules());
    // Every point's outcome goes to the shadow store, which keeps it only for points a
    // shadow run was recorded beside.
    let action_point = ActionPointService::default()
        .with_outcome_listener(ekg_backend::shadow::AGENT_SHADOW_OBJECT);
    // Every portfolio's outcome goes there too, for the portfolios a joint shadow run was
    // recorded beside.
    let portfolio =
        PortfolioService::default().with_outcome_listener(ekg_backend::shadow::AGENT_SHADOW_OBJECT);
    // The agent tool gateway's profiles, built here because the skill registry validates
    // every published skill against the tools they offer.
    let tool_profiles = Arc::new(propose::tool_profiles(
        &(Arc::clone(&world) as Arc<dyn WorldModel>),
        config.max_plan_actions,
    )?);
    let skill_registry = SkillRegistry::new(tool_profiles.offered_tools());
    let restate_endpoint = Endpoint::builder()
        .bind(ingest)
        .bind(sweep)
        .bind(monitor)
        .bind(action_point)
        .bind(portfolio)
        .bind(skill_registry)
        // The agent's governed rollout mode and its shadow records.
        .bind(ekg_backend::rollout::AgentRollout)
        .bind(ekg_backend::shadow::AgentShadow)
        .build();
    let restate_addr: SocketAddr = std::env::var("EKG_RESTATE_SERVICE_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9080".to_owned())
        .parse()?;

    // The governed write-back driver for the human promote gate. It holds its own Odoo
    // session and, like the ingestor above, fails at startup if the source is unreachable.
    let executor = Arc::new(
        OdooActionExecutor::connect(
            &config.odoo_url,
            &config.odoo_db,
            &config.odoo_user,
            &config.odoo_api_key,
        )
        .await?,
    );
    // Readiness probes the downstreams the governed path needs (Restate, GraphDB, Odoo)
    // with a bounded timeout.
    let ready_state = ReadyState::new(
        config.restate_admin_url.clone(),
        config.graphdb_url.clone(),
        config.odoo_url.clone(),
    )?;

    // The read-only GraphQL surface: its own world-model and what-if simulator handles (for
    // the tab and lane projections and the derived display metrics) and the active policy's
    // display values. It defines no mutation root, so every governed change stays a REST
    // command.
    let graphql_simulator = Arc::new(ekg_semantic::GraphDbSimulator::new(GraphDbClient::new(
        config.graphdb_url.clone(),
        config.graphdb_repo.clone(),
    )?)) as Arc<dyn ekg_domain::Simulator>;
    let graphql_state = ekg_backend::graphql::GraphqlState {
        http: reqwest::Client::new(),
        ingress_url: config.restate_ingress_url.clone(),
        admin_url: config.restate_admin_url.clone(),
        world: Arc::clone(&world) as Arc<dyn WorldModel>,
        simulator: graphql_simulator,
        rules: Arc::clone(&rules),
        policy: ekg_backend::graphql::PolicyDisplay {
            policy_id: AUTO_PROMOTE_POLICY_ID.to_owned(),
            cost_limit: config.auto_promote_cost_limit,
            cost_limit_currency: Some(config.auto_promote_cost_limit_currency.clone())
                .filter(|currency| !currency.is_empty()),
            confirmed_states: config.auto_promote_confirmed_states.clone(),
            auto_apply_actions: config.auto_promote_actions.clone(),
            undo_window: Duration::from_secs(GRAPHQL_UNDO_WINDOW_SECS),
        },
    };

    // The governed commands' shared state: the point commands and the portfolio commands
    // run the same claim protocol over the same ingress.
    let command_state = ApiState {
        http: reqwest::Client::new(),
        ingress_url: config.restate_ingress_url.clone(),
        admin_url: config.restate_admin_url.clone(),
        executor: Arc::clone(&executor) as Arc<dyn ekg_domain::ActionExecutor>,
        undo_window: Duration::from_secs(GRAPHQL_UNDO_WINDOW_SECS),
        claim_deadline: config.command_claim,
    };

    // The public /api surface: the ingestion webhook, the human oversight routes for
    // action points and portfolios, the read-only GraphQL surface, the liveness and
    // readiness endpoints, and the OpenAPI contract.
    let app = router(state)
        .merge(action_point::router(command_state.clone()))
        .merge(portfolio_command::router(command_state))
        .merge(ekg_backend::graphql::router(graphql_state))
        .merge(health::router(ready_state))
        .merge(openapi::router());

    // The agent tool gateway: the read-only MCP surface a runtime agent reads the world
    // model through. It has its own internal listener and is never merged into `/api`,
    // so the browser proxy and the API contract stay agent-free. Each capability
    // registers its profile (read context and related-entities hook).
    let tool_tokens = Arc::new(if let Some(secret) = &config.tool_token_secret {
        ekg_cognition::tools::TaskTokens::new(secret.as_bytes())?
    } else {
        println!(
            "ekg-backend tool gateway: EKG_TOOL_TOKEN_SECRET unset, task tokens are signed \
             with a random per-process secret (set it to share tokens across processes)"
        );
        ekg_cognition::tools::TaskTokens::random()?
    });
    // The gateway's per-task run ledger, shared in-process with the agent proposer, which
    // takes each run's accepted plan from it. It is never served.
    let run_ledger = Arc::new(ekg_cognition::tools::RunLedger::default());
    let gateway = ekg_cognition::tools::gateway_router(
        Arc::clone(&tool_profiles),
        Arc::clone(&tool_tokens),
        ekg_cognition::tools::GatewayConfig::default()
            .with_run_ledger(Arc::clone(&run_ledger))
            .with_text_bound(config.agent_text_max_chars)
            // The names the runtime reaches the engine by: the compose service, and the
            // host from a container when the engine runs on the host (`just backend`).
            .allow_host("ekg-backend")
            .allow_host("host.containers.internal")
            // The skill versions a task's token pins, read from the durable skill registry
            // over the ingress.
            .with_skill_source(Arc::new(RegistrySource::new(
                RegistryClient::new(reqwest::Client::new(), config.restate_ingress_url.clone()),
                DEFAULT_SKILL_CACHE_CAPACITY,
            ))),
    );
    let gateway_addr = SocketAddr::from(([0, 0, 0, 0], config.tool_gateway_port));

    // Serve the surfaces concurrently; if any exits, the process exits.
    let api = async move {
        let listener = tokio::net::TcpListener::bind(&api_addr).await?;
        println!(
            "ekg-backend /api listening on {api_addr} \
             (POST /api/ingest/odoo, GET/POST /api/action-points, \
             POST /api/portfolios/{{promote,undo,abort}}, \
             GET /api/health, GET /api/ready, GET /api/openapi.json)"
        );
        axum::serve(listener, app).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    let tools = async move {
        let listener = tokio::net::TcpListener::bind(gateway_addr).await?;
        println!(
            "ekg-backend agent tool gateway listening on {gateway_addr} \
             (MCP at /mcp, skills at /skills, bearer task token, not part of /api)"
        );
        axum::serve(listener, gateway).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    let worker = async move {
        println!("ekg-backend Restate ingest endpoint on {restate_addr}");
        HttpServer::new(restate_endpoint)
            .listen_and_serve(restate_addr)
            .await;
        Ok::<(), Box<dyn std::error::Error>>(())
    };

    // Bridge the RdfChanged fan-out into the durable monitor: every materialization
    // (from the worker or the sweep) is forwarded to the monitor, which evaluates the
    // rules and opens an action point on drift. The event source is `monitor_bridge`,
    // chosen above.
    {
        let cognition = Arc::clone(&cognition);
        // Two proposal agents behind one router: supply-risk points (`sale.order`) and
        // process-drift points (`mrp.*`) each reach their own bounded agent. The bridge
        // runs the router on each fresh open.
        let supply_risk = Arc::new(propose::supply_risk_agent(
            Arc::clone(&world) as Arc<dyn WorldModel>,
            Arc::clone(&cognition),
            config.max_plan_actions,
        )) as Arc<dyn Proposer>;
        let process = Arc::new(propose::process_agent(
            Arc::clone(&world) as Arc<dyn WorldModel>,
            Arc::clone(&cognition),
            config.max_plan_actions,
        )) as Arc<dyn Proposer>;
        let agent =
            Arc::new(drift::RoutingProposer::new(supply_risk, process)) as Arc<dyn Proposer>;
        // The slate agents loop propose, simulate, observe, revise over a GraphDB what-if
        // projection and the capability's own scorer, and emit a ranked slate the bridge
        // attaches to a freshly opened point. They only read and propose; a projection
        // commits nothing.
        let simulator = Arc::new(ekg_semantic::GraphDbSimulator::new(GraphDbClient::new(
            config.graphdb_url.clone(),
            config.graphdb_repo.clone(),
        )?)) as Arc<dyn ekg_domain::Simulator>;
        // One slate agent per capability, behind a router that dispatches each subject to
        // its own: a `sale.order` point to the supply-risk slate, an `mrp.*` point to the
        // process slate. Each loop is bounded by the configured time budget; the bridge's
        // outer guard sits above it (budget, plus one call's cap, plus a margin) and only
        // abandons a hung task.
        let supply_risk_slate = Arc::new(
            propose::supply_risk_slate_agent(
                Arc::clone(&world) as Arc<dyn WorldModel>,
                Arc::clone(&cognition),
                Arc::clone(&simulator),
                config.max_plan_actions,
            )
            .with_budget(config.slate_budget),
        ) as Arc<dyn ekg_domain::SlateProposer>;
        let process_slate = Arc::new(
            propose::process_slate_agent(
                Arc::clone(&world) as Arc<dyn WorldModel>,
                Arc::clone(&cognition),
                Arc::clone(&simulator),
                config.max_plan_actions,
            )
            .with_budget(config.slate_budget),
        ) as Arc<dyn ekg_domain::SlateProposer>;
        // The joint portfolio slate: one slate per formed portfolio, planning across its
        // members with both capabilities' catalogs, ranked first by how many members the
        // monitor's own rules still flag in a projection. Its members get no per-point slate
        // (they fall back to one if it fails).
        let portfolio_slate = Arc::new(
            ekg_backend::joint::portfolio_slate_agent(
                &(Arc::clone(&world) as Arc<dyn WorldModel>),
                &cognition,
                Arc::clone(&simulator),
                Arc::clone(&rules),
                config.max_plan_actions,
            )
            .with_budget(config.slate_budget),
        ) as Arc<dyn ekg_domain::GroupSlateProposer>;
        // The agent's branch: beside each freshly opened point's slate, and each formed
        // portfolio's joint slate, the agent's slate in its own task, only while the durable
        // rollout mode allows it (`legacy`, the default, runs none). In `shadow` it is recorded
        // apart; in `agent` it is the point's slate when its run completed, and the portfolio's
        // only under criteria that cover joint slates. The agent runs in the served runtime, so the direct cognition
        // route has neither.
        let shadow = match &config.cognition_route {
            ekg_cognition::CognitionRoute::Runtime { url } => {
                // A point's agent run is bounded like one model call; a portfolio's has limits
                // of its own, and with them a guard of its own.
                let (limits, joint_limits) = (config.agent_limits(), config.agent_joint_limits());
                let analyst_limits = config.agent_analyst_limits();
                let profile = |id: &str| {
                    tool_profiles
                        .get(id)
                        .ok_or_else(|| format!("tool profile {id:?} is not registered"))
                };
                let seams = propose::AgentSeams {
                    world: Arc::clone(&world) as Arc<dyn WorldModel>,
                    simulator: Arc::clone(&simulator),
                    supply_risk: profile(propose::SUPPLY_RISK_PROFILE)?,
                    process: profile(propose::PROCESS_PROFILE)?,
                    portfolio: profile(propose::PORTFOLIO_PROFILE)?,
                    rules: Arc::clone(&rules),
                    tokens: Arc::clone(&tool_tokens),
                    reasoner: Arc::new(ekg_cognition::RuntimeReasoner::new(
                        url,
                        config.agent_shadow_concurrency,
                    )?),
                    ledger: Arc::clone(&run_ledger),
                    pins: Arc::new(RegistryClient::new(
                        reqwest::Client::new(),
                        config.restate_ingress_url.clone(),
                    )),
                    limits,
                    joint_limits,
                    analyst_limits,
                    supply_risk_analyst: profile(propose::SUPPLY_RISK_ANALYST_PROFILE)?,
                    process_analyst: profile(propose::PROCESS_ANALYST_PROFILE)?,
                    findings: Arc::new(propose::SemanticFindings::new(
                        Arc::clone(&world) as Arc<dyn WorldModel>,
                        profile(propose::SUPPLY_RISK_ANALYST_PROFILE)?,
                        profile(propose::PROCESS_ANALYST_PROFILE)?,
                        config.agent_text_max_chars,
                    )),
                    planner_strategies: propose::planner_strategies(config.agent_planners),
                    planner_reads: config.agent_planner_reads,
                    text_max_chars: config.agent_text_max_chars,
                    budget: config.slate_budget,
                };
                Some(ekg_backend::slates::ShadowStep {
                    rollout: Arc::new(ekg_backend::rollout::RolloutClient::new(
                        reqwest::Client::new(),
                        config.restate_ingress_url.clone(),
                    )),
                    store: Arc::new(ekg_backend::shadow::ShadowClient::new(
                        reqwest::Client::new(),
                        config.restate_ingress_url.clone(),
                    )),
                    slates: {
                        let seams = seams.clone();
                        Arc::new(move |run: ekg_backend::slates::ShadowRun| {
                            let (supply_risk, process) =
                                propose::agent_slate_agents(&seams, &run.gate, &run.recorder);
                            Arc::new(drift::RoutingSlateProposer::new(
                                Arc::new(supply_risk),
                                Arc::new(process),
                            )) as Arc<dyn ekg_domain::SlateProposer>
                        })
                    },
                    // The agent's joint slate for a formed portfolio: the same slate loop,
                    // simulator and scorer as the joint slate above, drafted by the agent.
                    joint: Arc::new(move |run: ekg_backend::slates::ShadowRun| {
                        Arc::new(ekg_backend::joint::agent_portfolio_slate_agent(
                            &seams,
                            &run.gate,
                            &run.recorder,
                        )) as Arc<dyn ekg_domain::GroupSlateProposer>
                    }),
                    permits: Arc::new(tokio::sync::Semaphore::new(
                        config.agent_shadow_concurrency.get(),
                    )),
                    guard: drift::slate_guard(config.slate_budget, limits.run_cap),
                    // An attempt in flight may have its analysts and its planner still to run.
                    joint_guard: drift::slate_guard(
                        config.slate_budget,
                        analyst_limits.run_cap + joint_limits.run_cap,
                    ),
                })
            }
            ekg_cognition::CognitionRoute::Direct => {
                println!(
                    "ekg-backend agent shadow: unavailable on the direct cognition route \
                     (the agent runs in the served runtime); no shadow task starts"
                );
                None
            }
        };
        let slate_step = Some(drift::SlateStep {
            proposer: Arc::new(drift::RoutingSlateProposer::new(
                supply_risk_slate,
                process_slate,
            )) as Arc<dyn ekg_domain::SlateProposer>,
            portfolio: Some(portfolio_slate),
            guard: drift::slate_guard(config.slate_budget, config.cognition_limits.total_timeout),
            shadow,
        });
        let ingress = config.restate_ingress_url.clone();
        // The promotion policy: human-gated by default, auto-promoting only a plan whose
        // every action is allowlisted and reversible and passes every predicate below.
        // Config carries the allowlist, cost limit, and confirmed states.
        let predicates: Vec<Arc<dyn PromotionPredicate>> = vec![
            Arc::new(NoPromisedDateMove),
            Arc::new(StructuralChange),
            Arc::new(CostWithinLimit::new(config.auto_promote_cost_limit)),
            Arc::new(SupplierConfirmed::new(
                config
                    .auto_promote_confirmed_states
                    .iter()
                    .cloned()
                    .collect(),
            )),
        ];
        let policy = PolicyEngine::with_predicates(
            AUTO_PROMOTE_POLICY_ID,
            config.auto_promote_actions.iter().cloned().collect(),
            predicates,
        );
        // The bridge shares the same governed write-back driver as the human gate.
        let executor = Arc::clone(&executor) as Arc<dyn ActionExecutor>;
        match monitor_bridge {
            Bridge::InProcess(rx) => {
                println!(
                    "ekg-backend drift monitor bridge subscribed to the in-process RdfChanged \
                     fan-out"
                );
                tokio::spawn(drift::run_monitor_bridge(
                    rx,
                    ingress,
                    config.command_claim,
                    agent,
                    policy,
                    executor,
                    Arc::clone(&world) as Arc<dyn WorldModel>,
                    slate_step,
                ));
            }
            Bridge::Nats(js) => {
                println!("ekg-backend drift monitor bridge consuming the NATS JetStream backbone");
                tokio::spawn(drift::run_monitor_bridge_nats(
                    *js,
                    ingress,
                    config.command_claim,
                    agent,
                    policy,
                    executor,
                    Arc::clone(&world) as Arc<dyn WorldModel>,
                    slate_step,
                ));
            }
        }
    }

    // The vector projection: a second, independent consumer of the same backbone that
    // keeps a non-authoritative Qdrant index current (a change is grounded, embedded and
    // upserted; a retraction deletes). Off unless EKG_VECTOR_PROJECTION=on. Connecting
    // ensures the collection at the pinned embedding width, so an unreachable Qdrant
    // fails at startup.
    if let Some(projection_bridge) = projection_bridge {
        let world = Arc::clone(&world) as Arc<dyn WorldModel>;
        let cognition = Arc::clone(&cognition);
        let qdrant = Arc::new(
            QdrantClient::connect(
                &config.qdrant_url,
                &config.qdrant_collection,
                config.cognition_embed_dim,
            )
            .await?,
        ) as Arc<dyn VectorSink>;
        match projection_bridge {
            Bridge::InProcess(rx) => {
                println!(
                    "ekg-backend vector projection subscribed to the in-process RdfChanged fan-out"
                );
                tokio::spawn(projection::run_vector_projection(
                    rx, world, cognition, qdrant,
                ));
            }
            Bridge::Nats(js) => {
                println!("ekg-backend vector projection consuming the NATS JetStream backbone");
                tokio::spawn(projection::run_vector_projection_nats(
                    *js, world, cognition, qdrant,
                ));
            }
        }
    }

    // Best-effort first sweep trigger: the sweep only reschedules itself after its first
    // pass, so on a fresh boot something has to start it. Not idempotent: if a timer
    // chain survived in Restate (a restart against kept state), this adds a second one.
    // Set `EKG_SWEEP_BOOTSTRAP=false` where the timer is known to be running (the
    // containerized stack does, and triggers once after registering). Registration is a
    // deploy step (`just restate-register`), so the trigger may 404 until then; that is
    // logged and `/api` stays up.
    if config.sweep_bootstrap {
        let ingress = config.restate_ingress_url.clone();
        tokio::spawn(async move {
            // Let the local SDK endpoint bind before asking the runtime to invoke it.
            tokio::time::sleep(Duration::from_millis(500)).await;
            match trigger_first_sweep(&ingress).await {
                Ok(()) => println!("ekg-backend triggered the first reconciliation sweep"),
                Err(why) => eprintln!(
                    "warning: ekg-backend could not trigger the first sweep ({why}); \
                     run `just sweep-trigger` once Restate is reachable and the \
                     deployment is registered"
                ),
            }
        });
    }

    let outcome: Result<(), Box<dyn std::error::Error>> = tokio::select! {
        result = api => result,
        result = tools => result,
        result = worker => result,
        () = stop_signal() => Ok(()),
    };
    // Whatever ended the server, send the spans still queued, within the flush bound.
    telemetry.shutdown();
    outcome
}

/// Resolves when the process is asked to stop (Ctrl-C, or SIGTERM from a container runtime).
async fn stop_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = interrupt => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = interrupt.await;
}
