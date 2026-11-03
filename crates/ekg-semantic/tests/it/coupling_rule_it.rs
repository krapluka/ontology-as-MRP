//! Gated live integration test: the coupling SELECT ([`ontology::COUPLING_SELECT_SPARQL`]) over
//! a throwaway GraphDB fixture yields the couplings the monitor groups by (a sales order
//! and the manufacturing order fulfilling it, and two orders contending for a work center
//! beyond its capacity) and nothing for orders that share a work center within its capacity.
//!
//! Ignored by default. Run against a reachable GraphDB (e.g. `just podman-local-up`):
//!
//! ```text
//! cargo test -p ekg-semantic --test it coupling_rule_it -- --ignored
//! ```
//!
//! The rows are interpreted as the capability's interpreter does: a `fulfills` row
//! always couples, a `contention` row only when [`ontology::over_capacity`] holds (the shared
//! predicate), and each coupling is undirected. Skips if the store is unreachable, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set, when it fails.

#![allow(clippy::expect_used, clippy::doc_markdown)]

use std::collections::BTreeSet;

use ekg_domain::WorldModel;
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

use crate::common;
use common::{fixture_repo_slot, graphdb_url, infra_unavailable, provision_repo, rec};

const REPO: &str = "ekg_coupling_rule_it";

/// A work order of `mo` on work center `wc` over `[start, end)` hours of one day.
fn work_order(id: i64, mo: i64, wc: i64, start: u8, end: u8) -> ekg_domain::SourceRecord {
    rec(
        "mrp.workorder",
        id,
        &[
            ("production_id", &format!("[{mo}]")),
            ("workcenter_id", &format!("[{wc}]")),
            ("date_start", &format!("2027-10-23T{start:02}:00:00")),
            ("date_finished", &format!("2027-10-23T{end:02}:00:00")),
            ("duration_expected", "60"),
        ],
    )
}

/// The fixture:
/// - MO/1 fulfils SO/1 (`origin` = the order's `name`);
/// - on the Assembly Line (WC/1, capacity 1) MO/1's work order 08-12 overlaps MO/2's 09-11:
///   concurrency 2 > 1, so MO/1 and MO/2 contend;
/// - MO/3 shares the Assembly Line at 14-16 (no overlap) and runs with MO/4 on the Paint
///   Booth (WC/2, capacity 2) at overlapping times: within capacity, so no coupling.
fn fixtures() -> Vec<ekg_domain::SourceRecord> {
    vec![
        rec("sale.order", 1, &[("name", "S00001")]),
        rec("mrp.production", 1, &[("origin", "S00001")]),
        rec("mrp.production", 2, &[("origin", "S00077")]),
        rec("mrp.production", 3, &[("origin", "S00078")]),
        rec("mrp.production", 4, &[("origin", "S00079")]),
        rec("product.product", 900, &[("name", "E-Bike")]),
        rec("mrp.workcenter", 1, &[("name", "Assembly Line")]),
        rec("mrp.workcenter", 2, &[("name", "Paint Booth")]),
        rec(
            "mrp.workcenter.capacity",
            1,
            &[
                ("workcenter_id", "[1]"),
                ("product_id", "[900]"),
                ("capacity", "1"),
            ],
        ),
        rec(
            "mrp.workcenter.capacity",
            2,
            &[
                ("workcenter_id", "[2]"),
                ("product_id", "[900]"),
                ("capacity", "2"),
            ],
        ),
        work_order(11, 1, 1, 8, 12),
        work_order(12, 2, 1, 9, 11),
        work_order(13, 3, 1, 14, 16),
        work_order(31, 3, 2, 8, 10),
        work_order(41, 4, 2, 9, 11),
    ]
}

/// The undirected couplings a row set yields under the capability's interpretation, as
/// `(kind, lesser end, greater end)`.
fn couplings(
    rows: &[std::collections::BTreeMap<String, String>],
) -> BTreeSet<(String, String, String)> {
    rows.iter()
        .filter_map(|row| {
            let (a, b, kind) = (row.get("a")?, row.get("b")?, row.get("kind")?);
            if kind == "contention" {
                let capacity: f64 = row.get("capacity")?.parse().ok()?;
                let peers: i64 = row.get("overlapPeers")?.parse().ok()?;
                if !ontology::over_capacity(capacity, peers)? {
                    return None;
                }
            }
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            Some((kind.clone(), lo.clone(), hi.clone()))
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn coupling_links_fulfilment_and_over_capacity_contention_only() {
    let _slot = fixture_repo_slot().await.expect("fixture slot");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            infra_unavailable(&format!("no GraphDB reachable at {base}"));
            return;
        }
        Err(e) => panic!("throwaway repo provisioning failed: {e}"),
    }

    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("driver client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    for record in fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize fixture");
    }
    let client = GraphDbClient::new(&base, REPO).expect("query client");
    for derivation in ontology::STRUCTURAL_DERIVATIONS {
        client.update(derivation.sparql).await.expect("derive");
    }
    let rows = client
        .select(ontology::COUPLING_SELECT_SPARQL)
        .await
        .expect("coupling select");

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;

    let so = ontology::entity_iri("sale.order", 1);
    let mo = |n: i64| ontology::entity_iri("mrp.production", n);
    let got = couplings(&rows);
    let expected = BTreeSet::from([
        ("fulfills".to_owned(), mo(1), so),
        ("contention".to_owned(), mo(1), mo(2)),
    ]);
    assert_eq!(got, expected, "rows: {rows:?}");
    // The within-capacity orders do appear as contention rows; the interpreter, not the
    // SPARQL, keeps them apart.
    assert!(
        rows.iter().any(|r| r.get("a") == Some(&mo(3))
            && r.get("kind").map(String::as_str) == Some("contention")),
        "the within-capacity overlap reaches the interpreter: {rows:?}"
    );
    for n in [3, 4] {
        assert!(
            got.iter().all(|(_, a, b)| *a != mo(n) && *b != mo(n)),
            "MO/{n} shares a center within capacity and is coupled to nothing"
        );
    }
}
