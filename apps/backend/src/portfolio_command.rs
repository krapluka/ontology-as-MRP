//! The portfolio commands: promote one option of a portfolio's joint slate, undo that
//! promotion, or dismiss the plan, each as one governed step across every member, behind
//! `/api/portfolios/{promote,undo,abort}`.
//!
//! They follow the point commands' claim protocol ([`crate::command`]): they validate their
//! oversight evidence (422), claim every member point in sorted order (refusing the whole
//! command with 409 and releasing every claim taken when one member cannot be claimed), then
//! run one governed write-back and record it. A held claim freezes the member's portfolio
//! membership, so the portfolio checked before the claims cannot be superseded before the
//! write-back.
//!
//! A promotion executes the chosen option's actions as one all-or-nothing plan, records
//! the portfolio's promotion first (its final gate and the undo's source of truth), then each
//! member's, with the portfolio named. An undo reverses what an earlier partial undo left, and
//! either records every member and the portfolio reversed, or records the progress on the
//! portfolio, releases every claim and answers 502 naming what remains. A dismissal writes
//! nothing to any system of record: it records the portfolio dismissed first, then aborts each
//! member, the portfolio named in its audit.

// Docs mention product names (Restate, Odoo, EU AI Act).
#![allow(clippy::doc_markdown)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::post};
use ekg_domain::{
    ActionExecutor, ActionPointId, ActionPointState, CorrelationId, ExecutionPlan, Plan,
    PromotedPlan, Promotion, ProposedAction, Reversal, ReviewerId, Revision,
};
use ekg_dynamic::{
    ClaimCommand, DismissPortfolioRequest, PortfolioRecord, PortfolioState,
    PromotePortfolioRequest, PromotedVia, RecordPortfolioPartialReverse, ReversePortfolioRequest,
    ReverseProgress,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::action_point::{ApiState, Problem, undo_window_open, validate_evidence};
use crate::command::{
    CommandLedger, ingress_call, partial_reverse_problem, service_url, write_back_problem,
};

/// Mount the portfolio command routes on `/api`.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/portfolios/promote", post(promote))
        .route("/api/portfolios/undo", post(undo))
        .route("/api/portfolios/abort", post(abort))
        .with_state(state)
}

// --- request DTOs ---

/// Promote one option of a portfolio's joint slate across every member.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PortfolioPromoteInput {
    /// The portfolio id (`portfolio/<id>`).
    portfolio: String,
    /// The generation the reviewer decided for; it must be the portfolio's current one.
    generation: u32,
    /// The `rank_key` of the joint-slate option the reviewer selected. Opaque ordering id,
    /// never a capability or metric name.
    option_rank_key: String,
    /// The reviewer promoting (identity; no auth yet). Non-empty, and never a `policy:`
    /// identity.
    reviewer: String,
    /// The reviewer's rationale (non-rubber-stamping). Non-empty.
    rationale: String,
    /// A record of the context the reviewer was shown.
    #[serde(default)]
    context_seen: String,
}

/// Undo a promoted portfolio within its reversal window.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PortfolioUndoInput {
    /// The portfolio id (`portfolio/<id>`).
    portfolio: String,
    /// The reviewer requesting the undo. Non-empty, and never a `policy:` identity.
    reviewer: String,
    /// Their reason, recorded in the audit trail. Required and non-empty.
    reason: String,
}

/// Dismiss an active portfolio's plan: abort every member, write nothing to Odoo.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PortfolioAbortInput {
    /// The portfolio id (`portfolio/<id>`).
    portfolio: String,
    /// The generation the reviewer saw; it must be the portfolio's current one.
    generation: u32,
    /// The reviewer dismissing (identity; no auth yet). Non-empty, and never a `policy:`
    /// identity.
    reviewer: String,
    /// Their reason, recorded in the audit trail. Required and non-empty.
    reason: String,
}

// --- the portfolio ledger ---

/// The durable portfolio record as a command sees it: read it and record a promotion, a
/// partial reverse or a reverse.
#[async_trait]
pub trait PortfolioLedger: Send + Sync {
    /// Read the portfolio's durable record (`None` when it was never formed).
    async fn record(&self, id: &str) -> Result<Option<PortfolioRecord>, Problem>;

    /// Record the promotion (the portfolio's final gate: 409 unless it is active at the
    /// promotion's generation with the option on its slate).
    async fn promote(&self, id: &str, request: &PromotePortfolioRequest) -> Result<(), Problem>;

    /// Record a partial reverse's progress.
    async fn record_partial_reverse(
        &self,
        id: &str,
        request: &RecordPortfolioPartialReverse,
    ) -> Result<(), Problem>;

    /// Record the reverse (`promoted` to `reversed`).
    async fn reverse(&self, id: &str, request: &ReversePortfolioRequest) -> Result<(), Problem>;

    /// Record the dismissal (`active` to `dismissed`; 409 unless it is active at the
    /// dismissal's generation).
    async fn dismiss(&self, id: &str, request: &DismissPortfolioRequest) -> Result<(), Problem>;
}

/// The [`PortfolioLedger`] over the durable `PortfolioService` object, reached through the
/// Restate ingress.
#[derive(Clone)]
pub struct IngressPortfolioLedger {
    http: reqwest::Client,
    ingress_url: String,
}

impl IngressPortfolioLedger {
    /// A ledger over the ingress at `ingress_url`.
    #[must_use]
    pub const fn new(http: reqwest::Client, ingress_url: String) -> Self {
        Self { http, ingress_url }
    }

    /// Record `what` through `handler`; a refusal is a 409, a missing portfolio a 404.
    async fn record_outcome(
        &self,
        id: &str,
        handler: &str,
        what: &str,
        body: &impl Serialize,
    ) -> Result<(), Problem> {
        let body = serde_json::to_string(body).map_err(|e| Problem::bad_gateway(e.to_string()))?;
        let url = service_url(&self.ingress_url, "PortfolioService", id, handler)?;
        ingress_call(&self.http, url, Some(body))
            .await?
            .map(drop)
            .map_err(|(status, detail)| match status {
                404 => Problem::not_found(detail),
                409 => Problem::conflict(format!("{what} rejected by the portfolio: {detail}")),
                other => Problem::bad_gateway(format!(
                    "{what} on the portfolio returned {other}: {detail}"
                )),
            })
    }
}

#[async_trait]
impl PortfolioLedger for IngressPortfolioLedger {
    async fn record(&self, id: &str) -> Result<Option<PortfolioRecord>, Problem> {
        let url = service_url(&self.ingress_url, "PortfolioService", id, "record")?;
        let text = ingress_call(&self.http, url, None)
            .await?
            .map_err(|(status, _)| {
                Problem::bad_gateway(format!("portfolio record read returned {status}"))
            })?;
        serde_json::from_str::<Option<PortfolioRecord>>(&text)
            .map_err(|e| Problem::bad_gateway(format!("undecodable portfolio record: {e}")))
    }

    async fn promote(&self, id: &str, request: &PromotePortfolioRequest) -> Result<(), Problem> {
        self.record_outcome(id, "promote", "promotion", request)
            .await
    }

    async fn record_partial_reverse(
        &self,
        id: &str,
        request: &RecordPortfolioPartialReverse,
    ) -> Result<(), Problem> {
        self.record_outcome(id, "record_partial_reverse", "partial reverse", request)
            .await
    }

