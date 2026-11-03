//! The durable Restate wrappers: the action-point virtual object (keyed by
//! subject), the portfolio virtual object (keyed by portfolio id) and the drift-monitor
//! service.
//!
//! All are thin: the rules live in [`crate::lifecycle`] and [`crate::monitor`], which
//! are unit-tested without a runtime. Here they gain per-key exclusivity
//! (idempotency), durable state (the lifecycle) and a durable timer (escalation).
//! Runtime behaviour is covered by the gated integration test. The macros go on
//! `impl` blocks (the trait-based API is deprecated in the SDK); each generates a
//! `<Name>Client` for object-to-object calls.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ekg_domain::{
    ActionPointId, ActionPointState, CouplingLink, EngineError, EntityUri, ExecutionPlan,
    GroundedProposal, PortfolioId, Promotion, RdfChanged, Reversal, ReviewerId, Revision, Slate,
    WorldModel,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::lifecycle::{
    ActionPointRecord, ClaimCommand, CommandClaim, ExpireOutcome, OpenOutcome, PointOutcome,
    PromotedVia, ReverseProgress,
};
use crate::monitor::{self, NotifyOutcome};
use crate::portfolio::{
    FormOutcome, FormReceipt, PlannedPortfolio, PortfolioOutcome, PortfolioPromotion,
    PortfolioRecord, portfolio_id,
};
use crate::rule::{CouplingRule, RiskRule};

/// The state key under which each object stores its [`ActionPointRecord`].
const RECORD_KEY: &str = "record";
/// The handler of the outcome listener that receives a [`PointOutcome`].
pub const OUTCOME_HANDLER: &str = "decide";
/// The handler of the outcome listener that receives a [`PortfolioOutcome`].
pub const PORTFOLIO_OUTCOME_HANDLER: &str = "decide_portfolio";
/// Default escalation window before an un-actioned point expires.
pub const DEFAULT_ESCALATION: Duration = Duration::from_secs(48 * 60 * 60);
/// The margin a deferred expiry waits past a held claim's deadline, so the re-armed
/// timer fires after the claim has lapsed or been cleared.
const EXPIRY_RETRY_MARGIN_SECS: u64 = 5;

/// The single input to the `open` handler (handlers take exactly one argument).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRequest {
    /// Deterministic reason the monitor is opening the point.
    pub reason: String,
    /// The `RdfChanged` revision that triggered the evaluation (revision guard).
    pub revision: u64,
}

/// The input to the `claim` handler: a governed command asks to hold the point before
/// its write-back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// The command claiming the point.
    pub command: ClaimCommand,
    /// Who issued it.
    pub reviewer: String,
    /// The states the command may start from; a point in any other state is refused.
    pub expected_state: Vec<ActionPointState>,
    /// How long the claim holds before the next command may take it over, in seconds.
    pub deadline_secs: u64,
}

/// The input to the `release` handler: the holder's token and why its write-back
/// failed (recorded in the audit trail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequest {
    /// The token the `claim` handler returned.
    pub token: String,
    /// Why the command failed.
    pub reason: String,
}

/// The input to the `abort` handler: the claim token, the reviewer and their reason
/// (the stop control records who and why).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortRequest {
    /// The token of the claim the abort holds.
    pub token: String,
    /// The reviewer invoking the stop control.
    pub reviewer: String,
    /// Their reason, recorded in the audit trail.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The input to the `promote` handler: the single human promotion and the executed
/// [`ExecutionPlan`] for the governed write-back the caller already ran as one step.
/// A single-action promotion is a 1-action plan through the same path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoteRequest {
    /// The token of the claim the promotion holds.
    pub token: String,
    /// The single human promotion authorizing the whole plan.
    pub promotion: Promotion,
    /// The executed plan with its receipts, kept for a later `reverse_plan` and the audit.
    pub execution_plan: ExecutionPlan,
    /// The portfolio promotion this point is promoted through, when it is a member of its
    /// portfolio's promoted plan. Defaulted: a point promoted on its own sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_via: Option<PromotedVia>,
}

/// The input to the `record_partial_reverse` handler: the undo's claim token and how far
/// the reverse got.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordPartialReverseRequest {
    /// The token of the claim the undo holds.
    pub token: String,
    /// The receipts reversed so far and the ones the last attempt refused.
    pub progress: ReverseProgress,
}

/// The input to the `reverse` handler: the claim token and the governed reversal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseRequest {
    /// The token of the claim the undo holds.
    pub token: String,
    /// Who reversed the plan, and why.
    pub reversal: Reversal,
}

/// The input to the action point's `set_portfolio` handler: the portfolio the monitor's
/// grouping put the point in, or `None` when it is on its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPortfolioRequest {
    /// The portfolio, if any.
    #[serde(default)]
    pub portfolio: Option<PortfolioId>,
}

/// What the action point's `set_portfolio` handler did: the point's portfolio afterwards,
/// and the portfolio it left in this call (if any), so the monitor can report the
/// points a pass released to stand alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPortfolioReceipt {
    /// The point's portfolio after the call.
    #[serde(default)]
    pub portfolio: Option<PortfolioId>,
    /// The portfolio the point left in this call.
    #[serde(default)]
    pub left: Option<PortfolioId>,
}

/// The input to the portfolio's `attach_slate` handler: the joint slate and the generation
/// it was produced for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPortfolioSlate {
    /// The generation the slate was produced for; it must be the portfolio's current one.
    pub generation: u32,
    /// The joint slate (commits nothing).
    pub slate: Slate,
}

