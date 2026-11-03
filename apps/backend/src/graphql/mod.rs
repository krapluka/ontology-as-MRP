//! The read-only GraphQL surface behind `POST /api/graphql`.
//!
//! One typed graph the operator frontend slices per view: the action-point list, a plan (a
//! point's ranked slate) with its options, changes, `needs`, opaque display metrics and
//! reasoning trace, the before/after work-center lanes, the four read-model tabs (Timeline,
//! Graph, Flow, Suppliers), the auto-applied changes list, and the active policy.
//!
//! The schema has a `Query` root only. It is built with [`EmptyMutation`], so a client cannot
//! approve, dismiss or undo through this surface; those stay on the REST command endpoints
//! (`promote`, `abort`, `undo`).
//!
//! Every read is non-authoritative and commits nothing. Resolvers read two sources: the
//! Restate durable `ActionPointService` state (plans, options, trace, needs, auto-applied)
//! and GraphDB SPARQL projections (the tabs and lanes, plus the derived display metrics).
//! Neither is a system of record.
//!
//! The types name plans, options, changes, metrics and tabs, never a capability or a metric.
//! Capability-specific values (the display metrics) ride an opaque [`model::Metric`] bag the
//! schema does not enumerate.

// GraphQL field names are camelCase by async-graphql convention (the schema is its own
// contract); the REST `/api` JSON stays snake_case. Docs name products (Restate, GraphDB).
#![allow(clippy::doc_markdown)]

pub mod model;
pub mod plans;
pub mod projections;
pub mod query;

use std::sync::Arc;
use std::time::Duration;

use async_graphql::{EmptyMutation, EmptySubscription, Schema, SchemaBuilder};
use async_graphql_axum::GraphQL;
use axum::Router;
use ekg_domain::{Simulator, WorldModel};
use ekg_dynamic::RiskRule;

use query::QueryRoot;

/// The read-only GraphQL schema: a `Query` root, no mutation, no subscription.
pub type ReadSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

/// The active promotion policy's display values, served read-only by the `policy` query and
/// used to fill an auto-applied change's satisfied criteria and reversal window.
///
/// These mirror the policy the auto-promote bridge is wired with in `main`. The read surface
/// only shows them; evaluating and enforcing the policy stays in `ekg-kinetic`.
#[derive(Debug, Clone)]
pub struct PolicyDisplay {
    /// The policy version id (e.g. `local-autopromote/v1`).
    pub policy_id: String,
    /// The auto-promote cost ceiling (a change costing more is human-gated); `None` when
    /// no limit is configured (the cost predicate then imposes no ceiling).
    pub cost_limit: Option<f64>,
    /// The currency the cost limit is expressed in (ISO 4217 code), when configured.
    pub cost_limit_currency: Option<String>,
    /// The purchase-order states that count as "supplier confirmed".
    pub confirmed_states: Vec<String>,
    /// The verbs the policy pre-authorizes for auto-apply (reversible and allowlisted).
    pub auto_apply_actions: Vec<String>,
    /// How long after apply an auto-applied change may still be reversed. Anchored to
    /// the durable `promoted_at`; an undo requested after it is refused.
    pub undo_window: Duration,
}

/// State shared by every GraphQL resolver: the two read sources plus the policy display
/// values. Injected into the schema via `.data(...)` and read in resolvers with
/// `ctx.data::<GraphqlState>()`.
#[derive(Clone)]
pub struct GraphqlState {
    /// HTTP client for the Restate ingress and admin APIs (durable-state reads).
    pub http: reqwest::Client,
    /// Restate ingress base URL (object handler calls: `record` / `slate` / `get`).
    pub ingress_url: String,
    /// Restate admin base URL (`/query` introspection for the list).
    pub admin_url: String,
    /// The governed RDF world model. The tabs and the "before" lanes read it (SPARQL).
    pub world: Arc<dyn WorldModel>,
    /// The what-if simulator. The "after" lanes and the derived display metrics project
    /// a plan onto a non-authoritative read model and read the projection.
    pub simulator: Arc<dyn Simulator>,
    /// The rule set shared with the drift monitor. A tab read evaluates it for its at-risk
    /// markers, so a tab reflects the rules' current verdict.
    pub rules: Arc<[RiskRule]>,
    /// The active policy's display values.
    pub policy: PolicyDisplay,
}

