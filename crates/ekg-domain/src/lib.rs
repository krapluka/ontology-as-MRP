//! Neutral domain core for the Enterprise Knowledge Graph engine.
//!
//! This crate owns the vocabulary and the contract: entity identifiers,
//! source records, the `RdfChanged` event, grounding + omissions, proposals,
//! action points, and the async trait *seams* every external effect flows
//! through ([`WorldModel`], [`SourceIngestor`], [`RdfChangePublisher`],
//! [`CognitionProvider`], [`ActionExecutor`], [`Proposer`], [`Simulator`]). It has
//! no I/O of its own.
//!
//! Real drivers for the seams live in the layer crates (`ekg-semantic`,
//! `ekg-kinetic`, `ekg-dynamic`, `ekg-cognition`); in-memory [`fakes`] (behind
//! the `test-support` feature) let the whole governed loop run hermetically in
//! tests.
//!
//! Governance invariants encoded in the shape: detection is deterministic and
//! cognition is explanatory only; AI only ever *proposes*; a promotion is the
//! sole path to a governed write-back; every write-back is reversible; and
//! policy-hidden facts surface as recorded omissions.

// Type and seam names echo their module (e.g. `events::RdfChanged`).
#![allow(clippy::module_name_repetitions)]
// Docs mention product and standard names (GraphDB, NATS JetStream, Odoo, SHACL);
// backticking every one is noise.
#![allow(clippy::doc_markdown)]

pub mod action_point;
pub mod error;
pub mod events;
pub mod grounding;
pub mod ids;
pub mod proposal;
pub mod read_policy;
pub mod seams;
pub mod slate;
pub mod source;

#[cfg(any(test, feature = "test-support"))]
pub mod fakes;

#[cfg(test)]
mod tests;

pub use action_point::{
    ActionPoint, ActionPointState, ExecutionPlan, ExecutionReceipt, PromotedAction, PromotedPlan,
    Promotion, Reversal,
};
pub use error::{EngineError, Result};
pub use events::{ChangeKind, RdfChanged};
pub use grounding::{EvidenceEdge, Grounding, Omission, OmissionKind, ReadContext};
pub use ids::{
    ActionPointId, CorrelationId, EntityUri, NamedGraph, PolicyId, PortfolioId, ReviewerId,
    Revision, SHORT_DIGEST_HEX_CHARS, entity_model, short_digest,
};
pub use proposal::{ActionType, GroundedProposal, Plan, Proposal, ProposedAction};
pub use read_policy::{ReadPolicy, Visibility};
pub use seams::{
    ActionExecutor, CandidateProposer, CognitionProvider, GroupCandidateProposer, GroupScorer,
    GroupSlateProposer, Projection, Proposer, RdfChangePublisher, ReverseOutcome, Scorer,
    Simulator, SlateProposer, SourceIngestor, WorldModel,
};
pub use slate::{
    CouplingLink, Need, RankedPlan, Score, ScoredCandidate, Slate, SubjectGroup, Trace, TraceStep,
    TraceSummary, TraceTag,
};
pub use source::{SourceRecord, SourceRef};

/// The three architectural layers the neutral core preserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// Typed, governed objects and links.
    Semantic,
    /// Governed actions that write back under policy.
    Kinetic,
    /// Durable monitors and agent loops.
    Dynamic,
}

impl Layer {
    /// All layers, in spine order.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::Semantic, Self::Kinetic, Self::Dynamic]
    }
}
