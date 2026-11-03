//! The GraphDB driver for the [`WorldModel`] seam.
//!
//! A thin async client over GraphDB's RDF4J HTTP protocol (SPARQL query/update +
//! the statements endpoint as a Graph Store surface) plus a [`GraphDbWorldModel`]
//! that implements the seam: named-graph materialization with mandatory
//! `ekg:wasDerivedFrom` provenance, per-entity monotonic revisions, grounding
//! with the structural read-policy, and a raw SPARQL `select` path the monitor
//! uses to run the risk rule.
//!
//! Nothing here names the supply-risk use case. Which SPARQL a caller runs (e.g.
//! [`ontology::RISK_RULE_SPARQL`](crate::ontology::RISK_RULE_SPARQL)) and which
//! [`MappingContract`] the driver is given are the capability's concern.
//!
//! Materialization projects each record into a typed entity graph through the
//! injected contract: `rdf:type` (the domain class plus the neutral
//! `ekg:GovernedEntity`), typed datatype literals, and object relations to other
//! entities. The typed graph is what the SHACL shapes validate on write and what
//! the SPARQL risk rule traverses.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use ekg_domain::{
    EngineError, EntityUri, EvidenceEdge, Grounding, ReadContext, ReadPolicy, Result, Revision,
    SourceRecord, SourceRef, WorldModel,
};
use opentelemetry::trace::{Span as _, SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, global};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use tokio::sync::Mutex as AsyncMutex;

use crate::mapping::{self, MappingContract};
use crate::ontology;

/// Metadata graph holding each entity's `SourceRef`, monotonic revision, and
/// tombstone flag. Kept separate from the entity's data graph so it survives a
/// data-graph replace (materialize) or drop (retract).
const META_GRAPH: &str = "https://ekg.example/graph/_meta";

/// The `rdf:type` predicate IRI. Structural typing is skipped when grounding.
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The mandatory provenance predicate (CURIE form) every materialized entity
/// carries. The projection emits it and `materialize` asserts it before any store
/// write, so the driver can never leave an unprovenanced fact.
const PROVENANCE_PREDICATE_CURIE: &str = "ekg:wasDerivedFrom";

/// Map any error carrying a message to a typed backend error (never panic).
fn backend<E: std::fmt::Display>(err: E) -> EngineError {
    EngineError::Backend(err.to_string())
}

// --- whole-graph consistency report ---

/// One cross-entity consistency violation from a whole-graph SHACL check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The node that failed a constraint.
    pub focus_node: String,
    /// The property path the constraint is on (may be empty).
    pub path: String,
    /// The human-readable violation message (may be empty).
    pub message: String,
}

/// The structured result of a whole-graph SHACL consistency check: a non-blocking
/// governance signal, not a write gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    /// Whether the settled graph conforms to the consistency shapes.
    pub conforms: bool,
    /// The violations found (empty when `conforms`).
    pub violations: Vec<Violation>,
}

// --- HTTP client ---

/// A thin async client over the GraphDB / RDF4J REST protocol for one repository.
///
/// Reached over `http://` (no TLS, see the crate's dependency note). Every
/// non-2xx response or transport failure surfaces as [`EngineError::Backend`].
pub struct GraphDbClient {
    http: reqwest::Client,
    /// Base URL, e.g. `http://localhost:7200` (no trailing slash).
    base_url: String,
    /// Repository id, e.g. `ekg`.
    repo_id: String,
}

