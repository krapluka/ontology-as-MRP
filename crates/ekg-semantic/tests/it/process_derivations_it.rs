//! Live integration test: the structural process-graph derivations
//! (`derive-precedes.rq`, `derive-process-edges.rq`) run over typed entities
//! materialized through the mapping contract and produce the expected edges, and
//! reclaim stale edges when source data changes.
//!
//! Ignored by default. Run against a reachable GraphDB (e.g. `just podman-local-up`):
//!
//! ```text
//! cargo test -p ekg-semantic --test it process_derivations_it -- --ignored
//! ```
//!
//! Covers the consecutive-operation `ekg:precedes` selection (10 to 20 to 30, never 10 to
//! 30), the BOM-component `ekg:used` walk, the `origin`/`name` `ekg:fulfills` match and
//! the `MAX`-per-agent `ekg:capacity` aggregation. It then mutates a fixture and re-derives:
//! the derivation clears everything it derived before inserting, which a bare re-insert
//! would not, so stale edges go away. Skips if the store is unreachable.

// `expect`/`panic` are the assertion mechanism here; prose names domain terms.
// One linear scenario asserting every derivation reads best as a single test.
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use ekg_domain::WorldModel;
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

use crate::common;
use common::{graphdb_url, provision_repo, rec};

const REPO: &str = "ekg_process_deriv_it";

/// A minimal process fixture exercising every derivation:
/// - BOM/20 has operations at sequences 10/20/30 (precedes 10 to 20 and 20 to 30, not 10
///   to 30);
/// - MO/1 builds_from BOM/20 whose one component is Part(101) (used: 101);
/// - MO/1 `origin` "S00001" equals sale.order/1 `name` "S00001" (fulfills: SO/1);
/// - WorkCenter/1 has two capacity rows 2 and 3 (derived capacity = MAX = 3).
fn fixtures() -> Vec<ekg_domain::SourceRecord> {
    vec![
        rec("sale.order", 1, &[("name", "S00001")]),
        rec("product.product", 100, &[("name", "Widget")]),
        rec("product.product", 101, &[("name", "Part")]),
        rec(
            "mrp.bom",
            20,
            &[("product_id", "[100]"), ("bom_line_ids", "[30]")],
        ),
        rec(
            "mrp.bom.line",
            30,
            &[("product_id", "[101]"), ("product_qty", "1")],
        ),
        rec(
            "mrp.production",
            1,
            &[
                ("product_id", "[100]"),
                ("bom_id", "[20]"),
                ("product_qty", "5"),
                ("state", "confirmed"),
                ("origin", "S00001"),
            ],
        ),
        rec("mrp.workcenter", 1, &[("name", "Assembly")]),
        capacity_rec(1, 100, "2"),
        capacity_rec(2, 101, "3"),
        operation_rec(1, "Op A", "10"),
        operation_rec(2, "Op B", "20"),
        operation_rec(3, "Op C", "30"),
    ]
}

fn capacity_rec(id: i64, product: i64, capacity: &str) -> ekg_domain::SourceRecord {
    rec(
        "mrp.workcenter.capacity",
        id,
        &[
            ("workcenter_id", "[1]"),
            ("product_id", &format!("[{product}]")),
            ("capacity", capacity),
        ],
    )
}

fn operation_rec(id: i64, name: &str, sequence: &str) -> ekg_domain::SourceRecord {
    rec(
        "mrp.routing.workcenter",
        id,
        &[
            ("bom_id", "[20]"),
            ("workcenter_id", "[1]"),
            ("name", name),
            ("sequence", sequence),
        ],
    )
}

async fn run_derivations(client: &GraphDbClient) {
    client
        .update(ontology::DERIVE_PRECEDES_SPARQL)
        .await
        .expect("run derive-precedes");
    client
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("run derive-process-edges");
}

