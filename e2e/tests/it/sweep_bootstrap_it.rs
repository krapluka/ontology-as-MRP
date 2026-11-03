//! Gated live end-to-end test: on a fresh boot the reconciliation sweep runs without
//! an explicit trigger. The backend's startup bootstrap fires the first pass, which
//! re-pulls a change that never got a webhook.
//!
//! Ignored by default. Same stack and callback requirements as `sweep_e2e_it`
//! (Odoo, GraphDB and Restate, worker endpoint reachable from the runtime):
//!
//! ```text
//! just podman-local-up && just podman-local-up-sources
//! cargo test -p ekg-e2e --test it sweep_bootstrap_it -- --ignored --nocapture
//! ```
//!
//! Scenario, against a throwaway GraphDB repo: create a `res.partner` that is never
//! webhooked, then call the same `trigger_first_sweep` the backend runs at startup
//! (not a hand-written `/Sweep/default/run/send` POST) and assert the record is
//! re-pulled (its provenance appears).
//!
//! The record is created fresh per run, so its `write_date` always beats the durable
//! `Sweep` watermark carried across runs, and the assertion holds on a stack that has
//! run before. If any hop is unreachable the test skips instead of failing.
//!
//! The trigger targets the stack's own key (`Sweep/default`), so the run claims `Ingest`
//! and `Sweep` through `common::with_sweep_claimed` under that key: the stack's timer is
//! paused meanwhile, the pass this test starts (and its reschedule) is cancelled afterwards,
//! and the stack's chain(s) are re-armed on its backend, a panic or hang included. Caveat:
//! the pass runs on the stack's key, so it advances the stack's shared watermark with a
//! `res.partner`-only pass (other models' changes older than it are then left to webhooks).

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::InProcessPublisher;
use ekg_backend::sweep::{SWEEP_KEY, Sweep, trigger_first_sweep};
use ekg_backend::worker::{INGEST_OBJECT, Ingest};
use ekg_domain::WorldModel;
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::json;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Does the entity carry provenance (i.e. is it currently materialized)?
async fn has_provenance(client: &GraphDbClient, model: &str, id: i64) -> bool {
    let entity = ontology::entity_iri(model, id);
    let query = format!(
        "SELECT ?o WHERE {{ GRAPH ?g {{ <{entity}> <{vocab}wasDerivedFrom> ?o }} }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    client
        .select(&query)
        .await
        .is_ok_and(|rows| !rows.is_empty())
}

/// Create an active `res.partner` and return its id (blocking RPC off the runtime).
async fn create_partner(odoo: &Arc<Odoo>, name: &str) -> i64 {
    let odoo = Arc::clone(odoo);
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || odoo.create("res.partner", json!({ "name": name })))
        .await
        .expect("join")
        .expect("create partner")
}

/// Best-effort hard-delete of a throwaway partner (cleanup).
async fn unlink_partner(odoo: &Arc<Odoo>, id: i64) {
    let odoo = Arc::clone(odoo);
    let _ = tokio::task::spawn_blocking(move || odoo.unlink("res.partner", json!([id]))).await;
}

