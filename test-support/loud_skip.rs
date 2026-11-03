//! The loud-skip helper shared by every crate's gated-test harness. One copy, included by
//! `#[path]` from each `tests/it/common/mod.rs` (a sibling crate's test module cannot be
//! imported, and hand-kept copies drift). std-only.

/// The environment flag that turns a gated test's "infrastructure unavailable" skip into a
/// failure. `just test-e2e` sets it, so a gated test that reports through
/// [`infra_unavailable`] cannot pass on a silent `SKIP` there. Suites that print their
/// own `SKIP` ignore it.
pub const REQUIRE_INFRA_ENV: &str = "EKG_E2E_REQUIRE_INFRA";

/// Whether a [`REQUIRE_INFRA_ENV`] value (as read by `std::env::var_os`) demands real
/// infrastructure. Fails closed: any value other than unset / empty / `0` / `false`
/// requires it, so a typo (`yes`, `on`, `ture`) or a non-Unicode value can never silently
/// re-enable skipping.
#[must_use]
pub fn infra_required(flag: Option<&std::ffi::OsStr>) -> bool {
    match flag.map(std::ffi::OsStr::to_str) {
        None => false,
        Some(None) => true,
        Some(Some(v)) => {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false")
        }
    }
}

/// Report that a gated test's infrastructure is unavailable: fail loudly when it is
/// `required`, else print `SKIP: why` so the caller can return early.
///
/// # Panics
/// When `required`, so the run fails instead of passing on a skip.
pub fn skip_or_fail(required: bool, why: &str) {
    assert!(
        !required,
        "{REQUIRE_INFRA_ENV} is set but gated infrastructure is unavailable: {why}"
    );
    eprintln!("SKIP: {why}");
}

/// [`skip_or_fail`] driven by the process environment ([`REQUIRE_INFRA_ENV`]). Call it at
/// every "infrastructure unavailable" early return of a gated test. A value that is not
/// valid Unicode also requires infrastructure (the flag fails closed).
///
/// # Panics
/// When [`REQUIRE_INFRA_ENV`] is set: the missing dependency fails the run.
pub fn infra_unavailable(why: &str) {
    skip_or_fail(
        infra_required(std::env::var_os(REQUIRE_INFRA_ENV).as_deref()),
        why,
    );
}
