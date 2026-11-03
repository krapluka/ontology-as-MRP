//! Gated integration test for the throwaway fixture world against a live GraphDB.
//!
//! A fixture world is a repository of its own: records are materialized into it through the
//! mapping contract and the write-shapes, it reads like any world model, and its repository
//! is gone once it is discarded, also when its user panicked.
//!
//! Ignored by default; needs a reachable GraphDB (`GRAPHDB_URL`, default the local substrate):
//!
//! ```text
//! cargo test -p ekg-semantic --test it fixture_it -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown)]

use crate::common;

use std::time::Duration;

use common::{fixture_repo_slot, graphdb_url, infra_unavailable, rec};
use ekg_domain::{EntityUri, ReadContext, ReadPolicy, WorldModel};
use ekg_semantic::fixture::FixtureWorld;
use ekg_semantic::{GraphDbClient, ontology};

/// A prefix of this suite's own, so it never judges another suite's repositories.
const PREFIX: &str = "fixit_";

async fn has_repo(server: &GraphDbClient, repo: &str) -> bool {
    server
        .repository_ids()
        .await
        .expect("list repositories")
        .iter()
        .any(|id| id == repo)
}

/// Poll until `repo` is gone (a dropped world's repository drop is spawned), for up to five
/// seconds. GraphDB can fail a listing while a repository is being removed, so a failed
/// listing is asked again.
async fn gone(server: &GraphDbClient, repo: &str) -> bool {
    for _ in 0..50 {
        if let Ok(ids) = server.repository_ids().await
            && !ids.iter().any(|id| id == repo)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn a_fixture_world_is_loaded_read_back_and_dropped_even_after_a_panic() {
    let _slot = fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let server = GraphDbClient::new(&base, "ekg").expect("client");
    if server.repository_ids().await.is_err() {
        infra_unavailable(&format!("no GraphDB reachable at {base}"));
        return;
    }
    let records = vec![
        rec(
            "sale.order",
            42,
            &[
                ("name", "S00042"),
                ("commitment_date", "2027-10-12T00:00:00"),
            ],
        ),
        rec("mrp.workcenter", 1, &[("name", "Shared Line")]),
    ];
    let contract = || ontology::supply_risk_contract().expect("contract");

    let world = FixtureWorld::load(&server, PREFIX, contract(), &records)
        .await
        .expect("load");
    let repo = world.repo_id().to_owned();
    assert!(repo.starts_with(PREFIX), "{repo}");
    assert!(has_repo(&server, &repo).await);

    let model = world
        .world_model(contract(), ReadPolicy::permissive())
        .expect("world model");
    let order = EntityUri(ontology::entity_iri("sale.order", 42));
    let grounding = model
        .ground(
            &order,
            &ReadContext {
                role: "planner".to_owned(),
                purpose: "delivery-promise".to_owned(),
            },
        )
        .await
        .expect("the materialized order grounds");
    assert!(!grounding.edges.is_empty(), "{grounding:?}");

    world.discard().await;
    assert!(
        !has_repo(&server, &repo).await,
        "discard drops the repository"
    );

    // A user that panics while holding the world still leaves no repository behind.
    let (sent, received) = tokio::sync::oneshot::channel();
    let task_server = GraphDbClient::new(&base, "ekg").expect("client");
    let task = tokio::spawn(async move {
        let world = FixtureWorld::load(&task_server, PREFIX, contract(), &records)
            .await
            .expect("load");
        let _ = sent.send(world.repo_id().to_owned());
        panic!("the user of the world failed");
    });
    let leaked = received.await.expect("the repository id");
    assert!(task.await.is_err(), "the task panicked");
    assert!(gone(&server, &leaked).await, "{leaked} outlived its world");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable GraphDB (set GRAPHDB_URL; run with --ignored)"]
async fn a_fixture_the_mapping_refuses_leaves_no_repository() {
    let _slot = fixture_repo_slot()
        .await
        .expect("the fixture-repo slot is never closed");
    let base = graphdb_url();
    let server = GraphDbClient::new(&base, "ekg").expect("client");
    let Ok(before) = server.repository_ids().await else {
        infra_unavailable(&format!("no GraphDB reachable at {base}"));
        return;
    };
    // A model the contract does not map.
    let records = vec![rec("no.such.model", 1, &[("name", "x")])];
    let refused = FixtureWorld::load(
        &server,
        PREFIX,
        ontology::supply_risk_contract().expect("contract"),
        &records,
    )
    .await;
    assert!(refused.is_err(), "an unmapped model is refused");
    let after = server.repository_ids().await.expect("list");
    assert_eq!(
        after.iter().filter(|id| id.starts_with(PREFIX)).count(),
        before.iter().filter(|id| id.starts_with(PREFIX)).count(),
        "the refused world's repository was dropped"
    );
}
