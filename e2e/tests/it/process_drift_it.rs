//! Gated live integration test: the two process-drift rules
//! (`process_contention_rule`, `process_infeasibility_rule`) fire over typed
//! process entities materialized through the mapping contract, running each rule's
//! SELECT and interpreter against a live GraphDB, like `ekg-semantic`'s
//! `risk_rule_it.rs`.
//!
//! Ignored by default. Run against a reachable GraphDB, e.g. the local stack
//! (`just podman-local-up`, which supplies the license), with:
//!
//! ```text
//! cargo test -p ekg-e2e --test it process_drift_it -- --ignored
//! ```
//!
//! Contention: two work orders on one capacity-1 work center have overlapping
//! windows, so the affected manufacturing orders are flagged with a reason naming
//! the capacity; a lone work order on a second work center is not. Infeasibility: a
//! manufacturing order planned to finish after the commitment it fulfils is flagged;
//! one that finishes before its commitment is not. If the store is unreachable the
//! test skips.

// `expect` and `panic` are the assertion mechanism here; prose names domain terms.
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use ekg_backend::drift::{process_contention_rule, process_infeasibility_rule};
use ekg_domain::{SourceRecord, WorldModel};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

use crate::common;
use common::{graphdb_url, provision_repo, rec};

const REPO: &str = "ekg_process_drift_it";

