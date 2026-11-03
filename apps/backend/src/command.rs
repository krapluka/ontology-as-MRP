//! The claim protocol every governed command on an action point follows.
//!
//! A command (a human promotion, a policy auto-promotion, an undo or an abort) first
//! claims the point in its durable Restate record, then runs its write-back, then
//! records its outcome against the claim. A write-back that fails releases the claim
//! with the failure's reason, which the record audits (`command_failed`). A second
//! command's claim is refused while the first holds the point, so two commands can never
//! both write back. The claim, the release and the recording all go through the
//! [`CommandLedger`] seam, so the protocol is unit-testable without the substrate.

// Docs name products (Restate, Odoo).
#![allow(clippy::doc_markdown)]

use std::time::Duration;

use async_trait::async_trait;
use axum::http::header;
use ekg_domain::{ActionPointState, EngineError, ExecutionPlan, Promotion, Reversal};
use ekg_dynamic::{
    ActionPointRecord, ClaimCommand, ClaimRequest, PromotedVia, ReleaseRequest, ReverseProgress,
};
use serde::Serialize;
use serde_json::Value;

use crate::action_point::Problem;

/// The durable action-point record as a command sees it: read it, claim it, release it,
/// and record a command's outcome against its claim.
#[async_trait]
pub trait CommandLedger: Send + Sync {
    /// Read the point's durable record (`None` when nothing was raised for `subject`).
    async fn record(&self, subject: &str) -> Result<Option<ActionPointRecord>, Problem>;

    /// Claim the point for `command` by `reviewer` when it is in one of `expected`
    /// states, returning the claim token. Refused with 409 (naming the holder) while
    /// another command holds it or when it is in another state; 404 when no point was
    /// raised.
    async fn claim(
        &self,
        subject: &str,
        command: ClaimCommand,
        reviewer: &str,
        expected: &[ActionPointState],
    ) -> Result<String, Problem>;

    /// Release `token`'s claim after a failure, auditing `reason`. Idempotent.
    async fn release(&self, subject: &str, token: &str, reason: &str) -> Result<(), Problem>;

    /// Record a promotion and its executed plan against `token`'s claim, through the
    /// portfolio promotion `via` when the point is promoted as a member of its portfolio's
    /// plan.
    async fn promote(
        &self,
        subject: &str,
        token: &str,
        promotion: &Promotion,
        execution_plan: &ExecutionPlan,
        via: Option<&PromotedVia>,
    ) -> Result<(), Problem>;

    /// Record a reversal against `token`'s claim.
    async fn reverse(&self, subject: &str, token: &str, reversal: &Reversal)
    -> Result<(), Problem>;

    /// Record a partial reverse's `progress` against `token`'s claim; the record releases
    /// the claim.
    async fn record_partial_reverse(
        &self,
        subject: &str,
        token: &str,
        progress: &ReverseProgress,
    ) -> Result<(), Problem>;

    /// Record an abort against `token`'s claim.
    async fn abort(
        &self,
        subject: &str,
        token: &str,
        reviewer: &str,
        reason: &str,
    ) -> Result<(), Problem>;
}

/// How a claimed command failed, which decides what happens to its claim.
#[derive(Debug)]
pub enum Failure {
    /// Nothing was committed, or the write-back failed: release the claim, auditing
    /// the reason, so a later command can act.
    Release(Problem),
    /// The write-back committed but its record did not land: keep the claim until its
    /// deadline, since releasing it would let a second command write back again.
    Hold(Problem),
    /// The command's own record already released the claim (a partial reverse): nothing
    /// is left to release.
    Settled(Problem),
}

