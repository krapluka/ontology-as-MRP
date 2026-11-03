//! Gated live e2e: the `agent` mode is entered only with a sign-off for the active criteria,
//! and rolls itself back.
//!
//! Serves the rollout and the shadow store against a live Restate, the rollout under a key of
//! this run so the engine's own rollout is never touched. Shadow records and the paired report
//! are hand-built, so the suite needs no model and no world model.
//!
//! - a sign-off is refused against an unmet verdict and from the one who registered the
//!   criteria, and recorded otherwise, switching nothing;
//! - `agent` is refused before the sign-off and entered after it, naming it; a claim in it is
//!   authoritative;
//! - a run of another skill version, failures over the bound, a reversed promotion reported by
//!   the shadow store, and other criteria each roll the mode back, and the history names the
//!   trigger.
//!
//! A second test is the joint slate's gate: under criteria with joint bounds and a sign-off, a
//! portfolio formed in `agent` mode carries the agent's joint slate (scripted), promoting it
//! records the decision on the joint shadow record, and undoing it rolls the mode back.
//!
//! Ignored by default. Skips if Restate is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is
//! set (as `just test-e2e` does), when that fails the run.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it agent_promotion_gate_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ekg_backend::promotion::{self, Criteria, SetFacts, Verdict, Version};
use ekg_backend::rollout::{
    AgentRollout, Claim, IngressError, Mode, RegisterRequest, RolloutClient, RolloutState,
    RunReport, RunVerdict, SetModeRequest, SignoffRequest,
};
use ekg_backend::shadow::{
    AgentShadow, Paired, PairedSlate, ShadowClient, ShadowOutcome, ShadowRecord, Shown,
    VersionTuple,
};
use ekg_backend::shadow_report;
use ekg_cognition::eval::{CaseResult, Grade, PairedReport, Report, TrialResult};
use ekg_domain::{
    ActionPointId, ActionPointState, ActionType, EntityUri, Plan, ProposedAction, RankedPlan,
    ReviewerId, Score, Slate, Trace,
};
use ekg_dynamic::PointOutcome;
use restate_sdk::prelude::Endpoint;

/// One passing eval case and one compared record meet these; one failed run in a row and no
/// reversed promotion are allowed in `agent` mode.
const DOCUMENT: &str = "\
version: 1
eval:
  set_version: 7
  min_trials: 1
  min_passed: 1
shadow:
  min_compared: 1
rollback:
  max_consecutive_agent_failures: 1
  max_reversed_promotions: 0
";

const REGISTRANT: &str = "planner-1";
const SIGNER: &str = "planner-2";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
}

fn skills(version: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("propose-plan".to_owned(), version.to_owned())])
}

fn action(subject: &EntityUri) -> ProposedAction {
    ProposedAction {
        action_type: ActionType("re_promise_date".to_owned()),
        target: subject.clone(),
        params: BTreeMap::from([("new_date".to_owned(), "2027-12-31 00:00:00".to_owned())]),
    }
}

