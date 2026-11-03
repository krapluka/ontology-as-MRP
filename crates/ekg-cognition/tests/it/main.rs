//! `ekg-cognition`'s integration tests, built as one test binary to keep link time down.
//! Each suite is a module named after its file, so one suite runs on its own by name,
//! e.g. `cargo test -p ekg-cognition --test it tls_smoke_it -- --ignored`.

/// The shared loud-skip helper, included once for every gated suite in this binary.
#[path = "../../../../test-support/loud_skip.rs"]
mod loud_skip;

mod agent_proposer;
mod agent_runtime_interop_it;
mod cognition_boundary;
mod runtime_contract_it;
mod spans;
mod tls_smoke_it;
mod tool_gateway;
mod tool_gateway_interop_it;
