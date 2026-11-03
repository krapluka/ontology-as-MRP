//! Gated live end-to-end test: the scoped `resync` is a shared handler, so it answers while a
//! sweep `run` pass on the same key is still in flight. An exclusive handler would queue
//! behind that pass.
//!
//! How: the `Sweep` is built over a world model whose `select` parks until released. A `run`
//! pass reaches its per-model `select` (the materialized listing) and parks there. `resync`
//! never calls `select`, so it must return its report while `run` is still parked.
//!
//! Like `sweep_e2e_it` and `sweep_resync_e2e_it`, it force-registers `Ingest` and `Sweep`,
//! so two such tests running in parallel would steal each other's invocations. It uses
//! `mrp.workcenter` (a handful of seeded records) to keep the pass and the re-sync small.
//!
//! ```text
//! just podman-local-up
//! EKG_E2E_REQUIRE_INFRA=1 cargo test -p ekg-e2e --test it sweep_resync_shared_e2e_it -- --ignored
//! ```
//!
//! Skips when a hop is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as
//! `just test-e2e` does), when it fails.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ekg_backend::InProcessPublisher;
use ekg_backend::sweep::{RESYNC_HANDLER, SWEEP_HANDLER, SWEEP_OBJECT, Sweep};
use ekg_backend::worker::{INGEST_OBJECT, Ingest};
use ekg_domain::{
    EntityUri, Grounding, ReadContext, Result as EngineResult, Revision, SourceRecord, WorldModel,
};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use restate_sdk::prelude::Endpoint;
use serde_json::json;
use tokio::sync::watch;

/// The model the pass and the re-sync cover. It has few records, so the test stays light.
const MODEL: &str = "mrp.workcenter";

/// A world model whose `select` parks until `release` flips to true (every other call passes
/// straight through), recording that a `select` is parked.
struct GatedWorld {
    inner: Arc<dyn WorldModel>,
    release: watch::Receiver<bool>,
    parked: Arc<AtomicBool>,
}

#[async_trait]
impl WorldModel for GatedWorld {
    async fn materialize(&self, record: &SourceRecord) -> EngineResult<Revision> {
        self.inner.materialize(record).await
    }

    async fn retract(&self, entity: &EntityUri) -> EngineResult<Revision> {
        self.inner.retract(entity).await
    }

    async fn ground(&self, entity: &EntityUri, ctx: &ReadContext) -> EngineResult<Grounding> {
        self.inner.ground(entity, ctx).await
    }

    async fn select(&self, sparql: &str) -> EngineResult<Vec<BTreeMap<String, String>>> {
        self.parked.store(true, Ordering::SeqCst);
        let mut release = self.release.clone();
        let _ = release.wait_for(|released| *released).await;
        self.inner.select(sparql).await
    }
}

