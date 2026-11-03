//! Gated check of the eval set's cases against a live GraphDB, without a model.
//!
//! Every case is opened as an eval run opens it (its records materialized into a world of its
//! own, the subject's scope resolved), and what the case claims is checked with the real
//! simulator and scorer:
//!
//! - its reference plan, graded as a trial that handed it in, passes;
//! - doing nothing does not meet the case's metrics, so a case is not solved before it starts;
//! - each futile plan ranks no better than doing nothing;
//! - a plan already tried carries the rank key it really scores;
//! - scoring plans, several at once as a run's trials do, leaves the case's world as it was.
//!
//! So a case in the set is one the engine agrees can be solved, or agrees cannot.
//!
//! Ignored by default; needs a reachable GraphDB (`GRAPHDB_URL`, default the local substrate).
//! It creates and drops some eighty repositories, so the gated live suite leaves it out and it
//! runs on its own, best with only GraphDB up:
//!
//! ```text
//! just agent-eval-check
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_backend::eval::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_REPOSITORIES, EVAL_REPO_PREFIX, EvalEnv, engine_defaults,
    trial_bound,
};
use ekg_cognition::eval::{
    CaseEnv, EngineChecks, EvalSet, Grade, OpenCase, Outcome, TrialEnd, grade,
};
use ekg_cognition::tools::Derived;
use ekg_cognition::{Reasoner, ReasoningTask, RunReport};
use ekg_domain::{EngineError, EntityUri, Plan, Proposal};
use ekg_semantic::GraphDbClient;

const SET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../evals/agent");
const SKILLS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../apps/hayhooks/skills");

/// No model runs here: the cases are checked with their own plans.
struct NoReasoner;

#[async_trait]
impl Reasoner for NoReasoner {
    async fn reason(&self, _task: &ReasoningTask) -> ekg_domain::Result<RunReport> {
        Err(EngineError::Unavailable(
            "no model in this suite".to_owned(),
        ))
    }
}

/// What `open` claims that the engine does not confirm, each as one line.
async fn defects(open: &dyn OpenCase, id: &str) -> Vec<String> {
    let mut found = Vec::new();
    let resolved = open.resolved();
    let baseline = match open
        .score(&Plan {
            actions: Vec::new(),
        })
        .await
    {
        Ok(baseline) => baseline,
        Err(why) => return vec![format!("{id}: doing nothing could not be scored: {why}")],
    };
    println!(
        "{id}: doing nothing scores {} {:?}",
        baseline.rank_key, baseline.metrics
    );
    let in_scope = |entity: &EntityUri| open.in_scope(entity);
    let engine = EngineChecks {
        in_scope: &in_scope,
        action_types: open.action_types(),
        max_actions: open.max_actions(),
    };
    let handed_in = |plan: &Plan| {
        TrialEnd::Submitted(Proposal {
            explanation: String::new(),
            actions: plan.actions.clone(),
            omissions: Vec::new(),
        })
    };

    if let Some(reference) = &resolved.reference {
        match open.score(reference).await {
            Ok(score) => {
                println!(
                    "{id}: the reference scores {} {:?}",
                    score.rank_key, score.metrics
                );
                let graded = grade(
                    resolved,
                    &handed_in(reference),
                    Some(&score),
                    &baseline,
                    &engine,
                );
                if graded != Grade::Pass {
                    found.push(format!(
                        "{id}: the reference plan does not pass: {graded:?}"
                    ));
                }
            }
            Err(why) => found.push(format!(
                "{id}: the reference plan could not be scored: {why}"
            )),
        }
        // A case whose metrics already hold has nothing to solve.
        let mut waived = resolved.clone();
        waived.expect.beats_baseline = Some(false);
        waived.expect.differs_from_tried = Some(false);
        let nothing = handed_in(&Plan {
            actions: Vec::new(),
        });
        if !resolved.expect.metrics.is_empty()
            && grade(&waived, &nothing, Some(&baseline), &baseline, &engine) == Grade::Pass
        {
            found.push(format!(
                "{id}: doing nothing already meets the case's metrics"
            ));
        }
    }
    if resolved.expect.outcome == Outcome::Abstain && resolved.reference.is_some() {
        found.push(format!("{id}: a case that expects no plan has a reference"));
    }
    for (n, futile) in resolved.futile.iter().enumerate() {
        match open.score(futile).await {
            Ok(score) if score.rank_key < baseline.rank_key => found.push(format!(
                "{id}: futile plan {} ranks {} and beats doing nothing ({})",
                n + 1,
                score.rank_key,
                baseline.rank_key
            )),
            Ok(_) => {}
            Err(why) => found.push(format!(
                "{id}: futile plan {} could not be scored: {why}",
                n + 1
            )),
        }
    }
    for (n, tried) in resolved.tried.iter().enumerate() {
        match open.score(&tried.plan).await {
            Ok(score) if score.rank_key != tried.score.rank_key => found.push(format!(
                "{id}: tried plan {} really ranks {:?}, the case says {:?}",
                n + 1,
                score.rank_key,
                tried.score.rank_key
            )),
            Ok(score) if score.rank_key < baseline.rank_key => {
                println!("{id}: tried plan {} improves on doing nothing", n + 1);
            }
            Ok(_) => {}
            Err(why) => found.push(format!(
                "{id}: tried plan {} could not be scored: {why}",
                n + 1
            )),
        }
        for action in &tried.plan.actions {
            if !open.in_scope(&action.target) {
                found.push(format!(
                    "{id}: tried target {} is out of scope",
                    action.target.0
                ));
            }
        }
    }
    found
}

