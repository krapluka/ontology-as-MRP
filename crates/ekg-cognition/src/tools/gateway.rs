//! The MCP server: [`ToolGateway`] (an `rmcp` [`ServerHandler`]) behind the bearer check of
//! [`gateway_router`].
//!
//! Every request authenticates itself with a task token, so the service is stateless (no
//! MCP session store) and answers plain JSON. The bearer middleware verifies the token and
//! resolves its profile before MCP sees the request; the handler reads the resulting
//! [`TaskContext`] from the request extensions.

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use ekg_domain::WorldModel;
use opentelemetry::trace::FutureExt;
use opentelemetry::{Context, KeyValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;

use super::finding::{SUBMIT_FINDING, parse_finding, unread_citations};
use super::ledger::{ObservedCall, RunLedger};
use super::profile::{
    AgentTool, DEFAULT_TEXT_BOUND, ProfileRegistry, ReadOnlyWorld, ToolError, ToolProfile,
};
use super::scope::{DEFAULT_SCOPE_CACHE_CAPACITY, ScopeCache, TaskScope};
use super::skill_routes::{self, SkillLog, SkillRecord, SkillSource};
use super::submit::{SUBMIT_PLAN, plan_actions};
use super::token::{TaskClaims, TaskTokens, TokenError, now_secs};
use crate::telemetry;

/// The `WWW-Authenticate` challenge every refused request is answered with.
pub const WWW_AUTHENTICATE: &str = "Bearer realm=\"ekg-tool-gateway\"";

/// The path the MCP service is served at.
const MCP_PATH: &str = "/mcp";

/// The `Host` names admitted by default (the SDK's DNS-rebinding protection): loopback
/// only. A deployment adds the names the runtime reaches it by.
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The verified task a request runs for, inserted by the bearer middleware.
#[derive(Debug, Clone)]
pub struct TaskContext {
    /// The verified token's claims.
    pub claims: TaskClaims,
    /// The profile the token names.
    pub profile: Arc<ToolProfile>,
}

/// One `tools/call`, as the call log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    /// The task's trace id.
    pub trace_id: String,
    /// The token id.
    pub jti: String,
    /// The profile id.
    pub profile: String,
    /// The tool called.
    pub tool: String,
    /// The entity the call named, if any.
    pub entity: Option<String>,
    /// `ok`, `refused`, `rejected` or `error`.
    pub outcome: &'static str,
}

impl fmt::Display for CallRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tool-gateway trace={} jti={} profile={} tool={} entity={} outcome={}",
            self.trace_id,
            self.jti,
            self.profile,
            self.tool,
            self.entity.as_deref().unwrap_or("-"),
            self.outcome
        )
    }
}

/// Where call records go. The default writes each as one line to the engine's log.
pub type CallLog = Arc<dyn Fn(&CallRecord) + Send + Sync>;

/// How the gateway router is served.
#[derive(Clone)]
pub struct GatewayConfig {
    allowed_hosts: Vec<String>,
    call_log: CallLog,
    scope_cache_capacity: usize,
    skill_source: Option<Arc<dyn SkillSource>>,
    skill_log: SkillLog,
    run_ledger: Option<Arc<RunLedger>>,
    text_bound: usize,
}

impl fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("allowed_hosts", &self.allowed_hosts)
            .field("scope_cache_capacity", &self.scope_cache_capacity)
            .field("serves_skills", &self.skill_source.is_some())
            .field("keeps_run_ledger", &self.run_ledger.is_some())
            .field("text_bound", &self.text_bound)
            .finish_non_exhaustive()
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: LOOPBACK_HOSTS.iter().map(|&host| host.to_owned()).collect(),
            call_log: Arc::new(|record: &CallRecord| eprintln!("{record}")),
            scope_cache_capacity: DEFAULT_SCOPE_CACHE_CAPACITY,
            skill_source: None,
            skill_log: Arc::new(|record: &SkillRecord| eprintln!("{record}")),
            run_ledger: None,
            text_bound: DEFAULT_TEXT_BOUND,
        }
    }
}

