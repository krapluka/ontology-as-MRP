//! `ekg-kinetic`'s integration tests, built as one test binary. Each suite is a module
//! named after its file, so one suite still runs on its own by name, e.g.
//! `cargo test -p ekg-kinetic --test it writeback_it -- --ignored`.

mod executor_fake_odoo;
mod writeback_it;
