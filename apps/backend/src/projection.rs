//! The vector-projection consumer: a second, independent subscriber of the
//! `RdfChanged` fan-out that keeps a Qdrant similarity index of the world model
//! current. A change is grounded, embedded and upserted; a retraction deletes.
//!
//! Shaped like the drift monitor: one per-event handler ([`project_one`]) driven by
//! either the in-process broadcast ([`run_vector_projection`]) or a durable JetStream
//! consumer ([`run_vector_projection_nats`]) with its own durable name, so the monitor
//! and the projection track independent cursors.
//!
//! The index is a read model: it sources no facts, performs no governed writes, and can
//! be rebuilt by replaying the stream. It embeds a least-privilege, read-policy-filtered
//! grounding, so policy-sensitive nodes and edges never enter the shared index.

use std::sync::Arc;

use async_nats::jetstream;
use async_trait::async_trait;
use ekg_domain::{
    ChangeKind, CognitionProvider, Grounding, RdfChanged, ReadContext, Result, Revision, WorldModel,
};
use futures::StreamExt;
use tokio::sync::broadcast::{self, error::RecvError};

/// The durable JetStream consumer name for the projection; distinct from the
/// monitor bridge's so JetStream tracks the two cursors independently.
const VECTOR_CONSUMER: &str = "ekg-vector-projection";

/// The fixed, least-privilege read-context the projection grounds under. A shared,
/// pre-computed index cannot be re-filtered per reader, so it must hold only
/// least-privilege content; a real reader still grounds under its own role at query
/// time. Sensitive nodes and edges are omitted without being reported as omissions,
/// since this is an index build and not an agent-facing grounding.
fn projection_context() -> ReadContext {
    ReadContext {
        role: "projection".to_owned(),
        purpose: "vector-index".to_owned(),
    }
}

/// The write side of the vector projection: the [`QdrantClient`](crate::QdrantClient)
/// in production, an in-memory fake in tests, so [`project_one`] (the revision guard
/// and the ground/embed/upsert flow) can be unit-tested without a live store.
#[async_trait]
pub trait VectorSink: Send + Sync {
    /// The `revision` stored on `entity_uri`'s point, or `None` if absent.
    async fn revision_of(&self, entity_uri: &str) -> Result<Option<Revision>>;
    /// Upserts the point for `entity_uri` with its embedding and payload.
    async fn upsert(
        &self,
        entity_uri: &str,
        model: &str,
        subject: &str,
        revision: Revision,
        vector: Vec<f32>,
    ) -> Result<()>;
    /// Deletes the point for `entity_uri` (idempotent).
    async fn delete(&self, entity_uri: &str) -> Result<()>;
}

#[async_trait]
impl VectorSink for crate::QdrantClient {
    async fn revision_of(&self, entity_uri: &str) -> Result<Option<Revision>> {
        crate::QdrantClient::revision_of(self, entity_uri).await
    }
    async fn upsert(
        &self,
        entity_uri: &str,
        model: &str,
        subject: &str,
        revision: Revision,
        vector: Vec<f32>,
    ) -> Result<()> {
        crate::QdrantClient::upsert(self, entity_uri, model, subject, revision, vector).await
    }
    async fn delete(&self, entity_uri: &str) -> Result<()> {
        crate::QdrantClient::delete(self, entity_uri).await
    }
}

/// Renders a grounding to the text embedded for the entity: the entity URI followed by
/// one `predicate object` line per visible edge. Deterministic, so re-projecting an
/// unchanged entity embeds identically.
fn embed_text(grounding: &Grounding) -> String {
    let mut text = format!("entity: {}", grounding.entity.0);
    for edge in &grounding.edges {
        text.push('\n');
        text.push_str(&edge.predicate);
        text.push(' ');
        text.push_str(&edge.object);
    }
    text
}

