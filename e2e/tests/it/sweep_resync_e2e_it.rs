//! Gated live end-to-end test: the sweep's scoped re-sync backfills a newly mapped field
//! onto an already-materialized record, as a governed, provenance-bearing materialization,
//! refuses a non-allowlisted model (HTTP 400, nothing pulled), and leaves the watermark and
//! the sweep timer untouched.
//!
//! It also checks that the order currency lands: every seeded sales and purchase order,
//! materialized without the `currency_id` mapping, is backfilled by
//! `resync sale.order purchase.order res.currency` with `ekg:in_currency` pointing at an
//! `ekg:Currency` whose `ekg:currency_code` is `USD`. This is the same re-sync
//! `just sweep-resync sale.order purchase.order res.currency` runs against the stack.
//!
//! It binds `Ingest` and `Sweep` and force-registers them, as `sweep_e2e_it` does. Two such
//! tests running at once would steal each other's invocations (Restate routes a service to
//! its most recently registered deployment), so it claims them through
//! `common::with_sweep_claimed`.
//!
//! ```text
//! just podman-local-up
//! EKG_E2E_REQUIRE_INFRA=1 cargo test -p ekg-e2e --test it sweep_resync_e2e_it -- --ignored
//! ```
//!
//! Skips when a hop is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as
//! `just test-e2e` does), when it fails.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::InProcessPublisher;
use ekg_backend::sweep::{RESYNC_HANDLER, SWEEP_HANDLER, SWEEP_OBJECT, Sweep};
use ekg_backend::worker::{INGEST_OBJECT, Ingest};
use ekg_domain::{SourceIngestor, SourceRef, WorldModel};
use ekg_semantic::mapping::MappingContract;
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::json;