/// Settle a claimed command: pass a success through; on a failure, release the claim
/// (auditing the problem's detail) or hold it, and return the problem. A release that
/// itself fails is logged and never masks the command's own failure; the claim then
/// lapses at its deadline.
///
/// # Errors
/// The failure's [`Problem`], after the claim was released or held.
pub async fn settle<T>(
    ledger: &dyn CommandLedger,
    subject: &str,
    token: &str,
    outcome: Result<T, Failure>,
) -> Result<T, Problem> {
    match outcome {
        Ok(value) => Ok(value),
        Err(Failure::Hold(problem) | Failure::Settled(problem)) => Err(problem),
        Err(Failure::Release(problem)) => {
            if let Err(why) = ledger.release(subject, token, &problem.detail).await {
                eprintln!(
                    "warning: could not release the command claim on {subject} ({}); it lapses at \
                     its deadline",
                    why.detail
                );
            }
            Err(problem)
        }
    }
}

/// Map a failed governed write-back (`execute_plan`) to its problem. A partial commit (a
/// mid-plan rollback refused by a concurrent source edit) says so, because the source was
/// changed and needs human attention before any retry.
#[must_use]
pub fn write_back_problem(err: &EngineError) -> Problem {
    match err {
        EngineError::PlanPartiallyApplied { .. } => Problem::bad_gateway(format!(
            "governed write-back partially applied: {err}; the source was partly changed and \
             needs human attention before any retry"
        )),
        other => Problem::bad_gateway(format!("governed write-back failed: {other}")),
    }
}

