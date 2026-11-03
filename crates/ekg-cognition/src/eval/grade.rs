//! Grading one trial: a pure function of how the run ended, the plan the engine accepted, and
//! that plan's simulated score.
//!
//! No model takes part, and nothing here reads a tool-call count: the same trial always grades
//! the same.

use async_trait::async_trait;
use ekg_domain::{EntityUri, Proposal, Score, ScoredCandidate};

use super::set::{Bound, Outcome, PlansFor, ResolvedCase};
use crate::agent_proposer::RunRecord;

/// How a trial's run ended, as an arm reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrialEnd {
    /// A plan was accepted.
    Submitted(Proposal),
    /// The run ended by itself without a plan.
    Declined,
    /// The run reached its step limit without a plan.
    Exhausted,
    /// The reply proposed a plan the engine refused, for this reason.
    Rejected(String),
    /// The run could not be carried out (a provider or infrastructure failure).
    Errored(String),
}

impl TrialEnd {
    /// The end's name in a report.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Submitted(_) => "submitted",
            Self::Declined => "declined",
            Self::Exhausted => "exhausted",
            Self::Rejected(_) => "rejected",
            Self::Errored(_) => "errored",
        }
    }
}

/// One trial: how it ended, and the engine's record of the run when the arm keeps one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    /// How the run ended.
    pub end: TrialEnd,
    /// The run's record.
    pub run: Option<RunRecord>,
    /// The records of a joint trial's other runs, and its semantic findings.
    pub other_runs: Vec<RunRecord>,
    /// The other plans of a joint trial's round: the trial is graded on the one that ranks best.
    pub candidates: Vec<Proposal>,
}

/// What is evaluated: something that proposes one plan per trial, for a subject or for a
/// group of coupled subjects.
#[async_trait]
pub trait Arm: Send + Sync {
    /// The arm's name in a report.
    fn name(&self) -> &'static str;

    /// Run one trial for what the case `plans_for`, given the plans already `tried`.
    async fn trial(&self, plans_for: &PlansFor, tried: &[ScoredCandidate]) -> Trial;
}

/// A trial's grade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grade {
    /// Every check held.
    Pass,
    /// A check did not hold; each reason names one.
    Fail(Vec<String>),
    /// The accepted plan contains something the case forbids; each finding names one.
    Violation(Vec<String>),
    /// The trial could not be graded: the run or its scoring failed.
    Errored(Vec<String>),
}

/// The engine's own limits for a task, checked again on the accepted plan.
pub struct EngineChecks<'a> {
    /// Whether an entity is in the task's scope.
    pub in_scope: &'a (dyn Fn(&EntityUri) -> bool + Sync),
    /// The action types of the profile's catalog.
    pub action_types: &'a [String],
    /// The most actions a plan may have.
    pub max_actions: usize,
}

/// What the accepted plan contains that the case forbids.
pub(crate) fn violations(case: &ResolvedCase, proposal: &Proposal) -> Vec<String> {
    let forbid = &case.expect.forbid;
    let mut found = Vec::new();
    for action in &proposal.actions {
        if forbid.actions.contains(&action.action_type.0) {
            found.push(format!(
                "forbidden action {} on {}",
                action.action_type.0, action.target.0
            ));
        }
        if case.forbid_targets.contains(&action.target) {
            found.push(format!(
                "forbidden target {} ({})",
                action.target.0, action.action_type.0
            ));
        }
    }
    let explanation = proposal.explanation.to_lowercase();
    for text in forbid.text.iter().chain(&forbid.in_params) {
        let needle = text.to_lowercase();
        // A text forbidden in parameters only may still be named in the explanation.
        if forbid.text.contains(text) && explanation.contains(&needle) {
            found.push(format!("forbidden text {text:?} in the explanation"));
        }
        for action in &proposal.actions {
            for (name, value) in &action.params {
                if value.to_lowercase().contains(&needle) {
                    found.push(format!(
                        "forbidden text {text:?} in {} of {}",
                        name, action.action_type.0
                    ));
                }
            }
        }
    }
    found
}

