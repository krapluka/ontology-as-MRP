//! A paired report: the legacy arm and the agent arm on the same cases, side by side.
//!
//! It adds nothing to either arm's report but the comparison, and it judges nothing: which
//! numbers an arm has to reach is not decided here.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::report::{CaseResult, Report, Slice};

/// Which arms pass a case at `k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairVerdict {
    /// Both arms pass.
    Both,
    /// Only the agent passes.
    AgentOnly,
    /// Only the legacy arm passes.
    LegacyOnly,
    /// Neither passes.
    Neither,
}

impl PairVerdict {
    fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::AgentOnly => "agent only",
            Self::LegacyOnly => "legacy only",
            Self::Neither => "neither",
        }
    }
}

/// One arm's result on a case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmCase {
    /// The trials that passed.
    pub passed: u32,
    /// The trials per case (`k`).
    pub of: u32,
    /// The violations, as `trial N: finding`.
    pub violations: Vec<String>,
}

impl ArmCase {
    fn of(case: &CaseResult, k: u32) -> Self {
        Self {
            passed: case.passed(),
            of: k,
            violations: case.violations(),
        }
    }
}

/// One case under both arms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedCase {
    /// The case's id.
    pub id: String,
    /// Its category.
    pub category: String,
    /// Its profile.
    pub profile: String,
    /// The legacy arm's result.
    pub legacy: ArmCase,
    /// The agent's result.
    pub agent: ArmCase,
    /// Which of them pass at `k`.
    pub verdict: PairVerdict,
}

/// Both arms' runs of the same cases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedReport {
    /// The legacy arm's report.
    pub legacy: Report,
    /// The agent's report.
    pub agent: Report,
    /// The cases, ordered by id.
    pub cases: Vec<PairedCase>,
    /// The cases only one arm passes at `k`.
    pub differ: u32,
    /// Why the pair is incomplete: each arm's reasons, prefixed with the arm. Empty when
    /// both runs are complete.
    pub incomplete: Vec<String>,
    /// The digest of the promotion criteria document the run started under, if there was
    /// one. It ties the run to criteria that existed before it; it changes no grade.
    #[serde(default)]
    pub criteria: Option<String>,
}

impl PairedReport {
    /// Pair `legacy` with `agent`.
    ///
    /// # Errors
    /// A message when the two did not run the same set, the same `k` or the same cases: such
    /// reports do not compare.
    pub fn new(legacy: Report, agent: Report) -> Result<Self, String> {
        if legacy.version.set_digest != agent.version.set_digest {
            return Err(format!(
                "the arms ran different eval sets (legacy {}, agent {})",
                legacy.version.set_digest, agent.version.set_digest
            ));
        }
        let k = agent.version.trials;
        if legacy.version.trials != k {
            return Err(format!(
                "the arms ran different k (legacy {}, agent {k})",
                legacy.version.trials
            ));
        }
        let ids = |report: &Report| -> Vec<String> {
            report.cases.iter().map(|case| case.id.clone()).collect()
        };
        if ids(&legacy) != ids(&agent) {
            return Err(format!(
                "the arms ran different cases (legacy {}, agent {})",
                ids(&legacy).join(", "),
                ids(&agent).join(", ")
            ));
        }
        let cases: Vec<PairedCase> = legacy
            .cases
            .iter()
            .zip(&agent.cases)
            .map(|(old, new)| PairedCase {
                id: new.id.clone(),
                category: new.category.clone(),
                profile: new.profile.clone(),
                legacy: ArmCase::of(old, k),
                agent: ArmCase::of(new, k),
                verdict: match (old.passed_at(k), new.passed_at(k)) {
                    (true, true) => PairVerdict::Both,
                    (false, true) => PairVerdict::AgentOnly,
                    (true, false) => PairVerdict::LegacyOnly,
                    (false, false) => PairVerdict::Neither,
                },
            })
            .collect();
        let differ = cases
            .iter()
            .filter(|case| {
                matches!(
                    case.verdict,
                    PairVerdict::AgentOnly | PairVerdict::LegacyOnly
                )
            })
            .count();
        let incomplete = [&legacy, &agent]
            .into_iter()
            .flat_map(|report| {
                report
                    .incomplete
                    .iter()
                    .map(move |why| format!("{}: {why}", report.arm))
            })
            .collect();
        Ok(Self {
            legacy,
            agent,
            cases,
            differ: u32::try_from(differ).unwrap_or(u32::MAX),
            incomplete,
            criteria: None,
        })
    }

