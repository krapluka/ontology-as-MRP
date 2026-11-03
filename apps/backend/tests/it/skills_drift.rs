//! The skill drift check: every skill committed under `apps/hayhooks/skills` must name
//! registered tool profiles, and only tools those profiles offer. It needs no infrastructure
//! and runs in the full gate (`just verify`), so a renamed or removed tool or profile fails
//! the PR instead of a live run.
//!
//! ```text
//! cargo test -p ekg-backend --test it skills_drift
//! ```

#![allow(clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use ekg_backend::propose::tool_profiles;
use ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS;
use ekg_cognition::skills::read_and_validate_all;
use ekg_domain::WorldModel;
use ekg_domain::fakes::InMemoryWorldModel;

#[test]
fn every_committed_skill_names_registered_profiles_and_their_tools() {
    let world: Arc<dyn WorldModel> = Arc::new(InMemoryWorldModel::default());
    let profiles = tool_profiles(&world, DEFAULT_MAX_PLAN_ACTIONS)
        .expect("profiles")
        .offered_tools();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hayhooks/skills");
    match read_and_validate_all(&root, &profiles) {
        Ok(bundles) => assert!(
            !bundles.is_empty(),
            "no skill found under {}",
            root.display()
        ),
        Err(errors) => panic!(
            "invalid skills under {}:\n{}",
            root.display(),
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}