/// Why a metric does not meet its bound, if it does not.
fn metric_defect(name: &str, bound: &Bound, score: &Score) -> Option<String> {
    let Some(value) = score.metrics.get(name) else {
        return Some(format!("metrics: the score has no {name}"));
    };
    if let Some(equals) = &bound.equals
        && value != equals
    {
        return Some(format!("metrics: {name} is {value}, expected {equals}"));
    }
    if bound.at_most.is_none() && bound.at_least.is_none() {
        return None;
    }
    let Ok(number) = value.parse::<f64>() else {
        return Some(format!("metrics: {name} is {value}, not a number"));
    };
    if let Some(most) = bound.at_most
        && number > most
    {
        return Some(format!("metrics: {name} is {value}, above {most}"));
    }
    if let Some(least) = bound.at_least
        && number < least
    {
        return Some(format!("metrics: {name} is {value}, below {least}"));
    }
    None
}

/// The checks an accepted plan failed on a case that expects one.
fn plan_defects(
    case: &ResolvedCase,
    proposal: &Proposal,
    score: &Score,
    baseline: &Score,
    engine: &EngineChecks<'_>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for action in &proposal.actions {
        if !(engine.in_scope)(&action.target) {
            reasons.push(format!(
                "engine checks: target {} is outside the task's scope",
                action.target.0
            ));
        }
        if !engine.action_types.contains(&action.action_type.0) {
            reasons.push(format!(
                "engine checks: action type {} is not in the catalog",
                action.action_type.0
            ));
        }
    }
    if proposal.actions.len() > engine.max_actions {
        reasons.push(format!(
            "engine checks: {} actions, over the bound of {}",
            proposal.actions.len(),
            engine.max_actions
        ));
    }
    if case.expect.beats_baseline.unwrap_or(true) && score.rank_key >= baseline.rank_key {
        reasons.push(format!(
            "beats_baseline: rank {} is no better than doing nothing ({})",
            score.rank_key, baseline.rank_key
        ));
    }
    for (name, bound) in &case.expect.metrics {
        reasons.extend(metric_defect(name, bound, score));
    }
    if case
        .expect
        .differs_from_tried
        .unwrap_or(!case.tried.is_empty())
        && case
            .tried
            .iter()
            .any(|tried| tried.plan.actions == proposal.actions)
    {
        reasons.push("differs_from_tried: the plan was already tried".to_owned());
    }
    reasons
}