    /// The same report, of a run that started under the criteria document with `digest`.
    #[must_use]
    pub fn under(mut self, digest: Option<String>) -> Self {
        self.criteria = digest;
        self
    }

    /// Whether both runs are complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }

    /// The pair as a table: what each arm ran with, one line per case, then pass^k per
    /// slice for each arm.
    #[must_use]
    pub fn table(&self) -> String {
        let k = self.agent.version.trials;
        let mut out = String::new();
        let _ = writeln!(out, "{}", self.legacy.measured_with());
        let _ = writeln!(out, "{}", self.agent.measured_with());
        let _ = match &self.criteria {
            Some(digest) => writeln!(out, "under criteria {digest}\n"),
            None => writeln!(out, "under no criteria\n"),
        };
        let _ = writeln!(
            out,
            "{:<40} {:<24} {:<12} {:<7} {:<7} passes",
            "case", "category", "profile", "legacy", "agent"
        );
        for case in &self.cases {
            let mut line = format!(
                "{:<40} {:<24} {:<12} {:<7} {:<7} {}",
                case.id,
                case.category,
                case.profile,
                format!("{}/{k}", case.legacy.passed),
                format!("{}/{k}", case.agent.passed),
                case.verdict.name()
            );
            for (arm, result) in [("legacy", &case.legacy), ("agent", &case.agent)] {
                if !result.violations.is_empty() {
                    let _ = write!(line, "  VIOLATION ({arm})");
                }
            }
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out);
        let slices = |name: &str, old: Option<&Slice>, new: &Slice| {
            let old = old.copied().unwrap_or_default();
            format!(
                "{name:<40} legacy {}/{}, agent {}/{} cases at k={k}",
                old.passed, old.cases, new.passed, new.cases
            )
        };
        let by = |old: &BTreeMap<String, Slice>, new: &BTreeMap<String, Slice>| -> String {
            new.iter()
                .map(|(name, slice)| slices(name, old.get(name), slice) + "\n")
                .collect()
        };
        out.push_str(&by(&self.legacy.by_category, &self.agent.by_category));
        out.push_str(&by(&self.legacy.by_profile, &self.agent.by_profile));
        let _ = writeln!(
            out,
            "{}",
            slices("all", Some(&self.legacy.overall), &self.agent.overall)
        );
        let _ = writeln!(
            out,
            "\nthe arms differ on {} of {} case(s)",
            self.differ,
            self.cases.len()
        );
        for report in [&self.legacy, &self.agent] {
            for line in report.retried() {
                let _ = writeln!(out, "retried: {}: {line}", report.arm);
            }
        }
        if let Some(narrowing) = &self.agent.narrowed_to {
            let _ = writeln!(out, "{}", narrowing.describe());
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::grade::Grade;
    use super::super::report::Measures;
    use super::super::report::tests::{case, fail, version};
    use super::*;

    fn report(arm: &str, cases: Vec<CaseResult>) -> Report {
        let mut version = version(2);
        version.model = format!("{arm}-model");
        Report::new(arm, version, cases, 0)
    }

    fn pass() -> Vec<Grade> {
        vec![Grade::Pass, Grade::Pass]
    }

    fn flaky() -> Vec<Grade> {
        vec![Grade::Pass, fail()]
    }

    fn four(legacy: [Vec<Grade>; 4], agent: [Vec<Grade>; 4]) -> PairedReport {
        let cases = |grades: [Vec<Grade>; 4]| -> Vec<CaseResult> {
            ["a", "b", "c", "d"]
                .into_iter()
                .zip(grades)
                .map(|(id, grades)| case(id, "late", "alpha", grades))
                .collect()
        };
        PairedReport::new(
            report("legacy", cases(legacy)),
            report("agent", cases(agent)),
        )
        .expect("paired")
    }

    #[test]
    fn each_case_says_which_arms_pass_and_only_a_split_counts_as_differing() {
        let paired = four(
            [pass(), flaky(), pass(), flaky()],
            [pass(), pass(), flaky(), flaky()],
        );
        let verdicts: Vec<PairVerdict> = paired.cases.iter().map(|case| case.verdict).collect();
        assert_eq!(
            verdicts,
            [
                PairVerdict::Both,
                PairVerdict::AgentOnly,
                PairVerdict::LegacyOnly,
                PairVerdict::Neither
            ]
        );
        assert_eq!(paired.differ, 2);
        assert_eq!(
            (paired.cases[1].legacy.passed, paired.cases[1].agent.passed),
            (1, 2)
        );
        assert_eq!(paired.cases[1].legacy.of, 2);
        assert!(paired.is_complete());
    }

    #[test]
    fn a_violation_is_shown_under_the_arm_that_made_it() {
        let found = Grade::Violation(vec!["forbidden text \"777\" in to of move".to_owned()]);
        let paired = PairedReport::new(
            report(
                "legacy",
                vec![case("a", "injection", "alpha", vec![found, Grade::Pass])],
            ),
            report("agent", vec![case("a", "injection", "alpha", pass())]),
        )
        .expect("paired");
        assert_eq!(
            paired.cases[0].legacy.violations,
            ["trial 1: forbidden text \"777\" in to of move"]
        );
        assert!(paired.cases[0].agent.violations.is_empty());
        let table = paired.table();
        assert!(table.contains("VIOLATION (legacy)"), "{table}");
        assert!(!table.contains("VIOLATION (agent)"), "{table}");
    }

    #[test]
    fn an_incomplete_arm_makes_the_pair_incomplete_and_is_named() {
        let errored = vec![Grade::Pass, Grade::Errored(vec!["429".to_owned()])];
        let paired = PairedReport::new(
            report("legacy", vec![case("a", "late", "alpha", errored)]),
            report("agent", vec![case("a", "late", "alpha", pass())]),
        )
        .expect("paired");
        assert!(!paired.is_complete());
        assert_eq!(paired.incomplete, ["legacy: a trial 2 errored: 429"]);
    }

    #[test]
    fn a_report_names_the_criteria_it_ran_under_and_an_older_one_reads_as_none() {
        let pair = || {
            PairedReport::new(
                report("legacy", vec![case("a", "late", "alpha", pass())]),
                report("agent", vec![case("a", "late", "alpha", pass())]),
            )
            .expect("paired")
        };
        assert_eq!(pair().criteria, None);
        assert!(pair().table().contains("under no criteria"));
        let under = pair().under(Some("abc123".to_owned()));
        assert!(under.table().contains("under criteria abc123"));
        // The digest is about the run, not the results.
        assert_eq!(under.cases, pair().cases);

        let mut stored = serde_json::to_value(&under).expect("json");
        stored.as_object_mut().expect("object").remove("criteria");
        let read: PairedReport = serde_json::from_value(stored).expect("report");
        assert_eq!(read.criteria, None);
        let json = serde_json::to_string(&under).expect("json");
        assert_eq!(
            serde_json::from_str::<PairedReport>(&json).expect("report"),
            under
        );
    }

    #[test]
    fn reports_of_another_set_k_or_selection_are_refused() {
        let one = |id: &str| vec![case(id, "late", "alpha", pass())];
        let mut other_set = report("legacy", one("a"));
        other_set.version.set_digest = "d2".to_owned();
        let why = PairedReport::new(other_set, report("agent", one("a"))).expect_err("set");
        assert!(why.contains("d2") && why.contains("d1"), "{why}");

        let mut other_k = report("legacy", one("a"));
        other_k.version.trials = 3;
        let why = PairedReport::new(other_k, report("agent", one("a"))).expect_err("k");
        assert!(why.contains("different k"), "{why}");

        let why = PairedReport::new(report("legacy", one("a")), report("agent", one("b")))
            .expect_err("cases");
        assert!(why.contains("different cases"), "{why}");
    }

    #[test]
    fn the_table_sets_both_arms_side_by_side_under_their_models() {
        let mut legacy = report(
            "legacy",
            vec![
                case("a", "late", "alpha", flaky()),
                case("b", "abstain", "beta", pass()),
            ],
        )
        .measuring(Measures::CALLS_ONLY);
        legacy.version.skills.clear();
        let agent = report(
            "agent",
            vec![
                case("a", "late", "alpha", pass()),
                case("b", "abstain", "beta", pass()),
            ],
        );
        let table = PairedReport::new(legacy, agent).expect("paired").table();
        assert!(table.contains("model legacy-model"), "{table}");
        assert!(table.contains("model agent-model"), "{table}");
        let line = |start: &str| -> String {
            let found = table.lines().find(|line| line.starts_with(start));
            found
                .expect(start)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(line("a "), "a late alpha 1/2 2/2 agent only");
        assert_eq!(line("late"), "late legacy 0/1, agent 1/1 cases at k=2");
        assert_eq!(line("beta"), "beta legacy 1/1, agent 1/1 cases at k=2");
        assert_eq!(line("all"), "all legacy 1/2, agent 2/2 cases at k=2");
        assert!(
            table.contains("the arms differ on 1 of 2 case(s)"),
            "{table}"
        );
    }
}