/// Build the read-only schema, injecting the shared read state.
///
/// Bounded to guard the public surface: a query deeper than [`MAX_DEPTH`] levels or
/// wider than [`MAX_COMPLEXITY`] field selections is rejected before execution.
#[must_use]
pub fn schema(state: GraphqlState) -> ReadSchema {
    builder()
        .data(state)
        .limit_depth(MAX_DEPTH)
        .limit_complexity(MAX_COMPLEXITY)
        .finish()
}

/// The read schema's SDL: the contract the frontend's committed `schema.graphql` snapshot
/// is generated from (`just frontend-contracts`) and drift-checked against. Built without
/// runtime data (resolvers only read `.data` at execute time), so no backend needs to run.
/// The depth and complexity limits bound execution, not the schema shape, so they are not
/// applied here.
#[must_use]
pub fn sdl() -> String {
    builder().finish().sdl()
}

/// The schema builder both the served [`schema`] and the exported [`sdl`] start from, so any
/// option that affects the shape (a registered type, a directive) lands in both. `schema()`
/// adds only the execution-time settings: the injected data and the depth and complexity
/// limits.
fn builder() -> SchemaBuilder<QueryRoot, EmptyMutation, EmptySubscription> {
    Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
}

/// Maximum query nesting depth accepted on the public read surface.
const MAX_DEPTH: usize = 12;
/// Maximum number of field selections accepted on the public read surface.
const MAX_COMPLEXITY: usize = 512;

