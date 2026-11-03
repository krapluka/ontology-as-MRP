//! Dynamic layer: durable monitors and the action-point lifecycle (Restate).
//!
//! Consumes `RdfChanged`, runs the deterministic monitor (a capability rule via
//! `WorldModel::select`), and drives the [`ActionPoint`] lifecycle keyed by the
//! subject entity (so sweeps stay idempotent, one open point per subject), with
//! an escalation timer and the audit trail. It orchestrates the other seams;
//! detection stays deterministic and cognition is explanatory only.
//!
//! The monitor names no use case. The risk rule is an injected [`RiskRule`], so
//! another capability reuses the monitor unchanged. The lifecycle and monitor rules
//! live in [`lifecycle`] and [`monitor`] as pure functions; [`object`] adds
//! durability over them via Restate.
//!
//! Coupled points awaiting oversight are grouped into durable portfolios
//! ([`portfolio`]): the monitor evaluates injected [`CouplingRule`]s like its risk
//! rules, and the grouping commits nothing.

// Type and seam names echo their module (e.g. `rule::RiskRule`).
#![allow(clippy::module_name_repetitions)]
// Docs mention product and standard names (Restate, RdfChanged, SPARQL).
#![allow(clippy::doc_markdown)]

pub mod lifecycle;
pub mod monitor;
pub mod object;
pub mod portfolio;
pub mod rule;

pub use ekg_domain::{ActionPoint, ActionPointState};

pub use lifecycle::{
    ActionPointRecord, AuditEntry, AuditKind, ClaimCommand, CommandClaim, ExpireOutcome,
    MEMBER_AUTO_PROMOTE_REFUSAL, OpenOutcome, PointOutcome, PromotedVia, RefusedReverse,
    ReverseProgress, can_transition, is_pre_promotion, is_terminal,
};
pub use monitor::{
    FormedPortfolio, NotifyOutcome, coupling_edges, evaluate, flagged_subjects, notify_outcome,
    plan_grouping, projected_verdicts, verdicts,
};
pub use object::{
    AbortRequest, ActionPointService, ActionPointServiceClient, AttachPortfolioSlate, ClaimRequest,
    DEFAULT_ESCALATION, DismissPortfolioRequest, DriftMonitorService, DriftMonitorServiceClient,
    FormRequest, OUTCOME_HANDLER, OpenRequest, PORTFOLIO_OUTCOME_HANDLER, PortfolioService,
    PortfolioServiceClient, PromotePortfolioRequest, PromoteRequest, RecordPartialReverseRequest,
    RecordPortfolioPartialReverse, ReleaseRequest, ReversePortfolioRequest, ReverseRequest,
    SetPortfolioReceipt, SetPortfolioRequest, SupersedeRequest,
};
pub use portfolio::{
    FormOutcome, FormReceipt, GroupingPlan, PlannedPortfolio, PortfolioAuditEntry, PortfolioEvent,
    PortfolioOutcome, PortfolioPromotion, PortfolioRecord, PortfolioState, components,
    grouping_plan, portfolio_id,
};
pub use rule::{CouplingEdge, CouplingRule, DriftVerdict, RiskRule, Row};
