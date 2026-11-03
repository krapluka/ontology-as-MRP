//! Promotion criteria for the agent, and the verdict on the evidence gathered under them.
//!
//! The criteria are one checked-in document ([`PROMOTION_FILE`], in the eval set's directory):
//! thresholds on the paired eval and on the shadow records. A document may also state joint
//! bounds, on the joint cases and the joint shadow records: only then do the criteria cover a
//! portfolio's joint slate. A document counts once it is
//! registered with the rollout ([`crate::rollout::apply_register`]), and from then on only for
//! evidence gathered after it:
//!
//! - a paired eval report carries the digest of the document it ran under, which it cannot
//!   carry before that document was written;
//! - a shadow window carries the digest of the registration that was active when it opened,
//!   and only records whose run started inside such a window count.
//!
//! [`verdict`] is a pure function of the rollout state, a paired report and the shadow records.
//! It checks each criterion and shows what it observed; evidence that is missing is a criterion
//! that does not hold, not an error. It decides nothing: whether the agent's plans may become
//! authoritative is a recorded decision of its own, taken against a verdict.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use ekg_cognition::eval::{PairVerdict, PairedCase, PairedReport, digest};
use serde::{Deserialize, Serialize};

use crate::eval::EVAL_JOINT_PROFILES;
use crate::rollout::{Mode, RolloutState};
use crate::shadow::ShadowRecord;
use crate::shadow_report::{self, Sources};

/// The criteria document of an eval set.
pub const PROMOTION_FILE: &str = "promotion.yaml";
/// The document format that may state joint bounds. The earlier format, 1, is still read.
const FORMAT: u32 = 2;

/// What the paired eval must show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalCriteria {
    /// The eval set version the thresholds were written for.
    pub set_version: u32,
    /// The fewest trials per case (`k`).
    pub min_trials: u32,
    /// The fewest cases the agent passes at `k`.
    pub min_passed: u32,
    /// The fewest cases the agent passes at `k`, per category.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub min_passed_by_category: BTreeMap<String, u32>,
    /// The fewest cases the agent passes at `k`, per profile.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub min_passed_by_profile: BTreeMap<String, u32>,
    /// The most cases in which a trial of the agent is a violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_violating_cases: Option<u32>,
    /// The most cases the legacy arm passes at `k` and the agent does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_legacy_only_cases: Option<u32>,
}

/// What the shadow records must show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowCriteria {
    /// The fewest compared records. Below it the percentages are not evaluated.
    pub min_compared: u32,
    /// The most records whose agent run failed or was aborted, as a percentage of the records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_failed_percent: Option<u32>,
    /// The most compared records where the existing path's best option ranks better, as a
    /// percentage of the compared records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_legacy_better_percent: Option<u32>,
    /// The fewest promotions taken on the paired slate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_promotions: Option<u32>,
    /// The fewest of those promotions whose executed plan the agent's slate offered, as a
    /// percentage of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_executed_offered_percent: Option<u32>,
}

/// What the joint cases of the paired eval must show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JointEvalCriteria {
    /// The fewest joint cases the agent passes at `k`.
    pub min_passed: u32,
    /// The most joint cases in which a trial of the agent is a violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_violating_cases: Option<u32>,
    /// The most joint cases the legacy arm passes at `k` and the agent does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_legacy_only_cases: Option<u32>,
}

/// What joint evidence must show: the joint cases of the paired eval, and the joint shadow
/// records, by the same bounds as point records have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JointCriteria {
    /// The thresholds on the joint cases.
    pub eval: JointEvalCriteria,
    /// The thresholds on the joint shadow records.
    pub shadow: ShadowCriteria,
}

/// When the `agent` mode switches itself back to `legacy`. A bound that is not stated
/// triggers nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackCriteria {
    /// The most agent runs that may fail in a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_agent_failures: Option<u32>,
    /// The most promotions of an agent slate that may be reversed since the mode was entered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reversed_promotions: Option<u32>,
}

/// A criteria document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criteria {
    /// The document format.
    pub version: u32,
    /// The thresholds on the paired eval.
    pub eval: EvalCriteria,
    /// The thresholds on the shadow records of points.
    pub shadow: ShadowCriteria,
    /// The thresholds on joint evidence, when the document states any. With them, the
    /// criteria cover a portfolio's joint slate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joint: Option<JointCriteria>,
    /// The rollback bounds of the `agent` mode, when the document states any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackCriteria>,
}

impl Criteria {
    /// Parse a criteria document.
    ///
    /// # Errors
    /// A message naming the field that is unknown, missing or out of range.
    pub fn parse(text: &str) -> Result<Self, String> {
        let criteria: Self =
            serde_saphyr::from_str(text).map_err(|why| format!("{PROMOTION_FILE}: {why}"))?;
        if !(1..=FORMAT).contains(&criteria.version) {
            return Err(format!(
                "{PROMOTION_FILE}: version is {}, this engine reads versions 1 to {FORMAT}",
                criteria.version
            ));
        }
        // The earlier format has no joint bounds: there the block is a field it does not know.
        if criteria.version < FORMAT && criteria.joint.is_some() {
            return Err(format!(
                "{PROMOTION_FILE}: unknown field `joint` in a version {} document",
                criteria.version
            ));
        }
        let joint = criteria.joint.as_ref().map(|joint| &joint.shadow);
        for (block, shadow) in [("", Some(&criteria.shadow)), ("joint.shadow.", joint)] {
            let Some(shadow) = shadow else { continue };
            for (field, percent) in [
                ("max_agent_failed_percent", shadow.max_agent_failed_percent),
                (
                    "max_legacy_better_percent",
                    shadow.max_legacy_better_percent,
                ),
                (
                    "min_executed_offered_percent",
                    shadow.min_executed_offered_percent,
                ),
            ] {
                if let Some(percent) = percent.filter(|percent| *percent > 100) {
                    return Err(format!(
                        "{PROMOTION_FILE}: {block}{field} is {percent}, above 100"
                    ));
                }
            }
        }
        Ok(criteria)
    }

    /// Whether the criteria cover a portfolio's joint slate: they state joint bounds.
    #[must_use]
    pub fn covers_joint(&self) -> bool {
        self.joint.is_some()
    }

    /// The rollback bounds: the document's, or none.
    #[must_use]
    pub fn rollback(&self) -> RollbackCriteria {
        self.rollback.clone().unwrap_or_default()
    }

    /// The digest that identifies a document's text: any change to it is another digest.
    #[must_use]
    pub fn digest_of(text: &str) -> String {
        digest(&BTreeMap::from([(
            PROMOTION_FILE.to_owned(),
            text.to_owned(),
        )]))
    }
}

/// The digest of the criteria document in the eval set directory `dir`, or `None` when the
/// directory holds none.
///
/// # Errors
/// A message when the document cannot be read or is not a criteria document.
pub fn digest_in(dir: &Path) -> Result<Option<String>, String> {
    let path = dir.join(PROMOTION_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).map_err(|why| format!("{}: {why}", path.display()))?;
    Criteria::parse(&text)?;
    Ok(Some(Criteria::digest_of(&text)))
}

/// Whether a criterion holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Held {
    /// The evidence satisfies it.
    Held,
    /// The evidence does not.
    Failed,
    /// There is no evidence that counts for it.
    NoEvidence,
}

/// One criterion against the evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// What is checked.
    pub name: String,
    /// What the criteria ask for.
    pub required: String,
    /// What the evidence shows.
    pub observed: String,
    /// Whether it holds.
    pub held: Held,
}

/// A shadow window tied to the active registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// When it opened, in seconds since the Unix epoch.
    pub from: u64,
    /// When the next switch ended it; `None` while it is the current mode.
    pub to: Option<u64>,
}

/// The shadow records a verdict used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowEvidence {
    /// The windows tied to the active registration.
    pub windows: Vec<Window>,
    /// The records whose run started inside one of them.
    pub counted: u32,
    /// The records left out: started outside every such window.
    pub left_out: u32,
}

/// What a run or a record ran with, as far as a verdict compares it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    /// The engine version.
    pub engine: String,
    /// The model.
    pub model: String,
    /// The skill versions, by name.
    pub skills: BTreeMap<String, String>,
}