/// Poll until `condition` holds or the deadline passes.
async fn wait_until<F>(timeout: Duration, mut condition: F) -> bool
where
    F: AsyncFnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn bootstrap_trigger_runs_the_sweep_without_an_explicit_kick() {
    let odoo_url = env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = env_or("ODOO_DB", "ekg");
    let odoo_user = env_or("ODOO_USER", "admin");
    let odoo_key = env_or("ODOO_API_KEY", "admin");
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(4);
    let callback_url = common::callback_url(4);

    let http = reqwest::Client::new();

    // --- guard: Restate admin reachable, else skip ---
    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        eprintln!("SKIP: Restate admin unreachable at {admin_url}");
        return;
    }

    // --- raw Odoo client for the create/unlink mutations (blocking) ---
    let odoo_raw = {
        let (url, db, user, key) = (
            odoo_url.clone(),
            odoo_db.clone(),
            odoo_user.clone(),
            odoo_key.clone(),
        );
        match tokio::task::spawn_blocking(move || Odoo::connect(&url, &db, &user, &key))
            .await
            .expect("join")
        {
            Ok(o) => Arc::new(o),
            Err(e) => {
                eprintln!("SKIP: Odoo unreachable: {e}");
                return;
            }
        }
    };

    // --- connect the ingestor and provision a throwaway GraphDB repo ---
    let ingestor = OdooIngestor::connect(OdooConfig {
        base_url: odoo_url.clone(),
        db: odoo_db,
        login: odoo_user,
        password: odoo_key,
        system: "odoo".to_owned(),
        allowlist: vec!["res.partner".to_owned()],
        scopes: std::collections::BTreeMap::new(),
    })
    .await
    .expect("connect ingestor");

    let repo = "ekg_sweep_bootstrap_it";
    let Ok(list) = http
        .get(format!("{graphdb_url}/rest/repositories"))
        .send()
        .await
    else {
        eprintln!("SKIP: GraphDB unreachable at {graphdb_url}");
        return;
    };
    if !list.status().is_success() {
        eprintln!("SKIP: GraphDB not ready");
        return;
    }
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let template = http
        .get(format!(
            "{graphdb_url}/rest/repositories/default-config/graphdb"
        ))
        .send()
        .await
        .expect("template")
        .text()
        .await
        .expect("template body");
    let mut cfg: serde_json::Value = serde_json::from_str(&template).expect("template json");
    cfg["id"] = serde_json::Value::from(repo);
    cfg["title"] = serde_json::Value::from("EKG sweep bootstrap IT (throwaway)");
    if let Some(v) = cfg.pointer_mut("/params/enableContextIndex/value") {
        *v = serde_json::Value::from("true");
    }
    http.post(format!("{graphdb_url}/rest/repositories"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&cfg).expect("cfg"))
        .send()
        .await
        .expect("create repo");

    // --- serve Ingest and Sweep in-process; register with Restate ---
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let publisher = Arc::new(InProcessPublisher::new(64));
    let odoo = Arc::new(ingestor);
    let ingest = Ingest::new(
        Arc::clone(&odoo) as Arc<dyn ekg_domain::SourceIngestor>,
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
    );
    let sweep = Sweep::new(
        Arc::clone(&odoo),
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
        vec!["res.partner".to_owned()],
        3600, // long reschedule; the bootstrap trigger fires the single pass we assert
    );
    let endpoint = Endpoint::builder().bind(ingest).bind(sweep).build();
    // The listener is bound first, so a port collision fails here and not as a later
    // timeout, and the socket is listening before we register.
    common::spawn_endpoint(endpoint, &service_addr).await;
    let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");

    // A fresh partner that is never webhooked: only the sweep can materialize it. Its
    // `write_date` is newer than any watermark the durable `Sweep` carries from a prior
    // run, so the bootstrap pass always re-pulls it.
    let missed_id = create_partner(&odoo_raw, "EKG sweep bootstrap IT missed (throwaway)").await;

    // The startup trigger targets the stack's own key, so the claim runs under SWEEP_KEY: the
    // stack's paused chain(s) are re-armed on its backend afterwards, and the pass this test
    // started (and its reschedule) is cancelled, a panic or hang included.
    let claim = common::SweepClaim {
        http: &http,
        admin_url: &admin_url,
        ingress_url: &ingress_url,
        callback_url: &callback_url,
        sweep_key: SWEEP_KEY,
    };
    let claimed = common::with_sweep_claimed(&claim, async |deployment: &str| {
        // --- the startup path: what the backend runs on boot, no explicit POST ---
        let triggered = trigger_first_sweep(&ingress_url).await;

        // --- the never-webhooked partner is re-pulled by the auto-triggered sweep ---
        let repulled = triggered.is_ok()
            && wait_until(Duration::from_secs(60), async || {
                has_provenance(&reader, "res.partner", missed_id).await
            })
            .await;
        // Let the pass's ingests finish before anything is torn down.
        let drained = common::wait_finished(
            &http,
            &admin_url,
            common::Invocations::of(INGEST_OBJECT)
                .key_prefix("res.partner:")
                .run_by(deployment),
            Duration::from_secs(120),
        )
        .await;
        (triggered, repulled, drained)
    })
    .await;

    // --- the test's own teardown, then the assertions ---
    unlink_partner(&odoo_raw, missed_id).await;
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let (triggered, repulled, drained) = claimed.into_inner();

    triggered.expect("bootstrap sweep trigger");
    assert!(
        repulled,
        "the startup bootstrap did not run the sweep: the missed change was not re-pulled"
    );
    drained.expect("every ingest the pass sent finished");
}