/// How many `MODEL` entities the repo holds with provenance.
async fn materialized(client: &GraphDbClient) -> usize {
    let query = format!(
        "SELECT (COUNT(DISTINCT ?e) AS ?n) WHERE {{ GRAPH ?g {{ ?e <{vocab}wasDerivedFrom> ?s }} \
         FILTER(STRSTARTS(STR(?e), \"{ns}{MODEL}/\")) }}",
        vocab = ontology::VOCAB_NAMESPACE,
        ns = ontology::ID_NAMESPACE,
    );
    client
        .select(&query)
        .await
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|r| r.get("n"))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn resync_answers_while_a_sweep_pass_on_the_same_key_is_in_flight() {
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = common::env_or("ODOO_DB", "ekg");
    let odoo_user = common::env_or("ODOO_USER", "admin");
    let odoo_key = common::env_or("ODOO_API_KEY", "admin");
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(13);
    let callback_url = common::callback_url(13);
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
    let repo = "ekg_sweep_resync_shared_it";
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
    let ingestor = match OdooIngestor::connect(OdooConfig {
        base_url: odoo_url,
        db: odoo_db,
        login: odoo_user,
        password: odoo_key,
        system: "odoo".to_owned(),
        allowlist: vec![MODEL.to_owned()],
        scopes: BTreeMap::new(),
    })
    .await
    {
        Ok(ingestor) => Arc::new(ingestor),
        Err(e) => {
            common::infra_unavailable(&format!("Odoo unreachable: {e}"));
            return;
        }
    };

    let world: Arc<dyn WorldModel> = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let (release_tx, release_rx) = watch::channel(false);
    let parked = Arc::new(AtomicBool::new(false));
    let gated: Arc<dyn WorldModel> = Arc::new(GatedWorld {
        inner: Arc::clone(&world),
        release: release_rx,
        parked: Arc::clone(&parked),
    });
    let publisher = Arc::new(InProcessPublisher::new(64));
    let ingest = Ingest::new(
        Arc::clone(&ingestor) as Arc<dyn ekg_domain::SourceIngestor>,
        Arc::clone(&world),
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
    );
    let sweep = Sweep::new(
        Arc::clone(&ingestor),
        gated,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
        vec![MODEL.to_owned()],
        3600,
    );
    common::spawn_endpoint(
        Endpoint::builder().bind(ingest).bind(sweep).build(),
        &service_addr,
    )
    .await;

    let key = format!("resync-shared-it-{}", common::unique_base());
    let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
    // The stack's own sweep timer must not fire on this test's deployment mid-check; it is
    // paused while the deployment claims `Sweep` and re-armed afterwards, whatever happens.
    let claim = common::SweepClaim {
        http: &http,
        admin_url: &admin_url,
        ingress_url: &ingress_url,
        callback_url: &callback_url,
        sweep_key: &key,
    };
    // The pass itself, not its `scheduled` successor: a finished pass has already journaled
    // its reschedule, which alone must not read as "still in flight".
    let this_pass = common::Invocations::of(SWEEP_OBJECT)
        .key(&key)
        .handler(SWEEP_HANDLER)
        .not_scheduled();
    let claimed = common::with_sweep_claimed(&claim, async |deployment: &str| {
        // --- start a sweep pass on the key and let it park on its `select` ---
        let started = http
            .post(format!(
                "{ingress_url}/{SWEEP_OBJECT}/{key}/{SWEEP_HANDLER}/send"
            ))
            .send()
            .await
            .expect("send run");
        let started_ok = started.status().is_success();
        let run_parked = common::wait_until(Duration::from_secs(30), async || {
            parked.load(Ordering::SeqCst)
        })
        .await;

        // --- while it is parked, the re-sync on the same key must answer ---
        let resync = http
            .post(format!(
                "{ingress_url}/{SWEEP_OBJECT}/{key}/{RESYNC_HANDLER}"
            ))
            .header("content-type", "application/json")
            .body(json!({ "models": [MODEL] }).to_string())
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        // Read from Restate, before the release: the pass on the key has not finished.
        let pass_in_flight = common::unfinished(&http, &admin_url, this_pass).await;
        let answered_while_parked = matches!(&resync, Ok(r) if r.status().is_success());
        let report: serde_json::Value = match resync {
            Ok(r) => serde_json::from_str(&r.text().await.unwrap_or_default())
                .unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        };

        // --- release the pass; let it finish and let every ingest it and the re-sync
        // started finish (a wait on the unfinished invocations themselves) ---
        let _ = release_tx.send(true);
        let pass_finished = common::wait_until(Duration::from_secs(60), async || {
            common::unfinished(&http, &admin_url, this_pass)
                .await
                .is_ok_and(|ids| ids.is_empty())
        })
        .await;
        let drained = common::wait_finished(
            &http,
            &admin_url,
            common::Invocations::of(INGEST_OBJECT)
                .key_prefix(&format!("{MODEL}:"))
                .run_by(deployment),
            Duration::from_secs(120),
        )
        .await;
        let current = ingestor.current_ids(MODEL).await;
        let materialized = materialized(&reader).await;
        Observed {
            started_ok,
            run_parked,
            pass_in_flight,
            answered_while_parked,
            report,
            pass_finished,
            drained,
            current,
            materialized,
        }
    })
    .await;

    // --- the test's own teardown, then the assertions ---
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let Observed {
        started_ok,
        run_parked,
        pass_in_flight,
        answered_while_parked,
        report,
        pass_finished,
        drained,
        current,
        materialized,
    } = claimed.into_inner();

    assert!(started_ok, "the sweep pass was accepted");
    assert!(run_parked, "the sweep pass reached its (gated) select");
    let pass_in_flight = pass_in_flight.expect("introspect the pass on the key");
    assert!(
        answered_while_parked && !pass_in_flight.is_empty(),
        "resync answered while the pass on the same key was still in flight \
         (unfinished pass: {pass_in_flight:?}): {report}"
    );
    assert!(
        report["resubmitted"][0][0] == MODEL,
        "the report names the re-synced model: {report}"
    );
    assert!(pass_finished, "the released pass finished");
    drained.expect("every ingest the pass and the re-sync started finished");
    let current = current.expect("list the model's current records");
    assert!(
        materialized >= current.len(),
        "every {MODEL} record the pass and the re-sync submitted was materialized \
         ({materialized} of {})",
        current.len()
    );
}

/// What the claimed section observed (asserted only after cleanup).
#[allow(clippy::struct_excessive_bools)] // a flat record of independent observations
struct Observed {
    started_ok: bool,
    run_parked: bool,
    pass_in_flight: Result<Vec<String>, String>,
    answered_while_parked: bool,
    report: serde_json::Value,
    pass_finished: bool,
    drained: Result<(), String>,
    current: EngineResult<Vec<i64>>,
    materialized: usize,
}
