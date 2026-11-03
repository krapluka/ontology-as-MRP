//! Gated live e2e: the decision on a point reaches the shadow records paired with it.
//!
//! Serves the action-point object with the shadow store as its outcome listener, and the
//! shadow store itself, against a live Restate. Points are driven through the object's
//! handlers directly, with hand-built proposals, slates and executed plans, so the suite
//! needs no model, no world model and no system of record, and writes to none.
//!
//! - a promotion lands on the paired record, naming the option whose actions were executed,
//!   and a later reversal is added to it;
//! - an abort lands with its reviewer and no actions;
//! - when the subject drifts again, the first point's record keeps its decision and the
//!   second point's record gets its own;
//! - the point's own audit trail is what it is without a listener;
//! - a subject that was never shadowed gets no shadow state;
//! - the paired report over these records shows each decision beside both slates, reads the
//!   decision of a record that has none from its point, and changes neither records nor points.
//!
//! Ignored by default. Skips if Restate is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is
//! set (as `just test-e2e` does), when that fails the run.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it agent_shadow_pairing_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::time::Duration;

use ekg_backend::shadow::{
    AGENT_SHADOW_OBJECT, AgentShadow, DecisionState, Paired, PairedSlate, ShadowClient,
    ShadowOutcome, ShadowRecord, VersionTuple,
};
use ekg_backend::shadow_report::{self, Compared, Comparison, Executed, ShadowReport};
use ekg_domain::{
    ActionPointState, ActionType, CorrelationId, EntityUri, ExecutionPlan, ExecutionReceipt,
    GroundedProposal, Plan, Promotion, Proposal, ProposedAction, RankedPlan, Reversal, ReviewerId,
    Revision, Score, Slate, Trace,
};
use ekg_dynamic::{
    AbortRequest, ActionPointRecord, ActionPointService, ClaimCommand, ClaimRequest, OpenRequest,
    PromoteRequest, ReverseRequest,
};
use restate_sdk::prelude::Endpoint;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// How long a decision may take to reach the shadow store.
const SETTLE: Duration = Duration::from_secs(20);

/// The live objects, over the ingress.
struct Live {
    http: reqwest::Client,
    ingress: String,
}

impl Live {
    /// Calls a handler of `subject`'s action point and decodes its answer.
    async fn point<Req: Serialize + Sync, Res: DeserializeOwned>(
        &self,
        subject: &EntityUri,
        handler: &str,
        body: Option<&Req>,
    ) -> Res {
        let mut url = reqwest::Url::parse(&self.ingress).expect("ingress url");
        url.path_segments_mut()
            .expect("base url")
            .pop_if_empty()
            .extend(["ActionPointService", &subject.0, handler]);
        let mut request = self.http.post(url);
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(serde_json::to_vec(body).expect("encode"));
        }
        let response = request.send().await.expect("the ingress answers");
        let status = response.status();
        let text = response.text().await.expect("a body");
        assert!(status.is_success(), "{handler} answered {status}: {text}");
        let text = if text.is_empty() { "null" } else { &text };
        serde_json::from_str(text).unwrap_or_else(|why| panic!("{handler} answered {text}: {why}"))
    }

    async fn record(&self, subject: &EntityUri) -> ActionPointRecord {
        self.point::<(), Option<ActionPointRecord>>(subject, "record", None)
            .await
            .expect("a point was raised")
    }

    async fn claim(
        &self,
        subject: &EntityUri,
        command: ClaimCommand,
        from: ActionPointState,
    ) -> String {
        self.point(
            subject,
            "claim",
            Some(&ClaimRequest {
                command,
                reviewer: "planner-1".to_owned(),
                expected_state: vec![from],
                deadline_secs: 600,
            }),
        )
        .await
    }

    fn shadow(&self) -> ShadowClient {
        ShadowClient::new(self.http.clone(), self.ingress.clone())
    }

    /// The subject's shadow records once `settled` holds for them.
    async fn records_when(
        &self,
        subject: &EntityUri,
        settled: impl Fn(&[ShadowRecord]) -> bool,
    ) -> Vec<ShadowRecord> {
        let shadow = self.shadow();
        let reached = common::wait_until(SETTLE, async || {
            shadow
                .records(subject)
                .await
                .is_ok_and(|records| settled(&records))
        })
        .await;
        let records = shadow.records(subject).await.expect("records");
        assert!(reached, "the decision did not reach the store: {records:?}");
        records
    }
}