impl GatewayConfig {
    /// Also admit requests whose `Host` names `host` (a bare name matches any port), e.g.
    /// the service name the runtime reaches the engine by.
    #[must_use]
    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        let host = host.into();
        if !self.allowed_hosts.contains(&host) {
            self.allowed_hosts.push(host);
        }
        self
    }

    /// Send call records to `log` instead of the engine's log.
    #[must_use]
    pub fn with_call_log(mut self, log: CallLog) -> Self {
        self.call_log = log;
        self
    }

    /// Hold at most `capacity` task scopes at once.
    #[must_use]
    pub fn with_scope_cache_capacity(mut self, capacity: usize) -> Self {
        self.scope_cache_capacity = capacity;
        self
    }

    /// Serve the skill versions tokens pin from `source` (without one, every skill route
    /// answers 404).
    #[must_use]
    pub fn with_skill_source(mut self, source: Arc<dyn SkillSource>) -> Self {
        self.skill_source = Some(source);
        self
    }

    /// Send skill records to `log` instead of the engine's log.
    #[must_use]
    pub fn with_skill_log(mut self, log: SkillLog) -> Self {
        self.skill_log = log;
        self
    }

    /// Record every task's calls and accepted plan in `ledger`, which the engine shares and
    /// reads after each task. The ledger is never served.
    #[must_use]
    pub fn with_run_ledger(mut self, ledger: Arc<RunLedger>) -> Self {
        self.run_ledger = Some(ledger);
        self
    }

    /// Hand a tool a literal of at most `chars` characters; a longer one is withheld.
    #[must_use]
    pub fn with_text_bound(mut self, chars: usize) -> Self {
        self.text_bound = chars;
        self
    }

    /// The `Host` names admitted.
    #[must_use]
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }
}

/// The MCP handler: lists a task's allowed tools and runs them through the task's
/// read-only world and scope. It holds the world model only to hand each call a
/// [`super::ReadOnlyWorld`]; no tool ever receives it.
#[derive(Clone)]
pub struct ToolGateway {
    world: Arc<dyn WorldModel>,
    scopes: Arc<ScopeCache>,
    log: CallLog,
    ledger: Option<Arc<RunLedger>>,
    text_bound: usize,
}

impl ToolGateway {
    /// Log one call, and record it in the run ledger with the tool's `result` when the tool
    /// ran.
    fn record(
        &self,
        task: &TaskContext,
        tool: &str,
        entity: Option<String>,
        outcome: &'static str,
        result: Option<&Result<Value, ToolError>>,
    ) {
        if let Some(ledger) = &self.ledger {
            let call = ObservedCall {
                tool,
                outcome,
                result,
            };
            ledger.record(&task.claims, &call, now_secs().unwrap_or(0));
        }
        (self.log)(&CallRecord {
            trace_id: task.claims.trace_id.clone(),
            jti: task.claims.jti.clone(),
            profile: task.claims.profile.clone(),
            tool: tool.to_owned(),
            entity,
            outcome,
        });
    }
}

impl ToolGateway {
    /// The span of one call of `task`: under the caller's span when the request continues the
    /// task's trace, under the task's run span otherwise, and none when neither is known.
    /// It names `tool` only when the profile offers it: any other name is the caller's text.
    fn call_span(
        &self,
        task: &TaskContext,
        context: &RequestContext<RoleServer>,
        tool: &str,
    ) -> Context {
        let headers = context
            .extensions
            .get::<axum::http::request::Parts>()
            .map(|parts| &parts.headers);
        let run_span = self
            .ledger
            .as_ref()
            .and_then(|ledger| ledger.run_span(&task.claims.jti));
        let parent = telemetry::call_parent(headers, &task.claims.trace_id, run_span);
        telemetry::call_span(parent, format!("execute_tool {}", offered(task, tool)))
    }

    /// What the task still has to read before what it hands in is accepted: asked of the
    /// profile's required reads when `tool` is a hand-in tool, against the task's calls so far.
    /// Nothing for any other tool, and nothing without a run ledger, which knows the calls.
    async fn unread(
        &self,
        task: &TaskContext,
        tool: &str,
        world: &ReadOnlyWorld,
        scope: &TaskScope,
    ) -> Result<Vec<String>, ToolError> {
        let required = task.profile.required_reads();
        let Some(ledger) = self
            .ledger
            .as_ref()
            .filter(|_| matches!(tool, SUBMIT_PLAN | SUBMIT_FINDING))
        else {
            return Ok(Vec::new());
        };
        if required.is_empty() {
            return Ok(Vec::new());
        }
        let run = ledger.so_far(&task.claims.jti).unwrap_or_default();
        let mut missing = Vec::new();
        for read in required {
            missing.extend(read.missing(&run, world, scope).await?);
        }
        Ok(missing)
    }

    /// What the profile's plan rules hold against what `tool` hands in. Nothing for any tool
    /// but `submit_plan`, and a malformed action is `submit_plan`'s own to refuse.
    async fn plan_defects(
        &self,
        task: &TaskContext,
        tool: &str,
        args: &Value,
    ) -> Result<Vec<String>, ToolError> {
        let rules = task.profile.plan_rules();
        if tool != SUBMIT_PLAN || rules.is_empty() {
            return Ok(Vec::new());
        }
        let actions = plan_actions(args);
        let mut defects = Vec::new();
        for rule in rules {
            defects.extend(rule.defects(&actions).await?);
        }
        Ok(defects)
    }

