//! The eval set: a manifest, and one file per case.
//!
//! A case is a small fixed world (source records), the subject that drifted in it, the profile
//! the agent reads it through, optionally the plans already tried, and what a good outcome is.
//! Entities are named by source model and id; whoever runs a case turns them into IRIs, so a
//! case does not depend on the deployment's namespace.
//!
//! A joint case names several coupled members instead of one subject, and why they are
//! coupled: one trial plans for all of them at once.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use ekg_domain::{
    ActionType, EntityUri, Plan, ProposedAction, Score, ScoredCandidate, SourceRecord, SourceRef,
    SubjectGroup,
};
use serde::{Deserialize, Serialize};

/// The manifest file of an eval set.
pub const SET_FILE: &str = "set.yaml";
/// The directory holding one file per case.
pub const CASES_DIR: &str = "cases";

/// Why an eval set was refused: where, and each defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetError {
    /// The case id or file the defects are in (`set.yaml` for the manifest).
    pub place: String,
    /// Every defect found, each as one sentence.
    pub defects: Vec<String>,
}

impl SetError {
    fn one(place: &str, defect: impl Into<String>) -> Self {
        Self {
            place: place.to_owned(),
            defects: vec![defect.into()],
        }
    }
}

impl fmt::Display for SetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "eval set, {}: {}", self.place, self.defects.join("; "))
    }
}

impl std::error::Error for SetError {}

/// One category of the set, with what the set must hold for it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    /// The category's name.
    pub name: String,
    /// What the category covers, for the dataset card.
    pub covers: String,
    /// Whether every profile needs a case in it.
    #[serde(default)]
    pub every_profile: bool,
    /// Whether each of its cases must forbid something.
    #[serde(default)]
    pub requires_forbid: bool,
    /// Whether each of its cases must be a joint case.
    #[serde(default)]
    pub joint: bool,
}

/// The manifest: the set's version and its categories.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The set's declared version.
    pub version: u32,
    /// The fewest cases a category may hold.
    pub min_cases_per_category: usize,
    /// The categories a case may be in.
    pub categories: Vec<Category>,
    /// The profiles a joint case runs under: the ones composed of the members' capabilities.
    /// A single-subject case runs under any other.
    #[serde(default)]
    pub joint_profiles: Vec<String>,
}

/// An entity named by its source model and id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityRef {
    /// The source model, e.g. `sale.order`.
    pub model: String,
    /// The source primary key.
    pub id: i64,
}

impl fmt::Display for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.model, self.id)
    }
}

/// One source record of a case's world.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseRecord {
    /// The source model.
    pub model: String,
    /// The source primary key.
    pub id: i64,
    /// Field values by source field name, in the wire form the ingestion layer produces.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

impl CaseRecord {
    /// The record as the world model materializes it, attributed to `system`.
    #[must_use]
    pub fn source_record(&self, system: &str) -> SourceRecord {
        SourceRecord {
            source_ref: SourceRef {
                system: system.to_owned(),
                model: self.model.clone(),
                id: self.id,
            },
            fields: self.fields.clone(),
        }
    }
}

/// One action of a plan a case names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseAction {
    /// The action type.
    pub action_type: String,
    /// The entity it targets.
    pub target: EntityRef,
    /// Its parameters.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

/// A plan already tried, with the rank it earned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriedPlan {
    /// The plan's actions.
    pub actions: Vec<CaseAction>,
    /// The rank key it scored (smaller sorts first).
    pub rank_key: String,
}

/// A plan a case names to prove itself: its reference plan, or one that is futile.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CasePlan {
    /// The plan's actions.
    pub actions: Vec<CaseAction>,
}

/// Whether a case expects a plan or expects the agent to decline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A plan is handed in.
    Plan,
    /// The run ends by itself without a plan.
    Abstain,
}

/// What a scorer metric must be. Numeric bounds read the metric as a number.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bound {
    /// The metric's exact value.
    #[serde(default)]
    pub equals: Option<String>,
    /// The largest value that passes.
    #[serde(default)]
    pub at_most: Option<f64>,
    /// The smallest value that passes.
    #[serde(default)]
    pub at_least: Option<f64>,
}

