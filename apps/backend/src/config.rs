//! Ingestion configuration, loaded from the environment (fail loud).
//!
//! Secrets and scope are configuration, not code: the webhook secret, Odoo credentials,
//! and the model allowlist all arrive here. The allowlist is the capability's concern;
//! the endpoint and worker carry no baked-in vocabulary.

use std::collections::BTreeMap;

use ekg_domain::{PolicyId, ReadPolicy};

/// Which transport carries the `RdfChanged` fan-out backbone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventBus {
    /// A single-process `tokio::broadcast` fan-out. The default: needs no broker, and is
    /// what `just check` and the unit tests run on.
    InProcess,
    /// Durable NATS JetStream: replayable and cross-process. The local stack runs on it.
    Nats,
}

/// Everything the ingestion surface needs to authenticate, pull, and route.
#[derive(Debug, Clone)]
pub struct IngestionConfig {
    /// Shared secret every webhook notification must present (constant-time compared).
    pub webhook_secret: String,
    /// Odoo base URL, e.g. `http://localhost:8069`.
    pub odoo_url: String,
    /// Odoo database name.
    pub odoo_db: String,
    /// Odoo login.
    pub odoo_user: String,
    /// Odoo password or API key.
    pub odoo_api_key: String,
    /// Source models the ingestor accepts, pulls and sweeps.
    pub allowlist: Vec<String>,
    /// Action types a human-authored policy pre-authorizes to auto-promote. The default
    /// is empty, which means fully human-gated. A reversible allowlisted verb still commits
    /// only via a `PromotedAction`, and the reversibility guard in the policy engine bounds
    /// what may auto-promote.
    pub auto_promote_actions: Vec<String>,
    /// The auto-apply cost limit: a plan whose touched order-line cost exceeds this is
    /// human-gated. `None` (unset) is fail-closed: a cost-bearing plan is never
    /// auto-promoted without an explicit limit.
    pub auto_promote_cost_limit: Option<f64>,
    /// The currency the cost limit is expressed in (ISO 4217), as the policy read serves it.
    /// Defaults to the seed's currency, `USD`.
    pub auto_promote_cost_limit_currency: String,
    /// The purchase-order lifecycle states the supplier-confirmed predicate treats as
    /// confirmed (default `purchase,done`).
    pub auto_promote_confirmed_states: Vec<String>,
    /// The maximum number of actions a produced plan may carry. The bounded agent rejects
    /// a longer plan instead of truncating it, so a plan stays reviewable in one oversight
    /// decision (Art. 14) and the generation loop terminates. Defaults to
    /// [`ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS`].
    pub max_plan_actions: usize,
    /// Read-policy: the policy id stamped on every recorded omission.
    pub read_policy_id: String,
    /// Roles that bypass the read-policy (see the full grounding, no omissions).
    pub read_policy_privileged_roles: Vec<String>,
    /// Sensitive edge predicate local-names hidden from a non-privileged reader.
    pub read_policy_sensitive_edges: Vec<String>,
    /// Sensitive node field local-names hidden from a non-privileged reader.
    pub read_policy_sensitive_nodes: Vec<String>,
    /// Which event backbone carries the `RdfChanged` fan-out. Defaults to
    /// [`EventBus::InProcess`] so `just check` and the unit tests need no broker.
    pub event_bus: EventBus,
    /// NATS client URL used when [`Self::event_bus`] is [`EventBus::Nats`].
    pub nats_url: String,
    /// Whether to run the non-authoritative vector projection consumer. Default `false`
    /// so no Qdrant is needed for `just check`.
    pub vector_projection: bool,
    /// Qdrant base URL used when [`Self::vector_projection`] is on.
    pub qdrant_url: String,
    /// Qdrant collection the projection upserts into (width = [`Self::cognition_embed_dim`]).
    pub qdrant_collection: String,
    /// Restate ingress base URL the endpoint submits durable work to.
    pub restate_ingress_url: String,
    /// Restate admin base URL (introspection `/query`), used to list action points.
    pub restate_admin_url: String,
    /// GraphDB base URL the durable worker materializes into, e.g. `http://localhost:7200`.
    pub graphdb_url: String,
    /// GraphDB repository id the world model lives in.
    pub graphdb_repo: String,
    /// Reconciliation sweep interval, in seconds.
    pub sweep_interval_secs: u64,
    /// Whether the backend best-effort triggers the first sweep pass on startup.
    /// Defaults on; set `false` for tests and multi-instance deployments.
    pub sweep_bootstrap: bool,
    /// How many what-if projections the process runs at once, so the store's repository
    /// cap is never exceeded. Defaults to
    /// [`ekg_semantic::simulator::DEFAULT_MAX_PROJECTIONS`].
    pub sim_max_projections: std::num::NonZeroUsize,
    /// How old a what-if projection's scratch repository must be before the startup sweep
    /// reclaims it as left behind by an earlier process (`EKG_SIM_ORPHAN_MAX_AGE_SECS`).
    /// It must stay above the longest time a projection is held: a younger one may be
    /// another process's live projection. Defaults to [`DEFAULT_SIM_ORPHAN_MAX_AGE`].
    pub sim_orphan_max_age: std::time::Duration,
    /// How cognition reaches the provider (`EKG_COGNITION_ROUTE`): through the served
    /// runtime at `HAYHOOKS_URL` (the default) or directly.
    pub cognition_route: ekg_cognition::CognitionRoute,
    /// Base URL of the OpenAI-compatible cognition endpoint on the direct route.
    pub cognition_base_url: String,
    /// Optional bearer token for the cognition endpoint (`None` when unset/empty).
    pub cognition_api_key: Option<String>,
    /// Chat/completions model the bounded agent generates with.
    pub cognition_chat_model: String,
    /// Embeddings model name.
    pub cognition_embed_model: String,
    /// Pinned embedding width; a response of a different width is rejected.
    pub cognition_embed_dim: usize,
    /// The cognition boundary's timeout, retry and concurrency bounds, each from an
    /// `EKG_COGNITION_*` variable.
    pub cognition_limits: ekg_cognition::CognitionLimits,
    /// The slate loop's time budget for generating candidates (`EKG_SLATE_BUDGET_SECS`).
    /// Defaults to [`ekg_cognition::DEFAULT_SLATE_BUDGET`].
    pub slate_budget: std::time::Duration,
    /// How long a governed command's claim on an action point holds before the next
    /// command may take it over (`EKG_COMMAND_CLAIM_SECS`). It must exceed the longest
    /// bounded write-back (a plan's actions times the source request timeout, plus the
    /// rollback). Defaults to [`DEFAULT_COMMAND_CLAIM`].
    pub command_claim: std::time::Duration,
    /// The agent tool gateway's internal listener port (`EKG_TOOL_GATEWAY_PORT`), default
    /// [`DEFAULT_TOOL_GATEWAY_PORT`]. Never part of `/api`.
    pub tool_gateway_port: u16,
    /// The task-token signing secret (`EKG_TOOL_TOKEN_SECRET`): at least
    /// [`ekg_cognition::tools::MIN_SECRET_LEN`] bytes when set. `None` (unset or blank)
    /// signs with a random per-process secret. That is enough for one engine that mints and
    /// verifies; several replicas, or host tests minting tokens, must share one.
    pub tool_token_secret: Option<String>,
    /// How many shadow agent slates may run at once (`EKG_AGENT_SHADOW_CONCURRENCY`),
    /// default 1. This is shadow's own limit, apart from the legacy path's generation
    /// permits.
    pub agent_shadow_concurrency: std::num::NonZeroUsize,
    /// The step limit of one agent run (`EKG_AGENT_MAX_STEPS`), default
    /// [`ekg_cognition::DEFAULT_MAX_AGENT_STEPS`].
    pub agent_max_steps: u32,
    /// The step limit of a portfolio's planner run (`EKG_AGENT_JOINT_MAX_STEPS`), default
    /// [`ekg_cognition::DEFAULT_MAX_JOINT_AGENT_STEPS`].
    pub agent_joint_max_steps: u32,
    /// The cap on a portfolio's planner run (`EKG_AGENT_JOINT_RUN_SECS`), default
    /// [`ekg_cognition::DEFAULT_JOINT_RUN_CAP`]. Never longer than a task token can outlive.
    pub agent_joint_run_cap: std::time::Duration,
    /// The step limit of an analyst run (`EKG_AGENT_ANALYST_MAX_STEPS`), default
    /// [`ekg_cognition::DEFAULT_MAX_ANALYST_STEPS`].
    pub agent_analyst_max_steps: u32,
    /// The cap on an analyst run (`EKG_AGENT_ANALYST_RUN_SECS`), default
    /// [`ekg_cognition::DEFAULT_ANALYST_RUN_CAP`]. Never longer than a task token can outlive.
    pub agent_analyst_run_cap: std::time::Duration,
    /// The planners of a round of a portfolio's slate (`EKG_AGENT_PLANNERS`), default 3, at most
    /// the number of strategies.
    pub agent_planners: usize,
    /// Whether a portfolio's planners get read tools (`EKG_AGENT_PLANNER_READS`).
    pub agent_planner_reads: bool,
    /// The most characters of a record's text a gateway tool hands to an agent run
    /// (`EKG_AGENT_TEXT_MAX_CHARS`), default [`ekg_cognition::tools::DEFAULT_TEXT_BOUND`]. A
    /// longer text is withheld.
    pub agent_text_max_chars: usize,
}

