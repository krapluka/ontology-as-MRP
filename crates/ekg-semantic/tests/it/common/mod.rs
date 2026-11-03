//! Shared harness for the gated GraphDB integration tests: the target URL, a
//! seed-shaped [`SourceRecord`] builder, and throwaway-repo provisioning. Kept
//! here so the provisioning contract has one home rather than a copy per IT file.

// Not every suite uses every helper; its prose names products and services (as every suite's does).
#![allow(dead_code, clippy::doc_markdown)]

use ekg_domain::{SourceRecord, SourceRef};
use ekg_semantic::GraphDbClient;

/// The GraphDB base URL under test (`GRAPHDB_URL`, default the local substrate).
pub fn graphdb_url() -> String {
    std::env::var("GRAPHDB_URL").unwrap_or_else(|_| "http://localhost:7200".to_owned())
}

/// A seed-shaped source record. Reference fields carry the referenced record's id(s)
/// as the normalized wire form the ingestion layer produces.
pub fn rec(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
    SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        },
        fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

/// GraphDB Free caps a server at five repositories, and the stack's own `ekg` is one of
/// them, so tests that each provision a throwaway repo must not run all at once. The slot is
/// process-wide, so it spans every suite of this crate's one `it` test binary: holding it
/// for a test's whole life keeps one fixture repo per crate at a time.
static FIXTURE_REPO_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Wait for the crate-wide fixture-repo slot; keep the permit until the test ends.
/// The slot is never closed, so this only errs if that invariant is broken.
pub async fn fixture_repo_slot()
-> Result<tokio::sync::SemaphorePermit<'static>, tokio::sync::AcquireError> {
    FIXTURE_REPO_SLOT.acquire().await
}

/// Create a fresh throwaway `repo` from GraphDB's default JSON template (no SHACL).
/// `Ok(false)` means the store is unreachable and the caller should skip.
pub async fn provision_repo(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
) -> Result<bool, String> {
    let Ok(resp) = http.get(format!("{base}/rest/repositories")).send().await else {
        return Ok(false);
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GraphDB reachable but returned {status} listing repos: {body}"
        ));
    }
    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
    let template = http
        .get(format!("{base}/rest/repositories/default-config/graphdb"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = serde_json::from_str(&template).map_err(|e| e.to_string())?;
    cfg["id"] = serde_json::Value::from(repo);
    cfg["title"] = serde_json::Value::from("EKG integration test (throwaway)");
    if let Some(v) = cfg.pointer_mut("/params/enableContextIndex/value") {
        *v = serde_json::Value::from("true");
    }
    let body = serde_json::to_string(&cfg).map_err(|e| e.to_string())?;
    let resp = http
        .post(format!("{base}/rest/repositories"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("repo create failed {status}: {text}"));
    }
    Ok(true)
}

/// Create a fresh SHACL-enabled throwaway `repo` (`isShacl=true`) and load the
/// write-shapes into GraphDB's reserved SHACL graph so they are enforced on write, as in
/// the production `ekg` repo. `Ok(false)` means the store is unreachable.
pub async fn provision_shacl_repo(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
) -> Result<bool, String> {
    let Ok(resp) = http.get(format!("{base}/rest/repositories")).send().await else {
        return Ok(false);
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GraphDB reachable but returned {status} listing repos: {body}"
        ));
    }
    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
    GraphDbClient::new(base, repo)
        .map_err(|e| e.to_string())?
        .create_shacl_repo(repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Count repositories whose id begins with `sim_` (the simulator's ephemeral projection
/// repos). Must be 0 once every projection has been discarded.
pub async fn sim_repo_count(http: &reqwest::Client, base: &str) -> usize {
    let Ok(resp) = http.get(format!("{base}/rest/repositories")).send().await else {
        return 0;
    };
    let body = resp.text().await.unwrap_or_default();
    let Ok(list) = serde_json::from_str::<serde_json::Value>(&body) else {
        return 0;
    };
    list.as_array().map_or(0, |repos| {
        repos
            .iter()
            .filter(|r| {
                r.get("id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| id.starts_with("sim_"))
            })
            .count()
    })
}

/// Poll until no `sim_*` repository is left (a projection's teardown is spawned when it is
/// dropped), for up to five seconds. Returns how many remain: 0 unless one leaked.
pub async fn wait_for_no_sim_repos(http: &reqwest::Client, base: &str) -> usize {
    let mut remaining = sim_repo_count(http, base).await;
    for _ in 0..50 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        remaining = sim_repo_count(http, base).await;
    }
    remaining
}

// --- loud skip ---
// One shared copy for every crate's harness (see the file's own docs).

#[path = "../../../../../test-support/loud_skip.rs"]
mod loud_skip;
// Not every suite uses every helper (as with the module's `dead_code` allowance).
#[allow(unused_imports)]
pub use loud_skip::{REQUIRE_INFRA_ENV, infra_required, infra_unavailable, skip_or_fail};

// --- Odoo fixtures ---
// Shared with the backend's gated harness (see the file's own docs).

#[path = "../../../../../test-support/odoo_fixture.rs"]
pub mod odoo_fixture;
