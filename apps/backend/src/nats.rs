//! NATS JetStream [`RdfChangePublisher`]: the durable event backbone.
//!
//! Publishes each [`RdfChanged`] (JSON) to its [`subject()`](RdfChanged::subject)
//! on a JetStream stream ([`STREAM_NAME`] over [`STREAM_SUBJECTS`]) and awaits the
//! PubAck, so a transport failure surfaces. The ingest worker logs it without failing
//! and the reconciliation sweep backfills. The in-process
//! [`InProcessPublisher`](crate::InProcessPublisher) is the hermetic fake behind the
//! same seam; this is the durable, cross-process driver a restart replays from.

use async_nats::jetstream::{self, stream::Config as StreamConfig};
use async_trait::async_trait;
use ekg_domain::{EngineError, RdfChanged, Result};

/// The JetStream stream capturing every versioned RDF-change subject.
pub const STREAM_NAME: &str = "EKG_RDF";
/// The subject wildcard the stream binds: all `ekg.rdf.v1.<model>.<kind>` events.
pub const STREAM_SUBJECTS: &str = "ekg.rdf.v1.>";

/// A JetStream-backed publisher: a cloneable handle over the JetStream
/// [`Context`](jetstream::Context).
#[derive(Clone)]
pub struct NatsPublisher {
    js: jetstream::Context,
}

impl NatsPublisher {
    /// Connects to `url` and makes sure the [`STREAM_NAME`] stream binds
    /// [`STREAM_SUBJECTS`] (create-or-update, so safe to repeat).
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the broker is unreachable or the stream cannot be
    /// created/updated.
    pub async fn connect(url: &str) -> Result<Self> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| EngineError::Backend(format!("nats connect {url}: {e}")))?;
        let js = jetstream::new(client);
        js.get_or_create_stream(StreamConfig {
            name: STREAM_NAME.to_owned(),
            subjects: vec![STREAM_SUBJECTS.to_owned()],
            ..Default::default()
        })
        .await
        .map_err(|e| EngineError::Backend(format!("nats ensure stream {STREAM_NAME}: {e}")))?;
        Ok(Self { js })
    }

    /// The JetStream context, for wiring the monitor's durable consumer against the
    /// same connection (see [`crate::drift::run_monitor_bridge_nats`]).
    #[must_use]
    pub fn context(&self) -> jetstream::Context {
        self.js.clone()
    }
}

#[async_trait]
impl ekg_domain::RdfChangePublisher for NatsPublisher {
    async fn publish(&self, event: &RdfChanged) -> Result<()> {
        let payload = serde_json::to_vec(event)
            .map_err(|e| EngineError::Backend(format!("encode RdfChanged: {e}")))?;
        // The PubAck is where a store failure shows up; without awaiting it the event
        // would be dropped silently.
        let ack = self
            .js
            .publish(event.subject(), payload.into())
            .await
            .map_err(|e| EngineError::Backend(format!("nats publish: {e}")))?;
        ack.await
            .map_err(|e| EngineError::Backend(format!("nats puback: {e}")))?;
        Ok(())
    }
}