/// The agent tool gateway's default listener port.
pub const DEFAULT_TOOL_GATEWAY_PORT: u16 = 8091;

/// The default claim deadline of a governed command: above a 4-action plan at 60 s per
/// source request plus its rollback.
pub const DEFAULT_COMMAND_CLAIM: std::time::Duration = std::time::Duration::from_secs(600);

/// The default age past which the startup sweep reclaims a what-if projection's scratch
/// repository: far above the slate budget and its guard, the longest a projection is held.
pub const DEFAULT_SIM_ORPHAN_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(900);

impl IngestionConfig {
    /// Load from process environment.
    ///
    /// # Errors
    /// Returns the name of the first required-but-missing variable.
    pub fn from_env() -> Result<Self, String> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        Self::from_map(&vars)
    }

    /// Load from an explicit variable map (the testable core of [`Self::from_env`]).
    ///
    /// # Errors
    /// Returns the name of the first required-but-missing variable. `EKG_WEBHOOK_SECRET`
    /// and the Odoo credentials are required; the rest have sensible local defaults.
    // One flat table of settings; splitting it would only scatter them.
    #[allow(clippy::too_many_lines)]
    pub fn from_map(vars: &BTreeMap<String, String>) -> Result<Self, String> {
        let required = |key: &str| vars.get(key).cloned().ok_or_else(|| key.to_owned());
        let optional =
            |key: &str, default: &str| vars.get(key).cloned().unwrap_or_else(|| default.to_owned());
        Ok(Self {
            webhook_secret: required("EKG_WEBHOOK_SECRET")?,
            odoo_url: optional("ODOO_URL", "http://localhost:8069"),
            odoo_db: optional("ODOO_DB", "ekg"),
            odoo_user: optional("ODOO_USER", "admin"),
            odoo_api_key: required("ODOO_API_KEY")?,
            allowlist: optional(
                "EKG_INGEST_ALLOWLIST",
                "sale.order,sale.order.line,product.product,mrp.bom,mrp.bom.line,\
                 res.partner,product.supplierinfo,purchase.order,purchase.order.line,\
                 stock.picking,stock.quant,stock.warehouse.orderpoint,\
                 mrp.production,mrp.workorder,mrp.workcenter,mrp.routing.workcenter,\
                 mrp.workcenter.capacity,res.currency",
            )
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
            // Default empty: no verb auto-promotes unless a human opts it in.
            auto_promote_actions: optional("EKG_AUTOPROMOTE_ACTIONS", "")
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            // Fail closed: unset or blank means no limit, and then a cost-bearing plan is
            // human-gated.
            auto_promote_cost_limit: {
                let raw = optional("EKG_AUTOPROMOTE_COST_LIMIT", "");
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.parse::<f64>().map_err(|_| {
                        format!("EKG_AUTOPROMOTE_COST_LIMIT is not a number: {trimmed:?}")
                    })?)
                }
            },
            auto_promote_cost_limit_currency: optional(
                "EKG_AUTOPROMOTE_COST_LIMIT_CURRENCY",
                "USD",
            )
            .trim()
            .to_owned(),
            auto_promote_confirmed_states: optional(
                "EKG_AUTOPROMOTE_CONFIRMED_STATES",
                "purchase,done",
            )
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
            max_plan_actions: parse_max_plan_actions(&optional("EKG_MAX_PLAN_ACTIONS", ""))?,
            // Read-policy defaults reproduce the demo: a non-`procurement` reader has
            // `unit_price`/`supplier` hidden and recorded as omissions.
            read_policy_id: optional("EKG_READ_POLICY_ID", "read-policy/demo-v1"),
            read_policy_privileged_roles: optional(
                "EKG_READ_POLICY_PRIVILEGED_ROLES",
                "procurement",
            )
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
            read_policy_sensitive_edges: optional("EKG_READ_POLICY_SENSITIVE_EDGES", "unit_price")
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            read_policy_sensitive_nodes: optional("EKG_READ_POLICY_SENSITIVE_NODES", "supplier")
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            // In-process by default (no broker). An unknown value is an error.
            event_bus: match optional("EKG_EVENT_BUS", "in_process").as_str() {
                "in_process" => EventBus::InProcess,
                "nats" => EventBus::Nats,
                _ => return Err("EKG_EVENT_BUS".to_owned()),
            },
            nats_url: optional("NATS_URL", "nats://localhost:4222"),
            // Off by default (no Qdrant needed). An unknown value is an error.
            vector_projection: match optional("EKG_VECTOR_PROJECTION", "off").as_str() {
                "on" => true,
                "off" => false,
                _ => return Err("EKG_VECTOR_PROJECTION".to_owned()),
            },
            qdrant_url: optional("QDRANT_URL", "http://localhost:6333"),
            qdrant_collection: optional("QDRANT_COLLECTION", "ekg_rdf"),
            restate_ingress_url: optional("RESTATE_INGRESS_URL", "http://localhost:8080"),
            restate_admin_url: optional("RESTATE_ADMIN_URL", "http://localhost:9070"),
            graphdb_url: optional("GRAPHDB_URL", "http://localhost:7200"),
            graphdb_repo: optional("GRAPHDB_REPO", "ekg"),
            sweep_interval_secs: optional("EKG_SWEEP_INTERVAL_SECS", "300")
                .parse()
                .map_err(|_| "EKG_SWEEP_INTERVAL_SECS".to_owned())?,
            sweep_bootstrap: optional("EKG_SWEEP_BOOTSTRAP", "true")
                .parse()
                .map_err(|_| "EKG_SWEEP_BOOTSTRAP".to_owned())?,
            sim_max_projections: parse_sim_max_projections(&optional(
                "EKG_SIM_MAX_PROJECTIONS",
                "",
            ))?,
            sim_orphan_max_age: std::time::Duration::from_secs(parse_positive(
                "EKG_SIM_ORPHAN_MAX_AGE_SECS",
                &optional("EKG_SIM_ORPHAN_MAX_AGE_SECS", ""),
                DEFAULT_SIM_ORPHAN_MAX_AGE.as_secs(),
            )?),
            cognition_route: parse_cognition_route(
                &optional("EKG_COGNITION_ROUTE", "runtime"),
                &optional("HAYHOOKS_URL", "http://localhost:1416"),
            )?,
            // The same variables the provider-neutral model boundary reads.
            cognition_base_url: optional("MODEL_BASE_URL", "https://openrouter.ai/api/v1"),
            cognition_api_key: vars.get("MODEL_API_KEY").filter(|s| !s.is_empty()).cloned(),
            cognition_chat_model: optional("CHAT_MODEL", "openai/gpt-oss-120b"),
            cognition_embed_model: optional("EMBED_MODEL", "qwen/qwen3-embedding-8b"),
            cognition_embed_dim: optional("EMBED_DIM", "4096")
                .parse()
                .map_err(|_| "EMBED_DIM".to_owned())?,
            cognition_limits: parse_cognition_limits(&optional)?,
            slate_budget: std::time::Duration::from_secs(parse_positive(
                "EKG_SLATE_BUDGET_SECS",
                &optional("EKG_SLATE_BUDGET_SECS", ""),
                ekg_cognition::DEFAULT_SLATE_BUDGET.as_secs(),
            )?),
            command_claim: std::time::Duration::from_secs(parse_positive(
                "EKG_COMMAND_CLAIM_SECS",
                &optional("EKG_COMMAND_CLAIM_SECS", ""),
                DEFAULT_COMMAND_CLAIM.as_secs(),
            )?),
            tool_gateway_port: {
                let raw = optional("EKG_TOOL_GATEWAY_PORT", "");
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    DEFAULT_TOOL_GATEWAY_PORT
                } else {
                    trimmed
                        .parse()
                        .map_err(|_| "EKG_TOOL_GATEWAY_PORT".to_owned())?
                }
            },
            tool_token_secret: parse_tool_token_secret(vars.get("EKG_TOOL_TOKEN_SECRET"))?,
            agent_shadow_concurrency: parse_positive(
                "EKG_AGENT_SHADOW_CONCURRENCY",
                &optional("EKG_AGENT_SHADOW_CONCURRENCY", ""),
                1_usize,
            )
            .map(|n| std::num::NonZeroUsize::new(n).unwrap_or(std::num::NonZeroUsize::MIN))?,
            agent_max_steps: parse_positive(
                "EKG_AGENT_MAX_STEPS",
                &optional("EKG_AGENT_MAX_STEPS", ""),
                ekg_cognition::DEFAULT_MAX_AGENT_STEPS,
            )?,
            agent_joint_max_steps: parse_positive(
                "EKG_AGENT_JOINT_MAX_STEPS",
                &optional("EKG_AGENT_JOINT_MAX_STEPS", ""),
                ekg_cognition::DEFAULT_MAX_JOINT_AGENT_STEPS,
            )?,
            agent_joint_run_cap: parse_run_cap(
                "EKG_AGENT_JOINT_RUN_SECS",
                &optional("EKG_AGENT_JOINT_RUN_SECS", ""),
                ekg_cognition::DEFAULT_JOINT_RUN_CAP,
            )?,
            agent_analyst_max_steps: parse_positive(
                "EKG_AGENT_ANALYST_MAX_STEPS",
                &optional("EKG_AGENT_ANALYST_MAX_STEPS", ""),
                ekg_cognition::DEFAULT_MAX_ANALYST_STEPS,
            )?,
            agent_analyst_run_cap: parse_run_cap(
                "EKG_AGENT_ANALYST_RUN_SECS",
                &optional("EKG_AGENT_ANALYST_RUN_SECS", ""),
                ekg_cognition::DEFAULT_ANALYST_RUN_CAP,
            )?,
            agent_planners: parse_planners(&optional("EKG_AGENT_PLANNERS", ""))?,
            agent_planner_reads: optional("EKG_AGENT_PLANNER_READS", "") == "true",
            agent_text_max_chars: parse_text_max_chars(&optional("EKG_AGENT_TEXT_MAX_CHARS", ""))?,
        })
    }

    /// The limits of an agent run for one point: its step limit, and the cap on one model
    /// call as its run cap.
    #[must_use]
    pub fn agent_limits(&self) -> ekg_cognition::AgentLimits {
        ekg_cognition::AgentLimits {
            max_steps: self.agent_max_steps,
            run_cap: self.cognition_limits.total_timeout,
        }
    }

    /// The limits of a portfolio's planner run.
    #[must_use]
    pub fn agent_joint_limits(&self) -> ekg_cognition::AgentLimits {
        ekg_cognition::AgentLimits {
            max_steps: self.agent_joint_max_steps,
            run_cap: self.agent_joint_run_cap,
        }
    }

    /// The limits of an analyst run.
    #[must_use]
    pub fn agent_analyst_limits(&self) -> ekg_cognition::AgentLimits {
        ekg_cognition::AgentLimits {
            max_steps: self.agent_analyst_max_steps,
            run_cap: self.agent_analyst_run_cap,
        }
    }

    /// Whether `model` is within the ingestion allowlist.
    #[must_use]
    pub fn allows(&self, model: &str) -> bool {
        self.allowlist.iter().any(|m| m == model)
    }

    /// Build the role and purpose read-policy from config. An edge's reported omission type
    /// is the predicate as-is (`unit_price` gives `ekg:unit_price`); a node's is the
    /// PascalCase of the field (`supplier` gives `ekg:Supplier`), following the ontology's
    /// node-type convention. Defaults reproduce the demo policy.
    #[must_use]
    pub fn read_policy(&self) -> ReadPolicy {
        let edges = self
            .read_policy_sensitive_edges
            .iter()
            .map(|e| (e.clone(), format!("ekg:{e}")))
            .collect();
        let nodes = self
            .read_policy_sensitive_nodes
            .iter()
            .map(|n| (n.clone(), format!("ekg:{}", pascal_case(n))))
            .collect();
        ReadPolicy::new(
            PolicyId(self.read_policy_id.clone()),
            self.read_policy_privileged_roles.iter().cloned().collect(),
            edges,
            nodes,
        )
    }
}

