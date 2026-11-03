//! An eval run's report: every trial's result, pass^k per case and slice, and what the run was
//! measured with.
//!
//! A case passes at `k` when all `k` of its trials pass. A slice's pass^k is the share of its
//! cases that do; the slices are each category, each profile, and the whole set.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use ekg_domain::{ProposedAction, Score};
use serde::{Deserialize, Serialize};

use super::grade::Grade;
use crate::agent_proposer::RunRecord;

/// What a run was measured with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionTuple {
    /// The engine version.
    pub engine: String,
    /// The model the runtime reported (empty when no run reported one).
    pub model: String,
    /// The skill versions, by name.
    pub skills: BTreeMap<String, String>,
    /// The tools each profile offered, by profile.
    pub tools: BTreeMap<String, Vec<String>>,
    /// The eval set's version.
    pub set_version: u32,
    /// The digest of the eval set's cases.
    pub set_digest: String,
    /// The trials per case (`k`).
    pub trials: u32,
    /// The step limit of one run.
    pub max_steps: u32,
    /// The step limit of a joint case's run. Zero in a report written before joint runs had a
    /// limit of their own (they ran under `max_steps`), and for an arm without a step limit.
    #[serde(default)]
    pub joint_max_steps: u32,
    /// The step limit of a joint case's analysts. Zero in a report written before a joint trial
    /// had analysts (one run planned the whole group), and for an arm without a step limit.
    #[serde(default)]
    pub analyst_max_steps: u32,
    /// The planners of a joint round. Zero in a report written before rounds, and for an arm
    /// without any.
    #[serde(default)]
    pub planners: u32,
    /// Whether planners had read tools.
    #[serde(default)]
    pub planner_reads: bool,
}

/// What an arm's report measures. An arm that does not report something says so here, so
/// a reader does not take a zero for a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measures {
    /// Whether the runs report token usage.
    pub tokens: bool,
    /// Whether the arm keeps a run record per trial, and so has skills, tools and a step
    /// limit to name.
    pub run_records: bool,
}

impl Measures {
    /// An arm that is one model call per trial: no token usage, no run record.
    pub const CALLS_ONLY: Self = Self {
        tokens: false,
        run_records: false,
    };
}

impl Default for Measures {
    fn default() -> Self {
        Self {
            tokens: true,
            run_records: true,
        }
    }
}

/// How a run was run. Two runs of one version that differ only here are the same evidence, so
/// this is not part of the [`VersionTuple`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSettings {
    /// The most trials of one case that ran at the same time.
    pub concurrency: u32,
    /// The further attempts an errored trial was allowed.
    pub retries: u32,
}

impl Default for RunSettings {
    /// A report written before the settings existed: one trial at a time, no further attempt.
    fn default() -> Self {
        Self {
            concurrency: 1,
            retries: 0,
        }
    }
}

/// The experiment run a run's traces name, so a report leads to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRef {
    /// The experiment run's id.
    pub id: String,
    /// The experiment run's name.
    pub name: String,
}

/// What a run was narrowed to. A narrowed run is no evidence for a promotion verdict.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Narrowing {
    /// The case ids asked for.
    pub cases: Vec<String>,
    /// The categories asked for.
    pub categories: Vec<String>,
    /// The profiles asked for.
    pub profiles: Vec<String>,
}

impl Narrowing {
    /// Whether nothing was asked for, so the run is of the whole set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty() && self.categories.is_empty() && self.profiles.is_empty()
    }

    /// What the run was narrowed to and what that makes it, in one line.
    #[must_use]
    pub fn describe(&self) -> String {
        let by: Vec<String> = [
            ("case", &self.cases),
            ("category", &self.categories),
            ("profile", &self.profiles),
        ]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(kind, names)| format!("{kind} {}", names.join(", ")))
        .collect();
        format!(
            "narrowed to {}: no evidence for a promotion verdict",
            by.join("; ")
        )
    }
}

