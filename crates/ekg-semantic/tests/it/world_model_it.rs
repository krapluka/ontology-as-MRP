//! Gated live integration test for the GraphDB [`WorldModel`] driver.
//!
//! Ignored by default. Run against a reachable GraphDB, e.g. the local substrate
//! (`just podman-local-up`), which supplies the license, with:
//!
//! ```text
//! cargo test -p ekg-semantic -- --ignored
//! ```
//!
//! Target another store via `GRAPHDB_URL` (default `http://localhost:7200`). The
//! test provisions and tears down its own throwaway repository, so it never
//! touches the `ekg` world-model repo. If the store is unreachable it skips
//! (prints a clear message) rather than failing, so `just check`, which never
//! runs `--ignored` tests, stays green without Docker.

// In this integration test `expect`/`panic` are the assertion mechanism, and the
// prose names products (GraphDB, RDF4J) without backticks.
#![allow(clippy::expect_used, clippy::doc_markdown)]

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{
    EntityUri, OmissionKind, PolicyId, ReadContext, ReadPolicy, SourceRecord, SourceRef, WorldModel,
};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

fn graphdb_url() -> String {
    std::env::var("GRAPHDB_URL").unwrap_or_else(|_| "http://localhost:7200".to_owned())
}

fn read_context(role: &str) -> ReadContext {
    ReadContext {
        role: role.to_owned(),
        purpose: "integration-test".to_owned(),
    }
}