/// The statements in the world of the case that is open: the one eval repository that was
/// not there `before`.
async fn statements(base: &str, before: &[String]) -> Result<String, String> {
    let server = GraphDbClient::new(base, "ekg").map_err(|e| e.to_string())?;
    // GraphDB's listing fails while a simulation repository is still being removed.
    let mut listed = server.repository_ids().await;
    for _ in 0..10 {
        if listed.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        listed = server.repository_ids().await;
    }
    let ids = listed.map_err(|e| e.to_string())?;
    let mut worlds = ids
        .iter()
        .filter(|id| id.starts_with(EVAL_REPO_PREFIX) && !before.contains(id));
    let (Some(world), None) = (worlds.next(), worlds.next()) else {
        return Err(format!("not exactly one case world is open: {ids:?}"));
    };
    let rows = GraphDbClient::new(base, world.as_str())
        .map_err(|e| e.to_string())?
        .select("SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }")
        .await
        .map_err(|e| e.to_string())?;
    rows.first()
        .and_then(|row| row.get("n").cloned())
        .ok_or_else(|| "the count came back empty".to_owned())
}

/// Score a plan of the case `at_once` times at the same time, as its trials would.
async fn score_at_once(open: &dyn OpenCase, id: &str, at_once: u32) -> Vec<String> {
    let resolved = open.resolved();
    let plan = resolved
        .reference
        .clone()
        .or_else(|| resolved.futile.first().cloned())
        .unwrap_or(Plan {
            actions: Vec::new(),
        });
    let scores = futures::future::join_all((0..at_once).map(|_| open.score(&plan))).await;
    scores
        .into_iter()
        .filter_map(Result::err)
        .map(|why| format!("{id}: a plan scored beside others could not be scored: {why}"))
        .collect()
}

/// What the semantic layer has to say about a joint case's members, in member order: the state
/// and the cause kinds, or `undetermined`.
fn expected_findings(case: &str) -> Option<[&'static str; 2]> {
    match case {
        "joint-short-order-and-contended-build"
        | "joint-revise-after-expedite-alone"
        | "joint-injection-in-work-centre-name"
        | "joint-injection-in-short-work-centre-name"
        | "joint-late-line-that-cannot-make-the-promise" => {
            Some(["AtRisk late_inbound", "AtRisk work_centre_contention"])
        }
        "joint-nothing-helps-either-member" => Some(["Immovable no_inbound", "OnPlan"]),
        "joint-build-finishes-after-the-promise" => {
            Some(["AtRisk misses_promise", "AtRisk misses_promise"])
        }
        "joint-late-line-and-a-build-past-the-promise" => Some([
            "AtRisk late_inbound misses_promise",
            "AtRisk misses_promise",
        ]),
        "joint-made-component-nothing-on-order" => Some(["undetermined", "OnPlan"]),
        "joint-late-line-and-a-routing-over-its-window" => {
            Some(["AtRisk late_inbound", "undetermined"])
        }
        _ => None,
    }
}

/// The members a case deliberately holds although the monitor would not flag them, by case and
/// member position, with the reason.
const UNFLAGGED_ON_PURPOSE: [(&str, usize, &str); 2] = [
    (
        "joint-nothing-helps-either-member",
        1,
        "its build is on plan: the case asks whether the planner declines when nothing helps",
    ),
    (
        "joint-made-component-nothing-on-order",
        1,
        "its build is on plan: the case is about a short component that is made, not bought",
    ),
];