/// Mount the GraphQL read surface at `/api/graphql` on the existing axum router. The
/// `async-graphql-axum` [`GraphQL`] service handles the `POST` query; there is no
/// mutation to serve and no subscription/WebSocket path.
pub fn router(state: GraphqlState) -> Router {
    Router::new().route_service("/api/graphql", GraphQL::new(schema(state)))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    // The builder and SDL the served schema and the committed frontend snapshot come from,
    // so these assertions and the snapshot drift check read one schema.
    use super::{builder, sdl};

    #[test]
    fn the_schema_has_no_mutation_root() {
        // No Mutation type is defined (EmptyMutation), so a client cannot mutate governed
        // state here.
        let sdl = sdl();
        assert!(
            !sdl.contains("type Mutation"),
            "the read surface must define no Mutation type; SDL:\n{sdl}"
        );
        assert!(
            !sdl.contains("type Subscription"),
            "the read surface defines no Subscription either"
        );
        assert!(sdl.contains("type Query"), "a Query root is present");
    }

    #[tokio::test]
    async fn introspection_reports_a_null_mutation_type() {
        // The introspection a client runs sees no mutation root either.
        let schema = builder().finish();
        let resp = schema
            .execute("{ __schema { mutationType { name } queryType { name } } }")
            .await;
        assert!(
            resp.errors.is_empty(),
            "introspection succeeds: {:?}",
            resp.errors
        );
        let data = resp.data.into_json().expect("json");
        assert!(
            data["__schema"]["mutationType"].is_null(),
            "mutationType must be null (no mutation root): {data}"
        );
        assert_eq!(
            data["__schema"]["queryType"]["name"].as_str(),
            Some("Query")
        );
    }

    /// The SDL with its `"""…"""` descriptions stripped, so a neutrality scan sees only
    /// type and field names and keywords. A description may explain the derived
    /// Delivery/Cost display metrics, but no name may reference a capability or a metric.
    fn sdl_names_only() -> String {
        sdl()
            .split("\"\"\"")
            .step_by(2) // keep the code segments; drop the description segments between them
            .collect()
    }

    #[test]
    fn the_schema_names_no_capability_and_no_metric() {
        // No type or field names a capability or a metric: the display metrics ride the
        // opaque `Metric` bag, never a schema field.
        let sdl = sdl_names_only();
        for neutral in [
            "type Plan",
            "type PlanOption",
            "type Change",
            "type Metric",
            "type Lane",
            "type ReadModelView",
            "type AppliedChange",
            "type Policy",
            // The plan rows, the approved list and the scope.
            "type PlanEntry",
            "type ApprovedPlan",
            "type PortfolioRef",
            "type ScopeCount",
            "plans: [PlanEntry!]!",
            "approvedPlans(since: String): [ApprovedPlan!]!",
            "scope: [ScopeCount!]!",
            // Display labels read by id.
            "type EntityLabel",
            "labels(ids: [ID!]!): [EntityLabel!]!",
        ] {
            assert!(
                sdl.contains(neutral),
                "neutral type {neutral} missing from SDL"
            );
        }
        for forbidden in [
            "SupplyRisk",
            "supply_risk",
            "Churn",
            "Contention",
            "Delivery",
            "riskLeft",
            // The display framing rides the opaque bag, so none of these capability/metric
            // names may become a schema field.
            "severity",
            "Severity",
            "projectedCompletion",
            "daySlip",
            "orderValue",
            "revenueAtRisk",
            "lateOrders",
            // The scope kinds are data, never schema names (a lane's `workCenter` is its own
            // neutral field).
            "purchaseOrder",
            "salesOrder",
        ] {
            assert!(
                !sdl.contains(forbidden),
                "SDL must name no capability/metric, found `{forbidden}`:\n{sdl}"
            );
        }
    }

    #[tokio::test]
    async fn the_policy_serves_its_limit_currency() {
        let state = super::projections::tests::canned_state();
        let state = super::GraphqlState {
            policy: super::PolicyDisplay {
                cost_limit: Some(250.0),
                cost_limit_currency: Some("EUR".to_owned()),
                ..state.policy.clone()
            },
            ..state
        };
        let resp = super::schema(state)
            .execute("{ policy { costLimitCurrency criteria { rule value } } }")
            .await;
        assert!(resp.errors.is_empty(), "{:?}", resp.errors);
        let data = resp.data.into_json().expect("json");
        assert_eq!(data["policy"]["costLimitCurrency"], "EUR", "{data}");
    }

    #[tokio::test]
    async fn labels_are_served_in_request_order_for_the_ids_the_world_names() {
        use ekg_semantic::ontology::entity_iri;
        let line = entity_iri("purchase.order.line", 3);
        let so = entity_iri("sale.order", 1);
        let unnamed = entity_iri("product.product", 4);
        let query = format!(
            r#"{{ labels(ids: ["{so}", "{unnamed}", "https://evil.example/x", "{line}", "{so}"]) {{ id label }} }}"#
        );
        let resp = super::schema(super::projections::tests::canned_state())
            .execute(query.as_str())
            .await;
        assert!(resp.errors.is_empty(), "{:?}", resp.errors);
        let data = resp.data.into_json().expect("json");
        assert_eq!(
            data["labels"],
            serde_json::json!([
                { "id": so, "label": "S00001" },
                { "id": line, "label": "P00001 · 10 × Battery Pack 500Wh" },
            ]),
            "request order, each id once; the unnamed and foreign ids get none"
        );
    }

    /// A `labels` query over `n` sales-order ids.
    fn labels_query(n: usize) -> String {
        let ids = (1..=n)
            .map(|id| {
                let id = i64::try_from(id).expect("a small id");
                format!(
                    "\"{}\"",
                    ekg_semantic::ontology::entity_iri("sale.order", id)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{{ labels(ids: [{ids}]) {{ id }} }}")
    }

    #[tokio::test]
    async fn a_labels_request_is_bounded() {
        let schema = super::schema(super::projections::tests::canned_state());
        let resp = schema
            .execute(labels_query(super::query::MAX_LABEL_IDS + 1).as_str())
            .await;
        assert_eq!(resp.errors.len(), 1, "a field error beyond the bound");
        assert!(
            resp.errors[0].message.contains("at most 200 ids"),
            "{:?}",
            resp.errors
        );
        // At the bound, the request is served.
        let resp = schema
            .execute(labels_query(super::query::MAX_LABEL_IDS).as_str())
            .await;
        assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    }

    #[test]
    fn the_display_framing_rides_the_opaque_neutral_surface() {
        // The plan display framing (severity, projected date, day-slip, order value,
        // title/effect) and the headline KPIs are served only through the opaque,
        // capability-filled containers (a change's from and to values, the neutral [Metric]
        // bag and KPI list), never as capability- or metric-named schema fields.
        let sdl = sdl();
        // Every change carries `from` (the current value) beside its params (the new value).
        assert!(
            sdl.contains("from:"),
            "a Change exposes its from value:\n{sdl}"
        );
        // The auto-applied change carries an opaque display bag (title/effect ride it). Scope
        // the check to the AppliedChange type body: `metrics: [Metric!]!` also appears on
        // other types, so an unscoped substring check would pass even if the field were
        // dropped from AppliedChange.
        let applied_block = sdl
            .split_once("type AppliedChange {")
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(body, _)| body)
            .expect("the SDL defines an AppliedChange type");
        assert!(
            applied_block.contains("metrics: [Metric!]!"),
            "AppliedChange carries the opaque metrics bag:\n{sdl}"
        );
        // The headline KPIs are an opaque, capability-filled [Metric] list on the read model.
        assert!(
            sdl.contains("headlineMetrics: [Metric!]!"),
            "the headline metrics are an opaque KPI list:\n{sdl}"
        );
    }
}
