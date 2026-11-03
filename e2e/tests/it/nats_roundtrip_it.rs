//! Gated live round-trip over NATS JetStream: publish an [`RdfChanged`] through the real
//! [`NatsPublisher`] and consume it back off the `EKG_RDF` stream, asserting the subject,
//! revision, and full payload survive.
//!
//! Ignored by default; started via testcontainers, so it needs a container runtime
//! (Podman/Docker). It skips cleanly (returns) when none is reachable. Run with:
//!
//! ```text
//! cargo test -p ekg-e2e --test it nats_roundtrip_it -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown)]

use async_nats::jetstream;
use ekg_backend::nats::{NatsPublisher, STREAM_NAME, STREAM_SUBJECTS};
use ekg_domain::{
    ChangeKind, EntityUri, NamedGraph, RdfChangePublisher, RdfChanged, Revision, SourceRef,
};
use futures::StreamExt;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// A representative change event on a versioned subject with a non-trivial revision.
fn event() -> RdfChanged {
    RdfChanged {
        entity_uri: EntityUri("https://ekg.example/id/sale.order/7".to_owned()),
        named_graph: NamedGraph("https://ekg.example/graph/sale.order/7".to_owned()),
        change_kind: ChangeKind::Created,
        source: SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 7,
        },
        revision: Revision(3),
    }
}

#[tokio::test]
#[ignore = "requires a container runtime (run with --ignored)"]
async fn publish_then_consume_preserves_subject_revision_and_payload() {
    // --- start NATS with JetStream; skip if no container runtime is available ---
    let container = match GenericImage::new("nats", "2.10.22-alpine")
        .with_exposed_port(4222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
    {
        Ok(container) => container,
        Err(why) => {
            eprintln!("SKIP: no container runtime for NATS: {why}");
            return;
        }
    };
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(4222.tcp())
        .await
        .expect("mapped 4222");
    let url = format!("nats://{host}:{port}");

    // --- publish through the real driver (which also ensures the EKG_RDF stream) ---
    let publisher = NatsPublisher::connect(&url).await.expect("connect");
    let sent = event();
    publisher.publish(&sent).await.expect("publish");

    // --- bind a consumer on the stream and read the one message back ---
    let js = publisher.context();
    let stream = js.get_stream(STREAM_NAME).await.expect("get stream");
    let consumer = stream
        .get_or_create_consumer(
            "roundtrip-it",
            jetstream::consumer::pull::Config {
                durable_name: Some("roundtrip-it".to_owned()),
                filter_subject: STREAM_SUBJECTS.to_owned(),
                ..Default::default()
            },
        )
        .await
        .expect("consumer");
    let mut messages = consumer.messages().await.expect("messages");
    let message = messages
        .next()
        .await
        .expect("a message is delivered")
        .expect("the message is ok");
    message.ack().await.expect("ack");

    // --- the subject, revision, and full payload survive the round-trip ---
    assert_eq!(
        message.subject.as_str(),
        sent.subject(),
        "published on ekg.rdf.v1.<model>.<kind>"
    );
    let got: RdfChanged = serde_json::from_slice(&message.payload).expect("decode RdfChanged");
    assert_eq!(got, sent, "the full event round-trips unchanged");
    assert_eq!(got.revision, Revision(3), "the revision survives");
}