/// The entity's `ekg:name` values, read only from graphs where the entity carries its
/// provenance (so a value counts only as part of a governed materialization).
async fn provenanced_names(client: &GraphDbClient, model: &str, id: i64) -> Vec<String> {
    let entity = ontology::entity_iri(model, id);
    let query = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?n WHERE {{ GRAPH ?g {{ <{entity}> ekg:wasDerivedFrom ?src ; ekg:name ?n }} }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    client
        .select(&query)
        .await
        .map(|rows| rows.iter().filter_map(|r| r.get("n").cloned()).collect())
        .unwrap_or_default()
}

/// The currency codes `order` (of `model`) is recorded in, read only as governed
/// materializations: the order's `ekg:in_currency` from a graph where it carries its provenance,
/// and the currency's `ekg:currency_code` from one where the currency carries its own.
async fn provenanced_currency(client: &GraphDbClient, model: &str, id: i64) -> Vec<String> {
    let entity = ontology::entity_iri(model, id);
    let query = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?code WHERE {{ \
           GRAPH ?g {{ <{entity}> ekg:wasDerivedFrom ?src ; ekg:in_currency ?c }} \
           GRAPH ?h {{ ?c a ekg:Currency ; ekg:wasDerivedFrom ?csrc ; ekg:currency_code ?code }} }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    client
        .select(&query)
        .await
        .map(|rows| rows.iter().filter_map(|r| r.get("code").cloned()).collect())
        .unwrap_or_default()
}

/// The order models whose currency the re-sync backfills.
const ORDER_MODELS: [&str; 2] = ["sale.order", "purchase.order"];

/// How many `res.partner` entities the repo holds with provenance (materialized).
async fn materialized_partners(client: &GraphDbClient) -> usize {
    let query = format!(
        "SELECT (COUNT(DISTINCT ?e) AS ?n) WHERE {{ GRAPH ?g {{ ?e <{vocab}wasDerivedFrom> ?s }} \
         FILTER(STRSTARTS(STR(?e), \"{ns}res.partner/\")) }}",
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

/// Scoped re-sync backfill: a record materialized before a field was mapped (and unchanged at
/// the source since, so the watermark sweep never re-pulls it) gains that field once the
/// operator runs the re-sync, as a governed, provenance-bearing materialization. The re-sync
/// leaves the watermark and the sweep timer untouched, and refuses a model outside the
/// allowlist.
///
/// "Before the field was mapped" is simulated by materializing the partner through a contract
/// with the partner's `name` mapping removed; the re-sync then runs the real contract.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn resync_backfills_a_newly_mapped_field_without_moving_the_watermark() {
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = common::env_or("ODOO_DB", "ekg");
    let odoo_user = common::env_or("ODOO_USER", "admin");
    let odoo_key = common::env_or("ODOO_API_KEY", "admin");
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(12);
    let callback_url = common::callback_url(12);
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
    let repo = "ekg_sweep_resync_it";
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

    // --- everything that can fail without the Odoo partner, before it exists ---
    let base = common::unique_base();
    let ingestor = Arc::new(
        OdooIngestor::connect(OdooConfig {
            base_url: odoo_url.clone(),
            db: odoo_db,
            login: odoo_user,
            password: odoo_key,
            system: "odoo".to_owned(),
            allowlist: resync_models(),
            scopes: std::collections::BTreeMap::new(),
        })
        .await
        .expect("connect ingestor"),
    );
    // "Before `name` was mapped": the real contract with the partner's name mapping removed.
    let reduced_yaml = ontology::CONTRACT_YAML.replacen(
        "      name: { property: name, datatype: xsd:string }\n      supplier_rank:",
        "      supplier_rank:",
        1,
    );
    assert_ne!(
        reduced_yaml,
        ontology::CONTRACT_YAML,
        "the partner's name mapping was located and removed"
    );
    // "Before `currency_id` was mapped": both orders' currency references removed too.
    let unmapped = reduced_yaml
        .replacen(
            "      currency_id: { property: in_currency, ref: res.currency }   # the currency amount_total is in\n",
            "",
            1,
        )
        .replacen(
            "      currency_id: { property: in_currency, ref: res.currency }   # the currency its line prices are in\n",
            "",
            1,
        );
    assert_eq!(
        reduced_yaml.matches("ref: res.currency").count(),
        2,
        "the real contract references the currency from both orders"
    );
    assert!(
        !unmapped.contains("ref: res.currency"),
        "both orders' currency mappings were located and removed"
    );
    let reduced_yaml = unmapped;
    let reduced_world = GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        MappingContract::load(
            &reduced_yaml,
            &format!("{}\n{}", ontology::CORE_TTL, ontology::ONTOLOGY_TTL),
        )
        .expect("reduced contract"),
    );
    let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
    // Ingest and Sweep over the real contract.
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let publisher = Arc::new(InProcessPublisher::new(64));
    let ingest = Ingest::new(
        Arc::clone(&ingestor) as Arc<dyn ekg_domain::SourceIngestor>,
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
    );
    let sweep = Sweep::new(
        Arc::clone(&ingestor),
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
        resync_models(),
        3600,
    );
    let endpoint = Endpoint::builder().bind(ingest).bind(sweep).build();
    common::spawn_endpoint(endpoint, &service_addr).await;

    // --- the throwaway partner, materialized as it was before `name` was mapped; a failure
    // here unlinks it before it fails the test ---
    let partner_name = format!("EKG resync IT backfill {base}");
    let partner_id = common::create_partner(&odoo_raw, &partner_name).await;
    let partner_ref = SourceRef {
        system: "odoo".to_owned(),
        model: "res.partner".to_owned(),
        id: partner_id,
    };
    let premapped: Result<(), String> = async {
        let record = ingestor
            .pull(&partner_ref)
            .await
            .map_err(|e| format!("pull partner: {e}"))?;
        reduced_world
            .materialize(&record)
            .await
            .map_err(|e| format!("materialize pre-mapping partner: {e}"))?;
        if !common::provenance(&reader, "res.partner", partner_id).await? {
            return Err("the pre-mapping partner is not materialized with provenance".to_owned());
        }
        let names = provenanced_names(&reader, "res.partner", partner_id).await;
        if !names.is_empty() {
            return Err(format!(
                "before the re-sync the partner already carries a name (the field was not mapped yet): {names:?}"
            ));
        }
        Ok(())
    }
    .await;
    // The seeded orders, materialized as they were before `currency_id` was mapped.
    let seeded: Result<Vec<(&str, i64)>, String> = async {
        let mut seeded = Vec::new();
        for model in ORDER_MODELS {
            let odoo = Arc::clone(&odoo_raw);
            let ids = tokio::task::spawn_blocking(move || odoo.search(model, json!([])))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("search {model}: {e}"))?;
            if ids.is_empty() {
                return Err(format!("no seeded {model} to backfill (seed the stack)"));
            }
            for id in ids {
                let record = ingestor
                    .pull(&SourceRef {
                        system: "odoo".to_owned(),
                        model: model.to_owned(),
                        id,
                    })
                    .await
                    .map_err(|e| format!("pull {model}/{id}: {e}"))?;
                reduced_world
                    .materialize(&record)
                    .await
                    .map_err(|e| format!("materialize pre-mapping {model}/{id}: {e}"))?;
                if !provenanced_currency(&reader, model, id).await.is_empty() {
                    return Err(format!(
                        "before the re-sync {model}/{id} already carries a currency"
                    ));
                }
                seeded.push((model, id));
            }
        }
        Ok(seeded)
    }
    .await;
    let premapped = premapped.and(seeded.as_ref().map(|_| ()).map_err(Clone::clone));
    if let Err(why) = premapped {
        common::unlink_partner(&odoo_raw, partner_id).await;
        let _ = http
            .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
            .send()
            .await;
        panic!("precondition: {why}");
    }
    let seeded = seeded.expect("checked above");

    // An isolated sweep key: its durable state starts empty, so "the watermark was not
    // written" is observable (a pass of `run` would have set it).
    let key = format!("resync-it-{base}");
    let resync_url = format!("{ingress_url}/{SWEEP_OBJECT}/{key}/{RESYNC_HANDLER}");
    // The stack's own sweep timer is paused while this deployment claims `Sweep`, and re-armed
    // afterwards, whatever happens in the body.
    let claim = common::SweepClaim {
        http: &http,
        admin_url: &admin_url,
        ingress_url: &ingress_url,
        callback_url: &callback_url,
        sweep_key: &key,
    };
    let claimed = common::with_sweep_claimed(&claim, async |deployment: &str| {
        // --- a model outside the allowlist is refused, nothing pulled ---
        let refused = http
            .post(&resync_url)
            .header("content-type", "application/json")
            .body(json!({ "models": ["res.partner", "account.move"] }).to_string())
            // Bounded: the ingress holds the call until the re-sync completes.
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .expect("post refused resync");
        let refused_status = refused.status().as_u16();
        let refused_body = refused.text().await.unwrap_or_default();
        // "nothing pulled": give a (wrongly) partially-accepted refusal time to backfill the
        // allowlisted partner. It must not.
        let pulled_on_refusal = common::wait_until(Duration::from_secs(5), async || {
            !provenanced_names(&reader, "res.partner", partner_id)
                .await
                .is_empty()
        })
        .await;

        // --- the scoped re-sync backfills the partner's name and the orders' currency ---
        let resync = http
            .post(&resync_url)
            .header("content-type", "application/json")
            .body(json!({ "models": resync_models() }).to_string())
            // Bounded: the ingress holds the call until the re-sync completes.
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .expect("post resync");
        let resync_ok = resync.status().is_success();
        let report: serde_json::Value =
            serde_json::from_str(&resync.text().await.unwrap_or_default())
                .unwrap_or(serde_json::Value::Null);
        let backfilled = common::wait_until(Duration::from_secs(60), async || {
            provenanced_names(&reader, "res.partner", partner_id)
                .await
                .contains(&partner_name)
        })
        .await;

        // Wait for the currency backfill; the per-order codes read below are what is asserted.
        common::wait_until(Duration::from_secs(60), async || {
            for (model, id) in &seeded {
                if provenanced_currency(&reader, model, *id).await != ["USD"] {
                    return false;
                }
            }
            true
        })
        .await;

        // Let every ingest the re-sync started finish before the repo is dropped, so no
        // straggler retries against a deleted repo or a deregistered endpoint. The wait is on
        // the unfinished invocations themselves, not on a count that one early batch can
        // satisfy.
        let mut drained = Ok(());
        for model in resync_models() {
            let prefix = format!("{model}:");
            let this = common::wait_finished(
                &http,
                &admin_url,
                common::Invocations::of(INGEST_OBJECT)
                    .key_prefix(&prefix)
                    .run_by(deployment),
                Duration::from_secs(120),
            )
            .await;
            drained = drained.and(this);
        }
        let currencies: Vec<((&str, i64), Vec<String>)> = {
            let mut out = Vec::new();
            for (model, id) in &seeded {
                out.push((
                    (*model, *id),
                    provenanced_currency(&reader, model, *id).await,
                ));
            }
            out
        };
        // The re-sync re-pulled every current record of the model, not just ours.
        let resubmitted = report["resubmitted"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|pair| pair.get(1))
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        let materialized = materialized_partners(&reader).await;

        // --- the watermark and the timer are untouched under this key ---
        let watermark_rows = common::admin_rows(
            &http,
            &admin_url,
            &format!(
                "SELECT value_utf8 FROM state WHERE service_name = '{SWEEP_OBJECT}' \
                 AND service_key = '{key}' AND key = 'watermark'"
            ),
        )
        .await;
        let scheduled_runs = common::admin_rows(
            &http,
            &admin_url,
            &format!(
                "SELECT id FROM sys_invocation WHERE target_service_name = '{SWEEP_OBJECT}' \
                 AND target_service_key = '{key}' AND target_handler_name = '{SWEEP_HANDLER}'"
            ),
        )
        .await;
        Observed {
            refused_status,
            refused_body,
            pulled_on_refusal,
            resync_ok,
            report,
            backfilled,
            currencies,
            drained,
            resubmitted,
            materialized,
            watermark_rows,
            scheduled_runs,
        }
    })
    .await;

    // --- the test's own teardown, then the assertions ---
    common::unlink_partner(&odoo_raw, partner_id).await;
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let Observed {
        refused_status,
        refused_body,
        pulled_on_refusal,
        resync_ok,
        report,
        backfilled,
        currencies,
        drained,
        resubmitted,
        materialized,
        watermark_rows,
        scheduled_runs,
    } = claimed.into_inner();

    let watermark_rows = watermark_rows.expect("introspect the sweep's watermark state");
    let scheduled_runs = scheduled_runs.expect("introspect the sweep's invocations");
    assert_eq!(
        refused_status, 400,
        "a re-sync naming a non-allowlisted model is refused as the caller's error: {refused_body}"
    );
    assert!(
        refused_body.contains("account.move"),
        "the refusal names the offending model: {refused_body}"
    );
    assert!(
        !pulled_on_refusal,
        "a refused re-sync pulled nothing (the partner gained no name before the accepted one)"
    );
    assert!(resync_ok, "the scoped re-sync was accepted: {report}");
    assert!(
        resubmitted >= 1,
        "the re-sync resubmitted the partner model's current records: {report}"
    );
    drained.expect("every ingest the re-sync started finished");
    assert!(
        materialized >= resubmitted,
        "every one of the {resubmitted} resubmitted partners was re-materialized (found {materialized})"
    );
    assert!(
        backfilled,
        "the re-sync backfilled the newly mapped name onto the materialized partner"
    );
    for ((model, id), codes) in &currencies {
        assert_eq!(
            codes,
            &["USD"],
            "the re-sync backfilled {model}/{id}'s currency as exactly USD: {currencies:?}"
        );
    }
    assert!(
        watermark_rows.is_empty(),
        "the re-sync never writes the sweep watermark: {watermark_rows:?}"
    );
    assert!(
        scheduled_runs.is_empty(),
        "the re-sync never schedules a sweep pass: {scheduled_runs:?}"
    );
}

/// The models the re-sync runs over: the partner (its name backfill) and the order currency's
/// models.
fn resync_models() -> Vec<String> {
    [
        "res.partner",
        "sale.order",
        "purchase.order",
        "res.currency",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// What the claimed section observed (asserted only after cleanup).
struct Observed<'s> {
    refused_status: u16,
    refused_body: String,
    pulled_on_refusal: bool,
    resync_ok: bool,
    report: serde_json::Value,
    backfilled: bool,
    currencies: Vec<((&'s str, i64), Vec<String>)>,
    drained: Result<(), String>,
    resubmitted: usize,
    materialized: usize,
    watermark_rows: Result<Vec<serde_json::Value>, String>,
    scheduled_runs: Result<Vec<serde_json::Value>, String>,
}
