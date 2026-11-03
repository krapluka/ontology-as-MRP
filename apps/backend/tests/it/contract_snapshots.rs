//! Contract-snapshot drift check.
//!
//! The frontend's typed clients are generated from two committed snapshots,
//! `apps/frontend/schema.graphql` (the read surface) and `apps/frontend/openapi.json` (the
//! command surface), and its tab fixtures are checked against a third,
//! `apps/frontend/tab-rows.json` (the read-model tab row vocabulary). All are exported from
//! this crate's code by `just frontend-contracts`. This test fails whenever a committed
//! snapshot differs from what the code generates, so a contract change cannot ship with a
//! stale snapshot. (That the typed clients match the snapshots is checked separately by
//! `just frontend-check`, which regenerates them offline and fails on any diff.) No
//! backend runs.
//!
//! ```text
//! cargo test -p ekg-backend --test it contract_snapshots
//! ```

#![allow(clippy::expect_used)]

use std::path::PathBuf;

use ekg_backend::graphql;
use ekg_backend::openapi::ApiDoc;
use utoipa::OpenApi;

const REGENERATE: &str = "run `just frontend-contracts` to regenerate the snapshots from code";

fn snapshot(name: &str) -> (PathBuf, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../frontend")
        .join(name);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "read committed snapshot {}: {err}; {REGENERATE}",
            path.display()
        )
    });
    // Report `apps/frontend/...`, not `apps/backend/../frontend/...`.
    let path = path.canonicalize().unwrap_or(path);
    (path, content)
}

/// Line endings and trailing whitespace at the end of the file are formatting, not
/// contract. Normalise both sides so only a real schema difference fails.
fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_owned()
}

#[test]
fn the_committed_graphql_snapshot_matches_the_read_schema() {
    let (path, committed) = snapshot("schema.graphql");
    let committed = normalise(&committed);
    let generated = normalise(&graphql::sdl());
    if committed != generated {
        let first_diff = committed
            .lines()
            .zip(generated.lines())
            .position(|(c, g)| c != g)
            .map_or_else(
                || {
                    "one side is a prefix of the other (lines added or removed at the end)"
                        .to_owned()
                },
                |i| {
                    format!(
                        "first difference at line {}:\n  committed: {}\n  generated: {}",
                        i + 1,
                        committed.lines().nth(i).unwrap_or_default(),
                        generated.lines().nth(i).unwrap_or_default()
                    )
                },
            );
        panic!(
            "{} has drifted from the read schema; {REGENERATE}.\n{first_diff}",
            path.display()
        );
    }
}

#[test]
fn the_committed_openapi_snapshot_matches_the_command_surface() {
    let (path, committed) = snapshot("openapi.json");
    // Compared as parsed JSON, so key order and indentation cannot cause a false failure.
    let committed: serde_json::Value = serde_json::from_str(&committed)
        .unwrap_or_else(|err| panic!("{} is not valid JSON ({err}); {REGENERATE}", path.display()));
    let generated =
        serde_json::to_value(ApiDoc::openapi()).expect("serialize the OpenAPI document");
    assert!(
        committed == generated,
        "{} has drifted from the OpenAPI document the code generates; {REGENERATE}",
        path.display()
    );
}

#[test]
fn the_committed_tab_row_vocabulary_matches_the_tab_queries() {
    let (path, committed) = snapshot("tab-rows.json");
    // Compared as parsed JSON, so formatting cannot cause a false failure.
    let committed: serde_json::Value = serde_json::from_str(&committed)
        .unwrap_or_else(|err| panic!("{} is not valid JSON ({err}); {REGENERATE}", path.display()));
    let generated = serde_json::to_value(ekg_semantic::tabs::row_vocabulary())
        .expect("serialize the tab row vocabulary");
    assert!(
        committed == generated,
        "{} has drifted from the tab row vocabulary the tab queries serve; {REGENERATE}",
        path.display()
    );
}

/// The frontend words every governed verb by a fixed label map
/// (`apps/frontend/src/viewmodel/verbLabels.json`). Every code the command surface defines
/// must have a label there, so no raw code reaches the operator when a verb is added; a label
/// for a code the surface does not define is stale.
#[test]
fn the_frontend_verb_labels_cover_every_governed_verb() {
    let (path, committed) = snapshot("src/viewmodel/verbLabels.json");
    let labels: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&committed).expect("the verb-label map is a JSON object");
    let codes: Vec<&str> = ekg_kinetic::Verb::ALL.iter().map(|v| v.name()).collect();
    for code in &codes {
        let label = labels.get(*code).and_then(serde_json::Value::as_str);
        assert!(
            label.is_some_and(|l| !l.trim().is_empty()),
            "{} has no label for the governed verb `{code}`: add one",
            path.display()
        );
    }
    for code in labels.keys() {
        assert!(
            codes.contains(&code.as_str()),
            "{} labels `{code}`, which is not a governed verb: remove it",
            path.display()
        );
    }
}
