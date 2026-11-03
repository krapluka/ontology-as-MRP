//! Gated live end-to-end test: the durable drift monitor groups coupled points awaiting
//! oversight into durable portfolios, regroups them as the world changes, and a member leaving
//! oversight supersedes its portfolio. `notify` reports the portfolios a pass formed (with
//! their generation) and the points it released, and a joint slate lands only for a
//! portfolio's current generation.
//!
//! Ignored by default. Requires GraphDB + Restate with the test endpoint reachable from the
//! runtime (Odoo is not needed: typed fixtures are materialized into a throwaway repo):
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it portfolio_grouping_e2e_it -- --ignored --nocapture
//! ```
//!
//! The monitor runs a fixture risk rule (every entity whose `ekg:name` carries the run's tag
//! is flagged) and the capability's own coupling rule, over manufacturing orders whose
//! origin names a tagged sales order (so the derived `ekg:fulfills` couples them). The
//! scenario:
//!
//! 1. the order and one manufacturing order form one active portfolio; each point records it;
//! 2. a second manufacturing order fulfilling the same order regroups the three into a new
//!    portfolio and supersedes the first;
//! 3. a redelivered event forms no second portfolio and changes nothing;
//! 4. aborting a member clears its portfolio and supersedes the portfolio with the reason;
//! 5. a stale redelivery re-forms the first pair as generation 2: a joint slate for generation
//!    1 is refused (409), one for generation 2 lands;
//! 6. aborting the pair's manufacturing order leaves the order alone, and `notify` reports it
//!    released.
//!
//! A joint slate for generation 1 attached after step 1 lands and is audited.
//!
//! If any hop is unreachable the test skips, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as
//! `just test-e2e` does), when it fails loudly.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::drift;
use ekg_domain::{ChangeKind, EntityUri, NamedGraph, RdfChanged, Revision, SourceRef, WorldModel};
use ekg_dynamic::{
    ActionPointService, DriftMonitorService, DriftVerdict, PortfolioService, RiskRule, portfolio_id,
};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};
use restate_sdk::prelude::Endpoint;
use serde_json::{Value, json};

const REPO: &str = "ekg_portfolio_grouping_e2e_it";

/// A fixture risk rule: flag every entity whose `ekg:name` starts with `tag`.
fn tagged_rule(tag: &str) -> RiskRule {
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\nSELECT ?entity WHERE {{ ?entity ekg:name ?n . \
         FILTER(STRSTARTS(STR(?n), \"{tag}\")) }}",
        vocab = ontology::VOCAB_NAMESPACE
    );
    RiskRule::new(sparql, |row| {
        Some(DriftVerdict {
            subject: EntityUri(row.get("entity")?.clone()),
            reason: "fixture flag".to_owned(),
        })
    })
}

/// A `RdfChanged` for `model`/`id` at `revision`.
fn changed(model: &str, id: i64, revision: u64) -> RdfChanged {
    RdfChanged {
        entity_uri: EntityUri(ontology::entity_iri(model, id)),
        named_graph: NamedGraph(ontology::named_graph(model, id)),
        change_kind: ChangeKind::Updated,
        source: SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        },
        revision: Revision(revision),
    }
}

/// `POST {ingress}/{service}/{key}/{handler}` with an optional JSON body; the JSON answer.
async fn call(
    http: &reqwest::Client,
    ingress: &str,
    service: &str,
    key: &str,
    handler: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let mut url = reqwest::Url::parse(ingress).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|()| "base url".to_owned())?
        .pop_if_empty()
        .extend([service, key, handler]);
    let mut req = http.post(url).timeout(Duration::from_secs(30));
    if let Some(body) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{service}/{handler} returned {status}: {text}"));
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))
}

/// Hand one change to the durable monitor and wait for its pass to finish; the pass's
/// outcome (`opened`, `formed`, `released`).
async fn notify(
    http: &reqwest::Client,
    ingress: &str,
    event: &RdfChanged,
) -> Result<Value, String> {
    // The synchronous `notify` endpoint: the call returns once the pass has run.
    let url = drift::monitor_notify_url(ingress)?;
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(event).map_err(|e| e.to_string())?)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("monitor rejected notify: {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))
}

