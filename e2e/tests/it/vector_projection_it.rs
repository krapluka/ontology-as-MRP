//! Gated live round-trip for the vector projection: drive the durable NATS JetStream
//! consumer against a real Qdrant and assert the index tracks the world model.
//!
//! Ignored by default; started via testcontainers, so it needs a container runtime
//! (Podman/Docker). It skips (returns) when none is reachable. Run with:
//!
//! ```text
//! cargo test -p ekg-e2e --test it vector_projection_it -- --ignored
//! ```
//!
//! Covers: a `Created`/`Updated` event upserts a point at the pinned embedding width
//! carrying the newer revision (over NATS end to end); a stale (non-increasing)
//! revision is a no-op (the per-entity guard, checked directly against real Qdrant);
//! a `Retracted` deletes the point.

#![allow(clippy::expect_used, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ekg_backend::nats::NatsPublisher;
use ekg_backend::projection::{self, VectorSink};
use ekg_backend::qdrant::{QdrantClient, point_id};
use ekg_domain::{
    ChangeKind, CognitionProvider, EntityUri, EvidenceEdge, Grounding, NamedGraph,
    RdfChangePublisher, RdfChanged, ReadContext, Result, Revision, SourceRecord, SourceRef,
    WorldModel,
};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// The pinned embedding width used for the throwaway collection (and the fake embed).
const DIM: usize = 4;

/// A world model returning one canned grounding; other seam methods are unused.
struct FakeWorld;

#[async_trait]
impl WorldModel for FakeWorld {
    async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
        unreachable!("projection never materializes")
    }
    async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
        unreachable!("projection never retracts via the world model")
    }
    async fn ground(&self, entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
        Ok(Grounding {
            entity: entity.clone(),
            edges: vec![EvidenceEdge {
                subject: entity.clone(),
                predicate: "ekg:commitment_date".to_owned(),
                object: "2027-10-07".to_owned(),
                source: SourceRef {
                    system: "odoo".to_owned(),
                    model: "sale.order".to_owned(),
                    id: 1,
                },
                revision: Revision(1),
            }],
            omissions: vec![],
        })
    }
    async fn select(
        &self,
        _sparql: &str,
    ) -> Result<Vec<std::collections::BTreeMap<String, String>>> {
        unreachable!("projection never selects")
    }
}

/// A cognition provider whose `embed` returns a fixed vector of the pinned width.
struct FakeCognition;

#[async_trait]
impl CognitionProvider for FakeCognition {
    async fn generate(&self, _prompt: &str) -> Result<String> {
        unreachable!("projection never generates")
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(vec![0.10, 0.20, 0.30, 0.40])
    }
}

fn event(entity: &str, kind: ChangeKind, revision: u64) -> RdfChanged {
    RdfChanged {
        entity_uri: EntityUri(entity.to_owned()),
        named_graph: NamedGraph(format!("{entity}/graph")),
        change_kind: kind,
        source: SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 1,
        },
        revision: Revision(revision),
    }
}