fn slate(rank_key: &str, subject: &EntityUri) -> Slate {
    Slate {
        baseline: Score {
            metrics: BTreeMap::new(),
            rank_key: "9".to_owned(),
        },
        options: vec![RankedPlan {
            plan: Plan {
                actions: vec![action(subject)],
            },
            score: Score {
                metrics: BTreeMap::new(),
                rank_key: rank_key.to_owned(),
            },
            recommended: false,
            needs: Vec::new(),
            omissions: Vec::new(),
            trace: Trace::from_steps(Vec::new()),
        }],
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
            skills: skills("v1"),
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
                category: "late".to_owned(),
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
                skills("v1")
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

fn signoff(verdict: Verdict, by: &str) -> SignoffRequest {
    SignoffRequest {
        verdict,
        report: "report-digest".to_owned(),
        by: by.to_owned(),
        reason: "the evidence holds".to_owned(),
    }
}

fn agent_mode() -> SetModeRequest {
    SetModeRequest {
        mode: Mode::Agent {
            until: now() + 3_600,
            max_slates: 20,
            max_tokens: 1_000_000,
        },
        by: REGISTRANT.to_owned(),
        reason: "the criteria are met and signed".to_owned(),
    }
}

fn run(epoch: u32, completed: bool, skill: &str) -> RunReport {
    RunReport {
        epoch,
        tokens: 100,
        completed,
        version: Version {
            engine: "0.1.0".to_owned(),
            model: "m/1".to_owned(),
            skills: skills(skill),
        },
    }
}

/// The last switch of `state`, when a trigger made it: the trigger's name.
fn rolled_back_by(state: &RolloutState) -> Option<&str> {
    let last = state.history.last()?;
    (state.mode == Mode::Legacy && last.mode == Mode::Legacy)
        .then(|| last.by.strip_prefix("trigger:"))
        .flatten()
}

fn refused(result: &Result<RolloutState, IngressError>, parts: &[&str]) -> bool {
    matches!(result, Err(IngressError::Refused(400, why)) if parts.iter().all(|part| why.contains(part)))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn the_agent_mode_needs_a_sign_off_and_rolls_itself_back() {
    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let (service_addr, callback_url) = (common::service_addr(21), common::callback_url(21));
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
    let key = format!("test-{base}");
    let rollout = RolloutClient::new(http.clone(), ingress.clone()).under(key.clone());
    let shadow = ShadowClient::new(http.clone(), ingress.clone());
    let subject = EntityUri(format!("https://ekg.example/id/test.gate/{base}"));
    let set = SetFacts {
        digest: "set-d".to_owned(),
        case_ids: ["a", "b"].into_iter().map(str::to_owned).collect(),
    };
    let judge = || async {
        let state = rollout.get().await.expect("the rollout state");
        let (records, points) =
            shadow_report::read(&http, &ingress, std::slice::from_ref(&subject))
                .await
                .expect("the records");
        let digest = state.active().expect("registered").digest.clone();
        promotion::verdict(
            &state,
            &set,
            Some(&paired(&digest)),
            &records,
            &points,
            now(),
        )
    };

    // --- criteria, and a shadow window under them ---
    rollout
        .register_criteria(&RegisterRequest {
            document: DOCUMENT.to_owned(),
            by: REGISTRANT.to_owned(),
            reason: "before the window".to_owned(),
        })
        .await
        .expect("registered");
    let digest = Criteria::digest_of(DOCUMENT);
    let unsigned = rollout.set_mode(&agent_mode()).await;
    assert!(refused(&unsigned, &["sign-off", &digest]), "{unsigned:?}");
    let state = rollout
        .set_mode(&SetModeRequest {
            mode: Mode::Shadow {
                until: now() + 3_600,
                max_slates: 5,
                max_tokens: 1_000,
            },
            by: REGISTRANT.to_owned(),
            reason: "collect evidence".to_owned(),
        })
        .await
        .expect("shadow");
    let opened = state.history[0].at;

    // --- the sign-off ---
    // No record yet: the verdict is not met, and cannot be signed off.
    let unmet = judge().await;
    assert!(!unmet.met, "{}", unmet.table());
    let refusal = rollout.sign_off(&signoff(unmet, SIGNER)).await;
    assert!(
        refused(&refusal, &["not met", "shadow: compared records"]),
        "{refusal:?}"
    );
    shadow
        .record(&record(&subject, "in-the-window", opened))
        .await
        .expect("stored");
    let met = judge().await;
    assert!(met.met, "{}", met.table());
    // The one who registered the criteria cannot sign them off.
    let own = rollout.sign_off(&signoff(met.clone(), REGISTRANT)).await;
    assert!(refused(&own, &["another operator has to sign"]), "{own:?}");
    let before = rollout.get().await.expect("state");
    assert!(
        before.signoffs.is_empty(),
        "a refused sign-off records nothing"
    );

    let signed_at = now();
    let state = rollout
        .sign_off(&signoff(met.clone(), SIGNER))
        .await
        .expect("signed off");
    let recorded = state.signoffs.last().expect("the sign-off");
    assert_eq!(recorded.criteria, digest);
    assert_eq!(recorded.report, "report-digest");
    assert_eq!(recorded.verdict, met);
    assert_eq!(recorded.version.model, "m/1");
    assert_eq!(recorded.version.skills, skills("v1"));
    assert_eq!(
        (recorded.by.as_str(), recorded.reason.as_str()),
        (SIGNER, "the evidence holds")
    );
    assert!(recorded.at >= signed_at, "{recorded:?}");
    assert!(
        matches!(state.mode, Mode::Shadow { .. }),
        "a sign-off switches nothing"
    );
    assert_eq!(state.history, before.history);

    // --- entering the mode ---
    let enter = || async {
        let state = rollout.set_mode(&agent_mode()).await.expect("agent");
        assert!(matches!(state.mode, Mode::Agent { .. }));
        let entry = state.history.last().expect("the switch").clone();
        assert_eq!(entry.signoff, Some(recorded.at), "it names its sign-off");
        assert_eq!(entry.criteria.as_deref(), Some(digest.as_str()));
        state.epoch()
    };
    let epoch = enter().await;
    assert_eq!(
        rollout
            .claim_slate(ekg_backend::rollout::SlateKind::Point)
            .await
            .expect("claim"),
        Claim::Granted {
            epoch,
            slate: 1,
            authoritative: true
        }
    );
    assert_eq!(
        rollout
            .report_run(&run(epoch, true, "v1"))
            .await
            .expect("report"),
        RunVerdict::Proceed
    );

    // --- a run of another skill version ---
    let verdict = rollout
        .report_run(&run(epoch, true, "v2"))
        .await
        .expect("report");
    assert!(
        matches!(&verdict, RunVerdict::RolledBack { reason } if reason.contains("skill propose-plan: v1, v2")),
        "{verdict:?}"
    );
    let state = rollout.get().await.expect("state");
    assert_eq!(rolled_back_by(&state), Some("version_changed"));
    assert!(
        state
            .history
            .last()
            .is_some_and(|entry| entry.reason.contains("skill propose-plan: v1, v2")),
        "{:?}",
        state.history.last()
    );
    // A run still in flight from that epoch is stale, and no new one starts.
    assert_eq!(
        rollout
            .report_run(&run(epoch, true, "v1"))
            .await
            .expect("report"),
        RunVerdict::Stale
    );
    assert!(matches!(
        rollout.claim_slate(ekg_backend::rollout::SlateKind::Point).await.expect("claim"),
        Claim::Refused { reason } if reason == "legacy"
    ));

    // --- failures in a row ---
    let epoch = enter().await;
    for expected in [None, Some("agent_failures")] {
        assert_eq!(
            rollout
                .report_run(&run(epoch, false, "v1"))
                .await
                .expect("report"),
            RunVerdict::Fallback
        );
        let state = rollout.get().await.expect("state");
        assert_eq!(
            rolled_back_by(&state),
            expected,
            "{:?}",
            state.history.last()
        );
    }

    // --- a reversed promotion, reported by the shadow store ---
    let epoch = enter().await;
    let mut shown = record(&subject, "agent-shown", now());
    shown.shown = Some(Shown::Agent);
    shown.epoch = Some(epoch);
    shown.rollout = Some(key.clone());
    if let Some(Paired::Point(paired)) = shown.paired.as_mut() {
        paired.point_seq = 2;
    }
    shadow.record(&shown).await.expect("stored");
    let outcome = |state: ActionPointState| PointOutcome {
        action_point: ActionPointId(format!("action-point/{}", subject.0)),
        point_seq: 2,
        state,
        at: now(),
        reviewer: Some(ReviewerId("planner-3".to_owned())),
        actions: if state == ActionPointState::Promoted {
            vec![action(&subject)]
        } else {
            Vec::new()
        },
        promoted_via: None,
        slate_attached_at: Some("1".to_owned()),
    };
    shadow
        .decide(&subject, &outcome(ActionPointState::Promoted))
        .await
        .expect("promoted");
    let state = rollout.get().await.expect("state");
    assert!(
        matches!(state.mode, Mode::Agent { .. }),
        "a promotion is no reversal"
    );
    shadow
        .decide(&subject, &outcome(ActionPointState::Reversed))
        .await
        .expect("reversed");
    // The shadow store reports one-way; the rollout switches when the report arrives.
    let switched = common::wait_until(Duration::from_secs(20), async || {
        rollout
            .get()
            .await
            .is_ok_and(|state| state.mode == Mode::Legacy)
    })
    .await;
    assert!(switched, "the reversal did not roll the mode back");
    let state = rollout.get().await.expect("state");
    assert_eq!(rolled_back_by(&state), Some("promotion_reversed"));
    let stored = shadow.records(&subject).await.expect("records");
    assert!(
        stored.iter().any(|record| record.run_id == "agent-shown"
            && record
                .decision
                .as_ref()
                .is_some_and(|d| d.reversed_at.is_some())),
        "the decision and its reversal are on the record"
    );

    // --- other criteria ---
    enter().await;
    let stricter = DOCUMENT.replace("min_compared: 1", "min_compared: 2");
    let state = rollout
        .register_criteria(&RegisterRequest {
            document: stricter.clone(),
            by: REGISTRANT.to_owned(),
            reason: "stricter".to_owned(),
        })
        .await
        .expect("registered");
    assert_eq!(state.registrations.len(), 2, "the registration is recorded");
    assert_eq!(rolled_back_by(&state), Some("criteria_changed"));
    // The sign-off is for the earlier criteria: it does not open the mode again.
    let again = rollout.set_mode(&agent_mode()).await;
    assert!(
        refused(&again, &[&digest, &Criteria::digest_of(&stricter)]),
        "{again:?}"
    );

    // Every way in and out is in the one history, in order.
    let state = rollout.get().await.expect("state");
    let history: Vec<(&str, &str)> = state
        .history
        .iter()
        .map(|entry| (entry.mode.name(), entry.by.as_str()))
        .collect();
    assert_eq!(
        history,
        [
            ("shadow", REGISTRANT),
            ("agent", REGISTRANT),
            ("legacy", "trigger:version_changed"),
            ("agent", REGISTRANT),
            ("legacy", "trigger:agent_failures"),
            ("agent", REGISTRANT),
            ("legacy", "trigger:promotion_reversed"),
            ("agent", REGISTRANT),
            ("legacy", "trigger:criteria_changed"),
        ]
    );
    common::deregister(&http, &admin_url, &callback_url).await;
}

/// Criteria in the format that states joint bounds, allowing no reversed promotion.
const JOINT_DOCUMENT: &str = "\
version: 2
eval:
  set_version: 7
  min_trials: 1
  min_passed: 1
shadow:
  min_compared: 1
joint:
  eval:
    min_passed: 1
  shadow:
    min_compared: 1
rollback:
  max_consecutive_agent_failures: 1
  max_reversed_promotions: 0
";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn under_joint_criteria_a_portfolio_carries_the_agents_joint_slate_and_its_undo_rolls_back() {
    use std::sync::Arc;

    use ekg_backend::shadow::{AGENT_SHADOW_OBJECT, DecisionState};
    use ekg_backend::slates::{IngressSink, PortfolioSlateOutcome, ShadowStep, portfolio_slates};
    use ekg_domain::fakes::{CannedGroupSlateProposer, InMemoryWorldModel};
    use ekg_dynamic::{PortfolioEvent, PortfolioService, PortfolioState};
    use ekg_kinetic::PolicyEngine;

    // One test deployment per binary at a time (see `common::deployment_slot`).
    let _deployment = common::deployment_slot().await.expect("deployment slot");
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let (service_addr, callback_url) = (common::service_addr(23), common::callback_url(23));
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
        .bind(PortfolioService::default().with_outcome_listener(AGENT_SHADOW_OBJECT))
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    common::register(&http, &admin_url, &callback_url)
        .await
        .expect("register");

    let base = common::unique_base();
    let key = format!("test-joint-{base}");
    let rollout = RolloutClient::new(http.clone(), ingress.clone()).under(key.clone());
    let shadow = ShadowClient::new(http.clone(), ingress.clone());
    // What the scripted agent's runs report, and so what the sign-off is for.
    let signed = Version {
        engine: env!("CARGO_PKG_VERSION").to_owned(),
        model: "scripted/1".to_owned(),
        skills: skills("v1"),
    };

    // --- criteria with joint bounds, signed off by another operator ---
    rollout
        .register_criteria(&RegisterRequest {
            document: JOINT_DOCUMENT.to_owned(),
            by: REGISTRANT.to_owned(),
            reason: "joint bounds".to_owned(),
        })
        .await
        .expect("registered");
    let digest = Criteria::digest_of(JOINT_DOCUMENT);
    let unsigned = rollout.set_mode(&agent_mode()).await;
    assert!(refused(&unsigned, &["sign-off", &digest]), "{unsigned:?}");
    let verdict = Verdict {
        criteria: Some(digest.clone()),
        met: true,
        checks: Vec::new(),
        eval_missing: None,
        shadow: promotion::ShadowEvidence::default(),
        eval_version: Some(signed),
        shadow_versions: Vec::new(),
    };
    rollout
        .sign_off(&signoff(verdict, SIGNER))
        .await
        .expect("signed off");
    let state = rollout.set_mode(&agent_mode()).await.expect("agent");
    assert!(state.covers_joint(), "the criteria state joint bounds");
    let epoch = state.epoch();

    // --- a portfolio that forms carries the agent's joint slate ---
    let member = |model: &str| EntityUri(format!("https://ekg.example/id/{model}/{base}"));
    let formed = common::form_portfolio(
        &http,
        &ingress,
        &[member("sale.order"), member("mrp.production")],
    )
    .await;
    let step = ShadowStep {
        rollout: Arc::new(rollout.clone()),
        store: Arc::new(shadow.clone()),
        slates: Arc::new(|_run| panic!("no point is planned for here")),
        joint: common::scripted_joint_slates("scripted/1", "v1"),
        permits: Arc::new(tokio::sync::Semaphore::new(1)),
        guard: Duration::from_secs(20),
        joint_guard: Duration::from_secs(20),
    };
    let sink = IngressSink::new(http.clone(), ingress.clone());
    let (outcome, handle) = portfolio_slates(
        &sink,
        &CannedGroupSlateProposer,
        &PolicyEngine::new("p/v1", std::collections::BTreeSet::new()),
        &InMemoryWorldModel::default(),
        &formed,
        Some(&step),
    )
    .await;
    assert_eq!(outcome, PortfolioSlateOutcome::Attached);
    let record = handle
        .expect("an agent task")
        .await
        .expect("the agent's task")
        .expect("a joint record");
    assert_eq!(record.shown, Some(Shown::Agent));
    assert_eq!(record.epoch, Some(epoch));
    assert_eq!(record.rollout.as_deref(), Some(key.as_str()));

    let portfolio = common::portfolio_record(&http, &ingress, &formed.id.0).await;
    let on_portfolio = portfolio.slate.clone().expect("a joint slate");
    assert!(
        common::is_scripted_agents(&on_portfolio),
        "the portfolio carries the agent's joint slate"
    );
    let attached = portfolio
        .audit
        .iter()
        .filter(|entry| entry.event == PortfolioEvent::SlateAttached)
        .count();
    assert_eq!(attached, 1, "one joint slate reached the portfolio");
    let steps = &on_portfolio.trace.steps;
    assert!(steps[0].text.starts_with("Linked: "), "{}", steps[0].text);
    assert!(
        steps[1].text.contains("produced by the tool-using agent")
            && steps[1].text.contains("scripted/1")
            && steps[1].text.contains("propose-plan@v1"),
        "{}",
        steps[1].text
    );
    // The existing path's joint slate is recorded with the run.
    let paired = record.paired.as_ref().expect("the existing joint slate");
    assert!(!common::is_scripted_agents(paired.slate()));
    assert!(matches!(paired, Paired::Joint(_)));

    // --- promoting it records the decision on the joint record ---
    let option = &on_portfolio.options[0];
    let receipts: Vec<serde_json::Value> = option
        .plan
        .actions
        .iter()
        .map(|action| {
            serde_json::json!({
                "action": action,
                "external_ref": "test://joint",
                "source_revision": 1,
                "reversible": true,
            })
        })
        .collect();
    common::post_object(
        &http,
        &ingress,
        ["PortfolioService", &formed.id.0, "promote"],
        Some(&serde_json::json!({
            "generation": formed.generation,
            "rank_key": option.score.rank_key,
            "reviewer": "planner-3",
            "rationale": "the joint plan clears both",
            "execution_plan": { "actions": option.plan.actions, "receipts": receipts },
        })),
    )
    .await
    .expect("promoted");
    let records = || async {
        shadow
            .records_under(&formed.id.0)
            .await
            .expect("the joint records")
    };
    let decided = common::wait_until(Duration::from_secs(20), async || {
        records().await[0].decision.is_some()
    })
    .await;
    assert!(decided, "the promotion reached the shadow store");
    let decision = records().await[0].decision.clone().expect("a decision");
    assert_eq!(decision.state, DecisionState::Promoted);
    assert_eq!(decision.reviewer.as_deref(), Some("planner-3"));
    assert_eq!(decision.actions, option.plan.actions);
    assert_eq!(decision.reversed_at, None);
    let state = rollout.get().await.expect("state");
    assert!(
        matches!(state.mode, Mode::Agent { .. }),
        "a promotion is no reversal"
    );

    // --- undoing it rolls the mode back ---
    common::post_object(
        &http,
        &ingress,
        ["PortfolioService", &formed.id.0, "reverse"],
        Some(
            &serde_json::json!({ "reviewer": "planner-3", "reason": "the supplier fell through" }),
        ),
    )
    .await
    .expect("reversed");
    let portfolio = common::portfolio_record(&http, &ingress, &formed.id.0).await;
    assert_eq!(portfolio.state, PortfolioState::Reversed);
    // The portfolio reports one-way to the store, the store one-way to the rollout.
    let switched = common::wait_until(Duration::from_secs(30), async || {
        rollout
            .get()
            .await
            .is_ok_and(|state| rolled_back_by(&state) == Some("promotion_reversed"))
    })
    .await;
    let state = rollout.get().await.expect("state");
    assert!(switched, "{:?}", state.history.last());
    assert!(
        records().await[0]
            .decision
            .as_ref()
            .is_some_and(|decision| decision.state == DecisionState::Promoted
                && decision.reversed_at.is_some()),
        "the decision keeps the promotion and carries the reversal"
    );

    common::deregister(&http, &admin_url, &callback_url).await;
}
