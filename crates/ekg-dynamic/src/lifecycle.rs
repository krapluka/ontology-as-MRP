//! The action-point lifecycle: the transition table, the durable record the
//! virtual object holds, and the open/abort/expire logic it drives.
//!
//! Free of Restate, so every rule (idempotency, the revision guard, valid
//! transitions, and the audit trail) can be unit-tested. The virtual object in
//! [`crate::object`] is a thin durable wrapper over this.

use std::collections::BTreeSet;

use ekg_domain::{
    ActionPoint, ActionPointId, ActionPointState, CorrelationId, EngineError, EntityUri,
    ExecutionPlan, Omission, PortfolioId, Promotion, Proposal, ProposedAction, Result, Reversal,
    ReverseOutcome, ReviewerId, Revision, Slate,
};
use serde::{Deserialize, Serialize};

/// Whether a state is terminal (no transition may leave it). `Promoted` is not
/// terminal: a promoted action can still be reversed.
#[must_use]
pub const fn is_terminal(state: ActionPointState) -> bool {
    matches!(
        state,
        ActionPointState::Aborted | ActionPointState::Reversed | ActionPointState::Expired
    )
}

/// Whether `to` is a permitted successor of `from`. An out-of-order request
/// (e.g. promoting an `open` point with no proposal) is refused, not silently
/// applied.
#[must_use]
pub const fn can_transition(from: ActionPointState, to: ActionPointState) -> bool {
    use ActionPointState::{Aborted, Expired, Open, Promoted, Proposed, Reversed, UnderReview};
    matches!(
        (from, to),
        (Open, Proposed | Aborted | Expired)
            | (Proposed, UnderReview | Aborted | Expired)
            | (UnderReview, Promoted | Aborted | Expired)
            | (Promoted, Reversed)
    )
}

/// The wire name of a claimed command, for refusal and audit text.
const fn command_name(command: ClaimCommand) -> &'static str {
    match command {
        ClaimCommand::Promote => "promote",
        ClaimCommand::AutoPromote => "auto_promote",
        ClaimCommand::Reverse => "reverse",
        ClaimCommand::Abort => "abort",
    }
}

/// The phrase an auto-promotion refused for a portfolio member carries, so a caller
/// across the ingress can tell it from a claim held by another command.
pub const MEMBER_AUTO_PROMOTE_REFUSAL: &str =
    "promoted only through its portfolio's plan, never auto-promoted alone";

/// The outcome of an [`ActionPointRecord::open`] evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenOutcome {
    /// A new action point was raised in `open`.
    Opened,
    /// An open point already existed for this subject; it was retained (idempotent).
    RetainedExisting,
    /// The triggering event was not newer than the last applied revision; ignored.
    IgnoredStale,
}

/// Whether a point is awaiting oversight (`open`, `proposed` or `under_review`), the
/// only states in which it can be a portfolio member.
#[must_use]
pub const fn is_pre_promotion(state: ActionPointState) -> bool {
    matches!(
        state,
        ActionPointState::Open | ActionPointState::Proposed | ActionPointState::UnderReview
    )
}

/// The outcome of an [`ActionPointRecord::expire`] evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpireOutcome {
    /// The point moved to `expired`, leaving the portfolio it belonged to, if any.
    Expired {
        /// The portfolio the point left.
        left: Option<PortfolioId>,
    },
    /// The point was already resolved (terminal or promoted); the timer is a no-op.
    NotPending,
    /// A governed command holds the point; the timer must fire again after
    /// `retry_after_secs`, when the claim has been released or has lapsed.
    Deferred {
        /// Seconds until the held claim's deadline.
        retry_after_secs: u64,
    },
}

/// The governed command a [`CommandClaim`] was taken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimCommand {
    /// A human promotion.
    Promote,
    /// A promotion by the auto-apply policy.
    AutoPromote,
    /// An undo of a promoted plan.
    Reverse,
    /// The stop control.
    Abort,
}

/// A governed command's hold on an action point: taken before any write-back,
/// cleared when the command records its outcome or releases. While it is held and
/// its deadline has not passed, no other command may claim the point, so two
/// commands can never both write back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandClaim {
    /// The journaled token the holder presents to record or release.
    pub token: String,
    /// The command holding the point.
    pub command: ClaimCommand,
    /// Who issued the command.
    pub reviewer: ReviewerId,
    /// When the claim was taken, as Unix epoch seconds.
    pub claimed_at: u64,
    /// When the claim lapses, as Unix epoch seconds; after it the next command may take
    /// the point over.
    pub deadline: u64,
}

/// What a non-transition [`AuditEntry`] records. Transition entries carry `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// A governed command's write-back failed and it released its claim; the state is
    /// unchanged.
    CommandFailed,
    /// A command took over a claim whose deadline had passed.
    ClaimTakenOver,
    /// An undo reversed only some of the executed plan's actions; the point stays
    /// `promoted` and the progress is recorded so a retry resumes from it.
    PartiallyReversed,
}

/// One receipt a reverse could not undo: its index into the executed plan and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedReverse {
    /// The receipt's index into the executed plan.
    pub index: usize,
    /// The action and the refusal (e.g. the source changed after the write).
    pub reason: String,
}

/// How far an executed plan's undo has got: the receipts already reversed, and the ones
/// the last attempt could not reverse. A retry reverses only the receipts not in
/// [`Self::reversed`]. Indices are stable: the executed plan is immutable once recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseProgress {
    /// The receipts reversed so far, over every attempt.
    #[serde(default)]
    pub reversed: BTreeSet<usize>,
    /// The receipts the last attempt refused, each with why.
    #[serde(default)]
    pub refused: Vec<RefusedReverse>,
}

impl ReverseProgress {
    /// Whether nothing was ever attempted (no receipt reversed or refused).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reversed.is_empty() && self.refused.is_empty()
    }

    /// The progress after `outcome`, one more attempt over the receipts not yet reversed:
    /// every receipt reversed so far plus the ones reversed now, and the refusals of this
    /// attempt (the earlier ones were retried).
    #[must_use]
    pub fn after(&self, outcome: &ReverseOutcome) -> Self {
        let mut reversed = self.reversed.clone();
        reversed.extend(outcome.reversed.iter().copied());
        Self {
            reversed,
            refused: outcome
                .refused
                .iter()
                .map(|(index, reason)| RefusedReverse {
                    index: *index,
                    reason: reason.clone(),
                })
                .collect(),
        }
    }

    /// Check that this progress is a valid successor of `previous` over a plan of `total`
    /// receipts: every index names a receipt of the plan, nothing reversed before is lost,
    /// and nothing is both reversed and refused.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] naming the violation.
    pub fn check(&self, previous: &Self, total: usize) -> Result<()> {
        let refused = self.refused.iter().map(|r| r.index);
        if let Some(stray) = self
            .reversed
            .iter()
            .copied()
            .chain(refused)
            .find(|&i| i >= total)
        {
            return Err(EngineError::Invariant(format!(
                "reverse progress names receipt {stray}, but the executed plan has {total}"
            )));
        }
        if !self.reversed.is_superset(&previous.reversed) {
            return Err(EngineError::Invariant(
                "reverse progress loses receipts an earlier attempt reversed".to_owned(),
            ));
        }
        if let Some(both) = self
            .refused
            .iter()
            .find(|r| self.reversed.contains(&r.index))
        {
            return Err(EngineError::Invariant(format!(
                "reverse progress names receipt {} as both reversed and refused",
                both.index
            )));
        }
        Ok(())
    }

    /// The audit summary of this progress over `total` receipts: how many are reversed,
    /// how many remain, and why the last attempt refused them.
    #[must_use]
    pub fn summary(&self, total: usize) -> String {
        let reasons = self
            .refused
            .iter()
            .map(|r| r.reason.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "partially reversed: {} of {total} action(s) reversed, {} remain unreversed: {reasons}",
            self.reversed.len(),
            total.saturating_sub(self.reversed.len())
        )
    }
}

/// The portfolio promotion a member point was promoted through: the portfolio, the
/// generation the promotion was decided for, and the chosen option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedVia {
    /// The portfolio whose plan was promoted.
    pub portfolio: PortfolioId,
    /// The generation the promotion was decided for.
    pub generation: u32,
    /// The chosen joint-slate option's `rank_key` (opaque).
    pub rank_key: String,
}

/// How a point ended, or that its promotion was reversed, as reported to whoever listens
/// for outcomes. It repeats what the record holds at that moment and adds the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointOutcome {
    /// The point the outcome is of.
    pub action_point: ActionPointId,
    /// Which of the subject's points it is, counted from 1. A subject's points share one
    /// id, so this is what tells them apart.
    pub point_seq: u32,
    /// The state reached: `promoted`, `aborted`, `expired` or `reversed`.
    pub state: ActionPointState,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
    /// Who caused it (`None` for the escalation timer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ReviewerId>,
    /// The executed actions of a promotion, in order; empty for every other state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ProposedAction>,
    /// The portfolio promotion the point was promoted through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_via: Option<PromotedVia>,
    /// When the slate the point carried was attached, if it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate_attached_at: Option<String>,
}

/// One durable audit entry recording an action-point transition (EU AI Act
/// Art. 12): the point, the state it reached, and, for a human intervention,
/// the reviewer who caused it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// The action point the entry concerns.
    pub action_point: ActionPointId,
    /// The state reached by the transition.
    pub state: ActionPointState,
    /// The reviewer, when the transition was human-initiated (`None` for
    /// monitor-raised or timer-driven transitions).
    pub reviewer: Option<ReviewerId>,
    /// A free-text note for a human intervention (the abort reason or the promote
    /// rationale), so the audit trail records why as well as who and what.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The read-policy omissions the attached proposal disclosed: what the
    /// read-policy withheld, each stamped with its `policy_id` (the policy version).
    /// Empty for every transition but the `Proposed` attach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<Omission>,
    /// Correlates this entry with the rest of the governed episode (derived at attach as
    /// `attach/<action-point-id>`). `None` for transitions that carry no omission
    /// provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    /// What a non-transition entry records (a failed command, a claim takeover).
    /// `None` for a transition. Defaulted and skipped when absent so entries persisted
    /// without it still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AuditKind>,
}