/// The input to the portfolio's `form` handler: the members and why they are linked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormRequest {
    /// The member subjects; the object's key must be their [`portfolio_id`].
    pub members: Vec<EntityUri>,
    /// Why they are linked (the coupling reasons), recorded in the audit trail.
    pub reason: String,
    /// Each coupling link with its facts, recorded in the audit trail. Defaulted so a
    /// request without it still forms.
    #[serde(default)]
    pub links: Vec<CouplingLink>,
}

/// The input to the portfolio's `promote` handler: the promotion of one joint-slate
/// option, whose plan the caller already executed as one governed step. The handler
/// stamps when it was promoted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotePortfolioRequest {
    /// The generation the promotion was decided for; it must be the portfolio's current one.
    pub generation: u32,
    /// The chosen option's `rank_key`; it must be on the slate for that generation.
    pub rank_key: String,
    /// The human who promoted it.
    pub reviewer: ReviewerId,
    /// Their rationale.
    pub rationale: String,
    /// The executed plan (every receipt of the one write-back).
    pub execution_plan: ExecutionPlan,
}

/// The input to the portfolio's `record_partial_reverse` handler: who ran the undo and how
/// far it got.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordPortfolioPartialReverse {
    /// The human who ran the undo.
    pub reviewer: ReviewerId,
    /// The receipts reversed so far and the ones the last attempt refused.
    pub progress: ReverseProgress,
}

/// The input to the portfolio's `reverse` handler: who undid the promoted plan and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReversePortfolioRequest {
    /// The human who undid it.
    pub reviewer: ReviewerId,
    /// Their reason, recorded in the audit trail.
    pub reason: String,
}

/// The input to the portfolio's `dismiss` handler: the generation the operator saw, who
/// dismissed the plan and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DismissPortfolioRequest {
    /// The generation the dismissal was decided for; it must be the portfolio's current one.
    pub generation: u32,
    /// The human who dismissed it.
    pub reviewer: ReviewerId,
    /// Their reason, recorded in the audit trail.
    pub reason: String,
}

/// The input to the portfolio's `supersede` handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupersedeRequest {
    /// Why the portfolio stopped being current, recorded in the audit trail.
    pub reason: String,
}

/// The action-point virtual object, keyed by the subject entity IRI. One instance
/// exists per subject, so "at most one open action point per subject" and the
/// revision guard hold under concurrent, at-least-once delivery. Holds only its
/// escalation window and its outcome listener, both fixed at deploy, so every handler
/// is deterministic across replay.
#[derive(Clone)]
pub struct ActionPointService {
    escalation: Duration,
    outcome_listener: Option<String>,
}

impl ActionPointService {
    /// Build the object with an escalation window and no outcome listener.
    #[must_use]
    pub const fn new(escalation: Duration) -> Self {
        Self {
            escalation,
            outcome_listener: None,
        }
    }

    /// Report every point's outcome to the virtual object `listener`, which must be
    /// registered with the same runtime: after a point is promoted, aborted, expired or
    /// reversed, its `decide` handler is sent a [`PointOutcome`] under the subject's key.
    #[must_use]
    pub fn with_outcome_listener(mut self, listener: impl Into<String>) -> Self {
        self.outcome_listener = Some(listener.into());
        self
    }

    /// When a listener is set, the time to stamp an outcome with, journaled as `name`.
    /// Without one no step is journaled.
    async fn outcome_time(
        &self,
        ctx: &ObjectContext<'_>,
        name: &str,
    ) -> Result<Option<u64>, TerminalError> {
        match self.outcome_listener {
            Some(_) => now_secs(ctx, name).await.map(Some),
            None => Ok(None),
        }
    }

    /// Send `outcome` to the listener, if both are there. A durable one-way send, ordered
    /// after the caller's state write; the handler does not wait for it.
    fn report(&self, ctx: &ObjectContext<'_>, outcome: Option<PointOutcome>) {
        if let (Some(listener), Some(outcome)) = (&self.outcome_listener, outcome) {
            ctx.request::<_, ()>(
                restate_sdk::context::RequestTarget::object(
                    listener.as_str(),
                    ctx.key(),
                    OUTCOME_HANDLER,
                ),
                Json(outcome),
            )
            .send();
        }
    }
}

impl Default for ActionPointService {
    fn default() -> Self {
        Self::new(DEFAULT_ESCALATION)
    }
}

#[restate_sdk::object]
impl ActionPointService {
    /// Idempotently open a point for this subject at `revision`, recording the
    /// reason. Arms the escalation timer only on a fresh open. Returns the outcome
    /// so the monitor can trigger the proposer once per fresh open.
    #[handler]
    async fn open(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<OpenRequest>,
    ) -> Result<Json<OpenOutcome>, HandlerError> {
        let req = req.into_inner();
        let subject = EntityUri(ctx.key().to_owned());
        let id = ActionPointId(format!("action-point/{}", ctx.key()));
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);

        let outcome = record.open(id, subject, req.reason, Revision(req.revision));
        ctx.set(RECORD_KEY, Json(record));

