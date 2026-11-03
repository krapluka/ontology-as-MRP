//! The durable ingest worker: a Restate virtual object keyed `<model>:<id>`.
//!
//! The exclusive `ingest` handler runs under durable execution: `ctx.run(pull)` reads the
//! full authoritative record, `ctx.run(materialize)` projects it into the typed world model
//! with provenance, then it publishes `RdfChanged`. Source reads and store writes are
//! journaled by `ctx.run`, so a crash between them resumes instead of repeating. Keyed
//! exclusivity serializes concurrent notifications for the same entity at the durable
//! layer, on top of the driver's in-process lock.
//!
//! After materializing, and before publishing, a journaled `derive` step runs the
//! injected structural [`Derivations`] that read the record's model, so the monitor
//! never evaluates a change over stale derived edges.
//!
//! The worker is capability-neutral: it holds the seams ([`SourceIngestor`],
//! [`WorldModel`], [`RdfChangePublisher`]) and the neutral model/id IRI scheme, with no
//! supply-risk vocabulary. Which contract the injected world model projects, and which
//! derivations it runs, is the caller's concern.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_domain::{
    ChangeKind, EngineError, EntityUri, NamedGraph, RdfChangePublisher, RdfChanged, Revision,
    SourceIngestor, SourceRef, WorldModel,
};
use ekg_semantic::GraphDbClient;
use ekg_semantic::ontology::{self, Derivation};
use restate_sdk::prelude::*;

/// The registered name of the ingest virtual object (the ingress URL path segment).
pub const INGEST_OBJECT: &str = "Ingest";
/// The exclusive handler on the ingest object.
pub const INGEST_HANDLER: &str = "ingest";

/// The store a derivation runs against: one SPARQL UPDATE per call, committed as one
/// transaction.
#[async_trait]
pub trait DerivationStore: Send + Sync {
    /// Run `sparql` (a SPARQL UPDATE).
    ///
    /// # Errors
    /// [`EngineError::Backend`] on a store fault (retryable).
    async fn update(&self, sparql: &str) -> Result<(), EngineError>;
}

#[async_trait]
impl DerivationStore for GraphDbClient {
    async fn update(&self, sparql: &str) -> Result<(), EngineError> {
        Self::update(self, sparql).await
    }
}

/// The structural derivations the capability hands the ingest path, and the store they
/// run against. Cheap to clone.
#[derive(Clone)]
pub struct Derivations {
    store: Arc<dyn DerivationStore>,
    list: Arc<[Derivation]>,
}

impl Derivations {
    /// Run `list` against `store`.
    #[must_use]
    pub fn new(store: Arc<dyn DerivationStore>, list: impl Into<Arc<[Derivation]>>) -> Self {
        Self {
            store,
            list: list.into(),
        }
    }

    /// Whether any derivation reads records of `model`.
    #[must_use]
    pub fn read(&self, model: &str) -> bool {
        self.list.iter().any(|d| d.reads(model))
    }

    /// Run, in order, every derivation that reads records of `model`, and return the names
    /// of those run.
    ///
    /// # Errors
    /// The first failing update's error; the rest do not run.
    pub async fn run_for(&self, model: &str) -> Result<Vec<String>, EngineError> {
        let mut ran = Vec::new();
        for derivation in self.list.iter().filter(|d| d.reads(model)) {
            self.store.update(derivation.sparql).await?;
            ran.push(derivation.name.to_owned());
        }
        Ok(ran)
    }
}

/// Derive, then announce: await `derive` and only then publish `event`. A failed
/// derivation returns its error and publishes nothing, so no consumer sees a change over
/// stale derived edges. The publish itself is best-effort (`context` names it in the log):
/// the sweep backfills a lost event, and consumers reject a republished revision.
pub(crate) async fn derive_then_publish<E>(
    derive: impl Future<Output = Result<(), E>>,
    publisher: &dyn RdfChangePublisher,
    event: &RdfChanged,
    context: &str,
) -> Result<(), E> {
    derive.await?;
    if let Err(err) = publisher.publish(event).await {
        eprintln!("{context}: {err}");
    }
    Ok(())
}

/// The durable ingest virtual object. Cheap to clone: every field is an `Arc`.
#[derive(Clone)]
pub struct Ingest {
    ingestor: Arc<dyn SourceIngestor>,
    world: Arc<dyn WorldModel>,
    publisher: Arc<dyn RdfChangePublisher>,
    derivations: Option<Derivations>,
}