/// Drives one change event into the projection: on `Retracted` it deletes the point;
/// otherwise it applies the per-entity monotonic-revision guard, then grounds, embeds
/// and upserts. Best-effort: any failure is logged and not fatal, because the index can
/// be rebuilt by stream replay. The guard makes at-least-once and out-of-order delivery
/// safe.
pub async fn project_one(
    world: &dyn WorldModel,
    cognition: &dyn CognitionProvider,
    sink: &dyn VectorSink,
    event: &RdfChanged,
) {
    let entity = &event.entity_uri;

    // A retraction removes the point regardless of revision: a delete is idempotent,
    // and a later re-creation carries a higher revision and re-adds it.
    if event.change_kind == ChangeKind::Retracted {
        if let Err(why) = sink.delete(&entity.0).await {
            eprintln!(
                "warning: vector projection: delete failed for {} ({why})",
                entity.0
            );
        }
        return;
    }

    // Ignore an event that is not newer than the point's stored revision, so a replayed
    // or out-of-order delivery cannot regress the index.
    match sink.revision_of(&entity.0).await {
        Ok(Some(stored)) if event.revision <= stored => return,
        Ok(_) => {}
        Err(why) => {
            eprintln!(
                "warning: vector projection: revision lookup failed for {} ({why}); skipping",
                entity.0
            );
            return;
        }
    }

    // Ground under the least-privilege projection context.
    let grounding = match world.ground(entity, &projection_context()).await {
        Ok(grounding) => grounding,
        Err(why) => {
            eprintln!(
                "warning: vector projection: grounding failed for {} ({why})",
                entity.0
            );
            return;
        }
    };
    // The provider boundary enforces the pinned embedding width.
    let vector = match cognition.embed(&embed_text(&grounding)).await {
        Ok(vector) => vector,
        Err(why) => {
            eprintln!(
                "warning: vector projection: embed failed for {} ({why})",
                entity.0
            );
            return;
        }
    };
    if let Err(why) = sink
        .upsert(
            &entity.0,
            &event.source.model,
            &event.subject(),
            event.revision,
            vector,
        )
        .await
    {
        eprintln!(
            "warning: vector projection: upsert failed for {} ({why})",
            entity.0
        );
    }
}

/// Runs the projection off the in-process broadcast fan-out (a second subscriber
/// alongside the monitor bridge). Returns when the publisher is dropped.
pub async fn run_vector_projection(
    mut rx: broadcast::Receiver<RdfChanged>,
    world: Arc<dyn WorldModel>,
    cognition: Arc<dyn CognitionProvider>,
    sink: Arc<dyn VectorSink>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                project_one(world.as_ref(), cognition.as_ref(), sink.as_ref(), &event).await;
            }
            // Dropped events are re-derived on the next change or a full rebuild.
            Err(RecvError::Lagged(skipped)) => eprintln!(
                "warning: vector projection lagged, {skipped} events skipped (rebuildable by replay)"
            ),
            Err(RecvError::Closed) => break,
        }
    }
}