/// Poll `revision_of(entity)` until it satisfies `want`, or return the last value
/// after ~10s (the caller asserts on it).
async fn poll_revision(
    qdrant: &QdrantClient,
    entity: &str,
    want: impl Fn(Option<Revision>) -> bool,
) -> Option<Revision> {
    for _ in 0..40 {
        let got = qdrant.revision_of(entity).await.expect("revision_of");
        if want(got) {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    qdrant.revision_of(entity).await.expect("revision_of")
}

// A sequential end-to-end scenario (start containers, create, update, guard, retract),
// so it is long but flat.
#[allow(clippy::too_many_lines)]
#[tokio::test]
#[ignore = "requires a container runtime (run with --ignored)"]
async fn projection_tracks_the_world_model_over_nats_and_qdrant() {
    // --- start NATS (JetStream) and Qdrant; skip if no container runtime ---
    let nats = match GenericImage::new("nats", "2.10.22-alpine")
        .with_exposed_port(4222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
    {
        Ok(c) => c,
        Err(why) => {
            eprintln!("SKIP: no container runtime for NATS: {why}");
            return;
        }
    };
    let qdrant_container = match GenericImage::new("qdrant/qdrant", "v1.12.4")
        .with_exposed_port(6333.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Qdrant HTTP listening on 6333"))
        .start()
        .await
    {
        Ok(c) => c,
        Err(why) => {
            eprintln!("SKIP: no container runtime for Qdrant: {why}");
            return;
        }
    };

    let nats_url = format!(
        "nats://{}:{}",
        nats.get_host().await.expect("host"),
        nats.get_host_port_ipv4(4222.tcp()).await.expect("port")
    );
    let qdrant_url = format!(
        "http://{}:{}",
        qdrant_container.get_host().await.expect("host"),
        qdrant_container
            .get_host_port_ipv4(6333.tcp())
            .await
            .expect("port")
    );

    // --- wire the real drivers and fakes and spawn the durable NATS consumer ---
    let publisher = NatsPublisher::connect(&nats_url)
        .await
        .expect("nats connect");
    let qdrant = Arc::new(
        QdrantClient::connect(&qdrant_url, "ekg_rdf", DIM)
            .await
            .expect("qdrant connect"),
    );
    let world = Arc::new(FakeWorld) as Arc<dyn WorldModel>;
    let cognition = Arc::new(FakeCognition) as Arc<dyn CognitionProvider>;
    tokio::spawn(projection::run_vector_projection_nats(
        publisher.context(),
        world,
        cognition,
        Arc::clone(&qdrant) as Arc<dyn VectorSink>,
    ));

    let order = "https://ekg.example/id/sale.order/1";

    // --- Created then Updated: the point tracks the newer revision (over NATS) ---
    publisher
        .publish(&event(order, ChangeKind::Created, 1))
        .await
        .expect("publish created");
    assert_eq!(
        poll_revision(&qdrant, order, |r| r == Some(Revision(1))).await,
        Some(Revision(1)),
        "the created entity is projected"
    );
    // The stored vector has the pinned width.
    assert_eq!(fetch_vector_len(&qdrant_url, order).await, DIM);

    publisher
        .publish(&event(order, ChangeKind::Updated, 3))
        .await
        .expect("publish updated");
    assert_eq!(
        poll_revision(&qdrant, order, |r| r == Some(Revision(3))).await,
        Some(Revision(3)),
        "a newer revision overwrites the point"
    );

    // --- the per-entity revision guard: a stale event is a no-op (direct, real Qdrant) ---
    let other = "https://ekg.example/id/sale.order/2";
    let world2 = FakeWorld;
    let cognition2 = FakeCognition;
    projection::project_one(
        &world2,
        &cognition2,
        qdrant.as_ref(),
        &event(other, ChangeKind::Created, 5),
    )
    .await;
    assert_eq!(
        qdrant.revision_of(other).await.expect("rev"),
        Some(Revision(5))
    );
    // Revision 5 is not greater than the stored 5, so it is ignored.
    projection::project_one(
        &world2,
        &cognition2,
        qdrant.as_ref(),
        &event(other, ChangeKind::Updated, 5),
    )
    .await;
    assert_eq!(
        qdrant.revision_of(other).await.expect("rev"),
        Some(Revision(5)),
        "a non-increasing revision must not change the point"
    );

    // --- Retracted deletes the point (over NATS) ---
    publisher
        .publish(&event(order, ChangeKind::Retracted, 4))
        .await
        .expect("publish retracted");
    assert_eq!(
        poll_revision(&qdrant, order, |r| r.is_none()).await,
        None,
        "a retraction removes the point"
    );
}

/// Fetch a point's stored vector length via the Qdrant REST API (the client does not
/// expose the vector, so the test reads it directly to assert the pinned width).
async fn fetch_vector_len(qdrant_url: &str, entity: &str) -> usize {
    let url = format!(
        "{}/collections/ekg_rdf/points/{}?with_vector=true",
        qdrant_url.trim_end_matches('/'),
        point_id(entity)
    );
    let body = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .expect("get point")
        .text()
        .await
        .expect("body");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("json");
    doc.pointer("/result/vector")
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .expect("a vector array")
}