    /// What a finding cites and its task did not read. Without a ledger nothing was read: such
    /// a gateway has no record to take a finding from either.
    fn unread_in(&self, task: &TaskContext, args: &Value) -> Vec<String> {
        let Ok(finding) = parse_finding(args) else {
            return Vec::new();
        };
        let run = self
            .ledger
            .as_ref()
            .and_then(|ledger| ledger.so_far(&task.claims.jti))
            .unwrap_or_default();
        unread_citations(&finding, &run)
    }

    /// Run one call and say how it ended, as the call log names it.
    async fn serve_call(
        &self,
        task: &TaskContext,
        request: CallToolRequestParams,
    ) -> (&'static str, Result<CallToolResponse, ErrorData>) {
        let name = request.name.as_ref();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let Some(tool) = allowed_tools(task).find(|tool| tool.name() == name) else {
            self.record(task, name, None, "refused", None);
            return (
                "refused",
                Err(ErrorData::invalid_params(
                    format!("tool {name:?} is not allowed for this task"),
                    None,
                )),
            );
        };
        let entity = tool.entity_of(&args);
        let world = task
            .profile
            .reader(Arc::clone(&self.world))
            .with_text_bound(self.text_bound);
        let now = match now_secs() {
            Ok(now) => now,
            Err(e) => return ("error", Err(ErrorData::internal_error(e.to_string(), None))),
        };
        let scope = match self.scopes.scope(&task.claims, &world, now).await {
            Ok(scope) => scope,
            Err(error) => {
                self.record(task, name, entity, "error", None);
                return (
                    "error",
                    Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "failed: the task's scope could not be resolved: {error}"
                    ))])
                    .into()),
                );
            }
        };
        let citations = if name == SUBMIT_FINDING {
            self.unread_in(task, &args)
        } else {
            Vec::new()
        };
        let result = match self.unread(task, name, &world, &scope).await {
            Ok(missing) if missing.is_empty() => match self.plan_defects(task, name, &args).await {
                Ok(defects) if defects.is_empty() => {
                    with_citations(tool.call(args, &world, &scope).await, citations)
                }
                Ok(defects) => Err(ToolError::Rejected {
                    defects,
                    guidance: Some("Change the plan, then submit again.".to_owned()),
                }),
                Err(error) => Err(error),
            },
            Ok(defects) => Err(ToolError::Rejected {
                defects,
                guidance: Some(
                    "What you handed in was not checked. Make the reads, then submit again."
                        .to_owned(),
                ),
            }),
            Err(error) => Err(error),
        };
        let outcome = match &result {
            Ok(_) => "ok",
            Err(error) => error.outcome(),
        };
        self.record(task, name, entity, outcome, Some(&result));
        let response = match result {
            Ok(value @ Value::Object(_)) => CallToolResult::structured(value),
            Ok(other) => CallToolResult::success(vec![ContentBlock::text(other.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        (outcome, Ok(response.into()))
    }
}

/// `result` with the finding's `citations` among its defects, so that one rejection names them
/// all.
fn with_citations(
    result: Result<Value, ToolError>,
    citations: Vec<String>,
) -> Result<Value, ToolError> {
    match (result, citations.is_empty()) {
        (result, true) => result,
        (Ok(_), false) => Err(ToolError::Rejected {
            defects: citations,
            guidance: None,
        }),
        (
            Err(ToolError::Rejected {
                mut defects,
                guidance,
            }),
            false,
        ) => {
            defects.extend(citations);
            Err(ToolError::Rejected { defects, guidance })
        }
        (Err(other), false) => Err(other),
    }
}

/// `tool` when the task's profile offers it, `unknown` otherwise.
fn offered<'a>(task: &TaskContext, tool: &'a str) -> &'a str {
    if task.profile.tool_names().contains(&tool) {
        tool
    } else {
        "unknown"
    }
}

/// The task the bearer middleware verified for this request.
fn task_context(context: &RequestContext<RoleServer>) -> Result<TaskContext, ErrorData> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<TaskContext>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("tool gateway: no verified task", None))
}

/// A tool as MCP lists it.
fn mcp_tool(tool: &dyn AgentTool) -> Tool {
    let schema = match tool.input_schema() {
        Value::Object(schema) => schema,
        _ => serde_json::Map::new(),
    };
    Tool::new(
        tool.name().to_owned(),
        tool.description().to_owned(),
        Arc::new(schema),
    )
}

