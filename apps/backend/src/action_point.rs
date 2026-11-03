//! The human-oversight surface: list, read, promote, abort and undo action points behind `/api`.
//!
//! Human oversight in the sense of EU AI Act Art. 14/12 is enforced here: a reviewer reads the
//! proposal and the omissions they were shown, promotes or aborts, and has to name themselves
//! and give a rationale to promote. A proposal that hid an omission is rejected earlier, at
//! attach. Every transition lands in the durable audit trail (Restate).
//!
//! The routes are thin. Reads and lifecycle transitions go to the durable
//! `ActionPointService` via the Restate ingress; the list reads Restate's state
//! introspection. Each command validates its oversight evidence (422), claims the point in
//! its durable record (409 while another command holds it), runs the governed write-back
//! in-process (the `ekg-kinetic` `OdooActionExecutor`) and records the outcome against the
//! claim. On failure it releases the claim with the failure's reason, which is audited. The
//! monitor bridge's auto-promotion follows the same protocol ([`crate::command`]). The
//! write-back is always a `PromotedAction`, so AI never commits on its own.

// Docs reference product and regulation names (Odoo, Restate, EU AI Act).
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ekg_domain::{
    ActionExecutor, ActionPoint, ActionPointState, CorrelationId, ExecutionPlan, Plan,
    PromotedPlan, Promotion, ProposedAction, Reversal, ReviewerId, Revision, Slate,
};
use ekg_dynamic::{ActionPointRecord, ClaimCommand};

use crate::command::{
    CommandLedger, Failure, IngressLedger, object_url, partial_reverse_problem, promote_claimed,
    settle,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

/// State shared by the action-point routes.
#[derive(Clone)]
pub struct ApiState {
    /// HTTP client for the Restate ingress and admin APIs.
    pub http: reqwest::Client,
    /// Restate ingress base URL (object handler calls).
    pub ingress_url: String,
    /// Restate admin base URL (`/query` introspection for the list).
    pub admin_url: String,
    /// The governed write-back driver run at the promote gate (and the reverse driver
    /// the `undo` command runs).
    pub executor: Arc<dyn ActionExecutor>,
    /// How long after apply an auto-applied change may still be reversed via `undo`,
    /// checked against the durable `promoted_at`. An undo after it is refused.
    pub undo_window: Duration,
    /// How long a command's claim on a point holds before another command may take it
    /// over (`EKG_COMMAND_CLAIM_SECS`).
    pub claim_deadline: Duration,
}

impl ApiState {
    /// The claim protocol's ledger over this state's Restate ingress.
    pub(crate) fn ledger(&self) -> IngressLedger {
        IngressLedger::new(
            self.http.clone(),
            self.ingress_url.clone(),
            self.claim_deadline,
        )
    }
}

/// Mount the action-point routes on `/api`.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/action-points", get(list))
        .route("/api/action-points/one", get(get_one))
        .route("/api/action-points/promote", post(promote))
        .route("/api/action-points/abort", post(abort))
        .route("/api/action-points/undo", post(undo))
        .with_state(state)
}

// --- RFC 7807 problem+json ---

/// An RFC-7807 error body. Serialized as `application/problem+json`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Problem {
    /// A URI reference identifying the problem type (`about:blank` for generic).
    pub r#type: String,
    /// A short, human-readable summary.
    pub title: String,
    /// The HTTP status code.
    pub status: u16,
    /// A human-readable explanation specific to this occurrence.
    pub detail: String,
}

impl Problem {
    pub(crate) fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            r#type: "about:blank".to_owned(),
            title: status.canonical_reason().unwrap_or("Error").to_owned(),
            status: status.as_u16(),
            detail: detail.into(),
        }
    }
    pub(crate) fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }
    pub(crate) fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, detail)
    }
    pub(crate) fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }
    pub(crate) fn bad_gateway(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, detail)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            code,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(self),
        )
            .into_response()
    }
}

// --- request and response DTOs ---

/// `?subject=<entity-iri>` for the single-point read.
#[derive(Debug, Deserialize)]
pub struct SubjectQuery {
    subject: String,
}

/// Promote a proposed point: who, why, and what they saw.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PromoteInput {
    /// The subject entity IRI (the action point's key).
    subject: String,
    /// The reviewer promoting (identity; no auth yet). Non-empty, and never a `policy:`
    /// identity (reserved for the auto-apply policy).
    reviewer: String,
    /// The reviewer's rationale (non-rubber-stamping). Non-empty.
    rationale: String,
    /// A record of the context the reviewer was shown.
    #[serde(default)]
    context_seen: String,
    /// The `rank_key` of the slate option the reviewer selected. Required when the point
    /// carries a slate (the promotion commits that option's plan; an unknown/stale key is
    /// refused). Ignored when the point has no slate: the staged proposal is promoted.
    /// An opaque ordering id, never a capability or metric name.
    #[serde(default)]
    option_rank_key: Option<String>,
}

/// Abort a point via the stop control.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AbortInput {
    /// The subject entity IRI.
    subject: String,
    /// The reviewer invoking the stop control. Non-empty, and never a `policy:` identity.
    reviewer: String,
    /// Their reason, recorded in the audit trail. Required and non-empty.
    reason: String,
}

/// Undo an auto-applied change within its reversal window.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UndoInput {
    /// The subject entity IRI (the action point's key).
    subject: String,
    /// The reviewer requesting the undo (identity; no auth yet). Non-empty, and never a
    /// `policy:` identity.
    reviewer: String,
    /// Their reason, recorded in the audit trail. Required and non-empty.
    reason: String,
}

// --- handlers ---