/// A trial's grade without its reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GradeKind {
    /// Every check held.
    Pass,
    /// A check did not hold.
    Fail,
    /// The accepted plan contains something the case forbids.
    Violation,
    /// The run or its scoring failed.
    Errored,
}

/// The plan a trial handed in, as the engine accepted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedPlan {
    /// The plan's actions.
    pub actions: Vec<ProposedAction>,
    /// The explanation for the operator.
    pub explanation: String,
}

/// Whether a count is zero, to leave it out of a report.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// One trial's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialResult {
    /// The trial's number within its case, from 1.
    pub trial: u32,
    /// How the run ended: `submitted`, `declined`, `exhausted`, `rejected` or `errored`.
    pub end: String,
    /// The grade.
    pub grade: GradeKind,
    /// Why: the failed checks, the findings, or the error.
    pub reasons: Vec<String>,
    /// The accepted plan, if any.
    pub plan: Option<ReportedPlan>,
    /// The plan's simulated score, if it was scored.
    pub score: Option<Score>,
    /// The engine's record of the run, when the arm keeps one.
    pub run: Option<RunRecord>,
    /// The engine's records of a joint trial's other runs and its findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_runs: Vec<RunRecord>,
    /// The round's plans the scorer did not choose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_plans: Vec<ReportedPlan>,
    /// The plans of a joint round set aside because none beat doing nothing.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub set_aside: usize,
    /// Why each earlier attempt of this trial errored. The fields above are the last
    /// attempt's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errored_attempts: Vec<String>,
}

impl TrialResult {
    /// The result of a trial that ended `end` and was graded `grade`.
    #[must_use]
    pub fn new(
        trial: u32,
        end: &str,
        grade: Grade,
        plan: Option<ReportedPlan>,
        score: Option<Score>,
        run: Option<RunRecord>,
    ) -> Self {
        let (grade, reasons) = match grade {
            Grade::Pass => (GradeKind::Pass, Vec::new()),
            Grade::Fail(reasons) => (GradeKind::Fail, reasons),
            Grade::Violation(found) => (GradeKind::Violation, found),
            Grade::Errored(why) => (GradeKind::Errored, why),
        };
        Self {
            trial,
            end: end.to_owned(),
            grade,
            reasons,
            plan,
            score,
            run,
            other_runs: Vec::new(),
            other_plans: Vec::new(),
            set_aside: 0,
            errored_attempts: Vec::new(),
        }
    }

    /// The result with the records of its trial's `other_runs`.
    #[must_use]
    pub fn with_other_runs(mut self, other_runs: Vec<RunRecord>) -> Self {
        self.other_runs = other_runs;
        self
    }

    /// The result with the number of plans `set_aside`.
    #[must_use]
    pub fn with_set_aside(mut self, set_aside: usize) -> Self {
        self.set_aside = set_aside;
        self
    }

    /// The result with the round's plans the scorer did not choose.
    #[must_use]
    pub fn with_other_plans(mut self, other_plans: Vec<ReportedPlan>) -> Self {
        self.other_plans = other_plans;
        self
    }
}

/// One case's results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseResult {
    /// The case's id.
    pub id: String,
    /// Its category.
    pub category: String,
    /// Its profile.
    pub profile: String,
    /// The score of doing nothing in the case's world, when it could be computed.
    pub baseline: Option<Score>,
    /// The trials that ran.
    pub trials: Vec<TrialResult>,
    /// The trials that did not start.
    pub not_run: u32,
}