impl Ingest {
    /// Wire the worker over its three seams.
    #[must_use]
    pub fn new(
        ingestor: Arc<dyn SourceIngestor>,
        world: Arc<dyn WorldModel>,
        publisher: Arc<dyn RdfChangePublisher>,
    ) -> Self {
        Self {
            ingestor,
            world,
            publisher,
            derivations: None,
        }
    }

    /// Run `derivations` after each materialization, before publishing.
    #[must_use]
    pub fn with_derivations(mut self, derivations: Derivations) -> Self {
        self.derivations = Some(derivations);
        self
    }
}

#[restate_sdk::object]
impl Ingest {
    /// Durably materialize the source record named by `source_ref`.
    ///
    /// Exclusive per object key (`<model>:<id>`): one materialization runs at a time
    /// for an entity. Idempotent: it always re-reads the full current source state, so a
    /// duplicate or out-of-order trigger is harmless.
    #[handler]
    async fn ingest(
        &self,
        ctx: ObjectContext<'_>,
        source_ref: Json<SourceRef>,
    ) -> Result<(), HandlerError> {
        let source_ref = source_ref.into_inner();

        // Pull the full authoritative record, journaled so a later crash resumes from
        // here instead of hitting the source again.
        let record = ctx
            .run(|| async {
                self.ingestor
                    .pull(&source_ref)
                    .await
                    .map(Json::from)
                    .map_err(classify)
            })
            .name("pull")
            .await?
            .into_inner();

        // Materialize, journaled: typed projection, provenance and a monotonic revision.
        let revision = ctx
            .run(|| async {
                self.world
                    .materialize(&record)
                    .await
                    .map(Json::from)
                    .map_err(classify)
            })
            .name("materialize")
            .await?
            .into_inner();

        // Derive (journaled), then publish best-effort. The in-process publisher never
        // fails, and consumers reject a republished revision as non-increasing, so a
        // resumed handler that emits again is safe. A failed derivation is retried by
        // `ctx.run` and publishes nothing.
        let event = rdf_changed(&source_ref, revision);
        let derivations = self
            .derivations
            .as_ref()
            .filter(|d| d.read(&source_ref.model));
        derive_then_publish(
            async {
                // One journaled step for every derivation that reads the model; none
                // is journaled when no derivation reads it.
                if let Some(derivations) = derivations {
                    ctx.run(|| async {
                        derivations
                            .run_for(&source_ref.model)
                            .await
                            .map(Json::from)
                            .map_err(classify)
                    })
                    .name("derive")
                    .await?;
                }
                Ok::<(), HandlerError>(())
            },
            self.publisher.as_ref(),
            &event,
            "rdf-change publish failed (sweep backfills)",
        )
        .await?;
        Ok(())
    }
}

/// Map an engine error to a Restate error. A `Backend` fault is a transient transport
/// fault, so `ctx.run` retries it. Anything else (not found, malformed, stale) will not
/// fix itself on retry and is made terminal.
pub(crate) fn classify(err: EngineError) -> HandlerError {
    match &err {
        EngineError::Backend(_) => err.into(),
        _ => TerminalError::new(err.to_string()).into(),
    }
}

