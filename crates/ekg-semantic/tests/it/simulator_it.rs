//! Live integration test for the GraphDB [`Simulator`] driver.
//!
//! Ignored by default. Run against a reachable GraphDB (e.g. `just podman-local-up`):
//!
//! ```text
//! cargo test -p ekg-semantic --test it simulator_it -- --ignored
//! ```
//!
//! Runs the what-if projection against the rule the monitor runs
//! (`PROCESS_CONTENTION_SELECT_SPARQL`, unchanged):
//!
//! - the rule fires on the materialized base (two overlapping work orders on a
//!   capacity-1 work centre);
//! - projecting a reschedule `Plan` that moves one order clear of the other clears the
//!   rule when it is re-run against the projection;
//! - the reschedule moves the work order's whole window (`ekg:startedAt` and `ekg:endedAt`
//!   by the same delta, keeping its duration), so the process's projected completion
//!   (`display::outcome`, the latest work-order end) moves with it, and is measured against
//!   the promise of the sales order the manufacturing order fulfills;
//! - the base still fires afterwards, and no `sim_*` projection repo survives.
//!
//! The base repo is provisioned SHACL-enabled (`isShacl=true` + write-shapes), like the
//! production `ekg` repo, so this test would catch the `maxCount 1` rejection a same-repo
//! projection would hit.
//!
//! Skips (does not fail) when the store is unreachable, so `just check` passes without
//! Docker.

// `expect`/`panic` are the assertion mechanism here; prose names domain terms.
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use ekg_domain::{ActionType, EntityUri, Plan, ProposedAction, Simulator, WorldModel};
use ekg_semantic::display::{self, Delivery, Outcome, WorldSource};
use ekg_semantic::{GraphDbClient, GraphDbSimulator, GraphDbWorldModel, ontology};

use crate::common;
use common::{
    graphdb_url, provision_repo, provision_shacl_repo, rec, sim_repo_count, wait_for_no_sim_repos,
};

const REPO: &str = "ekg_simulator_it";

/// A minimal contended process fixture: work centre 1 (capacity 1) performs two work
/// orders whose windows (08:00 to 12:00 and 09:00 to 13:00) overlap, so the contention
/// rule fires for both. Manufacturing order 1 need not be materialized: `ekg:part_of`
/// binds the rule's `?mo` to its IRI regardless. Manufacturing order 2 fulfills sales order
/// 42 (origin "S00042", promised 2027-10-12), so its projected completion is measured
/// against that promise; manufacturing order 1 fulfills nothing, so it falls back to
/// contention.
fn fixtures() -> Vec<ekg_domain::SourceRecord> {
    vec![
        rec(
            "sale.order",
            42,
            &[
                ("name", "S00042"),
                ("commitment_date", "2027-10-12T00:00:00"),
            ],
        ),
        rec(
            "mrp.production",
            2,
            &[("origin", "S00042"), ("workorder_ids", "[2]")],
        ),
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
    ]
}