/// The 502 for an undo that could reverse only some of a plan's `total` actions: how many
/// remain and why, and that a retry reverses only those.
#[must_use]
pub fn partial_reverse_problem(what: &str, total: usize, progress: &ReverseProgress) -> Problem {
    let reasons = progress
        .refused
        .iter()
        .map(|refused| refused.reason.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Problem::bad_gateway(format!(
        "governed reverse only partially completed: {} of {total} action(s) remain unreversed: \
         {reasons}; the progress is recorded and {what} stays promoted. Resolve the refusal \
         (e.g. revert the conflicting source edit) and undo again to reverse only what remains",
        total.saturating_sub(progress.reversed.len())
    ))
}

/// Promote `promotion`'s plan under a claim `token` already holds: run the governed
/// write-back, then record it. A failed write-back releases the claim; a record that
/// does not land after the write-back committed holds it.
///
/// # Errors
/// The write-back's or the record's [`Problem`], with the claim released or held.
pub async fn promote_claimed(
    ledger: &dyn CommandLedger,
    executor: &dyn ekg_domain::ActionExecutor,
    subject: &str,
    token: &str,
    promoted: &ekg_domain::PromotedPlan,
) -> Result<ExecutionPlan, Problem> {
    let outcome = async {
        let execution_plan = executor
            .execute_plan(promoted)
            .await
            .map_err(|e| Failure::Release(write_back_problem(&e)))?;
        ledger
            .promote(subject, token, &promoted.promotion, &execution_plan, None)
            .await
            .map_err(Failure::Hold)?;
        Ok(execution_plan)
    }
    .await;
    settle(ledger, subject, token, outcome).await
}

// --- the Restate ingress ledger ---

/// The [`CommandLedger`] over the durable `ActionPointService` object, reached through the
/// Restate ingress. Every claim it takes holds for `claim_deadline`.
#[derive(Clone)]
pub struct IngressLedger {
    http: reqwest::Client,
    ingress_url: String,
    claim_deadline: Duration,
}

impl IngressLedger {
    /// A ledger over the ingress at `ingress_url` whose claims hold for `claim_deadline`.
    #[must_use]
    pub const fn new(http: reqwest::Client, ingress_url: String, claim_deadline: Duration) -> Self {
        Self {
            http,
            ingress_url,
            claim_deadline,
        }
    }

    /// POST `body` (when given) to `handler` on the subject's object; the response body
    /// on success, else the status and the ingress's error message.
    async fn call(
        &self,
        subject: &str,
        handler: &str,
        body: Option<String>,
    ) -> Result<Result<String, (u16, String)>, Problem> {
        ingress_call(
            &self.http,
            object_url(&self.ingress_url, subject, handler)?,
            body,
        )
        .await
    }

    /// Record a command's outcome through `handler`; a refusal is a 409 naming `what`.
    async fn record_outcome(
        &self,
        subject: &str,
        handler: &str,
        what: &str,
        body: &impl Serialize,
    ) -> Result<(), Problem> {
        let body = serde_json::to_string(body).map_err(|e| Problem::bad_gateway(e.to_string()))?;
        self.call(subject, handler, Some(body))
            .await?
            .map(drop)
            .map_err(|(status, detail)| {
                Problem::conflict(format!(
                    "{what} rejected by the action point ({status}): {detail}"
                ))
            })
    }
}

#[async_trait]
impl CommandLedger for IngressLedger {
    async fn record(&self, subject: &str) -> Result<Option<ActionPointRecord>, Problem> {
        let text = self
            .call(subject, "record", None)
            .await?
            .map_err(|(status, _)| {
                Problem::bad_gateway(format!("action point record read returned {status}"))
            })?;
        serde_json::from_str::<Option<ActionPointRecord>>(&text)
            .map_err(|e| Problem::bad_gateway(format!("undecodable action point record: {e}")))
    }

    async fn claim(
        &self,
        subject: &str,
        command: ClaimCommand,
        reviewer: &str,
        expected: &[ActionPointState],
    ) -> Result<String, Problem> {
        let body = serde_json::to_string(&ClaimRequest {
            command,
            reviewer: reviewer.to_owned(),
            expected_state: expected.to_vec(),
            deadline_secs: self.claim_deadline.as_secs(),
        })
        .map_err(|e| Problem::bad_gateway(e.to_string()))?;
        let text = self
            .call(subject, "claim", Some(body))
            .await?
            .map_err(|(status, detail)| claim_refusal(status, &detail))?;
        serde_json::from_str::<String>(&text)
            .map_err(|e| Problem::bad_gateway(format!("undecodable claim token: {e}")))
    }

    async fn release(&self, subject: &str, token: &str, reason: &str) -> Result<(), Problem> {
        let body = serde_json::to_string(&ReleaseRequest {
            token: token.to_owned(),
            reason: reason.to_owned(),
        })
        .map_err(|e| Problem::bad_gateway(e.to_string()))?;
        self.call(subject, "release", Some(body))
            .await?
            .map(drop)
            .map_err(|(status, detail)| {
                Problem::bad_gateway(format!("claim release returned {status}: {detail}"))
            })
    }

    async fn promote(
        &self,
        subject: &str,
        token: &str,
        promotion: &Promotion,
        execution_plan: &ExecutionPlan,
        via: Option<&PromotedVia>,
    ) -> Result<(), Problem> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
            promotion: &'a Promotion,
            execution_plan: &'a ExecutionPlan,
            #[serde(skip_serializing_if = "Option::is_none")]
            promoted_via: Option<&'a PromotedVia>,
        }
        let body = Body {
            token,
            promotion,
            execution_plan,
            promoted_via: via,
        };
        self.record_outcome(subject, "promote", "promote", &body)
            .await
    }

    async fn reverse(
        &self,
        subject: &str,
        token: &str,
        reversal: &Reversal,
    ) -> Result<(), Problem> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
            reversal: &'a Reversal,
        }
        self.record_outcome(subject, "reverse", "reverse", &Body { token, reversal })
            .await
    }

    async fn record_partial_reverse(
        &self,
        subject: &str,
        token: &str,
        progress: &ReverseProgress,
    ) -> Result<(), Problem> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
            progress: &'a ReverseProgress,
        }
        self.record_outcome(
            subject,
            "record_partial_reverse",
            "partial reverse",
            &Body { token, progress },
        )
        .await
    }

    async fn abort(
        &self,
        subject: &str,
        token: &str,
        reviewer: &str,
        reason: &str,
    ) -> Result<(), Problem> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
            reviewer: &'a str,
            reason: &'a str,
        }
        let body = Body {
            token,
            reviewer,
            reason,
        };
        self.record_outcome(subject, "abort", "abort", &body).await
    }
}