/// Contention: work center 1 (capacity 1) performs two work orders whose day-5
/// windows overlap (08:00 to 12:00 and 09:00 to 13:00), so concurrency is 2 > 1 and
/// both their manufacturing orders (1, 2) are contended. Work center 2 (capacity 1)
/// performs a single work order (order 3) with no peer, so it is within capacity
/// (the control).
///
/// Infeasibility: order 10 fulfils SO-INFEAS (commitment 10-05) but is planned to
/// finish 10-10, so it misses its promise. Order 11 fulfils SO-OK (commitment 10-20)
/// and finishes 10-06, within the promise, so it is not flagged (the control). Both
/// `ekg:capacity` (contention) and the `ekg:fulfills` link to the `commitment`
/// (infeasibility) are derived by `DERIVE_PROCESS_EDGES_SPARQL` from the fixtures.
fn fixtures() -> Vec<SourceRecord> {
    vec![
        // --- contention: work center 1 (capacity 1), two overlapping work orders ---
        rec("mrp.workcenter", 1, &[("name", "Shared Line")]),
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
            "mrp.workorder",
            1,
            &[
                ("production_id", "[1]"),
                ("workcenter_id", "[1]"),
                ("date_start", "2027-10-11T08:00:00"),
                ("date_finished", "2027-10-11T12:00:00"),
                ("duration_expected", "240"),
            ],
        ),
        rec(
            "mrp.workorder",
            2,
            &[
                ("production_id", "[2]"),
                ("workcenter_id", "[1]"),
                ("date_start", "2027-10-11T09:00:00"),
                ("date_finished", "2027-10-11T13:00:00"),
                ("duration_expected", "240"),
            ],
        ),
        // --- contention control: work center 2 (capacity 1), one lone work order ---
        rec("mrp.workcenter", 2, &[("name", "Idle Line")]),
        rec(
            "mrp.workcenter.capacity",
            2,
            &[
                ("workcenter_id", "[2]"),
                ("product_id", "[900]"),
                ("capacity", "1"),
            ],
        ),
        rec(
            "mrp.workorder",
            3,
            &[
                ("production_id", "[3]"),
                ("workcenter_id", "[2]"),
                ("date_start", "2027-10-11T08:00:00"),
                ("date_finished", "2027-10-11T12:00:00"),
                ("duration_expected", "240"),
            ],
        ),
        // --- infeasibility: order 10 fulfils SO-INFEAS (commitment 10-05) but is
        //     planned to finish 10-10, so it is flagged ---
        rec(
            "sale.order",
            50,
            &[
                ("name", "SO-INFEAS"),
                ("commitment_date", "2027-10-11T00:00:00"),
            ],
        ),
        rec(
            "mrp.production",
            10,
            &[
                ("bom_id", "[50]"),
                ("origin", "SO-INFEAS"),
                ("date_start", "2027-10-11T08:00:00"),
                ("date_finished", "2027-10-16T16:00:00"),
            ],
        ),
        rec(
            "mrp.routing.workcenter",
            60,
            &[
                ("bom_id", "[50]"),
                ("sequence", "10"),
                ("time_cycle_manual", "180"),
            ],
        ),
        // --- infeasibility control: order 11 fulfils SO-OK (commitment 10-20) and is
        //     planned to finish 10-06, so it is not flagged ---
        rec(
            "sale.order",
            51,
            &[
                ("name", "SO-OK"),
                ("commitment_date", "2027-10-26T00:00:00"),
            ],
        ),
        rec(
            "mrp.production",
            11,
            &[
                ("bom_id", "[51]"),
                ("origin", "SO-OK"),
                ("date_start", "2027-10-11T08:00:00"),
                ("date_finished", "2027-10-12T16:00:00"),
            ],
        ),
        rec(
            "mrp.routing.workcenter",
            61,
            &[
                ("bom_id", "[51]"),
                ("sequence", "10"),
                ("time_cycle_manual", "180"),
            ],
        ),
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn process_drift_rules_fire_on_live_typed_entities() {
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

    // Materialize every fixture as a typed entity through the mapping contract.
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build driver client"),
        ontology::supply_risk_contract().expect("load supply-risk contract"),
    );
    for record in fixtures() {
        world
            .materialize(&record)
            .await
            .expect("materialize typed fixture");
    }

    // Derive ekg:capacity onto the work-center agents (contention reads it).
    let client = GraphDbClient::new(&base, REPO).expect("build query client");
    client
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive process edges (capacity)");

    let mo = |n: i64| ontology::entity_iri("mrp.production", n);

    // --- contention: the two orders on the shared line are flagged; the lone one is not ---
    let contention = process_contention_rule();
    let rows = client
        .select(contention.sparql())
        .await
        .expect("run contention SELECT");
    let verdicts = contention.verdicts(&rows);
    let contended: Vec<&str> = verdicts.iter().map(|v| v.subject.0.as_str()).collect();
    assert!(
        contended.contains(&mo(1).as_str()) && contended.contains(&mo(2).as_str()),
        "both orders sharing the over-capacity line are contended: {contended:?}"
    );
    assert!(
        !contended.contains(&mo(3).as_str()),
        "the lone work order on the second line is within capacity (control)"
    );
    // The reason names the capacity and the work center.
    let reason = &verdicts
        .iter()
        .find(|v| v.subject.0 == mo(1))
        .expect("order 1 has a contention verdict")
        .reason;
    assert!(
        reason.contains("contention"),
        "reason names the drift: {reason}"
    );
    assert!(
        reason.contains("capacity"),
        "reason is capacity-attributed: {reason}"
    );
    assert!(
        reason.contains(&ontology::entity_iri("mrp.workcenter", 1)),
        "reason names the contended work center: {reason}"
    );

    // --- infeasibility: the order that misses its commitment is flagged; the one that
    //     finishes before its commitment is not ---
    let infeasibility = process_infeasibility_rule();
    let rows = client
        .select(infeasibility.sparql())
        .await
        .expect("run infeasibility SELECT");
    let verdicts = infeasibility.verdicts(&rows);
    let infeasible: Vec<&str> = verdicts.iter().map(|v| v.subject.0.as_str()).collect();
    assert!(
        infeasible.contains(&mo(10).as_str()),
        "the order planned to finish after its commitment is flagged: {infeasible:?}"
    );
    assert!(
        !infeasible.contains(&mo(11).as_str()),
        "the order that finishes before its commitment is not flagged (control)"
    );
    let reason = &verdicts
        .iter()
        .find(|v| v.subject.0 == mo(10))
        .expect("order 10 has an infeasibility verdict")
        .reason;
    assert!(
        reason.contains("infeasibility"),
        "reason names the drift: {reason}"
    );
    assert!(
        reason.contains("finish") && reason.contains("commitment"),
        "reason states the planned finish vs the commitment: {reason}"
    );

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;
}