impl CaseResult {
    fn count(&self, kind: GradeKind) -> u32 {
        let count = self.trials.iter().filter(|t| t.grade == kind).count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// The trials that passed.
    #[must_use]
    pub fn passed(&self) -> u32 {
        self.count(GradeKind::Pass)
    }

    /// The trials that failed a check.
    #[must_use]
    pub fn failed(&self) -> u32 {
        self.count(GradeKind::Fail)
    }

    /// The trials that violated what the case forbids.
    #[must_use]
    pub fn violating(&self) -> u32 {
        self.count(GradeKind::Violation)
    }

    /// The trials that errored.
    #[must_use]
    pub fn errored(&self) -> u32 {
        self.count(GradeKind::Errored)
    }

    /// Whether all `k` trials ran and passed.
    #[must_use]
    pub fn passed_at(&self, k: u32) -> bool {
        k > 0 && self.passed() == k
    }

    /// Each trial that was attempted again, as `trial N: reason; reason`.
    #[must_use]
    pub fn retried(&self) -> Vec<String> {
        self.trials
            .iter()
            .filter(|trial| !trial.errored_attempts.is_empty())
            .map(|trial| {
                format!(
                    "trial {}: {}",
                    trial.trial,
                    trial.errored_attempts.join("; ")
                )
            })
            .collect()
    }

    /// How many of the trials' candidates, the chosen plan and the others of its round, hold
    /// each action type. A candidate counts once per type, however many such actions it holds.
    #[must_use]
    pub fn candidates_by_action(&self) -> BTreeMap<String, u32> {
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        let candidates = self
            .trials
            .iter()
            .flat_map(|trial| trial.plan.iter().chain(&trial.other_plans));
        for candidate in candidates {
            let mut types: Vec<&str> = candidate
                .actions
                .iter()
                .map(|action| action.action_type.0.as_str())
                .collect();
            types.sort_unstable();
            types.dedup();
            for action_type in types {
                *counts.entry(action_type.to_owned()).or_default() += 1;
            }
        }
        counts
    }

    /// Each violation, as `trial N: finding`.
    #[must_use]
    pub fn violations(&self) -> Vec<String> {
        self.trials
            .iter()
            .filter(|trial| trial.grade == GradeKind::Violation)
            .flat_map(|trial| {
                trial
                    .reasons
                    .iter()
                    .map(move |found| format!("trial {}: {found}", trial.trial))
            })
            .collect()
    }
}

/// A slice's pass^k: how many of its cases passed at `k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Slice {
    /// The cases that passed at `k`.
    pub passed: u32,
    /// The cases in the slice.
    pub cases: u32,
}

/// One eval run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The arm that was evaluated.
    pub arm: String,
    /// What the run was measured with.
    pub version: VersionTuple,
    /// Which of the version tuple and the counts below the arm reports at all.
    #[serde(default)]
    pub measures: Measures,
    /// How the run was run.
    #[serde(default)]
    pub run: RunSettings,
    /// What the run was narrowed to, when it left cases out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrowed_to: Option<Narrowing>,
    /// The experiment run the run's traces were marked as, when traces were exported. It says
    /// what the traces carry, not that a trace backend received them. `#[serde(default)]`, so
    /// a report written before the field existed still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<ExperimentRef>,
    /// The tokens the runs reported, input and output.
    pub tokens: u64,
    /// The run ledger can count a tool call twice when the client reconnects, so the tool-call
    /// counts in the run records are approximate. Always true.
    pub tool_calls_approximate: bool,
    /// Why the run is incomplete: each trial that errored or did not start. Empty when
    /// complete.
    pub incomplete: Vec<String>,
    /// pass^k over the whole set.
    pub overall: Slice,
    /// pass^k per category.
    pub by_category: BTreeMap<String, Slice>,
    /// pass^k per profile.
    pub by_profile: BTreeMap<String, Slice>,
    /// The cases, ordered by id.
    pub cases: Vec<CaseResult>,
    /// Per case, how many of its trials' candidates, chosen or not, hold each action type, so
    /// a verb planners reach for over another shows. `#[serde(default)]`, so a report written
    /// before the field existed still reads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub candidates_by_action: BTreeMap<String, BTreeMap<String, u32>>,
}

