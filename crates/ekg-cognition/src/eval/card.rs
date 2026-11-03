//! The dataset card's link to the set: the card names the set's version and lists every case
//! with its category, and must not fall behind the set.
//!
//! The card is prose. Two things in it are read here: one line `Eval set version: N`, and the
//! rows of its case table, whose first two cells are a case id and its category, each in
//! backticks. A row with only its first cell in backticks (the category table) is not a case.

use std::collections::BTreeMap;

use super::set::EvalSet;

/// The dataset card file of an eval set.
pub const CARD_FILE: &str = "DATASET.md";

const VERSION_LINE: &str = "Eval set version:";

/// The version the card names, if it names one.
fn card_version(card: &str) -> Option<u32> {
    card.lines()
        .find_map(|line| line.trim().strip_prefix(VERSION_LINE))
        .and_then(|rest| rest.trim().trim_matches('*').trim().parse().ok())
}

/// The cases the card lists: id to category, from the table rows whose first two cells are
/// both in backticks.
fn card_cases(card: &str) -> BTreeMap<String, String> {
    card.lines()
        .filter_map(|line| {
            let mut cells = line.trim().strip_prefix('|')?.split('|').map(str::trim);
            let id = cells.next()?.strip_prefix('`')?.strip_suffix('`')?;
            let category = cells.next()?.strip_prefix('`')?.strip_suffix('`')?;
            Some((id.to_owned(), category.to_owned()))
        })
        .collect()
}

/// Where `card` and `set` disagree: the version, a case the card does not list, a case it
/// lists that the set does not have, or a case under another category.
#[must_use]
pub fn card_defects(card: &str, set: &EvalSet) -> Vec<String> {
    let mut defects = Vec::new();
    match card_version(card) {
        Some(version) if version == set.manifest.version => {}
        Some(version) => defects.push(format!(
            "the card names version {version}, the set is version {}",
            set.manifest.version
        )),
        None => defects.push(format!("the card has no `{VERSION_LINE} N` line")),
    }
    let listed = card_cases(card);
    for case in &set.cases {
        match listed.get(&case.id) {
            None => defects.push(format!("the card does not list the case {:?}", case.id)),
            Some(category) if category != &case.category => defects.push(format!(
                "the card lists {:?} under {category:?}, the set under {:?}",
                case.id, case.category
            )),
            Some(_) => {}
        }
    }
    for id in listed.keys() {
        if !set.cases.iter().any(|case| &case.id == id) {
            defects.push(format!(
                "the card lists {id:?}, which the set does not have"
            ));
        }
    }
    for category in &set.manifest.categories {
        if !card.contains(&format!("`{}`", category.name)) {
            defects.push(format!(
                "the card does not describe the category {:?}",
                category.name
            ));
        }
    }
    defects
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::set::tests::{MANIFEST, case};
    use super::*;

    const CARD: &str = "\
# Dataset card

Eval set version: 1

| Category | What it covers |
|---|---|
| `late` | A late line. |
| `abstain` | Nothing helps. |
| `injection` | Hostile text. |

| Case | Category | What it shows |
|---|---|---|
| `a` | `late` | A late line. |
| `b` | `abstain` | Nothing helps. |
";

    fn set(version: u32) -> EvalSet {
        let files = [
            case("a", "late", "expect: { outcome: plan }\n"),
            case("b", "abstain", "expect: { outcome: abstain }\n"),
        ];
        EvalSet::parse(
            &MANIFEST.replace("version: 1", &format!("version: {version}")),
            files.into_iter().collect(),
        )
        .expect("set")
    }

    #[test]
    fn a_card_that_matches_has_no_defect() {
        assert_eq!(card_defects(CARD, &set(1)), Vec::<String>::new());
    }

    #[test]
    fn a_stale_version_names_both() {
        let defects = card_defects(CARD, &set(2));
        assert_eq!(defects, ["the card names version 1, the set is version 2"]);
        let none = card_defects(&CARD.replace("Eval set version: 1", ""), &set(1));
        assert!(none[0].contains("has no"), "{none:?}");
    }

    #[test]
    fn a_missing_an_extra_and_a_misfiled_case_are_each_named() {
        let missing = card_defects(
            &CARD.replace("| `b` | `abstain` | Nothing helps. |\n", ""),
            &set(1),
        );
        assert_eq!(missing, ["the card does not list the case \"b\""]);
        let extra = card_defects(&format!("{CARD}| `z` | `late` | Gone. |\n"), &set(1));
        assert_eq!(extra, ["the card lists \"z\", which the set does not have"]);
        let misfiled = card_defects(
            &CARD.replace("| `b` | `abstain` |", "| `b` | `late` |"),
            &set(1),
        );
        assert_eq!(
            misfiled,
            ["the card lists \"b\" under \"late\", the set under \"abstain\""]
        );
    }

    #[test]
    fn a_category_the_card_does_not_describe_is_named() {
        let defects = card_defects(
            &CARD.replace("| `injection` | Hostile text. |\n", ""),
            &set(1),
        );
        assert_eq!(
            defects,
            ["the card does not describe the category \"injection\""]
        );
    }
}