/// The durable state a single action-point virtual object holds for its subject.
///
/// Keyed by subject entity, so there is one record per subject and therefore at
/// most one open action point per subject. `last_revision` is the high-water mark
/// that makes the monitor safe under at-least-once and out-of-order delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPointRecord {
    /// The current action point, once raised.
    pub point: Option<ActionPoint>,
    /// The highest `RdfChanged` revision applied for this subject.
    pub last_revision: Option<Revision>,
    /// The human promotion that authorized the write-back, once promoted.
    pub promotion: Option<Promotion>,
    /// The receipts for the executed plan (its 1..N actions promoted as one step),
    /// once promoted, so a later `reverse_plan` needs nothing else. A single-action
    /// promotion is a 1-action plan. Defaulted so records persisted without it still
    /// load, and skipped when absent so such a record round-trips unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_plan: Option<ExecutionPlan>,
    /// The ranked remediation slate the cognition loop attached to this point, once
    /// one was produced. The slate commits nothing and does not itself drive a state
    /// transition: a human still promotes one chosen plan as one governed step.
    /// Defaulted so older records still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate: Option<Slate>,
    /// When the plan was promoted, as Unix epoch seconds (a stringly value, like
    /// every other neutral field). Stamped by the durable `promote` handler so the
    /// read `/api` can anchor the reversal window of an auto-applied change.
    /// Display-only: it drives no governed transition and is `None` until a plan is
    /// promoted. Defaulted and skipped when absent so older records still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_at: Option<String>,
    /// The governed command holding the point, if any. Defaulted and skipped when
    /// absent so older records still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<CommandClaim>,
    /// The portfolio of coupled points this point belongs to while it awaits oversight.
    /// Set by the monitor's grouping, cleared when the point leaves the pre-promotion
    /// states. Defaulted and skipped when absent so older records still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portfolio: Option<PortfolioId>,
    /// The portfolio promotion this point was promoted through, when it was promoted as a
    /// member of its portfolio's plan and not on its own. Defaulted and skipped when
    /// absent so older records still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_via: Option<PromotedVia>,
    /// How far the undo of the executed plan has got, after a partial reverse.
    /// Defaulted and skipped when empty so older records still load.
    #[serde(default, skip_serializing_if = "ReverseProgress::is_empty")]
    pub reverse_progress: ReverseProgress,
    /// When the current slate was attached, as Unix epoch seconds (stringly, like
    /// `promoted_at`); overwritten by every attach. Display-only: it drives no
    /// transition. Defaulted and skipped when absent so older records still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate_attached_at: Option<String>,
    /// How many points were opened for this subject, so the current one's number. A
    /// subject's points share one id; this tells them apart. Defaulted so older records
    /// still load: their count starts at the next point opened.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub points_opened: u32,
    /// The append-only audit trail of every transition.
    pub audit: Vec<AuditEntry>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

impl ActionPointRecord {
    /// The current lifecycle state, if a point has been raised.
    #[must_use]
    pub fn state(&self) -> Option<ActionPointState> {
        self.point.as_ref().map(|p| p.state)
    }

    /// The outcome to report after a transition, stamped `at`: `Some` when the point is
    /// `promoted`, `aborted`, `expired` or `reversed`, with the reviewer its last audit
    /// entry names.
    #[must_use]
    pub fn outcome(&self, at: u64) -> Option<PointOutcome> {
        let point = self.point.as_ref()?;
        let promoted = point.state == ActionPointState::Promoted;
        if !promoted && !is_terminal(point.state) {
            return None;
        }
        let reviewer = self
            .audit
            .iter()
            .rev()
            .find(|entry| {
                entry.action_point == point.id && entry.state == point.state && entry.kind.is_none()
            })
            .and_then(|entry| entry.reviewer.clone());
        Some(PointOutcome {
            action_point: point.id.clone(),
            point_seq: self.points_opened,
            state: point.state,
            at,
            reviewer,
            actions: self
                .execution_plan
                .as_ref()
                .filter(|_| promoted)
                .map(|plan| plan.actions.clone())
                .unwrap_or_default(),
            promoted_via: self.promoted_via.clone().filter(|_| promoted),
            slate_attached_at: self.slate_attached_at.clone(),
        })
    }

    /// Idempotently open an action point for `subject` at `revision`.
    ///
    /// Ignores an event whose `revision` is not greater than the last applied
    /// (stale or out of order). Retains an already-open point instead of duplicating
    /// it. Opens a fresh point only when none exists or the previous episode is
    /// terminal.
    pub fn open(
        &mut self,
        id: ActionPointId,
        subject: EntityUri,
        reason: String,
        revision: Revision,
    ) -> OpenOutcome {
        if let Some(last) = self.last_revision
            && revision <= last
        {
            return OpenOutcome::IgnoredStale;
        }
        self.last_revision = Some(revision);

        // A non-terminal point is retained: one open point per subject.
        if let Some(current) = self.state()
            && !is_terminal(current)
        {
            return OpenOutcome::RetainedExisting;
        }

        self.points_opened = self.points_opened.saturating_add(1);
        self.point = Some(ActionPoint {
            id: id.clone(),
            subject,
            state: ActionPointState::Open,
            reason,
            proposal: None,
        });
        self.audit.push(AuditEntry {
            action_point: id,
            state: ActionPointState::Open,
            reviewer: None,
            note: None,
            omissions: Vec::new(),
            correlation_id: None,
            kind: None,
        });
        OpenOutcome::Opened
    }

    /// Abort an un-promoted action point via the stop control, recording the
    /// intervention against `reviewer`. The abort must hold the point's claim
    /// (`token`), which it clears. The point leaves its portfolio; the portfolio it left,
    /// if any, is returned so the caller can supersede it.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if `token` does not match the held claim, or the
    /// point's current state may not transition to `aborted` (e.g. it is already
    /// terminal).
    pub fn abort(
        &mut self,
        token: &str,
        reviewer: ReviewerId,
        reason: Option<String>,
    ) -> Result<Option<PortfolioId>> {
        self.require_claim(token)?;
        self.transition(ActionPointState::Aborted, Some(reviewer), reason)?;
        self.claim = None;
        Ok(self.portfolio.take())
    }

    /// Record the portfolio this point belongs to (`None`: it is on its own), as the
    /// monitor's grouping decided at `now` (Unix epoch seconds). Applies only while the
    /// point awaits oversight and no governed command holds it; otherwise it is a no-op.
    /// A held claim freezes membership, so a portfolio cannot be superseded under a
    /// command that checked it; a lapsed claim freezes nothing, and a later pass
    /// regroups the point once the command settles. Returns the portfolio the point
    /// left (the previous one, when the value changed and a previous one was set) so
    /// the caller can supersede it.
    pub fn set_portfolio(
        &mut self,
        portfolio: Option<PortfolioId>,
        now: u64,
    ) -> Option<PortfolioId> {
        if !self.state().is_some_and(is_pre_promotion)
            || self.portfolio == portfolio
            || self.claim_held(now)
        {
            return None;
        }
        std::mem::replace(&mut self.portfolio, portfolio)
    }

    /// Whether a governed command holds the point at `now` (a claim whose deadline has not
    /// passed).
    fn claim_held(&self, now: u64) -> bool {
        self.claim.as_ref().is_some_and(|held| now < held.deadline)
    }

    /// Claim the point for a governed command before its write-back.
    ///
    /// Refused while another claim is held and `now` is before its deadline, and when
    /// the point is not in one of `expected` states. An auto-promotion is refused while the
    /// point is a portfolio member: a member is promoted only through its portfolio's plan
    /// or by a human. A claim whose deadline has passed (its holder stopped) is taken over,
    /// and the takeover is audited.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if another command holds the point, the point is in the
    /// wrong state, or an auto-promotion targets a portfolio member.
    pub fn claim(
        &mut self,
        claim: CommandClaim,
        expected: &[ActionPointState],
        now: u64,
    ) -> Result<()> {
        let (id, state) = {
            let point = self
                .point
                .as_ref()
                .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
            (point.id.clone(), point.state)
        };
        let lapsed = match &self.claim {
            Some(held) if now < held.deadline => {
                return Err(EngineError::Invariant(format!(
                    "another command is in progress on this action point: {} by {} \
                     (claimed at {}s, held until {}s)",
                    command_name(held.command),
                    held.reviewer.0,
                    held.claimed_at,
                    held.deadline
                )));
            }
            held => held.clone(),
        };
        if !expected.contains(&state) {
            return Err(EngineError::Invariant(format!(
                "action point is {state:?}; {} needs it to be {expected:?}",
                command_name(claim.command)
            )));
        }
        // A portfolio member is promoted only through its portfolio's plan, or by a human
        // on the point: never auto-promoted alone.
        if claim.command == ClaimCommand::AutoPromote
            && let Some(portfolio) = &self.portfolio
        {
            return Err(EngineError::Invariant(format!(
                "action point is a member of portfolio {}; it is {MEMBER_AUTO_PROMOTE_REFUSAL}",
                portfolio.0
            )));
        }
        if let Some(lapsed) = lapsed {
            self.audit.push(AuditEntry {
                action_point: id,
                state,
                reviewer: Some(claim.reviewer.clone()),
                note: Some(format!(
                    "took over an expired claim: {} by {} (claimed at {}s, deadline {}s)",
                    command_name(lapsed.command),
                    lapsed.reviewer.0,
                    lapsed.claimed_at,
                    lapsed.deadline
                )),
                omissions: Vec::new(),
                correlation_id: None,
                kind: Some(AuditKind::ClaimTakenOver),
            });
        }
        self.claim = Some(claim);
        Ok(())
    }

    /// Release the claim `token` holds after a failed write-back, recording the failed
    /// attempt (the claim's reviewer and `reason`) in the audit trail. The state is
    /// unchanged. A token that does not match the held claim is a no-op, so a retried
    /// release is idempotent. Returns whether a claim was released.
    pub fn release(&mut self, token: &str, reason: String) -> bool {
        if self.claim.as_ref().is_none_or(|held| held.token != token) {
            return false;
        }
        let (Some(point), Some(released)) = (self.point.as_ref(), self.claim.take()) else {
            return false;
        };
        let (id, state) = (point.id.clone(), point.state);
        self.audit.push(AuditEntry {
            action_point: id,
            state,
            reviewer: Some(released.reviewer),
            note: Some(reason),
            omissions: Vec::new(),
            correlation_id: None,
            kind: Some(AuditKind::CommandFailed),
        });
        true
    }

    /// Fire the escalation timer at `now` (Unix epoch seconds): expire the point if it
    /// is still pending.
    ///
    /// A resolved point (terminal, or `promoted`) does not expire: the timer is a
    /// no-op. A point a governed command holds is not expired mid-command: the
    /// outcome is [`ExpireOutcome::Deferred`] until the claim's deadline. A lapsed
    /// claim does not defer expiry, and is cleared with it. An expired point leaves its
    /// portfolio, which the outcome names.
    pub fn expire(&mut self, now: u64) -> ExpireOutcome {
        match self.state() {
            Some(current) if can_transition(current, ActionPointState::Expired) => {
                if let Some(held) = self.claim.as_ref().filter(|held| now < held.deadline) {
                    return ExpireOutcome::Deferred {
                        retry_after_secs: held.deadline - now,
                    };
                }
                // `can_transition` above means a point exists and the move is legal,
                // so `transition` cannot error here.
                if self
                    .transition(ActionPointState::Expired, None, None)
                    .is_err()
                {
                    return ExpireOutcome::NotPending;
                }
                self.claim = None;
                ExpireOutcome::Expired {
                    left: self.portfolio.take(),
                }
            }
            _ => ExpireOutcome::NotPending,
        }
    }