impl GraphDbClient {
    /// Build a client for `repo_id` at `base_url` (e.g. `http://localhost:7200`).
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the underlying HTTP client cannot be built.
    pub fn new(base_url: impl Into<String>, repo_id: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder().build().map_err(backend)?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            repo_id: repo_id.into(),
        })
    }

    /// Send `request` and read the whole answer: its status and its body.
    ///
    /// Under an exported span the request is a span of its own, named by `operation` and
    /// `repo`, so the time of a traced operation splits into its store requests. Outside one
    /// (a monitor's poll, an ingest) nothing is recorded. The span never carries the query.
    async fn request(
        &self,
        operation: &'static str,
        repo: Option<&str>,
        request: reqwest::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, String)> {
        let parent = Context::current();
        if !parent.span().is_recording() {
            let resp = request.send().await.map_err(backend)?;
            let status = resp.status();
            return Ok((status, resp.text().await.map_err(backend)?));
        }
        let tracer = global::tracer("ekg-engine");
        let mut span = tracer
            .span_builder(format!("graphdb {operation}"))
            .with_kind(SpanKind::Client)
            .with_attributes([
                KeyValue::new("db.system.name", "graphdb"),
                KeyValue::new("db.operation.name", operation),
            ])
            .start_with_context(&tracer, &parent);
        if let Some(repo) = repo {
            span.set_attribute(KeyValue::new("db.namespace", repo.to_owned()));
        }
        let answer = async {
            let resp = request.send().await.map_err(backend)?;
            let status = resp.status();
            Ok::<_, EngineError>((status, resp.text().await.map_err(backend)?))
        }
        .await;
        match &answer {
            Ok((status, _)) => {
                span.set_attribute(KeyValue::new(
                    "http.response.status_code",
                    i64::from(status.as_u16()),
                ));
                if !status.is_success() {
                    span.set_status(Status::error(status.as_str().to_owned()));
                }
            }
            Err(_) => span.set_status(Status::error("transport")),
        }
        span.end();
        answer
    }

    /// The SPARQL query endpoint for the repository.
    fn query_endpoint(&self) -> String {
        format!("{}/repositories/{}", self.base_url, self.repo_id)
    }

    /// The SPARQL update / statements endpoint for the repository.
    fn statements_endpoint(&self) -> String {
        format!("{}/repositories/{}/statements", self.base_url, self.repo_id)
    }

    /// Run a SPARQL SELECT and return solution rows (variable to term string).
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure, a non-2xx response, or an
    /// unparseable results document.
    pub async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
        let request = self
            .http
            .post(self.query_endpoint())
            .header(ACCEPT, "application/sparql-results+json")
            .form(&[("query", sparql)]);
        let (status, body) = self.request("select", Some(&self.repo_id), request).await?;
        if !status.is_success() {
            return Err(EngineError::Backend(format!(
                "SPARQL select {status}: {body}"
            )));
        }
        parse_sparql_results(&body)
    }

    /// Run a SPARQL UPDATE against the repository.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response (which
    /// includes a SHACL validation failure when shapes are enforced).
    pub async fn update(&self, sparql: &str) -> Result<()> {
        let request = self
            .http
            .post(self.statements_endpoint())
            .form(&[("update", sparql)]);
        let answer = self.request("update", Some(&self.repo_id), request).await?;
        expect_success(&answer, "SPARQL update")
    }

    /// Replace a named graph's contents with `turtle` (Graph Store PUT semantics,
    /// idempotent for the same input).
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response (a
    /// SHACL violation is reported here when validate-on-write is enabled).
    pub async fn put_turtle(&self, graph_iri: &str, turtle: &str) -> Result<()> {
        let request = self
            .http
            .put(self.statements_endpoint())
            .query(&[("context", format!("<{graph_iri}>"))])
            .header(CONTENT_TYPE, "text/turtle")
            .body(turtle.to_owned());
        let answer = self
            .request("put_graph", Some(&self.repo_id), request)
            .await?;
        expect_success(&answer, "graph PUT")
    }

    /// Drop a named graph entirely.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn delete_graph(&self, graph_iri: &str) -> Result<()> {
        let request = self
            .http
            .delete(self.statements_endpoint())
            .query(&[("context", format!("<{graph_iri}>"))]);
        let answer = self
            .request("delete_graph", Some(&self.repo_id), request)
            .await?;
        expect_success(&answer, "graph DELETE")
    }

    /// The base URL this client targets (e.g. `http://localhost:7200`).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The repository id this client targets.
    #[must_use]
    pub fn repo_id(&self) -> &str {
        &self.repo_id
    }

    /// Create a fresh repository `new_repo_id` without SHACL from GraphDB's default
    /// configuration template (context index on, write-shapes not loaded). Used by the
    /// [`Simulator`](ekg_domain::Simulator) driver to build an isolated projection
    /// repo, so the transient duplicate of a `maxCount 1` predicate during a diff is
    /// never validated against the production write-shapes.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure, an unparseable template, or a
    /// non-2xx create response.
    pub async fn create_scratch_repo(&self, new_repo_id: &str) -> Result<()> {
        // The diff's transient two-value state would be rejected by `maxCount 1` with
        // SHACL on, so force it off rather than trusting the template default.
        self.create_repo(new_repo_id, "EKG simulator projection (ephemeral)", false)
            .await
    }

    /// Create a fresh repository `new_repo_id` that enforces the write-shapes on every write,
    /// as the production repository does: SHACL on, the shapes loaded into GraphDB's reserved
    /// shapes graph. For a throwaway world a fixture is materialized into.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure, an unparseable template, a non-2xx
    /// create response, or shapes that do not load.
    pub async fn create_shacl_repo(&self, new_repo_id: &str) -> Result<()> {
        self.create_repo(new_repo_id, "EKG fixture world (throwaway)", true)
            .await?;
        Self::new(self.base_url.clone(), new_repo_id)?
            .put_turtle(ontology::SHACL_SHAPES_GRAPH, ontology::WRITE_SHAPES_TTL)
            .await
    }

    /// Create `new_repo_id` from GraphDB's default configuration template, with the context
    /// index on and SHACL as `shacl` says.
    async fn create_repo(&self, new_repo_id: &str, title: &str, shacl: bool) -> Result<()> {
        let request = self.http.get(format!(
            "{}/rest/repositories/default-config/graphdb",
            self.base_url
        ));
        let (_, template) = self
            .request("create_repository", Some(new_repo_id), request)
            .await?;
        let mut cfg: serde_json::Value = serde_json::from_str(&template).map_err(backend)?;
        cfg["id"] = serde_json::Value::from(new_repo_id);
        cfg["title"] = serde_json::Value::from(title);
        let shacl = if shacl { "true" } else { "false" };
        for (key, val) in [("enableContextIndex", "true"), ("isShacl", shacl)] {
            if let Some(v) = cfg.pointer_mut(&format!("/params/{key}/value")) {
                *v = serde_json::Value::from(val);
            }
        }
        let body = serde_json::to_string(&cfg).map_err(backend)?;
        let request = self
            .http
            .post(format!("{}/rest/repositories", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        let answer = self
            .request("create_repository", Some(new_repo_id), request)
            .await?;
        expect_success(&answer, "repo create")
    }

    /// Drop the repository `repo_id` entirely (REST repository management). GraphDB answers
    /// 2xx for a repository it does not have, so dropping one twice is not an error.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn drop_repo(&self, repo_id: &str) -> Result<()> {
        let request = self
            .http
            .delete(format!("{}/rest/repositories/{repo_id}", self.base_url));
        let answer = self
            .request("drop_repository", Some(repo_id), request)
            .await?;
        expect_success(&answer, "repo drop")
    }

    /// The ids of every repository on this client's server (REST repository management).
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure, a non-2xx response or an unparseable
    /// listing.
    pub async fn repository_ids(&self) -> Result<Vec<String>> {
        let request = self
            .http
            .get(format!("{}/rest/repositories", self.base_url))
            .header(ACCEPT, "application/json");
        let (status, body) = self.request("list_repositories", None, request).await?;
        if !status.is_success() {
            return Err(backend(format!("repo list returned {status}: {body}")));
        }
        let list: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(backend)?;
        Ok(list
            .iter()
            .filter_map(|repo| repo.get("id").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect())
    }

    /// Export all statements of this client's repository as N-Quads (every named
    /// graph, contexts preserved).
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn export_nquads(&self) -> Result<String> {
        let request = self
            .http
            .get(self.statements_endpoint())
            .header(ACCEPT, "application/n-quads");
        let (status, body) = self.request("export", Some(&self.repo_id), request).await?;
        if !status.is_success() {
            return Err(EngineError::Backend(format!(
                "statements export {status}: {body}"
            )));
        }
        Ok(body)
    }

    /// Import N-Quads into this client's repository (contexts preserved), adding to
    /// whatever is already present.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn import_nquads(&self, nquads: &str) -> Result<()> {
        let request = self
            .http
            .post(self.statements_endpoint())
            .header(CONTENT_TYPE, "application/n-quads")
            .body(nquads.to_owned());
        let answer = self.request("import", Some(&self.repo_id), request).await?;
        expect_success(&answer, "statements import")
    }

    /// The whole-graph SHACL validation endpoint (Workbench REST).
    fn validate_endpoint(&self) -> String {
        format!(
            "{}/rest/repositories/{}/validate/text",
            self.base_url, self.repo_id
        )
    }

    /// Validate the settled whole graph against `shapes_ttl` via GraphDB's bulk
    /// validation endpoint, returning a structured [`ConformanceReport`]. This is a
    /// non-blocking whole-graph check: it never mutates or gates the write path.
    /// Cross-entity consistency is assessed over the complete graph, where it is
    /// meaningful, rather than at per-entity write time.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn validate(&self, shapes_ttl: &str) -> Result<ConformanceReport> {
        let request = self
            .http
            .post(self.validate_endpoint())
            .header(CONTENT_TYPE, "text/turtle")
            .body(shapes_ttl.to_owned());
        let (status, body) = self
            .request("validate", Some(&self.repo_id), request)
            .await?;
        if !status.is_success() {
            return Err(EngineError::Backend(format!(
                "SHACL validate {status}: {body}"
            )));
        }
        Ok(parse_validation_report(&body))
    }
}