/// POST `body` (when given) to an ingress `url`; the response body on success, else the
/// status and the ingress's error message.
pub(crate) async fn ingress_call(
    http: &reqwest::Client,
    url: String,
    body: Option<String>,
) -> Result<Result<String, (u16, String)>, Problem> {
    let mut request = http.post(url);
    if let Some(body) = body {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| Problem::bad_gateway(format!("restate ingress unreachable: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Ok(if status.is_success() {
        Ok(text)
    } else {
        Err((status.as_u16(), ingress_message(&text)))
    })
}

/// Map a refused claim to its problem: the object codes a refusal 409 and a missing
/// point 404; anything else is an upstream failure.
fn claim_refusal(status: u16, detail: &str) -> Problem {
    match status {
        404 => Problem::not_found(detail.to_owned()),
        409 => Problem::conflict(format!("the action point is not available: {detail}")),
        other => Problem::bad_gateway(format!("claim returned {other}: {detail}")),
    }
}

/// The message of a Restate ingress error body (`{"message": …}`), else the raw text.
fn ingress_message(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| text.to_owned())
}

/// Build the ingress URL for an `ActionPointService` handler, percent-encoding the
/// subject key.
pub(crate) fn object_url(ingress: &str, subject: &str, handler: &str) -> Result<String, Problem> {
    service_url(ingress, "ActionPointService", subject, handler)
}

/// Build the ingress URL for `handler` on the `service` object keyed `key`,
/// percent-encoding the key.
pub(crate) fn service_url(
    ingress: &str,
    service: &str,
    key: &str,
    handler: &str,
) -> Result<String, Problem> {
    let mut url = reqwest::Url::parse(ingress)
        .map_err(|e| Problem::bad_gateway(format!("bad ingress url: {e}")))?;
    url.path_segments_mut()
        .map_err(|()| Problem::bad_gateway("ingress url cannot be a base"))?
        .pop_if_empty()
        .extend([service, key, handler]);
    Ok(url.into())
}

