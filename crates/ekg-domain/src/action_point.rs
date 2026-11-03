//! Action points and the promotion and reversal records.

use serde::{Deserialize, Serialize};

use crate::ids::{ActionPointId, CorrelationId, EntityUri, ReviewerId, Revision};
use crate::proposal::{Plan, Proposal, ProposedAction};

/// The lifecycle state of an action point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPointState {
    /// Raised by the monitor; awaiting a proposal.
    Open,
    /// The bounded agent attached a grounded proposal.
    Proposed,
    /// A human is reviewing.
    UnderReview,
    /// A human promoted the proposal; the governed write-back ran.
    Promoted,
    /// A human aborted it (stop control) without writing back.
    Aborted,
    /// A promoted action was reversed by its inverse verb.
    Reversed,
    /// It expired via an escalation timer without promotion.
    Expired,
}

/// A durable action point raised when something drifts off plan.
///
/// Keyed by the subject entity (e.g. `SalesOrder/123`), one open action point per
/// subject, so reconciliation sweeps stay idempotent. `reason` is a plain string
/// produced by the capability's monitor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPoint {
    /// Stable id.
    pub id: ActionPointId,
    /// The subject entity this point is keyed by.
    pub subject: EntityUri,
    /// Current lifecycle state.
    pub state: ActionPointState,
    /// Deterministic reason the monitor opened it (not model-generated).
    pub reason: String,
    /// The agent's proposal, once attached.
    pub proposal: Option<Proposal>,
}

/// A human promotion, the only path to a governed write-back. Records the reviewer,
/// their rationale and the context they saw, plus the action point and correlation
/// id that anchor it in the audit trail (EU AI Act Art. 14 and 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promotion {
    /// The action point being promoted (audit linkage).
    pub action_point: ActionPointId,
    /// Correlates this promotion with the rest of the governed episode.
    pub correlation_id: CorrelationId,
    /// Who promoted.
    pub reviewer: ReviewerId,
    /// Why they promoted (their rationale, not the model's).
    pub rationale: String,
    /// A record of the context the reviewer was shown, including any omissions.
    pub context_seen: String,
}

/// A proposed action paired with the human promotion that authorized it, and the
/// world revision the decision was based on.
///
/// This is the only thing [`crate::ActionExecutor::execute`] accepts, so a governed
/// write-back cannot be expressed without a promotion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedAction {
    /// The governed action to commit.
    pub action: ProposedAction,
    /// The human promotion authorizing it.
    pub promotion: Promotion,
    /// The world-model revision the promotion was decided against. Carried onto
    /// the receipt so a later reverse can detect a conflicting source change.
    pub basis_revision: Revision,
}

/// A [`Plan`] paired with the single human promotion that authorized it and the
/// world revision the decision was based on. The plan-level analogue of
/// [`PromotedAction`].
///
/// One promotion authorizes all actions, so promoting a plan is one oversight
/// interaction (EU AI Act Art. 14 and 12). The per-action [`PromotedAction`]s are
/// derived at execute time (see [`PromotedPlan::promoted_actions`]), so every source
/// write goes through the single-action path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedPlan {
    /// The plan of proposed actions to commit.
    pub plan: Plan,
    /// The single human promotion authorizing the whole plan.
    pub promotion: Promotion,
    /// The world-model revision the promotion was decided against. Carried onto
    /// each receipt so a later reverse can detect a conflicting source change.
    pub basis_revision: Revision,
}

impl PromotedPlan {
    /// Derive the per-action [`PromotedAction`]s in the plan's listed order, each
    /// carrying the plan's shared [`Promotion`] and `basis_revision`.
    #[must_use]
    pub fn promoted_actions(&self) -> Vec<PromotedAction> {
        self.plan
            .actions
            .iter()
            .map(|action| PromotedAction {
                action: action.clone(),
                promotion: self.promotion.clone(),
                basis_revision: self.basis_revision,
            })
            .collect()
    }
}

/// The receipts for an executed [`Plan`]: enough to reverse and audit its actions as
/// a unit. The plan-level analogue of [`ExecutionReceipt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// The actions that were committed, in listed order.
    pub actions: Vec<ProposedAction>,
    /// One receipt per committed action, in the same order. A reverse walks these
    /// back to front, each under the per-action watermark guard.
    pub receipts: Vec<ExecutionReceipt>,
}

/// A receipt for an executed governed write-back: enough to reverse and audit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    /// The action that was committed.
    pub action: ProposedAction,
    /// A reference to the change in the system of record (e.g. the Odoo write).
    pub external_ref: String,
    /// The source revision the write was made against; a conditional reverse
    /// compares the current source revision to this and refuses on a mismatch.
    pub source_revision: Revision,
    /// Whether the inverse verb can undo it.
    pub reversible: bool,
}

/// A reversal of a previously promoted action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reversal {
    /// The action point whose promoted action is being reversed (audit linkage).
    pub action_point: ActionPointId,
    /// Correlates this reversal with the original governed episode.
    pub correlation_id: CorrelationId,
    /// Who reversed it.
    pub reviewer: ReviewerId,
    /// Why.
    pub reason: String,
}
