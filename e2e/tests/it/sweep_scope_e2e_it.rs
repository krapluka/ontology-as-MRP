//! Gated live end-to-end test: a record outside its model's source scope that is already
//! materialized is retracted by a real reconciliation pass, while the in-scope record stays.
//!
//! Self-contained: a throwaway product given a non-zero inventory adjustment at `WH/Stock` yields
//! one internal quant and one counterpart quant at the virtual `Inventory adjustment` location
//! (the shared `test-support` quant fixture). Both are materialized directly (as an unscoped
//! ingest would have), then a `Sweep` pass under an isolated key, with the contract's scopes,
//! diffs materialized quants against the scoped `current_ids`. The pass's retraction events are
//! observed on the publisher, so a wrong retraction of the internal quant cannot hide behind the
//! same pass re-ingesting it.
//!
//! Like the other `Sweep` force-claimers, it binds `Ingest` and `Sweep` and force-registers
//! them through `common::with_sweep_claimed`, which pauses the stack's own sweep timer
//! meanwhile and restores everything afterwards, a panic included.
//!
//! ```text
//! just podman-local-up
//! EKG_E2E_REQUIRE_INFRA=1 cargo test -p ekg-e2e --test it sweep_scope_e2e_it -- --ignored
//! ```
//!
//! Skips when a hop is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as
//! `just test-e2e` does), in which case it fails.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::odoo_fixture::QuantFixture;
use ekg_backend::InProcessPublisher;
use ekg_backend::sweep::{SWEEP_HANDLER, SWEEP_OBJECT, Sweep};
use ekg_backend::worker::{INGEST_OBJECT, Ingest};
use ekg_domain::{ChangeKind, SourceIngestor, SourceRef, WorldModel};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use tokio::sync::broadcast::error::TryRecvError;

const MODEL: &str = "stock.quant";

