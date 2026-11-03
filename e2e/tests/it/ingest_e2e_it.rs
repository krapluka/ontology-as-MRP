//! Gated live end-to-end test: an authenticated webhook durably materializes a
//! typed, provenanced entity through the Restate ingest worker.
//!
//! Ignored by default. It needs the whole substrate up (Odoo, GraphDB, Restate)
//! and the durable worker's SDK endpoint reachable from the Restate runtime:
//!
//! ```text
//! just podman-local-up                       # graphdb + restate
//! just podman-local-up-sources               # odoo (+ postgres)
//! cargo test -p ekg-e2e --test it ingest_e2e_it -- --ignored --nocapture
//! ```
//!
//! Because Restate calls back into the worker endpoint, the test serves that
//! endpoint in-process and registers it with the Restate admin API under a callback
//! URL the runtime can dial. In the podman substrate the runtime reaches the host at
//! `host.containers.internal`; override with `EKG_SERVICE_CALLBACK_URL` if needed.
//! If any hop is unreachable the test skips (returns) rather than failing.
//!
//! Uses `res.partner` id 1 (the company partner present in a fresh Odoo), so it
//! needs no supply-risk seed.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::InProcessPublisher;
use ekg_backend::submit::{RestateSubmitter, Submitter};
use ekg_backend::worker::Ingest;
use ekg_domain::SourceRef;
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use restate_sdk::prelude::Endpoint;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
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
#[ignore = "requires Odoo + GraphDB + Restate, and the worker endpoint reachable from the runtime (run with --ignored)"]
async fn authenticated_webhook_durably_materializes_the_entity() {
    let odoo_url = env_or("ODOO_URL", "http://localhost:8069");
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    // The URL the Restate runtime dials to reach this in-process worker endpoint.
    let service_addr = common::service_addr(0);
    let callback_url = common::callback_url(0);

    let http = reqwest::Client::new();

    // --- guard: Restate admin reachable, else skip ---
    if http
        .get(format!("{admin_url}/health"))
        .send()
        .await
        .is_err()
        && http
            .get(format!("{admin_url}/deployments"))
            .send()
            .await
            .is_err()
    {
        eprintln!("SKIP: Restate admin unreachable at {admin_url}");
        return;
    }

    // --- connect the Odoo ingestor; skip if unreachable ---
    let odoo = OdooConfig {
        base_url: odoo_url.clone(),
        db: env_or("ODOO_DB", "ekg"),
        login: env_or("ODOO_USER", "admin"),
        password: env_or("ODOO_API_KEY", "admin"),
        system: "odoo".to_owned(),
        allowlist: vec!["res.partner".to_owned()],
        scopes: std::collections::BTreeMap::new(),
    };
    let ingestor = match OdooIngestor::connect(odoo).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("SKIP: Odoo unreachable: {e}");
            return;
        }
    };

    // --- provision a throwaway GraphDB repo; skip if GraphDB is unreachable ---
    let repo = "ekg_ingest_e2e_it";
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
    cfg["title"] = serde_json::Value::from("EKG ingest e2e IT (throwaway)");
    if let Some(v) = cfg.pointer_mut("/params/enableContextIndex/value") {
        *v = serde_json::Value::from("true");
    }
    http.post(format!("{graphdb_url}/rest/repositories"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&cfg).expect("cfg"))
        .send()
        .await
        .expect("create repo");

    // --- serve the durable ingest worker endpoint in-process ---
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    let ingest = Ingest::new(
        Arc::new(ingestor),
        Arc::new(world),
        Arc::new(InProcessPublisher::new(64)),
    );
    let endpoint = Endpoint::builder().bind(ingest).build();
    // Pre-binds the listener, so a port collision fails here (not as a later timeout)
    // and the socket is listening before the deployment is registered.
    common::spawn_endpoint(endpoint, &service_addr).await;

    // --- register the deployment with Restate so it can call back in ---
    let register = http
        .post(format!("{admin_url}/deployments"))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "uri": callback_url, "force": true }).to_string())
        .send()
        .await
        .expect("register deployment");
    assert!(
        register.status().is_success(),
        "Restate rejected deployment registration ({}): is {callback_url} reachable from the runtime?",
        register.status()
    );

    // --- fire the authenticated webhook path: submit to the ingress ---
    let source_ref = SourceRef {
        system: "odoo".to_owned(),
        model: "res.partner".to_owned(),
        id: 1,
    };
    RestateSubmitter::new(ingress_url)
        .submit(&source_ref)
        .await
        .expect("submit to ingress");

    // --- poll the world model until the entity's provenance lands ---
    let world_reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
    let entity = ontology::entity_iri("res.partner", 1);
    let query = format!(
        "SELECT ?o WHERE {{ GRAPH ?g {{ <{entity}> <{vocab}wasDerivedFrom> ?o }} }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    let materialized = wait_until(Duration::from_secs(30), async || {
        world_reader
            .select(&query)
            .await
            .is_ok_and(|rows| !rows.is_empty())
    })
    .await;

    // --- cleanup before asserting so a failure doesn't leak the repo ---
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    common::deregister(&http, &admin_url, &callback_url).await;

    assert!(
        materialized,
        "the webhook did not durably materialize res.partner/1 within the timeout"
    );
}
