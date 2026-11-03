//! Live integration test: the Odoo [`SourceIngestor`] fat-pulls a real record
//! and it materializes through the typed WorldModel with provenance.
//!
//! Ignored by default. Needs the local substrate (Odoo + GraphDB) up. Run with:
//!
//! ```text
//! cargo test -p ekg-semantic -- --ignored
//! ```
//!
//! Uses `res.partner` id 1 (the main company partner always present in a fresh
//! Odoo), so it needs no supply-risk seed. If Odoo or GraphDB is unreachable the
//! test skips rather than failing.

#![allow(clippy::expect_used, clippy::doc_markdown)]

use ekg_domain::{SourceRef, WorldModel};
use ekg_semantic::{
    GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, SourceIngestor, ontology,
};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[tokio::test]
#[ignore = "requires a reachable Odoo + GraphDB (run with --ignored)"]
async fn odoo_pull_materializes_a_typed_provenanced_entity() {
    // --- connect the ingestor; skip if Odoo is unreachable ---
    let config = OdooConfig {
        base_url: env_or("ODOO_URL", "http://localhost:8069"),
        db: env_or("ODOO_DB", "ekg"),
        login: env_or("ODOO_USER", "admin"),
        password: env_or("ODOO_API_KEY", "admin"),
        system: "odoo".to_owned(),
        allowlist: vec!["res.partner".to_owned()],
        scopes: std::collections::BTreeMap::new(),
    };
    let ingestor = match OdooIngestor::connect(config).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("SKIP: Odoo unreachable: {e}");
            return;
        }
    };

    // --- provision a throwaway GraphDB repo; skip if GraphDB is unreachable ---
    let base = env_or("GRAPHDB_URL", "http://localhost:7200");
    let repo = "ekg_ingest_it";
    let http = reqwest::Client::new();
    let Ok(list) = http.get(format!("{base}/rest/repositories")).send().await else {
        eprintln!("SKIP: GraphDB unreachable at {base}");
        return;
    };
    if !list.status().is_success() {
        eprintln!("SKIP: GraphDB not ready");
        return;
    }
    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
    let template = http
        .get(format!("{base}/rest/repositories/default-config/graphdb"))
        .send()
        .await
        .expect("template")
        .text()
        .await
        .expect("template body");
    let mut cfg: serde_json::Value = serde_json::from_str(&template).expect("template json");
    cfg["id"] = serde_json::Value::from(repo);
    cfg["title"] = serde_json::Value::from("EKG ingest IT (throwaway)");
    if let Some(v) = cfg.pointer_mut("/params/enableContextIndex/value") {
        *v = serde_json::Value::from("true");
    }
    http.post(format!("{base}/rest/repositories"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&cfg).expect("cfg"))
        .send()
        .await
        .expect("create repo");

    // --- fat-pull res.partner/1, materialize through the contract ---
    let source_ref = SourceRef {
        system: "odoo".to_owned(),
        model: "res.partner".to_owned(),
        id: 1,
    };
    let record = ingestor
        .pull(&source_ref)
        .await
        .expect("pull res.partner/1");
    assert!(
        record.fields.contains_key("name"),
        "the pulled partner carries a name: {:?}",
        record.fields
    );

    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    let revision = world
        .materialize(&record)
        .await
        .expect("materialize pulled record");
    assert_eq!(revision.0, 1);

    // --- provenance traces to the source record ---
    let entity = ontology::entity_iri("res.partner", 1);
    let prov = world
        .select(&format!(
            "SELECT ?o WHERE {{ GRAPH ?g {{ <{entity}> <{vocab}wasDerivedFrom> ?o }} }}",
            vocab = ontology::VOCAB_NAMESPACE,
        ))
        .await
        .expect("select provenance");
    assert_eq!(prov.len(), 1, "the materialized entity carries provenance");

    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
}