impl Report {
    /// The report of `cases` run by `arm` with `version`, having used `tokens`.
    #[must_use]
    pub fn new(arm: &str, version: VersionTuple, mut cases: Vec<CaseResult>, tokens: u64) -> Self {
        cases.sort_by(|a, b| a.id.cmp(&b.id));
        let k = version.trials;
        let mut overall = Slice::default();
        let mut by_category: BTreeMap<String, Slice> = BTreeMap::new();
        let mut by_profile: BTreeMap<String, Slice> = BTreeMap::new();
        let mut incomplete = Vec::new();
        let mut candidates_by_action = BTreeMap::new();
        for case in &cases {
            let counts = case.candidates_by_action();
            if !counts.is_empty() {
                candidates_by_action.insert(case.id.clone(), counts);
            }
            let passed = u32::from(case.passed_at(k));
            for slice in [
                &mut overall,
                by_category.entry(case.category.clone()).or_default(),
                by_profile.entry(case.profile.clone()).or_default(),
            ] {
                slice.cases += 1;
                slice.passed += passed;
            }
            for trial in &case.trials {
                if trial.grade == GradeKind::Errored {
                    incomplete.push(format!(
                        "{} trial {} errored: {}",
                        case.id,
                        trial.trial,
                        trial.reasons.join("; ")
                    ));
                }
            }
            if case.not_run > 0 {
                incomplete.push(format!(
                    "{}: {} trial(s) did not run",
                    case.id, case.not_run
                ));
            }
        }
        Self {
            arm: arm.to_owned(),
            version,
            measures: Measures::default(),
            run: RunSettings::default(),
            narrowed_to: None,
            experiment: None,
            tokens,
            tool_calls_approximate: true,
            incomplete,
            overall,
            by_category,
            by_profile,
            cases,
            candidates_by_action,
        }
    }

    /// The same report, of an arm that measures only `measures`.
    #[must_use]
    pub fn measuring(mut self, measures: Measures) -> Self {
        self.measures = measures;
        self
    }

    /// The same report, of a run that was run with `settings`.
    #[must_use]
    pub fn run_with(mut self, settings: RunSettings) -> Self {
        self.run = settings;
        self
    }

    /// The same report, of a run narrowed to `narrowing`. An empty one is the whole set.
    #[must_use]
    pub fn narrowed_to(mut self, narrowing: Narrowing) -> Self {
        self.narrowed_to = (!narrowing.is_empty()).then_some(narrowing);
        self
    }

    /// The same report, of a run whose traces were marked as `experiment`.
    #[must_use]
    pub fn exported_as(mut self, experiment: Option<ExperimentRef>) -> Self {
        self.experiment = experiment;
        self
    }

    /// Each trial that was attempted again, as `case trial N: reason; reason`.
    #[must_use]
    pub fn retried(&self) -> Vec<String> {
        self.cases
            .iter()
            .flat_map(|case| {
                case.retried()
                    .into_iter()
                    .map(move |line| format!("{} {line}", case.id))
            })
            .collect()
    }

    /// What the run was measured with, in one line.
    #[must_use]
    pub fn measured_with(&self) -> String {
        let model = if self.version.model.is_empty() {
            "(none reported)"
        } else {
            &self.version.model
        };
        let tokens = if self.measures.tokens {
            format!("{} tokens", self.tokens)
        } else {
            "token usage not reported by this arm".to_owned()
        };
        let mut line = format!(
            "{}, model {model}, set v{}, {tokens}",
            self.arm, self.version.set_version
        );
        let _ = write!(
            line,
            ", {} trial(s) at a time, {} further attempt(s)",
            self.run.concurrency, self.run.retries
        );
        if !self.measures.run_records {
            line.push_str("; the arm has no skills, tools, step limit or run records");
        }
        line
    }

    /// Whether every trial ran and was graded.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }

    /// Whether any trial is a violation.
    #[must_use]
    pub fn has_violations(&self) -> bool {
        self.cases.iter().any(|case| case.violating() > 0)
    }