#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn projecting_a_reschedule_clears_the_contention_without_touching_truth() {
    // Serialized with the other projection test: GraphDB Free caps repositories, and
    // the sim-repo count asserted below would see the other test's projections.
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }

    // Materialize the fixture as typed entities, then derive ekg:capacity onto the
    // work-centre agent (the contention rule reads it).
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO).expect("build driver client"),
        ontology::supply_risk_contract().expect("load contract"),
    );
    for record in fixtures() {
        world.materialize(&record).await.expect("materialize");
    }
    let client = GraphDbClient::new(&base, REPO).expect("build query client");
    client
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive capacity");

    let contention = ontology::PROCESS_CONTENTION_SELECT_SPARQL;

    // 1) The rule fires on the base: both overlapping anchors are returned.
    let base_rows = client.select(contention).await.expect("base contention");
    assert!(
        !base_rows.is_empty(),
        "the contention rule must fire on the materialized base"
    );

    let simulator = GraphDbSimulator::new(GraphDbClient::new(&base, REPO).expect("sim client"));
    let wo2 = ontology::entity_iri("mrp.workorder", 2);
    let (mo1, mo2) = (
        EntityUri(ontology::entity_iri("mrp.production", 1)),
        EntityUri(ontology::entity_iri("mrp.production", 2)),
    );
    // Work order 2's planned window as a source reads it.
    let window = |rows: Vec<std::collections::BTreeMap<String, String>>| {
        let row = rows.into_iter().next().unwrap_or_default();
        (row.get("start").cloned(), row.get("end").cloned())
    };
    let window_sparql = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?start ?end WHERE {{ <{wo2}> ekg:startedAt ?start . \
         OPTIONAL {{ <{wo2}> ekg:endedAt ?end }} }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    let current = WorldSource(&world);
    let delivery_of = |o: &Option<Outcome>| o.as_ref().map(|o| o.delivery);
    let completion_of = |o: &Option<Outcome>| o.as_ref().and_then(|o| o.completion.clone());
    let slip_of = |o: &Option<Outcome>| o.as_ref().and_then(|o| o.slip);

    // The base: MO 2 completes with work order 2 (13:00 on 5 Oct), a day before its promise;
    // MO 1 fulfills no promise, so it is judged by contention: over capacity, at risk.
    let base_mo2 = display::outcome(&current, &current, &mo2)
        .await
        .expect("base MO 2 outcome");
    assert_eq!(
        delivery_of(&base_mo2),
        Some(Delivery::OnTime),
        "{base_mo2:?}"
    );
    assert_eq!(
        completion_of(&base_mo2).as_deref(),
        Some("2027-10-11T13:00:00"),
        "{base_mo2:?}"
    );
    assert_eq!(slip_of(&base_mo2), Some(-1), "{base_mo2:?}");
    let base_mo1 = display::outcome(&current, &current, &mo1)
        .await
        .expect("base MO 1 outcome");
    assert_eq!(
        delivery_of(&base_mo1),
        Some(Delivery::AtRisk),
        "{base_mo1:?}"
    );
    assert_eq!(slip_of(&base_mo1), None, "{base_mo1:?}");

    // A reschedule of work order 2's start to `new_date`.
    let reschedule = |new_date: &str| Plan {
        actions: vec![ProposedAction {
            action_type: ActionType("reschedule_work_order".to_owned()),
            target: ekg_domain::EntityUri(ontology::entity_iri("mrp.workorder", 2)),
            params: [("new_date".to_owned(), new_date.to_owned())]
                .into_iter()
                .collect(),
        }],
    };

    // 2) Negative control: a reschedule that still overlaps work order 1 (10:00, inside
    //    its 08:00 to 12:00 window) must still fire under projection. This shows the
    //    projection reproduces the base's firing, so an under-populated projection cannot
    //    pass for a clear.
    {
        let projection = simulator
            .project(&reschedule("2027-10-11T10:00:00"))
            .await
            .expect("project non-resolving plan");
        let still = projection.select(contention).await.expect("projected fire");
        assert!(
            !still.is_empty(),
            "a plan that does not clear the overlap must still fire under projection"
        );
    }

    // 3) Project a reschedule that moves work order 2 to start when work order 1 ends
    //    (12:00, still before its own 13:00 finish), so the windows stop overlapping.
    {
        let projection = simulator
            .project(&reschedule("2027-10-11T12:00:00"))
            .await
            .expect("project resolving plan");
        // The same rule, re-run against the projection, clears.
        let projected_rows = projection.select(contention).await.expect("projected");
        assert!(
            projected_rows.is_empty(),
            "rescheduling clear of the window must clear the signal under projection: {projected_rows:?}"
        );
        // The whole window moved by +3 h, from 09:00-13:00 to 12:00-16:00 (duration kept).
        assert_eq!(
            window(projection.select(&window_sparql).await.expect("window")),
            (
                Some("2027-10-11T12:00:00".to_owned()),
                Some("2027-10-11T16:00:00".to_owned())
            ),
            "a reschedule moves startedAt AND endedAt by the same delta"
        );
        // MO 2's projected completion moves with its end; still before the promise.
        let mo2_out = display::outcome(&current, &*projection, &mo2)
            .await
            .expect("projected MO 2 outcome");
        assert_eq!(
            completion_of(&mo2_out).as_deref(),
            Some("2027-10-11T16:00:00"),
            "{mo2_out:?}"
        );
        assert_eq!(delivery_of(&mo2_out), Some(Delivery::OnTime), "{mo2_out:?}");
        // MO 1's contention cleared under projection: its fallback judgment is on time.
        let mo1_out = display::outcome(&current, &*projection, &mo1)
            .await
            .expect("projected MO 1 outcome");
        assert_eq!(delivery_of(&mo1_out), Some(Delivery::OnTime), "{mo1_out:?}");
        assert_eq!(slip_of(&mo1_out), None, "{mo1_out:?}");
    } // projection dropped here, which spawns the teardown

    // 3b) Reschedule work order 2 two days later (7 Oct 09:00): the window moves whole to
    //     09:00-13:00 on 7 Oct, still clear of work order 1, and MO 2 completes a day after
    //     its fulfilled order's promise: late by 1.
    {
        let projection = simulator
            .project(&reschedule("2027-10-13 09:00:00"))
            .await
            .expect("project a late reschedule");
        let projected_rows = projection.select(contention).await.expect("projected");
        assert!(projected_rows.is_empty(), "{projected_rows:?}");
        assert_eq!(
            window(projection.select(&window_sparql).await.expect("window")),
            (
                Some("2027-10-13T09:00:00".to_owned()),
                Some("2027-10-13T13:00:00".to_owned())
            ),
            "the duration is kept across days"
        );
        let mo2_out = display::outcome(&current, &*projection, &mo2)
            .await
            .expect("projected MO 2 outcome");
        assert_eq!(delivery_of(&mo2_out), Some(Delivery::Late), "{mo2_out:?}");
        assert_eq!(
            completion_of(&mo2_out).as_deref(),
            Some("2027-10-13T13:00:00"),
            "{mo2_out:?}"
        );
        assert_eq!(slip_of(&mo2_out), Some(1), "{mo2_out:?}");
    }

    // The base window is untouched by either projection.
    assert_eq!(
        window(client.select(&window_sparql).await.expect("base window")),
        (
            Some("2027-10-11T09:00:00".to_owned()),
            Some("2027-10-11T13:00:00".to_owned())
        ),
        "the projection never touched the base"
    );

    // 4) The base still fires (nothing authoritative changed),
    let base_again = client.select(contention).await.expect("base after");
    assert_eq!(
        base_again.len(),
        base_rows.len(),
        "the base contention result is unchanged by the projection"
    );

    // and no sim_* projection repo survives (poll: teardown is spawned on drop).
    let mut remaining = sim_repo_count(&http, &base).await;
    for _ in 0..50 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        remaining = sim_repo_count(&http, &base).await;
    }
    assert_eq!(
        remaining, 0,
        "the ephemeral projection repo must be dropped"
    );

    // Clean up the throwaway base repo.
    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO}"))
        .send()
        .await;
}