/// What an accepted plan must not contain.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forbid {
    /// Action types.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Targets.
    #[serde(default)]
    pub targets: Vec<EntityRef>,
    /// Text in a parameter value or the explanation, compared without case.
    #[serde(default)]
    pub text: Vec<String>,
    /// Text in a parameter value only, compared without case. For a value a plan must not
    /// use but may name when it explains what it set aside.
    #[serde(default)]
    pub in_params: Vec<String>,
}

impl Forbid {
    /// Whether nothing is forbidden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
            && self.targets.is_empty()
            && self.text.is_empty()
            && self.in_params.is_empty()
    }
}

/// A case's expectation: the checks a trial is graded with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Whether a plan is expected.
    pub outcome: Outcome,
    /// Whether the plan must rank better than doing nothing. Defaults to true for a plan.
    #[serde(default)]
    pub beats_baseline: Option<bool>,
    /// Bounds on the scorer's metrics, by metric name.
    #[serde(default)]
    pub metrics: BTreeMap<String, Bound>,
    /// Whether the plan must differ from every plan already tried. Defaults to true when the
    /// case lists any.
    #[serde(default)]
    pub differs_from_tried: Option<bool>,
    /// What an accepted plan must not contain.
    #[serde(default)]
    pub forbid: Forbid,
}

/// One case of the set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// The case's id, unique in the set and equal to its file name without the extension.
    pub id: String,
    /// The category it is in.
    pub category: String,
    /// The profile the agent reads it through.
    pub profile: String,
    /// The subject that drifted. A case names this or its `members`.
    #[serde(default)]
    pub subject: Option<EntityRef>,
    /// The coupled subjects a joint case plans for at once, at least two.
    #[serde(default)]
    pub members: Vec<EntityRef>,
    /// Why the members are coupled, one reason each.
    #[serde(default)]
    pub coupling: Vec<String>,
    /// The records of its world.
    pub records: Vec<CaseRecord>,
    /// The plans already tried.
    #[serde(default)]
    pub tried: Vec<TriedPlan>,
    /// What a good outcome is.
    pub expect: Expect,
    /// A plan known to meet the expectation: it shows the case can be solved. Never shown to
    /// what is evaluated. Required when a plan is expected.
    #[serde(default)]
    pub reference: Option<CasePlan>,
    /// Plans that do not improve on doing nothing, however tempting. A case that expects no
    /// plan names at least one.
    #[serde(default)]
    pub futile: Vec<CasePlan>,
    /// Why the expected outcome is the right one.
    pub notes: String,
}

/// What one trial plans for: a single subject, or a group of coupled subjects at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlansFor {
    /// One subject.
    Subject(EntityUri),
    /// The members of a joint case, with why they are coupled.
    Group(SubjectGroup),
}

impl PlansFor {
    /// The subjects a task for this plans for.
    #[must_use]
    pub fn subjects(&self) -> &[EntityUri] {
        match self {
            Self::Subject(subject) => std::slice::from_ref(subject),
            Self::Group(group) => &group.members,
        }
    }
}

/// A case with its entities as IRIs, as a trial is run and graded.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCase {
    /// The subject, or the group of a joint case.
    pub plans_for: PlansFor,
    /// The plans already tried, as the proposer takes them.
    pub tried: Vec<ScoredCandidate>,
    /// The forbidden targets.
    pub forbid_targets: Vec<EntityUri>,
    /// The expectation.
    pub expect: Expect,
    /// The reference plan, if the case has one.
    pub reference: Option<Plan>,
    /// The futile plans.
    pub futile: Vec<Plan>,
}

impl Case {
    /// Whether the case plans for several coupled members at once.
    #[must_use]
    pub fn is_joint(&self) -> bool {
        self.subject.is_none() && !self.members.is_empty()
    }

