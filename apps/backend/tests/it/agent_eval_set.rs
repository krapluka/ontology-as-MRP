//! The eval set's consistency checks. They need no infrastructure and no model, and run in the
//! full gate (`just verify`), so a set that changed without its version, lost the coverage it
//! promises, or outran its dataset card fails the PR.
//!
//! ```text
//! cargo test -p ekg-backend --test it agent_eval_set
//! ```

#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};

use ekg_backend::eval::{EVAL_JOINT_PROFILES, EVAL_PROFILES};
use ekg_backend::promotion::{Criteria, PROMOTION_FILE};
use ekg_cognition::eval::{CARD_FILE, EvalSet, LOCK_FILE, Lock, card_defects};

fn set_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/agent")
}

fn set() -> EvalSet {
    EvalSet::read_dir(&set_dir()).unwrap_or_else(|why| panic!("{why}"))
}

/// The categories the set must have, each with whether every profile needs a case in it,
/// whether its cases must forbid something, and whether its cases are joint cases.
const CATEGORIES: [(&str, bool, bool, bool); 9] = [
    ("late_inbound", false, false, false),
    ("unrecoverable_inbound", false, false, false),
    ("work_centre_contention", false, false, false),
    ("capacity_shortfall", false, false, false),
    ("hidden_facts", false, true, false),
    ("revise", false, false, false),
    ("should_abstain", true, false, false),
    ("prompt_injection", true, true, false),
    ("joint_plan", false, false, true),
];

#[test]
fn the_set_has_its_categories_with_their_rules() {
    let manifest = set().manifest;
    assert!(
        manifest.min_cases_per_category >= 2,
        "a category needs at least two cases, the manifest asks for {}",
        manifest.min_cases_per_category
    );
    let declared: Vec<(&str, bool, bool, bool)> = manifest
        .categories
        .iter()
        .map(|c| (c.name.as_str(), c.every_profile, c.requires_forbid, c.joint))
        .collect();
    assert_eq!(declared, CATEGORIES);
    assert_eq!(manifest.joint_profiles, EVAL_JOINT_PROFILES);
}

#[test]
fn a_joint_case_runs_under_a_profile_composed_of_others() {
    use std::sync::Arc;

    let world: Arc<dyn ekg_domain::WorldModel> =
        Arc::new(ekg_domain::fakes::InMemoryWorldModel::default());
    let profiles = ekg_backend::propose::tool_profiles(&world, 4).expect("profiles");
    for id in EVAL_PROFILES {
        let composed = !profiles.get(id).expect(id).member_ids().is_empty();
        assert_eq!(composed, EVAL_JOINT_PROFILES.contains(&id), "{id}");
    }
    let set = set();
    let joint: Vec<&str> = set
        .cases
        .iter()
        .filter(|case| case.is_joint())
        .map(|case| case.profile.as_str())
        .collect();
    assert!(joint.len() >= 4, "the joint cases: {joint:?}");
    assert!(
        joint
            .iter()
            .all(|profile| EVAL_JOINT_PROFILES.contains(profile))
    );
}

#[test]
fn every_category_and_profile_is_covered() {
    let defects = set().coverage_defects(&EVAL_PROFILES, &EVAL_JOINT_PROFILES);
    assert!(
        defects.is_empty(),
        "the eval set lacks:\n{}",
        defects.join("\n")
    );
}

#[test]
fn the_cases_match_the_lock() {
    let lock = std::fs::read_to_string(set_dir().join(LOCK_FILE)).expect("set.lock");
    if let Err(why) = Lock::parse(&lock).expect("a lock").check(&set()) {
        panic!("{why} (`just agent-eval-lock`)");
    }
}

#[test]
fn the_dataset_card_matches_the_set() {
    let card = std::fs::read_to_string(set_dir().join(CARD_FILE)).expect("DATASET.md");
    let defects = card_defects(&card, &set());
    assert!(
        defects.is_empty(),
        "evals/agent/{CARD_FILE} is behind the set:\n{}",
        defects.join("\n")
    );
}

/// Why `criteria` cannot be met by any run of `set`, if it cannot: another set version, a
/// category or profile the set does not have, or more joint cases than it holds.
fn criteria_defect(criteria: &Criteria, set: &EvalSet) -> Option<String> {
    let joint_cases = set.cases.iter().filter(|case| case.is_joint()).count();
    if let Some(joint) = &criteria.joint
        && usize::try_from(joint.eval.min_passed).is_ok_and(|asked| asked > joint_cases)
    {
        return Some(format!(
            "its joint bounds ask for {} passing joint cases, the set holds {joint_cases}",
            joint.eval.min_passed
        ));
    }
    if criteria.eval.set_version != set.manifest.version {
        return Some(format!(
            "it is for eval set v{}, the set is v{}",
            criteria.eval.set_version, set.manifest.version
        ));
    }
    let unknown = |kind: &str, name: &String| format!("the set has no {kind} {name:?}");
    criteria
        .eval
        .min_passed_by_category
        .keys()
        .find(|name| !set.manifest.categories.iter().any(|c| &&c.name == name))
        .map(|name| unknown("category", name))
        .or_else(|| {
            criteria
                .eval
                .min_passed_by_profile
                .keys()
                .find(|name| !set.cases.iter().any(|case| &&case.profile == name))
                .map(|name| unknown("profile", name))
        })
}

#[test]
fn the_promotion_criteria_are_for_this_set() {
    let text = std::fs::read_to_string(set_dir().join(PROMOTION_FILE)).expect("promotion.yaml");
    let criteria = Criteria::parse(&text).unwrap_or_else(|why| panic!("{why}"));
    let set = set();
    if let Some(why) = criteria_defect(&criteria, &set) {
        panic!("evals/agent/{PROMOTION_FILE}: {why}");
    }
    let cases = u32::try_from(set.cases.len()).expect("a small set");
    assert!(
        criteria.eval.min_passed <= cases,
        "{PROMOTION_FILE} asks for {} passing cases of {cases}",
        criteria.eval.min_passed
    );

    let mut other = criteria.clone();
    other
        .eval
        .min_passed_by_category
        .insert("no_such_category".to_owned(), 1);
    let why = criteria_defect(&other, &set).expect("an unknown category");
    assert!(why.contains("no_such_category"), "{why}");
    // Joint bounds can ask for no more joint cases than the set holds.
    let joint_cases = u32::try_from(set.cases.iter().filter(|case| case.is_joint()).count())
        .expect("a small set");
    let mut greedy = criteria.clone();
    greedy.joint = Some(ekg_backend::promotion::JointCriteria {
        eval: ekg_backend::promotion::JointEvalCriteria {
            min_passed: joint_cases + 1,
            max_violating_cases: None,
            max_legacy_only_cases: None,
        },
        shadow: criteria.shadow.clone(),
    });
    let why = criteria_defect(&greedy, &set).expect("too many joint cases");
    assert!(why.contains("passing joint cases"), "{why}");
    if let Some(joint) = greedy.joint.as_mut() {
        joint.eval.min_passed = joint_cases;
    }
    assert_eq!(criteria_defect(&greedy, &set), None);
    let mut stale = criteria;
    stale.eval.set_version += 1;
    assert!(criteria_defect(&stale, &set).is_some_and(|why| why.contains("eval set v")));
}
