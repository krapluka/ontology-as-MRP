//! The backend's live suites, built as one test binary: each suite is a module named after
//! its file, so one suite runs on its own by name, e.g.
//! `cargo test -p ekg-e2e --test it auto_apply_policy_e2e_it -- --ignored`.

mod common;

mod agent_eval_fixtures_it;
mod agent_promotion_criteria_e2e_it;
mod agent_promotion_gate_e2e_it;
mod agent_shadow_e2e_it;
mod agent_shadow_pairing_e2e_it;
mod auto_apply_policy_e2e_it;
mod autopromote_e2e_it;
mod derive_on_ingest_e2e_it;
mod drift_monitor_e2e_it;
mod ingest_e2e_it;
mod loud_skip;
mod nats_roundtrip_it;
mod plans_read_e2e_it;
mod plant_pulse_api_e2e_it;
mod portfolio_dismiss_e2e_it;
mod portfolio_grouping_e2e_it;
mod portfolio_promote_e2e_it;
mod portfolio_slate_e2e_it;
mod process_drift_it;
mod promote_abort_e2e_it;
mod promote_slate_option_e2e_it;
mod propose_stage_e2e_it;
mod skill_registry_e2e_it;
mod slate_e2e_it;
mod slate_supply_risk_e2e_it;
mod sweep_bootstrap_it;
mod sweep_e2e_it;
mod sweep_resync_e2e_it;
mod sweep_resync_shared_e2e_it;
mod sweep_scope_e2e_it;
mod tool_gateway_e2e_it;
mod transport_integration_it;
mod vector_projection_it;