/// The `(a, b)` pairs of `ekg:precedes`, sorted for deterministic comparison.
async fn precedes_pairs(client: &GraphDbClient) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = client
        .select(&format!(
            "PREFIX ekg: <{}> SELECT ?a ?b WHERE {{ ?a ekg:precedes ?b }}",
            ontology::VOCAB_NAMESPACE
        ))
        .await
        .expect("select precedes")
        .iter()
        .map(|r| (r["a"].clone(), r["b"].clone()))
        .collect();
    pairs.sort();
    pairs
}

/// The single derived `ekg:capacity` value on WorkCenter/1 (fails if there is not one).
async fn agent_capacity(client: &GraphDbClient) -> f64 {
    let rows = client
        .select(&format!(
            "PREFIX ekg: <{v}> SELECT ?c WHERE {{ <{wc}> ekg:capacity ?c }}",
            v = ontology::VOCAB_NAMESPACE,
            wc = ontology::entity_iri("mrp.workcenter", 1),
        ))
        .await
        .expect("select agent capacity");
    assert_eq!(
        rows.len(),
        1,
        "the agent carries exactly one derived capacity"
    );
    rows[0]["c"].parse().expect("capacity numeric")
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn process_derivations_produce_and_reclaim_edges() {
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build driver client"),
        ontology::supply_risk_contract().expect("load contract"),
    );
    for record in fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize fixture");
    }

    let client = GraphDbClient::new(&base, REPO).expect("build query client");
    run_derivations(&client).await;

    let vocab = ontology::VOCAB_NAMESPACE;
    let op = |n: i64| ontology::entity_iri("mrp.routing.workcenter", n);

    // --- precedes: the consecutive pairs only, no transitive 10 to 30 ---
    let pairs = precedes_pairs(&client).await;
    assert_eq!(
        pairs,
        vec![(op(1), op(2)), (op(2), op(3))],
        "10->20, 20->30 only"
    );

    // --- used / fulfills ---
    assert!(
        ask_true(
            &client,
            vocab,
            &format!(
                "<{}> ekg:used <{}>",
                ontology::entity_iri("mrp.production", 1),
                ontology::entity_iri("product.product", 101)
            )
        )
        .await,
        "MO used its BOM component"
    );
    assert!(
        ask_true(
            &client,
            vocab,
            &format!(
                "<{}> ekg:fulfills <{}>",
                ontology::entity_iri("mrp.production", 1),
                ontology::entity_iri("sale.order", 1)
            )
        )
        .await,
        "MO fulfills the matching sales order"
    );

    // --- capacity: the agent carries MAX(2,3) = 3 ---
    assert!(
        (agent_capacity(&client).await - 3.0).abs() < f64::EPSILON,
        "capacity is MAX = 3"
    );

    // --- reclaim: change source data and re-derive; stale edges must be gone ---
    // Move Op B to sequence 5 (the order becomes 5, 10, 30, so precedes is 5 to 10 and 10 to
    // 30) and drop the higher capacity row to 1 (MAX becomes 2). The clear before the insert
    // must remove the stale 10-to-20 and 20-to-30 edges and the stale capacity 3.
    for record in [operation_rec(2, "Op B", "5"), capacity_rec(2, 101, "1")] {
        world
            .materialize(&record)
            .await
            .expect("re-materialize mutated fixture");
    }
    run_derivations(&client).await;

    let reclaimed = precedes_pairs(&client).await;
    assert_eq!(
        reclaimed,
        vec![(op(1), op(3)), (op(2), op(1))],
        "reclaimed to 5->10, 10->30; stale 10->20 / 20->30 gone: {reclaimed:?}"
    );
    assert!(
        (agent_capacity(&client).await - 2.0).abs() < f64::EPSILON,
        "capacity reclaimed to MAX(2,1) = 2, not the stale 3"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;
}

/// Evaluate a graph pattern as a boolean. A query or transport error panics instead of
/// reading as an absent edge.
async fn ask_true(client: &GraphDbClient, vocab: &str, pattern: &str) -> bool {
    let rows = client
        .select(&format!(
            "PREFIX ekg: <{vocab}> SELECT ?x WHERE {{ {pattern} BIND(1 AS ?x) }} LIMIT 1"
        ))
        .await
        .expect("ASK-style query executes");
    !rows.is_empty()
}
