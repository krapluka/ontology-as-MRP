//! Gated live end-to-end test: the durable ingest path keeps the derived process edges
//! current. A throwaway manufacturing order whose origin names a sales order is materialized
//! through the worker, its origin is changed at the source and it is re-ingested, then it is
//! removed at the source and the sweep retracts it. After each step `ekg:fulfills` is checked
//! by a test subscriber when the step's `RdfChanged` reaches it, which shows the
//! derivation landed before the change was announced.
//!
//! Ignored by default. Same substrate and callback requirements as `sweep_e2e_it` (Odoo,
//! GraphDB and Restate, worker endpoint reachable from the runtime):
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it derive_on_ingest_e2e_it -- --ignored --nocapture
//! ```
//!
//! Everything lands in a throwaway GraphDB repo. The order is built on a throwaway product
//! with no bill of materials, so it has no work orders and never contends with the seed. The
//! run claims `Ingest` + `Sweep` through `common::with_sweep_claimed` and sweeps under its own
//! key. If any hop is unreachable the test skips, unless `EKG_E2E_REQUIRE_INFRA=1` is set
//! (as `just test-e2e` does), when it fails loudly.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ekg_backend::InProcessPublisher;
use ekg_backend::submit::{RestateSubmitter, Submitter};
use ekg_backend::sweep::{SWEEP_HANDLER, SWEEP_OBJECT, Sweep};
use ekg_backend::worker::{DerivationStore, Derivations, INGEST_OBJECT, Ingest};
use ekg_domain::{ChangeKind, RdfChangePublisher, SourceIngestor, SourceRef, WorldModel};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::json;

/// What the subscriber saw when one of the order's changes reached it: the change kind and
/// the sales orders the order `ekg:fulfills` in the store at that moment.
type Observation = (ChangeKind, BTreeSet<String>);

/// The sales orders `mo` fulfils, read from the store now.
async fn fulfilled(reader: &GraphDbClient, mo: &str) -> Result<BTreeSet<String>, String> {
    let query = format!(
        "PREFIX ekg: <{vocab}>\nSELECT ?so WHERE {{ <{mo}> ekg:fulfills ?so }}",
        vocab = ontology::VOCAB_NAMESPACE
    );
    reader
        .select(&query)
        .await
        .map(|rows| rows.iter().filter_map(|r| r.get("so").cloned()).collect())
        .map_err(|e| e.to_string())
}