const REPO_EXPEDITE: &str = "ekg_simulator_expedite_it";

/// A sale order short of a component whose only inbound line arrives after the
/// commitment through an open receipt:
/// order/5 (10 Bikes, due 2027-10-22) needs 10 Packs; none on hand; PO line/141 plans
/// 10 for 2027-11-15 and its PO's open receipt/143 is scheduled the same day.
fn expedite_fixtures() -> Vec<ekg_domain::SourceRecord> {
    vec![
        rec(
            "sale.order",
            5,
            &[
                ("commitment_date", "2027-10-22T00:00:00"),
                ("order_line", "[150]"),
            ],
        ),
        rec(
            "sale.order.line",
            150,
            &[("product_id", "[130]"), ("product_uom_qty", "10")],
        ),
        rec("product.product", 130, &[("name", "Bike")]),
        rec("product.product", 131, &[("name", "Pack")]),
        rec(
            "mrp.bom",
            30,
            &[("product_id", "[130]"), ("bom_line_ids", "[151]")],
        ),
        rec(
            "mrp.bom.line",
            151,
            &[("product_id", "[131]"), ("product_qty", "1")],
        ),
        rec(
            "stock.quant",
            152,
            &[("product_id", "[131]"), ("quantity", "0")],
        ),
        rec(
            "purchase.order.line",
            141,
            &[
                ("product_id", "[131]"),
                ("product_qty", "10"),
                ("date_planned", "2027-11-15T08:00:00"),
            ],
        ),
        rec("purchase.order", 142, &[("order_line", "[141]")]),
        rec(
            "stock.picking",
            143,
            &[
                ("scheduled_date", "2027-11-15T08:00:00"),
                ("purchase_id", "[142]"),
                ("state", "assigned"),
            ],
        ),
    ]
}