/// Create a fresh throwaway repo `repo` from GraphDB's default-config template (no
/// SHACL). `Ok(false)` means the store is unreachable and the test should skip.
/// Each test uses its own repo id so `--ignored` tests can run in parallel.
async fn provision_repo(http: &reqwest::Client, base: &str, repo: &str) -> Result<bool, String> {
    // Reachability probe: a transport failure means no store is there, so skip.
    // A store that answers with an error status is reachable but misbehaving:
    // surface it as an error rather than passing the test.
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
    // Drop any leftover from a prior run, then create from the full default template
    // (patched id/title) so every required repo param is present.
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
    cfg["title"] = serde_json::Value::from("EKG WorldModel driver IT (throwaway)");
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

async fn delete_repo(http: &reqwest::Client, base: &str, repo: &str) {
    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
#[allow(clippy::too_many_lines)]
async fn driver_round_trip_against_live_graphdb() {
    const REPO: &str = "ekg_it_roundtrip";
    let base = graphdb_url();
    let http = reqwest::Client::builder()
        .build()
        .expect("build http client");

    match provision_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "SKIP: no GraphDB reachable at {base} (set GRAPHDB_URL or start the substrate)"
            );
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    // The server's repository listing names this test's own repository.
    let listed = GraphDbClient::new(&base, REPO)
        .expect("build listing client")
        .repository_ids()
        .await
        .expect("list repositories");
    assert!(
        listed.iter().any(|id| id == REPO),
        "the listing names the provisioned repository: {listed:?}"
    );

    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build driver client"),
        ekg_semantic::ontology::supply_risk_contract().expect("load supply-risk contract"),
    );

    // A typed product record (name + internal reference), projected through the
    // supply-risk mapping contract. (The read-policy's field-level omissions on the live
    // driver are exercised further down, under an injected policy.)
    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), "VeloMax E-Bike".to_owned());
    fields.insert("default_code".to_owned(), "EBIKE-01".to_owned());
    let record = SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: "product.product".to_owned(),
            id: 7,
        },
        fields,
    };
    let entity = EntityUri(ontology::entity_iri("product.product", 7));

    // --- materialize: first revision, provenance queryable ---
    let revision = world.materialize(&record).await.expect("materialize");
    assert_eq!(revision.0, 1, "first materialization is revision 1");

    let prov = world
        .select(&format!(
            "SELECT ?o WHERE {{ GRAPH ?g {{ <{entity}> <{vocab}wasDerivedFrom> ?o }} }}",
            entity = entity.0,
            vocab = ontology::VOCAB_NAMESPACE,
        ))
        .await
        .expect("select provenance");
    assert_eq!(prov.len(), 1, "the materialized entity carries provenance");

    // --- ground: the typed entity's evidence edges carry source + revision ---
    let grounding = world
        .ground(&entity, &read_context("procurement"))
        .await
        .expect("ground");
    assert!(
        grounding.edges.iter().any(|e| e.predicate == "ekg:name"),
        "grounding surfaces the typed name edge"
    );
    // Every evidence edge carries source + revision provenance.
    assert!(
        grounding
            .edges
            .iter()
            .all(|e| e.source.system == "odoo" && e.revision.0 == 1),
        "evidence edges carry source + revision provenance"
    );

    // --- read-policy: the live driver delegates to the injected ReadPolicy ---
    // A second world over the same repo, under a policy that marks the (definitely
    // materialized) `name` edge sensitive: a non-privileged reader must have it hidden and
    // recorded as an omission, while a privileged role still sees it.
    let restricted = GraphDbWorldModel::new_with_read_policy(
        GraphDbClient::new(&base, REPO).expect("build restricted client"),
        ekg_semantic::ontology::supply_risk_contract().expect("load supply-risk contract"),
        ReadPolicy::new(
            PolicyId("read-policy/it".to_owned()),
            BTreeSet::from(["procurement".to_owned()]),
            BTreeMap::from([("name".to_owned(), "ekg:name".to_owned())]),
            BTreeMap::new(),
        ),
    );
    let planner_view = restricted
        .ground(&entity, &read_context("planner"))
        .await
        .expect("ground under the restricted policy");
    assert!(
        !planner_view.edges.iter().any(|e| e.predicate == "ekg:name"),
        "the sensitive name edge is hidden from a non-privileged reader"
    );
    let name_omission = planner_view
        .omissions
        .iter()
        .find(|o| o.type_iri == "ekg:name")
        .expect("the hidden name is recorded as an omission");
    assert_eq!(name_omission.kind, OmissionKind::Edge);
    assert_eq!(name_omission.count, 1);
    assert_eq!(
        name_omission.policy_id,
        PolicyId("read-policy/it".to_owned())
    );
    let privileged_view = restricted
        .ground(&entity, &read_context("procurement"))
        .await
        .expect("ground as a privileged role");
    assert!(
        privileged_view
            .edges
            .iter()
            .any(|e| e.predicate == "ekg:name"),
        "a privileged role still sees the name edge"
    );
    assert!(
        !privileged_view.has_omissions(),
        "a privileged role has no omissions"
    );

    // --- retract: bumps revision, and a subsequent ground is NotFound ---
    let retract_rev = world.retract(&entity).await.expect("retract");
    assert_eq!(retract_rev.0, 2, "retract bumps the revision");
    assert!(
        world
            .ground(&entity, &read_context("procurement"))
            .await
            .is_err(),
        "grounding a retracted entity is an error"
    );

    delete_repo(&http, &base, REPO).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn concurrent_materializations_preserve_monotonic_revisions() {
    use std::sync::Arc;

    const REPO: &str = "ekg_it_concurrency";
    const N: u64 = 5;
    let base = graphdb_url();
    let http = reqwest::Client::builder()
        .build()
        .expect("build http client");

    match provision_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base}");
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build client"),
        ekg_semantic::ontology::supply_risk_contract().expect("load supply-risk contract"),
    ));

    // N concurrent materializations of the same entity must each get a distinct,
    // monotonic revision: the per-entity lock serializes the read-then-write sequence.
    let mut handles = Vec::new();
    for i in 0..N {
        let world = Arc::clone(&world);
        handles.push(tokio::spawn(async move {
            let mut fields = BTreeMap::new();
            fields.insert("name".to_owned(), format!("attempt {i}"));
            let record = SourceRecord {
                source_ref: SourceRef {
                    system: "odoo".to_owned(),
                    model: "product.product".to_owned(),
                    id: 42,
                },
                fields,
            };
            world.materialize(&record).await.expect("materialize").0
        }));
    }

    let mut revisions = Vec::new();
    for h in handles {
        revisions.push(h.await.expect("join task"));
    }
    revisions.sort_unstable();
    assert_eq!(
        revisions,
        (1..=N).collect::<Vec<_>>(),
        "concurrent materializations yield distinct monotonic revisions with no duplicates"
    );

    delete_repo(&http, &base, REPO).await;
}