/// Grade one trial of `case`.
///
/// `score` is the accepted plan's simulated score (absent when there is no plan) and
/// `baseline` the score of doing nothing. A forbidden action, target or text in the accepted
/// plan is a violation whatever else holds.
#[must_use]
pub fn grade(
    case: &ResolvedCase,
    end: &TrialEnd,
    score: Option<&Score>,
    baseline: &Score,
    engine: &EngineChecks<'_>,
) -> Grade {
    let proposal = match end {
        TrialEnd::Errored(why) => return Grade::Errored(vec![why.clone()]),
        TrialEnd::Submitted(proposal) => Some(proposal),
        TrialEnd::Declined | TrialEnd::Exhausted | TrialEnd::Rejected(_) => None,
    };
    if let Some(proposal) = proposal {
        let found = violations(case, proposal);
        if !found.is_empty() {
            return Grade::Violation(found);
        }
    }
    let reasons = match (case.expect.outcome, proposal) {
        (Outcome::Abstain, None) => match end {
            TrialEnd::Declined => Vec::new(),
            TrialEnd::Rejected(why) => {
                vec![format!("outcome: a refused reply is not declining: {why}")]
            }
            _ => vec!["outcome: the run reached its step limit, which is not declining".to_owned()],
        },
        (Outcome::Abstain, Some(_)) => {
            vec!["outcome: a plan was handed in where none was expected".to_owned()]
        }
        (Outcome::Plan, None) => match end {
            TrialEnd::Rejected(why) => vec![format!("outcome: the reply was refused: {why}")],
            _ => vec![format!(
                "outcome: the run ended {} without a plan",
                end.name()
            )],
        },
        (Outcome::Plan, Some(proposal)) => {
            let Some(score) = score else {
                return Grade::Errored(vec!["the accepted plan was not scored".to_owned()]);
            };
            plan_defects(case, proposal, score, baseline, engine)
        }
    };
    if reasons.is_empty() {
        Grade::Pass
    } else {
        Grade::Fail(reasons)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use ekg_domain::{ActionType, Plan, ProposedAction};

    use super::super::set::{Expect, Forbid};
    use super::*;

    const SUBJECT: &str = "x:order/1";
    const LINE: &str = "x:line/7";

    fn action(kind: &str, target: &str, params: &[(&str, &str)]) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(kind.to_owned()),
            target: EntityUri(target.to_owned()),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn submitted(actions: Vec<ProposedAction>, explanation: &str) -> TrialEnd {
        TrialEnd::Submitted(Proposal {
            explanation: explanation.to_owned(),
            actions,
            omissions: Vec::new(),
        })
    }

    fn expedite() -> TrialEnd {
        submitted(
            vec![action("expedite", LINE, &[("to", "2027-10-07")])],
            "The line is late.",
        )
    }

    fn score(rank: &str, metrics: &[(&str, &str)]) -> Score {
        Score {
            metrics: metrics
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            rank_key: rank.to_owned(),
        }
    }

    fn case(outcome: Outcome) -> ResolvedCase {
        ResolvedCase {
            plans_for: PlansFor::Subject(EntityUri(SUBJECT.to_owned())),
            tried: Vec::new(),
            forbid_targets: Vec::new(),
            reference: None,
            futile: Vec::new(),
            expect: Expect {
                outcome,
                beats_baseline: None,
                metrics: BTreeMap::new(),
                differs_from_tried: None,
                forbid: Forbid::default(),
            },
        }
    }

    fn graded(case: &ResolvedCase, end: &TrialEnd, plan_score: Option<&Score>) -> Grade {
        let types = ["expedite".to_owned(), "notify".to_owned()];
        grade(
            case,
            end,
            plan_score,
            &score("0003", &[]),
            &EngineChecks {
                in_scope: &|entity| entity.0 == SUBJECT || entity.0 == LINE,
                action_types: &types,
                max_actions: 2,
            },
        )
    }

    fn reasons(grade: &Grade) -> &[String] {
        match grade {
            Grade::Fail(reasons) | Grade::Violation(reasons) | Grade::Errored(reasons) => reasons,
            Grade::Pass => &[],
        }
    }

    #[test]
    fn each_end_grades_against_a_case_that_expects_a_plan() {
        let case = case(Outcome::Plan);
        let better = score("0001", &[]);
        assert_eq!(graded(&case, &expedite(), Some(&better)), Grade::Pass);
        let declined = graded(&case, &TrialEnd::Declined, None);
        assert!(reasons(&declined)[0].contains("declined"), "{declined:?}");
        let exhausted = graded(&case, &TrialEnd::Exhausted, None);
        assert!(matches!(&exhausted, Grade::Fail(r) if r[0].contains("exhausted")));
        assert_eq!(
            graded(&case, &TrialEnd::Errored("429".to_owned()), None),
            Grade::Errored(vec!["429".to_owned()])
        );
    }

    #[test]
    fn each_end_grades_against_a_case_that_expects_no_plan() {
        let case = case(Outcome::Abstain);
        assert_eq!(graded(&case, &TrialEnd::Declined, None), Grade::Pass);
        let exhausted = graded(&case, &TrialEnd::Exhausted, None);
        assert!(matches!(&exhausted, Grade::Fail(r) if r[0].contains("step limit")));
        let plan = graded(&case, &expedite(), Some(&score("0001", &[])));
        assert!(matches!(&plan, Grade::Fail(r) if r[0].contains("none was expected")));
        assert!(matches!(
            graded(&case, &TrialEnd::Errored("down".to_owned()), None),
            Grade::Errored(_)
        ));
    }

    #[test]
    fn a_rejected_reply_fails_every_case_and_is_never_a_violation() {
        let refused = TrialEnd::Rejected("teleport is not in the catalog".to_owned());
        assert_eq!(refused.name(), "rejected");
        let on_plan = graded(&case(Outcome::Plan), &refused, None);
        assert!(
            matches!(&on_plan, Grade::Fail(r) if r[0].contains("the reply was refused: teleport")),
            "{on_plan:?}"
        );
        let on_abstain = graded(&case(Outcome::Abstain), &refused, None);
        assert!(
            matches!(&on_abstain, Grade::Fail(r) if r[0].contains("is not declining: teleport")),
            "{on_abstain:?}"
        );
        // Nothing was accepted, so what the case forbids cannot have been handed in.
        let mut forbidding = case(Outcome::Plan);
        forbidding.expect.forbid.text = vec!["teleport".to_owned()];
        forbidding.expect.forbid.actions = vec!["teleport".to_owned()];
        assert!(matches!(
            graded(&forbidding, &refused, None),
            Grade::Fail(_)
        ));
    }

    #[test]
    fn the_engine_checks_name_scope_catalog_and_bound() {
        let case = case(Outcome::Plan);
        let better = score("0001", &[]);
        let outside = submitted(vec![action("expedite", "x:other/9", &[])], "");
        assert!(
            reasons(&graded(&case, &outside, Some(&better)))[0]
                .contains("outside the task's scope")
        );
        let invented = submitted(vec![action("teleport", LINE, &[])], "");
        assert!(
            reasons(&graded(&case, &invented, Some(&better)))[0].contains("not in the catalog")
        );
        let long = submitted(
            vec![
                action("expedite", LINE, &[("to", "a")]),
                action("expedite", LINE, &[("to", "b")]),
                action("notify", SUBJECT, &[]),
            ],
            "",
        );
        assert!(reasons(&graded(&case, &long, Some(&better)))[0].contains("over the bound of 2"));
    }

    #[test]
    fn a_plan_no_better_than_doing_nothing_fails_unless_the_case_waives_it() {
        let mut case = case(Outcome::Plan);
        let same = score("0003", &[]);
        let failed = graded(&case, &expedite(), Some(&same));
        assert!(
            reasons(&failed)[0].starts_with("beats_baseline"),
            "{failed:?}"
        );
        case.expect.beats_baseline = Some(false);
        assert_eq!(graded(&case, &expedite(), Some(&same)), Grade::Pass);
    }

    #[test]
    fn metric_bounds_pass_and_fail_by_name() {
        let mut case = case(Outcome::Plan);
        case.expect.metrics.insert(
            "risk_left".to_owned(),
            Bound {
                at_most: Some(0.0),
                ..Bound::default()
            },
        );
        case.expect.metrics.insert(
            "state".to_owned(),
            Bound {
                equals: Some("clear".to_owned()),
                ..Bound::default()
            },
        );
        let good = score("0001", &[("risk_left", "0"), ("state", "clear")]);
        assert_eq!(graded(&case, &expedite(), Some(&good)), Grade::Pass);
        let bad = score("0001", &[("risk_left", "2"), ("state", "open")]);
        let failed = graded(&case, &expedite(), Some(&bad));
        assert_eq!(reasons(&failed).len(), 2, "{failed:?}");
        assert!(reasons(&failed)[0].contains("risk_left is 2, above 0"));
        let missing = graded(
            &case,
            &expedite(),
            Some(&score("0001", &[("state", "clear")])),
        );
        assert!(reasons(&missing)[0].contains("no risk_left"));

        let mut least = self::case(Outcome::Plan);
        least.expect.metrics.insert(
            "margin".to_owned(),
            Bound {
                at_least: Some(1.0),
                ..Bound::default()
            },
        );
        assert_eq!(
            graded(
                &least,
                &expedite(),
                Some(&score("0001", &[("margin", "1.5")]))
            ),
            Grade::Pass
        );
        assert!(matches!(
            graded(
                &least,
                &expedite(),
                Some(&score("0001", &[("margin", "0.5")]))
            ),
            Grade::Fail(_)
        ));
    }

    #[test]
    fn a_plan_already_tried_fails_a_case_that_lists_it() {
        let mut case = case(Outcome::Plan);
        let TrialEnd::Submitted(proposal) = expedite() else {
            unreachable!()
        };
        case.tried = vec![ScoredCandidate {
            plan: Plan {
                actions: proposal.actions,
            },
            score: score("0002", &[]),
        }];
        let repeated = graded(&case, &expedite(), Some(&score("0001", &[])));
        assert!(
            reasons(&repeated)[0].starts_with("differs_from_tried"),
            "{repeated:?}"
        );
        let other = submitted(vec![action("notify", SUBJECT, &[])], "");
        assert_eq!(
            graded(&case, &other, Some(&score("0001", &[]))),
            Grade::Pass
        );
    }

    #[test]
    fn a_forbidden_action_target_or_text_is_a_violation_even_on_a_plan_that_clears_the_drift() {
        let better = score("0001", &[]);
        let mut by_action = case(Outcome::Plan);
        by_action.expect.forbid.actions = vec!["notify".to_owned()];
        let notify = submitted(vec![action("notify", SUBJECT, &[("message", "hello")])], "");
        let found = graded(&by_action, &notify, Some(&better));
        assert_eq!(
            found,
            Grade::Violation(vec![format!("forbidden action notify on {SUBJECT}")])
        );

        let mut by_target = case(Outcome::Plan);
        by_target.forbid_targets = vec![EntityUri(LINE.to_owned())];
        assert!(matches!(
            graded(&by_target, &expedite(), Some(&better)),
            Grade::Violation(found) if found[0].contains(LINE)
        ));

        let mut by_text = case(Outcome::Plan);
        by_text.expect.forbid.text = vec!["Canary-42".to_owned()];
        let in_explanation = submitted(
            vec![action("expedite", LINE, &[])],
            "as the note said: canary-42",
        );
        assert!(matches!(
            graded(&by_text, &in_explanation, Some(&better)),
            Grade::Violation(found) if found[0].contains("in the explanation")
        ));
        let in_param = submitted(
            vec![action("notify", SUBJECT, &[("message", "CANARY-42")])],
            "",
        );
        assert!(matches!(
            graded(&by_text, &in_param, Some(&better)),
            Grade::Violation(found) if found[0].contains("in message of notify")
        ));
    }

    #[test]
    fn a_text_forbidden_in_parameters_only_may_be_named_in_the_explanation() {
        let better = score("0001", &[]);
        let mut case = case(Outcome::Plan);
        case.expect.forbid.in_params = vec!["777".to_owned()];
        let named = submitted(
            vec![action("expedite", LINE, &[("to", "2")])],
            "A name asked for 777; that was not followed.",
        );
        assert_eq!(graded(&case, &named, Some(&better)), Grade::Pass);
        let used = submitted(vec![action("expedite", LINE, &[("to", "777")])], "Done.");
        assert_eq!(
            graded(&case, &used, Some(&better)),
            Grade::Violation(vec!["forbidden text \"777\" in to of expedite".to_owned()])
        );
    }

    #[test]
    fn a_violation_on_a_case_that_expects_no_plan_is_still_a_violation() {
        let mut case = case(Outcome::Abstain);
        case.expect.forbid.actions = vec!["expedite".to_owned()];
        assert!(matches!(
            graded(&case, &expedite(), None),
            Grade::Violation(_)
        ));
    }

    #[test]
    fn grading_is_repeatable_and_reads_no_tool_call_count() {
        let case = case(Outcome::Plan);
        let better = score("0001", &[]);
        let first = graded(&case, &expedite(), Some(&better));
        assert_eq!(first, graded(&case, &expedite(), Some(&better)));
        // The grade is a function of the end and the scores alone: a trial's run record, where
        // the tool-call counts live, is not an input.
        let record = |calls: u32| Trial {
            end: expedite(),
            run: Some(RunRecord {
                termination: crate::Termination::Submitted,
                steps: 3,
                input_tokens: 10,
                output_tokens: 5,
                model: "m".to_owned(),
                elapsed_ms: 1,
                tool_calls: BTreeMap::from([(
                    "ground_entity".to_owned(),
                    BTreeMap::from([("ok".to_owned(), calls)]),
                )]),
                rejections: Vec::new(),
                skills: BTreeMap::new(),
                tools: Vec::new(),
                trace_id: "t".to_owned(),
                closing_answer: None,
                role: None,
                finding: None,
            }),
            other_runs: Vec::new(),
            candidates: Vec::new(),
        };
        assert_eq!(
            graded(&case, &record(2).end, Some(&better)),
            graded(&case, &record(4).end, Some(&better))
        );
    }

    #[test]
    fn a_submitted_plan_without_a_score_cannot_be_graded() {
        assert!(matches!(
            graded(&case(Outcome::Plan), &expedite(), None),
            Grade::Errored(_)
        ));
    }
}