fn action(subject: &EntityUri, new_date: &str) -> ProposedAction {
    ProposedAction {
        action_type: ActionType("re_promise_date".to_owned()),
        target: subject.clone(),
        params: BTreeMap::from([("new_date".to_owned(), new_date.to_owned())]),
    }
}

fn option(rank_key: &str, action: ProposedAction) -> RankedPlan {
    RankedPlan {
        plan: Plan {
            actions: vec![action],
        },
        score: Score {
            metrics: BTreeMap::new(),
            rank_key: rank_key.to_owned(),
        },
        recommended: false,
        needs: Vec::new(),
        omissions: Vec::new(),
        trace: Trace::from_steps(Vec::new()),
    }
}

/// A slate offering to re-promise `subject` to an early or a late date.
fn slate(subject: &EntityUri) -> Slate {
    Slate {
        baseline: Score {
            metrics: BTreeMap::new(),
            rank_key: "9".to_owned(),
        },
        options: vec![
            option("1~early", action(subject, "2027-11-08 00:00:00")),
            option("2~late", action(subject, "2027-11-30 00:00:00")),
        ],
        trace: Trace::from_steps(Vec::new()),
    }
}

/// The slate the agent's shadow run produced: the late date first, then a later one.
fn agent_slate(subject: &EntityUri) -> Slate {
    Slate {
        options: vec![
            option("1~agent-late", action(subject, "2027-11-30 00:00:00")),
            option("3~agent-later", action(subject, "2027-12-31 00:00:00")),
        ],
        ..slate(subject)
    }
}

/// The records and the point records of `subjects`, as they are now.
async fn snapshot(
    live: &Live,
    subjects: &[EntityUri],
) -> (Vec<Vec<ShadowRecord>>, Vec<ActionPointRecord>) {
    let (mut records, mut points) = (Vec::new(), Vec::new());
    for subject in subjects {
        records.push(live.shadow().records(subject).await.expect("records"));
        points.push(live.record(subject).await);
    }
    (records, points)
}

/// The paired report over the records of `subjects`.
async fn report(live: &Live, subjects: &[EntityUri]) -> ShadowReport {
    let (records, points) = shadow_report::read(&live.http, &live.ingress, subjects)
        .await
        .expect("the records and points are read");
    shadow_report::build(&records, &points)
}

/// The compared row of the run `run`.
fn compared<'r>(report: &'r ShadowReport, run: &str) -> &'r Compared {
    let row = report
        .rows
        .iter()
        .find(|row| row.run_id == run)
        .unwrap_or_else(|| panic!("no row for {run}: {report:?}"));
    row.outcome
        .as_ref()
        .unwrap_or_else(|why| panic!("{run} was not compared: {why:?}"))
}

/// Opens a point for `subject` at `revision`, attaches a proposal and the slate, and returns
/// the point's record: the point is `proposed` and carries the slate.
async fn open_with_slate(live: &Live, subject: &EntityUri, revision: u64) -> ActionPointRecord {
    let _: serde_json::Value = live
        .point(
            subject,
            "open",
            Some(&OpenRequest {
                reason: "battery shortfall before the promise".to_owned(),
                revision,
            }),
        )
        .await;
    let grounded = GroundedProposal {
        proposal: Proposal::single(
            "remediate the battery delay".to_owned(),
            action(subject, "2027-11-30 00:00:00"),
            Vec::new(),
        ),
        grounding_omissions: Vec::new(),
        grounding_edges: Vec::new(),
        subject_members: Vec::new(),
    };
    let () = live
        .point(subject, "attach_proposal", Some(&grounded))
        .await;
    let () = live
        .point(subject, "attach_slate", Some(&slate(subject)))
        .await;
    let record = live.record(subject).await;
    assert_eq!(record.state(), Some(ActionPointState::Proposed));
    record
}

