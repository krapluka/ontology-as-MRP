//! Gated live end-to-end test: the reconciliation sweep retracts an entity archived
//! at the source and re-pulls a change that never got a webhook.
//!
//! Ignored by default. Same stack and callback requirements as `ingest_e2e_it`
//! (Odoo, GraphDB and Restate, worker endpoint reachable from the runtime):
//!
//! ```text
//! just podman-local-up && just podman-local-up-sources
//! cargo test -p ekg-e2e --test it sweep_e2e_it -- --ignored --nocapture
//! ```
//!
//! Scenario, both against a throwaway GraphDB repo:
//! - retract on archive: create a `res.partner`, materialize it via the ingress,
//!   archive it in Odoo, run the sweep. Its provenance is gone (tombstoned).
//! - missed-change re-pull: create a second `res.partner` that is never webhooked.
//!   The sweep's `changed_since` re-pulls it and its provenance appears.
//!
//! The pass runs under the test's own `Sweep` key, not the stack's: its durable state
//! (the watermark) starts empty, so the never-webhooked record is always re-pulled, and the
//! stack's shared watermark is not moved by a partner-only pass. The run claims `Ingest`
//! and `Sweep` through `common::with_sweep_claimed`, which pauses the stack's own sweep
//! timer meanwhile and re-arms it afterwards, a panic or hang included.
//!
//! If any hop is unreachable the test skips instead of failing, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), when it fails.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::InProcessPublisher;
use ekg_backend::submit::{RestateSubmitter, Submitter};
use ekg_backend::sweep::{SWEEP_HANDLER, SWEEP_OBJECT, Sweep};
use ekg_backend::worker::{INGEST_OBJECT, Ingest};
use ekg_domain::{SourceRef, WorldModel};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, OdooConfig, OdooIngestor, ontology};
use odoo_client::Odoo;
use restate_sdk::prelude::Endpoint;
use serde_json::json;

