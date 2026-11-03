//! Gated live e2e: promotion criteria are registered with the rollout, a shadow window is tied
//! to the registration it opened under, and the verdict counts only evidence of such a window.
//!
//! Serves the rollout and the shadow store against a live Restate, the rollout under a key of
//! this run so the engine's own rollout is never touched. Shadow records and the paired report
//! are hand-built, so the suite needs no model and no world model.
//!
//! - a registration is recorded with who, why and when, and refused without a reason;
//! - a window opened after it carries its digest, and a registration made while the window is
//!   open leaves that entry alone;
//! - the verdict counts the record that started inside the tied window and leaves out the one
//!   from before it, holds and fails criteria by their numbers, and counts nothing once other
//!   criteria are active;
//! - reading for a verdict changes neither the rollout nor a shadow record.
//!
//! Ignored by default. Skips if Restate is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is
//! set (as `just test-e2e` does), when that fails the run.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it agent_promotion_criteria_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ekg_backend::promotion::{self, Criteria, Held, SetFacts, Verdict};
use ekg_backend::rollout::{
    AgentRollout, IngressError, Mode, RegisterRequest, RolloutClient, RolloutState, SetModeRequest,
};
use ekg_backend::shadow::{
    AgentShadow, Paired, PairedSlate, ShadowClient, ShadowOutcome, ShadowRecord, VersionTuple,
};
use ekg_backend::shadow_report;
use ekg_cognition::eval::{CaseResult, Grade, PairedReport, Report, TrialResult};
use ekg_domain::{
    ActionPointId, ActionType, EntityUri, Plan, ProposedAction, RankedPlan, Score, Slate, Trace,
};
use restate_sdk::prelude::Endpoint;

/// Asks for one passing eval case per slice and one compared record; the second eval
/// threshold cannot be met by the two-case report below.
const DOCUMENT: &str = "\
version: 1
eval:
  set_version: 7
  min_trials: 1
  min_passed: 1
  min_passed_by_category: { late: 2 }
shadow:
  min_compared: 1
  max_legacy_better_percent: 0
";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
}

fn skills() -> BTreeMap<String, String> {
    BTreeMap::from([("propose-plan".to_owned(), "v1".to_owned())])
}

fn option(rank_key: &str, subject: &EntityUri) -> RankedPlan {
    RankedPlan {
        plan: Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("re_promise_date".to_owned()),
                target: subject.clone(),
                params: BTreeMap::from([("new_date".to_owned(), "2027-12-31 00:00:00".to_owned())]),
            }],
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

fn slate(rank_key: &str, subject: &EntityUri) -> Slate {
    Slate {
        baseline: Score {
            metrics: BTreeMap::new(),
            rank_key: "9".to_owned(),
        },
        options: vec![option(rank_key, subject)],
        trace: Trace::from_steps(Vec::new()),
    }
}

/// A completed, paired shadow record of `subject` whose run started at `started_at`; both
/// slates offer the same plan.
fn record(subject: &EntityUri, run: &str, started_at: u64) -> ShadowRecord {
    ShadowRecord {
        run_id: run.to_owned(),
        subject: subject.clone(),
        group: None,
        started_at,
        ended_at: started_at + 1,
        outcome: ShadowOutcome::Completed,
        reason: None,
        slate: Some(slate("1~agent", subject)),
        runs: Vec::new(),
        version: VersionTuple {
            engine: "0.1.0".to_owned(),
            model: "m/1".to_owned(),
            skills: skills(),
            tools: Vec::new(),
        },
        paired: Some(Paired::Point(PairedSlate {
            action_point: ActionPointId(format!("action-point/{}", subject.0)),
            point_seq: 1,
            slate: slate("1~legacy", subject),
            attached_at: "1".to_owned(),
        })),
        paired_missing: None,
        decision: None,
        shown: None,
        epoch: None,
        rollout: None,
    }
}

/// A complete paired report of two cases, both passed by both arms, run under `criteria`.
fn paired(criteria: &str) -> PairedReport {
    let arm = |name: &str| {
        let cases = ["a", "b"]
            .into_iter()
            .map(|id| CaseResult {
                id: id.to_owned(),
                category: if id == "a" { "late" } else { "other" }.to_owned(),
                profile: "alpha".to_owned(),
                baseline: None,
                trials: vec![TrialResult::new(
                    1,
                    "submitted",
                    Grade::Pass,
                    None,
                    None,
                    None,
                )],
                not_run: 0,
            })
            .collect();
        let version = ekg_cognition::eval::VersionTuple {
            engine: "0.1.0".to_owned(),
            model: "m/1".to_owned(),
            skills: if name == "agent" {
                skills()
            } else {
                BTreeMap::new()
            },
            tools: BTreeMap::new(),
            set_version: 7,
            set_digest: "set-d".to_owned(),
            trials: 1,
            max_steps: 0,
            joint_max_steps: 0,
            analyst_max_steps: 10,
            planners: 0,
            planner_reads: false,
        };
        Report::new(name, version, cases, 0)
    };
    PairedReport::new(arm("legacy"), arm("agent"))
        .expect("paired")
        .under(Some(criteria.to_owned()))
}