/// Map an answer's non-2xx status to a typed error. A store SHACL rejection (the body
/// carries a SHACL `ValidationReport`) is a rejected write, [`EngineError::Invariant`],
/// distinct from an opaque backend failure, so callers can tell "your entity is malformed"
/// from "the store is unreachable". The transaction is rejected whole, so prior state is
/// unchanged.
fn expect_success((status, body): &(reqwest::StatusCode, String), what: &str) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(classify_write_error(what, &status.to_string(), body))
}

// --- WorldModel driver ---

/// The GraphDB-backed [`WorldModel`]. Generic and capability-neutral: the domain
/// it projects is carried entirely by the injected [`MappingContract`], not by
/// this driver.
pub struct GraphDbWorldModel {
    client: GraphDbClient,
    /// The source-to-ontology mapping. Injected, not hardcoded, so the
    /// driver names no capability; `materialize` projects records through it.
    contract: MappingContract,
    /// The role+purpose read-policy. Injected config, not code: the demo policy is
    /// built from config in `main.rs`; a bare `new` defaults to permissive.
    read_policy: ReadPolicy,
    /// Per-entity async locks. `materialize`/`retract` take the lock for their
    /// entity so the read-revision then write sequence is serialized within the
    /// process, keeping revisions monotonic under concurrency. (Cross-process
    /// single-writer-per-entity is provided by the durable ingestion runtime.)
    entity_locks: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl GraphDbWorldModel {
    /// Wrap a [`GraphDbClient`] as a world model that projects records through
    /// `contract`. For the supply-risk capability, pass
    /// [`ontology::supply_risk_contract`](crate::ontology::supply_risk_contract).
    ///
    /// Grounding defaults to a permissive read-policy (hides nothing); the running
    /// app injects the real policy from config via [`Self::new_with_read_policy`].
    #[must_use]
    pub fn new(client: GraphDbClient, contract: MappingContract) -> Self {
        Self::new_with_read_policy(client, contract, ReadPolicy::permissive())
    }

    /// Wrap a [`GraphDbClient`] as a world model with an explicit role+purpose
    /// [`ReadPolicy`], which is what `main.rs` builds from config.
    #[must_use]
    pub fn new_with_read_policy(
        client: GraphDbClient,
        contract: MappingContract,
        read_policy: ReadPolicy,
    ) -> Self {
        Self {
            client,
            contract,
            read_policy,
            entity_locks: StdMutex::new(HashMap::new()),
        }
    }

    /// The async lock guarding writes to `entity`, created on first use. The inner
    /// (`std`) lock is held only to look the entry up, never across an `await`.
    fn lock_for(&self, entity: &str) -> Arc<AsyncMutex<()>> {
        let mut map = match self.entity_locks.lock() {
            Ok(guard) => guard,
            // A poisoned lock only means some other caller panicked mid-lookup; the
            // map itself is still sound, so recover it rather than propagate a panic.
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(
            map.entry(entity.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    /// The last revision recorded for `entity`, or `0` if it has never been
    /// materialized.
    async fn current_revision(&self, entity: &str) -> Result<u64> {
        let sparql = format!(
            "PREFIX ekg: <{vocab}>\n\
             SELECT ?rev WHERE {{ GRAPH <{meta}> {{ <{entity}> ekg:revision ?rev }} }} LIMIT 1",
            vocab = ontology::VOCAB_NAMESPACE,
            meta = META_GRAPH,
        );
        let rows = self.client.select(&sparql).await?;
        Ok(rows
            .first()
            .and_then(|r| r.get("rev"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0))
    }

    /// Read the metadata (source ref, revision, tombstone) recorded for `entity`,
    /// or `None` if it has never been materialized.
    async fn read_meta(&self, entity: &str) -> Result<Option<EntityMeta>> {
        let sparql = format!(
            "PREFIX ekg: <{vocab}>\n\
             SELECT ?system ?model ?id ?rev ?retracted WHERE {{ GRAPH <{meta}> {{\n\
             <{entity}> ekg:source_system ?system ; ekg:source_model ?model ;\n\
             ekg:source_id ?id ; ekg:revision ?rev .\n\
             OPTIONAL {{ <{entity}> ekg:retracted ?retracted }} }} }} LIMIT 1",
            vocab = ontology::VOCAB_NAMESPACE,
            meta = META_GRAPH,
        );
        let rows = self.client.select(&sparql).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let system = row.get("system").cloned().unwrap_or_default();
        let model = row.get("model").cloned().unwrap_or_default();
        let id = row
            .get("id")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let revision = row
            .get("rev")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let retracted = row.get("retracted").is_some_and(|s| s == "true");
        Ok(Some(EntityMeta {
            source_ref: SourceRef { system, model, id },
            revision,
            retracted,
        }))
    }
}

/// Metadata recorded for a materialized entity.
struct EntityMeta {
    source_ref: SourceRef,
    revision: u64,
    retracted: bool,
}

#[async_trait]
impl WorldModel for GraphDbWorldModel {
    async fn materialize(&self, record: &SourceRecord) -> Result<Revision> {
        // Identifier safety: reject source model / field names that could break
        // out of an IRI or predicate before any query/serialization is built.
        validate_source_record(record)?;
        let sr = &record.source_ref;
        let entity = ontology::entity_iri(&sr.model, sr.id);
        let graph = ontology::named_graph(&sr.model, sr.id);
        // Typed projection through the mapping contract: domain type, typed
        // literals, object relations and provenance.
        let body = self.contract.project(record, &entity)?;

        // Refuse an unprovenanced projection before any store write, so a violation
        // leaves the world model unchanged. The store's SHACL provenance shape checks
        // the same thing.
        ensure_provenance(&entity, &data_graph_turtle(&body))?;

        // Serialize the read-revision then write sequence for this entity so concurrent
        // materializations cannot observe the same revision.
        let lock = self.lock_for(&entity);
        let _guard = lock.lock().await;

        let next = self.current_revision(&entity).await? + 1;
        // One SPARQL update replaces the data graph and the entity's metadata in a
        // single transaction, so a partial failure can never leave them inconsistent.
        // A store-side SHACL violation surfaces here as a backend error, leaving the
        // prior state unchanged (the transaction is rejected whole).
        self.client
            .update(&materialize_update(&entity, &graph, &body, record, next))
            .await?;
        Ok(Revision(next))
    }

    async fn retract(&self, entity: &EntityUri) -> Result<Revision> {
        validate_entity_uri(&entity.0)?;
        let lock = self.lock_for(&entity.0);
        let _guard = lock.lock().await;

        let meta = self
            .read_meta(&entity.0)
            .await?
            .filter(|m| !m.retracted)
            .ok_or_else(|| EngineError::NotFound(entity.0.clone()))?;
        let next = meta.revision + 1;
        let graph = ontology::named_graph(&meta.source_ref.model, meta.source_ref.id);
        // Atomic: drop the data graph and tombstone the metadata in one transaction.
        self.client
            .update(&retract_update(&entity.0, &graph, &meta.source_ref, next))
            .await?;
        Ok(Revision(next))
    }

    async fn ground(&self, entity: &EntityUri, ctx: &ReadContext) -> Result<Grounding> {
        let meta = self
            .read_meta(&entity.0)
            .await?
            .filter(|m| !m.retracted)
            .ok_or_else(|| EngineError::NotFound(entity.0.clone()))?;
        let graph = ontology::named_graph(&meta.source_ref.model, meta.source_ref.id);

        let sparql = format!(
            "SELECT ?p ?o WHERE {{ GRAPH <{graph}> {{ <{entity}> ?p ?o }} }}",
            entity = entity.0,
        );
        let rows = self.client.select(&sparql).await?;

        // Apply the injected role+purpose read-policy: hidden nodes/edges become
        // recorded omissions. Delegates to the shared `ReadPolicy` so the
        // live store and the in-memory fake filter identically.
        let revision = Revision(meta.revision);
        let was_derived_from = format!("{}wasDerivedFrom", ontology::VOCAB_NAMESPACE);

        let mut edges = Vec::new();
        let mut omissions = self.read_policy.omissions();
        for row in &rows {
            let (Some(predicate), Some(object)) = (row.get("p"), row.get("o")) else {
                continue;
            };
            // Structural plumbing, not evidence: the neutral rdf:type and the
            // provenance edge (the latter is carried on every EvidenceEdge).
            if predicate == RDF_TYPE || predicate == &was_derived_from {
                continue;
            }
            let local = predicate
                .strip_prefix(ontology::VOCAB_NAMESPACE)
                .unwrap_or(predicate.as_str());
            if omissions.visible(ctx, local) {
                edges.push(EvidenceEdge {
                    subject: entity.clone(),
                    predicate: format!("ekg:{local}"),
                    object: object.clone(),
                    source: meta.source_ref.clone(),
                    revision,
                });
            }
        }

        Ok(Grounding {
            entity: entity.clone(),
            edges,
            omissions: omissions.into_omissions(),
        })
    }

    async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
        self.client.select(sparql).await
    }
}

// --- pure helpers ---

/// Refuse a projection that lacks a well-formed provenance edge. Called before any
/// store write so a violation leaves the world model unchanged.
///
/// Checks the two triples the projection must emit (the `ekg:wasDerivedFrom` edge
/// to this entity's source node and that node's `ekg:SourceRecord` type) rather
/// than a bare substring, so a field value or comment that merely contains the
/// predicate string cannot spoof the guard.
///
/// # Errors
/// [`EngineError::Invariant`] if `turtle` does not carry the provenance edge to an
/// `ekg:SourceRecord` for `entity`.
fn ensure_provenance(entity: &str, turtle: &str) -> Result<()> {
    let source_node = format!("{entity}#source");
    let edge = format!("{PROVENANCE_PREDICATE_CURIE} <{source_node}>");
    let source_decl = format!("<{source_node}> a ekg:SourceRecord");
    if turtle.contains(&edge) && turtle.contains(&source_decl) {
        Ok(())
    } else {
        Err(EngineError::Invariant(format!(
            "refusing to materialize {entity}: projection lacks a well-formed \
             {PROVENANCE_PREDICATE_CURIE} edge to an ekg:SourceRecord"
        )))
    }
}

/// Classify a non-2xx store write response. A body carrying a SHACL
/// `ValidationReport` (GraphDB returns one, with `sh:conforms false`, when a write
/// violates an enforced shape) is a rejected write ([`EngineError::Invariant`]),
/// not an opaque backend failure. Everything else stays [`EngineError::Backend`].
fn classify_write_error(what: &str, status: &str, body: &str) -> EngineError {
    if is_shacl_rejection(body) {
        EngineError::Invariant(format!(
            "{what} rejected by store SHACL validation (write not persisted): {body}"
        ))
    } else {
        EngineError::Backend(format!("{what} {status}: {body}"))
    }
}

/// Whether a store response body is a SHACL validation report (a rejected write).
fn is_shacl_rejection(body: &str) -> bool {
    body.contains("ValidationReport") || body.contains("shacl#conforms")
}

/// Parse a GraphDB SHACL `sh:ValidationReport` (Turtle) into a [`ConformanceReport`].
/// Extraction is lightweight (no RDF-parser dependency): the report is
/// machine-generated with stable predicate names, and each `sh:ValidationResult`
/// block lists its `sh:focusNode`, `sh:resultPath`, and `sh:resultMessage` before
/// the next result begins.
fn parse_validation_report(ttl: &str) -> ConformanceReport {
    let conforms = token_after(ttl, "sh:conforms").is_some_and(|t| t.starts_with("true"));
    let violations = ttl
        .split("sh:ValidationResult")
        .skip(1)
        .map(|block| Violation {
            focus_node: iri_after(block, "sh:focusNode").unwrap_or_default(),
            path: iri_after(block, "sh:resultPath").unwrap_or_default(),
            message: string_after(block, "sh:resultMessage").unwrap_or_default(),
        })
        .collect();
    ConformanceReport {
        conforms,
        violations,
    }
}

/// The whitespace-delimited token immediately following `pred` in `s`.
fn token_after<'a>(s: &'a str, pred: &str) -> Option<&'a str> {
    s.split_once(pred)
        .map(|(_, rest)| rest.trim_start())
        .map(|rest| {
            rest.split([' ', ';', ',', '\n', '\r', '\t', '.'])
                .next()
                .unwrap_or("")
        })
}

/// The angle-bracketed IRI (`<…>`) content immediately following `pred`.
fn iri_after(s: &str, pred: &str) -> Option<String> {
    let (_, rest) = s.split_once(pred)?;
    let start = rest.find('<')?;
    let end = rest[start + 1..].find('>')?;
    Some(rest[start + 1..start + 1 + end].to_owned())
}

/// The double-quoted string (`"…"`) content immediately following `pred`.
fn string_after(s: &str, pred: &str) -> Option<String> {
    let (_, rest) = s.split_once(pred)?;
    let start = rest.find('"')?;
    let end = rest[start + 1..].find('"')?;
    Some(rest[start + 1..start + 1 + end].to_owned())
}

/// The full Turtle document (with `@prefix ekg:`) for a projected data-graph
/// `body`, used for the provenance guard and unit tests. Typed literals in the
/// body carry full XSD datatype IRIs, so only the `ekg:` prefix is needed.
fn data_graph_turtle(body: &str) -> String {
    format!(
        "@prefix ekg: <{vocab}> .\n{body}",
        vocab = ontology::VOCAB_NAMESPACE,
    )
}

/// The atomic materialize update: in one transaction, replace the entity's data
/// graph with the projected `body` and rewrite its metadata (source ref + the new
/// `revision`), so a partial failure can never leave the data graph and metadata
/// inconsistent. The `DELETE … INSERT … WHERE` form keeps the write transactional
/// so a store-side SHACL rejection aborts it whole.
fn materialize_update(
    entity: &str,
    graph: &str,
    body: &str,
    record: &SourceRecord,
    revision: u64,
) -> String {
    let sr = &record.source_ref;
    format!(
        "PREFIX ekg: <{vocab}>\n\
         DELETE {{\n  GRAPH <{graph}> {{ ?ds ?dp ?do }}\n  GRAPH <{meta}> {{ <{entity}> ?mp ?mo }}\n}}\n\
         INSERT {{\n  GRAPH <{graph}> {{\n{body}  }}\n  GRAPH <{meta}> {{ <{entity}> ekg:source_system \"{system}\" ; ekg:source_model \"{model}\" ; ekg:source_id {id} ; ekg:revision {revision} . }}\n}}\n\
         WHERE {{\n  OPTIONAL {{ GRAPH <{graph}> {{ ?ds ?dp ?do }} }}\n  OPTIONAL {{ GRAPH <{meta}> {{ <{entity}> ?mp ?mo }} }}\n}}",
        vocab = ontology::VOCAB_NAMESPACE,
        meta = META_GRAPH,
        system = mapping::escape_literal(&sr.system),
        model = mapping::escape_literal(&sr.model),
        id = sr.id,
    )
}

/// The atomic retract update: in one transaction, drop the entity's data graph and
/// tombstone its metadata at the new `revision`.
fn retract_update(entity: &str, graph: &str, sr: &SourceRef, revision: u64) -> String {
    format!(
        "PREFIX ekg: <{vocab}>\n\
         DELETE {{\n  GRAPH <{graph}> {{ ?ds ?dp ?do }}\n  GRAPH <{meta}> {{ <{entity}> ?mp ?mo }}\n}}\n\
         INSERT {{\n  GRAPH <{meta}> {{ <{entity}> ekg:source_system \"{system}\" ; ekg:source_model \"{model}\" ; ekg:source_id {id} ; ekg:revision {revision} ; ekg:retracted true . }}\n}}\n\
         WHERE {{\n  OPTIONAL {{ GRAPH <{graph}> {{ ?ds ?dp ?do }} }}\n  OPTIONAL {{ GRAPH <{meta}> {{ <{entity}> ?mp ?mo }} }}\n}}",
        vocab = ontology::VOCAB_NAMESPACE,
        meta = META_GRAPH,
        system = mapping::escape_literal(&sr.system),
        model = mapping::escape_literal(&sr.model),
        id = sr.id,
    )
}

/// A source model / IRI path segment is safe when it is non-empty and limited to
/// ASCII letters, digits, `_`, `.`, `-`: no `/`, whitespace, or RDF/SPARQL
/// delimiters that could break out of an IRI.
fn is_safe_model(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// A field predicate local name is safe when it is a conservative CURIE local
/// name: an ASCII letter/underscore start, then ASCII letters/digits/underscore.
fn is_safe_field(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reject a source record whose model name or field predicate names could inject
/// into the SPARQL/Turtle the driver builds. Called before any serialization.
///
/// # Errors
/// [`EngineError::Invariant`] for an unsafe model name or field predicate name.
fn validate_source_record(record: &SourceRecord) -> Result<()> {
    if !is_safe_model(&record.source_ref.model) {
        return Err(EngineError::Invariant(format!(
            "unsafe source model name: {:?}",
            record.source_ref.model
        )));
    }
    for key in record.fields.keys() {
        if !is_safe_field(key) {
            return Err(EngineError::Invariant(format!(
                "unsafe field predicate name: {key:?}"
            )));
        }
    }
    Ok(())
}

/// Reject an entity IRI that is outside the neutral id namespace or carries
/// characters that could break out of an angle-bracketed IRI in SPARQL/Turtle.
///
/// # Errors
/// [`EngineError::Invariant`] if `uri` is not a safe entity IRI.
fn validate_entity_uri(uri: &str) -> Result<()> {
    let safe = uri.starts_with(ontology::ID_NAMESPACE)
        && !uri
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '{' | '}' | '\\' | '`'));
    if safe {
        Ok(())
    } else {
        Err(EngineError::Invariant(format!(
            "unsafe entity IRI: {uri:?}"
        )))
    }
}

/// Parse a SPARQL 1.1 JSON results document into rows of variable to term string.
fn parse_sparql_results(json: &str) -> Result<Vec<BTreeMap<String, String>>> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(backend)?;
    let bindings = value
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| EngineError::Backend("malformed SPARQL results: no bindings".to_owned()))?;
    let mut rows = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let Some(object) = binding.as_object() else {
            continue;
        };
        let mut row = BTreeMap::new();
        for (var, term) in object {
            if let Some(term_value) = term.get("value").and_then(serde_json::Value::as_str) {
                row.insert(var.clone(), term_value.to_owned());
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
// In unit tests `expect` on a known-good projection is the assertion mechanism.
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn contract() -> MappingContract {
        ontology::supply_risk_contract().expect("the embedded supply-risk contract loads")
    }

    fn record() -> SourceRecord {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_owned(), "VeloMax E-Bike".to_owned());
        fields.insert("default_code".to_owned(), "EBIKE-01".to_owned());
        SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: "product.product".to_owned(),
                id: 7,
            },
            fields,
        }
    }