/// A minimal joint slate: a baseline and a Linked opening step.
fn joint_slate(generation: u32) -> Value {
    json!({
        "generation": generation,
        "slate": {
            "baseline": { "metrics": {}, "rank_key": "9" },
            "options": [],
            "trace": { "steps": [
                { "tag": "graph", "text": "Linked: fixture", "rejected": false }
            ] }
        }
    })
}

/// Abort `subject`'s point through the stop control (claim + abort).
async fn abort(http: &reqwest::Client, ingress: &str, subject: &str) -> Result<(), String> {
    let token = call(
        http,
        ingress,
        "ActionPointService",
        subject,
        "claim",
        Some(json!({
            "command": "abort",
            "reviewer": "planner-1",
            "expected_state": ["open", "proposed", "under_review"],
            "deadline_secs": 60
        })),
    )
    .await?;
    call(
        http,
        ingress,
        "ActionPointService",
        subject,
        "abort",
        Some(json!({ "token": token, "reviewer": "planner-1", "reason": "not needed" })),
    )
    .await?;
    Ok(())
}

/// The ids and generations of the portfolios a `notify` outcome lists as formed.
fn formed(outcome: &Value) -> Vec<(String, u64)> {
    outcome["formed"]
        .as_array()
        .map(|f| {
            f.iter()
                .filter_map(|p| Some((p["id"].as_str()?.to_owned(), p["generation"].as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}

/// A point's durable record.
async fn point(http: &reqwest::Client, ingress: &str, subject: &str) -> Value {
    call(http, ingress, "ActionPointService", subject, "record", None)
        .await
        .expect("read point")
}

/// A portfolio's durable record (`null` when never formed).
async fn portfolio(http: &reqwest::Client, ingress: &str, id: &str) -> Value {
    call(http, ingress, "PortfolioService", id, "record", None)
        .await
        .expect("read portfolio")
}

fn members(record: &Value) -> Vec<String> {
    record["members"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires GraphDB + Restate, worker endpoint reachable from the runtime (run with --ignored)"]
async fn the_monitor_groups_regroups_and_supersedes_portfolios() {
    let graphdb_url = common::env_or("GRAPHDB_URL", "http://localhost:7200");
    let ingress_url = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let admin_url = common::env_or("RESTATE_ADMIN_URL", "http://localhost:9070");
    let service_addr = common::service_addr(17);
    let callback_url = common::callback_url(17);
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
    let _slot = common::fixture_repo_slot().await.expect("fixture slot");
    match common::provision_repo(&http, &graphdb_url, REPO).await {
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

    // Unique ids and tag per run: the subject-keyed Restate objects persist across runs.
    let base = common::unique_base();
    let tag = format!("T{base}");
    let (so, mo_a, mo_b) = (base + 1, base + 2, base + 3);
    let so_name = format!("{tag}-S");
    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));
    let writer = GraphDbClient::new(&graphdb_url, REPO).expect("client");
    let materialize = async |record: ekg_domain::SourceRecord| {
        world.materialize(&record).await.expect("materialize");
        for derivation in ontology::STRUCTURAL_DERIVATIONS {
            writer.update(derivation.sparql).await.expect("derive");
        }
    };
    let manufacturing = |id: i64, label: &str| {
        common::rec(
            "mrp.production",
            id,
            &[("name", &format!("{tag}-{label}")), ("origin", &so_name)],
        )
    };

    let monitor = DriftMonitorService::new(
        Arc::clone(&world) as Arc<dyn WorldModel>,
        vec![tagged_rule(&tag)],
    )
    .with_coupling(drift::coupling_rules());
    let endpoint = Endpoint::builder()
        .bind(monitor)
        .bind(ActionPointService::default())
        .bind(PortfolioService::default())
        .build();
    common::spawn_endpoint(endpoint, &service_addr).await;
    let registered = common::register(&http, &admin_url, &callback_url).await;

    let so_iri = ontology::entity_iri("sale.order", so);
    let a_iri = ontology::entity_iri("mrp.production", mo_a);
    let b_iri = ontology::entity_iri("mrp.production", mo_b);
    let pair = portfolio_id(&[EntityUri(so_iri.clone()), EntityUri(a_iri.clone())]);
    let trio = portfolio_id(&[
        EntityUri(so_iri.clone()),
        EntityUri(a_iri.clone()),
        EntityUri(b_iri.clone()),
    ]);

    let run = async || -> Result<Vec<(String, Value)>, String> {
        registered.clone()?;
        let mut seen = Vec::new();

        // 1. The order and the first manufacturing order fulfilling it.
        materialize(common::rec("sale.order", so, &[("name", &so_name)])).await;
        materialize(manufacturing(mo_a, "A")).await;
        seen.push((
            "notify@1".to_owned(),
            notify(&http, &ingress_url, &changed("mrp.production", mo_a, 1)).await?,
        ));
        // A joint slate for the pair's first generation lands.
        call(
            &http,
            &ingress_url,
            "PortfolioService",
            &pair.0,
            "attach_slate",
            Some(joint_slate(1)),
        )
        .await?;
        seen.push((
            "pair".to_owned(),
            portfolio(&http, &ingress_url, &pair.0).await,
        ));
        seen.push(("so@1".to_owned(), point(&http, &ingress_url, &so_iri).await));
        seen.push(("a@1".to_owned(), point(&http, &ingress_url, &a_iri).await));

        // 2. A second manufacturing order fulfilling the same order joins them.
        materialize(manufacturing(mo_b, "B")).await;
        seen.push((
            "notify@2".to_owned(),
            notify(&http, &ingress_url, &changed("mrp.production", mo_b, 2)).await?,
        ));
        // The superseded pair refuses a late slate for its old generation.
        seen.push((
            "late@2".to_owned(),
            Value::String(
                call(
                    &http,
                    &ingress_url,
                    "PortfolioService",
                    &pair.0,
                    "attach_slate",
                    Some(joint_slate(1)),
                )
                .await
                .err()
                .unwrap_or_default(),
            ),
        ));
        seen.push((
            "trio".to_owned(),
            portfolio(&http, &ingress_url, &trio.0).await,
        ));
        seen.push((
            "pair@2".to_owned(),
            portfolio(&http, &ingress_url, &pair.0).await,
        ));
        for (label, iri) in [("so@2", &so_iri), ("a@2", &a_iri), ("b@2", &b_iri)] {
            seen.push((label.to_owned(), point(&http, &ingress_url, iri).await));
        }

        // 3. The same event redelivered.
        seen.push((
            "notify@3".to_owned(),
            notify(&http, &ingress_url, &changed("mrp.production", mo_b, 2)).await?,
        ));
        seen.push((
            "trio@3".to_owned(),
            portfolio(&http, &ingress_url, &trio.0).await,
        ));

        // 4. The stop control on the second manufacturing order.
        abort(&http, &ingress_url, &b_iri).await?;
        seen.push(("b@4".to_owned(), point(&http, &ingress_url, &b_iri).await));
        // The supersession is a one-way send: wait for it to land.
        common::wait_until(Duration::from_secs(30), async || {
            portfolio(&http, &ingress_url, &trio.0).await["state"] == "superseded"
        })
        .await;
        seen.push((
            "trio@4".to_owned(),
            portfolio(&http, &ingress_url, &trio.0).await,
        ));

        // 5. A stale redelivery (no point reopens) regroups the two live members: the pair
        //    re-forms as generation 2.
        seen.push((
            "notify@5".to_owned(),
            notify(&http, &ingress_url, &changed("mrp.production", mo_b, 2)).await?,
        ));
        seen.push((
            "late@5".to_owned(),
            Value::String(
                call(
                    &http,
                    &ingress_url,
                    "PortfolioService",
                    &pair.0,
                    "attach_slate",
                    Some(joint_slate(1)),
                )
                .await
                .err()
                .unwrap_or_default(),
            ),
        ));
        call(
            &http,
            &ingress_url,
            "PortfolioService",
            &pair.0,
            "attach_slate",
            Some(joint_slate(2)),
        )
        .await?;
        seen.push((
            "pair@5".to_owned(),
            portfolio(&http, &ingress_url, &pair.0).await,
        ));

        // 6. Aborting the pair's manufacturing order leaves the order alone.
        abort(&http, &ingress_url, &a_iri).await?;
        common::wait_until(Duration::from_secs(30), async || {
            portfolio(&http, &ingress_url, &pair.0).await["state"] == "superseded"
        })
        .await;
        seen.push((
            "notify@6".to_owned(),
            notify(&http, &ingress_url, &changed("mrp.production", mo_b, 2)).await?,
        ));
        Ok(seen)
    };
    let outcome = run().await;

    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
    let deregistered = common::deregister_checked(&http, &admin_url, &callback_url).await;

    let seen: std::collections::BTreeMap<String, Value> =
        outcome.expect("the scenario ran").into_iter().collect();
    for (label, value) in &seen {
        println!("{label}: {value}");
    }
    deregistered.expect("the test deployment deregistered");

    // 1. `notify` reports the formed pair at generation 1; the slate for it landed, audited.
    assert_eq!(formed(&seen["notify@1"]), [(pair.0.clone(), 1)]);
    assert!(
        seen["notify@1"]["released"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    let first = &seen["pair"];
    assert_eq!(first["slate_generation"], 1, "{first}");
    let attached = first["audit"]
        .as_array()
        .and_then(|a| a.last())
        .expect("an audit entry");
    assert_eq!(attached["event"], "slate_attached", "{attached}");
    assert_eq!(attached["generation"], 1);
    assert!(
        attached["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("joint slate attached: 0 option(s)")),
        "{attached}"
    );

    // One active portfolio listing both; each member records it.
    assert_eq!(first["state"], "active", "{first}");
    assert_eq!(first["generation"], 1);
    let mut expected = vec![a_iri.clone(), so_iri.clone()];
    expected.sort();
    assert_eq!(members(first), expected);
    assert_eq!(seen["so@1"]["portfolio"], pair.0.as_str());
    assert_eq!(seen["a@1"]["portfolio"], pair.0.as_str());
    assert_eq!(
        seen["so@1"]["point"]["state"], "open",
        "grouping transitions nothing"
    );

    // 2. The third coupled subject regroups them and supersedes the first; a late slate for
    //    the superseded pair is refused with 409.
    assert_eq!(formed(&seen["notify@2"]), [(trio.0.clone(), 1)]);
    assert!(
        seen["late@2"].as_str().is_some_and(|e| e.contains("409")),
        "{}",
        seen["late@2"]
    );
    let regrouped = &seen["trio"];
    assert_eq!(regrouped["state"], "active", "{regrouped}");
    assert_eq!(members(regrouped).len(), 3);
    let superseded = &seen["pair@2"];
    assert_eq!(superseded["state"], "superseded", "{superseded}");
    let last = superseded["audit"]
        .as_array()
        .and_then(|a| a.last())
        .expect("an audit entry");
    assert_eq!(last["event"], "superseded");
    assert!(
        last["reason"]
            .as_str()
            .is_some_and(|r| r.contains("regrouped")),
        "{last}"
    );
    for label in ["so@2", "a@2", "b@2"] {
        assert_eq!(seen[label]["portfolio"], trio.0.as_str(), "{label}");
    }

    // 3. The redelivered event formed nothing new.
    assert_eq!(seen["trio@3"], seen["trio"], "redelivery changes nothing");
    assert!(formed(&seen["notify@3"]).is_empty(), "{}", seen["notify@3"]);

    // 4. The aborted member left; its portfolio is superseded with the reason.
    let aborted = &seen["b@4"];
    assert_eq!(aborted["point"]["state"], "aborted");
    assert!(
        aborted.get("portfolio").is_none_or(Value::is_null),
        "{aborted}"
    );
    let after = &seen["trio@4"];
    assert_eq!(after["state"], "superseded", "{after}");
    let reason = after["audit"]
        .as_array()
        .and_then(|a| a.last())
        .and_then(|e| e["reason"].as_str())
        .expect("a supersession reason");
    assert!(
        reason.contains(&b_iri) && reason.contains("aborted"),
        "{reason}"
    );

    // 5. The pair re-formed as generation 2: the old generation's slate is refused (409), the
    //    new one lands.
    assert_eq!(formed(&seen["notify@5"]), [(pair.0.clone(), 2)]);
    assert!(
        seen["late@5"].as_str().is_some_and(|e| e.contains("409")),
        "{}",
        seen["late@5"]
    );
    let reformed = &seen["pair@5"];
    assert_eq!(reformed["generation"], 2, "{reformed}");
    assert_eq!(reformed["slate_generation"], 2, "{reformed}");

    // 6. The order, left on its own, is reported released.
    let released: Vec<&str> = seen["notify@6"]["released"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert_eq!(released, [so_iri.as_str()], "{}", seen["notify@6"]);
    assert!(formed(&seen["notify@6"]).is_empty());
}
