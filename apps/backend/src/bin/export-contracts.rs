//! Export the API contracts the frontend consumes, straight from code with no running backend.
//!
//! ```text
//! cargo run -p ekg-backend --bin export-contracts -- <output-dir>
//! ```
//!
//! Writes `<output-dir>/schema.graphql` (the read surface's SDL, [`graphql::sdl`]),
//! `<output-dir>/openapi.json` (the command surface's [`ApiDoc`]) and
//! `<output-dir>/tab-rows.json` (the read-model tab row vocabulary,
//! [`ekg_semantic::tabs::row_vocabulary`]). Deterministic: the same
//! code always yields byte-identical files. `just frontend-contracts` runs this into
//! `apps/frontend` and then regenerates the typed clients; the backend's
//! `contract_snapshots` test fails whenever the committed files drift from this output.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ekg_backend::graphql;
use ekg_backend::openapi::ApiDoc;
use utoipa::OpenApi;

const USAGE: &str = "usage: export-contracts <output-dir>";

fn main() -> ExitCode {
    match run() {
        Ok(written) => {
            for path in written {
                println!("wrote {}", path.display());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("export-contracts: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Vec<PathBuf>, String> {
    let mut args = std::env::args_os().skip(1);
    let dir = PathBuf::from(args.next().ok_or(USAGE)?);
    if args.next().is_some() {
        return Err(USAGE.to_owned());
    }
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }

    let openapi = ApiDoc::openapi()
        .to_pretty_json()
        .map_err(|err| format!("serialize the OpenAPI document: {err}"))?;

    let tab_rows = serde_json::to_string_pretty(&ekg_semantic::tabs::row_vocabulary())
        .map_err(|err| format!("serialize the tab row vocabulary: {err}"))?;

    Ok(vec![
        write(&dir.join("schema.graphql"), &graphql::sdl())?,
        write(&dir.join("openapi.json"), &openapi)?,
        write(&dir.join("tab-rows.json"), &tab_rows)?,
    ])
}

/// Write `content` normalised to end in exactly one newline, so re-exports are
/// byte-identical and the committed snapshots stay diff-clean.
fn write(path: &Path, content: &str) -> Result<PathBuf, String> {
    let mut normalised = content.trim_end().to_owned();
    normalised.push('\n');
    std::fs::write(path, normalised).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(path.to_path_buf())
}