/// Parse the per-plan action bound: a blank value falls back to the cognition default; a
/// set value must be a positive integer or it fails loud. `0` is rejected, not clamped: a
/// bound of zero would forbid every plan.
fn parse_max_plan_actions(raw: &str) -> Result<usize, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS);
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("EKG_MAX_PLAN_ACTIONS".to_owned()),
    }
}

/// Parse `EKG_SIM_MAX_PROJECTIONS`: empty gives the simulator's default; otherwise it must
/// be a positive integer, else the error is the variable name.
fn parse_sim_max_projections(raw: &str) -> Result<std::num::NonZeroUsize, String> {
    let trimmed = raw.trim();
    let default = ekg_semantic::simulator::DEFAULT_MAX_PROJECTIONS;
    let parsed = if trimmed.is_empty() {
        Some(default)
    } else {
        trimmed.parse::<usize>().ok()
    };
    parsed
        .and_then(std::num::NonZeroUsize::new)
        .ok_or_else(|| "EKG_SIM_MAX_PROJECTIONS".to_owned())
}

/// Parse a positive setting: empty gives `default`; otherwise it must be a positive
/// integer, else the error is the variable name. `0` is rejected, not clamped.
fn parse_positive<T>(name: &str, raw: &str, default: T) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + Default,
{
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    match trimmed.parse::<T>() {
        Ok(n) if n > T::default() => Ok(n),
        _ => Err(name.to_owned()),
    }
}

