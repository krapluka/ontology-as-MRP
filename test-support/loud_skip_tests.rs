//! The loud-skip helper's own tests. One copy, included by `#[path]` from each crate's
//! `tests/it/loud_skip.rs`, so both crates' harnesses are exercised by identical cases.

use std::ffi::OsStr;

use crate::common::{infra_required, skip_or_fail};

#[test]
fn the_flag_requires_infrastructure_unless_unset_empty_or_falsy() {
    for required in ["1", " true ", "TRUE", "yes", "on", "ture"] {
        assert!(
            infra_required(Some(OsStr::new(required))),
            "{required:?} requires infrastructure"
        );
    }
    assert!(!infra_required(None));
    for optional in ["", "  ", "0", "false", "FALSE"] {
        assert!(
            !infra_required(Some(OsStr::new(optional))),
            "{optional:?} does not"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_non_unicode_flag_requires_infrastructure() {
    use std::os::unix::ffi::OsStrExt;
    assert!(infra_required(Some(OsStr::from_bytes(b"\xff"))));
}

#[test]
fn an_optional_dependency_only_skips() {
    skip_or_fail(false, "graphdb down");
}

#[test]
#[should_panic(expected = "gated infrastructure is unavailable: graphdb down")]
fn a_required_but_unavailable_dependency_fails_the_run() {
    skip_or_fail(true, "graphdb down");
}