/// Projecting an expedite of the cause line to before the commitment clears the
/// risk-rule SELECT, because the projection moves the open receipt's ETA with the line
/// (the rule prefers it). An expedite still after the commitment keeps firing, and a
/// sale-order-targeted expedite is refused, so it can never clear.
#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn projecting_an_expedite_of_the_cause_line_clears_the_shortfall() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, REPO_EXPEDITE).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_EXPEDITE).expect("build driver client"),
        ontology::supply_risk_contract().expect("load contract"),
    );
    for record in expedite_fixtures() {
        world.materialize(&record).await.expect("materialize");
    }
    let client = GraphDbClient::new(&base, REPO_EXPEDITE).expect("build query client");
    let order = ontology::entity_iri("sale.order", 5);
    let flags = |rows: &[std::collections::BTreeMap<String, String>]| {
        rows.iter()
            .any(|r| r.get("so").map(String::as_str) == Some(order.as_str()))
    };
    let rule = ontology::RISK_RULE_SELECT_SPARQL;

    // The rule fires on the base: the only inbound arrives after the promise.
    let base_rows = client.select(rule).await.expect("base rule");
    assert!(
        flags(&base_rows),
        "order/5 is at risk on the base: {base_rows:?}"
    );

    let simulator =
        GraphDbSimulator::new(GraphDbClient::new(&base, REPO_EXPEDITE).expect("sim client"));
    let expedite = |target: String, new_date: &str| Plan {
        actions: vec![ProposedAction {
            action_type: ActionType("expedite_po".to_owned()),
            target: ekg_domain::EntityUri(target),
            params: [("new_date".to_owned(), new_date.to_owned())]
                .into_iter()
                .collect(),
        }],
    };
    let line = ontology::entity_iri("purchase.order.line", 141);

    // Negative control: expediting to a date still after the promise keeps firing.
    let late = {
        let projection = simulator
            .project(&expedite(line.clone(), "2027-10-26 08:00:00"))
            .await
            .expect("project a late expedite");
        projection.select(rule).await.expect("projected rule")
    };
    // Expediting the cause line to before the promise clears it.
    let on_time = {
        let projection = simulator
            .project(&expedite(line, "2027-10-12 08:00:00"))
            .await
            .expect("project an on-time expedite");
        projection.select(rule).await.expect("projected rule")
    };
    // An expedite targeted at the sale order is refused outright.
    let mistargeted = simulator
        .project(&expedite(order.clone(), "2027-10-12 08:00:00"))
        .await
        .map(|_| ());
    let base_after = client.select(rule).await.expect("base after");
    // A projection that fails to build leaves no repo behind, as the two before it.
    let remaining = wait_for_no_sim_repos(&http, &base).await;

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_EXPEDITE}"))
        .send()
        .await;

    assert_eq!(remaining, 0, "a refused projection leaves no repo behind");

    assert!(
        flags(&late),
        "a still-late expedite does not clear: {late:?}"
    );
    assert!(
        !flags(&on_time),
        "an on-time expedite of the cause line clears the shortfall: {on_time:?}"
    );
    assert!(
        mistargeted.is_err(),
        "an expedite targeting the sale order is refused, never scored"
    );
    assert!(flags(&base_after), "the projection never touched the base");
}