/// The tools a task may use: offered by its profile and allowed by its token.
fn allowed_tools(task: &TaskContext) -> impl Iterator<Item = &Arc<dyn AgentTool>> {
    task.profile
        .tools()
        .iter()
        .filter(|tool| task.claims.tools.iter().any(|name| name == tool.name()))
}

impl ServerHandler for ToolGateway {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "ekg-tool-gateway",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Read-only tools over the governed world model, scoped to this task. Every \
                 result is filtered by the read policy; omissions are disclosed.",
            )
    }

    // The trait's signature is async; listing reads only the verified task.
    #[allow(clippy::unused_async_trait_impl)]
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let task = task_context(&context)?;
        Ok(ListToolsResult {
            tools: allowed_tools(&task)
                .map(|tool| mcp_tool(tool.as_ref()))
                .collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let task = task_context(&context)?;
        let tool = offered(&task, request.name.as_ref()).to_owned();
        let span = self.call_span(&task, &context, &tool);
        // The call runs with its span current, so the store requests it makes hang under it.
        let (outcome, response) = self
            .serve_call(&task, request)
            .with_context(span.clone())
            .await;
        telemetry::finish(
            &span,
            vec![
                KeyValue::new("gen_ai.operation.name", "execute_tool"),
                KeyValue::new("gen_ai.tool.name", tool),
                KeyValue::new("ekg.tool.outcome", outcome),
            ],
            outcome == "ok",
            Some(outcome),
        );
        response
    }
}

/// What the bearer middleware checks a request against.
#[derive(Clone)]
struct Auth {
    registry: Arc<ProfileRegistry>,
    tokens: Arc<TaskTokens>,
}

/// Why a request was refused before MCP saw it.
#[derive(Debug)]
enum Refusal {
    MissingBearer,
    Token(TokenError),
    UnknownProfile,
}

impl Refusal {
    fn reason(&self) -> &'static str {
        match self {
            Self::MissingBearer => "missing-bearer",
            Self::Token(error) => error.reason(),
            Self::UnknownProfile => "unknown-profile",
        }
    }
}

/// Verify the request's bearer token and resolve its profile.
fn authenticate(auth: &Auth, headers: &HeaderMap) -> Result<TaskContext, Refusal> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(Refusal::MissingBearer)?;
    let (scheme, token) = value.split_once(' ').ok_or(Refusal::MissingBearer)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Refusal::MissingBearer);
    }
    let claims = auth
        .tokens
        .verify_now(token.trim())
        .map_err(Refusal::Token)?;
    let profile = auth
        .registry
        .get(&claims.profile)
        .ok_or(Refusal::UnknownProfile)?;
    Ok(TaskContext { claims, profile })
}

/// The bearer middleware: a verified task goes on to MCP; anything else is a 401 with the
/// `WWW-Authenticate` challenge, logged without claims.
async fn bearer(State(auth): State<Auth>, mut request: Request, next: Next) -> Response {
    match authenticate(&auth, request.headers()) {
        Ok(task) => {
            request.extensions_mut().insert(task);
            next.run(request).await
        }
        Err(refusal) => {
            eprintln!(
                "tool-gateway refused status=401 reason={}",
                refusal.reason()
            );
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, WWW_AUTHENTICATE)],
            )
                .into_response()
        }
    }
}

/// The gateway router: the MCP service at `/mcp` and the skill routes at `/skills`, behind
/// the bearer check, serving the profiles in `registry` with tokens verified by `tokens`.
/// Never merged into `/api`.
pub fn gateway_router(
    registry: Arc<ProfileRegistry>,
    tokens: Arc<TaskTokens>,
    config: GatewayConfig,
) -> Router {
    let ledger = config.run_ledger.clone();
    let gateway = ToolGateway {
        world: registry.world(),
        scopes: Arc::new(ScopeCache::new(config.scope_cache_capacity)),
        log: config.call_log,
        ledger: config.run_ledger,
        text_bound: config.text_bound,
    };
    // Stateless (every request authenticates itself, so no session store) and plain JSON
    // (tools do not stream). `Host` is checked against the admitted names.
    let mcp = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(config.allowed_hosts);
    let service = StreamableHttpService::new(
        move || Ok(gateway.clone()),
        Arc::new(NeverSessionManager::default()),
        mcp,
    );
    // The skill routes sit outside the SDK's `Host` check, but behind the same bearer check,
    // and serve only the token's pinned, read-only versions.
    Router::new()
        .route_service(MCP_PATH, service)
        .merge(skill_routes::router(
            config.skill_source,
            config.skill_log,
            ledger,
        ))
        .layer(middleware::from_fn_with_state(
            Auth { registry, tokens },
            bearer,
        ))
}