    async fn reverse(&self, id: &str, request: &ReversePortfolioRequest) -> Result<(), Problem> {
        self.record_outcome(id, "reverse", "reverse", request).await
    }

    async fn dismiss(&self, id: &str, request: &DismissPortfolioRequest) -> Result<(), Problem> {
        self.record_outcome(id, "dismiss", "dismissal", request)
            .await
    }
}

// --- handlers ---

impl ApiState {
    /// The portfolio ledger over this state's Restate ingress.
    fn portfolios(&self) -> IngressPortfolioLedger {
        IngressPortfolioLedger::new(self.http.clone(), self.ingress_url.clone())
    }
}

/// `POST /api/portfolios/promote`: promote one option of a portfolio's joint slate as one
/// governed step across every member: claim every member, run one all-or-nothing
/// write-back, record the portfolio's promotion and then each member's.
///
/// # Errors
/// [`Problem`] (422) for missing oversight evidence; (404) for an unknown portfolio; (409)
/// when the portfolio is not active, the generation is stale, it has no joint slate for it,
/// the option is not on it, or a member is not proposed or is held by another command,
/// writing nothing; (502) if the write-back or a durable record fails.
#[utoipa::path(
    post,
    path = "/api/portfolios/promote",
    operation_id = "promote_portfolio",
    tag = "portfolios",
    request_body = PortfolioPromoteInput,
    responses(
        (status = 200, description = "Promoted across every member; the portfolio record",
         body = serde_json::Value),
        (status = 404, description = "No such portfolio", body = Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "The portfolio is not active, the generation is stale, it \
         has no joint slate for that generation, the option is not on it, or a member is not \
         proposed or another command is in progress on it; nothing is written",
         body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Missing oversight evidence: an empty reviewer or \
         rationale, or a policy: reviewer", body = Problem,
         content_type = "application/problem+json"),
        (status = 502, description = "Governed write-back or ingress failure", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn promote(
    State(st): State<ApiState>,
    Json(input): Json<PortfolioPromoteInput>,
) -> Result<Json<PortfolioRecord>, Problem> {
    let portfolios = st.portfolios();
    run_promote(&st.ledger(), &portfolios, st.executor.as_ref(), &input).await?;
    read_after(&portfolios, &input.portfolio, "promote").await
}

/// `POST /api/portfolios/undo`: undo a promoted portfolio's plan as one governed step,
/// within the reversal window: claim every member, reverse what is left of the executed plan
/// and record every member and the portfolio reversed or record the progress and answer
/// 502 naming what remains.
///
/// # Errors
/// [`Problem`] (422) for missing oversight evidence; (404) for an unknown portfolio; (409)
/// when it is not promoted, its window closed, or a member is held by another command,
/// reversing nothing; (502) if the reverse is partial (naming how many actions remain) or a
/// durable record fails.
#[utoipa::path(
    post,
    path = "/api/portfolios/undo",
    operation_id = "undo_portfolio",
    tag = "portfolios",
    request_body = PortfolioUndoInput,
    responses(
        (status = 200, description = "Reversed across every member; the portfolio record",
         body = serde_json::Value),
        (status = 404, description = "No such portfolio", body = Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "The portfolio is not promoted, its reversal window \
         closed, or another command is in progress on a member; nothing is reversed",
         body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Missing oversight evidence: an empty reviewer or reason, \
         or a policy: reviewer", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The reverse was partial (the reason states how many \
         actions remain; a later undo resumes) or an ingress failure", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn undo(
    State(st): State<ApiState>,
    Json(input): Json<PortfolioUndoInput>,
) -> Result<Json<PortfolioRecord>, Problem> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let portfolios = st.portfolios();
    run_undo(
        &st.ledger(),
        &portfolios,
        st.executor.as_ref(),
        &input,
        st.undo_window,
        now,
    )
    .await?;
    read_after(&portfolios, &input.portfolio, "undo").await
}

/// `POST /api/portfolios/abort`: dismiss an active portfolio's plan as one governed step:
/// claim every member, record the portfolio dismissed, then abort each member. Writes nothing
/// to any system of record.
///
/// # Errors
/// [`Problem`] (422) for missing oversight evidence; (404) for an unknown portfolio; (409)
/// when the portfolio is not active, the generation is stale, or a member is held by another
/// command, changing nothing; (502) when a member's abort does not record after the portfolio
/// was dismissed (its claim is held; a retry aborts only the members still awaiting).
#[utoipa::path(
    post,
    path = "/api/portfolios/abort",
    operation_id = "abort_portfolio",
    tag = "portfolios",
    request_body = PortfolioAbortInput,
    responses(
        (status = 200, description = "Dismissed; every member aborted. The portfolio record",
         body = serde_json::Value),
        (status = 404, description = "No such portfolio", body = Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "The portfolio is not active, the generation is stale, or \
         another command is in progress on a member; nothing is changed", body = Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Missing oversight evidence: an empty reviewer or reason, \
         or a policy: reviewer", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "A member's abort did not record after the dismissal (a \
         retry aborts only the members still awaiting) or an ingress failure", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn abort(
    State(st): State<ApiState>,
    Json(input): Json<PortfolioAbortInput>,
) -> Result<Json<PortfolioRecord>, Problem> {
    let portfolios = st.portfolios();
    run_abort(&st.ledger(), &portfolios, &input).await?;
    read_after(&portfolios, &input.portfolio, "abort").await
}

/// The portfolio's record after a command recorded it.
async fn read_after(
    portfolios: &dyn PortfolioLedger,
    id: &str,
    command: &str,
) -> Result<Json<PortfolioRecord>, Problem> {
    portfolios
        .record(id)
        .await?
        .map(Json)
        .ok_or_else(|| Problem::bad_gateway(format!("portfolio missing after {command}")))
}

// --- the commands ---

/// The promote command: validate, check, claim every member, re-check, write back once,
/// record the portfolio and then every member.
async fn run_promote(
    ledger: &dyn CommandLedger,
    portfolios: &dyn PortfolioLedger,
    executor: &dyn ActionExecutor,
    input: &PortfolioPromoteInput,
) -> Result<(), Problem> {
    validate_evidence(&input.reviewer, "rationale", &input.rationale)?;
    let id = input.portfolio.as_str();
    let (generation, key) = (input.generation, input.option_rank_key.as_str());
    let record = read_portfolio(portfolios, id).await?;
    promotable(&record, generation, key)?;

    let claims = claim_members(
        ledger,
        &record.members,
        ClaimCommand::Promote,
        &input.reviewer,
        &[ActionPointState::Proposed],
        &format!("portfolio {id} promotion"),
    )
    .await?;

    // The claims freeze every member's membership, so the portfolio cannot have been
    // superseded since the check above. Re-check anyway, and fail if it did not hold.
    let actions = match read_portfolio(portfolios, id)
        .await
        .and_then(|record| promotable(&record, generation, key))
    {
        Ok(actions) => actions,
        Err(problem) => return Err(release_all(ledger, &claims, problem).await),
    };

    let correlation_id = CorrelationId(format!("promote-portfolio/{id}/g{generation}"));
    let context = format!("portfolio {id} generation {generation} option rank_key={key}");
    let context_seen = if input.context_seen.is_empty() {
        context
    } else {
        format!("{}; {context}", input.context_seen)
    };
    let promotion_of = |member: &str| Promotion {
        action_point: point_id(member),
        correlation_id: correlation_id.clone(),
        reviewer: ReviewerId(input.reviewer.clone()),
        rationale: input.rationale.clone(),
        context_seen: context_seen.clone(),
    };
    // One plan, one write-back. The plan-level promotion is audit metadata for the executor;
    // it names the first member, and every member records its own below.
    let promoted = PromotedPlan {
        plan: Plan { actions },
        promotion: promotion_of(&claims[0].0),
        basis_revision: Revision(0),
    };
    let execution_plan = match executor.execute_plan(&promoted).await {
        Ok(plan) => plan,
        Err(err) => {
            return Err(release_all(ledger, &claims, write_back_problem(&err)).await);
        }
    };

    // The write-back committed: from here a failed record holds the claims (a release would
    // let a second write-back through). The portfolio records first, because its promotion
    // is the undo's source of truth, then every member, each naming the portfolio.
    portfolios
        .promote(
            id,
            &PromotePortfolioRequest {
                generation,
                rank_key: key.to_owned(),
                reviewer: ReviewerId(input.reviewer.clone()),
                rationale: input.rationale.clone(),
                execution_plan: execution_plan.clone(),
            },
        )
        .await
        .map_err(|problem| held(&problem, "the portfolio promotion"))?;
    let via = PromotedVia {
        portfolio: ekg_domain::PortfolioId(id.to_owned()),
        generation,
        rank_key: key.to_owned(),
    };
    let mut first_failure = None;
    for (member, token) in &claims {
        if let Err(problem) = ledger
            .promote(
                member,
                token,
                &promotion_of(member),
                &execution_plan,
                Some(&via),
            )
            .await
        {
            first_failure.get_or_insert(held(&problem, &format!("member {member}'s promotion")));
        }
    }
    first_failure.map_or(Ok(()), Err)
}

/// The undo command: validate, check the promotion and its window, claim every member,
/// reverse what is left, then record every member and the portfolio reversed, or record
/// the progress and release every claim.
async fn run_undo(
    ledger: &dyn CommandLedger,
    portfolios: &dyn PortfolioLedger,
    executor: &dyn ActionExecutor,
    input: &PortfolioUndoInput,
    undo_window: Duration,
    now_secs: u64,
) -> Result<(), Problem> {
    validate_evidence(&input.reviewer, "reason", &input.reason)?;
    let id = input.portfolio.as_str();
    let record = read_portfolio(portfolios, id).await?;
    undoable(&record, undo_window, now_secs)?;

    let claims = claim_members(
        ledger,
        &record.members,
        ClaimCommand::Reverse,
        &input.reviewer,
        &[ActionPointState::Promoted],
        &format!("portfolio {id} undo"),
    )
    .await?;

    // Re-read under the claims: another undo may have recorded progress since the check.
    let promotion = match read_portfolio(portfolios, id)
        .await
        .and_then(|record| undoable(&record, undo_window, now_secs).cloned())
    {
        Ok(promotion) => promotion,
        Err(problem) => return Err(release_all(ledger, &claims, problem).await),
    };
    let plan = &promotion.execution_plan;
    let outcome = match executor
        .reverse_plan_remaining(plan, &promotion.progress.reversed)
        .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            let problem = Problem::bad_gateway(format!("governed reverse failed: {err}"));
            return Err(release_all(ledger, &claims, problem).await);
        }
    };
    let progress = promotion.progress.after(&outcome);
    let reviewer = ReviewerId(input.reviewer.clone());

    if !progress.refused.is_empty() {
        return Err(
            record_partial(ledger, portfolios, id, &claims, reviewer, plan, &progress).await,
        );
    }

    // Every action reversed: record every member, then the portfolio.
    let correlation_id = CorrelationId(format!("undo-portfolio/{id}/g{}", promotion.generation));
    let mut first_failure = None;
    for (member, token) in &claims {
        let reversal = Reversal {
            action_point: point_id(member),
            correlation_id: correlation_id.clone(),
            reviewer: reviewer.clone(),
            reason: input.reason.clone(),
        };
        if let Err(problem) = ledger.reverse(member, token, &reversal).await {
            first_failure.get_or_insert(held(&problem, &format!("member {member}'s reversal")));
        }
    }
    if let Err(problem) = portfolios
        .reverse(
            id,
            &ReversePortfolioRequest {
                reviewer,
                reason: input.reason.clone(),
            },
        )
        .await
    {
        first_failure.get_or_insert(held(&problem, "the portfolio reversal"));
    }
    first_failure.map_or(Ok(()), Err)
}

/// The dismiss command: validate, check the portfolio is active at the generation shown, claim
/// every member, record the portfolio dismissed, then abort each member with the portfolio
/// named. A retry after a member failed (the portfolio already dismissed at that generation)
/// aborts only the members still awaiting oversight.
async fn run_abort(
    ledger: &dyn CommandLedger,
    portfolios: &dyn PortfolioLedger,
    input: &PortfolioAbortInput,
) -> Result<(), Problem> {
    validate_evidence(&input.reviewer, "reason", &input.reason)?;
    let id = input.portfolio.as_str();
    let generation = input.generation;
    let record = read_portfolio(portfolios, id).await?;
    let retry = dismissable(&record, generation)?;
    let awaiting = [
        ActionPointState::Open,
        ActionPointState::Proposed,
        ActionPointState::UnderReview,
    ];

    // A retry claims only the members an earlier attempt left awaiting; the others are
    // already aborted.
    let mut members = record.members.clone();
    if retry {
        let mut still = Vec::with_capacity(members.len());
        for member in members {
            let state = ledger.record(&member.0).await?.and_then(|r| r.state());
            if state.is_some_and(ekg_dynamic::is_pre_promotion) {
                still.push(member);
            }
        }
        members = still;
    }
    let claims = claim_members(
        ledger,
        &members,
        ClaimCommand::Abort,
        &input.reviewer,
        &awaiting,
        &format!("portfolio {id} dismissal"),
    )
    .await?;

    if !retry {
        // The claims freeze every member's membership, so the portfolio checked above is
        // still the one dismissed. Record it first: its dismissal is the governed step, and a
        // member that then fails to abort is visible and retryable.
        let recorded = portfolios
            .dismiss(
                id,
                &DismissPortfolioRequest {
                    generation,
                    reviewer: ReviewerId(input.reviewer.clone()),
                    reason: input.reason.clone(),
                },
            )
            .await;
        if let Err(problem) = recorded {
            return Err(release_all(ledger, &claims, problem).await);
        }
    }

    let note = format!(
        "{} (dismissed with portfolio {id}, generation {generation})",
        input.reason
    );
    let mut first_failure = None;
    for (member, token) in &claims {
        if let Err(problem) = ledger.abort(member, token, &input.reviewer, &note).await {
            first_failure.get_or_insert(Problem::bad_gateway(format!(
                "member {member}'s abort did not record after the portfolio was dismissed ({}); \
                 its claim is held until it lapses. Dismiss again to abort only the members \
                 still awaiting",
                problem.detail
            )));
        }
    }
    first_failure.map_or(Ok(()), Err)
}

/// Check a portfolio can be dismissed at `generation`: it is active at that generation, or
/// (on a retry after a member failed) already dismissed at it. Returns whether it is a
/// retry. Every refusal is a 409 naming why.
fn dismissable(record: &PortfolioRecord, generation: u32) -> Result<bool, Problem> {
    let retry = match record.state {
        PortfolioState::Active => false,
        PortfolioState::Dismissed => true,
        other => {
            return Err(Problem::conflict(format!(
                "the portfolio is {other:?}, not active; only an active portfolio's plan can be \
                 dismissed"
            )));
        }
    };
    if generation != record.generation {
        return Err(Problem::conflict(format!(
            "generation {generation} is stale: the portfolio is at generation {} (it formed \
             again since you viewed it); re-fetch and dismiss again",
            record.generation
        )));
    }
    if record.members.is_empty() {
        return Err(Problem::conflict("the portfolio has no members"));
    }
    Ok(retry)
}

/// Record a partial undo on the portfolio, release every member claim with the counts, and
/// return the 502 naming what remains. A progress record that does not land holds the
/// claims, since some actions were already reversed.
async fn record_partial(
    ledger: &dyn CommandLedger,
    portfolios: &dyn PortfolioLedger,
    id: &str,
    claims: &[(String, String)],
    reviewer: ReviewerId,
    plan: &ExecutionPlan,
    progress: &ReverseProgress,
) -> Problem {
    let total = plan.receipts.len();
    if let Err(problem) = portfolios
        .record_partial_reverse(
            id,
            &RecordPortfolioPartialReverse {
                reviewer,
                progress: progress.clone(),
            },
        )
        .await
    {
        return held(&problem, "the portfolio's partial-reverse progress");
    }
    release_all(
        ledger,
        claims,
        partial_reverse_problem("the portfolio", total, progress),
    )
    .await
}

// --- helpers ---

/// Read a portfolio, or 404 when it was never formed.
async fn read_portfolio(
    portfolios: &dyn PortfolioLedger,
    id: &str,
) -> Result<PortfolioRecord, Problem> {
    portfolios
        .record(id)
        .await?
        .ok_or_else(|| Problem::not_found(format!("no portfolio {id}")))
}

/// Check a portfolio can be promoted at `generation` with option `key`, returning the
/// option's actions: it is active at that generation, with a joint slate for it that
/// carries the option. Every refusal is a 409 naming why.
fn promotable(
    record: &PortfolioRecord,
    generation: u32,
    key: &str,
) -> Result<Vec<ProposedAction>, Problem> {
    if record.state != PortfolioState::Active {
        return Err(Problem::conflict(format!(
            "the portfolio is {:?}, not active; only an active portfolio's plan can be promoted",
            record.state
        )));
    }
    if generation != record.generation {
        return Err(Problem::conflict(format!(
            "generation {generation} is stale: the portfolio is at generation {} (it formed \
             again since you viewed it); re-fetch and choose again",
            record.generation
        )));
    }
    let slate = record
        .slate
        .as_ref()
        .filter(|_| record.slate_generation == Some(generation))
        .ok_or_else(|| {
            Problem::conflict(format!(
                "the portfolio has no joint slate for generation {generation} yet"
            ))
        })?;
    let option = slate
        .options
        .iter()
        .find(|option| option.score.rank_key == key)
        .ok_or_else(|| {
            Problem::conflict(format!(
                "option_rank_key {key:?} is not on the portfolio's joint slate for generation \
                 {generation}; re-fetch and choose again"
            ))
        })?;
    if record.members.is_empty() {
        return Err(Problem::conflict("the portfolio has no members"));
    }
    Ok(option.plan.actions.clone())
}

/// Check a portfolio can be undone at `now_secs`, returning its promotion: it is promoted
/// and inside the reversal window. Every refusal is a 409 naming why.
fn undoable(
    record: &PortfolioRecord,
    undo_window: Duration,
    now_secs: u64,
) -> Result<&ekg_dynamic::PortfolioPromotion, Problem> {
    let promotion = record
        .promotion
        .as_ref()
        .filter(|_| record.state == PortfolioState::Promoted)
        .ok_or_else(|| {
            Problem::conflict(format!(
                "the portfolio is {:?}, not promoted; only a promoted portfolio can be undone",
                record.state
            ))
        })?;
    undo_window_open(Some(&promotion.promoted_at), undo_window, now_secs)
        .map_err(Problem::conflict)?;
    Ok(promotion)
}

/// Claim every member (sorted, so two commands contend in the same order) for `command`
/// from one of `expected` states. On the first refusal, release every claim already taken
/// and return the refusal as a 409 naming the member. Returns each member with its token.
async fn claim_members(
    ledger: &dyn CommandLedger,
    members: &[ekg_domain::EntityUri],
    command: ClaimCommand,
    reviewer: &str,
    expected: &[ActionPointState],
    what: &str,
) -> Result<Vec<(String, String)>, Problem> {
    let mut sorted: Vec<&str> = members.iter().map(|m| m.0.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut claims: Vec<(String, String)> = Vec::with_capacity(sorted.len());
    for member in sorted {
        match ledger.claim(member, command, reviewer, expected).await {
            Ok(token) => claims.push((member.to_owned(), token)),
            Err(refused) => {
                let problem = Problem::conflict(format!(
                    "{what} refused: member {member} cannot be claimed: {}",
                    refused.detail
                ));
                return Err(release_all(ledger, &claims, problem).await);
            }
        }
    }
    Ok(claims)
}

/// Release every claim in `claims`, auditing `problem`'s detail, and return the problem. A
/// release that fails is logged; that claim lapses at its deadline.
async fn release_all(
    ledger: &dyn CommandLedger,
    claims: &[(String, String)],
    problem: Problem,
) -> Problem {
    for (member, token) in claims {
        if let Err(why) = ledger.release(member, token, &problem.detail).await {
            eprintln!(
                "warning: could not release the command claim on {member} ({}); it lapses at its \
                 deadline",
                why.detail
            );
        }
    }
    problem
}

/// A record that failed after the write-back committed: the claims are held, not released,
/// so no second write-back can run until they lapse. Surfaced as a 502 naming what failed.
fn held(problem: &Problem, what: &str) -> Problem {
    Problem::bad_gateway(format!(
        "{what} did not record after the governed write-back committed ({}); the member \
         claims are held until they lapse and need human attention",
        problem.detail
    ))
}

/// A member point's id (the action-point object's own naming: `action-point/<subject>`).
fn point_id(member: &str) -> ActionPointId {
    ActionPointId(format!("action-point/{member}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use ekg_domain::{
        ActionType, EngineError, EntityUri, Omission, PortfolioId, Proposal, RankedPlan, Score,
        Slate, Trace,
    };
    use ekg_dynamic::{ActionPointRecord, AuditKind, CommandClaim, portfolio_id};

    use super::*;
    use crate::command::fakes::FakeExecutor;

    const NOW: u64 = 1_000;
    const WINDOW: Duration = Duration::from_secs(100);

    fn member(n: u32) -> String {
        format!("https://ekg.example/id/sale.order/{n}")
    }

    fn action(n: u32) -> ProposedAction {
        ProposedAction {
            action_type: ActionType("expedite_po".to_owned()),
            target: EntityUri(member(n)),
            params: BTreeMap::new(),
        }
    }

    /// A call a fake received, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Claim(String),
        Release(String),
        Promote(String),
        Reverse(String),
        PartialReverse(String),
        Abort(String),
        PortfolioPromote,
        PortfolioDismiss,
        PortfolioPartial,
        PortfolioReverse,
    }

    /// The member points, each a real [`ActionPointRecord`] driven through its own
    /// transitions, so claims, refusals and records behave like the durable object.
    struct Members {
        records: Mutex<BTreeMap<String, ActionPointRecord>>,
        calls: Mutex<Vec<Call>>,
        refuse_record: bool,
        /// A member whose abort fails to record.
        refuse_abort: Option<String>,
    }

    impl Members {
        fn new(records: impl IntoIterator<Item = (String, ActionPointRecord)>) -> Self {
            Self {
                records: Mutex::new(records.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
                refuse_record: false,
                refuse_abort: None,
            }
        }
        fn get(&self, subject: &str) -> ActionPointRecord {
            self.records.lock().expect("records")[subject].clone()
        }
        fn push(&self, call: Call) {
            self.calls.lock().expect("calls").push(call);
        }
        fn apply<T>(
            &self,
            subject: &str,
            f: impl FnOnce(&mut ActionPointRecord) -> ekg_domain::Result<T>,
        ) -> Result<T, Problem> {
            let mut records = self.records.lock().expect("records");
            let record = records
                .get_mut(subject)
                .ok_or_else(|| Problem::not_found(format!("no point {subject}")))?;
            f(record).map_err(|e| Problem::conflict(e.to_string()))
        }
    }

    #[async_trait]
    impl CommandLedger for Members {
        async fn record(&self, subject: &str) -> Result<Option<ActionPointRecord>, Problem> {
            Ok(self.records.lock().expect("records").get(subject).cloned())
        }
        async fn claim(
            &self,
            subject: &str,
            command: ClaimCommand,
            reviewer: &str,
            expected: &[ActionPointState],
        ) -> Result<String, Problem> {
            self.push(Call::Claim(subject.to_owned()));
            let token = format!("token/{subject}");
            let claim = CommandClaim {
                token: token.clone(),
                command,
                reviewer: ReviewerId(reviewer.to_owned()),
                claimed_at: NOW,
                deadline: NOW + 600,
            };
            self.apply(subject, |r| r.claim(claim, expected, NOW))?;
            Ok(token)
        }
        async fn release(&self, subject: &str, token: &str, reason: &str) -> Result<(), Problem> {
            self.push(Call::Release(subject.to_owned()));
            self.apply(subject, |r| Ok(r.release(token, reason.to_owned())))
                .map(drop)
        }
        async fn promote(
            &self,
            subject: &str,
            token: &str,
            promotion: &Promotion,
            execution_plan: &ExecutionPlan,
            via: Option<&PromotedVia>,
        ) -> Result<(), Problem> {
            self.push(Call::Promote(subject.to_owned()));
            if self.refuse_record {
                return Err(Problem::bad_gateway("restate ingress unreachable"));
            }
            self.apply(subject, |r| {
                r.promote_plan(
                    token,
                    promotion.clone(),
                    execution_plan.clone(),
                    via.cloned(),
                )
            })
            .map(drop)
        }
        async fn reverse(
            &self,
            subject: &str,
            token: &str,
            reversal: &Reversal,
        ) -> Result<(), Problem> {
            self.push(Call::Reverse(subject.to_owned()));
            self.apply(subject, |r| r.reverse(token, reversal.clone()))
        }
        async fn record_partial_reverse(
            &self,
            subject: &str,
            token: &str,
            progress: &ReverseProgress,
        ) -> Result<(), Problem> {
            self.push(Call::PartialReverse(subject.to_owned()));
            self.apply(subject, |r| {
                r.record_partial_reverse(token, progress.clone())
            })
        }
        async fn abort(
            &self,
            subject: &str,
            token: &str,
            reviewer: &str,
            reason: &str,
        ) -> Result<(), Problem> {
            self.push(Call::Abort(subject.to_owned()));
            if self.refuse_record || self.refuse_abort.as_deref() == Some(subject) {
                return Err(Problem::bad_gateway("restate ingress unreachable"));
            }
            self.apply(subject, |r| {
                r.abort(
                    token,
                    ReviewerId(reviewer.to_owned()),
                    Some(reason.to_owned()),
                )
            })
            .map(drop)
        }
    }

    /// The portfolio, a real [`PortfolioRecord`] driven through its own transitions.
    struct Portfolio {
        record: Mutex<Option<PortfolioRecord>>,
        calls: Mutex<Vec<Call>>,
        /// Replace the record on the second read (a regroup racing the claims).
        second_read: Mutex<Option<PortfolioRecord>>,
        reads: Mutex<usize>,
    }

    impl Portfolio {
        fn new(record: Option<PortfolioRecord>) -> Self {
            Self {
                record: Mutex::new(record),
                calls: Mutex::new(Vec::new()),
                second_read: Mutex::new(None),
                reads: Mutex::new(0),
            }
        }
        fn get(&self) -> PortfolioRecord {
            self.record.lock().expect("record").clone().expect("formed")
        }
        fn apply(
            &self,
            call: Call,
            f: impl FnOnce(&mut PortfolioRecord) -> ekg_domain::Result<()>,
        ) -> Result<(), Problem> {
            self.calls.lock().expect("calls").push(call);
            let mut record = self.record.lock().expect("record");
            let record = record
                .as_mut()
                .ok_or_else(|| Problem::not_found("never formed"))?;
            f(record).map_err(|e| Problem::conflict(e.to_string()))
        }
    }

    #[async_trait]
    impl PortfolioLedger for Portfolio {
        async fn record(&self, _id: &str) -> Result<Option<PortfolioRecord>, Problem> {
            let mut reads = self.reads.lock().expect("reads");
            *reads += 1;
            if *reads == 2
                && let Some(swap) = self.second_read.lock().expect("swap").take()
            {
                *self.record.lock().expect("record") = Some(swap);
            }
            Ok(self.record.lock().expect("record").clone())
        }
        async fn promote(&self, _id: &str, r: &PromotePortfolioRequest) -> Result<(), Problem> {
            self.apply(Call::PortfolioPromote, |rec| {
                rec.promote(ekg_dynamic::PortfolioPromotion {
                    generation: r.generation,
                    rank_key: r.rank_key.clone(),
                    reviewer: r.reviewer.clone(),
                    rationale: r.rationale.clone(),
                    execution_plan: r.execution_plan.clone(),
                    promoted_at: NOW.to_string(),
                    progress: ReverseProgress::default(),
                })
            })
        }
        async fn record_partial_reverse(
            &self,
            _id: &str,
            r: &RecordPortfolioPartialReverse,
        ) -> Result<(), Problem> {
            self.apply(Call::PortfolioPartial, |rec| {
                rec.record_partial_reverse(r.reviewer.clone(), r.progress.clone())
            })
        }
        async fn reverse(&self, _id: &str, r: &ReversePortfolioRequest) -> Result<(), Problem> {
            self.apply(Call::PortfolioReverse, |rec| {
                rec.reverse(r.reviewer.clone(), r.reason.clone())
            })
        }
        async fn dismiss(&self, _id: &str, r: &DismissPortfolioRequest) -> Result<(), Problem> {
            self.apply(Call::PortfolioDismiss, |rec| {
                rec.dismiss(r.generation, r.reviewer.clone(), r.reason.clone())
            })
        }
    }

    fn proposed(n: u32) -> (String, ActionPointRecord) {
        let mut record = ActionPointRecord::default();
        record.open(
            point_id(&member(n)),
            EntityUri(member(n)),
            "late".to_owned(),
            ekg_domain::Revision(1),
        );
        record
            .attach_proposal(
                Proposal::single("why".to_owned(), action(n), Vec::<Omission>::new()),
                &[],
                &[],
            )
            .expect("attach");
        (member(n), record)
    }

    fn members() -> Vec<EntityUri> {
        vec![EntityUri(member(1)), EntityUri(member(2))]
    }

    /// An active portfolio of members 1 and 2 at generation 2, slated with option `A`
    /// (two actions) and `B`.
    fn active() -> PortfolioRecord {
        let mut record = PortfolioRecord::default();
        record.form(
            portfolio_id(&members()),
            members(),
            "linked".to_owned(),
            Vec::new(),
        );
        record.supersede("member joined".to_owned());
        record.form(
            portfolio_id(&members()),
            members(),
            "linked".to_owned(),
            Vec::new(),
        );
        let option = |key: &str, actions: Vec<ProposedAction>| RankedPlan {
            plan: Plan { actions },
            score: Score {
                metrics: BTreeMap::new(),
                rank_key: key.to_owned(),
            },
            recommended: key == "A",
            needs: Vec::new(),
            omissions: Vec::new(),
            trace: Trace::default(),
        };
        record
            .attach_slate(
                2,
                Slate {
                    baseline: Score {
                        metrics: BTreeMap::new(),
                        rank_key: "Z".to_owned(),
                    },
                    options: vec![
                        option("A", vec![action(1), action(2)]),
                        option("B", vec![action(1)]),
                    ],
                    trace: Trace::default(),
                },
                "900".to_owned(),
            )
            .expect("slate");
        record
    }

    fn id() -> String {
        portfolio_id(&members()).0
    }

    fn promote_input(generation: u32, key: &str) -> PortfolioPromoteInput {
        PortfolioPromoteInput {
            portfolio: id(),
            generation,
            option_rank_key: key.to_owned(),
            reviewer: "planner-1".to_owned(),
            rationale: "approve plan A".to_owned(),
            context_seen: String::new(),
        }
    }

    fn setup() -> (Members, Portfolio, FakeExecutor) {
        (
            Members::new([proposed(1), proposed(2)]),
            Portfolio::new(Some(active())),
            FakeExecutor::default(),
        )
    }

    fn calls(m: &Members) -> Vec<Call> {
        m.calls.lock().expect("calls").clone()
    }

    /// Nothing was written: no execution, the portfolio unchanged, every member proposed
    /// and unclaimed.
    fn assert_nothing_written(m: &Members, p: &Portfolio, e: &FakeExecutor) {
        assert_eq!(e.executed(), 0, "nothing executed");
        assert!(
            p.calls.lock().expect("calls").is_empty(),
            "portfolio untouched"
        );
        for n in [1, 2] {
            let record = m.get(&member(n));
            assert_eq!(record.state(), Some(ActionPointState::Proposed));
            assert!(record.claim.is_none(), "member {n}'s claim is released");
        }
    }

    // --- promote ---

    #[tokio::test]
    async fn a_missing_rationale_is_422_and_touches_nothing() {
        let (m, p, e) = setup();
        let mut input = promote_input(2, "A");
        input.rationale = " ".to_owned();
        let err = run_promote(&m, &p, &e, &input).await.expect_err("422");
        assert_eq!(err.status, 422);
        assert!(calls(&m).is_empty());
        assert_nothing_written(&m, &p, &e);
    }

    #[tokio::test]
    async fn an_unknown_portfolio_is_404() {
        let (m, _, e) = setup();
        let p = Portfolio::new(None);
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("404");
        assert_eq!(err.status, 404);
        assert!(calls(&m).is_empty());
    }

    #[tokio::test]
    async fn stale_superseded_or_unknown_option_refusals_are_409_and_write_nothing() {
        for (input, why) in [
            (promote_input(1, "A"), "generation 1 is stale"),
            (
                promote_input(2, "C"),
                "is not on the portfolio's joint slate",
            ),
        ] {
            let (m, p, e) = setup();
            let err = run_promote(&m, &p, &e, &input).await.expect_err("409");
            assert_eq!(err.status, 409);
            assert!(err.detail.contains(why), "{}", err.detail);
            assert!(calls(&m).is_empty(), "nothing claimed");
            assert_nothing_written(&m, &p, &e);
        }
        let (m, _, e) = setup();
        let mut superseded = active();
        superseded.supersede("member aborted".to_owned());
        let p = Portfolio::new(Some(superseded));
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("409");
        assert_eq!(err.status, 409);
        assert!(err.detail.contains("not active"), "{}", err.detail);
        assert_nothing_written(&m, &p, &e);
    }

    #[tokio::test]
    async fn a_member_not_proposed_refuses_the_whole_promotion_releasing_the_claims_taken() {
        let (_, p, e) = setup();
        let (_, mut open) = proposed(2);
        open.point.as_mut().expect("point").state = ActionPointState::Open;
        let m = Members::new([proposed(1), (member(2), open)]);
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("409");
        assert_eq!(err.status, 409);
        assert!(
            err.detail.contains(&member(2)),
            "names the member: {}",
            err.detail
        );
        assert_eq!(
            calls(&m),
            vec![
                Call::Claim(member(1)),
                Call::Claim(member(2)),
                Call::Release(member(1))
            ]
        );
        let first = m.get(&member(1));
        assert!(first.claim.is_none());
        assert_eq!(
            first.audit.last().and_then(|a| a.kind),
            Some(AuditKind::CommandFailed),
            "the release is audited"
        );
        assert_eq!(e.executed(), 0);
        assert!(p.calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn a_member_held_by_another_command_refuses_the_whole_promotion() {
        let (m, p, e) = setup();
        m.apply(&member(2), |r| {
            r.claim(
                CommandClaim {
                    token: "other".to_owned(),
                    command: ClaimCommand::Abort,
                    reviewer: ReviewerId("planner-2".to_owned()),
                    claimed_at: NOW,
                    deadline: NOW + 600,
                },
                &[ActionPointState::Proposed],
                NOW,
            )
        })
        .expect("held elsewhere");
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("409");
        assert_eq!(err.status, 409);
        assert!(
            err.detail.contains("planner-2"),
            "names the holder: {}",
            err.detail
        );
        assert!(
            m.get(&member(1)).claim.is_none(),
            "member 1's claim is released"
        );
        assert_eq!(
            m.get(&member(2)).claim.map(|c| c.token),
            Some("other".to_owned()),
            "the other command keeps its claim"
        );
        assert_eq!(e.executed(), 0);
    }

    #[tokio::test]
    async fn a_portfolio_superseded_under_the_claims_fails_loud_and_releases() {
        let (m, p, e) = setup();
        let mut superseded = active();
        superseded.supersede("member regrouped".to_owned());
        *p.second_read.lock().expect("swap") = Some(superseded);
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("409");
        assert_eq!(err.status, 409);
        assert_nothing_written(&m, &p, &e);
    }

    #[tokio::test]
    async fn a_clean_promotion_executes_once_and_records_the_portfolio_then_every_member() {
        let (m, p, e) = setup();
        run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect("promoted");
        assert_eq!(e.executed(), 1, "one write-back");
        let portfolio = p.get();
        assert_eq!(portfolio.state, PortfolioState::Promoted);
        let promotion = portfolio.promotion.expect("recorded");
        assert_eq!(
            (promotion.generation, promotion.rank_key.as_str()),
            (2, "A")
        );
        assert_eq!(promotion.execution_plan.receipts.len(), 2);
        assert_eq!(
            calls(&m),
            vec![
                Call::Claim(member(1)),
                Call::Claim(member(2)),
                Call::Promote(member(1)),
                Call::Promote(member(2)),
            ]
        );
        assert_eq!(
            *p.calls.lock().expect("calls"),
            vec![Call::PortfolioPromote]
        );
        for n in [1, 2] {
            let record = m.get(&member(n));
            assert_eq!(record.state(), Some(ActionPointState::Promoted));
            assert!(record.claim.is_none());
            assert_eq!(record.portfolio, None);
            assert_eq!(
                record.promoted_via,
                Some(PromotedVia {
                    portfolio: PortfolioId(id()),
                    generation: 2,
                    rank_key: "A".to_owned(),
                })
            );
            assert_eq!(
                record.execution_plan.map(|plan| plan.receipts.len()),
                Some(2),
                "every member records the whole plan"
            );
            let promotion = record.promotion.expect("member promotion");
            assert_eq!(promotion.action_point, point_id(&member(n)));
            assert!(
                promotion.context_seen.contains("option rank_key=A"),
                "{}",
                promotion.context_seen
            );
            let note = record
                .audit
                .last()
                .and_then(|a| a.note.clone())
                .expect("note");
            assert!(
                note.contains(&format!("portfolio {} (generation 2), option A", id())),
                "{note}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_write_back_releases_every_claim_and_leaves_the_portfolio_active() {
        let (m, p, _) = setup();
        let e = FakeExecutor {
            fail: Some(|| EngineError::Backend("odoo refused".to_owned())),
            ..FakeExecutor::default()
        };
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("502");
        assert_eq!(err.status, 502);
        assert_eq!(p.get().state, PortfolioState::Active);
        for n in [1, 2] {
            let record = m.get(&member(n));
            assert_eq!(record.state(), Some(ActionPointState::Proposed));
            assert!(record.claim.is_none());
            let last = record.audit.last().expect("audited");
            assert_eq!(last.kind, Some(AuditKind::CommandFailed));
            assert!(
                last.note
                    .as_deref()
                    .is_some_and(|n| n.contains("odoo refused")),
                "{last:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_record_failure_after_the_write_back_holds_the_claims() {
        let (mut m, p, e) = setup();
        m.refuse_record = true;
        let err = run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect_err("502");
        assert_eq!(err.status, 502);
        assert_eq!(e.executed(), 1);
        assert!(
            !calls(&m).iter().any(|c| matches!(c, Call::Release(_))),
            "no release: {:?}",
            calls(&m)
        );
        for n in [1, 2] {
            assert!(
                m.get(&member(n)).claim.is_some(),
                "member {n}'s claim is held"
            );
        }
    }

    // --- undo ---

    /// A promoted portfolio (option A, receipts `fake://odoo/0` and `/1`) and its members.
    async fn promoted() -> (Members, Portfolio, FakeExecutor) {
        let (m, p, e) = setup();
        run_promote(&m, &p, &e, &promote_input(2, "A"))
            .await
            .expect("promoted");
        m.calls.lock().expect("calls").clear();
        p.calls.lock().expect("calls").clear();
        (m, p, e)
    }

    fn undo_input() -> PortfolioUndoInput {
        PortfolioUndoInput {
            portfolio: id(),
            reviewer: "planner-1".to_owned(),
            reason: "the supplier confirmed after all".to_owned(),
        }
    }

    #[tokio::test]
    async fn an_undo_outside_the_window_or_of_a_portfolio_not_promoted_is_409() {
        let (m, p, e) = promoted().await;
        let err = run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW + 101)
            .await
            .expect_err("closed");
        assert_eq!(err.status, 409);
        assert!(err.detail.contains("window closed"), "{}", err.detail);
        assert!(calls(&m).is_empty(), "nothing claimed");
        assert_eq!(e.reversed(), 0);

        let (m, p, e) = setup();
        let err = run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW)
            .await
            .expect_err("active");
        assert_eq!(err.status, 409);
        assert!(err.detail.contains("not promoted"), "{}", err.detail);
        assert_eq!(e.reversed(), 0);
    }

    #[tokio::test]
    async fn a_clean_undo_reverses_every_member_and_the_portfolio() {
        let (m, p, e) = promoted().await;
        run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW + 50)
            .await
            .expect("undone");
        assert_eq!(e.reversed_refs(), ["fake://odoo/1", "fake://odoo/0"]);
        assert_eq!(p.get().state, PortfolioState::Reversed);
        for n in [1, 2] {
            let record = m.get(&member(n));
            assert_eq!(record.state(), Some(ActionPointState::Reversed));
            assert!(record.claim.is_none());
            assert_eq!(
                record.audit.last().and_then(|a| a.note.as_deref()),
                Some("the supplier confirmed after all")
            );
        }
        assert_eq!(
            *p.calls.lock().expect("calls"),
            vec![Call::PortfolioReverse]
        );
    }

    #[tokio::test]
    async fn a_partial_undo_records_the_progress_releases_and_answers_502() {
        let (m, p, e) = promoted().await;
        e.refuse("fake://odoo/0", true);
        let err = run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW + 50)
            .await
            .expect_err("partial");
        assert_eq!(err.status, 502);
        assert!(
            err.detail.contains("1 of 2 action(s) remain unreversed"),
            "{}",
            err.detail
        );
        let portfolio = p.get();
        assert_eq!(portfolio.state, PortfolioState::Promoted);
        let progress = portfolio.promotion.expect("promotion").progress;
        assert_eq!(progress.reversed, [1].into_iter().collect());
        assert_eq!(progress.refused.len(), 1);
        for n in [1, 2] {
            let record = m.get(&member(n));
            assert_eq!(record.state(), Some(ActionPointState::Promoted));
            assert!(record.claim.is_none(), "member {n} released");
            let last = record.audit.last().expect("audited");
            assert_eq!(last.kind, Some(AuditKind::CommandFailed));
            assert!(
                last.note.as_deref().is_some_and(|n| n.contains("1 of 2")),
                "the release carries the counts: {last:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_retried_undo_reverses_only_the_rest() {
        let (m, p, e) = promoted().await;
        e.refuse("fake://odoo/0", true);
        run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW + 50)
            .await
            .expect_err("partial");
        // The conflicting source edit is reverted.
        e.refuse("fake://odoo/0", false);
        run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW + 60)
            .await
            .expect("finished");
        assert_eq!(
            e.reversed_refs(),
            ["fake://odoo/1", "fake://odoo/0"],
            "receipt 1 is not reversed twice"
        );
        let portfolio = p.get();
        assert_eq!(portfolio.state, PortfolioState::Reversed);
        for n in [1, 2] {
            assert_eq!(m.get(&member(n)).state(), Some(ActionPointState::Reversed));
        }
    }

    #[tokio::test]
    async fn an_undo_while_a_member_is_held_is_409_and_reverses_nothing() {
        let (m, p, e) = promoted().await;
        m.apply(&member(1), |r| {
            r.claim(
                CommandClaim {
                    token: "other".to_owned(),
                    command: ClaimCommand::Reverse,
                    reviewer: ReviewerId("planner-2".to_owned()),
                    claimed_at: NOW,
                    deadline: NOW + 600,
                },
                &[ActionPointState::Promoted],
                NOW,
            )
        })
        .expect("held elsewhere");
        let err = run_undo(&m, &p, &e, &undo_input(), WINDOW, NOW + 50)
            .await
            .expect_err("held");
        assert_eq!(err.status, 409);
        assert_eq!(e.reversed(), 0);
    }

    // --- dismiss ---

    fn abort_input(generation: u32) -> PortfolioAbortInput {
        PortfolioAbortInput {
            portfolio: id(),
            generation,
            reviewer: "planner-1".to_owned(),
            reason: "the customer moved the order".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_dismiss_without_evidence_is_422_and_touches_nothing() {
        for (reviewer, reason) in [("planner-1", " "), ("", "why"), ("policy:x", "why")] {
            let (m, p, e) = setup();
            let mut input = abort_input(2);
            input.reviewer = reviewer.to_owned();
            input.reason = reason.to_owned();
            let err = run_abort(&m, &p, &input).await.expect_err("422");
            assert_eq!(err.status, 422);
            assert!(calls(&m).is_empty());
            assert_nothing_written(&m, &p, &e);
        }
    }

    #[tokio::test]
    async fn a_dismiss_of_an_unknown_portfolio_is_404() {
        let (m, _, _) = setup();
        let p = Portfolio::new(None);
        let err = run_abort(&m, &p, &abort_input(2)).await.expect_err("404");
        assert_eq!(err.status, 404);
        assert!(calls(&m).is_empty());
    }

    #[tokio::test]
    async fn a_stale_or_inactive_dismiss_is_409_and_changes_nothing() {
        let (m, p, e) = setup();
        let err = run_abort(&m, &p, &abort_input(1)).await.expect_err("stale");
        assert_eq!(err.status, 409);
        assert!(
            err.detail.contains("generation 1 is stale"),
            "{}",
            err.detail
        );
        assert!(calls(&m).is_empty(), "nothing claimed");
        assert_nothing_written(&m, &p, &e);

        let (m, _, e) = setup();
        let mut superseded = active();
        superseded.supersede("member regrouped".to_owned());
        let p = Portfolio::new(Some(superseded));
        let err = run_abort(&m, &p, &abort_input(2))
            .await
            .expect_err("inactive");
        assert_eq!(err.status, 409);
        assert!(err.detail.contains("not active"), "{}", err.detail);
        assert_nothing_written(&m, &p, &e);
    }

    #[tokio::test]
    async fn a_held_member_refuses_the_dismissal_and_the_portfolio_stays_active() {
        let (m, p, e) = setup();
        m.apply(&member(2), |r| {
            r.claim(
                CommandClaim {
                    token: "other".to_owned(),
                    command: ClaimCommand::Promote,
                    reviewer: ReviewerId("planner-2".to_owned()),
                    claimed_at: NOW,
                    deadline: NOW + 600,
                },
                &[ActionPointState::Proposed],
                NOW,
            )
        })
        .expect("held elsewhere");
        let err = run_abort(&m, &p, &abort_input(2)).await.expect_err("held");
        assert_eq!(err.status, 409);
        assert!(err.detail.contains("planner-2"), "{}", err.detail);
        assert_eq!(p.get().state, PortfolioState::Active);
        assert!(p.calls.lock().expect("calls").is_empty(), "not dismissed");
        assert!(m.get(&member(1)).claim.is_none(), "member 1 released");
        assert!(
            !calls(&m).iter().any(|c| matches!(c, Call::Abort(_))),
            "no point aborted"
        );
        assert_eq!(e.executed(), 0);
    }

    #[tokio::test]
    async fn a_dismiss_records_the_portfolio_first_then_aborts_every_member() {
        let (m, p, e) = setup();
        run_abort(&m, &p, &abort_input(2)).await.expect("dismissed");
        assert_eq!(e.executed(), 0, "nothing written to any system of record");
        let portfolio = p.get();
        assert_eq!(portfolio.state, PortfolioState::Dismissed);
        let last = portfolio.audit.last().expect("audited");
        assert_eq!(last.event, ekg_dynamic::PortfolioEvent::Dismissed);
        assert_eq!(last.reason, "the customer moved the order");
        assert_eq!(last.reviewer, Some(ReviewerId("planner-1".to_owned())));
        assert_eq!(
            calls(&m),
            vec![
                Call::Claim(member(1)),
                Call::Claim(member(2)),
                Call::Abort(member(1)),
                Call::Abort(member(2)),
            ]
        );
        assert_eq!(
            *p.calls.lock().expect("calls"),
            vec![Call::PortfolioDismiss],
            "the portfolio records before any member aborts"
        );
        for n in [1, 2] {
            let record = m.get(&member(n));
            assert_eq!(record.state(), Some(ActionPointState::Aborted));
            assert!(record.claim.is_none());
            let note = record
                .audit
                .last()
                .and_then(|a| a.note.clone())
                .expect("note");
            assert_eq!(
                note,
                format!(
                    "the customer moved the order (dismissed with portfolio {}, generation 2)",
                    id()
                ),
                "the member's audit names the portfolio"
            );
        }
    }

    #[tokio::test]
    async fn a_member_that_fails_to_abort_holds_its_claim_and_a_retry_aborts_only_it() {
        let (mut m, p, _) = setup();
        m.refuse_abort = Some(member(2));
        let err = run_abort(&m, &p, &abort_input(2)).await.expect_err("502");
        assert_eq!(err.status, 502);
        assert!(err.detail.contains(&member(2)), "{}", err.detail);
        assert_eq!(p.get().state, PortfolioState::Dismissed, "stays dismissed");
        assert_eq!(m.get(&member(1)).state(), Some(ActionPointState::Aborted));
        let held = m.get(&member(2));
        assert_eq!(held.state(), Some(ActionPointState::Proposed));
        assert!(held.claim.is_some(), "the failed member's claim is held");

        // The held claim lapses; the retry aborts only the member still awaiting.
        m.apply(&member(2), |r| {
            r.claim = None;
            Ok(())
        })
        .expect("lapse");
        m.refuse_abort = None;
        m.calls.lock().expect("calls").clear();
        p.calls.lock().expect("calls").clear();
        run_abort(&m, &p, &abort_input(2)).await.expect("retried");
        assert_eq!(
            calls(&m),
            vec![Call::Claim(member(2)), Call::Abort(member(2))],
            "only the remaining member"
        );
        assert!(
            p.calls.lock().expect("calls").is_empty(),
            "the dismissal is not recorded twice"
        );
        assert_eq!(m.get(&member(2)).state(), Some(ActionPointState::Aborted));
    }

    // --- routes ---

    #[tokio::test]
    async fn the_router_serves_both_command_paths() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode, header};
        use tower::ServiceExt;

        let executor = std::sync::Arc::new(FakeExecutor::default());
        let app = router(ApiState {
            http: reqwest::Client::new(),
            ingress_url: "http://127.0.0.1:9".to_owned(),
            admin_url: "http://127.0.0.1:9".to_owned(),
            executor,
            undo_window: WINDOW,
            claim_deadline: WINDOW,
        });
        for path in [
            "/api/portfolios/promote",
            "/api/portfolios/undo",
            "/api/portfolios/abort",
        ] {
            // An empty reviewer is refused before any ingress call: the route exists and
            // validates first.
            let resp = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"portfolio":"p","generation":1,"option_rank_key":"A",
                                "reviewer":"","rationale":"why","reason":"why"}"#,
                        ))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        }
    }
}