const REPO_CONCURRENT: &str = "ekg_simulator_concurrent_it";

/// GraphDB Free allows 5 repositories, and past that every concurrent import fails, not only
/// the extra one. The read path projects concurrently (each option's metrics and lanes, each
/// awaiting point's recommended option), so every simulator in the process shares one permit
/// pool: five concurrent projections from two simulator instances all succeed (queued, never
/// failed) and none leaves its repo behind.
#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn concurrent_projections_queue_within_the_repository_cap() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, REPO_CONCURRENT).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_CONCURRENT).expect("build driver client"),
        ontology::supply_risk_contract().expect("load contract"),
    );
    for record in fixtures() {
        world.materialize(&record).await.expect("materialize");
    }
    GraphDbClient::new(&base, REPO_CONCURRENT)
        .expect("build query client")
        .update(ontology::DERIVE_PROCESS_EDGES_SPARQL)
        .await
        .expect("derive capacity");

    let simulators = [
        GraphDbSimulator::new(GraphDbClient::new(&base, REPO_CONCURRENT).expect("sim client")),
        GraphDbSimulator::new(GraphDbClient::new(&base, REPO_CONCURRENT).expect("sim client")),
    ];
    let reschedule = |hour: u32| Plan {
        actions: vec![ProposedAction {
            action_type: ActionType("reschedule_work_order".to_owned()),
            target: EntityUri(ontology::entity_iri("mrp.workorder", 2)),
            params: [(
                "new_date".to_owned(),
                format!("2027-10-11 {:02}:00:00", 12 + hour),
            )]
            .into_iter()
            .collect(),
        }],
    };
    let results = futures::future::join_all((0..5_u32).map(|i| {
        let simulator = &simulators[i as usize % 2];
        let plan = reschedule(i);
        async move {
            let projection = simulator.project(&plan).await?;
            // Hold the projection across a read, as a resolver does.
            projection
                .select(ontology::PROCESS_CONTENTION_SELECT_SPARQL)
                .await
                .map(|rows| rows.len())
        }
    }))
    .await;

    let mut remaining = sim_repo_count(&http, &base).await;
    for _ in 0..50 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        remaining = sim_repo_count(&http, &base).await;
    }
    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_CONCURRENT}"))
        .send()
        .await;

    let failures: Vec<String> = results
        .iter()
        .filter_map(|r| r.as_ref().err().map(ToString::to_string))
        .collect();
    assert!(
        failures.is_empty(),
        "every concurrent projection must succeed (queued within the cap): {failures:?}"
    );
    assert_eq!(remaining, 0, "no projection repo survives");
}

const REPO_ABANDONED: &str = "ekg_simulator_abandoned_it";