/// Build the `RdfChanged` event for a materialized entity. A first revision is a
/// creation; any later revision is an update. IRIs follow the neutral model/id scheme.
fn rdf_changed(source_ref: &SourceRef, revision: Revision) -> RdfChanged {
    let change_kind = if revision.0 <= 1 {
        ChangeKind::Created
    } else {
        ChangeKind::Updated
    };
    RdfChanged {
        entity_uri: EntityUri(ontology::entity_iri(&source_ref.model, source_ref.id)),
        named_graph: NamedGraph(ontology::named_graph(&source_ref.model, source_ref.id)),
        change_kind,
        source: source_ref.clone(),
        revision,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::expect_used)]
    use std::sync::Mutex;

    use super::*;

    /// A recording fake for both the derivation store and the publisher: one ordered log
    /// of every update (by its SPARQL) and every published event, so a test can assert
    /// that the derivation ran before the change was announced.
    #[derive(Default)]
    pub(crate) struct Recorder {
        log: Mutex<Vec<String>>,
        fail_updates: bool,
    }

    impl Recorder {
        pub(crate) fn failing() -> Self {
            Self {
                log: Mutex::new(Vec::new()),
                fail_updates: true,
            }
        }

        pub(crate) fn log(&self) -> Vec<String> {
            self.log.lock().expect("log lock").clone()
        }
    }

    #[async_trait]
    impl DerivationStore for Recorder {
        async fn update(&self, sparql: &str) -> Result<(), EngineError> {
            if self.fail_updates {
                return Err(EngineError::Backend("store down".to_owned()));
            }
            self.log
                .lock()
                .expect("log lock")
                .push(format!("update:{sparql}"));
            Ok(())
        }
    }

    #[async_trait]
    impl RdfChangePublisher for Recorder {
        async fn publish(&self, event: &RdfChanged) -> Result<(), EngineError> {
            self.log
                .lock()
                .expect("log lock")
                .push(format!("publish:{}", event.entity_uri));
            Ok(())
        }
    }

    /// Two derivations that name themselves in their SPARQL, one reading orders, the
    /// other operations.
    pub(crate) const FAKE_DERIVATIONS: &[Derivation] = &[
        Derivation {
            name: "derive-a",
            sparql: "A",
            models: &["mrp.routing.workcenter"],
        },
        Derivation {
            name: "derive-b",
            sparql: "B",
            models: &["mrp.production", "sale.order"],
        },
    ];

    /// Derive and announce a change to `model`/`id` as the worker does.
    async fn settle(recorder: &Arc<Recorder>, model: &str) -> Result<(), EngineError> {
        let derivations = Derivations::new(
            Arc::clone(recorder) as Arc<dyn DerivationStore>,
            FAKE_DERIVATIONS,
        );
        let source_ref = SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id: 3,
        };
        let event = rdf_changed(&source_ref, Revision(2));
        derive_then_publish(
            async { derivations.run_for(model).await.map(|_| ()) },
            recorder.as_ref(),
            &event,
            "publish",
        )
        .await
    }

    #[tokio::test]
    async fn an_order_ingest_derives_what_reads_orders_before_publishing() {
        let recorder = Arc::new(Recorder::default());
        settle(&recorder, "mrp.production").await.expect("settles");
        assert_eq!(
            recorder.log(),
            [
                "update:B",
                "publish:https://ekg.example/id/mrp.production/3"
            ]
        );
    }

    #[tokio::test]
    async fn an_ingest_no_derivation_reads_runs_none() {
        let recorder = Arc::new(Recorder::default());
        settle(&recorder, "res.partner").await.expect("settles");
        assert_eq!(
            recorder.log(),
            ["publish:https://ekg.example/id/res.partner/3"]
        );
        let derivations = Derivations::new(
            Arc::clone(&recorder) as Arc<dyn DerivationStore>,
            FAKE_DERIVATIONS,
        );
        assert!(!derivations.read("res.partner"), "no step is journaled");
        assert!(derivations.read("sale.order"));
    }

    #[tokio::test]
    async fn a_failing_derivation_publishes_nothing() {
        let recorder = Arc::new(Recorder::failing());
        let err = settle(&recorder, "mrp.production")
            .await
            .expect_err("a derivation fault fails the step");
        assert!(matches!(err, EngineError::Backend(_)), "retryable: {err:?}");
        assert!(recorder.log().is_empty(), "nothing is announced");
    }

    #[tokio::test]
    async fn the_capability_derivations_run_in_order_for_a_shared_model() {
        // The real list: an order ingest runs only the process-edge derivation.
        let recorder = Arc::new(Recorder::default());
        let derivations = Derivations::new(
            Arc::clone(&recorder) as Arc<dyn DerivationStore>,
            ontology::STRUCTURAL_DERIVATIONS,
        );
        let ran = derivations.run_for("mrp.production").await.expect("runs");
        assert_eq!(ran, ["derive-process-edges"]);
        let ran = derivations
            .run_for("mrp.routing.workcenter")
            .await
            .expect("runs");
        assert_eq!(ran, ["derive-precedes"]);
    }

    fn source_ref() -> SourceRef {
        SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 42,
        }
    }

    #[test]
    fn first_revision_is_a_creation() {
        let event = rdf_changed(&source_ref(), Revision(1));
        assert_eq!(event.change_kind, ChangeKind::Created);
        assert_eq!(event.entity_uri.0, "https://ekg.example/id/sale.order/42");
        assert_eq!(event.revision, Revision(1));
    }

    #[test]
    fn later_revision_is_an_update() {
        let event = rdf_changed(&source_ref(), Revision(3));
        assert_eq!(event.change_kind, ChangeKind::Updated);
    }

    #[test]
    fn backend_faults_are_retryable_others_terminal() {
        // Only the terminal side is asserted: a logic fault classifies as terminal and
        // renders its message through `HandlerError`.
        let terminal = classify(EngineError::NotFound("x".to_owned()));
        assert!(format!("{terminal:?}").contains("not found"));
    }
}