/// Runs the projection off a durable NATS JetStream pull consumer over `ekg.rdf.v1.>`
/// (its own durable name, distinct from the monitor). A decode or handler error is
/// logged and the message acked; stream replay and the revision guard cover
/// correctness. Returns when the stream ends or the consumer cannot bind.
pub async fn run_vector_projection_nats(
    js: jetstream::Context,
    world: Arc<dyn WorldModel>,
    cognition: Arc<dyn CognitionProvider>,
    sink: Arc<dyn VectorSink>,
) {
    let stream = match js.get_stream(crate::nats::STREAM_NAME).await {
        Ok(stream) => stream,
        Err(why) => {
            eprintln!(
                "warning: vector projection: NATS stream {} unavailable ({why})",
                crate::nats::STREAM_NAME
            );
            return;
        }
    };
    let consumer = match stream
        .get_or_create_consumer(
            VECTOR_CONSUMER,
            jetstream::consumer::pull::Config {
                durable_name: Some(VECTOR_CONSUMER.to_owned()),
                filter_subject: crate::nats::STREAM_SUBJECTS.to_owned(),
                ..Default::default()
            },
        )
        .await
    {
        Ok(consumer) => consumer,
        Err(why) => {
            eprintln!("warning: vector projection: could not bind the NATS consumer ({why})");
            return;
        }
    };
    let mut messages = match consumer.messages().await {
        Ok(messages) => messages,
        Err(why) => {
            eprintln!("warning: vector projection: could not stream NATS messages ({why})");
            return;
        }
    };

    while let Some(next) = messages.next().await {
        let message = match next {
            Ok(message) => message,
            Err(why) => {
                eprintln!("warning: vector projection: NATS message error ({why})");
                continue;
            }
        };
        match serde_json::from_slice::<RdfChanged>(&message.payload) {
            Ok(event) => {
                project_one(world.as_ref(), cognition.as_ref(), sink.as_ref(), &event).await;
            }
            Err(why) => {
                eprintln!("warning: vector projection: undecodable RdfChanged, dropping ({why})");
            }
        }
        if let Err(why) = message.ack().await {
            eprintln!("warning: vector projection: NATS ack failed ({why})");
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use ekg_domain::{EngineError, EntityUri, EvidenceEdge, NamedGraph, SourceRecord, SourceRef};

    use super::*;

    /// A world model that returns a single canned grounding for `ground`; the other
    /// seam methods are unused by the projection and error if called.
    struct FakeWorld {
        grounding: Grounding,
    }

    #[async_trait]
    impl WorldModel for FakeWorld {
        async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
            Ok(self.grounding.clone())
        }
        async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            Err(EngineError::Backend("unused".to_owned()))
        }
    }

    /// A cognition provider whose `embed` returns a fixed vector and records that it
    /// was called; `generate` is unused.
    struct FakeCognition {
        embed_calls: Mutex<u32>,
    }

    #[async_trait]
    impl CognitionProvider for FakeCognition {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            *self.embed_calls.lock().expect("lock") += 1;
            Ok(vec![0.1, 0.2, 0.3, 0.4])
        }
    }

    /// An in-memory vector sink recording upserts/deletes and serving a preset stored
    /// revision for the guard.
    #[derive(Default)]
    struct FakeSink {
        stored_revision: Option<Revision>,
        upserts: Mutex<Vec<(String, Revision)>>,
        deletes: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl VectorSink for FakeSink {
        async fn revision_of(&self, _entity_uri: &str) -> Result<Option<Revision>> {
            Ok(self.stored_revision)
        }
        async fn upsert(
            &self,
            entity_uri: &str,
            _model: &str,
            _subject: &str,
            revision: Revision,
            _vector: Vec<f32>,
        ) -> Result<()> {
            self.upserts
                .lock()
                .expect("lock")
                .push((entity_uri.to_owned(), revision));
            Ok(())
        }
        async fn delete(&self, entity_uri: &str) -> Result<()> {
            self.deletes
                .lock()
                .expect("lock")
                .push(entity_uri.to_owned());
            Ok(())
        }
    }

    fn grounding(entity: &str) -> Grounding {
        Grounding {
            entity: EntityUri(entity.to_owned()),
            edges: vec![EvidenceEdge {
                subject: EntityUri(entity.to_owned()),
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
        }
    }

    fn event(kind: ChangeKind, revision: u64) -> RdfChanged {
        RdfChanged {
            entity_uri: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            named_graph: NamedGraph("https://ekg.example/graph/sale.order/1".to_owned()),
            change_kind: kind,
            source: SourceRef {
                system: "odoo".to_owned(),
                model: "sale.order".to_owned(),
                id: 1,
            },
            revision: Revision(revision),
        }
    }

    #[tokio::test]
    async fn a_created_event_grounds_embeds_and_upserts() {
        let world = FakeWorld {
            grounding: grounding("https://ekg.example/id/sale.order/1"),
        };
        let cognition = FakeCognition {
            embed_calls: Mutex::new(0),
        };
        let sink = FakeSink::default(); // no stored revision yet
        project_one(&world, &cognition, &sink, &event(ChangeKind::Created, 2)).await;
        assert_eq!(*cognition.embed_calls.lock().expect("lock"), 1);
        let upserts = sink.upserts.lock().expect("lock");
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].1, Revision(2));
        assert!(sink.deletes.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn a_stale_event_is_a_no_op() {
        let world = FakeWorld {
            grounding: grounding("https://ekg.example/id/sale.order/1"),
        };
        let cognition = FakeCognition {
            embed_calls: Mutex::new(0),
        };
        let sink = FakeSink {
            stored_revision: Some(Revision(5)),
            ..Default::default()
        };
        // Revision 5 is not greater than the stored 5, so it is skipped before embedding.
        project_one(&world, &cognition, &sink, &event(ChangeKind::Updated, 5)).await;
        assert_eq!(
            *cognition.embed_calls.lock().expect("lock"),
            0,
            "a non-increasing revision must not embed or upsert"
        );
        assert!(sink.upserts.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn a_newer_update_upserts_over_the_stored_revision() {
        let world = FakeWorld {
            grounding: grounding("https://ekg.example/id/sale.order/1"),
        };
        let cognition = FakeCognition {
            embed_calls: Mutex::new(0),
        };
        let sink = FakeSink {
            stored_revision: Some(Revision(5)),
            ..Default::default()
        };
        project_one(&world, &cognition, &sink, &event(ChangeKind::Updated, 6)).await;
        let upserts = sink.upserts.lock().expect("lock");
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].1, Revision(6));
    }

    #[tokio::test]
    async fn a_retraction_deletes_without_grounding() {
        let world = FakeWorld {
            grounding: grounding("https://ekg.example/id/sale.order/1"),
        };
        let cognition = FakeCognition {
            embed_calls: Mutex::new(0),
        };
        let sink = FakeSink {
            stored_revision: Some(Revision(3)),
            ..Default::default()
        };
        project_one(&world, &cognition, &sink, &event(ChangeKind::Retracted, 4)).await;
        let deletes = sink.deletes.lock().expect("lock");
        assert_eq!(deletes.len(), 1);
        assert_eq!(*cognition.embed_calls.lock().expect("lock"), 0);
        assert!(sink.upserts.lock().expect("lock").is_empty());
    }
}
