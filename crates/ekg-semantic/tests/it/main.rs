//! `ekg-semantic`'s integration tests, built as one test binary. Each suite is a module
//! named after its file, so one suite still runs on its own by name, e.g.
//! `cargo test -p ekg-semantic --test it coupling_rule_it -- --ignored`.

mod common;

mod coupling_rule_it;
mod delivery_lots_it;
mod fixture_it;
mod ingest_it;
mod loud_skip;
mod process_derivations_it;
mod reconciliation_active_scope_it;
mod risk_rule_it;
mod shacl_validation_it;
mod simulator_it;
mod tabs_it;
mod world_model_it;