/// Best-effort `res.partner` write of an arbitrary field payload (archive, cleanup).
async fn partner_write(odoo: &Arc<Odoo>, id: i64, values: serde_json::Value) {
    let odoo = Arc::clone(odoo);
    let _ =
        tokio::task::spawn_blocking(move || odoo.write("res.partner", json!([id]), values)).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Odoo + GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn sweep_retracts_archived_and_repulls_missed() {
    let odoo_url = common::env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = common::env_or("ODOO_DB", "ekg");
    let odoo_user = common::env_or("ODOO_USER", "admin");
    let odoo_key = common::env_or("ODOO_API_KEY", "admin");
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(3);
    let callback_url = common::callback_url(3);

    let http = reqwest::Client::new();

    // --- guard: Restate admin reachable, else skip ---
    if http
        .get(format!("{admin_url}/deployments"))
        .send()
        .await
        .is_err()
    {
        common::infra_unavailable(&format!("Restate admin unreachable at {admin_url}"));
        return;
    }

    // --- raw Odoo client for the create/archive/unlink mutations (blocking) ---
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

    // --- connect the ingestor and provision a throwaway GraphDB repo ---
    let ingestor = OdooIngestor::connect(OdooConfig {
        base_url: odoo_url.clone(),
        db: odoo_db,
        login: odoo_user,
        password: odoo_key,
        system: "odoo".to_owned(),
        allowlist: vec!["res.partner".to_owned()],
        scopes: std::collections::BTreeMap::new(),
    })
    .await
    .expect("connect ingestor");
    let repo = "ekg_sweep_e2e_it";
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

    // --- serve Ingest and Sweep in-process ---
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, repo).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let publisher = Arc::new(InProcessPublisher::new(64));
    let odoo = Arc::new(ingestor);
    let ingest = Ingest::new(
        Arc::clone(&odoo) as Arc<dyn ekg_domain::SourceIngestor>,
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
    );
    let sweep = Sweep::new(
        Arc::clone(&odoo),
        Arc::clone(&world) as Arc<dyn WorldModel>,
        Arc::clone(&publisher) as Arc<dyn ekg_domain::RdfChangePublisher>,
        vec!["res.partner".to_owned()],
        3600, // long reschedule; we trigger the pass explicitly
    );
    let endpoint = Endpoint::builder().bind(ingest).bind(sweep).build();
    // The listener is bound first, so a port collision fails here and not as a later
    // timeout, and the socket is listening before we register.
    common::spawn_endpoint(endpoint, &service_addr).await;
    let reader = GraphDbClient::new(&graphdb_url, repo).expect("reader");
    let submitter = RestateSubmitter::new(ingress_url.clone());

    // The throwaway partners, created last (nothing above can leak them). The retract case is
    // materialized then archived; the missed one is never webhooked.
    let partner_id = common::create_partner(&odoo_raw, "EKG sweep IT retract (throwaway)").await;
    let missed_id = common::create_partner(&odoo_raw, "EKG sweep IT missed (throwaway)").await;

    let key = format!("sweep-it-{}", common::unique_base());
    let claim = common::SweepClaim {
        http: &http,
        admin_url: &admin_url,
        ingress_url: &ingress_url,
        callback_url: &callback_url,
        sweep_key: &key,
    };
    let claimed = common::with_sweep_claimed(&claim, async |deployment: &str| {
        // --- materialize the throwaway partner via the ingress, then archive it ---
        let sent_partner = submitter
            .submit(&SourceRef {
                system: "odoo".to_owned(),
                model: "res.partner".to_owned(),
                id: partner_id,
            })
            .await
            .map_err(|e| e.to_string());
        let materialized = sent_partner.is_ok()
            && common::wait_until(Duration::from_secs(30), async || {
                common::has_provenance(&reader, "res.partner", partner_id).await
            })
            .await;
        if materialized {
            partner_write(&odoo_raw, partner_id, json!({ "active": false })).await;
        }

        // --- one sweep pass under the test's own key ---
        let trigger = http
            .post(format!(
                "{ingress_url}/{SWEEP_OBJECT}/{key}/{SWEEP_HANDLER}/send"
            ))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map(|r| r.status());

        // --- retract-on-archive: the archived partner is tombstoned (no provenance) ---
        // A failed read counts as "not yet retracted", not as proof of absence.
        let retracted = common::wait_until(Duration::from_secs(60), async || {
            matches!(
                common::provenance(&reader, "res.partner", partner_id).await,
                Ok(false)
            )
        })
        .await;
        // --- missed-change re-pull: the never-webhooked partner is pulled by the sweep ---
        let repulled = common::wait_until(Duration::from_secs(60), async || {
            common::has_provenance(&reader, "res.partner", missed_id).await
        })
        .await;
        // The pass re-pulls every partner changed since its (empty) watermark: let those
        // ingests finish before anything is torn down.
        let drained = common::wait_finished(
            &http,
            &admin_url,
            common::Invocations::of(INGEST_OBJECT)
                .key_prefix("res.partner:")
                .run_by(deployment),
            Duration::from_secs(180),
        )
        .await;
        (
            sent_partner,
            materialized,
            trigger,
            retracted,
            repulled,
            drained,
        )
    })
    .await;

    // --- the test's own teardown, then the assertions ---
    common::unlink_partner(&odoo_raw, partner_id).await;
    common::unlink_partner(&odoo_raw, missed_id).await;
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{repo}"))
        .send()
        .await;
    let (sent_partner, materialized, trigger, retracted, repulled, drained) = claimed.into_inner();

    sent_partner.expect("submit partner");
    assert!(materialized, "the throwaway partner did not materialize");
    let trigger = trigger.expect("trigger sweep");
    assert!(trigger.is_success(), "sweep trigger rejected: {trigger}");
    assert!(retracted, "the sweep did not retract the archived partner");
    assert!(
        repulled,
        "the sweep did not re-pull the missed (never-webhooked) change"
    );
    drained.expect("every ingest the pass sent finished");
}
