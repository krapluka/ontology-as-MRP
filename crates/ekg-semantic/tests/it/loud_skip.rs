//! The loud-skip helper's own tests, one suite per crate. The cases live in one shared file
//! (`test-support/loud_skip_tests.rs`), so both crates' harnesses run identical cases.

// The shared harness's prose names products and services, like every gated suite.
#![allow(clippy::doc_markdown)]

#[path = "../../../../test-support/loud_skip_tests.rs"]
mod loud_skip_tests;
