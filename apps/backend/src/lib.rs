//! `ekg-backend`: the HTTP surface behind `/api` and the ingestion wiring.
//!
//! Hosts the governed ingestion endpoint (`POST /api/ingest/odoo`): an untrusted thin
//! `{_model, _id}` webhook is authenticated and scope-checked here, then submitted to the
//! durable Restate worker that fat-pulls and materializes. The human promote gate and the
//! action-point routes are `action_point`, the portfolio promote/undo commands are
//! `portfolio_command`, the operational endpoints (liveness / readiness) are `health`, and
//! the code-generated OpenAPI contract (`GET /api/openapi.json`) is `openapi`. The durable
//! agent-skill registry (versions, activation, history) is `skills`; the agent's governed
//! rollout mode is `rollout`, and its shadow records are `shadow`. The offline eval of that
//! agent on fixed cases is `eval`. Trace export, off unless configured, is `telemetry`.

// Docs mention product names (Axum, Odoo, Restate, OpenAPI).
#![allow(clippy::doc_markdown)]
// async-graphql's `Enum` derive generates async trait-impl resolvers with no `.await`,
// because an enum resolves synchronously. The lint is attributed to the derive site, so
// the false positive is allowed at the crate root.
#![allow(clippy::unused_async_trait_impl)]

pub mod action_point;
pub mod command;
pub mod config;
pub mod drift;
pub mod eval;
pub mod graphql;
pub mod health;
pub mod ingest;
pub mod joint;
pub mod nats;
pub mod openapi;
pub mod operator;
pub mod policy_context;
pub mod portfolio_command;
pub mod projection;
pub mod promotion;
pub mod propose;
pub mod publisher;
pub mod qdrant;
pub mod rollout;
pub mod shadow;
pub mod shadow_report;
pub mod skills;
pub mod slates;
pub mod submit;
pub mod sweep;
pub mod telemetry;
pub mod worker;

pub use config::IngestionConfig;
pub use ingest::{AppState, router};
pub use nats::NatsPublisher;
pub use publisher::InProcessPublisher;
pub use qdrant::QdrantClient;
pub use submit::{RestateSubmitter, Submitter};
pub use sweep::Sweep;
pub use worker::Ingest;