    #[test]
    fn projection_carries_type_and_mandatory_provenance() {
        let entity = ontology::entity_iri("product.product", 7);
        let body = contract().project(&record(), &entity).expect("project");
        let turtle = data_graph_turtle(&body);
        // The neutral governance type (global provenance shape) plus the domain type.
        assert!(turtle.contains(
            "<https://ekg.example/id/product.product/7> a ekg:GovernedEntity, ekg:Product ;"
        ));
        // Mapped datatype literals.
        assert!(turtle.contains("ekg:name \"VeloMax E-Bike\""));
        assert!(turtle.contains("ekg:default_code \"EBIKE-01\""));
        assert!(
            turtle.contains("ekg:wasDerivedFrom <https://ekg.example/id/product.product/7#source>")
        );
        assert!(turtle.contains("a ekg:SourceRecord"));
        assert!(turtle.contains("ekg:source_system \"odoo\""));
        assert!(turtle.contains("ekg:source_model \"product.product\""));
        assert!(turtle.contains("ekg:source_id 7 ."));
    }

    #[test]
    fn ensure_provenance_accepts_a_provenanced_projection() {
        let entity = ontology::entity_iri("product.product", 7);
        let body = contract().project(&record(), &entity).expect("project");
        let turtle = data_graph_turtle(&body);
        assert!(ensure_provenance(&entity, &turtle).is_ok());
    }

