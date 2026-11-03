//! Live integration tests for two-tier SHACL validation.
//!
//! Ignored by default. Run against a reachable GraphDB, e.g. the local substrate
//! (`just podman-local-up`), with:
//!
//! ```text
//! cargo test -p ekg-semantic -- --ignored
//! ```
//!
//! Tier A (write-shapes): a structurally malformed own entity is rejected on write
//! while a valid incremental write (a referent not yet materialized) is accepted.
//! Tier B (consistency-shapes): the whole-graph check reports a cross-entity
//! violation on an incomplete graph and conforms once the graph is complete, without
//! blocking the write path. Each test provisions its own SHACL-enabled throwaway
//! repo; if the store is unreachable the test skips.

// `expect`/`panic` are the assertion mechanism here; prose names products.
#![allow(clippy::expect_used, clippy::doc_markdown)]

use ekg_domain::{EngineError, SourceRecord, SourceRef, WorldModel};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

fn graphdb_url() -> String {
    std::env::var("GRAPHDB_URL").unwrap_or_else(|_| "http://localhost:7200".to_owned())
}

fn rec(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
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

/// Provision a fresh SHACL-enabled throwaway repo (`isShacl=true`) and load the
/// write-shapes into GraphDB's reserved SHACL graph so they are enforced on write.
/// `Ok(false)` means the store is unreachable and the test should skip.
async fn provision_shacl_repo(
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
    cfg["title"] = serde_json::Value::from("EKG SHACL IT (throwaway)");
    for (key, val) in [("isShacl", "true"), ("enableContextIndex", "true")] {
        if let Some(v) = cfg.pointer_mut(&format!("/params/{key}/value")) {
            *v = serde_json::Value::from(val);
        }
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
    // Load the write-shapes into the reserved SHACL graph, which enforces them on write.
    GraphDbClient::new(base, repo)
        .map_err(|e| e.to_string())?
        .put_turtle(ontology::SHACL_SHAPES_GRAPH, ontology::WRITE_SHAPES_TTL)
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

fn world(base: &str, repo: &str) -> GraphDbWorldModel {
    GraphDbWorldModel::new(
        GraphDbClient::new(base, repo).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    )
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn write_shapes_reject_malformed_but_accept_valid_incremental() {
    let base = graphdb_url();
    let repo = "ekg_shacl_write_it";
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, base.as_str(), repo).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base}");
            return;
        }
        Err(e) => panic!("provisioning failed: {e}"),
    }
    let world = world(&base, repo);
    let client = GraphDbClient::new(&base, repo).expect("client");

    // --- malformed own entity: a Product with no name violates ProductShape ---
    let malformed = rec("product.product", 1, &[("default_code", "BAT")]);
    let err = world
        .materialize(&malformed)
        .await
        .expect_err("malformed write must be rejected");
    assert!(
        matches!(err, EngineError::Invariant(_)),
        "a store SHACL rejection is a rejected write (Invariant), got: {err:?}"
    );
    // Prior state intact: the rejected entity was never persisted.
    let rows = client
        .select(&format!(
            "SELECT ?p WHERE {{ GRAPH ?g {{ <{}> ?p ?o }} }}",
            ontology::entity_iri("product.product", 1)
        ))
        .await
        .expect("select");
    assert!(
        rows.is_empty(),
        "a rejected write leaves no persisted triples"
    );

    // --- valid incremental: a SalesOrder whose order line is not yet materialized ---
    let incremental = rec(
        "sale.order",
        1,
        &[
            ("commitment_date", "2027-10-19T00:00:00"),
            ("order_line", "[10]"),
        ],
    );
    let revision = world
        .materialize(&incremental)
        .await
        .expect("a valid incremental write must be accepted");
    assert_eq!(
        revision.0, 1,
        "the incremental write commits despite the absent referent"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn whole_graph_consistency_check_reports_then_clears() {
    let base = graphdb_url();
    let repo = "ekg_shacl_consistency_it";
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, base.as_str(), repo).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base}");
            return;
        }
        Err(e) => panic!("provisioning failed: {e}"),
    }
    let world = world(&base, repo);
    let client = GraphDbClient::new(&base, repo).expect("client");

    // Incomplete graph: a sales order whose referenced order line is absent.
    world
        .materialize(&rec(
            "sale.order",
            1,
            &[
                ("commitment_date", "2027-10-19T00:00:00"),
                ("order_line", "[10]"),
            ],
        ))
        .await
        .expect("materialize incomplete sales order (passes write-shapes)");

    let report = client
        .validate(ontology::CONSISTENCY_SHAPES_TTL)
        .await
        .expect("whole-graph validate");
    assert!(!report.conforms, "an incomplete graph does not conform");
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.focus_node == ontology::entity_iri("sale.order", 1)
                && v.path.ends_with("has_line_of")),
        "the violation is the sales order's missing order-line target: {:?}",
        report.violations
    );

    // Complete the graph: the order line and its product. These are ordinary
    // materializations; the consistency check never blocks the write path.
    world
        .materialize(&rec(
            "sale.order.line",
            10,
            &[("product_id", "[100]"), ("product_uom_qty", "5")],
        ))
        .await
        .expect("materialize order line");
    world
        .materialize(&rec("product.product", 100, &[("name", "Battery")]))
        .await
        .expect("materialize product");

    let report = client
        .validate(ontology::CONSISTENCY_SHAPES_TTL)
        .await
        .expect("whole-graph validate (complete)");
    assert!(
        report.conforms,
        "a complete graph conforms: {:?}",
        report.violations
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{repo}"))
        .send()
        .await;
}