/// Run a blocking Odoo call off the runtime.
async fn odoo_call<T: Send + 'static, E: std::fmt::Display + Send + 'static>(
    odoo: &Arc<Odoo>,
    call: impl FnOnce(&Odoo) -> Result<T, E> + Send + 'static,
) -> Result<T, String> {
    let odoo = Arc::clone(odoo);
    tokio::task::spawn_blocking(move || call(&odoo))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn derived_edges_follow_ingest_origin_change_and_retract() {
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = common::env_or("ODOO_DB", "ekg");
    let odoo_user = common::env_or("ODOO_USER", "admin");
    let odoo_key = common::env_or("ODOO_API_KEY", "admin");
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(16);
    let callback_url = common::callback_url(16);
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
                common::infra_unavailable(&format!("Odoo unreachable: {e}"));
                return;
            }
        }
    };

    // Two seeded sales orders the throwaway order's origin names in turn.
    let orders = odoo_call(&odoo_raw, |o| {
        o.search_read("sale.order", json!([]), json!(["id", "name"]))
    })
    .await
    .expect("read sales orders");
    let mut orders: Vec<(i64, String)> = orders
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|r| Some((r.get("id")?.as_i64()?, r.get("name")?.as_str()?.to_owned())))
        .collect();
    orders.sort();
    assert!(
        orders.len() >= 2,
        "the seed carries at least two sales orders"
    );
    let (so_a, so_b) = (&orders[0], &orders[1]);

    let contract = ontology::supply_risk_contract().expect("contract");
    let odoo = Arc::new(
        OdooIngestor::connect(OdooConfig {
            base_url: odoo_url.clone(),
            db: odoo_db,
            login: odoo_user,
            password: odoo_key,
            system: "odoo".to_owned(),
            allowlist: vec!["mrp.production".to_owned(), "sale.order".to_owned()],
            scopes: contract.source_scopes(),
        })
        .await
        .expect("connect ingestor"),
    );
    let _slot = common::fixture_repo_slot().await.expect("fixture slot");
    let repo = "ekg_derive_on_ingest_e2e_it";
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

    // --- the worker + sweep, deriving into the throwaway repo ---
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        contract,
    ));
    let publisher = Arc::new(InProcessPublisher::new(256));
    let derivations = Derivations::new(
        Arc::new(GraphDbClient::new(&graphdb_url, repo).expect("client"))
            as Arc<dyn DerivationStore>,
        ontology::STRUCTURAL_DERIVATIONS,
    );
    let ingest = Ingest::new(
        Arc::clone(&odoo) as Arc<dyn SourceIngestor>,
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn RdfChangePublisher>,
    )
    .with_derivations(derivations.clone());
    let sweep = Sweep::new(
        Arc::clone(&odoo),
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn RdfChangePublisher>,
        vec!["mrp.production".to_owned(), "sale.order".to_owned()],
        3600,
    )
    .with_derivations(derivations);
    let endpoint = Endpoint::builder().bind(ingest).bind(sweep).build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
    let submitter = RestateSubmitter::new(ingress_url.clone());

    // --- the throwaway order: a product with no BOM, origin naming the first order ---
    let stamp = common::odoo_fixture::stamp();
    let product = odoo_call(&odoo_raw, move |o| {
        o.create(
            "product.product",
            json!({ "name": format!("EKG derive IT product {stamp} (throwaway)"), "type": "consu" }),
        )
    })
    .await
    .expect("create product");
    let origin_a = so_a.1.clone();
    let mo = odoo_call(&odoo_raw, move |o| {
        o.create(
            "mrp.production",
            json!({ "product_id": product, "product_qty": 1.0, "bom_id": false, "origin": origin_a }),
        )
    })
    .await;
    let mo = match mo {
        Ok(id) => id,
        Err(why) => {
            let _ = odoo_call(&odoo_raw, move |o| {
                o.unlink("product.product", json!([product]))
            })
            .await;
            panic!("create the throwaway manufacturing order: {why}");
        }
    };
    let mo_iri = ontology::entity_iri("mrp.production", mo);
    let first_order_iri = ontology::entity_iri("sale.order", so_a.0);
    let second_order_iri = ontology::entity_iri("sale.order", so_b.0);

    // --- the subscriber: read `fulfills` the moment each of the order's changes arrives ---
    let seen: Arc<Mutex<Vec<Observation>>> = Arc::new(Mutex::new(Vec::new()));
    let mut rx = publisher.subscribe();
    let watcher = {
        let seen = Arc::clone(&seen);
        let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
        let mo_iri = mo_iri.clone();
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if event.entity_uri.0 == mo_iri {
                    let now = fulfilled(&reader, &mo_iri).await.unwrap_or_default();
                    seen.lock().expect("seen").push((event.change_kind, now));
                }
            }
        })
    };
    let observed = |n: usize| {
        let seen = Arc::clone(&seen);
        async move || seen.lock().expect("seen").len() >= n
    };

    let key = format!("derive-it-{}", common::unique_base());
    let claim = common::SweepClaim {
        http: &http,
        admin_url: &admin_url,
        ingress_url: &ingress_url,
        callback_url: &callback_url,
        sweep_key: &key,
    };
    let claimed = common::with_sweep_claimed(&claim, async |deployment: &str| {
        let submit = async |model: &str, id: i64| {
            submitter
                .submit(&SourceRef {
                    system: "odoo".to_owned(),
                    model: model.to_owned(),
                    id,
                })
                .await
                .map_err(|e| e.to_string())
        };
        // The two sales orders first, so the origin has a name to match.
        submit("sale.order", so_a.0).await?;
        submit("sale.order", so_b.0).await?;
        let orders_in = common::wait_until(Duration::from_secs(60), async || {
            common::has_provenance(&reader, "sale.order", so_a.0).await
                && common::has_provenance(&reader, "sale.order", so_b.0).await
        })
        .await;
        if !orders_in {
            return Err("the sales orders did not materialize".to_owned());
        }

        // 1. The order is ingested: its change arrives with `fulfills` pointing at the
        // first order.
        submit("mrp.production", mo).await?;
        let first = common::wait_until(Duration::from_secs(60), observed(1)).await;

        // 2. Its origin moves to the second order at the source; re-ingested.
        let origin_b = so_b.1.clone();
        odoo_call(&odoo_raw, move |o| {
            o.write("mrp.production", json!([mo]), json!({ "origin": origin_b }))
        })
        .await?;
        submit("mrp.production", mo).await?;
        let second = common::wait_until(Duration::from_secs(60), observed(2)).await;

        // 3. It is removed at the source; one sweep pass under the test's own key retracts it.
        odoo_call(&odoo_raw, move |o| o.unlink("mrp.production", json!([mo]))).await?;
        let trigger = http
            .post(format!(
                "{ingress_url}/{SWEEP_OBJECT}/{key}/{SWEEP_HANDLER}/send"
            ))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !trigger.status().is_success() {
            return Err(format!("sweep trigger rejected: {}", trigger.status()));
        }
        let third = common::wait_until(Duration::from_secs(90), async || {
            seen.lock()
                .expect("seen")
                .iter()
                .any(|(kind, _)| *kind == ChangeKind::Retracted)
        })
        .await;
        // The pass re-pulls every order changed since its (empty) watermark: let those ingests
        // finish before anything is torn down.
        let drained = common::wait_finished(
            &http,
            &admin_url,
            common::Invocations::of(INGEST_OBJECT).run_by(deployment),
            Duration::from_secs(180),
        )
        .await;
        Ok((first, second, third, drained))
    })
    .await;

    // --- teardown, then the assertions ---
    watcher.abort();
    let _ = odoo_call(&odoo_raw, move |o| o.unlink("mrp.production", json!([mo]))).await;
    let _ = odoo_call(&odoo_raw, move |o| {
        o.unlink("product.product", json!([product]))
    })
    .await;
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let (first, second, third, drained) = claimed.into_inner().expect("the run");
    let seen = seen.lock().expect("seen").clone();
    println!("observed at each announcement: {seen:?}");

    assert!(first, "the order's first ingest was never announced");
    assert_eq!(
        seen[0],
        (
            ChangeKind::Created,
            BTreeSet::from([first_order_iri.clone()])
        ),
        "the first announcement already sees the derived edge to the origin's order"
    );
    assert!(second, "the origin change was never announced");
    assert_eq!(
        seen[1],
        (ChangeKind::Updated, BTreeSet::from([second_order_iri])),
        "the re-ingest's announcement sees the edge moved to the new origin's order"
    );
    assert!(third, "the retraction was never announced");
    let retracted = seen
        .iter()
        .find(|(kind, _)| *kind == ChangeKind::Retracted)
        .expect("a retraction");
    assert!(
        retracted.1.is_empty(),
        "no derived edge from the retracted order remains when it is announced: {retracted:?}"
    );
    drained.expect("every ingest the pass sent finished");
}