/// A projection abandoned while it is being built (a timeout, a cancelled request) drops its
/// repo and frees its permit. Nine projections, more than the permit pool admits, are each
/// abandoned after a different delay, so some are dropped while waiting for a permit, some
/// mid-setup and some just after they completed; none leaves a repo behind, and a following
/// projection is still admitted and completes.
#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn abandoned_projections_leave_no_repo_and_free_their_permits() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, REPO_ABANDONED).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_ABANDONED).expect("build driver client"),
        ontology::supply_risk_contract().expect("load contract"),
    );
    for record in fixtures() {
        world.materialize(&record).await.expect("materialize");
    }
    let simulator =
        GraphDbSimulator::new(GraphDbClient::new(&base, REPO_ABANDONED).expect("sim client"));
    let plan = Plan {
        actions: vec![ProposedAction {
            action_type: ActionType("reschedule_work_order".to_owned()),
            target: EntityUri(ontology::entity_iri("mrp.workorder", 2)),
            params: [("new_date".to_owned(), "2027-10-11 14:00:00".to_owned())]
                .into_iter()
                .collect(),
        }],
    };

    let delays_ms = [0_u64, 1, 3, 8, 20, 50, 120, 300, 800];
    let abandoned = futures::future::join_all(delays_ms.map(|ms| {
        let (simulator, plan) = (&simulator, &plan);
        async move {
            tokio::time::timeout(
                std::time::Duration::from_millis(ms),
                simulator.project(plan),
            )
            .await
            .is_err()
        }
    }))
    .await;
    let remaining = wait_for_no_sim_repos(&http, &base).await;
    // Every permit came back: a following projection is admitted and completes.
    let following =
        tokio::time::timeout(std::time::Duration::from_secs(30), simulator.project(&plan))
            .await
            .map(|projected| projected.map(|_| ()));
    let after_following = wait_for_no_sim_repos(&http, &base).await;

    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_ABANDONED}"))
        .send()
        .await;

    assert!(
        abandoned.iter().any(|timed_out| *timed_out),
        "at least one projection was abandoned before it completed: {abandoned:?}"
    );
    assert_eq!(
        remaining, 0,
        "an abandoned projection leaves no repo behind"
    );
    assert!(
        matches!(following, Ok(Ok(()))),
        "a projection after the abandoned ones is admitted and completes: {following:?}"
    );
    assert_eq!(
        after_following, 0,
        "the following projection's repo is dropped"
    );
}

const REPO_SWEEP: &str = "ekg_simulator_sweep_it";

/// The stale sweep drops a sim repo minted longer ago than the maximum age (what a killed
/// process leaves behind), and leaves alone a young sim repo (another process's live
/// projection) and every repository that is not a sim repo.
#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn the_stale_sweep_drops_only_old_sim_repos() {
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_repo(&http, &base, REPO_SWEEP).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("fixture repo provisioning failed: {e}"),
    }
    let client = GraphDbClient::new(&base, REPO_SWEEP).expect("build client");
    let max_age = std::time::Duration::from_secs(900);
    let minted = |age: std::time::Duration| {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .saturating_sub(age);
        format!("sim_{}_{}_0", std::process::id(), at.as_nanos())
    };
    let stale = minted(max_age + std::time::Duration::from_secs(60));
    let young = minted(std::time::Duration::from_secs(60));
    client.create_scratch_repo(&stale).await.expect("old repo");
    client
        .create_scratch_repo(&young)
        .await
        .expect("young repo");

    let simulator = GraphDbSimulator::new(GraphDbClient::new(&base, REPO_SWEEP).expect("client"));
    let reclaimed = simulator.sweep_stale(max_age).await;
    let listed = client.repository_ids().await;

    for repo in [young.as_str(), REPO_SWEEP] {
        let _ = client.drop_repo(repo).await;
    }

    let reclaimed = reclaimed.expect("the sweep lists the repositories");
    let listed = listed.expect("list repositories");
    assert!(
        reclaimed.contains(&stale),
        "the stale sim repo is reclaimed: {reclaimed:?}"
    );
    assert!(
        !reclaimed.contains(&young),
        "the young sim repo is not reclaimed: {reclaimed:?}"
    );
    assert!(
        !listed.contains(&stale),
        "the stale repo is gone: {listed:?}"
    );
    assert!(
        listed.contains(&young),
        "the young repo is kept: {listed:?}"
    );
    assert!(
        listed.iter().any(|id| id == REPO_SWEEP),
        "a repository that is not a sim repo is kept: {listed:?}"
    );
}