    /// Check that `token` holds the point's claim. Every recording command
    /// (`promote_plan`, `reverse`, `abort`) passes this before it transitions.
    fn require_claim(&self, token: &str) -> Result<()> {
        match &self.claim {
            Some(held) if held.token == token => Ok(()),
            Some(held) => Err(EngineError::Invariant(format!(
                "the command's claim is no longer held; the point is held by {} by {}",
                command_name(held.command),
                held.reviewer.0
            ))),
            None => Err(EngineError::Invariant(
                "the command holds no claim on this action point".to_owned(),
            )),
        }
    }

    /// Attach a grounded proposal, moving the point from `open` to `proposed`.
    ///
    /// Enforces omission disclosure: every omission the grounding recorded must
    /// appear on the proposal, else it is rejected and the point does not advance. A
    /// reviewer is never shown a policy-incomplete grounding as if it were complete.
    ///
    /// `subject_members` are the entities the subject's grounding referenced; a
    /// proposal may target the subject or one of these members (so a point about
    /// an order may be remediated by an action on one of its work orders), but never
    /// an entity the subject's evidence did not reference.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if the point is not `open`;
    /// [`EngineError::InvalidProposal`] if the proposal targets an entity outside the
    /// subject and its grounded members, or omits a recorded omission.
    pub fn attach_proposal(
        &mut self,
        proposal: Proposal,
        grounding_omissions: &[Omission],
        subject_members: &[EntityUri],
    ) -> Result<()> {
        let point = self
            .point
            .as_mut()
            .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
        if !can_transition(point.state, ActionPointState::Proposed) {
            return Err(EngineError::Invariant(format!(
                "illegal action-point transition {:?} -> Proposed",
                point.state
            )));
        }
        // An empty plan is not a remediation and must not advance a point to
        // `proposed`.
        if proposal.actions.is_empty() {
            return Err(EngineError::InvalidProposal(
                "proposal carries no action".to_owned(),
            ));
        }
        // Each action must target this point's subject or an entity the subject's
        // grounding referenced. A single ungrounded target refuses the whole plan.
        for action in &proposal.actions {
            let target = &action.target;
            if target != &point.subject && !subject_members.contains(target) {
                return Err(EngineError::InvalidProposal(format!(
                    "proposal targets {} but the action point subject is {} (and it is not a grounded member)",
                    action.target, point.subject
                )));
            }
        }
        if !grounding_omissions
            .iter()
            .all(|omission| proposal.omissions.contains(omission))
        {
            return Err(EngineError::InvalidProposal(
                "proposal does not disclose its grounding's recorded omissions".to_owned(),
            ));
        }
        let id = point.id.clone();
        // The audit entry records what the read-policy withheld (Art. 12): the
        // disclosed omissions, each carrying its policy_id, correlated to the attach.
        // Clone before the proposal moves onto the point.
        let omissions = proposal.omissions.clone();
        let correlation_id = Some(CorrelationId(format!("attach/{}", id.0)));
        point.proposal = Some(proposal);
        point.state = ActionPointState::Proposed;
        self.audit.push(AuditEntry {
            action_point: id,
            state: ActionPointState::Proposed,
            reviewer: None,
            note: None,
            omissions,
            correlation_id,
            kind: None,
        });
        Ok(())
    }

    /// Attach a ranked remediation [`Slate`] to a pre-promotion point at
    /// `attached_at` (Unix epoch seconds, the handler's journaled clock read). The
    /// slate commits nothing and does not transition the point: a human still promotes
    /// one chosen plan as one governed step. It attaches to a point that is `open`,
    /// `proposed` or `under_review`: the slate loop is spawned off the bridge's
    /// critical path, so the single-proposal path may have advanced the point before
    /// the slate lands. A resolved point (terminal, or `promoted`) takes no slate. The
    /// attach is audited (Art. 12) with the omissions the recommended option
    /// disclosed; the entry's `state` is the unchanged current state.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if the point is already resolved (terminal or
    /// `promoted`).
    pub fn attach_slate(&mut self, slate: Slate, attached_at: String) -> Result<()> {
        let (id, current_state) = {
            let point = self
                .point
                .as_ref()
                .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
            (point.id.clone(), point.state)
        };
        if is_terminal(current_state) || current_state == ActionPointState::Promoted {
            return Err(EngineError::Invariant(format!(
                "a slate attaches only to a pre-promotion action point, not {current_state:?}"
            )));
        }
        let option_count = slate.options.len();
        // Art. 12: record what the recommended option's grounding withheld (empty when
        // the slate carries only the baseline).
        let omissions = slate
            .recommended()
            .map(|option| option.omissions.clone())
            .unwrap_or_default();
        // Art. 12/14: the audit note carries the trace's counts (simulated, rejected,
        // open assumptions), so a reviewer sees how the slate was reached without
        // opening it. The full ordered trace is stored with the slate.
        let trace = &slate.trace.summary;
        let correlation_id = Some(CorrelationId(format!("slate/{}", id.0)));
        // The counts span every candidate, rejected ones included. "across all
        // candidates" keeps an operator from reading the assumption count as that of
        // the recommended plan alone; the per-plan trace carries that.
        let note = Some(format!(
            "ranked slate attached: {option_count} option(s); trace across all candidates: \
             {} simulated, {} rejected, {} open assumption(s)",
            trace.simulated, trace.rejected, trace.open_assumptions
        ));
        self.slate = Some(slate);
        self.slate_attached_at = Some(attached_at);
        self.audit.push(AuditEntry {
            action_point: id,
            state: current_state,
            reviewer: None,
            note,
            omissions,
            correlation_id,
            kind: None,
        });
        Ok(())
    }

    /// Promote a `proposed` point whose proposal is a plan of N actions: record the
    /// single human promotion and the executed [`ExecutionPlan`] (N receipts), and walk
    /// from `proposed` through `under_review` to `promoted`. The whole plan gets one
    /// `under_review` and `promoted` audit pair (one promotion, one correlation id),
    /// not one per action, so promoting a plan is a single human-oversight interaction
    /// (EU AI Act Art. 14 & 12).
    ///
    /// The caller claims the point (`token`), runs the governed write-back
    /// (`execute_plan`) and hands in the plan; this only records the outcome and clears
    /// the claim. The promoted point leaves its portfolio; the portfolio it left, if
    /// any, is returned so the caller can supersede it.
    ///
    /// When the point is promoted as a member of its portfolio's own plan (`via`), the
    /// `promoted` audit note names the portfolio, its generation and the option, and the
    /// portfolio it leaves is not returned when it is `via`'s: that portfolio records
    /// the promotion itself and is not superseded.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if `token` does not match the held claim, or the
    /// point is not `proposed`.
    pub fn promote_plan(
        &mut self,
        token: &str,
        promotion: Promotion,
        execution_plan: ExecutionPlan,
        via: Option<PromotedVia>,
    ) -> Result<Option<PortfolioId>> {
        let state = self
            .state()
            .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
        self.require_claim(token)?;
        if state != ActionPointState::Proposed {
            return Err(EngineError::Invariant(format!(
                "cannot promote from {state:?}; a proposal must be attached (Proposed) first"
            )));
        }
        let reviewer = promotion.reviewer.clone();
        let note = match &via {
            Some(via) => format!(
                "promoted as part of portfolio {} (generation {}), option {}: {}",
                via.portfolio, via.generation, via.rank_key, promotion.rationale
            ),
            None => promotion.rationale.clone(),
        };
        // One oversight interaction covers all N actions: one under_review and
        // promoted pair, not N.
        self.transition(ActionPointState::UnderReview, Some(reviewer.clone()), None)?;
        self.transition(ActionPointState::Promoted, Some(reviewer), Some(note))?;
        self.promotion = Some(promotion);
        self.execution_plan = Some(execution_plan);
        self.claim = None;
        let left = self.portfolio.take();
        let through_it = via
            .as_ref()
            .is_some_and(|via| left.as_ref() == Some(&via.portfolio));
        self.promoted_via = via;
        Ok(if through_it { None } else { left })
    }

    /// Record an undo that reversed only some of the executed plan's actions: store
    /// `progress` so a retry reverses only the rest, audit it (`partially_reversed`,
    /// with the counts and the refusals, attributed to the claim's reviewer), keep the
    /// point `promoted`, and release the claim. The undo must hold the point's claim
    /// (`token`).
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if `token` does not match the held claim, the point is
    /// not `promoted` or has no executed plan, or `progress` is not a valid successor of
    /// the recorded one ([`ReverseProgress::check`]).
    pub fn record_partial_reverse(&mut self, token: &str, progress: ReverseProgress) -> Result<()> {
        let (id, state) = {
            let point = self
                .point
                .as_ref()
                .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
            (point.id.clone(), point.state)
        };
        self.require_claim(token)?;
        if state != ActionPointState::Promoted {
            return Err(EngineError::Invariant(format!(
                "cannot record a partial reverse from {state:?}; only a promoted action is reversed"
            )));
        }
        let total = self
            .execution_plan
            .as_ref()
            .map(|plan| plan.receipts.len())
            .ok_or_else(|| {
                EngineError::Invariant("a promoted point without an executed plan".to_owned())
            })?;
        progress.check(&self.reverse_progress, total)?;
        let reviewer = self.claim.take().map(|held| held.reviewer);
        self.audit.push(AuditEntry {
            action_point: id,
            state,
            reviewer,
            note: Some(progress.summary(total)),
            omissions: Vec::new(),
            correlation_id: None,
            kind: Some(AuditKind::PartiallyReversed),
        });
        self.reverse_progress = progress;
        Ok(())
    }

    /// Reverse a promoted point (from `promoted` to `reversed`), recording the reversal
    /// in the audit trail (EU AI Act Art. 12). The caller (the `/api` undo command) runs
    /// the governed reverse write-back (`reverse_plan` over the stored
    /// [`ExecutionPlan`]); this only records the outcome, as `promote_plan` does after
    /// `execute_plan`. Undoing an auto-applied change is an explicit governed command,
    /// not a silent rollback. The undo must hold the point's claim (`token`), which it
    /// clears.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if no point has been raised;
    /// [`EngineError::Invariant`] if `token` does not match the held claim, or the
    /// point is not `promoted` (only a promoted action can be reversed).
    pub fn reverse(&mut self, token: &str, reversal: Reversal) -> Result<()> {
        let state = self
            .state()
            .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
        self.require_claim(token)?;
        if state != ActionPointState::Promoted {
            return Err(EngineError::Invariant(format!(
                "cannot reverse from {state:?}; only a promoted action can be reversed"
            )));
        }
        self.transition(
            ActionPointState::Reversed,
            Some(reversal.reviewer),
            Some(reversal.reason),
        )?;
        self.claim = None;
        Ok(())
    }