/// Records a shadow run beside the point in `record`, as the slate step does.
async fn shadow_beside(live: &Live, subject: &EntityUri, record: &ActionPointRecord, run: &str) {
    let point = record.point.as_ref().expect("a point");
    live.shadow()
        .record(&ShadowRecord {
            run_id: run.to_owned(),
            subject: subject.clone(),
            group: None,
            started_at: 10,
            ended_at: 20,
            outcome: ShadowOutcome::Completed,
            reason: None,
            slate: Some(agent_slate(subject)),
            runs: Vec::new(),
            version: VersionTuple::default(),
            paired: Some(Paired::Point(PairedSlate {
                action_point: point.id.clone(),
                point_seq: record.points_opened,
                slate: record.slate.clone().expect("the attached slate"),
                attached_at: record.slate_attached_at.clone().expect("an attach time"),
            })),
            paired_missing: None,
            decision: None,
            shown: None,
            epoch: None,
            rollout: None,
        })
        .await
        .expect("the shadow record is stored");
}

/// Promotes `subject`'s point with the late option's actions, as a command would after its
/// write-back.
async fn promote(live: &Live, subject: &EntityUri) {
    let token = live
        .claim(subject, ClaimCommand::Promote, ActionPointState::Proposed)
        .await;
    let executed = action(subject, "2027-11-30 00:00:00");
    let point = live.record(subject).await.point.expect("a point").id;
    let () = live
        .point(
            subject,
            "promote",
            Some(&PromoteRequest {
                token,
                promotion: Promotion {
                    action_point: point,
                    correlation_id: CorrelationId(format!("corr/{}", subject.0)),
                    reviewer: ReviewerId("planner-1".to_owned()),
                    rationale: "shortfall confirmed; re-promise approved".to_owned(),
                    context_seen: "evidence subgraph, no omissions".to_owned(),
                },
                execution_plan: ExecutionPlan {
                    actions: vec![executed.clone()],
                    receipts: vec![ExecutionReceipt {
                        action: executed,
                        external_ref: "test/none".to_owned(),
                        source_revision: Revision(1),
                        reversible: true,
                    }],
                },
                promoted_via: None,
            }),
        )
        .await;
}

async fn abort(live: &Live, subject: &EntityUri, reviewer: &str) {
    let token = live
        .claim(subject, ClaimCommand::Abort, ActionPointState::Proposed)
        .await;
    let () = live
        .point(
            subject,
            "abort",
            Some(&AbortRequest {
                token,
                reviewer: reviewer.to_owned(),
                reason: Some("handled manually".to_owned()),
            }),
        )
        .await;
}

