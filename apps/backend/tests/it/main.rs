//! `ekg-backend`'s hermetic integration tests, built as one test binary: each suite is a
//! module named after its file, so one suite still runs on its own by name, e.g.
//! `cargo test -p ekg-backend --test it contract_snapshots`. The suites that need the running
//! stack are in the `ekg-e2e` crate (`e2e/`).

mod agent_eval_set;
mod contract_snapshots;
mod skills_drift;
mod slate_trace_serde_it;
mod workspace_rust_version;