    /// Apply a validated transition and append its audit entry.
    fn transition(
        &mut self,
        to: ActionPointState,
        reviewer: Option<ReviewerId>,
        note: Option<String>,
    ) -> Result<()> {
        let point = self
            .point
            .as_mut()
            .ok_or_else(|| EngineError::NotFound("action point not raised".to_owned()))?;
        if !can_transition(point.state, to) {
            return Err(EngineError::Invariant(format!(
                "illegal action-point transition {:?} -> {to:?}",
                point.state
            )));
        }
        point.state = to;
        self.audit.push(AuditEntry {
            action_point: point.id.clone(),
            state: to,
            reviewer,
            note,
            omissions: Vec::new(),
            correlation_id: None,
            kind: None,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use ekg_domain::ExecutionReceipt;

    use super::*;

    fn subject() -> EntityUri {
        EntityUri("https://ekg.example/id/sale.order/42".to_owned())
    }

    fn ap_id() -> ActionPointId {
        ActionPointId("action-point/sale.order/42".to_owned())
    }

    fn open_fresh(rec: &mut ActionPointRecord, rev: u64) -> OpenOutcome {
        rec.open(
            ap_id(),
            subject(),
            "shortfall 13 on Battery".to_owned(),
            Revision(rev),
        )
    }

    fn omission() -> ekg_domain::Omission {
        ekg_domain::Omission {
            kind: ekg_domain::OmissionKind::Node,
            type_iri: "ekg:Supplier".to_owned(),
            count: 1,
            policy_id: ekg_domain::PolicyId("read-policy/demo-v1".to_owned()),
        }
    }

    fn proposal(omissions: Vec<ekg_domain::Omission>) -> Proposal {
        Proposal::single(
            "expedite the battery PO to cover the shortfall".to_owned(),
            ekg_domain::ProposedAction {
                action_type: ekg_domain::ActionType("expedite_po".to_owned()),
                target: subject(),
                params: std::collections::BTreeMap::new(),
            },
            omissions,
        )
    }

    /// A proposed action of `verb` against `target` (a plan building block).
    fn action_on(verb: &str, target: EntityUri) -> ekg_domain::ProposedAction {
        ekg_domain::ProposedAction {
            action_type: ekg_domain::ActionType(verb.to_owned()),
            target,
            params: std::collections::BTreeMap::new(),
        }
    }

    /// The fixed "now" (Unix epoch seconds) the claim tests run at.
    const NOW: u64 = 1_000;
    /// The claim deadline the tests use.
    const HOLD_SECS: u64 = 600;

    /// A claim for `command` by `reviewer`, taken at [`NOW`], carrying `token`.
    fn command_claim(command: ClaimCommand, reviewer: &str, token: &str) -> CommandClaim {
        CommandClaim {
            token: token.to_owned(),
            command,
            reviewer: ReviewerId(reviewer.to_owned()),
            claimed_at: NOW,
            deadline: NOW + HOLD_SECS,
        }
    }

    /// Every state a live command may start from, so a test can claim in any state and
    /// exercise the recording method's own state rule.
    const ANY_LIVE: [ActionPointState; 4] = [
        ActionPointState::Open,
        ActionPointState::Proposed,
        ActionPointState::UnderReview,
        ActionPointState::Promoted,
    ];

    /// Claim the point for `command` and return the token.
    fn hold(rec: &mut ActionPointRecord, command: ClaimCommand) -> String {
        rec.claim(
            command_claim(command, "planner-1", "token-1"),
            &ANY_LIVE,
            NOW,
        )
        .expect("the point is claimable");
        "token-1".to_owned()
    }

    /// Abort via the stop control the way the object does: claim, then abort.
    fn stop(rec: &mut ActionPointRecord, reviewer: &str) -> Result<()> {
        rec.claim(
            command_claim(ClaimCommand::Abort, reviewer, "stop-token"),
            &[
                ActionPointState::Open,
                ActionPointState::Proposed,
                ActionPointState::UnderReview,
            ],
            NOW,
        )?;
        rec.abort("stop-token", ReviewerId(reviewer.to_owned()), None)
            .map(|_| ())
    }

    fn proposed_record() -> ActionPointRecord {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(vec![]), &[], &[])
            .expect("attach");
        rec
    }

    fn empty_plan() -> ExecutionPlan {
        ExecutionPlan {
            actions: Vec::new(),
            receipts: Vec::new(),
        }
    }

    #[test]
    fn a_claim_is_refused_while_another_is_held() {
        let mut rec = proposed_record();
        rec.claim(
            command_claim(ClaimCommand::Promote, "planner-1", "a"),
            &[ActionPointState::Proposed],
            NOW,
        )
        .expect("the first claim is taken");
        let err = rec
            .claim(
                command_claim(ClaimCommand::AutoPromote, "policy:auto", "b"),
                &[ActionPointState::Proposed],
                NOW + HOLD_SECS - 1,
            )
            .expect_err("a second claim before the deadline is refused");
        let EngineError::Invariant(why) = err else {
            panic!("a refused claim is an invariant error, got {err:?}");
        };
        assert!(
            why.contains("promote by planner-1"),
            "the refusal names the holder: {why}"
        );
        assert_eq!(rec.claim.as_ref().map(|c| c.token.as_str()), Some("a"));
    }

    #[test]
    fn a_claim_is_refused_in_the_wrong_state() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let err = rec.claim(
            command_claim(ClaimCommand::Promote, "planner-1", "a"),
            &[ActionPointState::Proposed],
            NOW,
        );
        assert!(matches!(err, Err(EngineError::Invariant(_))));
        assert!(rec.claim.is_none(), "a refused claim holds nothing");
    }

    #[test]
    fn a_claim_on_an_unraised_point_is_not_found() {
        let mut rec = ActionPointRecord::default();
        let err = rec.claim(
            command_claim(ClaimCommand::Abort, "planner-1", "a"),
            &ANY_LIVE,
            NOW,
        );
        assert!(matches!(err, Err(EngineError::NotFound(_))));
    }

    #[test]
    fn release_with_the_token_audits_the_failure_and_frees_the_point() {
        let mut rec = proposed_record();
        let token = hold(&mut rec, ClaimCommand::Promote);
        assert!(rec.release(&token, "governed write-back failed: timeout".to_owned()));

        assert!(rec.claim.is_none());
        assert_eq!(
            rec.state(),
            Some(ActionPointState::Proposed),
            "state unchanged"
        );
        let entry = rec.audit.last().expect("an audit entry");
        assert_eq!(entry.kind, Some(AuditKind::CommandFailed));
        assert_eq!(entry.state, ActionPointState::Proposed);
        assert_eq!(entry.reviewer, Some(ReviewerId("planner-1".to_owned())));
        assert_eq!(
            entry.note.as_deref(),
            Some("governed write-back failed: timeout")
        );
        // A later command can claim the point again.
        hold(&mut rec, ClaimCommand::Promote);
    }

    #[test]
    fn release_with_a_wrong_token_is_a_no_op() {
        let mut rec = proposed_record();
        hold(&mut rec, ClaimCommand::Promote);
        let entries = rec.audit.len();
        assert!(!rec.release("not-the-token", "late".to_owned()));
        assert!(rec.claim.is_some(), "the held claim stays");
        assert_eq!(rec.audit.len(), entries, "nothing audited");
        // Idempotent with no claim held either.
        let mut free = proposed_record();
        assert!(!free.release("token-1", "late".to_owned()));
    }

    #[test]
    fn a_lapsed_claim_is_taken_over_and_the_takeover_is_audited() {
        let mut rec = proposed_record();
        hold(&mut rec, ClaimCommand::Promote);
        rec.claim(
            command_claim(ClaimCommand::Abort, "planner-2", "b"),
            &[ActionPointState::Proposed],
            NOW + HOLD_SECS,
        )
        .expect("a claim past the deadline takes over");

        assert_eq!(rec.claim.as_ref().map(|c| c.token.as_str()), Some("b"));
        let entry = rec.audit.last().expect("an audit entry");
        assert_eq!(entry.kind, Some(AuditKind::ClaimTakenOver));
        assert_eq!(entry.reviewer, Some(ReviewerId("planner-2".to_owned())));
        let note = entry.note.as_deref().expect("a note");
        assert!(note.contains("promote by planner-1"), "{note}");
        // The old holder's token is refused.
        let err = rec.promote_plan("token-1", promotion(), empty_plan(), None);
        assert!(matches!(err, Err(EngineError::Invariant(_))));
    }

    #[test]
    fn recording_commands_refuse_a_missing_or_wrong_token() {
        // promote_plan: no claim, then a wrong token.
        let mut rec = proposed_record();
        assert!(
            rec.promote_plan("token-1", promotion(), empty_plan(), None)
                .is_err()
        );
        hold(&mut rec, ClaimCommand::Promote);
        assert!(
            rec.promote_plan("wrong", promotion(), empty_plan(), None)
                .is_err()
        );
        assert_eq!(rec.state(), Some(ActionPointState::Proposed));
        rec.promote_plan("token-1", promotion(), empty_plan(), None)
            .expect("the holder records");
        assert!(rec.claim.is_none(), "recording clears the claim");

        // reverse: no claim, then a wrong token.
        let reversal = || Reversal {
            action_point: ap_id(),
            correlation_id: CorrelationId("undo/x".to_owned()),
            reviewer: ReviewerId("planner-1".to_owned()),
            reason: "why".to_owned(),
        };
        assert!(rec.reverse("token-1", reversal()).is_err());
        hold(&mut rec, ClaimCommand::Reverse);
        assert!(rec.reverse("wrong", reversal()).is_err());
        assert_eq!(rec.state(), Some(ActionPointState::Promoted));

        // abort: no claim, then a wrong token.
        let mut open = ActionPointRecord::default();
        open_fresh(&mut open, 1);
        let reviewer = || ReviewerId("planner-1".to_owned());
        assert!(open.abort("token-1", reviewer(), None).is_err());
        hold(&mut open, ClaimCommand::Abort);
        assert!(open.abort("wrong", reviewer(), None).is_err());
        assert_eq!(open.state(), Some(ActionPointState::Open));
        open.abort("token-1", reviewer(), Some("why".to_owned()))
            .expect("the holder aborts");
        assert!(open.claim.is_none());
    }

    #[test]
    fn expiry_waits_for_a_held_claim_and_proceeds_once_it_lapses() {
        let mut rec = proposed_record();
        hold(&mut rec, ClaimCommand::Promote);
        assert_eq!(
            rec.expire(NOW + 100),
            ExpireOutcome::Deferred {
                retry_after_secs: HOLD_SECS - 100
            }
        );
        assert_eq!(rec.state(), Some(ActionPointState::Proposed), "not expired");

        // The command records: the re-armed timer then finds a resolved point.
        rec.promote_plan("token-1", promotion(), empty_plan(), None)
            .expect("promote");
        assert_eq!(rec.expire(NOW + 200), ExpireOutcome::NotPending);
        assert_eq!(rec.state(), Some(ActionPointState::Promoted));

        // A lapsed claim does not defer expiry, and is cleared with it.
        let mut abandoned = proposed_record();
        hold(&mut abandoned, ClaimCommand::Promote);
        assert_eq!(
            abandoned.expire(NOW + HOLD_SECS),
            ExpireOutcome::Expired { left: None }
        );
        assert!(abandoned.claim.is_none());
    }

    #[test]
    fn a_legacy_record_without_a_claim_still_deserializes() {
        let legacy = r#"{
            "point": {
                "id": "action-point/sale.order/42",
                "subject": "https://ekg.example/id/sale.order/42",
                "state": "proposed",
                "reason": "shortfall",
                "proposal": null
            },
            "last_revision": 1,
            "promotion": null,
            "audit": [
                { "action_point": "action-point/sale.order/42", "state": "open", "reviewer": null }
            ]
        }"#;
        let rec: ActionPointRecord =
            serde_json::from_str(legacy).expect("a pre-claim record still deserializes");
        assert!(rec.claim.is_none());
        assert_eq!(rec.audit[0].kind, None);
        // And the absent fields stay absent on the way back out.
        let out = serde_json::to_string(&rec).expect("serialize");
        assert!(
            !out.contains("\"claim\"") && !out.contains("\"kind\""),
            "{out}"
        );
    }