    /// The report as a table: one line per case, then one per slice.
    #[must_use]
    pub fn table(&self) -> String {
        let k = self.version.trials;
        let mut out = String::new();
        for case in &self.cases {
            let mark = if case.violating() > 0 {
                "VIOLATION"
            } else if case.errored() > 0 || case.not_run > 0 {
                "INCOMPLETE"
            } else if case.passed_at(k) {
                "pass"
            } else {
                "FAIL"
            };
            let _ = writeln!(
                out,
                "{:<40} {:<24} {:<12} {}/{k}  {mark}",
                case.id,
                case.category,
                case.profile,
                case.passed()
            );
            if let Some(counts) = self.candidates_by_action.get(&case.id) {
                let counts: Vec<String> = counts
                    .iter()
                    .map(|(action_type, n)| format!("{action_type} {n}"))
                    .collect();
                let _ = writeln!(out, "    candidates holding: {}", counts.join(", "));
            }
        }
        let _ = writeln!(out);
        for (name, slice) in &self.by_category {
            let _ = writeln!(
                out,
                "{name:<40} {}/{} cases at k={k}",
                slice.passed, slice.cases
            );
        }
        for (name, slice) in &self.by_profile {
            let _ = writeln!(
                out,
                "{name:<40} {}/{} cases at k={k}",
                slice.passed, slice.cases
            );
        }
        let _ = writeln!(
            out,
            "{:<40} {}/{} cases at k={k}",
            "all", self.overall.passed, self.overall.cases
        );
        let _ = writeln!(out, "\n{}", self.measured_with());
        for line in self.retried() {
            let _ = writeln!(out, "retried: {line}");
        }
        if let Some(narrowing) = &self.narrowed_to {
            let _ = writeln!(out, "{}", narrowing.describe());
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn version(k: u32) -> VersionTuple {
        VersionTuple {
            engine: "0.1.0".to_owned(),
            model: "m/1".to_owned(),
            skills: BTreeMap::from([("propose-plan".to_owned(), "abc".to_owned())]),
            tools: BTreeMap::from([("alpha".to_owned(), vec!["submit_plan".to_owned()])]),
            set_version: 1,
            set_digest: "d1".to_owned(),
            trials: k,
            max_steps: 12,
            joint_max_steps: 8,
            analyst_max_steps: 10,
            planners: 3,
            planner_reads: false,
        }
    }

    pub(crate) fn trial(n: u32, grade: Grade) -> TrialResult {
        TrialResult::new(n, "submitted", grade, None, None, None)
    }

    pub(crate) fn case(id: &str, category: &str, profile: &str, grades: Vec<Grade>) -> CaseResult {
        CaseResult {
            id: id.to_owned(),
            category: category.to_owned(),
            profile: profile.to_owned(),
            baseline: None,
            trials: grades
                .into_iter()
                .zip(1..)
                .map(|(grade, n)| trial(n, grade))
                .collect(),
            not_run: 0,
        }
    }

    pub(crate) fn fail() -> Grade {
        Grade::Fail(vec!["beats_baseline: no".to_owned()])
    }

    #[test]
    fn a_case_passes_at_k_only_when_every_trial_passes() {
        let all = case(
            "a",
            "late",
            "alpha",
            vec![Grade::Pass, Grade::Pass, Grade::Pass],
        );
        assert!(all.passed_at(3));
        assert_eq!((all.passed(), all.failed()), (3, 0));
        let two = case("b", "late", "alpha", vec![Grade::Pass, fail(), Grade::Pass]);
        assert!(!two.passed_at(3));
        assert_eq!((two.passed(), two.failed()), (2, 1));
    }

    #[test]
    fn slices_are_the_share_of_cases_per_category_profile_and_overall() {
        let report = Report::new(
            "agent",
            version(2),
            vec![
                case("b", "late", "beta", vec![Grade::Pass, fail()]),
                case("a", "late", "alpha", vec![Grade::Pass, Grade::Pass]),
                case("c", "abstain", "alpha", vec![Grade::Pass, Grade::Pass]),
            ],
            900,
        );
        assert_eq!(report.cases[0].id, "a", "ordered by id");
        assert_eq!(
            report.by_category["late"],
            Slice {
                passed: 1,
                cases: 2
            }
        );
        assert_eq!(
            report.by_category["abstain"],
            Slice {
                passed: 1,
                cases: 1
            }
        );
        assert_eq!(
            report.by_profile["alpha"],
            Slice {
                passed: 2,
                cases: 2
            }
        );
        assert_eq!(
            report.by_profile["beta"],
            Slice {
                passed: 0,
                cases: 1
            }
        );
        assert_eq!(
            report.overall,
            Slice {
                passed: 2,
                cases: 3
            }
        );
        assert!(report.is_complete());
        assert!(report.table().contains("1/2  FAIL"), "{}", report.table());
    }

    #[test]
    fn an_errored_trial_is_errored_not_failed_and_makes_the_run_incomplete() {
        let mut thin = case("b", "late", "alpha", vec![Grade::Pass]);
        thin.not_run = 1;
        let report = Report::new(
            "agent",
            version(2),
            vec![
                case(
                    "a",
                    "late",
                    "alpha",
                    vec![Grade::Pass, Grade::Errored(vec!["429".to_owned()])],
                ),
                thin,
            ],
            0,
        );
        assert_eq!(
            (report.cases[0].errored(), report.cases[0].failed()),
            (1, 0)
        );
        assert!(!report.is_complete());
        assert_eq!(
            report.incomplete,
            ["a trial 2 errored: 429", "b: 1 trial(s) did not run"]
        );
        assert_eq!(report.overall.passed, 0);
    }

    #[test]
    fn a_rejected_trial_counts_as_failed_and_keeps_the_run_complete() {
        let refused = TrialResult::new(
            2,
            "rejected",
            Grade::Fail(vec![
                "outcome: the reply was refused: no such verb".to_owned(),
            ]),
            None,
            None,
            None,
        );
        let mut case = case("a", "late", "alpha", vec![Grade::Pass]);
        case.trials.push(refused);
        assert_eq!((case.passed(), case.failed(), case.errored()), (1, 1, 0));
        assert!(!case.passed_at(2));
        let report = Report::new("legacy", version(2), vec![case], 0);
        assert_eq!(report.overall.passed, 0);
        assert!(report.is_complete());
        assert!(report.table().contains("1/2  FAIL"), "{}", report.table());
    }

    #[test]
    fn a_stored_report_without_measures_reads_as_measuring_everything() {
        let report = Report::new("agent", version(1), Vec::new(), 42);
        let mut json = serde_json::to_value(&report).expect("json");
        json.as_object_mut().expect("object").remove("measures");
        let read: Report = serde_json::from_value(json).expect("report");
        assert_eq!(read.measures, Measures::default());
        assert!(read.measures.tokens && read.measures.run_records);
        assert!(read.table().contains("42 tokens"), "{}", read.table());
    }

    #[test]
    fn a_stored_report_without_a_joint_step_limit_is_still_read() {
        let report = Report::new("agent", version(1), Vec::new(), 42);
        assert_eq!(report.version.joint_max_steps, 8);
        assert_eq!(report.version.analyst_max_steps, 10);
        let mut json = serde_json::to_value(&report).expect("json");
        json["version"]
            .as_object_mut()
            .expect("object")
            .remove("joint_max_steps");
        let read: Report = serde_json::from_value(json).expect("report");
        // Not recorded: its joint cases ran under the one step limit.
        assert_eq!(
            (read.version.max_steps, read.version.joint_max_steps),
            (12, 0)
        );
    }

    #[test]
    fn an_arm_that_reports_no_tokens_says_so_instead_of_showing_zero() {
        let report =
            Report::new("legacy", version(1), Vec::new(), 0).measuring(Measures::CALLS_ONLY);
        let table = report.table();
        assert!(
            table.contains("token usage not reported by this arm"),
            "{table}"
        );
        assert!(table.contains("no skills, tools, step limit"), "{table}");
        assert!(!table.contains("0 tokens"), "{table}");
        let json = serde_json::to_string(&report).expect("json");
        assert_eq!(
            serde_json::from_str::<Report>(&json).expect("report"),
            report
        );
    }

    #[test]
    fn a_retried_trial_keeps_its_errored_attempts_and_the_table_names_them() {
        let mut retried = case("a", "late", "alpha", vec![Grade::Pass, Grade::Pass]);
        retried.trials[1].errored_attempts =
            vec!["the answer stalled for 30s".to_owned(), "429".to_owned()];
        let plain = case("b", "late", "alpha", vec![Grade::Pass, Grade::Pass]);
        assert_eq!(
            retried.retried(),
            ["trial 2: the answer stalled for 30s; 429"]
        );
        assert!(plain.retried().is_empty());

        let report = Report::new("agent", version(2), vec![retried, plain], 0);
        assert!(
            report.is_complete(),
            "a retried trial that passed is graded"
        );
        assert_eq!(
            report.retried(),
            ["a trial 2: the answer stalled for 30s; 429"]
        );
        let table = report.table();
        assert!(
            table.contains("retried: a trial 2: the answer stalled for 30s; 429"),
            "{table}"
        );
        assert_eq!(table.matches("retried:").count(), 1, "{table}");

        let json = serde_json::to_value(&report).expect("json");
        let trials = &json["cases"][0]["trials"];
        assert!(trials[0].get("errored_attempts").is_none(), "{trials}");
        assert_eq!(trials[1]["errored_attempts"][1], "429");
        // A trial stored before the field existed has none.
        let read: Report = serde_json::from_value(json).expect("report");
        assert_eq!(read, report);
        assert!(read.cases[1].trials[0].errored_attempts.is_empty());
    }

    #[test]
    fn a_report_says_how_it_was_run_and_an_older_one_reads_as_sequential() {
        let report = Report::new("agent", version(1), Vec::new(), 42).run_with(RunSettings {
            concurrency: 3,
            retries: 2,
        });
        let table = report.table();
        assert!(
            table.contains("3 trial(s) at a time, 2 further attempt(s)"),
            "{table}"
        );
        let mut json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["run"]["concurrency"], 3);
        assert_eq!(json["run"]["retries"], 2);

        json.as_object_mut().expect("object").remove("run");
        let read: Report = serde_json::from_value(json).expect("report");
        assert_eq!(
            read.run,
            RunSettings {
                concurrency: 1,
                retries: 0
            }
        );
        assert!(
            read.table()
                .contains("1 trial(s) at a time, 0 further attempt(s)")
        );
        // How it was run is not what it was measured with.
        assert_eq!(read.version, report.version);
    }

    #[test]
    fn a_narrowed_report_says_by_what_and_what_it_is_not() {
        let whole = Report::new("agent", version(1), Vec::new(), 0);
        assert_eq!(
            whole.clone().narrowed_to(Narrowing::default()).narrowed_to,
            None
        );
        assert!(!whole.table().contains("narrowed"));
        let json = serde_json::to_value(&whole).expect("json");
        assert!(json.get("narrowed_to").is_none(), "{json}");
        assert_eq!(
            serde_json::from_value::<Report>(json).expect("report"),
            whole
        );

        let narrowed = whole.narrowed_to(Narrowing {
            cases: vec!["a".to_owned()],
            categories: Vec::new(),
            profiles: vec!["alpha".to_owned(), "beta".to_owned()],
        });
        let table = narrowed.table();
        assert!(
            table.contains(
                "narrowed to case a; profile alpha, beta: no evidence for a promotion verdict"
            ),
            "{table}"
        );
        let json = serde_json::to_value(&narrowed).expect("json");
        assert_eq!(json["narrowed_to"]["profiles"][1], "beta");
        assert_eq!(
            serde_json::from_value::<Report>(json).expect("report"),
            narrowed
        );
    }

    #[test]
    fn violations_are_listed_by_trial() {
        let found = Grade::Violation(vec!["forbidden action notify on x".to_owned()]);
        let case = case("a", "injection", "alpha", vec![Grade::Pass, found]);
        assert_eq!(case.violating(), 1);
        assert_eq!(case.violations(), ["trial 2: forbidden action notify on x"]);
    }

    #[test]
    fn a_cases_candidates_are_counted_by_action_type_chosen_or_not() {
        let plan = |types: &[&str]| ReportedPlan {
            actions: types
                .iter()
                .map(|action_type| ProposedAction {
                    action_type: ekg_domain::ActionType((*action_type).to_owned()),
                    target: ekg_domain::EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
                    params: BTreeMap::new(),
                })
                .collect(),
            explanation: String::new(),
        };
        let mut joint = case("joint", "joint_plan", "portfolio", vec![Grade::Pass]);
        joint.trials[0].plan = Some(plan(&["re_promise_date", "re_promise_date"]));
        joint.trials[0].other_plans = vec![
            plan(&["re_promise_date", "reschedule_work_order"]),
            plan(&["reschedule_work_order"]),
        ];
        let report = Report::new(
            "agent",
            version(1),
            vec![joint, case("quiet", "late", "alpha", vec![fail()])],
            0,
        );
        assert_eq!(
            report.candidates_by_action,
            BTreeMap::from([(
                "joint".to_owned(),
                BTreeMap::from([
                    ("re_promise_date".to_owned(), 2),
                    ("reschedule_work_order".to_owned(), 2),
                ])
            )])
        );
        assert!(
            report
                .table()
                .contains("candidates holding: re_promise_date 2, reschedule_work_order 2")
        );
    }

    #[test]
    fn a_report_round_trips_through_json_and_says_its_counts_are_approximate() {
        let report = Report::new(
            "agent",
            version(1),
            vec![case("a", "late", "alpha", vec![fail()])],
            42,
        );
        let json = serde_json::to_string(&report).expect("json");
        assert!(json.contains("\"tool_calls_approximate\":true"));
        assert_eq!(
            serde_json::from_str::<Report>(&json).expect("report"),
            report
        );
    }

    #[test]
    fn a_declined_trial_shows_its_closing_answer_and_an_older_result_reads_without_one() {
        let run = |closing_answer: Option<&str>| RunRecord {
            termination: crate::reasoner::Termination::NoPlan,
            steps: 4,
            input_tokens: 10,
            output_tokens: 5,
            model: "m".to_owned(),
            elapsed_ms: 1,
            tool_calls: BTreeMap::new(),
            rejections: Vec::new(),
            skills: BTreeMap::new(),
            tools: Vec::new(),
            trace_id: "t".to_owned(),
            closing_answer: closing_answer.map(str::to_owned),
            role: None,
            finding: None,
        };
        let declined =
            |answer| TrialResult::new(1, "declined", fail(), None, None, Some(run(answer)));

        let with = declined(Some("The build is on plan."));
        let json = serde_json::to_value(&with).expect("json");
        assert_eq!(json["run"]["closing_answer"], "The build is on plan.");
        assert_eq!(
            serde_json::from_value::<TrialResult>(json).expect("result"),
            with
        );

        // A result written before the field existed has no such key, and neither has one
        // written without an answer.
        let without = declined(None);
        let json = serde_json::to_value(&without).expect("json");
        assert!(json["run"].get("closing_answer").is_none());
        assert_eq!(
            serde_json::from_value::<TrialResult>(json).expect("an older result reads"),
            without
        );
        assert_eq!(with.grade, without.grade, "the answer is not graded");
    }

    #[test]
    fn a_report_names_its_experiment_run_only_when_its_traces_were_marked() {
        let plain = Report::new("agent", version(1), Vec::new(), 0);
        let json = serde_json::to_value(&plain).expect("json");
        assert!(json.get("experiment").is_none(), "nothing to name");
        // A report written before the field existed reads as one without it.
        assert_eq!(
            serde_json::from_value::<Report>(json)
                .expect("report")
                .experiment,
            None
        );

        let marked = plain.exported_as(Some(ExperimentRef {
            id: "20271014T120000Z-agent".to_owned(),
            name: "v1 agent 20271014T120000Z".to_owned(),
        }));
        let json = serde_json::to_value(&marked).expect("json");
        assert_eq!(json["experiment"]["id"], "20271014T120000Z-agent");
        assert_eq!(
            serde_json::from_value::<Report>(json).expect("report"),
            marked
        );
    }
}