/// In-memory doubles for the claim protocol's unit tests.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod fakes {
    use std::sync::Mutex;

    use ekg_domain::{ActionExecutor, ExecutionReceipt, PromotedAction, Result as EngineResult};

    use super::*;

    /// A call the [`FakeLedger`] received.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Claim(ClaimCommand),
        Release(String),
        Promote,
        Reverse(String),
        PartialReverse(ReverseProgress),
        Abort(String),
    }

    /// A [`CommandLedger`] that serves a fixed record, grants or refuses the claim, and
    /// records every call.
    pub struct FakeLedger {
        pub record: Option<ActionPointRecord>,
        pub refuse_claim: Option<Problem>,
        pub refuse_record: Option<Problem>,
        pub calls: Mutex<Vec<Call>>,
    }

    impl FakeLedger {
        pub fn with(record: Option<ActionPointRecord>) -> Self {
            Self {
                record,
                refuse_claim: None,
                refuse_record: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("calls").clone()
        }

        fn push(&self, call: Call) {
            self.calls.lock().expect("calls").push(call);
        }

        fn recorded(&self) -> Result<(), Problem> {
            self.refuse_record.clone().map_or(Ok(()), Err)
        }
    }

    #[async_trait]
    impl CommandLedger for FakeLedger {
        async fn record(&self, _subject: &str) -> Result<Option<ActionPointRecord>, Problem> {
            Ok(self.record.clone())
        }
        async fn claim(
            &self,
            _subject: &str,
            command: ClaimCommand,
            _reviewer: &str,
            _expected: &[ActionPointState],
        ) -> Result<String, Problem> {
            self.push(Call::Claim(command));
            self.refuse_claim
                .clone()
                .map_or_else(|| Ok("token-1".to_owned()), Err)
        }
        async fn release(&self, _subject: &str, token: &str, reason: &str) -> Result<(), Problem> {
            assert_eq!(token, "token-1", "releases the claim it holds");
            self.push(Call::Release(reason.to_owned()));
            Ok(())
        }
        async fn promote(
            &self,
            _subject: &str,
            token: &str,
            _promotion: &Promotion,
            _execution_plan: &ExecutionPlan,
            _via: Option<&PromotedVia>,
        ) -> Result<(), Problem> {
            assert_eq!(token, "token-1");
            self.push(Call::Promote);
            self.recorded()
        }
        async fn reverse(
            &self,
            _subject: &str,
            token: &str,
            reversal: &Reversal,
        ) -> Result<(), Problem> {
            assert_eq!(token, "token-1");
            self.push(Call::Reverse(reversal.reason.clone()));
            self.recorded()
        }
        async fn record_partial_reverse(
            &self,
            _subject: &str,
            token: &str,
            progress: &ReverseProgress,
        ) -> Result<(), Problem> {
            assert_eq!(token, "token-1");
            self.push(Call::PartialReverse(progress.clone()));
            self.recorded()
        }
        async fn abort(
            &self,
            _subject: &str,
            token: &str,
            _reviewer: &str,
            reason: &str,
        ) -> Result<(), Problem> {
            assert_eq!(token, "token-1");
            self.push(Call::Abort(reason.to_owned()));
            self.recorded()
        }
    }

    /// An [`ActionExecutor`] that counts executions and fails each action with `fail`
    /// when set. `execute_plan` writes one receipt per action (`fake://odoo/{i}`); a
    /// reverse fails with `fail_reverse` when set, and refuses (a watermark conflict) each
    /// receipt whose `external_ref` is in `refuse_reverse`.
    #[derive(Default)]
    pub struct FakeExecutor {
        pub fail: Option<fn() -> EngineError>,
        pub fail_reverse: Option<fn() -> EngineError>,
        pub refuse_reverse: Mutex<std::collections::BTreeSet<String>>,
        pub executed: Mutex<usize>,
        pub reversed: Mutex<usize>,
        pub reversed_refs: Mutex<Vec<String>>,
    }

    impl FakeExecutor {
        pub fn executed(&self) -> usize {
            *self.executed.lock().expect("executed")
        }
        pub fn reversed(&self) -> usize {
            *self.reversed.lock().expect("reversed")
        }
        /// The receipts reversed, in order.
        pub fn reversed_refs(&self) -> Vec<String> {
            self.reversed_refs.lock().expect("reversed refs").clone()
        }
        /// Refuse (or stop refusing) to reverse the receipt `external_ref`.
        pub fn refuse(&self, external_ref: &str, refuse: bool) {
            let mut set = self.refuse_reverse.lock().expect("refuse");
            if refuse {
                set.insert(external_ref.to_owned());
            } else {
                set.remove(external_ref);
            }
        }
    }

    #[async_trait]
    impl ActionExecutor for FakeExecutor {
        async fn execute(&self, promoted: &PromotedAction) -> EngineResult<ExecutionReceipt> {
            *self.executed.lock().expect("executed") += 1;
            if let Some(fail) = self.fail {
                return Err(fail());
            }
            Ok(ExecutionReceipt {
                action: promoted.action.clone(),
                external_ref: "fake://odoo".to_owned(),
                source_revision: promoted.basis_revision,
                reversible: true,
            })
        }
        async fn reverse(&self, receipt: &ExecutionReceipt) -> EngineResult<()> {
            *self.reversed.lock().expect("reversed") += 1;
            if let Some(fail) = self.fail_reverse {
                return Err(fail());
            }
            if self
                .refuse_reverse
                .lock()
                .expect("refuse")
                .contains(&receipt.external_ref)
            {
                return Err(EngineError::ReverseConflict {
                    external_ref: receipt.external_ref.clone(),
                    executed: 1,
                    found: 2,
                });
            }
            self.reversed_refs
                .lock()
                .expect("reversed refs")
                .push(receipt.external_ref.clone());
            Ok(())
        }
        async fn execute_plan(
            &self,
            promoted: &ekg_domain::PromotedPlan,
        ) -> EngineResult<ExecutionPlan> {
            *self.executed.lock().expect("executed") += 1;
            if let Some(fail) = self.fail {
                return Err(fail());
            }
            Ok(ExecutionPlan {
                actions: promoted.plan.actions.clone(),
                receipts: promoted
                    .plan
                    .actions
                    .iter()
                    .enumerate()
                    .map(|(i, action)| ExecutionReceipt {
                        action: action.clone(),
                        external_ref: format!("fake://odoo/{i}"),
                        source_revision: promoted.basis_revision,
                        reversible: true,
                    })
                    .collect(),
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ekg_domain::{ActionPointId, CorrelationId, Plan, PromotedPlan, ReviewerId, Revision};

    use super::fakes::{Call, FakeExecutor, FakeLedger};
    use super::*;

    fn promoted() -> PromotedPlan {
        PromotedPlan {
            plan: Plan {
                actions: Vec::new(),
            },
            promotion: Promotion {
                action_point: ActionPointId("action-point/s".to_owned()),
                correlation_id: CorrelationId("promote/s".to_owned()),
                reviewer: ReviewerId("planner-1".to_owned()),
                rationale: "why".to_owned(),
                context_seen: String::new(),
            },
            basis_revision: Revision(0),
        }
    }

    #[tokio::test]
    async fn a_failed_write_back_releases_the_claim_with_its_reason() {
        let ledger = FakeLedger::with(None);
        let executor = FakeExecutor {
            fail: Some(|| EngineError::Backend("odoo timed out".to_owned())),
            ..FakeExecutor::default()
        };
        let err = promote_claimed(&ledger, &executor, "s", "token-1", &promoted())
            .await
            .expect_err("the write-back failed");
        assert_eq!(err.status, 502);
        let calls = ledger.calls();
        assert!(
            matches!(calls.as_slice(), [Call::Release(reason)] if reason.contains("odoo timed out")),
            "released with the reason, nothing recorded: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_partially_applied_write_back_releases_and_says_the_source_changed() {
        let ledger = FakeLedger::with(None);
        let executor = FakeExecutor {
            fail: Some(|| EngineError::PlanPartiallyApplied {
                failure: "second action refused".to_owned(),
                unreversed: 1,
                details: "watermark advanced".to_owned(),
            }),
            ..FakeExecutor::default()
        };
        let err = promote_claimed(&ledger, &executor, "s", "token-1", &promoted())
            .await
            .expect_err("partial");
        assert_eq!(err.status, 502);
        let calls = ledger.calls();
        assert!(
            matches!(calls.as_slice(), [Call::Release(reason)] if reason.contains("partly changed")),
            "the release audits the partial change: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_committed_write_back_whose_record_fails_holds_the_claim() {
        let mut ledger = FakeLedger::with(None);
        ledger.refuse_record = Some(Problem::bad_gateway("restate ingress unreachable"));
        let executor = FakeExecutor::default();
        let err = promote_claimed(&ledger, &executor, "s", "token-1", &promoted())
            .await
            .expect_err("the record failed");
        assert_eq!(err.status, 502);
        assert_eq!(
            ledger.calls(),
            vec![Call::Promote],
            "no release: releasing would allow a second write-back"
        );
    }

    #[tokio::test]
    async fn a_successful_promotion_records_once_and_releases_nothing() {
        let ledger = FakeLedger::with(None);
        let executor = FakeExecutor::default();
        promote_claimed(&ledger, &executor, "s", "token-1", &promoted())
            .await
            .expect("promoted");
        assert_eq!(ledger.calls(), vec![Call::Promote]);
        assert_eq!(executor.executed(), 1);
    }

    #[test]
    fn a_refused_claim_maps_to_its_status_and_names_the_holder() {
        let held = claim_refusal(409, "another command is in progress: promote by planner-1");
        assert_eq!(held.status, 409);
        assert!(
            held.detail.contains("promote by planner-1"),
            "{}",
            held.detail
        );
        assert_eq!(claim_refusal(404, "action point not raised").status, 404);
        assert_eq!(claim_refusal(500, "boom").status, 502);
    }

    #[test]
    fn the_ingress_message_is_read_from_a_json_error_body() {
        assert_eq!(
            ingress_message(r#"{"message":"held by planner-1","code":409}"#),
            "held by planner-1"
        );
        assert_eq!(ingress_message("plain"), "plain");
    }
}