/// The verdict on the active registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// The active registration's digest; `None` when nothing is registered.
    pub criteria: Option<String>,
    /// Whether every criterion holds.
    pub met: bool,
    /// One entry per criterion the document states, and one for the versions.
    pub checks: Vec<Check>,
    /// Why the paired report does not count as eval evidence, when it does not.
    pub eval_missing: Option<String>,
    /// The shadow records used.
    pub shadow: ShadowEvidence,
    /// What the agent's eval run ran with, when its report counts.
    pub eval_version: Option<Version>,
    /// What the counted shadow records ran with.
    pub shadow_versions: Vec<Version>,
}

/// The eval set as it is on disk: what a paired report has to be a run of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetFacts {
    /// The digest of the set's case files.
    pub digest: String,
    /// The set's case ids.
    pub case_ids: BTreeSet<String>,
}

/// Why `paired` is no eval evidence for `criteria` under `active`, if it is none.
fn eval_defect(
    criteria: &EvalCriteria,
    active: &str,
    set: &SetFacts,
    paired: Option<&PairedReport>,
) -> Option<String> {
    let Some(paired) = paired else {
        return Some("no paired report was given".to_owned());
    };
    match paired.criteria.as_deref() {
        Some(under) if under == active => {}
        Some(under) => {
            return Some(format!(
                "the report ran under other criteria ({under}), registered are {active}"
            ));
        }
        None => {
            return Some(format!(
                "the report ran under no criteria, registered are {active}"
            ));
        }
    }
    if !paired.is_complete() {
        return Some(format!(
            "the run is incomplete: {}",
            paired.incomplete.join("; ")
        ));
    }
    let version = &paired.agent.version;
    if version.set_version != criteria.set_version {
        return Some(format!(
            "the report is of eval set v{}, the criteria are for v{}",
            version.set_version, criteria.set_version
        ));
    }
    if version.set_digest != set.digest {
        return Some(format!(
            "the report is of other cases ({}) than the set's ({})",
            version.set_digest, set.digest
        ));
    }
    let ran: BTreeSet<String> = paired.cases.iter().map(|case| case.id.clone()).collect();
    if ran != set.case_ids {
        let narrowed = paired
            .agent
            .narrowed_to
            .as_ref()
            .map_or_else(String::new, |by| format!("; the run was {}", by.describe()));
        return Some(format!(
            "the report covers {} of the set's {} cases ({}){narrowed}",
            ran.len(),
            set.case_ids.len(),
            ran.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    (version.trials < criteria.min_trials).then(|| {
        format!(
            "the report ran k={}, the criteria ask for at least {}",
            version.trials, criteria.min_trials
        )
    })
}

fn at_least(name: String, required: u32, observed: Option<u32>, of: &str) -> Check {
    Check {
        name,
        required: format!("at least {required}"),
        observed: observed.map_or_else(|| "no evidence".to_owned(), |n| format!("{n}{of}")),
        held: match observed {
            Some(n) if n >= required => Held::Held,
            Some(_) => Held::Failed,
            None => Held::NoEvidence,
        },
    }
}

fn at_most(name: &str, required: u32, observed: Option<u32>) -> Check {
    Check {
        name: name.to_owned(),
        required: format!("at most {required}"),
        observed: observed.map_or_else(|| "no evidence".to_owned(), |n| n.to_string()),
        held: match observed {
            Some(n) if n <= required => Held::Held,
            Some(_) => Held::Failed,
            None => Held::NoEvidence,
        },
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The eval checks; `paired` is `None` when the report does not count.
fn eval_checks(criteria: &EvalCriteria, paired: Option<&PairedReport>) -> Vec<Check> {
    let agent = paired.map(|paired| &paired.agent);
    let mut checks = vec![at_least(
        "eval: cases the agent passes at k".to_owned(),
        criteria.min_passed,
        agent.map(|agent| agent.overall.passed),
        &agent.map_or_else(String::new, |agent| format!(" of {}", agent.overall.cases)),
    )];
    for (kind, wanted, slices) in [
        (
            "category",
            &criteria.min_passed_by_category,
            agent.map(|agent| &agent.by_category),
        ),
        (
            "profile",
            &criteria.min_passed_by_profile,
            agent.map(|agent| &agent.by_profile),
        ),
    ] {
        for (name, required) in wanted {
            let label = format!("eval: {name} cases the agent passes at k");
            checks.push(match slices.map(|slices| slices.get(name)) {
                // A name the report does not have cannot hold: the criteria ask for cases
                // that were never run.
                Some(None) => Check {
                    name: label,
                    required: format!("at least {required}"),
                    observed: format!("the report has no {kind} {name:?}"),
                    held: Held::Failed,
                },
                Some(Some(slice)) => at_least(
                    label,
                    *required,
                    Some(slice.passed),
                    &format!(" of {}", slice.cases),
                ),
                None => at_least(label, *required, None, ""),
            });
        }
    }
    if let Some(most) = criteria.max_violating_cases {
        checks.push(at_most(
            "eval: cases with a violation by the agent",
            most,
            paired.map(|paired| {
                count(
                    paired
                        .cases
                        .iter()
                        .filter(|case| !case.agent.violations.is_empty())
                        .count(),
                )
            }),
        ));
    }
    if let Some(most) = criteria.max_legacy_only_cases {
        checks.push(at_most(
            "eval: cases only the legacy arm passes",
            most,
            paired.map(|paired| {
                count(
                    paired
                        .cases
                        .iter()
                        .filter(|case| case.verdict == PairVerdict::LegacyOnly)
                        .count(),
                )
            }),
        ));
    }
    checks
}

/// Whether `case` is a joint case: it ran under a profile joint cases run under.
fn is_joint(case: &PairedCase) -> bool {
    EVAL_JOINT_PROFILES.contains(&case.profile.as_str())
}

/// The checks on the joint cases of the paired eval; `paired` is `None` when the report does
/// not count. Each counts joint cases only.
fn joint_eval_checks(criteria: &JointEvalCriteria, paired: Option<&PairedReport>) -> Vec<Check> {
    let joint = paired.map(|paired| {
        paired
            .cases
            .iter()
            .filter(|case| is_joint(case))
            .collect::<Vec<_>>()
    });
    let counted = |holds: &dyn Fn(&PairedCase) -> bool| {
        joint
            .as_ref()
            .map(|cases| count(cases.iter().filter(|case| holds(case)).count()))
    };
    let mut checks = vec![at_least(
        "joint eval: joint cases the agent passes at k".to_owned(),
        criteria.min_passed,
        counted(&|case| case.agent.of > 0 && case.agent.passed == case.agent.of),
        &joint
            .as_ref()
            .map_or_else(String::new, |cases| format!(" of {}", cases.len())),
    )];
    if let Some(most) = criteria.max_violating_cases {
        checks.push(at_most(
            "joint eval: joint cases with a violation by the agent",
            most,
            counted(&|case| !case.agent.violations.is_empty()),
        ));
    }
    if let Some(most) = criteria.max_legacy_only_cases {
        checks.push(at_most(
            "joint eval: joint cases only the legacy arm passes",
            most,
            counted(&|case| case.verdict == PairVerdict::LegacyOnly),
        ));
    }
    checks
}

/// The windows tied to `active`: each `shadow` switch that carries it, until the next switch.
fn windows(state: &RolloutState, active: &str) -> Vec<Window> {
    state
        .history
        .iter()
        .enumerate()
        .filter(|(_, change)| {
            matches!(change.mode, Mode::Shadow { .. }) && change.criteria.as_deref() == Some(active)
        })
        .map(|(index, change)| Window {
            from: change.at,
            to: state.history.get(index + 1).map(|next| next.at),
        })
        .collect()
}

/// A percentage bound, compared without division so there is nothing to round.
fn percent_check(
    name: &str,
    required: u32,
    part: u32,
    of: u32,
    at_most: bool,
    evidence: bool,
) -> Check {
    let bound = if at_most { "at most" } else { "at least" };
    let (part_wide, of_wide) = (u64::from(part) * 100, u64::from(required) * u64::from(of));
    let holds = if at_most {
        part_wide <= of_wide
    } else {
        part_wide >= of_wide
    };
    Check {
        name: name.to_owned(),
        required: format!("{bound} {required}%"),
        observed: if evidence {
            format!("{part} of {of}")
        } else {
            format!("no evidence ({part} of {of})")
        },
        held: match (evidence, holds) {
            (false, _) => Held::NoEvidence,
            (true, true) => Held::Held,
            (true, false) => Held::Failed,
        },
    }
}

/// The checks of `criteria` on the shadow records counted in `totals`, named after `of`:
/// `shadow` for point records, `joint shadow` for joint records.
fn shadow_checks(
    of: &str,
    criteria: &ShadowCriteria,
    totals: &shadow_report::Totals,
) -> Vec<Check> {
    let enough = totals.compared >= criteria.min_compared;
    let mut checks = vec![at_least(
        format!("{of}: compared records"),
        criteria.min_compared,
        Some(totals.compared),
        "",
    )];
    if let Some(percent) = criteria.max_agent_failed_percent {
        checks.push(percent_check(
            &format!("{of}: records whose agent run failed or was aborted"),
            percent,
            totals.agent_failed + totals.agent_aborted,
            totals.records,
            true,
            enough,
        ));
    }
    if let Some(percent) = criteria.max_legacy_better_percent {
        checks.push(percent_check(
            &format!("{of}: compared records where the existing path ranks better"),
            percent,
            totals.legacy_better,
            totals.compared,
            true,
            enough,
        ));
    }
    if let Some(least) = criteria.min_promotions {
        checks.push(at_least(
            format!("{of}: promotions on the paired slate"),
            least,
            Some(totals.promotions),
            "",
        ));
    }
    if let Some(percent) = criteria.min_executed_offered_percent {
        checks.push(percent_check(
            &format!("{of}: promotions whose executed plan the agent offered"),
            percent,
            totals.executed_offered_by_agent,
            totals.promotions,
            false,
            enough && totals.promotions > 0,
        ));
    }
    checks
}

fn describe(version: &Version) -> String {
    let skills: Vec<String> = version
        .skills
        .iter()
        .map(|(name, version)| format!("{name}@{version}"))
        .collect();
    format!(
        "engine {}, model {}, skills {}",
        version.engine,
        if version.model.is_empty() {
            "none reported"
        } else {
            &version.model
        },
        if skills.is_empty() {
            "none".to_owned()
        } else {
            skills.join(" ")
        }
    )
}

/// What differs between `versions`, by part: each part that has more than one value. A skill
/// only some of them name is no difference: an eval run names every skill on disk, a shadow
/// record the ones its profile loaded.
pub(crate) fn differences(versions: &BTreeSet<&Version>) -> Vec<String> {
    let mut parts: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for version in versions {
        parts
            .entry("engine".to_owned())
            .or_default()
            .insert(&version.engine);
        parts
            .entry("model".to_owned())
            .or_default()
            .insert(&version.model);
    }
    let skills: BTreeSet<&String> = versions
        .iter()
        .flat_map(|version| version.skills.keys())
        .collect();
    for skill in skills {
        let values = parts.entry(format!("skill {skill}")).or_default();
        values.extend(
            versions
                .iter()
                .filter_map(|version| version.skills.get(skill).map(String::as_str)),
        );
    }
    parts
        .into_iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(part, values)| {
            format!(
                "{part}: {}",
                values.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

/// The check that all evidence is of one version: the counted records among themselves and
/// against the agent's eval run.
fn version_check(eval: Option<&Version>, records: &[Version]) -> Check {
    let name = "one version: engine, model and skills".to_owned();
    let required = "the eval run and every counted record ran the same".to_owned();
    let Some(eval) = eval.filter(|_| !records.is_empty()) else {
        return Check {
            name,
            required,
            observed: "no evidence".to_owned(),
            held: Held::NoEvidence,
        };
    };
    let mut all: BTreeSet<&Version> = records.iter().collect();
    all.insert(eval);
    let differ = differences(&all);
    Check {
        name,
        required,
        observed: if differ.is_empty() {
            describe(eval)
        } else {
            format!("they differ in {}", differ.join("; "))
        },
        held: if differ.is_empty() {
            Held::Held
        } else {
            Held::Failed
        },
    }
}

/// The verdict on the active registration of `state`, at `now`.
///
/// `paired` is the paired eval report offered as evidence, `set` the eval set on disk,
/// `records` every shadow record and `sources` what an undecided record's decision is read
/// from, as the shadow report takes them.
///
/// The shadow bounds count point records, the joint bounds joint records. Under criteria that
/// state no joint bounds, joint records are no evidence: the verdict is the one the same
/// evidence gives without them.
#[must_use]
pub fn verdict(
    state: &RolloutState,
    set: &SetFacts,
    paired: Option<&PairedReport>,
    records: &[ShadowRecord],
    sources: &Sources,
    now: u64,
) -> Verdict {
    let Some(active) = state.active() else {
        return Verdict {
            criteria: None,
            met: false,
            checks: Vec::new(),
            eval_missing: None,
            shadow: ShadowEvidence::default(),
            eval_version: None,
            shadow_versions: Vec::new(),
        };
    };
    let criteria = &active.criteria;
    let eval_missing = eval_defect(&criteria.eval, &active.digest, set, paired);
    let counted_report = paired.filter(|_| eval_missing.is_none());
    let mut checks = eval_checks(&criteria.eval, counted_report);

    let windows = windows(state, &active.digest);
    let evidence: Vec<&ShadowRecord> = records
        .iter()
        .filter(|record| criteria.joint.is_some() || record.group.is_none())
        .collect();
    let counted: Vec<ShadowRecord> = evidence
        .iter()
        .filter(|record| {
            windows.iter().any(|window| {
                record.started_at >= window.from && record.started_at < window.to.unwrap_or(now + 1)
            })
        })
        .map(|record| (*record).clone())
        .collect();
    let report = shadow_report::build(&counted, sources);
    checks.extend(shadow_checks("shadow", &criteria.shadow, &report.totals));
    if let Some(joint) = &criteria.joint {
        checks.extend(joint_eval_checks(&joint.eval, counted_report));
        checks.extend(shadow_checks("joint shadow", &joint.shadow, &report.joint));
    }

    let eval_version = counted_report.map(|paired| Version {
        engine: paired.agent.version.engine.clone(),
        model: paired.agent.version.model.clone(),
        skills: paired.agent.version.skills.clone(),
    });
    let shadow_versions: Vec<Version> = report
        .rows
        .iter()
        // A run that never reached the model reports none; it has no version to compare. It
        // still counts among the failed runs.
        .filter(|row| !row.version.model.is_empty())
        .map(|row| Version {
            engine: row.version.engine.clone(),
            model: row.version.model.clone(),
            skills: row.version.skills.clone(),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    checks.push(version_check(eval_version.as_ref(), &shadow_versions));

    Verdict {
        criteria: Some(active.digest.clone()),
        met: checks.iter().all(|check| check.held == Held::Held),
        checks,
        eval_missing,
        shadow: ShadowEvidence {
            windows,
            counted: count(counted.len()),
            left_out: count(evidence.len() - counted.len()),
        },
        eval_version,
        shadow_versions,
    }
}

impl Verdict {
    /// The verdict as text: the evidence used, one line per criterion, and whether the
    /// criteria are met. It says nothing about what follows from that.
    #[must_use]
    pub fn table(&self) -> String {
        let Some(criteria) = &self.criteria else {
            return "no promotion criteria are registered\ncriteria not met\n".to_owned();
        };
        let mut out = format!("criteria {criteria}\n");
        let _ = match &self.eval_missing {
            Some(why) => writeln!(out, "eval evidence: none, {why}"),
            None => writeln!(out, "eval evidence: the paired report"),
        };
        let _ = writeln!(
            out,
            "shadow evidence: {} record(s) from {} window(s) under these criteria, {} left out",
            self.shadow.counted,
            self.shadow.windows.len(),
            self.shadow.left_out
        );
        let _ = writeln!(out);
        for check in &self.checks {
            let mark = match check.held {
                Held::Held => "holds",
                Held::Failed => "FAILS",
                Held::NoEvidence => "NO EVIDENCE",
            };
            let _ = writeln!(
                out,
                "{mark:<12} {}: required {}, observed {}",
                check.name, check.required, check.observed
            );
        }
        let _ = writeln!(out);
        if let Some(version) = &self.eval_version {
            let _ = writeln!(out, "eval run:      {}", describe(version));
        }
        for version in &self.shadow_versions {
            let _ = writeln!(out, "shadow records: {}", describe(version));
        }
        let _ = writeln!(
            out,
            "{}",
            if self.met {
                "criteria met"
            } else {
                "criteria not met"
            }
        );
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod tests {
    use ekg_cognition::eval::{CaseResult, Grade, Narrowing, Report, TrialResult, VersionTuple};

    use super::*;
    use crate::rollout::{RegisterRequest, SetModeRequest, apply_register, apply_set_mode};
    use crate::shadow::{DecisionState, ShadowOutcome};
    use crate::shadow_report::tests::{decided, joint, record};

    pub(crate) const DOCUMENT: &str = "\
version: 1
eval:
  set_version: 2
  min_trials: 3
  min_passed: 3
  min_passed_by_category: { abstain: 1 }
  min_passed_by_profile: { alpha: 2 }
  max_violating_cases: 0
  max_legacy_only_cases: 0
shadow:
  min_compared: 4
  max_agent_failed_percent: 20
  max_legacy_better_percent: 25
  min_promotions: 2
  min_executed_offered_percent: 50
";
    const MINIMAL: &str = "\
version: 1
eval: { set_version: 2, min_trials: 3, min_passed: 14 }
shadow: { min_compared: 20 }
";
    const T0: u64 = 1_790_000_000;

    /// The joint block of a format 2 document.
    const JOINT: &str = "\
joint:
  eval:
    min_passed: 2
    max_violating_cases: 0
    max_legacy_only_cases: 0
  shadow:
    min_compared: 2
    max_agent_failed_percent: 50
    max_legacy_better_percent: 50
    min_promotions: 1
    min_executed_offered_percent: 50
";

    /// [`DOCUMENT`] in format 2 with joint bounds.
    fn joint_document() -> String {
        format!(
            "{}{JOINT}",
            DOCUMENT.replace("version: 1\n", "version: 2\n")
        )
    }

    #[test]
    fn joint_bounds_are_read_in_format_two_and_make_the_criteria_cover_joint_slates() {
        let with = Criteria::parse(&joint_document()).expect("format 2");
        assert!(with.covers_joint());
        let joint = with.joint.as_ref().expect("joint");
        assert_eq!(joint.eval.min_passed, 2);
        assert_eq!(joint.eval.max_legacy_only_cases, Some(0));
        assert_eq!(joint.shadow.min_compared, 2);
        assert_eq!(joint.shadow.min_promotions, Some(1));
        assert_ne!(
            Criteria::digest_of(&joint_document()),
            Criteria::digest_of(&DOCUMENT.replace("version: 1\n", "version: 2\n")),
            "the joint bounds are part of what was agreed"
        );
        // Format 2 without the block is read, and covers nothing more than before.
        let without = DOCUMENT.replace("version: 1\n", "version: 2\n");
        assert!(!Criteria::parse(&without).expect("format 2").covers_joint());
        // Only the two required fields.
        let least = format!(
            "{without}joint: {{ eval: {{ min_passed: 1 }}, shadow: {{ min_compared: 1 }} }}\n"
        );
        let least = Criteria::parse(&least).expect("minimal joint block");
        assert_eq!(least.joint.expect("joint").eval.max_violating_cases, None);
    }

    #[test]
    fn a_format_one_document_reads_as_before_and_keeps_its_digest() {
        let criteria = Criteria::parse(DOCUMENT).expect("format 1");
        assert_eq!(criteria.version, 1);
        assert!(!criteria.covers_joint());
        // The digest is of the text alone, so a registration made before joint bounds existed
        // is still the same registration.
        assert_eq!(
            Criteria::digest_of(DOCUMENT),
            digest(&BTreeMap::from([(
                PROMOTION_FILE.to_owned(),
                DOCUMENT.to_owned()
            )]))
        );
        // Stored criteria are written without the block, and read back as they were.
        let json = serde_json::to_value(&criteria).expect("json");
        assert!(json.get("joint").is_none(), "{json}");
        let back: Criteria = serde_json::from_value(json).expect("stored criteria");
        assert_eq!(back, criteria);
    }

    #[test]
    fn a_joint_block_in_format_one_or_without_a_required_field_or_out_of_range_is_refused() {
        let document = joint_document();
        for (text, field) in [
            (format!("{DOCUMENT}{JOINT}"), "unknown field `joint`"),
            (document.replace("    min_passed: 2\n", ""), "min_passed"),
            (
                document.replace("    min_compared: 2\n", ""),
                "min_compared",
            ),
            (
                document.replace(
                    "    max_agent_failed_percent: 50",
                    "    max_agent_failed_percent: 101",
                ),
                "joint.shadow.max_agent_failed_percent is 101",
            ),
            (
                document.replace("    min_promotions: 1", "    min_promotion: 1"),
                "min_promotion",
            ),
        ] {
            let why = Criteria::parse(&text).expect_err(field);
            assert!(why.contains(field), "{field}: {why}");
        }
    }

    #[test]
    fn a_complete_and_a_minimal_document_are_read() {
        let full = Criteria::parse(DOCUMENT).expect("complete");
        assert_eq!(full.eval.min_passed_by_category["abstain"], 1);
        assert_eq!(full.shadow.min_executed_offered_percent, Some(50));
        let minimal = Criteria::parse(MINIMAL).expect("minimal");
        assert_eq!(minimal.eval.max_violating_cases, None);
        assert!(minimal.eval.min_passed_by_profile.is_empty());
        assert_eq!(minimal.shadow.max_legacy_better_percent, None);
    }

    #[test]
    fn a_document_with_an_unknown_missing_or_impossible_field_is_refused_naming_it() {
        for (document, field) in [
            (
                DOCUMENT.replace("min_promotions", "min_promotion"),
                "min_promotion",
            ),
            (DOCUMENT.replace("  set_version: 2\n", ""), "set_version"),
            (DOCUMENT.replace("  min_trials: 3\n", ""), "min_trials"),
            (DOCUMENT.replace("  min_passed: 3\n", ""), "min_passed"),
            (DOCUMENT.replace("  min_compared: 4\n", ""), "min_compared"),
            (
                DOCUMENT.replace(
                    "max_legacy_better_percent: 25",
                    "max_legacy_better_percent: 101",
                ),
                "max_legacy_better_percent is 101",
            ),
            (
                DOCUMENT.replace("version: 1\n", "version: 3\n"),
                "version is 3",
            ),
            (format!("{DOCUMENT}notes: tuned\n"), "notes"),
        ] {
            let why = Criteria::parse(&document).expect_err(field);
            assert!(why.contains(field), "{field}: {why}");
        }
    }

    const ROLLBACK: &str = "\
rollback:
  max_consecutive_agent_failures: 2
  max_reversed_promotions: 0
";

    #[test]
    fn the_rollback_bounds_are_optional_as_a_section_and_per_field() {
        let without = Criteria::parse(DOCUMENT).expect("without");
        assert_eq!(without.rollback, None);
        assert_eq!(without.rollback(), RollbackCriteria::default());
        // A document without the section serializes as before.
        let json = serde_json::to_value(&without).expect("json");
        assert!(json.get("rollback").is_none(), "{json}");

        let with = Criteria::parse(&format!("{DOCUMENT}{ROLLBACK}")).expect("with");
        assert_eq!(
            with.rollback(),
            RollbackCriteria {
                max_consecutive_agent_failures: Some(2),
                max_reversed_promotions: Some(0),
            }
        );
        let one = format!("{DOCUMENT}rollback: {{ max_reversed_promotions: 1 }}\n");
        let one = Criteria::parse(&one).expect("one field");
        assert_eq!(one.rollback().max_consecutive_agent_failures, None);
        assert_eq!(one.rollback().max_reversed_promotions, Some(1));

        let unknown = format!("{DOCUMENT}rollback: {{ max_aborts: 1 }}\n");
        let why = Criteria::parse(&unknown).expect_err("an unknown bound");
        assert!(why.contains("max_aborts"), "{why}");

        assert_ne!(
            Criteria::digest_of(DOCUMENT),
            Criteria::digest_of(&format!("{DOCUMENT}{ROLLBACK}")),
            "the bounds are part of what was agreed"
        );
    }

    #[test]
    fn the_digest_is_stable_and_any_change_is_another_digest() {
        let digest = Criteria::digest_of(DOCUMENT);
        assert_eq!(digest, Criteria::digest_of(DOCUMENT));
        assert_eq!(digest.len(), 64);
        assert_ne!(
            digest,
            Criteria::digest_of(&DOCUMENT.replace("min_compared: 4", "min_compared: 3"))
        );
    }

    #[test]
    fn the_digest_of_a_directory_is_its_documents_or_none_and_a_broken_one_is_named() {
        let dir = std::env::temp_dir().join(format!("ekg-promotion-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(digest_in(&dir), Ok(None));
        std::fs::write(dir.join(PROMOTION_FILE), DOCUMENT).expect("write");
        assert_eq!(digest_in(&dir), Ok(Some(Criteria::digest_of(DOCUMENT))));
        std::fs::write(dir.join(PROMOTION_FILE), format!("{DOCUMENT}notes: x\n")).expect("write");
        let why = digest_in(&dir).expect_err("broken");
        assert!(why.contains("notes"), "{why}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- the verdict ---

    fn version(arm: &str) -> VersionTuple {
        VersionTuple {
            engine: "0.1.0".to_owned(),
            model: "m/1".to_owned(),
            skills: if arm == "agent" {
                BTreeMap::from([("propose-plan".to_owned(), "v1".to_owned())])
            } else {
                BTreeMap::new()
            },
            tools: BTreeMap::new(),
            set_version: 2,
            set_digest: "set-d".to_owned(),
            trials: 3,
            max_steps: 12,
            joint_max_steps: 8,
            analyst_max_steps: 10,
            planners: 0,
            planner_reads: false,
        }
    }

    /// A case result with one trial per grade.
    fn case(id: &str, category: &str, profile: &str, grades: Vec<Grade>) -> CaseResult {
        CaseResult {
            id: id.to_owned(),
            category: category.to_owned(),
            profile: profile.to_owned(),
            baseline: None,
            trials: grades
                .into_iter()
                .zip(1..)
                .map(|(grade, n)| TrialResult::new(n, "submitted", grade, None, None, None))
                .collect(),
            not_run: 0,
        }
    }

    fn pass() -> Vec<Grade> {
        vec![Grade::Pass; 3]
    }

    fn fail() -> Vec<Grade> {
        vec![Grade::Pass, Grade::Pass, Grade::Fail(vec!["no".to_owned()])]
    }

    const CASES: [(&str, &str, &str); 4] = [
        ("a", "late", "alpha"),
        ("b", "late", "alpha"),
        ("c", "abstain", "alpha"),
        ("d", "abstain", "beta"),
    ];

    /// A paired report of the four cases under `criteria`: each arm's grades in case order.
    fn paired(legacy: [Vec<Grade>; 4], agent: [Vec<Grade>; 4], criteria: &str) -> PairedReport {
        let arm = |name: &str, grades: [Vec<Grade>; 4]| {
            let cases = CASES
                .into_iter()
                .zip(grades)
                .map(|((id, category, profile), grades)| case(id, category, profile, grades))
                .collect();
            Report::new(name, version(name), cases, 0)
        };
        PairedReport::new(arm("legacy", legacy), arm("agent", agent))
            .expect("paired")
            .under(Some(criteria.to_owned()))
    }

    fn all_pass() -> [Vec<Grade>; 4] {
        [pass(), pass(), pass(), pass()]
    }

    fn fail_one() -> [Vec<Grade>; 4] {
        [pass(), fail(), pass(), pass()]
    }

    fn set() -> SetFacts {
        SetFacts {
            digest: "set-d".to_owned(),
            case_ids: CASES.iter().map(|(id, _, _)| (*id).to_owned()).collect(),
        }
    }

    fn register(state: &mut RolloutState, document: &str, at: u64) -> String {
        apply_register(
            state,
            &RegisterRequest {
                document: document.to_owned(),
                by: "ops".to_owned(),
                reason: "before the window".to_owned(),
            },
            at,
        )
        .expect("registered");
        Criteria::digest_of(document)
    }

    fn switch(state: &mut RolloutState, mode: Mode, at: u64) {
        apply_set_mode(
            state,
            &SetModeRequest {
                mode,
                by: "ops".to_owned(),
                reason: "evidence".to_owned(),
            },
            at,
        )
        .expect("switched");
    }

    fn shadow_mode(until: u64) -> Mode {
        Mode::Shadow {
            until,
            max_slates: 100,
            max_tokens: 1_000_000,
        }
    }

    /// A rollout with `DOCUMENT` registered at `T0` and a window open from `T0 + 10`.
    fn rollout() -> (RolloutState, String) {
        let mut state = RolloutState::default();
        let digest = register(&mut state, DOCUMENT, T0);
        switch(&mut state, shadow_mode(T0 + 10_000), T0 + 10);
        (state, digest)
    }

    /// A compared record started at `at`: the agent's best is the same plan (`same`) or ranks
    /// worse than the existing path's.
    fn compared(at: u64, same: bool) -> ShadowRecord {
        let agent: &[(&str, &str)] = if same {
            &[("1~x", "early")]
        } else {
            &[("5~x", "later")]
        };
        let mut record = record(&[("1~a", "early"), ("2~b", "late")], agent);
        record.started_at = at;
        record
    }

    fn promoted(at: u64, executed: &str) -> ShadowRecord {
        decided(compared(at, true), DecisionState::Promoted, &[executed])
    }

    /// Four compared records in the window: two promotions (one of a plan the agent offered),
    /// one where the existing path ranks better.
    fn window_records() -> Vec<ShadowRecord> {
        vec![
            promoted(T0 + 20, "early"),
            promoted(T0 + 21, "late"),
            compared(T0 + 22, true),
            compared(T0 + 23, false),
        ]
    }

    fn judged(
        state: &RolloutState,
        paired: Option<&PairedReport>,
        records: &[ShadowRecord],
    ) -> Verdict {
        verdict(
            state,
            &set(),
            paired,
            records,
            &Sources::default(),
            T0 + 100,
        )
    }

    fn check<'v>(verdict: &'v Verdict, name: &str) -> &'v Check {
        verdict
            .checks
            .iter()
            .find(|check| check.name.contains(name))
            .unwrap_or_else(|| panic!("no check {name:?} in {:#?}", verdict.checks))
    }

    #[test]
    fn evidence_that_satisfies_every_criterion_meets_them() {
        let (state, digest) = rollout();
        let report = paired(all_pass(), all_pass(), &digest);
        let verdict = judged(&state, Some(&report), &window_records());
        assert!(verdict.met, "{}", verdict.table());
        assert!(verdict.checks.iter().all(|check| check.held == Held::Held));
        assert_eq!(verdict.checks.len(), 11);
        assert_eq!(verdict.criteria.as_deref(), Some(digest.as_str()));
        assert_eq!(verdict.eval_missing, None);
        assert_eq!((verdict.shadow.counted, verdict.shadow.left_out), (4, 0));
        let overall = check(&verdict, "eval: cases the agent passes");
        assert_eq!(
            (overall.required.as_str(), overall.observed.as_str()),
            ("at least 3", "4 of 4")
        );
    }

    #[test]
    fn a_report_that_names_its_experiment_runs_is_the_same_evidence() {
        use ekg_cognition::eval::ExperimentRef;

        let (state, digest) = rollout();
        let plain = paired(all_pass(), fail_one(), &digest);
        let mut marked = plain.clone();
        for report in [&mut marked.legacy, &mut marked.agent] {
            report.experiment = Some(ExperimentRef {
                id: format!("20271014T120000Z-{}", report.arm),
                name: format!("v1 {} 20271014T120000Z", report.arm),
            });
        }
        // As the verdict command reads it: from the report's file.
        let json = serde_json::to_string(&marked).expect("json");
        let marked: PairedReport = serde_json::from_str(&json).expect("paired report");
        assert!(marked.agent.experiment.is_some());
        let (without, with) = (
            judged(&state, Some(&plain), &window_records()),
            judged(&state, Some(&marked), &window_records()),
        );
        assert_eq!(with.checks, without.checks);
        assert_eq!(with.met, without.met);
    }

    #[test]
    fn each_eval_criterion_fails_on_its_own_number() {
        let (state, digest) = rollout();
        let records = window_records();
        // The agent fails both abstain cases: overall 2 of 4, abstain 0, alpha 2, and the
        // legacy arm passes two cases alone.
        let report = paired(all_pass(), [pass(), pass(), fail(), fail()], &digest);
        let verdict = judged(&state, Some(&report), &records);
        assert!(!verdict.met);
        assert_eq!(
            check(&verdict, "cases the agent passes at k").held,
            Held::Failed
        );
        let abstain = check(&verdict, "abstain cases");
        assert_eq!(
            (abstain.held, abstain.observed.as_str()),
            (Held::Failed, "0 of 2")
        );
        assert_eq!(check(&verdict, "alpha cases").held, Held::Held);
        let legacy_only = check(&verdict, "only the legacy arm");
        assert_eq!(
            (legacy_only.held, legacy_only.observed.as_str()),
            (Held::Failed, "2")
        );
        assert_eq!(check(&verdict, "violation").held, Held::Held);

        let found = vec![
            Grade::Pass,
            Grade::Pass,
            Grade::Violation(vec!["forbidden".to_owned()]),
        ];
        let report = paired(all_pass(), [found, pass(), pass(), pass()], &digest);
        let violating = judged(&state, Some(&report), &records);
        let violation = check(&violating, "violation");
        assert_eq!(
            (violation.held, violation.observed.as_str()),
            (Held::Failed, "1")
        );
    }

    #[test]
    fn a_slice_the_report_does_not_have_cannot_hold() {
        let mut state = RolloutState::default();
        let document = DOCUMENT.replace("abstain: 1", "injection: 1");
        let digest = register(&mut state, &document, T0);
        let report = paired(all_pass(), all_pass(), &digest);
        let verdict = judged(&state, Some(&report), &[]);
        let missing = check(&verdict, "injection cases");
        assert_eq!(missing.held, Held::Failed);
        assert!(missing.observed.contains("no category"), "{missing:?}");
    }

    #[test]
    fn a_report_that_does_not_count_leaves_every_eval_check_without_evidence() {
        let (state, digest) = rollout();
        let good = || paired(all_pass(), all_pass(), &digest);
        let mut other = good();
        other.criteria = Some("another".to_owned());
        let mut none = good();
        none.criteria = None;
        let mut incomplete = good();
        incomplete.incomplete = vec!["agent: a trial 2 errored: 429".to_owned()];
        let mut old_set = good();
        old_set.agent.version.set_version = 1;
        let mut other_cases = good();
        other_cases.agent.version.set_digest = "set-x".to_owned();
        let mut narrowed = good();
        narrowed.cases.pop();
        narrowed.agent = narrowed.agent.narrowed_to(Narrowing {
            profiles: vec!["alpha".to_owned()],
            ..Narrowing::default()
        });
        let mut few_trials = good();
        few_trials.agent.version.trials = 2;
        for (report, why) in [
            (None, "no paired report"),
            (Some(other), "other criteria (another)"),
            (Some(none), "under no criteria"),
            (Some(incomplete), "incomplete: agent: a trial 2 errored"),
            (Some(old_set), "eval set v1"),
            (Some(other_cases), "other cases (set-x)"),
            (
                Some(narrowed),
                "3 of the set's 4 cases (a, b, c); the run was narrowed to profile alpha",
            ),
            (Some(few_trials), "k=2"),
        ] {
            let verdict = judged(&state, report.as_ref(), &window_records());
            let missing = verdict.eval_missing.as_deref().unwrap_or_default();
            assert!(missing.contains(why), "{why}: {missing}");
            assert!(!verdict.met, "{why}");
            for check in verdict
                .checks
                .iter()
                .filter(|c| c.name.starts_with("eval:"))
            {
                assert_eq!(check.held, Held::NoEvidence, "{why}: {check:?}");
            }
            assert_eq!(check(&verdict, "one version").held, Held::NoEvidence);
            assert_eq!(
                check(&verdict, "compared records").held,
                Held::Held,
                "{why}"
            );
        }
        let named = judged(&state, None, &[]).eval_missing;
        assert_eq!(named.as_deref(), Some("no paired report was given"));
        let both = judged(
            &state,
            Some(&{
                let mut report = good();
                report.criteria = Some("another".to_owned());
                report
            }),
            &[],
        );
        assert!(both.eval_missing.is_some_and(|why| why.contains(&digest)));
    }

    #[test]
    fn only_records_started_inside_a_window_under_the_active_criteria_count() {
        let mut state = RolloutState::default();
        // A window before any registration.
        switch(&mut state, shadow_mode(T0 + 9_000), T0);
        let digest = register(&mut state, DOCUMENT, T0 + 100);
        // Still the window opened without criteria: the registration does not reach it.
        switch(&mut state, Mode::Legacy, T0 + 200);
        // A window under the registration, then closed.
        switch(&mut state, shadow_mode(T0 + 9_000), T0 + 300);
        switch(&mut state, Mode::Legacy, T0 + 400);
        let records = [
            compared(T0 + 50, true),  // before the registration
            compared(T0 + 150, true), // after it, in the window opened without criteria
            compared(T0 + 250, true), // while the mode was legacy
            compared(T0 + 300, true), // at the exact start of the tied window
            compared(T0 + 399, true), // inside it
            compared(T0 + 400, true), // at its exact end
        ];
        let verdict = verdict(
            &state,
            &set(),
            None,
            &records,
            &Sources::default(),
            T0 + 500,
        );
        assert_eq!(
            verdict.shadow.windows,
            [Window {
                from: T0 + 300,
                to: Some(T0 + 400)
            }]
        );
        assert_eq!((verdict.shadow.counted, verdict.shadow.left_out), (2, 4));
        assert_eq!(check(&verdict, "compared records").observed, "2");

        // Other criteria registered later orphan that window too.
        let mut later = state.clone();
        let other = register(&mut later, MINIMAL, T0 + 450);
        assert_ne!(other, digest);
        let orphaned = judged(&later, None, &records);
        assert!(orphaned.shadow.windows.is_empty());
        assert_eq!((orphaned.shadow.counted, orphaned.shadow.left_out), (0, 6));

        // An open window runs until now.
        let (open, _) = rollout();
        let open = judged(
            &open,
            None,
            &[compared(T0 + 100, true), compared(T0 + 5, true)],
        );
        assert_eq!(open.shadow.windows[0].to, None);
        assert_eq!((open.shadow.counted, open.shadow.left_out), (1, 1));
    }

    #[test]
    fn records_of_an_agent_window_are_no_evidence_for_entering_the_mode() {
        let (mut state, digest) = rollout();
        // The window's evidence is signed off, and the mode entered under the same criteria.
        switch(&mut state, Mode::Legacy, T0 + 200);
        let signed = Verdict {
            criteria: Some(digest.clone()),
            met: true,
            checks: Vec::new(),
            eval_missing: None,
            shadow: ShadowEvidence::default(),
            eval_version: Some(Version {
                engine: "0.1.0".to_owned(),
                model: "m/1".to_owned(),
                skills: BTreeMap::new(),
            }),
            shadow_versions: Vec::new(),
        };
        crate::rollout::apply_signoff(
            &mut state,
            &crate::rollout::SignoffRequest {
                verdict: signed,
                report: "report-d".to_owned(),
                by: "qa".to_owned(),
                reason: "the evidence holds".to_owned(),
            },
            T0 + 250,
        )
        .expect("signed");
        let agent = Mode::Agent {
            until: T0 + 10_000,
            max_slates: 100,
            max_tokens: 1_000_000,
        };
        switch(&mut state, agent, T0 + 300);
        assert_eq!(
            state.history[2].criteria.as_deref(),
            Some(digest.as_str()),
            "the agent window is under the active criteria"
        );

        let records = [compared(T0 + 20, true), compared(T0 + 350, true)];
        let verdict = verdict(
            &state,
            &set(),
            None,
            &records,
            &Sources::default(),
            T0 + 500,
        );
        assert_eq!(
            verdict.shadow.windows,
            [Window {
                from: T0 + 10,
                to: Some(T0 + 200)
            }],
            "the shadow window only"
        );
        assert_eq!((verdict.shadow.counted, verdict.shadow.left_out), (1, 1));
    }

    #[test]
    fn the_shadow_percentages_hold_at_their_bound_and_fail_one_over() {
        let (state, _) = rollout();
        // 1 of 4 compared ranks worse (25%, the bound), 1 of 2 promotions was offered (50%).
        let at_bound = judged(&state, None, &window_records());
        for name in [
            "existing path ranks better",
            "agent offered",
            "promotions on the paired",
        ] {
            assert_eq!(check(&at_bound, name).held, Held::Held, "{name}");
        }
        assert_eq!(
            check(&at_bound, "existing path ranks better").observed,
            "1 of 4"
        );

        // 2 of 5 rank worse: 40%, over 25%.
        let mut over = window_records();
        over.push(compared(T0 + 24, false));
        assert_eq!(
            check(&judged(&state, None, &over), "existing path ranks better").held,
            Held::Failed
        );

        // 1 of 5 runs failed is 20%, the bound; 2 of 6 is over it.
        let mut failed = compared(T0 + 25, true);
        failed.outcome = ShadowOutcome::Failed;
        let mut records = window_records();
        records.push(failed.clone());
        let name = "agent run failed";
        assert_eq!(
            check(&judged(&state, None, &records), name).held,
            Held::Held
        );
        records.push(failed);
        let two = judged(&state, None, &records);
        assert_eq!(
            (check(&two, name).held, check(&two, name).observed.as_str()),
            (Held::Failed, "2 of 6")
        );

        // Neither promotion executed a plan the agent offered: 0 of 2, under 50%.
        let unoffered = [
            promoted(T0 + 20, "late"),
            promoted(T0 + 21, "late"),
            compared(T0 + 22, true),
            compared(T0 + 23, true),
        ];
        assert_eq!(
            check(&judged(&state, None, &unoffered), "agent offered").held,
            Held::Failed
        );
    }

    #[test]
    fn a_percentage_is_compared_exactly_not_rounded_down() {
        // 1 of 3 is a third: over 33%, and not yet 34%.
        assert_eq!(percent_check("x", 33, 1, 3, true, true).held, Held::Failed);
        assert_eq!(percent_check("x", 34, 1, 3, true, true).held, Held::Held);
        assert_eq!(percent_check("x", 33, 1, 3, false, true).held, Held::Held);
        assert_eq!(percent_check("x", 34, 1, 3, false, true).held, Held::Failed);
        assert_eq!(percent_check("x", 0, 0, 0, true, true).held, Held::Held);
    }

    #[test]
    fn too_few_compared_records_leave_the_percentages_without_evidence() {
        let (state, _) = rollout();
        let few = [promoted(T0 + 20, "early"), compared(T0 + 21, false)];
        let verdict = judged(&state, None, &few);
        let compared = check(&verdict, "compared records");
        assert_eq!(
            (compared.held, compared.observed.as_str()),
            (Held::Failed, "2")
        );
        for name in [
            "agent run failed",
            "existing path ranks better",
            "agent offered",
        ] {
            assert_eq!(check(&verdict, name).held, Held::NoEvidence, "{name}");
        }
        // A count is a count: one promotion is fewer than two.
        assert_eq!(
            check(&verdict, "promotions on the paired").held,
            Held::Failed
        );
        assert!(
            check(&verdict, "existing path ranks better")
                .observed
                .contains("no evidence (1 of 2)")
        );
    }

    #[test]
    fn evidence_of_more_than_one_version_does_not_meet_the_criteria() {
        let (state, digest) = rollout();
        let report = paired(all_pass(), all_pass(), &digest);
        let one = judged(&state, Some(&report), &window_records());
        assert_eq!(check(&one, "one version").held, Held::Held);
        assert_eq!(one.shadow_versions.len(), 1);

        let mut skill = window_records();
        skill[3].version.skills = BTreeMap::from([("propose-plan".to_owned(), "v2".to_owned())]);
        let two = judged(&state, Some(&report), &skill);
        let versions = check(&two, "one version");
        assert_eq!(versions.held, Held::Failed);
        assert!(
            versions.observed.contains("skill propose-plan: v1, v2"),
            "{versions:?}"
        );
        assert!(!two.met);

        let mut other_model = paired(all_pass(), all_pass(), &digest);
        other_model.agent.version.model = "m/2".to_owned();
        let eval = judged(&state, Some(&other_model), &window_records());
        let versions = check(&eval, "one version");
        assert_eq!(versions.held, Held::Failed);
        assert!(
            versions.observed.contains("model: m/1, m/2"),
            "{versions:?}"
        );

        assert_eq!(
            check(&judged(&state, Some(&report), &[]), "one version").held,
            Held::NoEvidence
        );

        // The eval names every skill on disk, a record the ones its profile loaded: a skill
        // only one side names is no difference.
        let mut more_skills = paired(all_pass(), all_pass(), &digest);
        more_skills
            .agent
            .version
            .skills
            .insert("process-work-centres".to_owned(), "w1".to_owned());
        let wider = judged(&state, Some(&more_skills), &window_records());
        assert_eq!(check(&wider, "one version").held, Held::Held);

        // A run that never reached the model has no version; it is not a second one.
        let mut unreached = window_records();
        unreached[3].version.model = String::new();
        unreached[3].version.skills = BTreeMap::new();
        let verdict = judged(&state, Some(&report), &unreached);
        assert_eq!(check(&verdict, "one version").held, Held::Held);
        assert_eq!(verdict.shadow_versions.len(), 1);
    }

    #[test]
    fn without_a_registration_nothing_is_met_and_the_table_says_so() {
        let verdict = judged(&RolloutState::default(), None, &window_records());
        assert!(!verdict.met);
        assert!(verdict.checks.is_empty());
        assert_eq!(verdict.criteria, None);
        assert!(
            verdict
                .table()
                .contains("no promotion criteria are registered")
        );
        assert!(verdict.table().contains("criteria not met"));
    }

    #[test]
    fn the_table_lists_every_check_and_never_says_what_follows() {
        let (state, digest) = rollout();
        let report = paired(all_pass(), [pass(), pass(), fail(), fail()], &digest);
        let verdict = judged(&state, Some(&report), &window_records()[..2]);
        let table = verdict.table();
        assert!(table.contains(&format!("criteria {digest}")), "{table}");
        assert!(table.contains("FAILS"), "{table}");
        assert!(table.contains("NO EVIDENCE"), "{table}");
        assert!(table.contains("holds"), "{table}");
        assert!(table.contains("2 record(s) from 1 window(s)"), "{table}");
        assert!(table.ends_with("criteria not met\n"), "{table}");
        let met = judged(
            &state,
            Some(&paired(all_pass(), all_pass(), &digest)),
            &window_records(),
        );
        assert!(met.table().ends_with("criteria met\n"));
        for table in [table, met.table()] {
            let lower = table.to_lowercase();
            assert!(
                !lower.contains("promoted") && !lower.contains("may be"),
                "{table}"
            );
        }
        let json = serde_json::to_string(&verdict).expect("json");
        assert_eq!(
            serde_json::from_str::<Verdict>(&json).expect("verdict"),
            verdict
        );
    }

    // --- joint evidence ---

    const JOINT_CASES: [(&str, &str, &str); 6] = [
        ("a", "late", "alpha"),
        ("b", "late", "alpha"),
        ("c", "abstain", "alpha"),
        ("d", "abstain", "beta"),
        ("j1", "joint_plan", "portfolio"),
        ("j2", "joint_plan", "portfolio"),
    ];

    /// A paired report of the six cases, the last two joint, under `criteria`.
    fn joint_paired(
        legacy: [Vec<Grade>; 6],
        agent: [Vec<Grade>; 6],
        criteria: &str,
    ) -> PairedReport {
        let arm = |name: &str, grades: [Vec<Grade>; 6]| {
            let cases = JOINT_CASES
                .into_iter()
                .zip(grades)
                .map(|((id, category, profile), grades)| case(id, category, profile, grades))
                .collect();
            Report::new(name, version(name), cases, 0)
        };
        PairedReport::new(arm("legacy", legacy), arm("agent", agent))
            .expect("paired")
            .under(Some(criteria.to_owned()))
    }

    fn six() -> [Vec<Grade>; 6] {
        [pass(), pass(), pass(), pass(), pass(), pass()]
    }

    /// As [`six`], with the grades of the second joint case replaced.
    fn six_but(last: Vec<Grade>) -> [Vec<Grade>; 6] {
        [pass(), pass(), pass(), pass(), pass(), last]
    }

    /// Two compared joint records in the window: one promoted on a plan the agent offered.
    fn joint_records() -> Vec<ShadowRecord> {
        vec![
            joint(promoted(T0 + 30, "early")),
            joint(compared(T0 + 31, true)),
        ]
    }

    /// A rollout with the joint document registered and a window open, and its digest.
    fn joint_rollout() -> (RolloutState, String) {
        let mut state = RolloutState::default();
        let digest = register(&mut state, &joint_document(), T0);
        switch(&mut state, shadow_mode(T0 + 10_000), T0 + 10);
        (state, digest)
    }

    fn joint_judged(
        state: &RolloutState,
        paired: &PairedReport,
        records: &[ShadowRecord],
    ) -> Verdict {
        let set = SetFacts {
            digest: "set-d".to_owned(),
            case_ids: JOINT_CASES
                .iter()
                .map(|(id, _, _)| (*id).to_owned())
                .collect(),
        };
        verdict(
            state,
            &set,
            Some(paired),
            records,
            &Sources::default(),
            T0 + 100,
        )
    }

    fn with_joint(mut records: Vec<ShadowRecord>) -> Vec<ShadowRecord> {
        records.extend(joint_records());
        records
    }

    #[test]
    fn criteria_with_joint_bounds_are_met_when_point_and_joint_evidence_both_hold() {
        let (state, digest) = joint_rollout();
        let report = joint_paired(six(), six(), &digest);
        let verdict = joint_judged(&state, &report, &with_joint(window_records()));
        assert!(verdict.met, "{}", verdict.table());
        let passed = check(&verdict, "joint eval: joint cases the agent passes at k");
        assert_eq!(
            (passed.required.as_str(), passed.observed.as_str()),
            ("at least 2", "2 of 2")
        );
        let compared = check(&verdict, "joint shadow: compared records");
        assert_eq!(compared.observed, "2");
        // The point bounds count point records only.
        assert_eq!(check(&verdict, "shadow: compared records").observed, "4");
        assert_eq!(verdict.shadow.counted, 6);
    }

    #[test]
    fn each_joint_eval_bound_fails_on_its_own_number() {
        let (state, digest) = joint_rollout();
        let records = with_joint(window_records());
        // The agent passes one of the two joint cases; every bound over all cases holds.
        let one = joint_paired(six(), six_but(fail()), &digest);
        let verdict = joint_judged(&state, &one, &records);
        assert!(!verdict.met);
        let passed = check(&verdict, "joint eval: joint cases the agent passes at k");
        assert_eq!(
            (
                passed.required.as_str(),
                passed.observed.as_str(),
                passed.held
            ),
            ("at least 2", "1 of 2", Held::Failed)
        );
        assert_eq!(
            check(&verdict, "eval: cases the agent passes at k").held,
            Held::Held
        );
        // That case is one only the legacy arm passes.
        let legacy_only = check(
            &verdict,
            "joint eval: joint cases only the legacy arm passes",
        );
        assert_eq!(
            (legacy_only.observed.as_str(), legacy_only.held),
            ("1", Held::Failed)
        );

        // A violation in a joint case.
        let violating = vec![
            Grade::Pass,
            Grade::Pass,
            Grade::Violation(vec!["forbidden text".to_owned()]),
        ];
        let report = joint_paired(six_but(fail()), six_but(violating), &digest);
        let verdict = joint_judged(&state, &report, &records);
        let violation = check(&verdict, "joint eval: joint cases with a violation");
        assert_eq!(
            (violation.observed.as_str(), violation.held),
            ("1", Held::Failed)
        );
    }

    #[test]
    fn too_few_joint_records_fail_their_bound_and_leave_the_percentages_without_evidence() {
        let (state, digest) = joint_rollout();
        let report = joint_paired(six(), six(), &digest);
        let mut records = window_records();
        records.push(joint(promoted(T0 + 30, "early")));
        let verdict = joint_judged(&state, &report, &records);
        assert!(!verdict.met);
        let compared = check(&verdict, "joint shadow: compared records");
        assert_eq!(
            (
                compared.required.as_str(),
                compared.observed.as_str(),
                compared.held
            ),
            ("at least 2", "1", Held::Failed)
        );
        for name in [
            "joint shadow: records whose agent run failed or was aborted",
            "joint shadow: compared records where the existing path ranks better",
            "joint shadow: promotions whose executed plan the agent offered",
        ] {
            assert_eq!(check(&verdict, name).held, Held::NoEvidence, "{name}");
        }
        // The point bounds are untouched by it.
        assert_eq!(check(&verdict, "shadow: compared records").held, Held::Held);
    }

    #[test]
    fn each_joint_shadow_bound_fails_on_its_own_number() {
        let (state, digest) = joint_rollout();
        let report = joint_paired(six(), six(), &digest);
        // The existing path ranks better on both joint records, and nothing was promoted.
        let mut records = window_records();
        records.push(joint(compared(T0 + 30, false)));
        records.push(joint(compared(T0 + 31, false)));
        let verdict = joint_judged(&state, &report, &records);
        let better = check(
            &verdict,
            "joint shadow: compared records where the existing path ranks better",
        );
        assert_eq!(
            (better.observed.as_str(), better.held),
            ("2 of 2", Held::Failed)
        );
        let promotions = check(&verdict, "joint shadow: promotions on the paired slate");
        assert_eq!(
            (promotions.observed.as_str(), promotions.held),
            ("0", Held::Failed)
        );
        assert_eq!(
            check(
                &verdict,
                "shadow: compared records where the existing path ranks better"
            )
            .observed,
            "1 of 4",
            "the point bound counts its own records"
        );

        // A failed joint run is counted over the joint records.
        let mut failed = joint(compared(T0 + 32, true));
        failed.outcome = ShadowOutcome::Failed;
        failed.slate = None;
        let mut records = with_joint(window_records());
        records.extend([failed.clone(), failed.clone(), failed]);
        let verdict = joint_judged(&state, &report, &records);
        let failures = check(
            &verdict,
            "joint shadow: records whose agent run failed or was aborted",
        );
        assert_eq!(
            (failures.observed.as_str(), failures.held),
            ("3 of 5", Held::Failed)
        );
    }

    #[test]
    fn the_point_bounds_do_not_count_joint_records() {
        let (state, digest) = joint_rollout();
        let report = joint_paired(six(), six(), &digest);
        // Three compared point records where four are asked for, and joint ones beside them.
        let mut records: Vec<ShadowRecord> = window_records().into_iter().take(3).collect();
        records.extend(joint_records());
        records.extend(joint_records());
        let verdict = joint_judged(&state, &report, &records);
        let compared = check(&verdict, "shadow: compared records");
        assert_eq!(
            (compared.observed.as_str(), compared.held),
            ("3", Held::Failed)
        );
        assert_eq!(
            check(&verdict, "joint shadow: compared records").observed,
            "4"
        );
    }

    #[test]
    fn criteria_without_joint_bounds_give_the_same_verdict_with_and_without_joint_records() {
        let (state, digest) = rollout();
        let report = paired(all_pass(), all_pass(), &digest);
        let without = judged(&state, Some(&report), &window_records());
        assert!(without.met, "{}", without.table());
        // Joint records of every kind, one of another model: none is evidence here.
        let mut other = joint(compared(T0 + 33, true));
        other.version.model = "m/2".to_owned();
        let mut failed = joint(compared(T0 + 34, true));
        failed.outcome = ShadowOutcome::Failed;
        let mut records = with_joint(window_records());
        records.extend([other, failed]);
        let with = judged(&state, Some(&report), &records);
        assert_eq!(with, without);
        assert!(!with.checks.iter().any(|check| check.name.contains("joint")));
    }
}