/// Parse `EKG_AGENT_PLANNERS`: one planner per strategy at most.
fn parse_planners(raw: &str) -> Result<usize, String> {
    const NAME: &str = "EKG_AGENT_PLANNERS";
    let most = crate::propose::PLANNER_STRATEGIES.len();
    let planners = parse_positive(NAME, raw, most)?;
    if planners > most {
        return Err(format!("{NAME}: at most {most}, one per strategy"));
    }
    Ok(planners)
}

/// Parse `EKG_AGENT_JOINT_RUN_SECS`: a positive number of seconds, at most what a task token
/// can outlive (the token is minted once, for the whole run).
fn parse_run_cap(
    name: &str,
    raw: &str,
    default: std::time::Duration,
) -> Result<std::time::Duration, String> {
    let secs = parse_positive(name, raw, default.as_secs())?;
    let longest = ekg_cognition::AgentLimits::longest_run_cap().as_secs();
    if secs > longest {
        return Err(format!(
            "{name}: at most {longest} seconds, so the run's task token outlives it"
        ));
    }
    Ok(std::time::Duration::from_secs(secs))
}

/// Parse `EKG_AGENT_TEXT_MAX_CHARS`: at least as long as a date with its time, so no typed
/// fact is ever withheld.
fn parse_text_max_chars(raw: &str) -> Result<usize, String> {
    const NAME: &str = "EKG_AGENT_TEXT_MAX_CHARS";
    const SHORTEST: usize = 20;
    let chars = parse_positive(NAME, raw, ekg_cognition::tools::DEFAULT_TEXT_BOUND)?;
    if chars < SHORTEST {
        return Err(format!(
            "{NAME}: at least {SHORTEST} characters, so a date with its time is handed on"
        ));
    }
    Ok(chars)
}

/// Parse `EKG_COGNITION_ROUTE`: `runtime` reaches the provider through the
/// served runtime at `hayhooks_url`, `direct` calls the endpoint itself. Anything else
/// fails loud, naming the variable.
fn parse_cognition_route(
    route: &str,
    hayhooks_url: &str,
) -> Result<ekg_cognition::CognitionRoute, String> {
    match route.trim() {
        "runtime" => Ok(ekg_cognition::CognitionRoute::Runtime {
            url: hayhooks_url.to_owned(),
        }),
        "direct" => Ok(ekg_cognition::CognitionRoute::Direct),
        _ => Err("EKG_COGNITION_ROUTE".to_owned()),
    }
}