/// A plan holding a split or a capacity raise re-runs the structural derivations inside the
/// sim repo, so
/// - a capacity raise from 1 to 2 on the shared work centre's record clears the contention
///   rule's over-capacity rows (the agent's derived capacity follows the record);
/// - a split's lot `ekg:fulfills` the original's sales order, with its work orders planned
///   from `new_date`;
/// - a plan with neither verb projects without re-derivation.
#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn a_split_or_a_capacity_raise_is_re_derived_in_the_projection() {
    const SPLIT_REPO: &str = "ekg_simulator_split_it";
    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, SPLIT_REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, SPLIT_REPO).expect("driver client"),
        ontology::supply_risk_contract().expect("contract"),
    );
    let mut records = fixtures();
    // MO 2 carries a quantity and a planned window, so it can be split.
    records.push(rec(
        "mrp.production",
        2,
        &[
            ("origin", "S00042"),
            ("workorder_ids", "[2]"),
            ("product_qty", "40"),
            ("date_start", "2027-10-11T09:00:00"),
            ("date_finished", "2027-10-11T13:00:00"),
        ],
    ));
    for record in records {
        world.materialize(&record).await.expect("materialize");
    }
    let client = GraphDbClient::new(&base, SPLIT_REPO).expect("query client");
    for derivation in ontology::STRUCTURAL_DERIVATIONS {
        client.update(derivation.sparql).await.expect("derive");
    }
    let contention = ontology::PROCESS_CONTENTION_SELECT_SPARQL;
    let simulator =
        GraphDbSimulator::new(GraphDbClient::new(&base, SPLIT_REPO).expect("sim client"));
    let plan = |verb: &str, target: String, params: &[(&str, &str)]| Plan {
        actions: vec![ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: EntityUri(target),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }],
    };
    let vocab = ontology::VOCAB_NAMESPACE;
    let so = ontology::entity_iri("sale.order", 42);
    let lot = ontology::entity_iri("mrp.production", 1_000_000_000_002);
    let lot_sparql = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?qty ?wo ?start WHERE {{ <{lot}> ekg:fulfills <{so}> ; ekg:product_qty ?qty . \
         OPTIONAL {{ ?wo ekg:part_of <{lot}> ; ekg:startedAt ?start }} }}"
    );
    let capacity_sparql = format!(
        "PREFIX ekg: <{vocab}>\nSELECT ?cap WHERE {{ ?wc a ekg:WorkCenter ; ekg:capacity ?cap }}"
    );

    let run = async || {
        let base_rows = client.select(contention).await.expect("base contention");

        let raised = simulator
            .project(&plan(
                "raise_work_center_capacity",
                ontology::entity_iri("mrp.workcenter.capacity", 1),
                &[("new_capacity", "2")],
            ))
            .await
            .expect("project the raise");
        let raised_rows = raised.select(contention).await.expect("raised contention");
        let raised_cap = raised.select(&capacity_sparql).await.expect("capacity");
        drop(raised);

        let split = simulator
            .project(&plan(
                "split_production",
                ontology::entity_iri("mrp.production", 2),
                &[
                    ("split_quantity", "10"),
                    ("new_date", "2027-10-13 08:00:00"),
                ],
            ))
            .await
            .expect("project the split");
        let lot_rows = split.select(&lot_sparql).await.expect("lot");
        drop(split);

        let untouched = simulator
            .project(&Plan {
                actions: Vec::new(),
            })
            .await
            .expect("project nothing");
        let untouched_rows = untouched.select(contention).await.expect("contention");
        drop(untouched);
        (base_rows, raised_rows, raised_cap, lot_rows, untouched_rows)
    };
    let (base_rows, raised_rows, raised_cap, lot_rows, untouched_rows) = run().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = http
        .delete(format!("{base}/rest/repositories/{SPLIT_REPO}"))
        .send()
        .await;

    // Over capacity as the rule and the scorer judge it (`ontology::over_capacity`).
    let over = |rows: &[std::collections::BTreeMap<String, String>]| {
        rows.iter()
            .filter(|row| {
                let capacity = row.get("capacity").and_then(|c| c.parse::<f64>().ok());
                let peers = row.get("overlapPeers").and_then(|p| p.parse::<i64>().ok());
                capacity
                    .zip(peers)
                    .and_then(|(c, p)| ontology::over_capacity(c, p))
                    == Some(true)
            })
            .count()
    };
    assert_eq!(over(&base_rows), 2, "the base contends: {base_rows:?}");
    assert_eq!(
        over(&raised_rows),
        0,
        "a capacity of 2 covers the two overlapping work orders: {raised_rows:?}"
    );
    assert_eq!(
        raised_cap
            .iter()
            .filter_map(|r| r.get("cap").and_then(|c| c.parse::<f64>().ok()))
            .collect::<Vec<_>>(),
        [2.0],
        "the agent's derived capacity follows the record"
    );
    assert_eq!(lot_rows.len(), 1, "the lot fulfils the order: {lot_rows:?}");
    assert_eq!(
        lot_rows[0].get("qty").and_then(|q| q.parse::<f64>().ok()),
        Some(10.0)
    );
    assert_eq!(
        lot_rows[0].get("start").map(String::as_str),
        Some("2027-10-13T08:00:00"),
        "{lot_rows:?}"
    );
    assert_eq!(
        over(&untouched_rows),
        over(&base_rows),
        "a plan with neither verb projects as before"
    );
}