/// `GET /api/action-points`: lists every action point from Restate state introspection
/// (durable state, authoritative for display).
///
/// # Errors
/// [`Problem`] (502) if the Restate admin introspection is unreachable or malformed.
#[utoipa::path(
    get,
    path = "/api/action-points",
    operation_id = "list_action_points",
    tag = "action-points",
    responses(
        (status = 200, description = "Every action point (durable state)", body = [serde_json::Value]),
        (status = 502, description = "Restate admin unreachable", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn list(State(st): State<ApiState>) -> Result<Json<Vec<ActionPoint>>, Problem> {
    let query = "SELECT service_key, value_utf8 FROM state \
                 WHERE service_name = 'ActionPointService' AND key = 'record'";
    let resp = st
        .http
        .post(format!("{}/query", st.admin_url.trim_end_matches('/')))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .map_err(|e| Problem::bad_gateway(format!("restate admin unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err(Problem::bad_gateway(format!(
            "restate admin /query returned {}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| Problem::bad_gateway(e.to_string()))?;
    let parsed: Value =
        serde_json::from_str(&text).map_err(|e| Problem::bad_gateway(e.to_string()))?;
    let mut points = Vec::new();
    for row in parsed
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(record) = row.get("value_utf8").and_then(Value::as_str)
            && let Ok(record) = serde_json::from_str::<ActionPointRecord>(record)
            && let Some(point) = record.point
        {
            points.push(point);
        }
    }
    Ok(Json(points))
}

/// `GET /api/action-points/one?subject=<iri>`: one point, with the proposal and the
/// omissions the reviewer has to see.
///
/// # Errors
/// [`Problem`] (404) if no point exists for the subject; (502) if the Restate ingress
/// is unreachable or returns an undecodable point.
#[utoipa::path(
    get,
    path = "/api/action-points/one",
    operation_id = "get_action_point",
    tag = "action-points",
    params(("subject" = String, Query, description = "The subject entity IRI (the point key)")),
    responses(
        (status = 200, description = "The action point for the subject", body = serde_json::Value),
        (status = 404, description = "No action point for that subject", body = Problem,
         content_type = "application/problem+json"),
        (status = 502, description = "Restate ingress unreachable", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn get_one(
    State(st): State<ApiState>,
    Query(q): Query<SubjectQuery>,
) -> Result<Json<ActionPoint>, Problem> {
    fetch_point(&st, &q.subject)
        .await?
        .map(Json)
        .ok_or_else(|| Problem::not_found(format!("no action point for {}", q.subject)))
}

/// `POST /api/action-points/promote`: the governed write-back. Only a `proposed` point can
/// be promoted: the command validates its evidence, claims the point, runs the write-back
/// in-process and records it durably (`proposed`, then `under_review`, then `promoted`).
///
/// # Errors
/// [`Problem`] (422) if the reviewer or rationale is empty or the reviewer is a `policy:`
/// identity; (404) if no point exists; (409) if it is not `proposed`, another command holds
/// it, or the selected option is missing or stale; (502) if the governed write-back or the
/// durable record fails.
#[utoipa::path(
    post,
    path = "/api/action-points/promote",
    operation_id = "promote_action_point",
    tag = "action-points",
    request_body = PromoteInput,
    responses(
        (status = 200, description = "Promoted; governed write-back committed", body = serde_json::Value),
        (status = 404, description = "No action point for that subject", body = Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Point is not proposed, another command is in progress on \
         it, or (with a slate) the selected option_rank_key is missing, unknown/stale, or the \
         slate has no promotable option", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Missing oversight evidence: an empty reviewer or \
         rationale, or a policy: reviewer", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "Governed write-back or ingress failure", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn promote(
    State(st): State<ApiState>,
    Json(input): Json<PromoteInput>,
) -> Result<Json<ActionPoint>, Problem> {
    run_promote(&st.ledger(), st.executor.as_ref(), &input).await?;
    fetch_point(&st, &input.subject)
        .await?
        .map(Json)
        .ok_or_else(|| Problem::bad_gateway("action point missing after promote"))
}

/// The promote command over the claim protocol: validate, claim, resolve the plan the
/// operator selected, write back, record. Any failure before the write-back committed
/// releases the claim.
async fn run_promote(
    ledger: &dyn CommandLedger,
    executor: &dyn ActionExecutor,
    input: &PromoteInput,
) -> Result<(), Problem> {
    validate_evidence(&input.reviewer, "rationale", &input.rationale)?;
    let subject = input.subject.as_str();
    let token = ledger
        .claim(
            subject,
            ClaimCommand::Promote,
            &input.reviewer,
            &[ActionPointState::Proposed],
        )
        .await?;

    // Resolve the plan under the claim, from the same durable read the GraphQL `plan`
    // resolver uses. It carries the point (state and staged proposal) and the ranked slate,
    // so the option the operator selected is committed, not the single staged proposal. A
    // refusal here commits nothing and releases the claim.
    let resolved = async {
        let record = ledger
            .record(subject)
            .await
            .map_err(Failure::Release)?
            .ok_or_else(|| {
                Failure::Release(Problem::not_found(format!("no action point for {subject}")))
            })?;
        let point = require_proposed(record.point).map_err(Failure::Release)?;
        let proposal_actions = point
            .proposal
            .as_ref()
            .map(|proposal| proposal.actions.as_slice());
        let (actions, chosen_rank_key) = plan_to_promote(
            record.slate.as_ref(),
            proposal_actions,
            input.option_rank_key.as_deref(),
        )
        .map_err(Failure::Release)?;
        Ok((point, actions, chosen_rank_key))
    }
    .await;
    let (point, actions, chosen_rank_key) = settle(ledger, subject, &token, resolved).await?;

    // Record which option the reviewer committed in the durable promotion provenance
    // (Art. 12). The opaque `rank_key` goes into the free-form `context_seen`, next to
    // reviewer and rationale, so the neutral base needs no extra field.
    let context_seen = match &chosen_rank_key {
        Some(key) if input.context_seen.is_empty() => {
            format!("promoted slate option rank_key={key}")
        }
        Some(key) => format!(
            "{}; promoted slate option rank_key={key}",
            input.context_seen
        ),
        None => input.context_seen.clone(),
    };
    let promoted = PromotedPlan {
        // The chosen plan is one or more actions (the selected option, or the staged
        // proposal for a point without a slate), promoted as one atomic step.
        plan: Plan { actions },
        promotion: Promotion {
            action_point: point.id.clone(),
            correlation_id: CorrelationId(format!("promote/{}", point.id.0)),
            reviewer: ReviewerId(input.reviewer.clone()),
            rationale: input.rationale.clone(),
            context_seen,
        },
        // Audit metadata: the reverse guard uses each receipt's own source watermark.
        basis_revision: Revision(0),
    };

    // The governed write-back is the only path that commits. The whole plan is applied
    // atomically (on a mid-plan failure `execute_plan` rolls the committed actions back),
    // then recorded durably as one oversight interaction over all its actions.
    promote_claimed(ledger, executor, subject, &token, &promoted).await?;
    Ok(())
}

/// `POST /api/action-points/abort`: the stop control (`open` or `proposed` to `aborted`),
/// no write-back. The abort claims the point, then records.
///
/// # Errors
/// [`Problem`] (422) if the reviewer or reason is empty or the reviewer is a `policy:`
/// identity; (404) if no point exists; (409) if the point cannot be aborted from its
/// current state or another command holds it; (502) if the Restate ingress is unreachable.
#[utoipa::path(
    post,
    path = "/api/action-points/abort",
    operation_id = "abort_action_point",
    tag = "action-points",
    request_body = AbortInput,
    responses(
        (status = 204, description = "Aborted via the stop control; no write-back"),
        (status = 404, description = "No action point for that subject", body = Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Point cannot be aborted from its current state, or \
         another command is in progress on it", body = Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Missing oversight evidence: an empty reviewer or reason, \
         or a policy: reviewer", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "Restate ingress unreachable", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn abort(
    State(st): State<ApiState>,
    Json(input): Json<AbortInput>,
) -> Result<StatusCode, Problem> {
    run_abort(&st.ledger(), &input).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The abort command over the claim protocol: validate, claim, record. A refused record
/// releases the claim.
async fn run_abort(ledger: &dyn CommandLedger, input: &AbortInput) -> Result<(), Problem> {
    validate_evidence(&input.reviewer, "reason", &input.reason)?;
    let subject = input.subject.as_str();
    let token = ledger
        .claim(
            subject,
            ClaimCommand::Abort,
            &input.reviewer,
            &[
                ActionPointState::Open,
                ActionPointState::Proposed,
                ActionPointState::UnderReview,
            ],
        )
        .await?;
    let outcome = ledger
        .abort(subject, &token, &input.reviewer, &input.reason)
        .await
        .map_err(Failure::Release);
    settle(ledger, subject, &token, outcome).await
}

/// `POST /api/action-points/undo`: reverse an auto-applied change, or a plan a human approved
/// on this point alone, within its reversal window. Only a `promoted` point can be undone:
/// the undo claims the point, runs the governed inverse write-back (the reversible
/// `OdooActionExecutor` path the plan was committed on) and records it durably
/// (`promoted` to `reversed`). An undo requested after the window has closed is refused with
/// an explicit reason.
///
/// # Errors
/// [`Problem`] (422) if the reviewer or reason is empty or the reviewer is a `policy:`
/// identity; (404) if no point exists; (409) if it is not `promoted`, was promoted through its
/// portfolio (undo the portfolio instead), has no executed plan, the reversal window has closed
/// (or no applied time is recorded), or another command holds it; (502) if the governed
/// reverse or the durable record fails.
#[utoipa::path(
    post,
    path = "/api/action-points/undo",
    operation_id = "undo_action_point",
    tag = "action-points",
    request_body = UndoInput,
    responses(
        (status = 200, description = "Reversed; governed inverse write-back committed", body = serde_json::Value),
        (status = 404, description = "No action point for that subject", body = Problem,
         content_type = "application/problem+json"),
        (status = 409, description = "Point is not promoted, was promoted through its portfolio \
         (undo the portfolio), has no plan, the window closed, or another command is in progress \
         on it", body = Problem,
         content_type = "application/problem+json"),
        (status = 422, description = "Missing oversight evidence: an empty reviewer or reason, \
         or a policy: reviewer", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "Governed reverse or ingress failure", body = Problem,
         content_type = "application/problem+json")
    )
)]
pub async fn undo(
    State(st): State<ApiState>,
    Json(input): Json<UndoInput>,
) -> Result<Json<ActionPoint>, Problem> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    run_undo(
        &st.ledger(),
        st.executor.as_ref(),
        &input,
        st.undo_window,
        now,
    )
    .await?;
    fetch_point(&st, &input.subject)
        .await?
        .map(Json)
        .ok_or_else(|| Problem::bad_gateway("action point missing after undo"))
}

/// The undo command over the claim protocol: validate, claim, check the change is an
/// auto-applied one or a plan a human approved on this point alone, still inside its window
/// (at `now_secs`), reverse what an earlier partial undo left, record. Any failure before
/// the record releases the claim.
///
/// A reverse that leaves some receipts unreversed (their source changed after the write)
/// records its progress durably, which releases the claim, and answers 502 naming how many
/// remain; the next undo reverses only those.
async fn run_undo(
    ledger: &dyn CommandLedger,
    executor: &dyn ActionExecutor,
    input: &UndoInput,
    undo_window: Duration,
    now_secs: u64,
) -> Result<(), Problem> {
    validate_evidence(&input.reviewer, "reason", &input.reason)?;
    let subject = input.subject.as_str();
    let token = ledger
        .claim(
            subject,
            ClaimCommand::Reverse,
            &input.reviewer,
            &[ActionPointState::Promoted],
        )
        .await?;
    let outcome = async {
        let record = ledger
            .record(subject)
            .await
            .map_err(Failure::Release)?
            .ok_or_else(|| {
                Failure::Release(Problem::not_found(format!("no action point for {subject}")))
            })?;
        let (reversal, execution_plan) =
            undoable(&record, input, undo_window, now_secs).map_err(Failure::Release)?;

        // The governed reverse write-back: reverse each receipt an earlier attempt left
        // through its recorded inverse, each under its own source-watermark guard. Receipts
        // already reversed are skipped: their own reverse bumped the source, so re-running
        // them would be refused forever.
        let already = &record.reverse_progress;
        let outcome = executor
            .reverse_plan_remaining(execution_plan, &already.reversed)
            .await
            .map_err(|e| {
                Failure::Release(Problem::bad_gateway(format!(
                    "governed reverse failed: {e}"
                )))
            })?;
        let progress = already.after(&outcome);
        if progress.refused.is_empty() {
            // Record the reversal durably (`promoted` to `reversed`) in the audit trail (Art. 12).
            return ledger
                .reverse(subject, &token, &reversal)
                .await
                .map_err(Failure::Hold);
        }
        // A partial reversal gets its own answer and is recorded durably: the point stays
        // promoted with its progress, and the record releases the claim. A record that does
        // not land after some receipts were reversed holds the claim.
        ledger
            .record_partial_reverse(subject, &token, &progress)
            .await
            .map_err(Failure::Hold)?;
        Err(Failure::Settled(partial_reverse_problem(
            "the point",
            execution_plan.receipts.len(),
            &progress,
        )))
    }
    .await;
    settle(ledger, subject, &token, outcome).await
}

/// Decide whether `record` can be undone by `input` at `now_secs`, returning the reversal
/// to record and the executed plan to reverse. The point has to be `promoted` (by the
/// auto-apply policy, or by a human on this point alone, not through its portfolio), have an
/// executed plan, and be inside its reversal window.
fn undoable<'r>(
    record: &'r ActionPointRecord,
    input: &UndoInput,
    undo_window: Duration,
    now_secs: u64,
) -> Result<(Reversal, &'r ExecutionPlan), Problem> {
    let point = record
        .point
        .as_ref()
        .ok_or_else(|| Problem::not_found("no action point raised for that subject"))?;
    if point.state != ActionPointState::Promoted {
        return Err(Problem::conflict(format!(
            "action point is {:?}, not promoted; only a promoted change can be undone",
            point.state
        )));
    }
    // Undo reverses what the inbox lists with an Undo: an auto-applied change (the promoter
    // is the policy, the same predicate the `autoApplied` list uses) and a plan a human
    // approved on this point alone. A point promoted as a member of its portfolio's plan is
    // undone only with its portfolio, through the portfolio undo.
    if let Some(via) = &record.promoted_via {
        return Err(Problem::conflict(format!(
            "this point was promoted as part of portfolio {} (generation {}); undo it through \
             the portfolio undo, which reverses the whole plan",
            via.portfolio, via.generation
        )));
    }
    let execution_plan = record
        .execution_plan
        .as_ref()
        .ok_or_else(|| Problem::conflict("promoted point has no executed plan to reverse"))?;
    // The reversal window: now must be within promoted_at + window. A record with no applied
    // time cannot be windowed, so it is refused.
    undo_window_open(record.promoted_at.as_deref(), undo_window, now_secs)
        .map_err(Problem::conflict)?;
    let reversal = Reversal {
        action_point: point.id.clone(),
        correlation_id: CorrelationId(format!("undo/{}", point.id.0)),
        reviewer: ReviewerId(input.reviewer.clone()),
        reason: input.reason.clone(),
    };
    Ok((reversal, execution_plan))
}

/// Validate a human command's oversight evidence before anything is claimed or written
/// (EU AI Act Art. 14/12): a non-empty reviewer that is not a `policy:` identity (reserved
/// for the auto-apply policy's own promotions), and a non-empty `why` (the promote
/// rationale, or the abort/undo reason) named `why_field`. Whitespace-only is empty.
pub(crate) fn validate_evidence(reviewer: &str, why_field: &str, why: &str) -> Result<(), Problem> {
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(Problem::unprocessable(
            "reviewer must be non-empty: a governed command records who acted",
        ));
    }
    if reviewer.starts_with("policy:") {
        return Err(Problem::unprocessable(
            "reviewer must not be a policy: identity; it is reserved for the auto-apply policy",
        ));
    }
    if why.trim().is_empty() {
        return Err(Problem::unprocessable(format!(
            "{why_field} must be non-empty: a governed command records why"
        )));
    }
    Ok(())
}

// --- helpers ---

/// Resolve the plan a promotion commits, plus the `rank_key` (if any) to record in the audit
/// trail. Pure, so every branch can be unit-tested without Restate.
///
/// - Slate present: `option_rank_key` is required and resolved against the point's current
///   slate. A missing key, or one absent from the slate (for example the slate was
///   regenerated since the operator viewed it), is refused with a 409 and commits nothing;
///   otherwise the promoted plan is that option's actions and its `rank_key` is returned to
///   be audited.
/// - No slate: the point's staged proposal is promoted (the option key is not required),
///   returning `None` for the audited `rank_key`.
fn plan_to_promote(
    slate: Option<&Slate>,
    proposal_actions: Option<&[ProposedAction]>,
    option_rank_key: Option<&str>,
) -> Result<(Vec<ProposedAction>, Option<String>), Problem> {
    if let Some(slate) = slate {
        // A baseline-only slate (no admissible option beats doing nothing) is a promote
        // dead-end: there is nothing to select and the point does not fall back to its staged
        // proposal. Name that cause instead of a missing key or a stale slate: the reviewer
        // has to understand why the promote was refused (Art. 14).
        if slate.options.is_empty() {
            return Err(Problem::conflict(
                "this action point's slate has no promotable option (no candidate beats doing \
                 nothing); dismiss or abort the point rather than promoting it",
            ));
        }
        let key = option_rank_key.ok_or_else(|| {
            Problem::conflict(
                "this action point carries a slate; option_rank_key is required so the \
                 promotion commits the option you selected",
            )
        })?;
        // `rank_key` is unique within a slate (the capability's key plus a plan digest), so
        // it identifies a single option.
        let option = slate
            .options
            .iter()
            .find(|option| option.score.rank_key == key)
            .ok_or_else(|| {
                Problem::conflict(format!(
                    "option_rank_key {key:?} is not in the point's current slate (it may have \
                     been regenerated since you viewed it); re-fetch and choose again"
                ))
            })?;
        Ok((
            option.plan.actions.clone(),
            Some(option.score.rank_key.clone()),
        ))
    } else {
        let actions = proposal_actions
            .ok_or_else(|| Problem::conflict("proposed point has no attached proposal"))?;
        Ok((actions.to_vec(), None))
    }
}

/// A promote precondition: the point must exist and be `proposed`.
fn require_proposed(point: Option<ActionPoint>) -> Result<ActionPoint, Problem> {
    match point {
        None => Err(Problem::not_found("no action point for that subject")),
        Some(point) if point.state == ActionPointState::Proposed => Ok(point),
        Some(point) => Err(Problem::conflict(format!(
            "action point is {:?}, not proposed",
            point.state
        ))),
    }
}

/// Read a point from the durable object's `get` handler via the ingress.
async fn fetch_point(st: &ApiState, subject: &str) -> Result<Option<ActionPoint>, Problem> {
    let url = object_url(&st.ingress_url, subject, "get")?;
    let resp = st
        .http
        .post(url)
        .send()
        .await
        .map_err(|e| Problem::bad_gateway(format!("restate ingress unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err(Problem::bad_gateway(format!(
            "action point read returned {}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| Problem::bad_gateway(e.to_string()))?;
    serde_json::from_str::<Option<ActionPoint>>(&text)
        .map_err(|e| Problem::bad_gateway(format!("undecodable action point: {e}")))
}

/// Whether a record's plan was promoted by the auto-apply policy (reviewer `policy:<id>`)
/// and not by a human. The `undo` command and the GraphQL `autoApplied` list both use this
/// definition of "auto-applied", so they cannot disagree on which changes undo accepts.
pub(crate) fn promoted_by_policy(record: &ActionPointRecord) -> bool {
    record
        .promotion
        .as_ref()
        .is_some_and(|promotion| promotion.reviewer.0.starts_with("policy:"))
}

/// Decide whether an undo is within the reversal window.
///
/// `Ok(())` when a parseable `promoted_at` exists and `now_secs` is at or before
/// `promoted_at + window`. `Err(reason)` when the window has closed or no applied time is
/// recorded: a record that cannot be windowed is refused.
pub(crate) fn undo_window_open(
    promoted_at: Option<&str>,
    window: Duration,
    now_secs: u64,
) -> Result<(), String> {
    let promoted_at = promoted_at
        .and_then(|at| at.parse::<u64>().ok())
        .ok_or_else(|| {
            "no applied time recorded; the reversal window cannot be verified".to_owned()
        })?;
    let deadline = promoted_at.saturating_add(window.as_secs());
    if now_secs > deadline {
        return Err(format!(
            "reversal window closed (applied at {promoted_at}s, deadline {deadline}s, now {now_secs}s); undo refused"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use ekg_domain::{
        ActionPointId, ActionType, EngineError, EntityUri, ExecutionReceipt, RankedPlan, Score,
        Trace,
    };
    use ekg_dynamic::{RefusedReverse, ReverseProgress};

    fn action(verb: &str, new_date: &str) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: EntityUri("https://ekg.example/id/purchase.order/7".to_owned()),
            params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
        }
    }

    fn option(rank_key: &str, verb: &str) -> RankedPlan {
        RankedPlan {
            plan: Plan {
                actions: vec![action(verb, "2027-10-16")],
            },
            score: Score {
                metrics: BTreeMap::new(),
                rank_key: rank_key.to_owned(),
            },
            recommended: false,
            needs: Vec::new(),
            omissions: Vec::new(),
            trace: Trace::default(),
        }
    }

    fn slate_of(options: Vec<RankedPlan>) -> Slate {
        Slate {
            baseline: Score {
                metrics: BTreeMap::new(),
                rank_key: "99999".to_owned(),
            },
            options,
            trace: Trace::default(),
        }
    }

    #[test]
    fn plan_to_promote_builds_the_selected_option_not_the_proposal() {
        // A slate whose options differ from the single staged proposal.
        let slate = slate_of(vec![
            option("00000", "notify_owner"),
            option("00001", "expedite_po"),
        ]);
        let proposal = [action("re_promise_date", "2027-10-18 00:00:00")]; // the staged proposal
        let (actions, chosen) = plan_to_promote(Some(&slate), Some(&proposal), Some("00001"))
            .expect("the matching option resolves");
        // The promoted plan is the chosen option's actions, never the staged proposal's.
        assert_eq!(chosen.as_deref(), Some("00001"));
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type.0, "expedite_po");
    }

    #[test]
    fn plan_to_promote_resolves_equally_scored_options_by_their_unique_key() {
        // Two options share the capability score (`00000`) and differ only by their plan
        // digest: promoting the second key commits the second option's actions, never the
        // first match on the score.
        let slate = slate_of(vec![
            option("00000~1a2b3c4d5e6f", "notify_owner"),
            option("00000~ffeeddccbbaa", "expedite_po"),
        ]);
        let (actions, chosen) =
            plan_to_promote(Some(&slate), None, Some("00000~ffeeddccbbaa")).expect("resolves");
        assert_eq!(chosen.as_deref(), Some("00000~ffeeddccbbaa"));
        assert_eq!(actions[0].action_type.0, "expedite_po");
        // The bare score is not an identifier.
        assert_eq!(
            plan_to_promote(Some(&slate), None, Some("00000"))
                .expect_err("a score is not an option key")
                .status,
            409
        );
    }

    #[test]
    fn plan_to_promote_refuses_a_missing_or_unknown_option_when_a_slate_exists() {
        let slate = slate_of(vec![option("00000", "notify_owner")]);
        // A missing key with a slate present is a 409 and commits nothing.
        let missing = plan_to_promote(Some(&slate), None, None).expect_err("missing key is 409");
        assert_eq!(missing.status, 409);
        // So is a stale or unknown key (e.g. the slate was regenerated).
        let unknown =
            plan_to_promote(Some(&slate), None, Some("deadbeef")).expect_err("unknown key is 409");
        assert_eq!(unknown.status, 409);
    }

    #[test]
    fn plan_to_promote_refuses_a_baseline_only_slate_with_a_clear_reason() {
        // A slate can carry only the baseline (no admissible option). Even with a staged
        // proposal present, a slate point does not fall back to it. The refusal has to name
        // that cause, not a missing key or a stale slate.
        let slate = slate_of(Vec::new());
        let proposal = [action("re_promise_date", "2027-10-18")];
        let err = plan_to_promote(Some(&slate), Some(&proposal), Some("00001"))
            .expect_err("a baseline-only slate is a promote dead-end");
        assert_eq!(err.status, 409);
        assert!(
            err.detail.contains("no promotable option"),
            "the reason names the real cause, not a stale/missing-key race: {}",
            err.detail
        );
    }

    #[test]
    fn plan_to_promote_falls_back_to_the_proposal_when_no_slate() {
        let proposal = [action("re_promise_date", "2027-10-18")];
        // Without a slate the staged proposal is promoted unchanged: no option key is
        // required and none is audited.
        let (actions, chosen) = plan_to_promote(None, Some(&proposal), None)
            .expect("the no-slate fallback promotes the proposal");
        assert_eq!(chosen, None);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type.0, "re_promise_date");
    }

    fn point(state: ActionPointState) -> ActionPoint {
        ActionPoint {
            id: ActionPointId("action-point/sale.order/1".to_owned()),
            subject: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            state,
            reason: "shortfall".to_owned(),
            proposal: None,
        }
    }

    #[test]
    fn require_proposed_accepts_only_a_proposed_point() {
        assert!(require_proposed(Some(point(ActionPointState::Proposed))).is_ok());
        let missing = require_proposed(None).expect_err("none is 404");
        assert_eq!(missing.status, 404);
        let wrong = require_proposed(Some(point(ActionPointState::Open))).expect_err("open is 409");
        assert_eq!(wrong.status, 409);
    }

    #[test]
    fn problem_is_problem_json_with_the_status() {
        let resp = Problem::conflict("nope").into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
    }

    #[test]
    fn object_url_percent_encodes_the_subject_key() {
        let url = object_url(
            "http://localhost:8080",
            "https://ekg.example/id/sale.order/1",
            "get",
        )
        .expect("url");
        assert!(url.starts_with("http://localhost:8080/ActionPointService/"));
        assert!(url.ends_with("/get"));
        // The subject's slashes are encoded so the IRI stays a single key segment. The `url`
        // crate leaves `:` unencoded, as it is a valid path-segment character.
        assert!(url.contains("%2Fid%2Fsale.order%2F1"), "got {url}");
    }

    #[test]
    fn undo_window_open_allows_within_and_at_the_deadline_only() {
        let window = Duration::from_secs(100);
        // Applied at t=1000; deadline t=1100.
        assert!(
            undo_window_open(Some("1000"), window, 1000).is_ok(),
            "at apply time: open"
        );
        assert!(
            undo_window_open(Some("1000"), window, 1100).is_ok(),
            "exactly at deadline: open"
        );
        assert!(
            undo_window_open(Some("1000"), window, 1101).is_err(),
            "one second past the deadline: closed"
        );
    }

    #[test]
    fn undo_window_open_refuses_a_missing_or_unparseable_applied_time() {
        let window = Duration::from_secs(100);
        assert!(
            undo_window_open(None, window, 1000).is_err(),
            "no applied time recorded: refused, never undone blindly"
        );
        assert!(
            undo_window_open(Some("not-a-number"), window, 1000).is_err(),
            "an unparseable applied time is refused"
        );
    }

    #[test]
    fn promote_and_abort_inputs_deserialize_from_snake_case() {
        let p: PromoteInput = serde_json::from_str(
            r#"{"subject":"s","reviewer":"r","rationale":"why","context_seen":"ctx"}"#,
        )
        .expect("promote input");
        assert_eq!(p.reviewer, "r");
        // `option_rank_key` is optional on the wire; the handler requires it only when the
        // point has a slate.
        assert_eq!(p.option_rank_key, None);
        let with_option: PromoteInput = serde_json::from_str(
            r#"{"subject":"s","reviewer":"r","rationale":"why","option_rank_key":"00001"}"#,
        )
        .expect("promote input with option");
        assert_eq!(with_option.option_rank_key.as_deref(), Some("00001"));
        let a: AbortInput =
            serde_json::from_str(r#"{"subject":"s","reviewer":"r","reason":"dup"}"#)
                .expect("abort input");
        assert_eq!(a.reason, "dup");
    }

    // --- oversight evidence (422) ---

    #[test]
    fn evidence_validation_refuses_each_missing_or_reserved_field_with_422() {
        let cases = [
            ("", "why", "reviewer"),
            ("   ", "why", "reviewer"),
            ("planner-1", "", "rationale"),
            ("planner-1", " \t\n", "rationale"),
            ("policy:x", "why", "policy:"),
            (" policy:auto-apply", "why", "policy:"),
        ];
        for (reviewer, why, named) in cases {
            let err = validate_evidence(reviewer, "rationale", why)
                .expect_err("missing evidence is refused");
            assert_eq!(err.status, 422, "{reviewer:?}/{why:?}");
            assert!(
                err.detail.contains(named),
                "the refusal names {named}: {}",
                err.detail
            );
        }
        validate_evidence("planner-1", "reason", "supplier confirmed by phone")
            .expect("real evidence is accepted");
    }

    #[tokio::test]
    async fn a_command_body_without_a_reason_is_refused_422_by_deserialization() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = Router::new()
            .route(
                "/undo",
                post(|Json(_): Json<UndoInput>| async { StatusCode::OK }),
            )
            .route(
                "/abort",
                post(|Json(_): Json<AbortInput>| async { StatusCode::OK }),
            );
        for path in ["/undo", "/abort"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"subject":"s","reviewer":"planner-1"}"#))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        }
    }

    // --- the claim protocol on each command ---

    use crate::command::fakes::{Call, FakeExecutor, FakeLedger};
    use ekg_domain::Proposal;

    const SUBJECT: &str = "https://ekg.example/id/sale.order/1";

    fn promote_input(option_rank_key: Option<&str>) -> PromoteInput {
        PromoteInput {
            subject: SUBJECT.to_owned(),
            reviewer: "planner-1".to_owned(),
            rationale: "shortfall confirmed".to_owned(),
            context_seen: String::new(),
            option_rank_key: option_rank_key.map(str::to_owned),
        }
    }

    fn record_in(state: ActionPointState, with_slate: Option<Slate>) -> ActionPointRecord {
        let mut p = point(state);
        p.proposal = Some(Proposal::single(
            "why".to_owned(),
            action("re_promise_date", "2027-10-18"),
            Vec::new(),
        ));
        ActionPointRecord {
            point: Some(p),
            slate: with_slate,
            ..ActionPointRecord::default()
        }
    }

    fn released_with(calls: &[Call], needle: &str) -> bool {
        calls
            .iter()
            .any(|call| matches!(call, Call::Release(reason) if reason.contains(needle)))
    }

    #[tokio::test]
    async fn invalid_evidence_is_refused_before_any_claim() {
        let ledger = FakeLedger::with(Some(record_in(ActionPointState::Proposed, None)));
        let executor = FakeExecutor::default();
        let mut input = promote_input(None);
        input.rationale = "  ".to_owned();
        let err = run_promote(&ledger, &executor, &input)
            .await
            .expect_err("empty rationale");
        assert_eq!(err.status, 422);
        assert!(ledger.calls().is_empty(), "nothing claimed or written");
        assert_eq!(executor.executed(), 0);
    }

    #[tokio::test]
    async fn a_refused_claim_is_409_and_runs_no_write_back() {
        let mut ledger = FakeLedger::with(Some(record_in(ActionPointState::Proposed, None)));
        ledger.refuse_claim = Some(Problem::conflict(
            "the action point is not available: another command is in progress: promote by planner-2",
        ));
        let executor = FakeExecutor::default();
        let err = run_promote(&ledger, &executor, &promote_input(None))
            .await
            .expect_err("held elsewhere");
        assert_eq!(err.status, 409);
        assert!(
            err.detail.contains("planner-2"),
            "names the holder: {}",
            err.detail
        );
        assert_eq!(ledger.calls(), vec![Call::Claim(ClaimCommand::Promote)]);
        assert_eq!(executor.executed(), 0);
    }

    #[tokio::test]
    async fn promote_releases_when_the_selected_option_is_stale() {
        let slate = slate_of(vec![option("00000", "notify_owner")]);
        let ledger = FakeLedger::with(Some(record_in(ActionPointState::Proposed, Some(slate))));
        let executor = FakeExecutor::default();
        let err = run_promote(&ledger, &executor, &promote_input(Some("deadbeef")))
            .await
            .expect_err("stale key");
        assert_eq!(err.status, 409);
        assert!(
            released_with(&ledger.calls(), "deadbeef"),
            "{:?}",
            ledger.calls()
        );
        assert_eq!(executor.executed(), 0);
    }

    #[tokio::test]
    async fn promote_releases_when_the_point_vanished_or_moved_on() {
        let ledger = FakeLedger::with(None);
        let executor = FakeExecutor::default();
        let err = run_promote(&ledger, &executor, &promote_input(None))
            .await
            .expect_err("no record");
        assert_eq!(err.status, 404);
        assert!(released_with(&ledger.calls(), "no action point"));

        let ledger = FakeLedger::with(Some(record_in(ActionPointState::Aborted, None)));
        let err = run_promote(&ledger, &executor, &promote_input(None))
            .await
            .expect_err("not proposed");
        assert_eq!(err.status, 409);
        assert!(released_with(&ledger.calls(), "not proposed"));
        assert_eq!(executor.executed(), 0);
    }

    #[tokio::test]
    async fn promote_releases_when_the_write_back_fails() {
        let ledger = FakeLedger::with(Some(record_in(ActionPointState::Proposed, None)));
        let executor = FakeExecutor {
            fail: Some(|| EngineError::Backend("odoo refused".to_owned())),
            ..FakeExecutor::default()
        };
        let err = run_promote(&ledger, &executor, &promote_input(None))
            .await
            .expect_err("write-back failed");
        assert_eq!(err.status, 502);
        let calls = ledger.calls();
        assert!(released_with(&calls, "odoo refused"), "{calls:?}");
        assert!(!calls.contains(&Call::Promote), "nothing recorded");
    }

    #[tokio::test]
    async fn promote_records_once_under_its_claim() {
        let ledger = FakeLedger::with(Some(record_in(ActionPointState::Proposed, None)));
        let executor = FakeExecutor::default();
        run_promote(&ledger, &executor, &promote_input(None))
            .await
            .expect("promoted");
        assert_eq!(
            ledger.calls(),
            vec![Call::Claim(ClaimCommand::Promote), Call::Promote]
        );
    }

    #[tokio::test]
    async fn abort_claims_then_records_and_releases_when_refused() {
        let input = AbortInput {
            subject: SUBJECT.to_owned(),
            reviewer: "planner-1".to_owned(),
            reason: "supplier confirmed by phone".to_owned(),
        };
        let ledger = FakeLedger::with(None);
        run_abort(&ledger, &input).await.expect("aborted");
        assert_eq!(
            ledger.calls(),
            vec![
                Call::Claim(ClaimCommand::Abort),
                Call::Abort("supplier confirmed by phone".to_owned())
            ],
            "the reason is recorded exactly as sent"
        );

        let mut refusing = FakeLedger::with(None);
        refusing.refuse_record = Some(Problem::conflict("abort rejected by the action point"));
        let err = run_abort(&refusing, &input).await.expect_err("refused");
        assert_eq!(err.status, 409);
        assert!(released_with(&refusing.calls(), "abort rejected"));
    }

    fn undo_input() -> UndoInput {
        UndoInput {
            subject: SUBJECT.to_owned(),
            reviewer: "planner-1".to_owned(),
            reason: "supplier confirmation fell through".to_owned(),
        }
    }

    /// A record auto-applied by the policy at t=1000 with an executed plan.
    fn auto_applied() -> ActionPointRecord {
        let mut record = record_in(ActionPointState::Promoted, None);
        record.promotion = Some(Promotion {
            action_point: ActionPointId("action-point/sale.order/1".to_owned()),
            correlation_id: CorrelationId("auto-promote/x".to_owned()),
            reviewer: ReviewerId("policy:auto-apply".to_owned()),
            rationale: "auto-promoted".to_owned(),
            context_seen: String::new(),
        });
        // Two receipts, so an undo can reverse one and be refused on the other.
        let receipt = |i: usize| ExecutionReceipt {
            action: action("expedite_po", "2027-10-16"),
            external_ref: format!("fake://odoo/{i}"),
            source_revision: Revision(1),
            reversible: true,
        };
        record.execution_plan = Some(ExecutionPlan {
            actions: vec![action("expedite_po", "2027-10-16"); 2],
            receipts: vec![receipt(0), receipt(1)],
        });
        record.promoted_at = Some("1000".to_owned());
        record
    }

    /// A record a human promoted on this point alone at t=1000.
    fn human_approved() -> ActionPointRecord {
        let mut record = auto_applied();
        if let Some(promotion) = record.promotion.as_mut() {
            promotion.reviewer = ReviewerId("planner-1".to_owned());
            promotion.rationale = "approve plan A".to_owned();
        }
        record
    }

    #[tokio::test]
    async fn a_plan_a_human_approved_on_its_own_is_undone_within_its_window() {
        let ledger = FakeLedger::with(Some(human_approved()));
        let executor = FakeExecutor::default();
        run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect("undone");
        assert_eq!(executor.reversed(), 2, "every receipt reversed");
        assert_eq!(
            ledger.calls(),
            vec![
                Call::Claim(ClaimCommand::Reverse),
                Call::Reverse("supplier confirmation fell through".to_owned())
            ],
            "claimed, reversed and recorded like an auto-applied change"
        );
        // Its window applies the same way.
        let ledger = FakeLedger::with(Some(human_approved()));
        let err = run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1101,
        )
        .await
        .expect_err("window closed");
        assert_eq!(err.status, 409);
    }

    #[tokio::test]
    async fn a_portfolio_member_is_refused_naming_its_portfolio() {
        let mut member = human_approved();
        member.promoted_via = Some(ekg_dynamic::PromotedVia {
            portfolio: ekg_domain::PortfolioId("portfolio/0123456789ab".to_owned()),
            generation: 2,
            rank_key: "A".to_owned(),
        });
        let ledger = FakeLedger::with(Some(member));
        let executor = FakeExecutor::default();
        let err = run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect_err("a member");
        assert_eq!(err.status, 409);
        assert!(
            err.detail.contains("portfolio/0123456789ab") && err.detail.contains("portfolio undo"),
            "{}",
            err.detail
        );
        assert!(released_with(&ledger.calls(), "portfolio undo"));
        assert_eq!(executor.reversed(), 0, "nothing reversed");
    }

    #[tokio::test]
    async fn undo_releases_when_the_window_has_closed() {
        let ledger = FakeLedger::with(Some(auto_applied()));
        let executor = FakeExecutor::default();
        let err = run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1101,
        )
        .await
        .expect_err("window closed");
        assert_eq!(err.status, 409);
        assert!(released_with(&ledger.calls(), "reversal window closed"));
        assert_eq!(executor.reversed(), 0);
    }

    #[tokio::test]
    async fn undo_releases_when_the_reverse_errors_and_records_on_success() {
        // A reverse that cannot even start (every receipt refused by a transport fault) is
        // still a partial outcome: nothing reversed, both remaining. It is recorded, not
        // released.
        let ledger = FakeLedger::with(Some(auto_applied()));
        let executor = FakeExecutor {
            fail_reverse: Some(|| EngineError::Backend("odoo down".to_owned())),
            ..FakeExecutor::default()
        };
        let err = run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect_err("nothing reversed");
        assert_eq!(err.status, 502);
        assert!(
            err.detail.contains("2 of 2 action(s) remain"),
            "{}",
            err.detail
        );

        let ledger = FakeLedger::with(Some(auto_applied()));
        let executor = FakeExecutor::default();
        run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect("undone");
        assert_eq!(
            ledger.calls(),
            vec![
                Call::Claim(ClaimCommand::Reverse),
                Call::Reverse("supplier confirmation fell through".to_owned())
            ]
        );
        assert_eq!(executor.reversed_refs(), ["fake://odoo/1", "fake://odoo/0"]);
    }

    // A partial reverse records its progress, which releases the claim. Releasing without
    // recording would make a retry re-run every receipt and be refused forever on the ones
    // already reversed.
    #[tokio::test]
    async fn a_partial_undo_records_the_progress_and_answers_502_with_the_remaining_count() {
        let ledger = FakeLedger::with(Some(auto_applied()));
        let executor = FakeExecutor::default();
        executor.refuse("fake://odoo/0", true);
        let err = run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect_err("partial");
        assert_eq!(err.status, 502);
        assert!(
            err.detail.contains("1 of 2 action(s) remain unreversed")
                && err.detail.contains("fake://odoo/0"),
            "{}",
            err.detail
        );
        let calls = ledger.calls();
        let [
            Call::Claim(ClaimCommand::Reverse),
            Call::PartialReverse(progress),
        ] = calls.as_slice()
        else {
            panic!("claimed, then recorded the progress, no release: {calls:?}");
        };
        assert_eq!(progress.reversed, [1].into_iter().collect());
        assert_eq!(progress.refused.len(), 1);
        assert_eq!(progress.refused[0].index, 0);
        assert_eq!(executor.reversed_refs(), ["fake://odoo/1"]);
    }

    #[tokio::test]
    async fn a_retried_undo_reverses_only_the_rest_and_records_reversed() {
        let mut record = auto_applied();
        record.reverse_progress = ReverseProgress {
            reversed: [1].into_iter().collect(),
            refused: vec![RefusedReverse {
                index: 0,
                reason: "source changed".to_owned(),
            }],
        };
        let ledger = FakeLedger::with(Some(record));
        let executor = FakeExecutor::default();
        run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect("finished");
        assert_eq!(
            executor.reversed_refs(),
            ["fake://odoo/0"],
            "only the receipt left"
        );
        assert_eq!(
            ledger.calls(),
            vec![
                Call::Claim(ClaimCommand::Reverse),
                Call::Reverse("supplier confirmation fell through".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn a_partial_undo_whose_record_fails_holds_the_claim() {
        let mut ledger = FakeLedger::with(Some(auto_applied()));
        ledger.refuse_record = Some(Problem::bad_gateway("restate ingress unreachable"));
        let executor = FakeExecutor::default();
        executor.refuse("fake://odoo/0", true);
        let err = run_undo(
            &ledger,
            &executor,
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect_err("record failed");
        assert_eq!(err.status, 502);
        assert!(
            !ledger.calls().iter().any(|c| matches!(c, Call::Release(_))),
            "a receipt was reversed; the claim is held, not released"
        );
    }

    #[tokio::test]
    async fn undo_holds_the_claim_when_the_record_fails_after_the_reverse() {
        let mut ledger = FakeLedger::with(Some(auto_applied()));
        ledger.refuse_record = Some(Problem::bad_gateway("restate ingress unreachable"));
        let err = run_undo(
            &ledger,
            &FakeExecutor::default(),
            &undo_input(),
            Duration::from_secs(100),
            1050,
        )
        .await
        .expect_err("record failed");
        assert_eq!(err.status, 502);
        assert!(
            !ledger.calls().iter().any(|c| matches!(c, Call::Release(_))),
            "the reverse committed; the claim is held, not released"
        );
    }
}
