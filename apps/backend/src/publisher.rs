//! In-process [`RdfChangePublisher`]: a `tokio::broadcast` fan-out.
//!
//! Ingestion publishes `RdfChanged` through the seam after every mutation, and the NATS
//! driver sits behind the same seam, so the ingest path does not know which one it has.
//! Independent consumers (monitor, projections) subscribe without polling the store.

use async_trait::async_trait;
use ekg_domain::{RdfChanged, Result};
use tokio::sync::broadcast;

/// A broadcast-backed publisher. Cloneable handle over one channel.
#[derive(Clone)]
pub struct InProcessPublisher {
    tx: broadcast::Sender<RdfChanged>,
}

impl InProcessPublisher {
    /// Build a publisher whose channel buffers up to `capacity` lagging events.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe a new consumer to the fan-out.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RdfChanged> {
        self.tx.subscribe()
    }
}

/// The `RdfChangePublisher` seam. Defined here to avoid a hard dependency on the
/// seam's location while the trait lives in `ekg_domain`.
#[async_trait]
impl ekg_domain::RdfChangePublisher for InProcessPublisher {
    async fn publish(&self, event: &RdfChanged) -> Result<()> {
        // `send` errors only when there are no live receivers. A consumer may not be
        // attached yet, so that must not fail the write path.
        let _ = self.tx.send(event.clone());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ekg_domain::{
        ChangeKind, EntityUri, NamedGraph, RdfChangePublisher, RdfChanged, Revision, SourceRef,
    };

    use super::*;

    fn event(revision: u64) -> RdfChanged {
        RdfChanged {
            entity_uri: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            named_graph: NamedGraph("https://ekg.example/graph/sale.order/1".to_owned()),
            change_kind: ChangeKind::Created,
            source: SourceRef {
                system: "odoo".to_owned(),
                model: "sale.order".to_owned(),
                id: 1,
            },
            revision: Revision(revision),
        }
    }

    #[tokio::test]
    async fn published_event_reaches_a_subscriber_with_its_revision() {
        let publisher = InProcessPublisher::new(16);
        let mut rx = publisher.subscribe();
        publisher.publish(&event(1)).await.expect("publish");
        let got = rx.try_recv().expect("subscriber receives the event");
        assert_eq!(got.revision, Revision(1));
        assert_eq!(got.subject(), "ekg.rdf.v1.sale.order.created");
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_ok() {
        let publisher = InProcessPublisher::new(16);
        assert!(publisher.publish(&event(2)).await.is_ok());
    }
}