/// Parse the cognition boundary's bounds from the `EKG_COGNITION_*` variables, each
/// defaulting to [`ekg_cognition::CognitionLimits::default`] and failing loud unless
/// positive.
fn parse_cognition_limits(
    optional: &dyn Fn(&str, &str) -> String,
) -> Result<ekg_cognition::CognitionLimits, String> {
    use std::time::Duration;
    let defaults = ekg_cognition::CognitionLimits::default();
    let secs = |name: &str, default: Duration| {
        parse_positive(name, &optional(name, ""), default.as_secs()).map(Duration::from_secs)
    };
    let max_concurrent = parse_positive(
        "EKG_COGNITION_MAX_CONCURRENT",
        &optional("EKG_COGNITION_MAX_CONCURRENT", ""),
        defaults.max_concurrent.get(),
    )?;
    Ok(ekg_cognition::CognitionLimits {
        max_retries: parse_positive(
            "EKG_COGNITION_MAX_RETRIES",
            &optional("EKG_COGNITION_MAX_RETRIES", ""),
            defaults.max_retries,
        )?,
        connect_timeout: secs(
            "EKG_COGNITION_CONNECT_TIMEOUT_SECS",
            defaults.connect_timeout,
        )?,
        idle_timeout: secs("EKG_COGNITION_IDLE_TIMEOUT_SECS", defaults.idle_timeout)?,
        total_timeout: secs("EKG_COGNITION_TOTAL_TIMEOUT_SECS", defaults.total_timeout)?,
        max_concurrent: std::num::NonZeroUsize::new(max_concurrent)
            .ok_or_else(|| "EKG_COGNITION_MAX_CONCURRENT".to_owned())?,
    })
}

/// Parse `EKG_TOOL_TOKEN_SECRET`: unset or blank gives `None` (a random per-process
/// secret); otherwise it must be at least [`ekg_cognition::tools::MIN_SECRET_LEN`] bytes,
/// or startup fails naming it.
fn parse_tool_token_secret(raw: Option<&String>) -> Result<Option<String>, String> {
    let Some(secret) = raw.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if secret.len() < ekg_cognition::tools::MIN_SECRET_LEN {
        return Err(format!(
            "EKG_TOOL_TOKEN_SECRET must be at least {} bytes (it is {})",
            ekg_cognition::tools::MIN_SECRET_LEN,
            secret.len()
        ));
    }
    Ok(Some(secret.to_owned()))
}