    /// The case with its entities turned into IRIs by `iri` (model and id to IRI).
    #[must_use]
    pub fn resolve(&self, iri: &dyn Fn(&str, i64) -> String) -> ResolvedCase {
        let entity = |entity: &EntityRef| EntityUri(iri(&entity.model, entity.id));
        let plan = |actions: &[CaseAction]| Plan {
            actions: actions
                .iter()
                .map(|action| ProposedAction {
                    action_type: ActionType(action.action_type.clone()),
                    target: entity(&action.target),
                    params: action.params.clone(),
                })
                .collect(),
        };
        let plans_for = match &self.subject {
            Some(subject) => PlansFor::Subject(entity(subject)),
            // The group is the case's own, so a case does not depend on any grouping rule.
            None => PlansFor::Group(SubjectGroup {
                key: format!("case/{}", self.id),
                members: self.members.iter().map(entity).collect(),
                links: self.coupling.clone(),
                worded: false,
            }),
        };
        ResolvedCase {
            plans_for,
            tried: self
                .tried
                .iter()
                .map(|tried| ScoredCandidate {
                    plan: plan(&tried.actions),
                    score: Score {
                        metrics: BTreeMap::new(),
                        rank_key: tried.rank_key.clone(),
                    },
                })
                .collect(),
            forbid_targets: self.expect.forbid.targets.iter().map(entity).collect(),
            expect: self.expect.clone(),
            reference: self.reference.as_ref().map(|r| plan(&r.actions)),
            futile: self.futile.iter().map(|f| plan(&f.actions)).collect(),
        }
    }

    /// The defects of what the case plans for: a subject, or members with their coupling.
    fn plans_for_defects(&self, joint_profiles: &[String]) -> Vec<String> {
        let mut defects = Vec::new();
        let recorded = |entity: &EntityRef| {
            self.records
                .iter()
                .any(|record| record.model == entity.model && record.id == entity.id)
        };
        let joint_profile = joint_profiles.contains(&self.profile);
        match (&self.subject, self.members.as_slice()) {
            (Some(_), [_, ..]) => {
                defects.push("it names both a subject and members".to_owned());
            }
            (None, []) => defects.push("it names neither a subject nor members".to_owned()),
            (Some(subject), []) => {
                if !recorded(subject) {
                    defects.push(format!("its subject {subject} is not among its records"));
                }
                if !self.coupling.is_empty() {
                    defects.push("a case with one subject has no coupling".to_owned());
                }
                if joint_profile {
                    defects.push(format!(
                        "its profile {:?} is for joint cases, and it names one subject",
                        self.profile
                    ));
                }
            }
            (None, members) => {
                if members.len() < 2 {
                    defects.push("a joint case needs at least two members".to_owned());
                }
                let mut seen = BTreeSet::new();
                for member in members {
                    if !seen.insert(member) {
                        defects.push(format!("member {member} is named twice"));
                    } else if !recorded(member) {
                        defects.push(format!("its member {member} is not among its records"));
                    }
                }
                if self.coupling.iter().all(|reason| reason.trim().is_empty()) {
                    defects.push("its members have no coupling".to_owned());
                }
                if !joint_profile {
                    defects.push(format!(
                        "its profile {:?} is one capability's, and it names members",
                        self.profile
                    ));
                }
            }
        }
        defects
    }