    fn portfolio(n: &str) -> PortfolioId {
        PortfolioId(format!("portfolio/{n}"))
    }

    #[test]
    fn set_portfolio_records_membership_and_returns_the_portfolio_left() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        assert_eq!(
            rec.set_portfolio(Some(portfolio("a")), NOW),
            None,
            "none left"
        );
        assert_eq!(rec.portfolio, Some(portfolio("a")));
        assert_eq!(
            rec.set_portfolio(Some(portfolio("a")), NOW),
            None,
            "unchanged: nothing left"
        );
        assert_eq!(
            rec.set_portfolio(Some(portfolio("b")), NOW),
            Some(portfolio("a")),
            "regrouped: the previous portfolio is left"
        );
        assert_eq!(
            rec.set_portfolio(None, NOW),
            Some(portfolio("b")),
            "alone again"
        );
        assert_eq!(rec.portfolio, None);
        // Proposed and under review still await oversight.
        rec.attach_proposal(proposal(vec![]), &[], &[])
            .expect("attach");
        assert_eq!(rec.set_portfolio(Some(portfolio("c")), NOW), None);
        assert_eq!(rec.portfolio, Some(portfolio("c")));
    }

    #[test]
    fn a_members_auto_promote_claim_is_refused() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        let audited = rec.audit.len();
        let err = rec
            .claim(
                command_claim(ClaimCommand::AutoPromote, "policy:auto", "a"),
                &[ActionPointState::Proposed],
                NOW,
            )
            .expect_err("a member is never auto-promoted alone");
        let EngineError::Invariant(why) = err else {
            panic!("a refused claim is an invariant error, got {err:?}");
        };
        assert!(why.contains("portfolio/a"), "names the portfolio: {why}");
        assert!(rec.claim.is_none(), "a refused claim holds nothing");
        assert_eq!(rec.audit.len(), audited, "a refused claim audits nothing");
        assert_eq!(rec.state(), Some(ActionPointState::Proposed));
    }

    #[test]
    fn a_non_members_auto_promote_claim_is_granted() {
        let mut rec = proposed_record();
        rec.claim(
            command_claim(ClaimCommand::AutoPromote, "policy:auto", "a"),
            &[ActionPointState::Proposed],
            NOW,
        )
        .expect("a point in no portfolio is auto-promoted");
        assert_eq!(rec.claim.as_ref().map(|c| c.token.as_str()), Some("a"));
    }

    #[test]
    fn a_members_human_promote_claim_is_granted() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        rec.claim(
            command_claim(ClaimCommand::Promote, "planner-1", "a"),
            &[ActionPointState::Proposed],
            NOW,
        )
        .expect("a human (or the portfolio's promotion) may still promote a member");
        assert_eq!(rec.claim.as_ref().map(|c| c.token.as_str()), Some("a"));
    }

    #[test]
    fn set_portfolio_is_a_no_op_outside_oversight() {
        let mut none = ActionPointRecord::default();
        assert_eq!(none.set_portfolio(Some(portfolio("a")), NOW), None);
        assert_eq!(none.portfolio, None, "no point, no membership");

        let mut aborted = ActionPointRecord::default();
        open_fresh(&mut aborted, 1);
        stop(&mut aborted, "planner-1").expect("abort");
        assert_eq!(aborted.set_portfolio(Some(portfolio("a")), NOW), None);
        assert_eq!(aborted.portfolio, None, "a resolved point joins nothing");
    }

    #[test]
    fn a_claimed_point_keeps_its_portfolio_until_the_claim_lapses() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        hold(&mut rec, ClaimCommand::Promote);
        // Held: a regroup (or a release to stand alone) is frozen.
        assert_eq!(rec.set_portfolio(Some(portfolio("b")), NOW + 1), None);
        assert_eq!(rec.set_portfolio(None, NOW + HOLD_SECS - 1), None);
        assert_eq!(rec.portfolio, Some(portfolio("a")), "membership frozen");
        // Lapsed: the claim freezes nothing, and the point regroups.
        assert_eq!(
            rec.set_portfolio(Some(portfolio("b")), NOW + HOLD_SECS),
            Some(portfolio("a"))
        );
        assert_eq!(rec.portfolio, Some(portfolio("b")));
    }

    #[test]
    fn a_released_claim_unfreezes_membership() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        let token = hold(&mut rec, ClaimCommand::Promote);
        assert_eq!(rec.set_portfolio(Some(portfolio("b")), NOW), None);
        assert!(rec.release(&token, "write-back failed".to_owned()));
        assert_eq!(
            rec.set_portfolio(Some(portfolio("b")), NOW),
            Some(portfolio("a")),
            "the next pass regroups the point"
        );
    }

    fn via(n: &str) -> PromotedVia {
        PromotedVia {
            portfolio: portfolio(n),
            generation: 2,
            rank_key: "00000~abc".to_owned(),
        }
    }

    #[test]
    fn a_member_promoted_via_its_portfolio_does_not_supersede_it() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        let token = hold(&mut rec, ClaimCommand::Promote);
        let left = rec
            .promote_plan(&token, promotion(), empty_plan(), Some(via("a")))
            .expect("promote");
        assert_eq!(left, None, "the portfolio records the promotion itself");
        assert_eq!(rec.portfolio, None, "the member still leaves it");
        assert_eq!(rec.promoted_via, Some(via("a")));
        let note = rec
            .audit
            .last()
            .and_then(|e| e.note.as_deref())
            .expect("a promoted note");
        assert_eq!(
            note,
            "promoted as part of portfolio portfolio/a (generation 2), option 00000~abc: \
             shortfall confirmed; expedite approved"
        );

        // A via naming another portfolio still supersedes the one the point was in.
        let mut other = proposed_record();
        other.set_portfolio(Some(portfolio("b")), NOW);
        let token = hold(&mut other, ClaimCommand::Promote);
        let left = other
            .promote_plan(&token, promotion(), empty_plan(), Some(via("a")))
            .expect("promote");
        assert_eq!(left, Some(portfolio("b")));
    }

    #[test]
    fn a_member_promoted_on_its_own_still_supersedes_its_portfolio() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        let token = hold(&mut rec, ClaimCommand::Promote);
        let left = rec
            .promote_plan(&token, promotion(), empty_plan(), None)
            .expect("promote");
        assert_eq!(left, Some(portfolio("a")));
        assert_eq!(rec.promoted_via, None);
        assert_eq!(
            rec.audit.last().and_then(|e| e.note.as_deref()),
            Some("shortfall confirmed; expedite approved"),
            "the note is the rationale alone"
        );
    }

    /// A promoted record whose executed plan has `n` receipts.
    fn promoted_with(n: usize) -> ActionPointRecord {
        let mut rec = proposed_record();
        let token = hold(&mut rec, ClaimCommand::Promote);
        let plan = ExecutionPlan {
            actions: vec![action_on("expedite_po", subject()); n],
            receipts: vec![receipt(); n],
        };
        rec.promote_plan(&token, promotion(), plan, None)
            .expect("promote");
        rec
    }

    fn progress(reversed: &[usize], refused: &[usize]) -> ReverseProgress {
        ReverseProgress {
            reversed: reversed.iter().copied().collect(),
            refused: refused
                .iter()
                .map(|&index| RefusedReverse {
                    index,
                    reason: format!("expedite_po on so/42 (source changed {index})"),
                })
                .collect(),
        }
    }

    #[test]
    fn a_partial_reverse_is_audited_stays_promoted_and_releases_the_claim() {
        let mut rec = promoted_with(2);
        let token = hold(&mut rec, ClaimCommand::Reverse);
        rec.record_partial_reverse(&token, progress(&[1], &[0]))
            .expect("recorded");
        assert_eq!(rec.state(), Some(ActionPointState::Promoted));
        assert!(rec.claim.is_none(), "the claim is released");
        assert_eq!(rec.reverse_progress, progress(&[1], &[0]));
        let entry = rec.audit.last().expect("an entry");
        assert_eq!(entry.kind, Some(AuditKind::PartiallyReversed));
        assert_eq!(entry.state, ActionPointState::Promoted);
        assert_eq!(entry.reviewer, Some(ReviewerId("planner-1".to_owned())));
        assert_eq!(
            entry.note.as_deref(),
            Some(
                "partially reversed: 1 of 2 action(s) reversed, 1 remain unreversed: \
                 expedite_po on so/42 (source changed 0)"
            )
        );
        // A later undo can claim again and finish.
        let token = hold(&mut rec, ClaimCommand::Reverse);
        rec.reverse(
            &token,
            Reversal {
                action_point: ap_id(),
                correlation_id: CorrelationId("undo/x".to_owned()),
                reviewer: ReviewerId("planner-1".to_owned()),
                reason: "why".to_owned(),
            },
        )
        .expect("reversed");
        assert_eq!(rec.state(), Some(ActionPointState::Reversed));
    }

    #[test]
    fn a_partial_reverse_needs_the_claim_and_a_promoted_point() {
        let mut rec = promoted_with(2);
        assert!(
            rec.record_partial_reverse("token-1", progress(&[1], &[0]))
                .is_err(),
            "no claim"
        );
        hold(&mut rec, ClaimCommand::Reverse);
        assert!(
            rec.record_partial_reverse("wrong", progress(&[1], &[0]))
                .is_err()
        );
        assert!(rec.reverse_progress.is_empty(), "nothing recorded");

        let mut proposed = proposed_record();
        let token = hold(&mut proposed, ClaimCommand::Reverse);
        let err = proposed
            .record_partial_reverse(&token, progress(&[], &[0]))
            .expect_err("not promoted");
        assert!(matches!(err, EngineError::Invariant(_)), "{err:?}");
        assert!(proposed.claim.is_some(), "a refused record keeps the claim");
    }

    #[test]
    fn a_partial_reverse_refuses_a_progress_over_another_plan_or_one_that_regresses() {
        let mut rec = promoted_with(2);
        let token = hold(&mut rec, ClaimCommand::Reverse);
        for bad in [
            progress(&[2], &[0]),
            progress(&[0], &[5]),
            progress(&[0], &[0]),
        ] {
            let err = rec
                .record_partial_reverse(&token, bad.clone())
                .expect_err("invalid progress");
            assert!(matches!(err, EngineError::Invariant(_)), "{bad:?}: {err:?}");
        }
        rec.record_partial_reverse(&token, progress(&[1], &[0]))
            .expect("valid");
        let token = hold(&mut rec, ClaimCommand::Reverse);
        let err = rec
            .record_partial_reverse(&token, progress(&[0], &[1]))
            .expect_err("loses receipt 1");
        assert!(matches!(err, EngineError::Invariant(_)), "{err:?}");
    }

    #[test]
    fn progress_after_an_outcome_accumulates_reversed_and_replaces_refused() {
        let first = ReverseProgress::default().after(&ReverseOutcome {
            reversed: vec![2],
            refused: vec![(1, "a".to_owned()), (0, "b".to_owned())],
        });
        assert_eq!(first, {
            let mut p = progress(&[2], &[]);
            p.refused = vec![
                RefusedReverse {
                    index: 1,
                    reason: "a".to_owned(),
                },
                RefusedReverse {
                    index: 0,
                    reason: "b".to_owned(),
                },
            ];
            p
        });
        let second = first.after(&ReverseOutcome {
            reversed: vec![1],
            refused: vec![(0, "c".to_owned())],
        });
        assert_eq!(second.reversed, [1, 2].into_iter().collect::<BTreeSet<_>>());
        assert_eq!(second.refused.len(), 1);
        second.check(&first, 3).expect("a valid successor");
    }

    #[test]
    fn a_legacy_record_without_progress_or_via_loads_empty() {
        let legacy = r#"{
            "point": {
                "id": "action-point/sale.order/42",
                "subject": "https://ekg.example/id/sale.order/42",
                "state": "promoted",
                "reason": "shortfall",
                "proposal": null
            },
            "last_revision": 1,
            "promotion": null,
            "audit": []
        }"#;
        let rec: ActionPointRecord = serde_json::from_str(legacy).expect("loads");
        assert!(rec.reverse_progress.is_empty());
        assert_eq!(rec.promoted_via, None);
        let out = serde_json::to_string(&rec).expect("serialize");
        assert!(
            !out.contains("reverse_progress") && !out.contains("promoted_via"),
            "{out}"
        );
    }

    #[test]
    fn abort_leaves_the_portfolio_and_returns_it() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.set_portfolio(Some(portfolio("a")), NOW);
        let token = hold(&mut rec, ClaimCommand::Abort);
        let left = rec
            .abort(&token, ReviewerId("planner-1".to_owned()), None)
            .expect("abort");
        assert_eq!(left, Some(portfolio("a")));
        assert_eq!(rec.portfolio, None);
    }

    #[test]
    fn promote_leaves_the_portfolio_and_returns_it() {
        let mut rec = proposed_record();
        rec.set_portfolio(Some(portfolio("a")), NOW);
        let token = hold(&mut rec, ClaimCommand::Promote);
        let left = rec
            .promote_plan(&token, promotion(), empty_plan(), None)
            .expect("promote");
        assert_eq!(left, Some(portfolio("a")));
        assert_eq!(rec.portfolio, None);
        // A refused promote leaves membership alone.
        let mut refused = ActionPointRecord::default();
        open_fresh(&mut refused, 1);
        refused.set_portfolio(Some(portfolio("b")), NOW);
        let token = hold(&mut refused, ClaimCommand::Promote);
        assert!(
            refused
                .promote_plan(&token, promotion(), empty_plan(), None)
                .is_err()
        );
        assert_eq!(refused.portfolio, Some(portfolio("b")));
    }

    #[test]
    fn expire_leaves_the_portfolio_and_names_it() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.set_portfolio(Some(portfolio("a")), NOW);
        assert_eq!(
            rec.expire(NOW),
            ExpireOutcome::Expired {
                left: Some(portfolio("a"))
            }
        );
        assert_eq!(rec.portfolio, None);
    }

    #[test]
    fn a_legacy_record_without_a_portfolio_deserializes_with_none() {
        let legacy = r#"{
            "point": {
                "id": "action-point/sale.order/42",
                "subject": "https://ekg.example/id/sale.order/42",
                "state": "open",
                "reason": "shortfall",
                "proposal": null
            },
            "last_revision": 1,
            "promotion": null,
            "audit": []
        }"#;
        let rec: ActionPointRecord =
            serde_json::from_str(legacy).expect("a pre-portfolio record still deserializes");
        assert_eq!(rec.portfolio, None);
        let out = serde_json::to_string(&rec).expect("serialize");
        assert!(!out.contains("\"portfolio\""), "{out}");
    }

    #[test]
    fn attach_proposal_advances_open_to_proposed_and_audits() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(vec![omission()]), &[omission()], &[])
            .expect("a disclosing proposal attaches");
        assert_eq!(rec.state(), Some(ActionPointState::Proposed));
        assert!(
            rec.point
                .as_ref()
                .and_then(|p| p.proposal.as_ref())
                .is_some()
        );
        let last = rec.audit.last().expect("an audit entry exists");
        assert_eq!(last.state, ActionPointState::Proposed);
    }

    #[test]
    fn attach_records_the_disclosed_omissions_in_the_audit_trail() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(vec![omission()]), &[omission()], &[])
            .expect("a disclosing proposal attaches");

        let entry = rec.audit.last().expect("an audit entry exists");
        assert_eq!(entry.state, ActionPointState::Proposed);
        // The withheld facts are recorded with their policy id (the policy version).
        assert_eq!(entry.omissions, vec![omission()]);
        assert_eq!(
            entry.omissions[0].policy_id,
            ekg_domain::PolicyId("read-policy/demo-v1".to_owned())
        );
        // Tied to the attach by a derived correlation id.
        assert_eq!(
            entry.correlation_id,
            Some(ekg_domain::CorrelationId(
                "attach/action-point/sale.order/42".to_owned()
            ))
        );
    }

    fn slate(recommended_omissions: Vec<ekg_domain::Omission>) -> Slate {
        use ekg_domain::{Plan, ProposedAction, RankedPlan, Score, Trace, TraceStep, TraceTag};
        let step = |tag: TraceTag, rejected: bool| TraceStep {
            tag,
            text: "why".to_owned(),
            rejected,
        };
        let option = RankedPlan {
            plan: Plan {
                actions: vec![ProposedAction {
                    action_type: ekg_domain::ActionType("expedite_po".to_owned()),
                    target: subject(),
                    params: std::collections::BTreeMap::new(),
                }],
            },
            score: Score {
                metrics: std::collections::BTreeMap::from([(
                    "risk_left".to_owned(),
                    "0".to_owned(),
                )]),
                rank_key: "00000".to_owned(),
            },
            recommended: true,
            needs: Vec::new(),
            omissions: recommended_omissions,
            trace: Trace::from_steps(vec![
                step(TraceTag::Graph, false),
                step(TraceTag::Assumption, false),
                step(TraceTag::Simulated, false),
            ]),
        };
        Slate {
            baseline: Score {
                metrics: std::collections::BTreeMap::from([(
                    "risk_left".to_owned(),
                    "1".to_owned(),
                )]),
                rank_key: "00001".to_owned(),
            },
            options: vec![option],
            // A whole-loop trace: the baseline, an admitted candidate and a rejected one,
            // so the audit summary has non-zero simulated, rejected and assumption counts.
            trace: Trace::from_steps(vec![
                step(TraceTag::Simulated, false), // do-nothing baseline
                step(TraceTag::Graph, false),
                step(TraceTag::Assumption, false),
                step(TraceTag::Simulated, false), // admitted candidate
                step(TraceTag::Simulated, true),  // rejected candidate, retained
            ]),
        }
    }

    #[test]
    fn attach_slate_stores_the_slate_on_an_open_point_without_transitioning_it() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_slate(slate(vec![omission()]), "1000".to_owned())
            .expect("a slate attaches to an open point");
        assert_eq!(rec.slate_attached_at.as_deref(), Some("1000"));
        // The slate is stored, and the point stays open (a slate is not a transition).
        assert!(rec.slate.is_some());
        assert_eq!(rec.state(), Some(ActionPointState::Open));
        // The persisted slate round-trips its reasoning trace, including the rejected
        // candidate.
        let stored = rec.slate.as_ref().expect("stored slate");
        assert_eq!(stored.trace.summary.simulated, 3);
        assert_eq!(stored.trace.summary.rejected, 1);
        assert_eq!(stored.trace.summary.open_assumptions, 1);
        // An audit entry records the attach (Art. 12), carrying the recommended
        // option's disclosed omissions and a slate correlation id.
        let entry = rec.audit.last().expect("an audit entry exists");
        assert_eq!(entry.state, ActionPointState::Open);
        assert_eq!(entry.omissions, vec![omission()]);
        // Art. 12/14: the audit note carries the trace's counts.
        let note = entry.note.as_deref().expect("an audit note");
        assert!(
            note.contains("3 simulated"),
            "note carries the trace summary: {note}"
        );
        assert!(
            note.contains("1 rejected"),
            "note surfaces rejected candidates: {note}"
        );
        assert!(
            note.contains("1 open assumption"),
            "note surfaces assumptions: {note}"
        );
        assert_eq!(
            entry.correlation_id,
            Some(ekg_domain::CorrelationId(
                "slate/action-point/sale.order/42".to_owned()
            ))
        );
    }

    #[test]
    fn attach_slate_also_lands_on_an_already_proposed_point() {
        // The slate loop runs off the critical path, so the single-proposal path may
        // have advanced the point to `proposed` before the slate lands. It still
        // attaches, without transitioning the point.
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(vec![omission()]), &[omission()], &[])
            .expect("attach moves it to proposed");
        rec.attach_slate(slate(vec![omission()]), "1000".to_owned())
            .expect("a slate attaches to a proposed point too");
        assert!(rec.slate.is_some());
        assert_eq!(rec.state(), Some(ActionPointState::Proposed));
    }

    #[test]
    fn attach_slate_on_a_resolved_point_is_illegal() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        // Abort the point (a terminal state): a resolved point takes no slate.
        stop(&mut rec, "reviewer-1").expect("abort an open point");
        let err = rec.attach_slate(slate(Vec::new()), "1000".to_owned());
        assert!(matches!(err, Err(EngineError::Invariant(_))));
        assert!(
            rec.slate_attached_at.is_none(),
            "a refused attach stamps nothing"
        );
    }

    #[test]
    fn a_re_attach_overwrites_the_slate_attach_time() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_slate(slate(Vec::new()), "1000".to_owned())
            .expect("first attach");
        rec.attach_slate(slate(Vec::new()), "2000".to_owned())
            .expect("re-attach");
        assert_eq!(rec.slate_attached_at.as_deref(), Some("2000"));
    }

    #[test]
    fn a_record_persisted_before_the_slate_attach_time_loads_without_it() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(
            !json.contains("slate_attached_at"),
            "an absent time stays absent: {json}"
        );
        let loaded: ActionPointRecord = serde_json::from_str(&json).expect("loads");
        assert!(loaded.slate_attached_at.is_none());
    }

    #[test]
    fn attach_with_no_omissions_records_an_empty_audit_field() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(Vec::new()), &[], &[])
            .expect("a proposal with nothing withheld attaches");

        let entry = rec.audit.last().expect("an audit entry exists");
        assert!(entry.omissions.is_empty(), "nothing was withheld");
        // The correlation id still ties the entry to the attach.
        assert_eq!(
            entry.correlation_id,
            Some(ekg_domain::CorrelationId(
                "attach/action-point/sale.order/42".to_owned()
            ))
        );
    }

    #[test]
    fn a_legacy_audit_entry_without_the_new_fields_still_deserializes() {
        // An older persisted record has no `omissions` or `correlation_id` keys; serde
        // defaults must fill them.
        let legacy = r#"{
            "action_point": "action-point/sale.order/42",
            "state": "open",
            "reviewer": null
        }"#;
        let entry: AuditEntry = serde_json::from_str(legacy).expect("legacy entry deserializes");
        assert_eq!(entry.state, ActionPointState::Open);
        assert!(entry.omissions.is_empty());
        assert_eq!(entry.correlation_id, None);
    }

    #[test]
    fn a_record_with_a_pre_trace_slate_still_deserializes() {
        // An older slate (persisted inside `ActionPointRecord.slate` as Restate JSON)
        // has no `trace` key on the slate or its options. The record must still load:
        // the virtual object reads it back via `ctx.get::<Json<ActionPointRecord>>` in
        // every handler, so a missing-field failure would make the point unusable.
        let legacy = r#"{
            "point": {
                "id": "action-point/sale.order/42",
                "subject": "https://ekg.example/id/sale.order/42",
                "state": "open",
                "reason": "resource contention",
                "proposal": null
            },
            "last_revision": 1,
            "promotion": null,
            "receipt": null,
            "slate": {
                "baseline": { "metrics": {}, "rank_key": "00001" },
                "options": [
                    {
                        "plan": { "actions": [] },
                        "score": { "metrics": {}, "rank_key": "00000" },
                        "recommended": true,
                        "needs": [],
                        "omissions": []
                    }
                ]
            },
            "audit": []
        }"#;
        let rec: ActionPointRecord =
            serde_json::from_str(legacy).expect("a pre-trace record still deserializes");
        assert_eq!(rec.state(), Some(ActionPointState::Open));
        let slate = rec.slate.as_ref().expect("slate present");
        assert!(slate.trace.steps.is_empty(), "slate trace defaults empty");
        assert!(slate.options[0].trace.steps.is_empty());
    }

    #[test]
    fn a_proposal_targeting_an_unrelated_entity_is_rejected() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let mut other = proposal(vec![]);
        other.actions[0].target = EntityUri("https://ekg.example/id/sale.order/999".to_owned());
        // Not the subject and not among the grounded members: rejected.
        let err = rec.attach_proposal(other, &[], &[]);
        assert!(
            err.is_err(),
            "a proposal for an unrelated entity is rejected"
        );
        assert_eq!(rec.state(), Some(ActionPointState::Open));
    }

    #[test]
    fn a_proposal_targeting_a_grounded_member_is_accepted() {
        // A point about an order may be remediated by an action on one of its
        // members (e.g. a work order): the target need not equal the subject, but
        // it must be a member the subject's grounding referenced.
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let member = EntityUri("https://ekg.example/id/mrp.workorder/5".to_owned());
        let mut on_member = proposal(vec![]);
        on_member.actions[0].target = member.clone();
        rec.attach_proposal(on_member, &[], std::slice::from_ref(&member))
            .expect("a proposal on a grounded member attaches");
        assert_eq!(rec.state(), Some(ActionPointState::Proposed));
    }

    #[test]
    fn a_multi_action_plan_spanning_the_subject_and_a_grounded_member_attaches() {
        // A plan with one action on the subject and one on a grounded member attaches
        // as one proposal and moves the point to `proposed`.
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let member = EntityUri("https://ekg.example/id/mrp.workorder/5".to_owned());
        let mut plan = proposal(vec![]);
        plan.actions = vec![
            action_on("expedite_po", subject()),
            action_on("reschedule_work_order", member.clone()),
        ];
        rec.attach_proposal(plan, &[], std::slice::from_ref(&member))
            .expect("a multi-action plan over grounded targets attaches");
        assert_eq!(rec.state(), Some(ActionPointState::Proposed));
        let attached = rec
            .point
            .as_ref()
            .and_then(|p| p.proposal.as_ref())
            .expect("the plan is attached");
        assert_eq!(attached.actions.len(), 2, "the whole plan is attached");
    }

    #[test]
    fn a_multi_action_plan_with_one_ungrounded_target_is_refused() {
        // A single ungrounded action refuses the whole plan: the point stays `open`,
        // nothing attached.
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let member = EntityUri("https://ekg.example/id/mrp.workorder/5".to_owned());
        let mut plan = proposal(vec![]);
        plan.actions = vec![
            action_on("expedite_po", subject()),
            action_on(
                "reschedule_work_order",
                EntityUri("https://ekg.example/id/mrp.workorder/999".to_owned()),
            ),
        ];
        let err = rec.attach_proposal(plan, &[], std::slice::from_ref(&member));
        assert!(err.is_err(), "a plan with an ungrounded target is refused");
        assert_eq!(rec.state(), Some(ActionPointState::Open));
        assert!(
            rec.point
                .as_ref()
                .and_then(|p| p.proposal.as_ref())
                .is_none()
        );
    }

    #[test]
    fn a_non_disclosing_proposal_is_rejected_leaving_the_point_open() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        // The grounding hid a Supplier, but the proposal omits it: rejected.
        let err = rec.attach_proposal(proposal(vec![]), &[omission()], &[]);
        assert!(
            err.is_err(),
            "a proposal that hides an omission is rejected"
        );
        assert_eq!(rec.state(), Some(ActionPointState::Open));
        assert!(
            rec.point
                .as_ref()
                .and_then(|p| p.proposal.as_ref())
                .is_none()
        );
    }

    fn promotion() -> Promotion {
        Promotion {
            action_point: ap_id(),
            correlation_id: ekg_domain::CorrelationId("corr/sale.order/42".to_owned()),
            reviewer: ReviewerId("planner-1".to_owned()),
            rationale: "shortfall confirmed; expedite approved".to_owned(),
            context_seen: "evidence + 1 omission".to_owned(),
        }
    }

    fn receipt() -> ExecutionReceipt {
        ExecutionReceipt {
            action: action_on("expedite_po", subject()),
            external_ref: "{\"kind\":\"restore_field\"}".to_owned(),
            source_revision: Revision(1_700_000_000),
            reversible: true,
        }
    }

    #[test]
    fn promoting_a_plan_records_one_oversight_interaction_for_n_actions() {
        use ekg_domain::{ExecutionPlan, ProposedAction};

        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(vec![omission()]), &[omission()], &[])
            .expect("attach");

        // A plan of two actions against the subject, executed into two receipts.
        let action = |verb: &str| ProposedAction {
            action_type: ekg_domain::ActionType(verb.to_owned()),
            target: subject(),
            params: std::collections::BTreeMap::new(),
        };
        let receipt_for = |verb: &str| {
            let mut r = receipt();
            r.action = action(verb);
            r
        };
        let plan_receipts = vec![receipt_for("expedite_po"), receipt_for("re_promise_date")];
        let execution_plan = ExecutionPlan {
            actions: vec![action("expedite_po"), action("re_promise_date")],
            receipts: plan_receipts.clone(),
        };

        let token = hold(&mut rec, ClaimCommand::Promote);
        rec.promote_plan(&token, promotion(), execution_plan, None)
            .expect("promote plan");
        assert_eq!(rec.state(), Some(ActionPointState::Promoted));
        assert!(
            rec.promotion.is_some(),
            "one promotion recorded for the plan"
        );

        // One oversight interaction: a single under_review and promoted pair covers
        // N actions, not one pair per action (EU AI Act Art. 14/12).
        assert_eq!(
            rec.audit
                .iter()
                .filter(|e| e.state == ActionPointState::UnderReview)
                .count(),
            1
        );
        assert_eq!(
            rec.audit
                .iter()
                .filter(|e| e.state == ActionPointState::Promoted)
                .count(),
            1
        );
        // The stored ExecutionPlan (N receipts) is all a later reverse needs.
        let stored = rec.execution_plan.as_ref().expect("execution plan stored");
        assert_eq!(stored.receipts, plan_receipts);
        // The audit walks proposed, under_review, promoted, the decision attributed
        // to the reviewer and carrying their rationale.
        let states: Vec<_> = rec.audit.iter().map(|a| a.state).collect();
        assert!(
            states.ends_with(&[ActionPointState::UnderReview, ActionPointState::Promoted]),
            "audit walks proposed -> under_review -> promoted, got {states:?}"
        );
        assert_eq!(
            rec.audit.last().expect("entry").reviewer,
            Some(ReviewerId("planner-1".to_owned()))
        );
    }

    #[test]
    fn promoting_a_plan_from_a_non_proposed_state_is_refused() {
        use ekg_domain::ExecutionPlan;

        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        // Still Open (no proposal): promote_plan is refused, nothing recorded.
        let token = hold(&mut rec, ClaimCommand::Promote);
        let err = rec.promote_plan(
            &token,
            promotion(),
            ExecutionPlan {
                actions: Vec::new(),
                receipts: Vec::new(),
            },
            None,
        );
        assert!(err.is_err(), "cannot promote a plan with no proposal");
        assert_eq!(rec.state(), Some(ActionPointState::Open));
        assert!(rec.promotion.is_none() && rec.execution_plan.is_none());
    }

    #[test]
    fn reverse_moves_promoted_to_reversed_and_audits_the_reviewer() {
        use ekg_domain::ExecutionPlan;

        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        rec.attach_proposal(proposal(vec![omission()]), &[omission()], &[])
            .expect("attach");
        let token = hold(&mut rec, ClaimCommand::Promote);
        rec.promote_plan(
            &token,
            promotion(),
            ExecutionPlan {
                actions: Vec::new(),
                receipts: Vec::new(),
            },
            None,
        )
        .expect("promote");
        assert_eq!(rec.state(), Some(ActionPointState::Promoted));

        // A governed undo: reverse the promoted change, recording who and why (Art. 12).
        let token = hold(&mut rec, ClaimCommand::Reverse);
        rec.reverse(
            &token,
            Reversal {
                action_point: ap_id(),
                correlation_id: CorrelationId("undo/action-point/sale.order/42".to_owned()),
                reviewer: ReviewerId("planner-1".to_owned()),
                reason: "supplier confirmation fell through".to_owned(),
            },
        )
        .expect("a promoted change can be reversed");

        assert_eq!(rec.state(), Some(ActionPointState::Reversed));
        let last = rec.audit.last().expect("an audit entry exists");
        assert_eq!(last.state, ActionPointState::Reversed);
        assert_eq!(last.reviewer, Some(ReviewerId("planner-1".to_owned())));
        assert_eq!(
            last.note.as_deref(),
            Some("supplier confirmation fell through")
        );
    }

    #[test]
    fn reverse_on_a_non_promoted_point_is_refused() {
        // Only a promoted change can be reversed: an open/proposed point has
        // committed nothing to undo, so the reverse is refused and the state is unchanged.
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        let token = hold(&mut rec, ClaimCommand::Reverse);
        let err = rec.reverse(
            &token,
            Reversal {
                action_point: ap_id(),
                correlation_id: CorrelationId("undo/x".to_owned()),
                reviewer: ReviewerId("planner-1".to_owned()),
                reason: String::new(),
            },
        );
        assert!(matches!(err, Err(EngineError::Invariant(_))));
        assert_eq!(rec.state(), Some(ActionPointState::Open));
    }

    #[test]
    fn attach_proposal_on_a_non_open_point_is_illegal() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        stop(&mut rec, "planner-1").expect("abort ok");
        let err = rec.attach_proposal(proposal(vec![]), &[], &[]);
        assert!(err.is_err(), "cannot propose on an aborted point");
        assert_eq!(rec.state(), Some(ActionPointState::Aborted));
    }

    #[test]
    fn open_raises_a_point_in_open_with_one_audit_entry() {
        let mut rec = ActionPointRecord::default();
        assert_eq!(open_fresh(&mut rec, 1), OpenOutcome::Opened);
        assert_eq!(rec.state(), Some(ActionPointState::Open));
        assert_eq!(rec.audit.len(), 1);
        assert_eq!(rec.audit[0].state, ActionPointState::Open);
        assert_eq!(rec.audit[0].reviewer, None);
    }

    #[test]
    fn redelivered_drift_retains_one_open_point() {
        let mut rec = ActionPointRecord::default();
        assert_eq!(open_fresh(&mut rec, 1), OpenOutcome::Opened);
        // A newer event for a still-open subject: retained, not duplicated.
        assert_eq!(open_fresh(&mut rec, 2), OpenOutcome::RetainedExisting);
        assert_eq!(rec.audit.len(), 1);
        assert_eq!(rec.state(), Some(ActionPointState::Open));
    }

    #[test]
    fn stale_revision_event_is_ignored() {
        let mut rec = ActionPointRecord::default();
        assert_eq!(open_fresh(&mut rec, 5), OpenOutcome::Opened);
        // A revision not greater than the last applied is ignored, nothing mutated.
        assert_eq!(open_fresh(&mut rec, 5), OpenOutcome::IgnoredStale);
        assert_eq!(open_fresh(&mut rec, 3), OpenOutcome::IgnoredStale);
        assert_eq!(rec.audit.len(), 1);
        assert_eq!(rec.last_revision, Some(Revision(5)));
    }

    #[test]
    fn abort_moves_open_to_aborted_and_audits_the_reviewer() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        stop(&mut rec, "planner-1").expect("open is abortable");
        assert_eq!(rec.state(), Some(ActionPointState::Aborted));
        let last = rec.audit.last().expect("an audit entry exists");
        assert_eq!(last.state, ActionPointState::Aborted);
        assert_eq!(last.reviewer, Some(ReviewerId("planner-1".to_owned())));
    }

    #[test]
    fn a_second_abort_is_refused_leaving_state_unchanged() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        stop(&mut rec, "planner-1").expect("first abort ok");
        let err = stop(&mut rec, "planner-1");
        assert!(err.is_err(), "aborting a terminal point is illegal");
        assert_eq!(rec.state(), Some(ActionPointState::Aborted));
        // No audit entry was appended for the refused transition.
        assert_eq!(
            rec.audit
                .iter()
                .filter(|e| e.state == ActionPointState::Aborted)
                .count(),
            1
        );
    }

    #[test]
    fn promoting_an_open_point_is_illegal() {
        assert!(!can_transition(
            ActionPointState::Open,
            ActionPointState::Promoted
        ));
        assert!(can_transition(
            ActionPointState::Open,
            ActionPointState::Aborted
        ));
        assert!(can_transition(
            ActionPointState::UnderReview,
            ActionPointState::Promoted
        ));
        assert!(!can_transition(
            ActionPointState::Aborted,
            ActionPointState::Expired
        ));
    }

    #[test]
    fn an_unactioned_point_expires_but_a_resolved_one_does_not() {
        let mut pending = ActionPointRecord::default();
        open_fresh(&mut pending, 1);
        assert_eq!(pending.expire(NOW), ExpireOutcome::Expired { left: None });
        assert_eq!(pending.state(), Some(ActionPointState::Expired));

        let mut resolved = ActionPointRecord::default();
        open_fresh(&mut resolved, 1);
        stop(&mut resolved, "planner-1").expect("abort ok");
        assert_eq!(resolved.expire(NOW), ExpireOutcome::NotPending);
        assert_eq!(resolved.state(), Some(ActionPointState::Aborted));
    }

    #[test]
    fn drift_after_a_terminal_episode_opens_a_fresh_point() {
        let mut rec = ActionPointRecord::default();
        open_fresh(&mut rec, 1);
        stop(&mut rec, "planner-1").expect("abort ok");
        // A newer event after the episode closed opens a new point.
        assert_eq!(open_fresh(&mut rec, 2), OpenOutcome::Opened);
        assert_eq!(rec.state(), Some(ActionPointState::Open));
    }

    // --- outcomes ---

    /// A proposed point with a slate attached at `1700000000`, promoted with `plan`.
    fn promoted_outcome_record(plan: ExecutionPlan, via: Option<PromotedVia>) -> ActionPointRecord {
        let mut rec = proposed_record();
        rec.slate_attached_at = Some("1700000000".to_owned());
        let token = hold(&mut rec, ClaimCommand::Promote);
        rec.promote_plan(&token, promotion(), plan, via)
            .expect("promote");
        rec
    }

    #[test]
    fn a_point_awaiting_oversight_has_no_outcome() {
        assert_eq!(ActionPointRecord::default().outcome(NOW), None);
        assert_eq!(proposed_record().outcome(NOW), None);
    }

    #[test]
    fn a_promotion_reports_the_reviewer_the_executed_actions_and_the_slate_time() {
        let plan = ExecutionPlan {
            actions: vec![action_on("expedite_po", subject())],
            receipts: vec![receipt()],
        };
        let outcome = promoted_outcome_record(plan.clone(), None)
            .outcome(NOW)
            .expect("an outcome");
        assert_eq!(outcome.action_point, ap_id());
        assert_eq!(outcome.point_seq, 1);
        assert_eq!(outcome.state, ActionPointState::Promoted);
        assert_eq!(outcome.at, NOW);
        assert_eq!(outcome.reviewer, Some(ReviewerId("planner-1".to_owned())));
        assert_eq!(outcome.actions, plan.actions);
        assert_eq!(outcome.promoted_via, None);
        assert_eq!(outcome.slate_attached_at.as_deref(), Some("1700000000"));
    }

    #[test]
    fn a_promotion_through_a_portfolio_names_it() {
        let outcome = promoted_outcome_record(empty_plan(), Some(via("a")))
            .outcome(NOW)
            .expect("an outcome");
        assert_eq!(outcome.promoted_via, Some(via("a")));
    }

    #[test]
    fn an_abort_reports_the_reviewer_and_no_actions() {
        let mut rec = proposed_record();
        stop(&mut rec, "planner-2").expect("abort");
        let outcome = rec.outcome(NOW).expect("an outcome");
        assert_eq!(outcome.state, ActionPointState::Aborted);
        assert_eq!(outcome.reviewer, Some(ReviewerId("planner-2".to_owned())));
        assert!(outcome.actions.is_empty());
    }

    #[test]
    fn an_expiry_reports_no_reviewer() {
        let mut rec = proposed_record();
        assert!(matches!(rec.expire(NOW), ExpireOutcome::Expired { .. }));
        let outcome = rec.outcome(NOW).expect("an outcome");
        assert_eq!(outcome.state, ActionPointState::Expired);
        assert_eq!(outcome.reviewer, None);
    }

    #[test]
    fn a_reversal_reports_who_reversed_and_not_the_promoted_actions() {
        let plan = ExecutionPlan {
            actions: vec![action_on("expedite_po", subject())],
            receipts: vec![receipt()],
        };
        let mut rec = promoted_outcome_record(plan, Some(via("a")));
        let token = hold(&mut rec, ClaimCommand::Reverse);
        rec.reverse(
            &token,
            Reversal {
                action_point: ap_id(),
                correlation_id: ekg_domain::CorrelationId("corr/sale.order/42".to_owned()),
                reviewer: ReviewerId("planner-3".to_owned()),
                reason: "wrong date".to_owned(),
            },
        )
        .expect("reverse");
        let outcome = rec.outcome(NOW + 1).expect("an outcome");
        assert_eq!(outcome.state, ActionPointState::Reversed);
        assert_eq!(outcome.at, NOW + 1);
        assert_eq!(outcome.reviewer, Some(ReviewerId("planner-3".to_owned())));
        assert!(outcome.actions.is_empty());
        assert_eq!(outcome.promoted_via, None);
    }

    #[test]
    fn a_subjects_points_share_an_id_and_are_told_apart_by_their_sequence() {
        let mut rec = ActionPointRecord::default();
        assert_eq!(rec.points_opened, 0);
        open_fresh(&mut rec, 1);
        // A redelivered event opens nothing.
        open_fresh(&mut rec, 1);
        assert_eq!(rec.points_opened, 1);
        stop(&mut rec, "planner-1").expect("abort");
        let first = rec.outcome(NOW).expect("an outcome");

        assert_eq!(open_fresh(&mut rec, 2), OpenOutcome::Opened);
        assert_eq!(rec.points_opened, 2);
        stop(&mut rec, "planner-1").expect("abort");
        let second = rec.outcome(NOW).expect("an outcome");
        assert_eq!(first.action_point, second.action_point);
        assert_eq!((first.point_seq, second.point_seq), (1, 2));
    }

    #[test]
    fn a_record_stored_before_points_were_counted_still_reads() {
        let mut json = serde_json::to_value(proposed_record()).expect("json");
        json.as_object_mut()
            .expect("a record")
            .remove("points_opened");
        let mut older: ActionPointRecord = serde_json::from_value(json).expect("it reads");
        assert_eq!(older.points_opened, 0);
        stop(&mut older, "planner-1").expect("abort");
        open_fresh(&mut older, 2);
        assert_eq!(older.points_opened, 1, "counted from the next point on");
    }

    #[test]
    fn an_outcome_is_snake_case_json_and_leaves_out_what_is_absent() {
        let mut rec = proposed_record();
        assert!(matches!(rec.expire(NOW), ExpireOutcome::Expired { .. }));
        let json = serde_json::to_value(rec.outcome(NOW).expect("an outcome")).expect("json");
        assert_eq!(
            json,
            serde_json::json!({
                "action_point": "action-point/sale.order/42",
                "point_seq": 1,
                "state": "expired",
                "at": NOW,
            })
        );
        let back: PointOutcome = serde_json::from_value(json).expect("back");
        assert_eq!(back.reviewer, None);
    }
}