        // Only a fresh open arms a new escalation timer (a durable delayed
        // self-call); a redelivered or stale event must not re-arm it.
        if outcome == OpenOutcome::Opened {
            ctx.object_client::<ActionPointServiceClient>(ctx.key().to_owned())
                .expire()
                .send_after(self.escalation);
        }
        Ok(Json(outcome))
    }

    /// Claim the point for a governed command before its write-back and return the
    /// claim token. The token and the claim time are journaled, so a replay yields the
    /// same claim. Refused (409) while another command holds the point or when the
    /// point is not in an expected state; 404 when no point was raised.
    #[handler]
    async fn claim(
        &self,
        mut ctx: ObjectContext<'_>,
        req: Json<ClaimRequest>,
    ) -> Result<Json<String>, HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        let now = now_secs(&ctx, "stamp-claimed-at").await?;
        let token = ctx.rand_uuid().to_string();
        let claim = CommandClaim {
            token: token.clone(),
            command: req.command,
            reviewer: ReviewerId(req.reviewer),
            claimed_at: now,
            deadline: now.saturating_add(req.deadline_secs),
        };
        record
            .claim(claim, &req.expected_state, now)
            .map_err(classify_claim)?;
        ctx.set(RECORD_KEY, Json(record));
        Ok(Json(token))
    }

    /// Release a claim after a failed write-back, recording the failed attempt in the
    /// audit trail; the state is unchanged. A token that no longer holds the point is a
    /// no-op, so the call is idempotent.
    #[handler]
    async fn release(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<ReleaseRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        if record.release(&req.token, req.reason) {
            ctx.set(RECORD_KEY, Json(record));
        }
        Ok(())
    }

    /// Abort the point via the stop control, recording the reviewer and reason. The
    /// abort must hold the point's claim.
    #[handler]
    async fn abort(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<AbortRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        let left = record
            .abort(&req.token, ReviewerId(req.reviewer), req.reason)
            .map_err(classify)?;
        let outcome = self
            .outcome_time(&ctx, "stamp-abort")
            .await?
            .and_then(|at| record.outcome(at));
        ctx.set(RECORD_KEY, Json(record));
        leave_portfolio(&ctx, left, "aborted");
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Record the portfolio the monitor's grouping put this point in (`None`: on its own),
    /// and return the point's portfolio afterwards with the one it left. Applies only while
    /// the point awaits oversight and no governed command holds it (otherwise a no-op, and a
    /// later pass regroups it); writes state only when the value changes. The portfolio the
    /// point left, if any, is superseded by a durable one-way send, ordered after the state
    /// write.
    #[handler]
    async fn set_portfolio(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<SetPortfolioRequest>,
    ) -> Result<Json<SetPortfolioReceipt>, HandlerError> {
        let req = req.into_inner();
        let Some(mut record) = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map(Json::into_inner)
        else {
            return Ok(Json(SetPortfolioReceipt::default()));
        };
        // A held claim freezes membership; the clock is journaled so a replay decides
        // the same way.
        let now = now_secs(&ctx, "stamp-regroup-check").await?;
        let before = record.portfolio.clone();
        let left = record.set_portfolio(req.portfolio, now);
        if record.portfolio != before {
            ctx.set(RECORD_KEY, Json(record.clone()));
        }
        leave_portfolio(&ctx, left.clone(), "regrouped");
        Ok(Json(SetPortfolioReceipt {
            portfolio: record.portfolio,
            left,
        }))
    }

    /// Attach a grounded proposal, moving the point from `open` to `proposed`. Enforces
    /// omission disclosure and rejects a proposal on a non-`open` point.
    #[handler]
    async fn attach_proposal(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<GroundedProposal>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        record
            .attach_proposal(req.proposal, &req.grounding_omissions, &req.subject_members)
            .map_err(classify)?;
        ctx.set(RECORD_KEY, Json(record));
        Ok(())
    }

    /// Attach a ranked remediation slate to this point before promotion. The slate
    /// commits nothing and does not transition the point; a human still promotes one
    /// chosen plan as one governed step. Accepts an `open` or `proposed` point (the
    /// slate loop runs off the bridge's critical path, so either may be current);
    /// rejects a slate on a resolved (terminal or `promoted`) point.
    #[handler]
    async fn attach_slate(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<Slate>,
    ) -> Result<(), HandlerError> {
        let slate = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        let attached_at = attached_now(&ctx).await?;
        record.attach_slate(slate, attached_at).map_err(classify)?;
        ctx.set(RECORD_KEY, Json(record));
        Ok(())
    }

    /// Read the ranked slate attached to this point, if any. The reasoning surface
    /// reads it here.
    #[handler]
    async fn slate(&self, ctx: ObjectContext<'_>) -> Result<Json<Option<Slate>>, HandlerError> {
        let record: Option<ActionPointRecord> = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map(Json::into_inner);
        Ok(Json(record.and_then(|r| r.slate)))
    }

    /// Promote a `proposed` point as one governed step: record the single human
    /// promotion and the executed [`ExecutionPlan`], and move the point from `proposed`
    /// through `under_review` to `promoted` with one oversight interaction over the
    /// whole plan (EU AI Act Art. 14 and 12). The caller (the API or the auto-promote
    /// bridge) runs the governed write-back via `execute_plan` and hands in the plan;
    /// this records the outcome. A single-action promotion is a 1-action plan through
    /// this same path. Rejects a promote on a non-`proposed` point, or one whose claim
    /// token does not hold the point.
    #[handler]
    async fn promote(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<PromoteRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        let left = record
            .promote_plan(
                &req.token,
                req.promotion,
                req.execution_plan,
                req.promoted_via,
            )
            .map_err(classify)?;
        // Stamp when the plan was promoted (Unix epoch seconds), so the read `/api`
        // can anchor an auto-applied change's reversal window. The wall clock is
        // non-deterministic, so it is journaled via `ctx.run` and stays stable
        // across replay.
        let promoted_at = promoted_now(&ctx).await?;
        let outcome = record.outcome(promoted_at.parse().unwrap_or_default());
        record.promoted_at = Some(promoted_at);
        ctx.set(RECORD_KEY, Json(record));
        leave_portfolio(&ctx, left, "promoted");
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Reverse a promoted point through its recorded inverse (`promoted` to `reversed`),
    /// recording the reversal in the audit trail. The caller (the `/api` undo command)
    /// has already run the governed reverse write-back over the stored
    /// [`ExecutionPlan`]; this records the outcome, as `promote` does. Rejects a
    /// reverse on a non-`promoted` point, or one whose claim token does not hold the
    /// point.
    #[handler]
    async fn reverse(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<ReverseRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        record.reverse(&req.token, req.reversal).map_err(classify)?;
        let outcome = self
            .outcome_time(&ctx, "stamp-reversal")
            .await?
            .and_then(|at| record.outcome(at));
        ctx.set(RECORD_KEY, Json(record));
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Record an undo that reversed only some of the executed plan's actions: store the
    /// progress so a retry reverses only the rest, audit it (`partially_reversed`), keep the
    /// point `promoted` and release the undo's claim. Rejects (409) a record without the
    /// claim, of a point that is not `promoted`, or a progress over another plan; 404 when
    /// no point was raised.
    #[handler]
    async fn record_partial_reverse(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<RecordPartialReverseRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        record
            .record_partial_reverse(&req.token, req.progress)
            .map_err(classify_claim)?;
        ctx.set(RECORD_KEY, Json(record));
        Ok(())
    }

    /// Read this subject's whole durable [`ActionPointRecord`], if any: the promotion,
    /// the executed plan (with its reversible receipts), the slate, `promoted_at`, and
    /// the audit trail. The read `/api` reads it here to serve a plan's auto-applied
    /// changes and to drive the governed undo; reading commits nothing.
    #[handler]
    async fn record(
        &self,
        ctx: ObjectContext<'_>,
    ) -> Result<Json<Option<ActionPointRecord>>, HandlerError> {
        let record: Option<ActionPointRecord> = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map(Json::into_inner);
        Ok(Json(record))
    }

    /// Escalation-timer callback: expire the point if it is still pending (a no-op
    /// if it was already resolved before the timer fired). While a governed command
    /// holds the point, the timer re-arms itself for just past the claim's deadline
    /// instead, so a point never expires mid-command; the re-armed call applies the
    /// same rule.
    #[handler]
    async fn expire(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        let mut record: ActionPointRecord = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map_or_else(ActionPointRecord::default, Json::into_inner);
        let now = now_secs(&ctx, "stamp-expiry-check").await?;
        let outcome = record.expire(now);
        if let Some(delay) = expiry_retry(&outcome) {
            ctx.object_client::<ActionPointServiceClient>(ctx.key().to_owned())
                .expire()
                .send_after(delay);
        } else {
            let expired = matches!(outcome, ExpireOutcome::Expired { .. });
            let reported = record.outcome(now).filter(|_| expired);
            ctx.set(RECORD_KEY, Json(record));
            self.report(&ctx, reported);
        }
        if let ExpireOutcome::Expired { left } = outcome {
            leave_portfolio(&ctx, left, "expired");
        }
        Ok(())
    }

    /// Read the current action point, if any.
    #[handler]
    async fn get(
        &self,
        ctx: ObjectContext<'_>,
    ) -> Result<Json<Option<ekg_domain::ActionPoint>>, HandlerError> {
        let record: Option<ActionPointRecord> = ctx
            .get::<Json<ActionPointRecord>>(RECORD_KEY)
            .await?
            .map(Json::into_inner);
        Ok(Json(record.and_then(|r| r.point)))
    }
}

/// The portfolio virtual object, keyed by its [`PortfolioId`]: the durable, audited record
/// of a group of coupled action points. It holds only its outcome listener, fixed at deploy,
/// so every handler is deterministic across replay. It commits nothing.
#[derive(Clone, Default)]
pub struct PortfolioService {
    outcome_listener: Option<String>,
}

impl PortfolioService {
    /// Report every portfolio's outcome to the virtual object `listener`, which must be
    /// registered with the same runtime: after a portfolio is promoted, dismissed, superseded
    /// or undone (in whole or in part), its `decide_portfolio` handler is sent a
    /// [`PortfolioOutcome`] under the portfolio's key.
    #[must_use]
    pub fn with_outcome_listener(mut self, listener: impl Into<String>) -> Self {
        self.outcome_listener = Some(listener.into());
        self
    }

    /// The outcome of `record` to report, stamped with a time journaled as `name`. Without a
    /// listener no step is journaled and there is nothing to report.
    async fn outcome(
        &self,
        ctx: &ObjectContext<'_>,
        record: &PortfolioRecord,
        name: &str,
    ) -> Result<Option<PortfolioOutcome>, TerminalError> {
        match self.outcome_listener {
            Some(_) => Ok(record.outcome(now_secs(ctx, name).await?)),
            None => Ok(None),
        }
    }

    /// Send `outcome` to the listener, if both are there. A durable one-way send, ordered
    /// after the caller's state write; the handler does not wait for it, so a listener that
    /// cannot be reached fails nothing here.
    fn report(&self, ctx: &ObjectContext<'_>, outcome: Option<PortfolioOutcome>) {
        if let (Some(listener), Some(outcome)) = (&self.outcome_listener, outcome) {
            ctx.request::<_, ()>(
                restate_sdk::context::RequestTarget::object(
                    listener.as_str(),
                    ctx.key(),
                    PORTFOLIO_OUTCOME_HANDLER,
                ),
                Json(outcome),
            )
            .send();
        }
    }
}

#[restate_sdk::object]
impl PortfolioService {
    /// Form this portfolio of `members`: create it (generation 1), leave an active one with
    /// the same members unchanged, or reactivate a superseded one under a new generation.
    /// Refused (terminal, 400) when the key is not the members' [`portfolio_id`]. Returns
    /// the outcome with the generation the portfolio is at afterwards.
    #[handler]
    async fn form(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<FormRequest>,
    ) -> Result<Json<FormReceipt>, HandlerError> {
        let req = req.into_inner();
        let id = portfolio_id(&req.members);
        if id.0 != ctx.key() {
            return Err(TerminalError::new_with_code(
                400,
                format!(
                    "portfolio {} is not the id of its members ({id})",
                    ctx.key()
                ),
            )
            .into());
        }
        let mut record: PortfolioRecord = ctx
            .get::<Json<PortfolioRecord>>(RECORD_KEY)
            .await?
            .map_or_else(PortfolioRecord::default, Json::into_inner);
        let outcome = record.form(id, req.members, req.reason, req.links);
        let generation = record.generation;
        if outcome != FormOutcome::Unchanged {
            ctx.set(RECORD_KEY, Json(record));
        }
        Ok(Json(FormReceipt {
            outcome,
            generation,
        }))
    }

    /// Attach the joint slate produced for `generation`, auditing it. Refused (terminal,
    /// 409) when the portfolio is not active or `generation` is not its current one, so a
    /// stale slate never lands; 404 when the portfolio was never formed. Commits nothing:
    /// it changes no membership, transitions no point and writes no system of record.
    #[handler]
    async fn attach_slate(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<AttachPortfolioSlate>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record = formed_portfolio(&ctx).await?;
        let attached_at = attached_now(&ctx).await?;
        record
            .attach_slate(req.generation, req.slate, attached_at)
            .map_err(classify_claim)?;
        ctx.set(RECORD_KEY, Json(record));
        Ok(())
    }

    /// Record the promotion of one joint-slate option across every member, auditing it, and
    /// move the portfolio from `active` to `promoted`. The caller has already run the one
    /// governed write-back; this is its final gate. Refused (terminal, 409) unless the
    /// portfolio is active at the promotion's generation with that option on its slate; 404
    /// when the portfolio was never formed.
    #[handler]
    async fn promote(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<PromotePortfolioRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record = formed_portfolio(&ctx).await?;
        let promoted_at = promoted_now(&ctx).await?;
        record
            .promote(PortfolioPromotion {
                generation: req.generation,
                rank_key: req.rank_key,
                reviewer: req.reviewer,
                rationale: req.rationale,
                execution_plan: req.execution_plan,
                promoted_at,
                progress: ReverseProgress::default(),
            })
            .map_err(classify_claim)?;
        let outcome = self
            .outcome(&ctx, &record, "stamp-promoted-outcome")
            .await?;
        ctx.set(RECORD_KEY, Json(record));
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Record an undo that reversed only some of the promoted plan's actions, so a retry
    /// reverses only the rest; the portfolio stays promoted. Refused (409) unless the
    /// portfolio is promoted and the progress is valid over its plan; 404 when it was never
    /// formed.
    #[handler]
    async fn record_partial_reverse(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<RecordPortfolioPartialReverse>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record = formed_portfolio(&ctx).await?;
        record
            .record_partial_reverse(req.reviewer, req.progress)
            .map_err(classify_claim)?;
        let outcome = self
            .outcome(&ctx, &record, "stamp-partial-reverse-outcome")
            .await?;
        ctx.set(RECORD_KEY, Json(record));
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Record the undo of the promoted plan, moving the portfolio from `promoted` to
    /// `reversed`. Refused (409) unless it is promoted; 404 when it was never formed.
    #[handler]
    async fn reverse(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<ReversePortfolioRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record = formed_portfolio(&ctx).await?;
        record
            .reverse(req.reviewer, req.reason)
            .map_err(classify_claim)?;
        let outcome = self
            .outcome(&ctx, &record, "stamp-reversed-outcome")
            .await?;
        ctx.set(RECORD_KEY, Json(record));
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Record the dismissal of this portfolio's plan, moving it from `active` to `dismissed`
    /// with an audit entry carrying the reviewer, the reason, the generation and the
    /// members. The caller has claimed every member and aborts them afterwards; this writes
    /// no system of record. Refused (terminal, 409) unless the portfolio is active at
    /// `generation`; 404 when it was never formed.
    #[handler]
    async fn dismiss(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<DismissPortfolioRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let mut record = formed_portfolio(&ctx).await?;
        record
            .dismiss(req.generation, req.reviewer, req.reason)
            .map_err(classify_claim)?;
        let outcome = self
            .outcome(&ctx, &record, "stamp-dismissed-outcome")
            .await?;
        ctx.set(RECORD_KEY, Json(record));
        self.report(&ctx, outcome);
        Ok(())
    }

    /// Supersede this portfolio, recording why. Idempotent: only the first transition is
    /// audited, and a portfolio never formed is left untouched.
    #[handler]
    async fn supersede(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<SupersedeRequest>,
    ) -> Result<(), HandlerError> {
        let req = req.into_inner();
        let Some(mut record) = ctx
            .get::<Json<PortfolioRecord>>(RECORD_KEY)
            .await?
            .map(Json::into_inner)
        else {
            return Ok(());
        };
        if record.supersede(req.reason) {
            let outcome = self
                .outcome(&ctx, &record, "stamp-superseded-outcome")
                .await?;
            ctx.set(RECORD_KEY, Json(record));
            self.report(&ctx, outcome);
        }
        Ok(())
    }

    /// Read this portfolio's durable record, if it was ever formed. Commits nothing.
    #[handler]
    async fn record(
        &self,
        ctx: ObjectContext<'_>,
    ) -> Result<Json<Option<PortfolioRecord>>, HandlerError> {
        Ok(Json(
            ctx.get::<Json<PortfolioRecord>>(RECORD_KEY)
                .await?
                .map(Json::into_inner),
        ))
    }
}

/// The drift monitor: re-evaluates the injected [`RiskRule`]s on each `RdfChanged`
/// and, on a drift verdict, durably opens the subject's action point. With coupling rules
/// injected, it then groups the coupled points awaiting oversight into portfolios.
#[derive(Clone)]
pub struct DriftMonitorService {
    world: Arc<dyn WorldModel>,
    rules: Arc<[RiskRule]>,
    coupling: Arc<[CouplingRule]>,
}

impl DriftMonitorService {
    /// Wire the monitor over its world-model seam and the injected capability rules.
    /// The monitor evaluates whatever rules it is handed and unions their verdicts.
    /// The rule set is shared (`Arc<[RiskRule]>`), so the read side can evaluate the
    /// same rules for its at-risk markers.
    #[must_use]
    pub fn new(world: Arc<dyn WorldModel>, rules: impl Into<Arc<[RiskRule]>>) -> Self {
        Self {
            world,
            rules: rules.into(),
            coupling: Arc::from(Vec::new()),
        }
    }

    /// Group coupled points by the injected coupling `rules` on every pass. The monitor
    /// unions whatever edges the rules yield.
    #[must_use]
    pub fn with_coupling(mut self, rules: impl Into<Arc<[CouplingRule]>>) -> Self {
        self.coupling = rules.into();
        self
    }
}

#[restate_sdk::service]
impl DriftMonitorService {
    /// Handle one world-model change: evaluate risk deterministically and durably
    /// open an action point for every subject the rule currently flags. Returns
    /// the subjects whose points were freshly opened, so the backend chains the
    /// proposal agent once per fresh open (without `ekg-dynamic` depending on
    /// cognition), with the portfolios the pass formed or re-formed and the subjects
    /// it released to stand alone, so the backend plans them.
    ///
    /// A change may open more than one point: drift is not local to the changed
    /// entity (a shared inbound shipment's ETA slip shifts every downstream order's
    /// risk), so the monitor re-evaluates the whole world model and opens each
    /// at-risk subject. Opening is idempotent per subject (the object de-dupes and
    /// the revision guard drops redelivery), so this stays safe under at-least-once.
    #[handler]
    async fn notify(
        &self,
        ctx: Context<'_>,
        event: Json<RdfChanged>,
    ) -> Result<Json<NotifyOutcome>, HandlerError> {
        let event = event.into_inner();

        // Deterministic detection, journaled so a crash resumes rather than
        // re-queries. Every injected rule is fanned out concurrently inside this
        // one durable step, so the joined union is a single journal entry. The
        // LLM is never consulted here.
        let verdicts = ctx
            .run(|| async {
                monitor::evaluate(self.world.as_ref(), &self.rules, &event)
                    .await
                    .map(Json::from)
                    .map_err(classify)
            })
            .name("evaluate-risk")
            .await?
            .into_inner();

        // Durably open each at-risk subject's point (the object serializes and
        // de-dupes per subject). Collect only the freshly opened subjects so the
        // backend triggers the proposer once per episode, never on a redelivered
        // or stale event that retained an open point.
        let mut freshly_opened = Vec::new();
        let flagged = monitor::flagged_subjects(&verdicts);
        for verdict in verdicts {
            let subject = verdict.subject.clone();
            let outcome = ctx
                .object_client::<ActionPointServiceClient>(subject.0.clone())
                .open(Json(OpenRequest {
                    reason: verdict.reason,
                    revision: event.revision.0,
                }))
                .call()
                .await?
                .into_inner();
            if outcome == OpenOutcome::Opened {
                freshly_opened.push(subject);
            }
        }
        let (formations, singles) = if self.coupling.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            self.group(&ctx, &flagged).await?
        };
        Ok(Json(monitor::notify_outcome(
            freshly_opened,
            &formations,
            &singles,
        )))
    }
}

impl DriftMonitorService {
    /// Group the flagged subjects whose points await oversight into portfolios. Commits
    /// nothing: it transitions no point, changes no verdict and writes no system of record.
    /// It only records membership. A failed coupling read skips grouping for this pass
    /// (logged); the next pass repairs it.
    ///
    /// Returns each planned portfolio with its formation receipt, and each single with the
    /// portfolio its assignment left.
    async fn group(
        &self,
        ctx: &Context<'_>,
        flagged: &[EntityUri],
    ) -> Result<Grouped, HandlerError> {
        // The edges, in one journaled step (the rules fanned out inside it). A read failure
        // is journaled as "no edges known", never retried into a stuck pass.
        let edges = ctx
            .run(|| async {
                Ok::<_, HandlerError>(Json(
                    monitor::coupling_edges(self.world.as_ref(), &self.coupling).await,
                ))
            })
            .name("evaluate-coupling")
            .await?
            .into_inner();
        let Some(edges) = edges else {
            return Ok((Vec::new(), Vec::new()));
        };

        // Each flagged subject's point state, read durably from its object.
        let mut states = BTreeMap::new();
        for subject in flagged {
            let point = ctx
                .object_client::<ActionPointServiceClient>(subject.0.clone())
                .get()
                .call()
                .await?
                .into_inner();
            states.insert(subject.clone(), point.map(|p| p.state));
        }

        let plan = monitor::plan_grouping(flagged, &states, &edges);
        let mut formations = Vec::with_capacity(plan.portfolios.len());
        for portfolio in plan.portfolios {
            let reason = portfolio.reason();
            let receipt = ctx
                .object_client::<PortfolioServiceClient>(portfolio.id.0.clone())
                .form(Json(FormRequest {
                    members: portfolio.members.clone(),
                    reason,
                    links: portfolio.coupling(),
                }))
                .call()
                .await?
                .into_inner();
            for member in &portfolio.members {
                self.assign(ctx, member.clone(), Some(portfolio.id.clone()))
                    .await?;
            }
            formations.push((portfolio, receipt));
        }
        let mut singles = Vec::with_capacity(plan.singles.len());
        for single in plan.singles {
            let receipt = self.assign(ctx, single.clone(), None).await?;
            singles.push((single, receipt.left));
        }
        Ok((formations, singles))
    }

    /// Record `subject`'s portfolio on its point (a durable call; the point supersedes the
    /// portfolio it leaves).
    async fn assign(
        &self,
        ctx: &Context<'_>,
        subject: EntityUri,
        portfolio: Option<PortfolioId>,
    ) -> Result<SetPortfolioReceipt, HandlerError> {
        Ok(ctx
            .object_client::<ActionPointServiceClient>(subject.0)
            .set_portfolio(Json(SetPortfolioRequest { portfolio }))
            .call()
            .await?
            .into_inner())
    }
}

/// What a grouping pass did: each planned portfolio with its formation receipt, and each
/// single with the portfolio its assignment left.
type Grouped = (
    Vec<(PlannedPortfolio, FormReceipt)>,
    Vec<(EntityUri, Option<PortfolioId>)>,
);

/// The portfolio object's record, or a 404 when it was never formed.
async fn formed_portfolio(ctx: &ObjectContext<'_>) -> Result<PortfolioRecord, HandlerError> {
    ctx.get::<Json<PortfolioRecord>>(RECORD_KEY)
        .await?
        .map(Json::into_inner)
        .ok_or_else(|| never_formed(ctx.key()))
}

/// The 404 for a portfolio that was never formed.
fn never_formed(key: &str) -> HandlerError {
    classify_claim(EngineError::NotFound(format!(
        "portfolio {key} was never formed"
    )))
}

/// Capture the current wall clock as Unix epoch seconds (a string), journaled via
/// `ctx.run` so the non-deterministic read happens once and stays stable across replay.
async fn promoted_now(ctx: &ObjectContext<'_>) -> Result<String, TerminalError> {
    Ok(now_secs(ctx, "stamp-promoted-at").await?.to_string())
}

/// Capture when a slate is attached, as Unix epoch seconds (a string), journaled like
/// [`promoted_now`].
async fn attached_now(ctx: &ObjectContext<'_>) -> Result<String, TerminalError> {
    Ok(now_secs(ctx, "stamp-slate-attached-at").await?.to_string())
}

/// When an expiry must fire again: after a deferral, just past the held claim's
/// deadline; otherwise never (the point expired or was already resolved).
fn expiry_retry(outcome: &ExpireOutcome) -> Option<Duration> {
    match outcome {
        ExpireOutcome::Deferred { retry_after_secs } => Some(Duration::from_secs(
            retry_after_secs.saturating_add(EXPIRY_RETRY_MARGIN_SECS),
        )),
        ExpireOutcome::Expired { .. } | ExpireOutcome::NotPending => None,
    }
}

/// Supersede the portfolio `left` (if any) that this point just left, with the reason
/// `member <subject> <how>`. This is a durable one-way send, ordered after the caller's
/// state write. Nothing is sent when the point belonged to no portfolio.
fn leave_portfolio(ctx: &ObjectContext<'_>, left: Option<PortfolioId>, how: &str) {
    if let Some(portfolio) = left {
        ctx.object_client::<PortfolioServiceClient>(portfolio.0)
            .supersede(Json(SupersedeRequest {
                reason: format!("member {} {how}", ctx.key()),
            }))
            .send();
    }
}

/// Read the wall-clock as Unix epoch seconds in a journaled `ctx.run` step named
/// `name`, so it is stable across replay.
async fn now_secs(ctx: &ObjectContext<'_>, name: &str) -> Result<u64, TerminalError> {
    ctx.run(|| async {
        Ok::<_, HandlerError>(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
    })
    .name(name)
    .await
}

/// Map a claim refusal to a coded terminal error, so the ingress answers with the
/// status the `/api` passes on: 404 when no point was raised, 409 when another command
/// holds the point or it is in the wrong state.
fn classify_claim(err: EngineError) -> HandlerError {
    match &err {
        EngineError::NotFound(_) => TerminalError::new_with_code(404, err.to_string()).into(),
        EngineError::Invariant(_) => TerminalError::new_with_code(409, err.to_string()).into(),
        _ => classify(err),
    }
}

/// Map an engine error to a Restate error: a `Backend` transport fault is
/// retryable (let `ctx.run` retry); anything else is terminal (it will not fix
/// itself on retry). Mirrors the ingest worker's classification.
fn classify(err: EngineError) -> HandlerError {
    match &err {
        EngineError::Backend(_) => err.into(),
        _ => TerminalError::new(err.to_string()).into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use ekg_domain::{ActionPointState, Proposal, ProposedAction};

    use super::*;

    #[test]
    fn expire_on_a_claimed_point_leaves_it_proposed_and_re_arms_past_the_deadline() {
        let mut record = ActionPointRecord::default();
        record.open(
            ActionPointId("action-point/s".to_owned()),
            EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            "shortfall".to_owned(),
            Revision(1),
        );
        let action = ProposedAction {
            action_type: ekg_domain::ActionType("expedite_po".to_owned()),
            target: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            params: std::collections::BTreeMap::new(),
        };
        record
            .attach_proposal(
                Proposal::single("why".to_owned(), action, Vec::new()),
                &[],
                &[],
            )
            .expect("attach");
        record
            .claim(
                CommandClaim {
                    token: "t".to_owned(),
                    command: ClaimCommand::Promote,
                    reviewer: ReviewerId("planner-1".to_owned()),
                    claimed_at: 1_000,
                    deadline: 1_600,
                },
                &[ActionPointState::Proposed],
                1_000,
            )
            .expect("claim");

        let retry = expiry_retry(&record.expire(1_100));
        assert_eq!(record.state(), Some(ActionPointState::Proposed));
        assert_eq!(
            retry,
            Some(Duration::from_secs(500 + EXPIRY_RETRY_MARGIN_SECS)),
            "the timer re-arms for just past the claim's deadline"
        );
        // Once the claim is released, the re-armed timer expires the point.
        assert!(record.release("t", "write-back failed".to_owned()));
        assert_eq!(expiry_retry(&record.expire(1_605)), None);
        assert_eq!(record.state(), Some(ActionPointState::Expired));
    }

    #[test]
    fn a_refused_portfolio_slate_attach_maps_to_409() {
        let mut record = PortfolioRecord::default();
        let members = vec![
            EntityUri("https://ekg.example/id/a/1".to_owned()),
            EntityUri("https://ekg.example/id/b/1".to_owned()),
        ];
        record.form(
            portfolio_id(&members),
            members,
            "linked".to_owned(),
            Vec::new(),
        );
        let slate = Slate {
            baseline: ekg_domain::Score {
                metrics: BTreeMap::new(),
                rank_key: "9".to_owned(),
            },
            options: Vec::new(),
            trace: ekg_domain::Trace::default(),
        };
        let refused = record
            .attach_slate(2, slate, "1000".to_owned())
            .map_err(classify_claim)
            .expect_err("a stale generation is refused");
        assert!(format!("{refused:?}").contains("409"), "{refused:?}");
    }

    #[test]
    fn a_stale_generation_portfolio_promote_maps_to_409_and_never_formed_to_404() {
        let mut record = PortfolioRecord::default();
        let members = vec![
            EntityUri("https://ekg.example/id/a/1".to_owned()),
            EntityUri("https://ekg.example/id/b/1".to_owned()),
        ];
        record.form(
            portfolio_id(&members),
            members,
            "linked".to_owned(),
            Vec::new(),
        );
        let refused = record
            .promote(PortfolioPromotion {
                generation: 2,
                rank_key: "0".to_owned(),
                reviewer: ReviewerId("planner-1".to_owned()),
                rationale: "why".to_owned(),
                execution_plan: ExecutionPlan {
                    actions: Vec::new(),
                    receipts: Vec::new(),
                },
                promoted_at: "1000".to_owned(),
                progress: ReverseProgress::default(),
            })
            .map_err(classify_claim)
            .expect_err("a stale generation is refused");
        assert!(format!("{refused:?}").contains("409"), "{refused:?}");
        let missing = never_formed("portfolio/0123456789ab");
        assert!(format!("{missing:?}").contains("404"), "{missing:?}");
    }

    #[test]
    fn a_dismiss_records_its_audit_and_a_refused_one_maps_to_409() {
        let mut record = PortfolioRecord::default();
        let members = vec![
            EntityUri("https://ekg.example/id/a/1".to_owned()),
            EntityUri("https://ekg.example/id/b/1".to_owned()),
        ];
        record.form(
            portfolio_id(&members),
            members.clone(),
            "linked".to_owned(),
            Vec::new(),
        );
        let req: DismissPortfolioRequest = serde_json::from_value(serde_json::json!({
            "generation": 1, "reviewer": "planner-1", "reason": "not needed"
        }))
        .expect("the handler's input deserializes");
        record
            .dismiss(req.generation, req.reviewer, req.reason)
            .map_err(classify_claim)
            .expect("dismiss");
        assert_eq!(record.state, crate::portfolio::PortfolioState::Dismissed);
        let last = record.audit.last().expect("audited");
        assert_eq!(last.event, crate::portfolio::PortfolioEvent::Dismissed);
        assert_eq!(last.reviewer, Some(ReviewerId("planner-1".to_owned())));
        assert_eq!(last.members, members);
        let again = record
            .dismiss(1, ReviewerId("planner-1".to_owned()), "again".to_owned())
            .map_err(classify_claim)
            .expect_err("a second dismiss is refused");
        assert!(format!("{again:?}").contains("409"), "{again:?}");
    }

    #[test]
    fn a_claim_refusal_carries_its_http_code() {
        let refused = classify_claim(EngineError::Invariant("held".to_owned()));
        let missing = classify_claim(EngineError::NotFound("none".to_owned()));
        assert!(format!("{refused:?}").contains("409"), "{refused:?}");
        assert!(format!("{missing:?}").contains("404"), "{missing:?}");
    }
}