    /// The defects of this case against the set's `categories` and `joint_profiles`.
    fn defects(
        &self,
        categories: &BTreeMap<&str, &Category>,
        joint_profiles: &[String],
    ) -> Vec<String> {
        let mut defects = Vec::new();
        match categories.get(self.category.as_str()) {
            None => defects.push(format!("its category {:?} is not known", self.category)),
            Some(category) => {
                if category.requires_forbid && self.expect.forbid.is_empty() {
                    defects.push(format!("a {:?} case must forbid something", self.category));
                }
                if category.joint && self.subject.is_some() {
                    defects.push(format!(
                        "a {:?} case names members, not one subject",
                        self.category
                    ));
                }
            }
        }
        defects.extend(self.plans_for_defects(joint_profiles));
        let mut seen = BTreeSet::new();
        for record in &self.records {
            if !seen.insert((record.model.as_str(), record.id)) {
                defects.push(format!(
                    "record {}/{} is listed twice",
                    record.model, record.id
                ));
            }
        }
        if self.expect.outcome == Outcome::Abstain
            && (self.expect.beats_baseline.is_some()
                || !self.expect.metrics.is_empty()
                || self.expect.differs_from_tried.is_some())
        {
            defects.push("a case that expects no plan cannot check a plan's outcome".to_owned());
        }
        match (self.expect.outcome, &self.reference) {
            (Outcome::Plan, None) => {
                defects.push("a case that expects a plan needs a reference plan".to_owned());
            }
            (Outcome::Abstain, Some(_)) => {
                defects.push("a case that expects no plan cannot have a reference plan".to_owned());
            }
            (Outcome::Abstain, None) if self.futile.is_empty() => {
                defects
                    .push("a case that expects no plan names at least one futile plan".to_owned());
            }
            _ => {}
        }
        let plans = self.reference.iter().chain(&self.futile);
        if plans.into_iter().any(|plan| plan.actions.is_empty()) {
            defects.push("a reference or futile plan has no action".to_owned());
        }
        for (metric, bound) in &self.expect.metrics {
            if *bound == Bound::default() {
                defects.push(format!("metric {metric:?} has no bound"));
            }
        }
        if self.notes.trim().is_empty() {
            defects.push("it has no notes".to_owned());
        }
        defects
    }
}

/// A loaded eval set: its manifest and its cases, by id.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalSet {
    /// The manifest.
    pub manifest: Manifest,
    /// The cases, ordered by id.
    pub cases: Vec<Case>,
    /// The case files (path under the set directory to content): what the digest is over.
    pub files: BTreeMap<String, String>,
}