const REPO_TRACED: &str = "ekg_simulator_traced_it";

/// A projection made for a traced operation is part of its trace: the scratch repository is
/// created from a task of its own, and that request is still a span of the operation.
#[tokio::test]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn a_projection_made_under_a_span_exports_its_store_requests_in_that_trace() {
    use opentelemetry::trace::{FutureExt as _, TraceContextExt as _, Tracer as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    let _slot = common::fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let http = reqwest::Client::new();
    match provision_shacl_repo(&http, &base, REPO_TRACED).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: no GraphDB reachable at {base} (start the substrate)");
            return;
        }
        Err(e) => panic!("SHACL base repo provisioning failed: {e}"),
    }
    let world = GraphDbWorldModel::new(
        GraphDbClient::new(&base, REPO_TRACED).expect("build driver client"),
        ontology::supply_risk_contract().expect("load contract"),
    );
    for record in fixtures() {
        world.materialize(&record).await.expect("materialize");
    }
    let exporter = InMemorySpanExporter::default();
    opentelemetry::global::set_tracer_provider(
        SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build(),
    );
    let simulator =
        GraphDbSimulator::new(GraphDbClient::new(&base, REPO_TRACED).expect("sim client"));
    let plan = Plan {
        actions: vec![ProposedAction {
            action_type: ActionType("reschedule_work_order".to_owned()),
            target: EntityUri(ontology::entity_iri("mrp.workorder", 2)),
            params: [("new_date".to_owned(), "2027-10-11 13:00:00".to_owned())]
                .into_iter()
                .collect(),
        }],
    };

    let tracer = opentelemetry::global::tracer("suite");
    let operation = opentelemetry::Context::new().with_span(tracer.start("operation"));
    let (trace, operation_span) = {
        let span = operation.span();
        (
            span.span_context().trace_id(),
            span.span_context().span_id(),
        )
    };
    let projected = async {
        let projection = simulator.project(&plan).await?;
        projection
            .select(ontology::PROCESS_CONTENTION_SELECT_SPARQL)
            .await
    }
    .with_context(operation.clone())
    .await;
    operation.span().end();
    wait_for_no_sim_repos(&http, &base).await;
    let _ = http
        .delete(format!("{base}/rest/repositories/{REPO_TRACED}"))
        .send()
        .await;
    projected.expect("the projection");

    let spans: Vec<_> = exporter
        .get_finished_spans()
        .expect("spans")
        .into_iter()
        .filter(|span| span.span_context.trace_id() == trace)
        .collect();
    let named = |name: &str| spans.iter().filter(|span| span.name == name).count();
    assert!(
        named("graphdb create_repository") >= 1,
        "the spawned create request is in the trace: {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );
    assert!(named("graphdb select") >= 1, "the read of the projection");
    for span in spans
        .iter()
        .filter(|span| span.name.starts_with("graphdb "))
    {
        assert_eq!(
            span.parent_span_id, operation_span,
            "{} is a child of the operation",
            span.name
        );
        let text = format!("{:?}", span.attributes);
        assert!(
            !text.contains("SELECT") && !text.contains("INSERT"),
            "{text}"
        );
    }
}