/// Uppercase the first character (`supplier` gives `Supplier`) and leave the rest as-is:
/// the ontology's node-type naming convention.
fn pascal_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn base() -> BTreeMap<String, String> {
        [("EKG_WEBHOOK_SECRET", "s3cret"), ("ODOO_API_KEY", "admin")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn loads_with_required_vars_and_defaults() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.webhook_secret, "s3cret");
        assert_eq!(cfg.odoo_url, "http://localhost:8069");
        assert!(cfg.allows("sale.order"));
        // Manufacturing-process models are in the default ingestion scope.
        assert!(cfg.allows("mrp.production"));
        assert!(cfg.allows("mrp.workcenter"));
        assert!(cfg.allows("mrp.workcenter.capacity"));
        // The currency an order's money is recorded in.
        assert!(cfg.allows("res.currency"));
        assert!(!cfg.allows("account.move"));
        assert_eq!(cfg.graphdb_url, "http://localhost:7200");
        assert_eq!(cfg.graphdb_repo, "ekg");
        assert_eq!(cfg.sweep_interval_secs, 300);
        assert!(cfg.sweep_bootstrap, "the startup sweep trigger defaults on");
        assert!(
            cfg.auto_promote_actions.is_empty(),
            "unset auto-promote allowlist is empty (fully human-gated)"
        );
        assert_eq!(
            cfg.auto_promote_cost_limit, None,
            "unset cost limit is fail-closed (no auto-apply of a cost-bearing plan)"
        );
        assert_eq!(
            cfg.auto_promote_cost_limit_currency, "USD",
            "the limit's currency defaults to the seed's"
        );
        assert_eq!(
            cfg.auto_promote_confirmed_states,
            vec!["purchase".to_owned(), "done".to_owned()],
            "confirmed states default to purchase/done"
        );
    }

    #[test]
    fn the_tool_gateway_defaults_to_port_8091_and_a_per_process_secret() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.tool_gateway_port, 8091);
        assert_eq!(cfg.tool_token_secret, None, "unset secret is accepted");
        let mut vars = base();
        vars.insert("EKG_TOOL_TOKEN_SECRET".to_owned(), "  ".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(
            cfg.tool_token_secret, None,
            "a blank secret counts as unset"
        );
    }

    #[test]
    fn the_tool_gateway_port_and_secret_parse_when_set() {
        let mut vars = base();
        vars.insert("EKG_TOOL_GATEWAY_PORT".to_owned(), "9191".to_owned());
        let secret = "ab".repeat(32);
        vars.insert("EKG_TOOL_TOKEN_SECRET".to_owned(), secret.clone());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.tool_gateway_port, 9191);
        assert_eq!(cfg.tool_token_secret, Some(secret));
        vars.insert("EKG_TOOL_GATEWAY_PORT".to_owned(), "port".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).err().as_deref(),
            Some("EKG_TOOL_GATEWAY_PORT")
        );
    }

    #[test]
    fn a_short_tool_token_secret_fails_naming_it() {
        let mut vars = base();
        vars.insert("EKG_TOOL_TOKEN_SECRET".to_owned(), "too-short".to_owned());
        let err = IngestionConfig::from_map(&vars).expect_err("refused");
        assert!(err.contains("EKG_TOOL_TOKEN_SECRET"), "{err}");
        assert!(err.contains("32"), "{err}");
    }

    #[test]
    fn max_plan_actions_defaults_to_the_cognition_default() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(
            cfg.max_plan_actions,
            ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS,
            "unset per-plan bound falls back to the cognition default (4)"
        );
    }

    #[test]
    fn max_plan_actions_parses_a_set_value() {
        let mut vars = base();
        vars.insert("EKG_MAX_PLAN_ACTIONS".to_owned(), "6".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.max_plan_actions, 6);
    }

    #[test]
    fn a_non_numeric_max_plan_actions_fails_loud() {
        let mut vars = base();
        vars.insert("EKG_MAX_PLAN_ACTIONS".to_owned(), "lots".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).expect_err("non-numeric must fail"),
            "EKG_MAX_PLAN_ACTIONS"
        );
    }

    #[test]
    fn a_zero_max_plan_actions_fails_loud() {
        // A bound of zero would forbid every plan, so `0` is rejected, not clamped.
        let mut vars = base();
        vars.insert("EKG_MAX_PLAN_ACTIONS".to_owned(), "0".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).expect_err("zero must fail"),
            "EKG_MAX_PLAN_ACTIONS"
        );
    }

    #[test]
    fn auto_promote_cost_limit_and_confirmed_states_parse() {
        let mut vars = base();
        vars.insert("EKG_AUTOPROMOTE_COST_LIMIT".to_owned(), "250".to_owned());
        vars.insert(
            "EKG_AUTOPROMOTE_COST_LIMIT_CURRENCY".to_owned(),
            " EUR ".to_owned(),
        );
        vars.insert(
            "EKG_AUTOPROMOTE_CONFIRMED_STATES".to_owned(),
            "purchase , done , received".to_owned(),
        );
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.auto_promote_cost_limit, Some(250.0));
        assert_eq!(cfg.auto_promote_cost_limit_currency, "EUR");
        assert_eq!(
            cfg.auto_promote_confirmed_states,
            vec![
                "purchase".to_owned(),
                "done".to_owned(),
                "received".to_owned()
            ]
        );
    }

    #[test]
    fn a_non_numeric_cost_limit_is_rejected() {
        let mut vars = base();
        vars.insert("EKG_AUTOPROMOTE_COST_LIMIT".to_owned(), "cheap".to_owned());
        assert!(IngestionConfig::from_map(&vars).is_err());
    }

    #[test]
    fn event_bus_defaults_to_in_process_with_the_local_nats_url() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(
            cfg.event_bus,
            EventBus::InProcess,
            "no broker is required by default"
        );
        assert_eq!(cfg.nats_url, "nats://localhost:4222");
    }

    #[test]
    fn event_bus_nats_parses() {
        let mut vars = base();
        vars.insert("EKG_EVENT_BUS".to_owned(), "nats".to_owned());
        vars.insert("NATS_URL".to_owned(), "nats://broker:4222".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.event_bus, EventBus::Nats);
        assert_eq!(cfg.nats_url, "nats://broker:4222");
    }

    #[test]
    fn unknown_event_bus_fails_loud() {
        let mut vars = base();
        vars.insert("EKG_EVENT_BUS".to_owned(), "kafka".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).expect_err("unknown bus must fail"),
            "EKG_EVENT_BUS"
        );
    }

    #[test]
    fn vector_projection_defaults_off_with_local_qdrant() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert!(!cfg.vector_projection, "no Qdrant is required by default");
        assert_eq!(cfg.qdrant_url, "http://localhost:6333");
        assert_eq!(cfg.qdrant_collection, "ekg_rdf");
    }

    #[test]
    fn vector_projection_on_parses() {
        let mut vars = base();
        vars.insert("EKG_VECTOR_PROJECTION".to_owned(), "on".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert!(cfg.vector_projection);
    }

    #[test]
    fn unknown_vector_projection_fails_loud() {
        let mut vars = base();
        vars.insert("EKG_VECTOR_PROJECTION".to_owned(), "maybe".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).expect_err("unknown toggle must fail"),
            "EKG_VECTOR_PROJECTION"
        );
    }

    #[test]
    fn read_policy_defaults_reproduce_the_demo_policy() {
        use ekg_domain::{OmissionKind, ReadContext, Visibility};
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.read_policy_id, "read-policy/demo-v1");
        let policy = cfg.read_policy();
        let planner = ReadContext {
            role: "planner".to_owned(),
            purpose: "delivery-promise".to_owned(),
        };
        // A non-privileged reader has the sensitive edge and node hidden under the demo
        // type IRIs; the privileged procurement role sees everything.
        assert_eq!(
            policy.classify(&planner, "unit_price"),
            Visibility::Hidden {
                kind: OmissionKind::Edge,
                type_iri: "ekg:unit_price".to_owned()
            }
        );
        assert_eq!(
            policy.classify(&planner, "supplier"),
            Visibility::Hidden {
                kind: OmissionKind::Node,
                type_iri: "ekg:Supplier".to_owned()
            }
        );
        let procurement = ReadContext {
            role: "procurement".to_owned(),
            purpose: "sourcing".to_owned(),
        };
        assert_eq!(
            policy.classify(&procurement, "unit_price"),
            Visibility::Visible
        );
    }

    #[test]
    fn read_policy_lists_parse_and_trim_from_csv() {
        let mut vars = base();
        vars.insert(
            "EKG_READ_POLICY_PRIVILEGED_ROLES".to_owned(),
            " procurement , finance ".to_owned(),
        );
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(
            cfg.read_policy_privileged_roles,
            vec!["procurement".to_owned(), "finance".to_owned()]
        );
    }

    #[test]
    fn auto_promote_actions_parse_and_trim_from_a_csv() {
        let mut vars = base();
        vars.insert(
            "EKG_AUTOPROMOTE_ACTIONS".to_owned(),
            " notify_owner , re_promise_date ".to_owned(),
        );
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(
            cfg.auto_promote_actions,
            vec!["notify_owner".to_owned(), "re_promise_date".to_owned()]
        );
    }

    #[test]
    fn sweep_bootstrap_can_be_disabled() {
        let mut vars = base();
        vars.insert("EKG_SWEEP_BOOTSTRAP".to_owned(), "false".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert!(
            !cfg.sweep_bootstrap,
            "an explicit false opts out of the trigger"
        );
    }

    #[test]
    fn the_projection_limit_defaults_and_is_configurable() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(
            cfg.sim_max_projections.get(),
            ekg_semantic::simulator::DEFAULT_MAX_PROJECTIONS
        );
        let mut vars = base();
        vars.insert("EKG_SIM_MAX_PROJECTIONS".to_owned(), "3".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.sim_max_projections.get(), 3);
        for bad in ["0", "-1", "lots"] {
            vars.insert("EKG_SIM_MAX_PROJECTIONS".to_owned(), bad.to_owned());
            assert_eq!(
                IngestionConfig::from_map(&vars).expect_err("must fail loud"),
                "EKG_SIM_MAX_PROJECTIONS",
                "{bad}"
            );
        }
    }

    #[test]
    fn the_orphan_age_defaults_and_fails_loud_unless_positive() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.sim_orphan_max_age, std::time::Duration::from_secs(900));
        let mut vars = base();
        vars.insert("EKG_SIM_ORPHAN_MAX_AGE_SECS".to_owned(), "3600".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.sim_orphan_max_age, std::time::Duration::from_secs(3600));
        for bad in ["0", "-1", "old"] {
            vars.insert("EKG_SIM_ORPHAN_MAX_AGE_SECS".to_owned(), bad.to_owned());
            assert_eq!(
                IngestionConfig::from_map(&vars).expect_err("must fail loud"),
                "EKG_SIM_ORPHAN_MAX_AGE_SECS",
                "{bad}"
            );
        }
    }

    #[test]
    fn the_chat_model_defaults_and_an_explicit_setting_wins() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(
            cfg.cognition_chat_model, "openai/gpt-oss-120b",
            "unset CHAT_MODEL falls back to the checked-in default"
        );
        let mut vars = base();
        vars.insert(
            "CHAT_MODEL".to_owned(),
            "qwen/qwen3-30b-a3b-instruct-2507".to_owned(),
        );
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.cognition_chat_model, "qwen/qwen3-30b-a3b-instruct-2507");
    }

    #[test]
    fn the_cognition_route_defaults_to_the_runtime_and_refuses_an_unknown_value() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(
            cfg.cognition_route,
            ekg_cognition::CognitionRoute::Runtime {
                url: "http://localhost:1416".to_owned()
            },
            "unset EKG_COGNITION_ROUTE goes through the runtime at the default URL"
        );

        let mut vars = base();
        vars.insert("HAYHOOKS_URL".to_owned(), "http://hayhooks:1416".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars)
                .expect("loads")
                .cognition_route,
            ekg_cognition::CognitionRoute::Runtime {
                url: "http://hayhooks:1416".to_owned()
            }
        );

        vars.insert("EKG_COGNITION_ROUTE".to_owned(), "direct".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars)
                .expect("loads")
                .cognition_route,
            ekg_cognition::CognitionRoute::Direct
        );

        vars.insert("EKG_COGNITION_ROUTE".to_owned(), "bogus".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).err().as_deref(),
            Some("EKG_COGNITION_ROUTE"),
            "an unknown route fails loud, naming the variable"
        );
    }

    #[test]
    fn the_cognition_limits_default_to_the_design_and_are_configurable() {
        use std::time::Duration;
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(
            cfg.cognition_limits,
            ekg_cognition::CognitionLimits::default()
        );
        assert_eq!(cfg.cognition_limits.max_retries, 3);
        assert_eq!(
            cfg.cognition_limits.connect_timeout,
            Duration::from_secs(30)
        );
        assert_eq!(cfg.cognition_limits.idle_timeout, Duration::from_secs(30));
        assert_eq!(cfg.cognition_limits.total_timeout, Duration::from_secs(120));
        assert_eq!(cfg.cognition_limits.max_concurrent.get(), 2);

        let mut vars = base();
        for (name, value) in [
            ("EKG_COGNITION_MAX_RETRIES", "5"),
            ("EKG_COGNITION_CONNECT_TIMEOUT_SECS", "10"),
            ("EKG_COGNITION_IDLE_TIMEOUT_SECS", "20"),
            ("EKG_COGNITION_TOTAL_TIMEOUT_SECS", "90"),
            ("EKG_COGNITION_MAX_CONCURRENT", "4"),
        ] {
            vars.insert(name.to_owned(), value.to_owned());
        }
        let limits = IngestionConfig::from_map(&vars)
            .expect("loads")
            .cognition_limits;
        assert_eq!(limits.max_retries, 5);
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.idle_timeout, Duration::from_secs(20));
        assert_eq!(limits.total_timeout, Duration::from_secs(90));
        assert_eq!(limits.max_concurrent.get(), 4);
    }

    #[test]
    fn the_slate_budget_defaults_and_fails_loud_unless_positive() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.slate_budget, std::time::Duration::from_secs(150));
        let mut vars = base();
        vars.insert("EKG_SLATE_BUDGET_SECS".to_owned(), "60".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.slate_budget, std::time::Duration::from_secs(60));
        for bad in ["0", "-1", "soon"] {
            vars.insert("EKG_SLATE_BUDGET_SECS".to_owned(), bad.to_owned());
            assert_eq!(
                IngestionConfig::from_map(&vars).expect_err("must fail loud"),
                "EKG_SLATE_BUDGET_SECS",
                "{bad}"
            );
        }
    }

    #[test]
    fn the_agent_shadow_concurrency_and_step_limit_default_and_fail_loud_unless_positive() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.agent_shadow_concurrency.get(), 1);
        assert_eq!(cfg.agent_max_steps, ekg_cognition::DEFAULT_MAX_AGENT_STEPS);
        let mut vars = base();
        vars.insert("EKG_AGENT_SHADOW_CONCURRENCY".to_owned(), "2".to_owned());
        vars.insert("EKG_AGENT_MAX_STEPS".to_owned(), "20".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.agent_shadow_concurrency.get(), 2);
        assert_eq!(cfg.agent_max_steps, 20);
        for (name, bad) in [
            ("EKG_AGENT_SHADOW_CONCURRENCY", "0"),
            ("EKG_AGENT_SHADOW_CONCURRENCY", "many"),
            ("EKG_AGENT_MAX_STEPS", "0"),
            ("EKG_AGENT_MAX_STEPS", "-3"),
        ] {
            let mut vars = base();
            vars.insert(name.to_owned(), bad.to_owned());
            assert_eq!(
                IngestionConfig::from_map(&vars).expect_err("must fail loud"),
                name,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_joint_run_limits_default_and_fail_loud_unless_positive() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.agent_joint_max_steps, 8);
        assert_eq!(cfg.agent_joint_run_cap, std::time::Duration::from_secs(120));
        assert_eq!((cfg.agent_planners, cfg.agent_planner_reads), (3, false));
        // A point's run keeps its own: the step limit and the cap on one model call.
        let point = cfg.agent_limits();
        assert_eq!(
            (point.max_steps, point.run_cap),
            (12, std::time::Duration::from_secs(120))
        );
        let joint = cfg.agent_joint_limits();
        assert_eq!(
            (joint.max_steps, joint.run_cap),
            (8, std::time::Duration::from_secs(120))
        );
        let analyst = cfg.agent_analyst_limits();
        assert_eq!(
            (analyst.max_steps, analyst.run_cap),
            (10, std::time::Duration::from_secs(240))
        );

        let mut vars = base();
        vars.insert("EKG_AGENT_JOINT_MAX_STEPS".to_owned(), "16".to_owned());
        vars.insert("EKG_AGENT_JOINT_RUN_SECS".to_owned(), "570".to_owned());
        vars.insert("EKG_AGENT_ANALYST_MAX_STEPS".to_owned(), "7".to_owned());
        vars.insert("EKG_AGENT_ANALYST_RUN_SECS".to_owned(), "90".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.agent_analyst_max_steps, 7);
        assert_eq!(
            cfg.agent_analyst_run_cap,
            std::time::Duration::from_secs(90)
        );
        assert_eq!(cfg.agent_joint_max_steps, 16);
        assert_eq!(cfg.agent_joint_run_cap, std::time::Duration::from_secs(570));
        assert_eq!(cfg.agent_max_steps, 12, "a point's is untouched");

        for name in [
            "EKG_AGENT_JOINT_MAX_STEPS",
            "EKG_AGENT_JOINT_RUN_SECS",
            "EKG_AGENT_ANALYST_MAX_STEPS",
            "EKG_AGENT_ANALYST_RUN_SECS",
        ] {
            for bad in ["0", "-3", "long"] {
                let mut vars = base();
                vars.insert(name.to_owned(), bad.to_owned());
                assert_eq!(
                    IngestionConfig::from_map(&vars).expect_err("must fail loud"),
                    name,
                    "{bad}"
                );
            }
        }
    }

    #[test]
    fn the_text_bound_of_agent_reads_defaults_to_80_and_is_never_shorter_than_a_date() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.agent_text_max_chars, 80);

        let mut vars = base();
        vars.insert("EKG_AGENT_TEXT_MAX_CHARS".to_owned(), "120".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.agent_text_max_chars, 120);

        for bad in ["0", "19", "-3", "long"] {
            let mut vars = base();
            vars.insert("EKG_AGENT_TEXT_MAX_CHARS".to_owned(), bad.to_owned());
            let why = IngestionConfig::from_map(&vars).expect_err("must fail loud");
            assert!(why.starts_with("EKG_AGENT_TEXT_MAX_CHARS"), "{bad}: {why}");
        }
    }

    #[test]
    fn a_joint_run_cap_no_task_token_outlives_is_refused() {
        let mut vars = base();
        vars.insert("EKG_AGENT_JOINT_RUN_SECS".to_owned(), "900".to_owned());
        let why = IngestionConfig::from_map(&vars).expect_err("must fail loud");
        assert!(
            why.starts_with("EKG_AGENT_JOINT_RUN_SECS") && why.contains("at most 570 seconds"),
            "{why}"
        );
    }

    #[test]
    fn the_command_claim_defaults_to_ten_minutes_and_fails_loud_unless_positive() {
        let cfg = IngestionConfig::from_map(&base()).expect("loads");
        assert_eq!(cfg.command_claim, std::time::Duration::from_secs(600));
        let mut vars = base();
        vars.insert("EKG_COMMAND_CLAIM_SECS".to_owned(), "900".to_owned());
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(cfg.command_claim, std::time::Duration::from_secs(900));
        for bad in ["0", "-5", "", "later"] {
            vars.insert("EKG_COMMAND_CLAIM_SECS".to_owned(), bad.to_owned());
            let loaded = IngestionConfig::from_map(&vars);
            if bad.is_empty() {
                assert_eq!(
                    loaded.expect("blank is the default").command_claim,
                    DEFAULT_COMMAND_CLAIM
                );
            } else {
                assert_eq!(
                    loaded.expect_err("must fail loud"),
                    "EKG_COMMAND_CLAIM_SECS",
                    "{bad}"
                );
            }
        }
    }

    #[test]
    fn a_non_positive_cognition_limit_fails_loud() {
        for name in [
            "EKG_COGNITION_MAX_RETRIES",
            "EKG_COGNITION_CONNECT_TIMEOUT_SECS",
            "EKG_COGNITION_IDLE_TIMEOUT_SECS",
            "EKG_COGNITION_TOTAL_TIMEOUT_SECS",
            "EKG_COGNITION_MAX_CONCURRENT",
        ] {
            for bad in ["0", "-1", "lots"] {
                let mut vars = base();
                vars.insert(name.to_owned(), bad.to_owned());
                assert_eq!(
                    IngestionConfig::from_map(&vars).expect_err("must fail loud"),
                    name,
                    "{name}={bad}"
                );
            }
        }
    }

    #[test]
    fn non_bool_sweep_bootstrap_fails_loud() {
        let mut vars = base();
        vars.insert("EKG_SWEEP_BOOTSTRAP".to_owned(), "maybe".to_owned());
        assert_eq!(
            IngestionConfig::from_map(&vars).expect_err("non-bool must fail"),
            "EKG_SWEEP_BOOTSTRAP"
        );
    }

    #[test]
    fn fails_loud_on_missing_required_var() {
        let mut vars = base();
        vars.remove("EKG_WEBHOOK_SECRET");
        assert_eq!(
            IngestionConfig::from_map(&vars).expect_err("missing secret must fail"),
            "EKG_WEBHOOK_SECRET"
        );
    }

    #[test]
    fn custom_allowlist_is_parsed() {
        let mut vars = base();
        vars.insert(
            "EKG_INGEST_ALLOWLIST".to_owned(),
            "sale.order, res.partner".to_owned(),
        );
        let cfg = IngestionConfig::from_map(&vars).expect("loads");
        assert_eq!(
            cfg.allowlist,
            vec!["sale.order".to_owned(), "res.partner".to_owned()]
        );
    }
}