/// The states of the point's own transitions, in order (command and slate entries left out).
fn transitions(record: &ActionPointRecord) -> Vec<ActionPointState> {
    let mut states: Vec<ActionPointState> = Vec::new();
    for entry in record.audit.iter().filter(|entry| entry.kind.is_none()) {
        if states.last() != Some(&entry.state) {
            states.push(entry.state);
        }
    }
    states
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn a_decision_reaches_the_shadow_records_of_its_point_only() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let (service_addr, callback_url) = (common::service_addr(19), common::callback_url(19));
    let http = reqwest::Client::new();
    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        common::infra_unavailable(&format!("Restate admin unreachable at {admin_url}"));
        return;
    }

    // The two objects, wired as the backend wires them.
    let endpoint = Endpoint::builder()
        .bind(ActionPointService::default().with_outcome_listener(AGENT_SHADOW_OBJECT))
        .bind(AgentShadow)
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    common::register(&http, &admin_url, &callback_url)
        .await
        .expect("register");
    let live = Live { http, ingress };

    let base = common::unique_base();
    // Not a `sale.order`: these points have no entity behind them, and the suites that pick a
    // seeded point from the live `/api` must not pick one of them.
    let subject = |offset: i64| {
        EntityUri(format!(
            "https://ekg.example/id/test.shadow-pairing/{}",
            base + offset
        ))
    };
    let (promoted, aborted, unshadowed) = (subject(0), subject(1), subject(2));

    // --- a promotion, then its reversal ---
    let record = open_with_slate(&live, &promoted, 1).await;
    shadow_beside(&live, &promoted, &record, "promoted-1").await;
    promote(&live, &promoted).await;
    let records = live
        .records_when(&promoted, |records| {
            records.iter().all(|r| r.decision.is_some())
        })
        .await;
    let decision = records[0].decision.clone().expect("a decision");
    assert_eq!(decision.state, DecisionState::Promoted);
    assert_eq!(decision.reviewer.as_deref(), Some("planner-1"));
    assert_eq!(
        decision.actions,
        vec![action(&promoted, "2027-11-30 00:00:00")]
    );
    assert_eq!(decision.option_rank_key.as_deref(), Some("2~late"));
    assert!(decision.on_paired_slate);
    assert!(decision.decided_at > 0);
    assert_eq!(decision.reversed_at, None);
    // The point's own trail is what it is without a listener.
    let after = live.record(&promoted).await;
    assert_eq!(
        transitions(&after),
        [
            ActionPointState::Open,
            ActionPointState::Proposed,
            ActionPointState::UnderReview,
            ActionPointState::Promoted,
        ]
    );
    assert!(after.claim.is_none());

    let token = live
        .claim(&promoted, ClaimCommand::Reverse, ActionPointState::Promoted)
        .await;
    let () = live
        .point(
            &promoted,
            "reverse",
            Some(&ReverseRequest {
                token,
                reversal: Reversal {
                    action_point: after.point.expect("a point").id,
                    correlation_id: CorrelationId(format!("corr/{}", promoted.0)),
                    reviewer: ReviewerId("planner-2".to_owned()),
                    reason: "wrong date".to_owned(),
                },
            }),
        )
        .await;
    let records = live
        .records_when(&promoted, |records| {
            records
                .iter()
                .all(|r| r.decision.as_ref().is_some_and(|d| d.reversed_at.is_some()))
        })
        .await;
    let reversed = records[0].decision.clone().expect("a decision");
    assert_eq!(
        reversed.state,
        DecisionState::Promoted,
        "the promotion stays"
    );
    assert_eq!(reversed.actions, decision.actions);
    assert_eq!(reversed.reviewer, decision.reviewer);

    // --- an abort, then the subject drifts again ---
    let first = open_with_slate(&live, &aborted, 1).await;
    shadow_beside(&live, &aborted, &first, "aborted-1").await;
    abort(&live, &aborted, "planner-1").await;
    let records = live
        .records_when(&aborted, |records| {
            records.iter().all(|r| r.decision.is_some())
        })
        .await;
    let first_decision = records[0].decision.clone().expect("a decision");
    assert_eq!(first_decision.state, DecisionState::Aborted);
    assert_eq!(first_decision.reviewer.as_deref(), Some("planner-1"));
    assert!(first_decision.actions.is_empty());
    assert_eq!(first_decision.option_rank_key, None);

    let second = open_with_slate(&live, &aborted, 2).await;
    assert_eq!(
        second.point.as_ref().map(|p| &p.id),
        first.point.as_ref().map(|p| &p.id),
        "a subject's points share one id"
    );
    assert_eq!(second.points_opened, first.points_opened + 1);
    shadow_beside(&live, &aborted, &second, "aborted-2").await;
    abort(&live, &aborted, "planner-2").await;
    let records = live
        .records_when(&aborted, |records| {
            records.len() == 2 && records.iter().all(|r| r.decision.is_some())
        })
        .await;
    assert_eq!(records[0].run_id, "aborted-1");
    assert_eq!(
        records[0].decision.as_ref(),
        Some(&first_decision),
        "the first point's record keeps its decision"
    );
    assert_eq!(
        records[1]
            .decision
            .as_ref()
            .and_then(|d| d.reviewer.as_deref()),
        Some("planner-2"),
        "the second decision is on the second point's record"
    );

    // --- a subject that was never shadowed gets no shadow state ---
    open_with_slate(&live, &unshadowed, 1).await;
    promote(&live, &unshadowed).await;
    // The promotions above settled within this time; give this one as long.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let state = common::admin_rows(
        &live.http,
        &admin_url,
        &format!(
            "SELECT key FROM state WHERE service_name = '{AGENT_SHADOW_OBJECT}' \
             AND service_key = '{}'",
            unshadowed.0
        ),
    )
    .await
    .expect("state query");
    assert!(
        state.is_empty(),
        "no shadow state for {}: {state:?}",
        unshadowed.0
    );
    assert_eq!(
        live.record(&unshadowed).await.state(),
        Some(ActionPointState::Promoted)
    );

    // --- the paired report over these records ---
    let shadowed = [promoted.clone(), aborted.clone()];
    let listed = shadow_report::subjects(&live.http, &admin_url)
        .await
        .expect("the admin endpoint lists the subjects");
    assert!(shadowed.iter().all(|subject| listed.contains(subject)));
    assert!(!listed.contains(&unshadowed));

    let before = snapshot(&live, &shadowed).await;
    let paired = report(&live, &shadowed).await;
    assert_eq!(snapshot(&live, &shadowed).await, before, "the report wrote");

    let row = compared(&paired, "promoted-1");
    // Both best options score the capability key 1 and are different plans.
    assert_eq!(row.comparison, Comparison::EqualRank);
    let shown = row.decision.as_ref().expect("the promotion");
    assert_eq!(shown.state, DecisionState::Promoted);
    assert!(shown.reversed && shown.on_paired_slate && !shown.from_point);
    assert_eq!(
        shown.executed,
        Some(Executed {
            legacy_option: Some(2),
            agent_option: Some(1)
        }),
        "the existing slate's second option, the agent's best"
    );
    for run in ["aborted-1", "aborted-2"] {
        let shown = compared(&paired, run).decision.as_ref().expect("the abort");
        assert_eq!(
            (shown.state, shown.executed),
            (DecisionState::Aborted, None)
        );
    }
    let totals = &paired.totals;
    assert_eq!(
        (totals.records, totals.compared, totals.equal_rank),
        (3, 3, 3)
    );
    assert_eq!(
        totals.decisions,
        BTreeMap::from([("promoted".to_owned(), 1), ("aborted".to_owned(), 2)])
    );
    assert_eq!(
        (totals.undecided, totals.reversed, totals.from_point),
        (0, 1, 0)
    );
    assert_eq!(
        (
            totals.promotions,
            totals.executed_legacy_best,
            totals.executed_offered_by_agent,
            totals.executed_agent_best
        ),
        (1, 0, 1, 1)
    );

    // A record stored after its point was decided never gets the decision: the report reads
    // it from the point, as long as the point is still that one.
    shadow_beside(&live, &promoted, &record, "promoted-late").await;
    shadow_beside(&live, &aborted, &second, "aborted-late").await;
    // The first point of this subject was replaced by the second.
    shadow_beside(&live, &aborted, &first, "aborted-replaced").await;
    let before = snapshot(&live, &shadowed).await;
    let paired = report(&live, &shadowed).await;
    assert_eq!(snapshot(&live, &shadowed).await, before, "the report wrote");
    assert!(
        before.0.iter().flatten().any(|r| r.decision.is_none()),
        "the late records stay without a decision in the store"
    );

    let late = compared(&paired, "promoted-late")
        .decision
        .as_ref()
        .expect("read from the point");
    assert!(late.from_point && late.reversed && late.on_paired_slate);
    assert_eq!(late.state, DecisionState::Promoted);
    assert_eq!(late.executed.and_then(|e| e.legacy_option), Some(2));
    let late = compared(&paired, "aborted-late")
        .decision
        .as_ref()
        .expect("read from the point");
    assert!(late.from_point);
    assert_eq!(late.state, DecisionState::Aborted);
    let replaced = compared(&paired, "aborted-replaced");
    assert!(replaced.decision.is_none() && replaced.point_replaced);
    assert_eq!(
        (
            paired.totals.records,
            paired.totals.from_point,
            paired.totals.undecided
        ),
        (6, 2, 1)
    );

    common::deregister(&live.http, &admin_url, &callback_url).await;
}