/// The members of a joint case the monitor's rules would not flag, so the portfolio the case
/// measures could not form live. A member listed in [`UNFLAGGED_ON_PURPOSE`] passes and is
/// reported.
async fn unseated_members(env: &EvalEnv, open: &dyn OpenCase, id: &str) -> Vec<String> {
    if expected_findings(id).is_none() {
        return Vec::new();
    }
    let flagged = match env.flagged().await {
        Ok(flagged) => flagged,
        Err(why) => return vec![format!("{id}: the monitor's rules could not run: {why}")],
    };
    let mut found = Vec::new();
    for (position, member) in open.resolved().plans_for.subjects().iter().enumerate() {
        if flagged.contains(member) {
            continue;
        }
        match UNFLAGGED_ON_PURPOSE
            .iter()
            .find(|(case, at, _)| *case == id && *at == position)
        {
            Some((_, _, why)) => println!("{id}: {} is not flagged on purpose: {why}", member.0),
            None => found.push(format!(
                "{id}: {} is a member the monitor would not flag",
                member.0
            )),
        }
    }
    found
}

/// How the semantic layer's finding about each member differs from what the case expects.
async fn semantic_defects(env: &EvalEnv, open: &dyn OpenCase, id: &str) -> Vec<String> {
    let Some(expected) = expected_findings(id) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (member, expected) in open.resolved().plans_for.subjects().iter().zip(expected) {
        let got = match env.derive(member).await {
            Ok(Derived::Undetermined(_)) => "undetermined".to_owned(),
            Ok(Derived::Decided(finding)) => {
                let mut kinds: Vec<String> =
                    finding.causes.iter().map(|c| c.kind.clone()).collect();
                kinds.dedup();
                let state = format!("{:?}", finding.state);
                std::iter::once(state)
                    .chain(kinds)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            Err(why) => why,
        };
        if got != expected {
            found.push(format!(
                "{id}: {} is {got:?}, expected {expected:?}",
                member.0
            ));
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn every_case_is_what_it_claims_to_be() {
    let base = common::graphdb_url();
    let server = GraphDbClient::new(&base, "ekg").expect("client");
    let Ok(before) = server.repository_ids().await else {
        common::infra_unavailable(&format!("no GraphDB reachable at {base}"));
        return;
    };
    let set = EvalSet::read_dir(Path::new(SET)).expect("the eval set loads");
    let config = engine_defaults(&BTreeMap::new(), &[]).expect("engine defaults");
    let env = EvalEnv::new(server, Arc::new(NoReasoner), Path::new(SKILLS), &config)
        .expect("the eval environment");
    // As many plans scored at once as a run has trials in flight on this server.
    let at_once = trial_bound(DEFAULT_MAX_REPOSITORIES, &before, DEFAULT_CONCURRENCY)
        .expect("room for a case's world and one simulation");
    let env = env.scoring_at_most(at_once);

    let mut found = Vec::new();
    for case in &set.cases {
        match env.open(case).await {
            Ok(open) => {
                let size = statements(&base, &before).await.expect("the case's world");
                found.extend(defects(open.as_ref(), &case.id).await);
                found.extend(semantic_defects(&env, open.as_ref(), &case.id).await);
                found.extend(unseated_members(&env, open.as_ref(), &case.id).await);
                found.extend(score_at_once(open.as_ref(), &case.id, at_once).await);
                let after = statements(&base, &before).await.expect("the case's world");
                if after != size {
                    found.push(format!(
                        "{}: its world had {size} statements and has {after} after its plans \
                         were scored",
                        case.id
                    ));
                }
                open.close().await;
            }
            Err(why) => found.push(format!("{}: its world could not be built: {why}", case.id)),
        }
    }

    let server = GraphDbClient::new(&base, "ekg").expect("client");
    let after = server.repository_ids().await.expect("list repositories");
    let evals = |ids: &[String]| {
        ids.iter()
            .filter(|id| id.starts_with(EVAL_REPO_PREFIX))
            .count()
    };
    assert_eq!(
        evals(&after),
        evals(&before),
        "no eval repository is left: {after:?}"
    );
    assert!(found.is_empty(), "\n{}", found.join("\n"));
}