    #[test]
    fn ensure_provenance_refuses_an_unprovenanced_projection() {
        // A hand-built projection missing ekg:wasDerivedFrom (what a future buggy
        // projection change might emit) must be refused before any store write.
        let entity = ontology::entity_iri("product.product", 8);
        let orphan = format!(
            "@prefix ekg: <{vocab}> .\n<{entity}> a ekg:GovernedEntity ; ekg:name \"orphan\" .\n",
            vocab = ontology::VOCAB_NAMESPACE,
        );
        assert!(matches!(
            ensure_provenance(&entity, &orphan),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn validate_source_record_accepts_canonical_names() {
        assert!(validate_source_record(&record()).is_ok());
    }

    #[test]
    fn validate_source_record_rejects_unsafe_field_name() {
        let mut rec = record();
        rec.fields.insert("evil name".to_owned(), "x".to_owned());
        assert!(matches!(
            validate_source_record(&rec),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn validate_source_record_rejects_unsafe_model() {
        let mut rec = record();
        rec.source_ref.model = "sales order/../hack".to_owned();
        assert!(matches!(
            validate_source_record(&rec),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn validate_entity_uri_accepts_canonical_and_rejects_injection() {
        let ok = ontology::entity_iri("product.product", 7);
        assert!(validate_entity_uri(&ok).is_ok());
        // An IRI carrying a `>` could close the angle brackets and inject.
        assert!(matches!(
            validate_entity_uri("https://ekg.example/id/x/1> } INSERT { ?s ?p ?o"),
            Err(EngineError::Invariant(_))
        ));
        // Outside the id namespace.
        assert!(matches!(
            validate_entity_uri("https://evil.example/id/x/1"),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn materialize_update_replaces_data_and_meta_atomically() {
        let entity = ontology::entity_iri("product.product", 7);
        let graph = ontology::named_graph("product.product", 7);
        let body = contract().project(&record(), &entity).expect("project");
        let sql = materialize_update(&entity, &graph, &body, &record(), 3);
        // One update, both graphs, the projected typed body, and the new revision.
        assert!(sql.contains("DELETE {") && sql.contains("INSERT {") && sql.contains("WHERE {"));
        assert!(sql.contains(&format!("GRAPH <{graph}>")));
        assert!(sql.contains(&format!("GRAPH <{META_GRAPH}>")));
        assert!(sql.contains("a ekg:GovernedEntity, ekg:Product"));
        assert!(sql.contains("ekg:revision 3"));
    }

    #[test]
    fn parses_a_nonconforming_validation_report() {
        // The shape GraphDB's bulk validate/text returns.
        let report = "\
@prefix sh: <http://www.w3.org/ns/shacl#> .
[] a sh:ValidationReport;
  sh:conforms false;
  sh:result [ a sh:ValidationResult;
      sh:focusNode <https://ekg.example/id/sale.order/1>;
      sh:resultPath <https://ekg.example/ontology#has_line_of>;
      sh:sourceConstraintComponent sh:MinCountConstraintComponent;
      sh:resultMessage \"A sales order must have at least one order line.\";
      sh:sourceShape [ a sh:PropertyShape ] ] .";
        let parsed = parse_validation_report(report);
        assert!(!parsed.conforms);
        assert_eq!(parsed.violations.len(), 1);
        let v = &parsed.violations[0];
        assert_eq!(v.focus_node, "https://ekg.example/id/sale.order/1");
        assert_eq!(v.path, "https://ekg.example/ontology#has_line_of");
        assert!(v.message.contains("at least one order line"));
    }

    #[test]
    fn parses_a_conforming_validation_report() {
        let report = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n[] a sh:ValidationReport; sh:conforms true .";
        assert!(parse_validation_report(report).conforms);
        assert!(parse_validation_report(report).violations.is_empty());
    }

    #[test]
    fn classifies_a_shacl_rejection_as_a_rejected_write() {
        // A GraphDB SHACL failure body carries a ValidationReport: Invariant.
        let report = "_:b0 <http://www.w3.org/ns/shacl#conforms> \"false\" .\n\
                      _:b0 a <http://www.w3.org/ns/shacl#ValidationReport> .";
        assert!(matches!(
            classify_write_error("SPARQL update", "500 Internal Server Error", report),
            EngineError::Invariant(_)
        ));
        // A plain backend failure (no report) stays Backend.
        assert!(matches!(
            classify_write_error("SPARQL update", "503 Service Unavailable", "upstream down"),
            EngineError::Backend(_)
        ));
    }

    #[test]
    fn parses_sparql_select_rows() {
        let json = r#"{
            "head": { "vars": ["p", "o"] },
            "results": { "bindings": [
                { "p": {"type":"uri","value":"https://ekg.example/ontology#name"},
                  "o": {"type":"literal","value":"VeloMax E-Bike"} },
                { "p": {"type":"uri","value":"https://ekg.example/ontology#at_risk"},
                  "o": {"type":"literal","value":"true"} }
            ] }
        }"#;
        let Ok(rows) = parse_sparql_results(json) else {
            panic!("valid results should parse");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get("p").map(String::as_str),
            Some("https://ekg.example/ontology#name")
        );
        assert_eq!(rows[0].get("o").map(String::as_str), Some("VeloMax E-Bike"));
        assert_eq!(rows[1].get("o").map(String::as_str), Some("true"));
    }

    #[test]
    fn empty_bindings_parse_to_no_rows() {
        let json = r#"{"head":{"vars":[]},"results":{"bindings":[]}}"#;
        assert!(matches!(parse_sparql_results(json), Ok(rows) if rows.is_empty()));
    }

    #[test]
    fn malformed_results_are_a_backend_error() {
        assert!(matches!(
            parse_sparql_results("not json"),
            Err(EngineError::Backend(_))
        ));
    }

    /// Exported spans: one in-memory exporter behind the process's tracer provider.
    fn exported() -> &'static opentelemetry_sdk::trace::InMemorySpanExporter {
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        static EXPORTER: std::sync::OnceLock<InMemorySpanExporter> = std::sync::OnceLock::new();
        EXPORTER.get_or_init(|| {
            let exporter = InMemorySpanExporter::default();
            global::set_tracer_provider(
                SdkTracerProvider::builder()
                    .with_simple_exporter(exporter.clone())
                    .build(),
            );
            exporter
        })
    }

    /// The exported store spans of the repository `repo`, as `(name, attributes as text)`.
    fn store_spans(repo: &str) -> Vec<opentelemetry_sdk::trace::SpanData> {
        exported()
            .get_finished_spans()
            .expect("spans")
            .into_iter()
            .filter(|span| {
                span.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == "db.namespace" && kv.value.as_str() == repo)
            })
            .collect()
    }

    /// A stand-in store: an empty result for a query, a 500 for anything else.
    async fn store() -> String {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        let router = Router::new()
            .route(
                "/repositories/{repo}",
                post(|| async { r#"{"head":{"vars":[]},"results":{"bindings":[]}}"# }),
            )
            .route(
                "/repositories/{repo}/statements",
                post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "the store is down") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_request_under_an_exported_span_is_a_span_without_its_query() {
        use opentelemetry::trace::FutureExt as _;

        let _ = exported();
        let client = GraphDbClient::new(store().await, "repo-traced").expect("client");
        let tracer = global::tracer("suite");
        let operation = Context::new().with_span(tracer.start("operation"));
        let parent = operation.span().span_context().clone();
        client
            .select("SELECT ?marker_in_the_query WHERE { ?s ?p ?o }")
            .with_context(operation.clone())
            .await
            .expect("select");
        client
            .update("INSERT DATA { <x:marker_in_the_update> <x:p> 1 }")
            .with_context(operation.clone())
            .await
            .expect_err("the store refuses");
        operation.span().end();

        let spans = store_spans("repo-traced");
        assert_eq!(spans.len(), 2, "one span per request");
        let attr = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
        };
        let select = spans
            .iter()
            .find(|span| span.name == "graphdb select")
            .expect("the select");
        assert_eq!(select.span_context.trace_id(), parent.trace_id());
        assert_eq!(select.parent_span_id, parent.span_id());
        assert_eq!(attr(select, "db.system.name").as_deref(), Some("graphdb"));
        assert_eq!(attr(select, "db.operation.name").as_deref(), Some("select"));
        assert_eq!(
            attr(select, "http.response.status_code").as_deref(),
            Some("200")
        );
        assert!(!matches!(select.status, Status::Error { .. }));
        let update = spans
            .iter()
            .find(|span| span.name == "graphdb update")
            .expect("the update");
        assert_eq!(
            attr(update, "http.response.status_code").as_deref(),
            Some("500")
        );
        assert!(matches!(update.status, Status::Error { .. }));
        for span in &spans {
            let text = format!("{} {:?}", span.name, span.attributes);
            assert!(!text.contains("marker_in_the"), "{text}");
            assert!(!text.contains("the store is down"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_request_outside_an_exported_span_is_not_a_span() {
        use opentelemetry::trace::FutureExt as _;

        let _ = exported();
        let client = GraphDbClient::new(store().await, "repo-untraced").expect("client");
        // No span at all, as a monitor's poll runs.
        client
            .select("SELECT * WHERE { ?s ?p ?o }")
            .with_context(Context::new())
            .await
            .expect("select");
        // A caller that is traced but not exported.
        let unsampled =
            Context::new().with_remote_span_context(opentelemetry::trace::SpanContext::new(
                opentelemetry::trace::TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736")
                    .expect("trace"),
                opentelemetry::trace::SpanId::from_hex("00f067aa0ba902b7").expect("span"),
                opentelemetry::trace::TraceFlags::NOT_SAMPLED,
                true,
                opentelemetry::trace::TraceState::default(),
            ));
        client
            .select("SELECT * WHERE { ?s ?p ?o }")
            .with_context(unsampled)
            .await
            .expect("select");
        assert!(store_spans("repo-untraced").is_empty());
    }
}