/// Room for every event of one pass: an upsert per in-scope quant of the shared Odoo it
/// re-ingests, plus the retraction. A lagging receiver fails the test (never a silent miss).
const EVENT_CAPACITY: usize = 16_384;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn reconciliation_retracts_an_already_materialized_out_of_scope_record() {
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = common::env_or("ODOO_DB", "ekg");
    let odoo_user = common::env_or("ODOO_USER", "admin");
    let odoo_key = common::env_or("ODOO_API_KEY", "admin");
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(14);
    let callback_url = common::callback_url(14);
    let http = reqwest::Client::new();

    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        common::infra_unavailable(&format!("Restate admin unreachable at {admin_url}"));
        return;
    }
    let repo = "ekg_sweep_scope_it";
    match common::provision_repo(&http, &graphdb_url, repo).await {
        Ok(true) => {}
        Ok(false) => {
            common::infra_unavailable(&format!("GraphDB unreachable at {graphdb_url}"));
            return;
        }
        Err(why) => {
            common::infra_unavailable(&format!("GraphDB not ready: {why}"));
            return;
        }
    }
    let odoo = {
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
                common::infra_unavailable(&format!("Odoo unreachable: {e}"));
                return;
            }
        }
    };
    let config = |scopes| OdooConfig {
        base_url: odoo_url.clone(),
        db: odoo_db.clone(),
        login: odoo_user.clone(),
        password: odoo_key.clone(),
        system: "odoo".to_owned(),
        allowlist: vec![MODEL.to_owned()],
        scopes,
    };

    // --- everything that can fail without the Odoo fixture, before it exists ---
    let stamp = common::unique_base();
    let world: Arc<dyn WorldModel> = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
    let unscoped = OdooIngestor::connect(config(BTreeMap::new()))
        .await
        .expect("connect unscoped ingestor");
    // Ingest + Sweep with the contract's scopes.
    let scoped = Arc::new(
        OdooIngestor::connect(config(
            ontology::supply_risk_contract()
                .expect("contract")
                .source_scopes(),
        ))
        .await
        .expect("connect scoped ingestor"),
    );
    let publisher = Arc::new(InProcessPublisher::new(EVENT_CAPACITY));
    // Subscribed before the pass, so every event it publishes is seen.
    let mut events = publisher.subscribe();
    let ingest = Ingest::new(
        Arc::clone(&scoped) as Arc<dyn SourceIngestor>,
        Arc::clone(&world),
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
    );
    let sweep = Sweep::new(
        Arc::clone(&scoped),
        Arc::clone(&world),
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
        vec![MODEL.to_owned()],
        3600,
    );
    common::spawn_endpoint(
        Endpoint::builder().bind(ingest).bind(sweep).build(),
        &service_addr,
    )
    .await;

    // --- self-contained fixture: an internal quant and its virtual counterpart ---
    let fixture = QuantFixture::create(&odoo, "EKG sweep scope IT").await;
    let (internal, virtual_quant) = (fixture.internal, fixture.virtual_quant);

    // --- materialize both, as an unscoped ingest would have; a failure here undoes the
    // fixture before it fails the test ---
    let mut seeded = Ok(());
    for id in [internal, virtual_quant] {
        let source_ref = SourceRef {
            system: "odoo".to_owned(),
            model: MODEL.to_owned(),
            id,
        };
        seeded = match unscoped.pull(&source_ref).await {
            Ok(record) => world.materialize(&record).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if seeded.is_err() {
            break;
        }
    }
    if let Err(e) = seeded {
        let teardown = fixture.teardown(&odoo).await;
        let _ = http
            .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
            .send()
            .await;
        panic!("materialize the fixture quants: {e} (fixture teardown: {teardown:?})");
    }
    let precondition = common::provenance(&reader, MODEL, virtual_quant).await;

    // --- one real reconciliation pass under an isolated key (the stack's timer paused) ---
    let key = format!("scope-it-{stamp}");
    let claim = common::SweepClaim {
        http: &http,
        admin_url: &admin_url,
        ingress_url: &ingress_url,
        callback_url: &callback_url,
        sweep_key: &key,
    };
    let claimed = common::with_sweep_claimed(&claim, async |deployment: &str| {
        let pass = http
            .post(format!(
                "{ingress_url}/{SWEEP_OBJECT}/{key}/{SWEEP_HANDLER}"
            ))
            .timeout(Duration::from_secs(120))
            .send()
            .await;
        let pass_ok = matches!(&pass, Ok(r) if r.status().is_success());
        let pass = format!("{pass:?}");
        // The pass sends every in-scope quant changed since its (epoch) watermark to the ingest
        // object before it retracts: let those ingests finish before anything is torn down, so
        // none retries against a deleted repo or a deregistered endpoint.
        let drained = common::wait_finished(
            &http,
            &admin_url,
            common::Invocations::of(INGEST_OBJECT)
                .key_prefix(&format!("{MODEL}:"))
                .run_by(deployment),
            Duration::from_secs(180),
        )
        .await;
        let virtual_after = common::provenance(&reader, MODEL, virtual_quant).await;
        let internal_after = common::provenance(&reader, MODEL, internal).await;
        (pass_ok, pass, drained, virtual_after, internal_after)
    })
    .await;

    // --- the test's own teardown, then the assertions ---
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let teardown = fixture.teardown(&odoo).await;
    // Reported at once: a body panic or a failed assertion below must not swallow it.
    if let Err(why) = &teardown {
        eprintln!("fixture teardown failed: {why}");
    }
    let (pass_ok, pass, drained, virtual_after, internal_after) = claimed.into_inner();

    // Every retraction the pass published (a lagging receiver is a failure, never a miss).
    let mut retracted = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) if event.change_kind == ChangeKind::Retracted => {
                retracted.push(event.source.id);
            }
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(missed)) => {
                panic!("the event receiver lagged and missed {missed} events")
            }
        }
    }

    assert!(
        precondition.expect("read the virtual quant's provenance"),
        "precondition: the virtual quant is materialized"
    );
    assert!(pass_ok, "the reconciliation pass ran: {pass}");
    drained.expect("every ingest the pass sent finished");
    assert!(
        !virtual_after.expect("read the virtual quant's provenance"),
        "the already-materialized virtual-location quant was retracted by reconciliation"
    );
    assert!(
        internal_after.expect("read the internal quant's provenance"),
        "the in-scope internal quant stays"
    );
    // A replayed handler may publish a retraction twice; what matters is that the virtual quant
    // was retracted and the internal one never was (a wrong retraction the pass's own re-ingest
    // then undid would still show here).
    assert!(
        retracted.contains(&virtual_quant),
        "the pass published the virtual quant's retraction: {retracted:?}"
    );
    assert!(
        !retracted.contains(&internal),
        "the pass never retracted the in-scope internal quant: {retracted:?}"
    );
    teardown.expect("the quant fixture is cleaned up");
}