fn held(verdict: &Verdict, name: &str) -> Held {
    verdict
        .checks
        .iter()
        .find(|check| check.name.contains(name))
        .unwrap_or_else(|| panic!("no check {name:?}: {:#?}", verdict.checks))
        .held
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn a_window_is_evidence_for_the_criteria_it_opened_under_only() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let (service_addr, callback_url) = (common::service_addr(20), common::callback_url(20));
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
    let endpoint = Endpoint::builder()
        .bind(AgentRollout)
        .bind(AgentShadow)
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    common::register(&http, &admin_url, &callback_url)
        .await
        .expect("register");

    let base = common::unique_base();
    // A rollout of this run's own: the engine's, under its fixed key, is never switched.
    let rollout = RolloutClient::new(http.clone(), ingress.clone()).under(format!("test-{base}"));
    let shadow = ShadowClient::new(http.clone(), ingress.clone());
    let subject = EntityUri(format!("https://ekg.example/id/test.promotion/{base}"));
    let request = |document: &str, reason: &str| RegisterRequest {
        document: document.to_owned(),
        by: "planner-1".to_owned(),
        reason: reason.to_owned(),
    };
    let read = || async {
        let state = rollout.get().await.expect("the rollout state");
        let (records, points) =
            shadow_report::read(&http, &ingress, std::slice::from_ref(&subject))
                .await
                .expect("the records");
        (state, records, points)
    };
    let set = SetFacts {
        digest: "set-d".to_owned(),
        case_ids: ["a", "b"].into_iter().map(str::to_owned).collect(),
    };

    // --- registering ---
    let refused = rollout.register_criteria(&request(DOCUMENT, " ")).await;
    assert!(
        matches!(&refused, Err(IngressError::Refused(400, why)) if why.contains("reason")),
        "{refused:?}"
    );
    assert!(rollout.get().await.expect("state").active().is_none());
    let started = now();
    let state: RolloutState = rollout
        .register_criteria(&request(DOCUMENT, "before the window"))
        .await
        .expect("registered");
    let digest = Criteria::digest_of(DOCUMENT);
    let active = state.active().expect("an active registration");
    assert_eq!(active.digest, digest);
    assert_eq!(
        (active.by.as_str(), active.reason.as_str()),
        ("planner-1", "before the window")
    );
    assert!(active.at >= started, "{active:?}");
    assert_eq!(state.mode, Mode::Legacy, "registering switches nothing");

    // --- a window under it ---
    let state = rollout
        .set_mode(&SetModeRequest {
            mode: Mode::Shadow {
                until: now() + 3_600,
                max_slates: 5,
                max_tokens: 1_000,
            },
            by: "planner-1".to_owned(),
            reason: "collect evidence".to_owned(),
        })
        .await
        .expect("shadow");
    assert_eq!(state.history[0].criteria.as_deref(), Some(digest.as_str()));
    let opened = state.history[0].at;

    shadow
        .record(&record(&subject, "before-the-window", opened - 60))
        .await
        .expect("stored");
    shadow
        .record(&record(&subject, "in-the-window", opened))
        .await
        .expect("stored");

    // --- the verdict ---
    let before = read().await;
    let report = paired(&digest);
    let verdict = promotion::verdict(&before.0, &set, Some(&report), &before.1, &before.2, now());
    assert_eq!(read().await, before, "reading for a verdict wrote");
    assert_eq!(verdict.criteria.as_deref(), Some(digest.as_str()));
    assert_eq!(verdict.eval_missing, None);
    assert_eq!((verdict.shadow.counted, verdict.shadow.left_out), (1, 1));
    assert_eq!(
        held(&verdict, "eval: cases the agent passes at k"),
        Held::Held
    );
    assert_eq!(
        held(&verdict, "late cases"),
        Held::Failed,
        "one of the two asked for"
    );
    assert_eq!(held(&verdict, "compared records"), Held::Held);
    assert_eq!(held(&verdict, "existing path ranks better"), Held::Held);
    assert_eq!(held(&verdict, "one version"), Held::Held);
    assert!(!verdict.met, "{}", verdict.table());

    // A report that ran under other criteria is no evidence, whatever it shows.
    let other_report = paired("another-digest");
    let unbound = promotion::verdict(
        &before.0,
        &set,
        Some(&other_report),
        &before.1,
        &before.2,
        now(),
    );
    assert!(
        unbound
            .eval_missing
            .as_deref()
            .is_some_and(|why| why.contains("another-digest"))
    );
    assert_eq!(held(&unbound, "late cases"), Held::NoEvidence);

    // --- other criteria, registered while the window is open ---
    let stricter = DOCUMENT.replace("min_compared: 1", "min_compared: 2");
    let state = rollout
        .register_criteria(&request(&stricter, "stricter"))
        .await
        .expect("registered");
    assert_eq!(state.registrations.len(), 2);
    assert_eq!(
        state.history[0].criteria.as_deref(),
        Some(digest.as_str()),
        "the open window keeps the criteria it opened under"
    );
    assert!(matches!(state.mode, Mode::Shadow { .. }));
    let after = read().await;
    let orphaned = promotion::verdict(&after.0, &set, Some(&report), &after.1, &after.2, now());
    assert_eq!(
        orphaned.criteria.as_deref(),
        Some(Criteria::digest_of(&stricter).as_str())
    );
    assert!(orphaned.shadow.windows.is_empty());
    assert_eq!((orphaned.shadow.counted, orphaned.shadow.left_out), (0, 2));
    assert_eq!(held(&orphaned, "compared records"), Held::Failed);
    assert!(
        orphaned.eval_missing.is_some(),
        "the report ran under the first criteria"
    );

    // The window is closed again; the rollout of this run is left in `legacy`.
    rollout
        .set_mode(&SetModeRequest {
            mode: Mode::Legacy,
            by: "planner-1".to_owned(),
            reason: "agent_promotion_criteria_e2e_it: cleanup".to_owned(),
        })
        .await
        .expect("legacy");
    common::deregister(&http, &admin_url, &callback_url).await;
}