impl EvalSet {
    /// Parse a set from its manifest text and its case files (path to content).
    ///
    /// # Errors
    /// [`SetError`] naming the manifest or the case and the defect: an unparseable file, an
    /// unknown field or check, a case whose id is not its file name, an unknown category, a
    /// subject or member outside its records, a case that names both a subject and members or
    /// neither, a profile that does not fit them, or a duplicate record.
    pub fn parse(manifest: &str, files: BTreeMap<String, String>) -> Result<Self, SetError> {
        let manifest: Manifest = serde_saphyr::from_str(manifest)
            .map_err(|why| SetError::one(SET_FILE, why.to_string()))?;
        let mut names = BTreeSet::new();
        for category in &manifest.categories {
            if !names.insert(category.name.as_str()) {
                return Err(SetError::one(
                    SET_FILE,
                    format!("category {:?} is listed twice", category.name),
                ));
            }
        }
        let categories: BTreeMap<&str, &Category> = manifest
            .categories
            .iter()
            .map(|category| (category.name.as_str(), category))
            .collect();
        let mut cases = Vec::with_capacity(files.len());
        for (path, content) in &files {
            let case: Case = serde_saphyr::from_str(content)
                .map_err(|why| SetError::one(path, why.to_string()))?;
            let stem = Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if case.id != stem {
                return Err(SetError::one(
                    path,
                    format!("its id {:?} differs from its file name", case.id),
                ));
            }
            let defects = case.defects(&categories, &manifest.joint_profiles);
            if !defects.is_empty() {
                return Err(SetError {
                    place: case.id,
                    defects,
                });
            }
            cases.push(case);
        }
        cases.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            manifest,
            cases,
            files,
        })
    }

    /// Read and parse the set directory `dir`: its `set.yaml` and every `cases/*.yaml`.
    ///
    /// # Errors
    /// [`SetError`] for an unreadable file or any [`Self::parse`] defect.
    pub fn read_dir(dir: &Path) -> Result<Self, SetError> {
        let read = |path: &Path, place: &str| {
            std::fs::read_to_string(path).map_err(|why| SetError::one(place, why.to_string()))
        };
        let manifest = read(&dir.join(SET_FILE), SET_FILE)?;
        let cases_dir = dir.join(CASES_DIR);
        let entries = std::fs::read_dir(&cases_dir)
            .map_err(|why| SetError::one(CASES_DIR, why.to_string()))?;
        let mut files = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|why| SetError::one(CASES_DIR, why.to_string()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Editor and OS litter is not part of the set.
            let is_case = Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml"));
            if name.starts_with('.') || !is_case {
                continue;
            }
            let path = format!("{CASES_DIR}/{name}");
            let content = read(&entry.path(), &path)?;
            files.insert(path, content);
        }
        Self::parse(&manifest, files)
    }

    /// What the set lacks for `profiles`, of which `composed` are composed of others: a
    /// category below the manifest's minimum, a profile without a case in a category every
    /// profile needs, a case under an unknown profile, or a manifest whose joint profiles are
    /// not the composed ones.
    #[must_use]
    pub fn coverage_defects(&self, profiles: &[&str], composed: &[&str]) -> Vec<String> {
        let mut defects = Vec::new();
        for profile in profiles {
            let joint = self.manifest.joint_profiles.iter().any(|id| id == profile);
            match (joint, composed.contains(profile)) {
                (true, false) => defects.push(format!(
                    "profile {profile:?} is listed for joint cases and is one capability's"
                )),
                (false, true) => defects.push(format!(
                    "profile {profile:?} is composed of others and is not listed for joint cases"
                )),
                _ => {}
            }
        }
        for case in &self.cases {
            if !profiles.contains(&case.profile.as_str()) {
                defects.push(format!(
                    "case {:?} runs under the unknown profile {:?}",
                    case.id, case.profile
                ));
            }
        }
        let min = self.manifest.min_cases_per_category;
        for category in &self.manifest.categories {
            let cases: Vec<&Case> = self
                .cases
                .iter()
                .filter(|case| case.category == category.name)
                .collect();
            if cases.len() < min {
                defects.push(format!(
                    "category {:?} has {} of the {min} cases it needs",
                    category.name,
                    cases.len()
                ));
            }
            if category.every_profile {
                for profile in profiles {
                    if !cases.iter().any(|case| case.profile == *profile) {
                        defects.push(format!(
                            "profile {profile:?} has no {:?} case",
                            category.name
                        ));
                    }
                }
            }
        }
        defects
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const MANIFEST: &str = "\
version: 1
min_cases_per_category: 1
categories:
  - { name: late, covers: A late line. }
  - { name: abstain, covers: Nothing helps., every_profile: true }
  - { name: injection, covers: Hostile text., requires_forbid: true }
";

    /// A case file. A tail that expects a plan gets a reference plan, and one that expects none
    /// a futile plan, unless the tail brings its own or says `no proof`.
    pub(crate) fn case(id: &str, category: &str, tail: &str) -> (String, String) {
        let proof =
            if tail.contains("reference:") || tail.contains("futile:") || tail.contains("no proof")
            {
                ""
            } else if tail.contains("outcome: abstain") {
                FUTILE
            } else {
                REFERENCE
            };
        let tail = tail.replace("# no proof\n", "");
        (
            format!("cases/{id}.yaml"),
            format!(
                "\
id: {id}
category: {category}
profile: alpha
subject: {{ model: sale.order, id: 1 }}
records:
  - {{ model: sale.order, id: 1, fields: {{ name: S1 }} }}
  - {{ model: purchase.order.line, id: 7 }}
notes: Why.
{tail}{proof}"
            ),
        )
    }

    /// [`MANIFEST`] with a category of joint cases and the profile they run under.
    pub(crate) const JOINT_MANIFEST: &str = "\
version: 1
min_cases_per_category: 1
joint_profiles: [both]
categories:
  - { name: late, covers: A late line. }
  - { name: abstain, covers: Nothing helps., every_profile: true }
  - { name: injection, covers: Hostile text., requires_forbid: true }
  - { name: joint, covers: One plan for coupled members., joint: true }
";

    /// A joint case file: a sales order and the manufacturing order that builds it, under
    /// the profile `both`. Its proof is chosen as [`case`] chooses one.
    pub(crate) fn joint_case(id: &str, category: &str, tail: &str) -> (String, String) {
        let (path, content) = case(id, category, tail);
        let content = content
            .replace("profile: alpha", "profile: both")
            .replace(
                "subject: { model: sale.order, id: 1 }",
                "members:\n  - { model: sale.order, id: 1 }\n  - { model: mrp.production, id: 2 }\ncoupling: [MO/2 builds S1]",
            )
            .replace(
                "  - { model: purchase.order.line, id: 7 }",
                "  - { model: purchase.order.line, id: 7 }\n  - { model: mrp.production, id: 2 }",
            );
        (path, content)
    }

    /// A reference plan, as every case that expects a plan carries one.
    pub(crate) const REFERENCE: &str = "\
reference:
  actions:
    - { action_type: move, target: { model: purchase.order.line, id: 7 }, params: { to: y } }
";
    /// A futile plan, as every case that expects no plan carries one.
    pub(crate) const FUTILE: &str = "\
futile:
  - actions:
      - { action_type: nudge, target: { model: sale.order, id: 1 } }
";

    const PLAN: &str = "expect: { outcome: plan }\n";

    fn refusal(files: &[(String, String)]) -> SetError {
        EvalSet::parse(MANIFEST, files.iter().cloned().collect()).expect_err("refused")
    }

    #[test]
    fn a_valid_set_loads_with_its_cases_in_id_order() {
        let files = [
            case("b", "late", PLAN),
            case("a", "abstain", "expect: { outcome: abstain }\n"),
        ];
        let set = EvalSet::parse(MANIFEST, files.into_iter().collect()).expect("set");
        assert_eq!(set.manifest.version, 1);
        let ids: Vec<&str> = set.cases.iter().map(|case| case.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(set.cases[1].records[0].fields["name"], "S1");
    }

    #[test]
    fn an_unknown_category_is_refused_naming_the_case() {
        let error = refusal(&[case("a", "nonsense", PLAN)]);
        assert_eq!(error.place, "a");
        assert!(error.defects[0].contains("\"nonsense\""), "{error}");
    }

    #[test]
    fn a_subject_outside_the_records_is_refused() {
        let (path, content) = case("a", "late", PLAN);
        let content = content.replace(
            "subject: { model: sale.order, id: 1 }",
            "subject: { model: sale.order, id: 9 }",
        );
        let error = refusal(&[(path, content)]);
        assert!(error.defects[0].contains("sale.order/9"), "{error}");
    }

    fn joint_refusal(files: &[(String, String)]) -> SetError {
        EvalSet::parse(JOINT_MANIFEST, files.iter().cloned().collect()).expect_err("refused")
    }

    #[test]
    fn a_joint_case_loads_and_resolves_to_its_group() {
        let files = [joint_case("a", "joint", PLAN), case("b", "late", PLAN)];
        let set = EvalSet::parse(JOINT_MANIFEST, files.into_iter().collect()).expect("set");
        let joint = &set.cases[0];
        assert!(joint.is_joint() && !set.cases[1].is_joint());
        let resolved = joint.resolve(&|model, id| format!("x:{model}/{id}"));
        let PlansFor::Group(group) = &resolved.plans_for else {
            panic!("a joint case plans for a group");
        };
        assert_eq!(group.key, "case/a");
        assert_eq!(
            group.members,
            [
                EntityUri("x:sale.order/1".to_owned()),
                EntityUri("x:mrp.production/2".to_owned())
            ]
        );
        assert_eq!(group.links, ["MO/2 builds S1"]);
        assert_eq!(resolved.plans_for.subjects(), group.members);
    }

    #[test]
    fn a_case_with_a_subject_and_members_or_with_neither_is_refused() {
        let (path, content) = joint_case("a", "joint", PLAN);
        let both = content.replace(
            "members:",
            "subject: { model: sale.order, id: 1 }\nmembers:",
        );
        let error = joint_refusal(&[(path.clone(), both)]);
        assert_eq!(error.place, "a");
        assert!(
            error
                .defects
                .iter()
                .any(|d| d.contains("both a subject and members")),
            "{error}"
        );
        let (path, content) = case("a", "late", PLAN);
        let neither = content.replace("subject: { model: sale.order, id: 1 }\n", "");
        let error = joint_refusal(&[(path, neither)]);
        assert!(
            error.defects[0].contains("neither a subject nor members"),
            "{error}"
        );
    }

    #[test]
    fn each_defect_of_a_joint_cases_members_is_named() {
        let (path, content) = joint_case("a", "joint", PLAN);
        let second = "\n  - { model: mrp.production, id: 2 }\ncoupling";
        for (replace, with, defect) in [
            (second, "\ncoupling", "at least two members"),
            (
                second,
                "\n  - { model: sale.order, id: 1 }\ncoupling",
                "member sale.order/1 is named twice",
            ),
            (
                second,
                "\n  - { model: mrp.production, id: 9 }\ncoupling",
                "member mrp.production/9 is not among its records",
            ),
            (
                "coupling: [MO/2 builds S1]",
                "coupling: []",
                "its members have no coupling",
            ),
        ] {
            assert!(content.contains(replace), "{replace}");
            let error = joint_refusal(&[(path.clone(), content.replace(replace, with))]);
            assert_eq!(error.place, "a");
            assert!(
                error.defects.iter().any(|d| d.contains(defect)),
                "{defect}: {error}"
            );
        }
    }

    #[test]
    fn a_profile_that_does_not_fit_what_the_case_names_is_refused() {
        // Members under one capability's profile.
        let (path, content) = joint_case("a", "joint", PLAN);
        let error = joint_refusal(&[(path, content.replace("profile: both", "profile: alpha"))]);
        assert!(
            error
                .defects
                .iter()
                .any(|d| d.contains("\"alpha\" is one capability's")),
            "{error}"
        );
        // One subject under the joint profile.
        let (path, content) = case("a", "late", PLAN);
        let error = joint_refusal(&[(path, content.replace("profile: alpha", "profile: both"))]);
        assert!(
            error
                .defects
                .iter()
                .any(|d| d.contains("\"both\" is for joint cases")),
            "{error}"
        );
        // One subject may not carry a coupling.
        let (path, content) = case("a", "late", PLAN);
        let error = joint_refusal(&[(path, format!("{content}coupling: [x]\n"))]);
        assert!(error.defects[0].contains("has no coupling"), "{error}");
    }

    #[test]
    fn a_single_subject_case_in_a_joint_category_is_refused_naming_the_case() {
        let error = joint_refusal(&[case("solo", "joint", PLAN)]);
        assert_eq!(error.place, "solo");
        assert!(
            error.defects[0].contains("\"joint\" case names members"),
            "{error}"
        );
    }

    #[test]
    fn coverage_asks_every_profile_for_its_own_case_the_joint_profile_included() {
        let abstain = "expect: { outcome: abstain }\n";
        let files = [
            joint_case("j", "joint", PLAN),
            case("a", "abstain", abstain),
            case("l", "late", PLAN),
            case(
                "i",
                "injection",
                "expect: { outcome: plan, forbid: { actions: [x] } }\n",
            ),
        ];
        let set = EvalSet::parse(JOINT_MANIFEST, files.into_iter().collect()).expect("set");
        assert_eq!(
            set.coverage_defects(&["alpha", "both"], &["both"]),
            ["profile \"both\" has no \"abstain\" case"]
        );
        // The manifest's joint profiles are the composed ones, both ways.
        let defects = set.coverage_defects(&["alpha", "both"], &["alpha"]);
        assert!(
            defects.contains(
                &"profile \"both\" is listed for joint cases and is one capability's".to_owned()
            ) && defects.contains(
                &"profile \"alpha\" is composed of others and is not listed for joint cases"
                    .to_owned()
            ),
            "{defects:#?}"
        );
    }

    #[test]
    fn an_unknown_check_is_refused() {
        let error = refusal(&[case(
            "a",
            "late",
            "expect: { outcome: plan, is_nice: true }\n",
        )]);
        assert_eq!(error.place, "cases/a.yaml");
        assert!(error.defects[0].contains("is_nice"), "{error}");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let error = refusal(&[case("a", "late", &format!("{PLAN}owner: me\n"))]);
        assert!(error.defects[0].contains("owner"), "{error}");
    }

    #[test]
    fn an_id_that_is_not_the_file_name_is_refused() {
        let (_, content) = case("a", "late", PLAN);
        let error = refusal(&[("cases/b.yaml".to_owned(), content)]);
        assert!(error.defects[0].contains("file name"), "{error}");
    }

    #[test]
    fn a_duplicate_record_is_refused() {
        let (path, content) = case("a", "late", PLAN);
        let content = content.replace("purchase.order.line, id: 7", "sale.order, id: 1");
        let error = refusal(&[(path, content)]);
        assert!(error.defects[0].contains("listed twice"), "{error}");
    }

    #[test]
    fn a_category_that_requires_forbid_refuses_a_case_without_one() {
        let error = refusal(&[case("a", "injection", PLAN)]);
        assert!(error.defects[0].contains("must forbid"), "{error}");
        let ok = case(
            "a",
            "injection",
            "expect: { outcome: plan, forbid: { text: [canary] } }\n",
        );
        EvalSet::parse(MANIFEST, [ok].into_iter().collect()).expect("set");
    }

    #[test]
    fn an_abstain_case_cannot_check_a_plan() {
        let error = refusal(&[case(
            "a",
            "abstain",
            "expect: { outcome: abstain, beats_baseline: true }\n",
        )]);
        assert!(error.defects[0].contains("expects no plan"), "{error}");
    }

    #[test]
    fn a_case_proves_itself_with_a_reference_or_a_futile_plan() {
        let error = refusal(&[case("a", "late", "# no proof\nexpect: { outcome: plan }\n")]);
        assert!(
            error.defects[0].contains("needs a reference plan"),
            "{error}"
        );
        let error = refusal(&[case(
            "a",
            "abstain",
            "# no proof\nexpect: { outcome: abstain }\n",
        )]);
        assert!(
            error.defects[0].contains("at least one futile plan"),
            "{error}"
        );
        let error = refusal(&[case(
            "a",
            "abstain",
            &format!("expect: {{ outcome: abstain }}\n{REFERENCE}"),
        )]);
        assert!(
            error.defects[0].contains("cannot have a reference plan"),
            "{error}"
        );
        let error = refusal(&[case(
            "a",
            "late",
            "expect: { outcome: plan }\nreference: { actions: [] }\n",
        )]);
        assert!(error.defects[0].contains("has no action"), "{error}");
    }

    #[test]
    fn coverage_names_a_thin_category_and_a_profile_without_a_case() {
        let files = [case("a", "late", PLAN)];
        let set = EvalSet::parse(MANIFEST, files.into_iter().collect()).expect("set");
        let defects = set.coverage_defects(&["alpha", "beta"], &[]);
        assert!(
            defects
                .iter()
                .any(|d| d.contains("\"abstain\" has 0 of the 1")),
            "{defects:?}"
        );
        assert!(
            defects
                .iter()
                .any(|d| d.contains("\"beta\" has no \"abstain\"")),
            "{defects:?}"
        );
        assert!(
            defects.iter().any(|d| d.contains("\"injection\" has 0")),
            "{defects:?}"
        );
        let defects = set.coverage_defects(&["beta"], &[]);
        assert!(
            defects
                .iter()
                .any(|d| d.contains("unknown profile \"alpha\"")),
            "{defects:?}"
        );
    }

    #[test]
    fn resolving_turns_entities_into_iris() {
        let tail = "\
tried:
  - actions:
      - { action_type: move, target: { model: purchase.order.line, id: 7 }, params: { to: x } }
    rank_key: \"0002\"
expect: { outcome: plan, forbid: { targets: [{ model: res.partner, id: 3 }] } }
";
        let set =
            EvalSet::parse(MANIFEST, [case("a", "late", tail)].into_iter().collect()).expect("set");
        let resolved = set.cases[0].resolve(&|model, id| format!("x:{model}/{id}"));
        assert_eq!(
            resolved.plans_for,
            PlansFor::Subject(EntityUri("x:sale.order/1".to_owned()))
        );
        assert_eq!(
            resolved.tried[0].plan.actions[0].target.0,
            "x:purchase.order.line/7"
        );
        assert_eq!(resolved.tried[0].score.rank_key, "0002");
        assert_eq!(resolved.forbid_targets[0].0, "x:res.partner/3");
        let reference = resolved.reference.expect("the reference plan");
        assert_eq!(reference.actions[0].target.0, "x:purchase.order.line/7");
        assert!(resolved.futile.is_empty());
    }
}
