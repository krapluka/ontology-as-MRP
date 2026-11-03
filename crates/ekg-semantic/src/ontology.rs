//! The supply-risk ontology artifacts and the world-model IRI scheme.
//!
//! The Turtle and SPARQL files under `crates/ekg-semantic/ontology/` are the source of
//! truth. They are embedded here so the rest of the engine references one copy, and
//! `scripts/graphdb-provision.sh` (`just graphdb-provision`) loads the same files into
//! GraphDB from disk, so the embedded constants and the loaded graphs cannot drift.

/// The capability-neutral core ontology: the governance base, the provenance
/// vocabulary and the PROV-aligned process metamodel
/// (`ekg:Activity`/`Agent`/`Artifact`/`ProcessState` + verbs). Carries no capability
/// term (a unit test enforces it); every capability specialises it. Loaded into its
/// own neutral ontology graph by `graphdb-provision.sh`.
pub const CORE_TTL: &str = include_str!("../ontology/ekg-core.ttl");

/// The OWL projection of Odoo's MRP model (classes, properties): the supply-risk and
/// manufacturing-process capability specialisation over [`CORE_TTL`].
pub const ONTOLOGY_TTL: &str = include_str!("../ontology/supply-risk.ttl");

/// SHACL write-shapes: own datatype/cardinality per class plus provenance inlined
/// on the `ekg:GovernedEntity` target. Enforced transactionally on every
/// materialization: loaded into the reserved SHACL shapes graph so a
/// structurally-malformed own entity is rejected on write. Carries no cross-entity
/// `sh:class` constraint, so a valid incremental write is never falsely rejected.
pub const WRITE_SHAPES_TTL: &str = include_str!("../ontology/write-shapes.ttl");

/// SHACL consistency-shapes: the cross-entity constraints (object-property
/// targets, relation cardinalities that span entities). Validated over the settled
/// whole graph as a non-blocking governance signal, never on the write path.
pub const CONSISTENCY_SHAPES_TTL: &str = include_str!("../ontology/consistency-shapes.ttl");

/// The SPARQL risk rule ("the plan") as a CONSTRUCT, an inspection artifact only: nothing
/// materializes it. The live signal is [`RISK_RULE_SELECT_SPARQL`], evaluated at read time;
/// the two share one WHERE (kept in lock-step by a unit test).
pub const RISK_RULE_SPARQL: &str = include_str!("../ontology/risk-rule.rq");

/// The SELECT projection of the risk rule: the live supply-risk signal the durable monitor,
/// the slate scorer and the read-model tabs evaluate. Same WHERE as [`RISK_RULE_SPARQL`],
/// projecting the at-risk sales order (`?so`), the short component (`?comp`), the
/// `?shortfall`, and the `?commitment` date, one row per still-short (order, component). Its
/// trailing `ORDER BY` is the cause order: per order, purchased components before made ones,
/// then the largest shortfall, then the component IRI. An order's first row is the cause the
/// tabs light and the reason a newly opened point carries.
pub const RISK_RULE_SELECT_SPARQL: &str = include_str!("../ontology/risk-rule-select.rq");

/// The SELECT projection of the process resource-contention rule the durable
/// monitor queries: per work-center agent it finds work orders whose planned
/// windows overlap, projecting each contended anchor work order (`?wo`), its agent
/// (`?wc`) and derived `?capacity`, its `part_of` manufacturing order (`?mo`), the
/// window, and the count of overlapping peers (`?overlapPeers`) plus their IRIs.
/// The interpreter flags the order when concurrency (peers + 1) exceeds the agent's
/// capacity.
pub const PROCESS_CONTENTION_SELECT_SPARQL: &str =
    include_str!("../ontology/process-contention-select.rq");

/// The per-row contention predicate the [`PROCESS_CONTENTION_SELECT_SPARQL`] rows are
/// interpreted by. The durable monitor (which opens the action point) and the slate scorer
/// (which scores a projection) share this one definition, so the two cannot diverge.
///
/// Concurrency at an anchor work order is its overlapping peers plus itself; the anchor is
/// over its agent's capacity iff that concurrency exceeds capacity (a capacity-2 agent
/// running two overlapping jobs is fine; a third is contention). Returns `None` when the
/// peer count cannot be represented; callers treat that as no verdict and skip the row.
#[must_use]
pub fn over_capacity(capacity: f64, overlap_peers: i64) -> Option<bool> {
    let concurrency = overlap_peers.checked_add(1)?;
    let concurrency = i32::try_from(concurrency).ok()?;
    Some(f64::from(concurrency) > capacity)
}

/// The SELECT projection of the supply-risk + manufacturing-process coupling rule the
/// durable monitor groups coupled action points by. Each row links two subjects `?a ?b` of a
/// `?kind`: `fulfills` (a sales order and a manufacturing order fulfilling it) or `contention`
/// (two orders with overlapping work orders on one work center `?via`, projecting the center's
/// `?capacity` and the anchor's `?overlapPeers`). The capability interpreter couples a
/// contention row only when [`over_capacity`] holds.
pub const COUPLING_SELECT_SPARQL: &str = include_str!("../ontology/coupling-select.rq");

/// The SELECT projection of the process schedule-infeasibility rule the durable
/// monitor queries: per manufacturing order it projects the summed operation
/// `duration_expected` (`?required`), the planned window (`?startedAt`/`?endedAt`),
/// and the fulfilled promise's `?commitment`. The interpreter flags the order when
/// the required duration exceeds the available window (preferring
/// `endedAt - startedAt`, else `commitment - startedAt`).
pub const PROCESS_INFEASIBILITY_SELECT_SPARQL: &str =
    include_str!("../ontology/process-infeasibility-select.rq");

/// Idempotent SPARQL UPDATE that derives `ekg:precedes` between consecutive
/// `ekg:Operation` of a BOM from their `ekg:op_sequence`. The durable ingest path runs it
/// after each change to a record it reads ([`STRUCTURAL_DERIVATIONS`]); the monitor never
/// does.
pub const DERIVE_PRECEDES_SPARQL: &str = include_str!("../ontology/derive-precedes.rq");

/// Idempotent SPARQL UPDATE deriving the process edges that are not direct source fields:
/// `ekg:used` (an order's BOM components), `ekg:fulfills` (order origin matched to a
/// sales-order name), and the agent's `ekg:capacity` (from its capacity records). The durable
/// ingest path runs it after each change to a record it reads ([`STRUCTURAL_DERIVATIONS`]),
/// which keeps capability multi-hop reads out of the neutral ingestor.
pub const DERIVE_PROCESS_EDGES_SPARQL: &str = include_str!("../ontology/derive-process-edges.rq");

/// A structural derivation as the durable ingest path runs it: an idempotent SPARQL UPDATE
/// plus the source models whose records it reads. The ingest worker and the sweep run every
/// derivation that reads the model of the record they just materialized or retracted, before
/// announcing the change. They name no derivation themselves; the capability hands them this
/// list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derivation {
    /// A stable name for logs and the journal, e.g. `derive-precedes`.
    pub name: &'static str,
    /// The SPARQL UPDATE (clear the overlay graph, then re-insert, in one transaction).
    pub sparql: &'static str,
    /// The source models whose records the update reads.
    pub models: &'static [&'static str],
}

impl Derivation {
    /// Whether a change to a record of `model` can change this derivation's output.
    #[must_use]
    pub fn reads(&self, model: &str) -> bool {
        self.models.contains(&model)
    }
}

/// The capability's structural derivations, in the order they run: the operation ordering
/// ([`DERIVE_PRECEDES_SPARQL`]) and the non-direct process edges
/// ([`DERIVE_PROCESS_EDGES_SPARQL`]), each with the source models it reads. `mrp.workcenter`
/// is the agent the derived capacity lands on.
pub const STRUCTURAL_DERIVATIONS: &[Derivation] = &[
    Derivation {
        name: "derive-precedes",
        sparql: DERIVE_PRECEDES_SPARQL,
        models: &["mrp.routing.workcenter"],
    },
    Derivation {
        name: "derive-process-edges",
        sparql: DERIVE_PROCESS_EDGES_SPARQL,
        models: &[
            "mrp.production",
            "mrp.bom",
            "mrp.bom.line",
            "sale.order",
            "mrp.workcenter",
            "mrp.workcenter.capacity",
        ],
    },
];

/// The supply-risk source-to-ontology mapping contract. It is capability
/// configuration, not code: it says how Odoo source records project into the
/// typed world model. Embedded so the on-disk contract and the compiled binary
/// cannot drift, like the ontology artifacts above.
pub const CONTRACT_YAML: &str = include_str!("../contracts/supply-risk.yaml");

/// Vocabulary namespace: the `ekg:` prefix (classes and properties live here).
pub const VOCAB_NAMESPACE: &str = "https://ekg.example/ontology#";

/// Entity-instance namespace: one IRI per source record.
pub const ID_NAMESPACE: &str = "https://ekg.example/id/";

/// Named-graph namespace: one graph per entity (per-entity provenance).
pub const GRAPH_NAMESPACE: &str = "https://ekg.example/graph/";

/// Named graph the capability ontology is loaded into.
pub const ONTOLOGY_GRAPH: &str = "https://ekg.example/graph/ontology";

/// Named graph the neutral core ontology ([`CORE_TTL`]) is loaded into, separate from
/// the capability ontology graph so each is PUT-replaced idempotently.
pub const ONTOLOGY_CORE_GRAPH: &str = "https://ekg.example/graph/ontology-core";

/// GraphDB's reserved graph for transactionally-enforced SHACL write-shapes. Shapes
/// placed here are validated on every commit.
pub const SHACL_SHAPES_GRAPH: &str = "http://rdf4j.org/schema/rdf4j#SHACLShapeGraph";

/// Load the supply-risk mapping contract, validated against this crate's
/// ontology. This is the capability wiring: it binds the neutral loader
/// ([`crate::mapping::MappingContract`]) to the supply-risk configuration.
///
/// # Errors
/// [`ekg_domain::EngineError::Invariant`] if the embedded contract fails to parse
/// or validate against the ontology (a build-time-embedded drift).
pub fn supply_risk_contract() -> ekg_domain::Result<crate::mapping::MappingContract> {
    // Validate against the neutral core plus the capability ontology: the contract
    // references both neutral terms (`ekg:wasDerivedFrom`, the process metamodel) and
    // capability terms (`ekg:SalesOrder`, `ekg:ManufacturingOrder`).
    let ontology = format!("{CORE_TTL}\n{ONTOLOGY_TTL}");
    crate::mapping::MappingContract::load(CONTRACT_YAML, &ontology)
}

/// Stable IRI of a governed entity, e.g. `entity_iri("sale.order", 42)` gives
/// `https://ekg.example/id/sale.order/42`.
///
/// This is the authoritative scheme; the `ekg-domain` in-memory fake mirrors it (see
/// `ekg_domain::fakes`).
#[must_use]
pub fn entity_iri(model: &str, id: i64) -> String {
    format!("{ID_NAMESPACE}{model}/{id}")
}

/// Validate an untrusted entity id and rebuild it as a canonical entity IRI
/// (`{ID_NAMESPACE}<model>/<digits>`), or `None` if it is not one. The model segment must be
/// lower-case ASCII letters, digits, dots, or underscores (matching Odoo model names such as
/// `sale.order` or `l10n_de.tax`) and the id plain ASCII digits, so no crafted string, and
/// no provenance node such as `.../sale.order/42#source`, can be interpolated into a SPARQL
/// query or surface as an entity. Every IRI the tab projections (`crate::tabs`) and the
/// backend's GraphQL projection reads interpolate goes through this guard; the world model's
/// own driver paths validate their entity IRIs separately.
#[must_use]
pub fn canonical_subject(subject: &str) -> Option<ekg_domain::EntityUri> {
    let (model, id) = subject.strip_prefix(ID_NAMESPACE)?.split_once('/')?;
    let model_ok = !model.is_empty()
        && model
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
    let digits_ok = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    let id: i64 = id.parse().ok()?;
    (model_ok && digits_ok).then(|| ekg_domain::EntityUri(entity_iri(model, id)))
}

/// Whether `iri` is already a canonical entity IRI (it round-trips through
/// [`canonical_subject`] unchanged). This is the admission test for a node or edge endpoint.
#[must_use]
pub fn is_canonical_entity_iri(iri: &str) -> bool {
    canonical_subject(iri).is_some_and(|canonical| canonical.0 == iri)
}

/// The named graph an entity's triples live in, e.g.
/// `named_graph("sale.order", 42)` gives `https://ekg.example/graph/sale.order/42`.
#[must_use]
pub fn named_graph(model: &str, id: i64) -> String {
    format!("{GRAPH_NAMESPACE}{model}/{id}")
}

#[cfg(test)]
mod tests {
    // `expect` is the assertion mechanism in these tests.
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn artifacts_are_embedded_and_non_empty() {
        assert!(!CORE_TTL.trim().is_empty());
        assert!(!ONTOLOGY_TTL.trim().is_empty());
        assert!(!WRITE_SHAPES_TTL.trim().is_empty());
        assert!(!CONSISTENCY_SHAPES_TTL.trim().is_empty());
        assert!(!RISK_RULE_SPARQL.trim().is_empty());
    }

    #[test]
    fn ontology_declares_the_vocabulary_and_load_bearing_terms() {
        // Namespace binding the whole engine keys off.
        assert!(ONTOLOGY_TTL.contains(VOCAB_NAMESPACE));
        // A representative class from each corner of the flagship traversal, plus the
        // manufacturing-process specialisation classes.
        for term in [
            "ekg:SalesOrder",
            "ekg:Product",
            "ekg:BOM",
            "ekg:SupplierLink",
            "ekg:PurchaseOrderLine",
            "ekg:StockLevel",
            "ekg:ReorderRule",
            "ekg:ManufacturingOrder",
            "ekg:WorkOrder",
            "ekg:WorkCenter",
            "ekg:Operation",
            "ekg:WorkCenterCapacity",
        ] {
            assert!(ONTOLOGY_TTL.contains(term), "ontology missing {term}");
        }
        // The planning fields "the plan" reads, plus the process fields. (Neutral terms like
        // ekg:wasDerivedFrom live in CORE_TTL.)
        for field in [
            "ekg:commitment_date",
            "ekg:date_planned",
            "ekg:product_min_qty",
            "ekg:delay",
            "ekg:at_risk",
            "ekg:op_sequence",
            "ekg:capacity",
            "ekg:fulfills",
        ] {
            assert!(ONTOLOGY_TTL.contains(field), "ontology missing {field}");
        }
    }

    #[test]
    fn core_declares_the_neutral_governance_base_and_process_metamodel() {
        assert!(CORE_TTL.contains(VOCAB_NAMESPACE));
        for term in [
            // Governance base + provenance.
            "ekg:GovernedEntity",
            "ekg:SourceRecord",
            "ekg:wasDerivedFrom",
            "ekg:source_system",
            "ekg:revision",
            // PROV-aligned process metamodel.
            "ekg:Activity",
            "ekg:Agent",
            "ekg:Artifact",
            "ekg:ProcessState",
            "ekg:performedBy",
            "ekg:used",
            "ekg:produced",
            "ekg:precedes",
            "ekg:startedAt",
            "ekg:endedAt",
            "ekg:lifecycle_state",
        ] {
            assert!(CORE_TTL.contains(term), "core ontology missing {term}");
        }
    }

    #[test]
    fn derive_precedes_is_a_structural_idempotent_update() {
        // Clears its overlay graph (so a retraction leaves no stale edge), then re-derives
        // keyed on consecutive op_sequence, with no risk/monitor vocabulary. Behavioural
        // coverage: tests/process_derivations_it.rs.
        assert!(DERIVE_PRECEDES_SPARQL.contains("CLEAR SILENT GRAPH"));
        assert!(DERIVE_PRECEDES_SPARQL.contains("graph/derived/precedes"));
        assert!(DERIVE_PRECEDES_SPARQL.contains("INSERT"));
        assert!(DERIVE_PRECEDES_SPARQL.contains("ekg:precedes"));
        assert!(DERIVE_PRECEDES_SPARQL.contains("ekg:op_sequence"));
        assert!(DERIVE_PRECEDES_SPARQL.contains("ekg:operation_of"));
        assert!(DERIVE_PRECEDES_SPARQL.contains("FILTER NOT EXISTS"));
        assert!(!DERIVE_PRECEDES_SPARQL.contains("at_risk"));
    }

    #[test]
    fn derive_process_edges_covers_used_fulfills_capacity() {
        let s = DERIVE_PROCESS_EDGES_SPARQL;
        // One CLEAR of the overlay graph, then an INSERT per non-direct edge.
        // Behavioural coverage (incl. retract/reclaim) is tests/process_derivations_it.rs.
        assert!(s.contains("CLEAR SILENT GRAPH"));
        assert!(s.contains("graph/derived/process-edges"));
        assert!(
            s.matches("INSERT").count() >= 3,
            "an insert per derived edge"
        );
        assert!(s.contains("ekg:used") && s.contains("ekg:component_product"));
        assert!(
            s.contains("ekg:fulfills") && s.contains("ekg:origin_ref") && s.contains("ekg:name")
        );
        assert!(s.contains("ekg:capacity") && s.contains("ekg:capacity_of") && s.contains("MAX"));
        // A structural derivation: no risk/monitor vocabulary.
        assert!(!s.contains("at_risk"));
    }

    /// The contract's class-to-model map and, per `ekg:` property, the models that
    /// project it, read from the embedded contract so the derivation test tracks it.
    fn contract_terms() -> (
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeMap<String, Vec<String>>,
    ) {
        let raw: serde_json::Value =
            serde_saphyr::from_str(CONTRACT_YAML).expect("contract parses");
        let models = raw
            .get("models")
            .and_then(serde_json::Value::as_object)
            .expect("contract has models");
        let mut classes = std::collections::BTreeMap::new();
        let mut properties: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (model, mapping) in models {
            let model = model.clone();
            let class = mapping
                .get("class")
                .and_then(serde_json::Value::as_str)
                .expect("model class");
            classes.insert(class.to_owned(), model.clone());
            let fields = mapping
                .get("fields")
                .and_then(serde_json::Value::as_object)
                .expect("model fields");
            for field in fields.values() {
                let property = field
                    .get("property")
                    .and_then(serde_json::Value::as_str)
                    .expect("field property");
                properties
                    .entry(property.to_owned())
                    .or_default()
                    .push(model.clone());
            }
        }
        (classes, properties)
    }

    /// The `ekg:` local names `sparql` uses, comments dropped.
    fn ekg_terms(sparql: &str) -> std::collections::BTreeSet<String> {
        sparql
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .flat_map(|line| line.split("ekg:").skip(1))
            .map(|tail| {
                tail.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .filter(|term| !term.is_empty())
            .collect()
    }

    #[test]
    fn the_structural_derivations_name_the_models_they_read() {
        let names: Vec<&str> = STRUCTURAL_DERIVATIONS.iter().map(|d| d.name).collect();
        assert_eq!(names, ["derive-precedes", "derive-process-edges"]);
        assert_eq!(STRUCTURAL_DERIVATIONS[0].sparql, DERIVE_PRECEDES_SPARQL);
        assert_eq!(
            STRUCTURAL_DERIVATIONS[1].sparql,
            DERIVE_PROCESS_EDGES_SPARQL
        );
        assert_eq!(STRUCTURAL_DERIVATIONS[0].models, ["mrp.routing.workcenter"]);
        assert_eq!(
            STRUCTURAL_DERIVATIONS[1].models,
            [
                "mrp.production",
                "mrp.bom",
                "mrp.bom.line",
                "sale.order",
                "mrp.workcenter",
                "mrp.workcenter.capacity",
            ]
        );

        // Every model the SPARQL reads is in its set: each class it types, and each
        // property only one model projects (a shared one, like `name`, is typed alongside).
        let (classes, properties) = contract_terms();
        for derivation in STRUCTURAL_DERIVATIONS {
            for term in ekg_terms(derivation.sparql) {
                let read = classes.get(&term).cloned().or_else(|| {
                    properties
                        .get(&term)
                        .filter(|models| models.len() == 1)
                        .map(|models| models[0].clone())
                });
                if let Some(model) = read {
                    assert!(
                        derivation.reads(&model),
                        "{} reads ekg:{term} of {model} but does not name it",
                        derivation.name
                    );
                }
            }
            // And every model it names is a contract model.
            for model in derivation.models {
                assert!(
                    classes.values().any(|m| m == model),
                    "{} names {model}, which the contract does not map",
                    derivation.name
                );
            }
        }
        assert!(!STRUCTURAL_DERIVATIONS[1].reads("res.partner"));
    }

    #[test]
    fn core_is_capability_neutral() {
        // The neutral core must name no use case: no capability class or
        // capability-specific term may be declared in it.
        for capability_term in [
            "SalesOrder",
            "PurchaseOrder",
            "Shipment",
            "SupplierLink",
            "StockLevel",
            "ReorderRule",
            "ManufacturingOrder",
            "WorkOrder",
            "WorkCenter",
            "commitment_date",
            "at_risk",
            "supplier",
        ] {
            assert!(
                !CORE_TTL.contains(capability_term),
                "neutral core must not name the capability term {capability_term:?}"
            );
        }
    }

    #[test]
    fn manufacturing_classes_specialise_the_neutral_core() {
        // The subclass axioms that tie the manufacturing specialisation to the core.
        for axiom in [
            "ekg:ManufacturingOrder\n    a owl:Class ;\n    rdfs:subClassOf ekg:Activity",
            "ekg:WorkOrder\n    a owl:Class ;\n    rdfs:subClassOf ekg:Activity",
            "ekg:Operation\n    a owl:Class ;\n    rdfs:subClassOf ekg:Activity",
            "ekg:WorkCenter\n    a owl:Class ;\n    rdfs:subClassOf ekg:Agent",
        ] {
            assert!(
                ONTOLOGY_TTL.contains(axiom),
                "missing specialisation axiom:\n{axiom}"
            );
        }
        // Product is retro-typed as an artifact so used/produced link to it.
        assert!(ONTOLOGY_TTL.contains("rdfs:subClassOf ekg:Artifact"));
    }

    #[test]
    fn write_shapes_enforce_provenance_inline_and_carry_no_cross_entity_class() {
        // Provenance is enforced on write via an inlined property on the neutral
        // base-class target (the `sh:node` indirection does not fire on write).
        assert!(WRITE_SHAPES_TTL.contains("sh:targetClass ekg:GovernedEntity"));
        assert!(WRITE_SHAPES_TTL.contains("ekg:wasDerivedFrom"));
        assert!(
            !WRITE_SHAPES_TTL.contains("sh:node ekg:"),
            "write-shapes must inline constraints, not use the sh:node indirection"
        );
        // Write-shapes must not carry cross-entity sh:class constraints (those would
        // reject valid incremental writes), except the same-transaction provenance
        // SourceRecord node. So the only sh:class permitted is ekg:SourceRecord.
        // Count over non-comment lines so prose mentioning sh:class doesn't fool us.
        let class_lines: Vec<&str> = WRITE_SHAPES_TTL
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && l.contains("sh:class "))
            .collect();
        assert!(
            class_lines
                .iter()
                .all(|l| l.contains("sh:class ekg:SourceRecord")),
            "write-shapes may only assert sh:class on the provenance SourceRecord node: {class_lines:?}"
        );
    }

    #[test]
    fn consistency_shapes_carry_the_cross_entity_constraints() {
        // The cross-entity constraints live here, validated over the whole graph.
        for term in [
            "sh:class ekg:OrderLine",
            "sh:class ekg:Product",
            "sh:targetClass ekg:SalesOrder",
        ] {
            assert!(
                CONSISTENCY_SHAPES_TTL.contains(term),
                "consistency-shapes missing {term}"
            );
        }
        // Provenance/own-value are not repeated here.
        assert!(!CONSISTENCY_SHAPES_TTL.contains("ekg:wasDerivedFrom"));
    }

    #[test]
    fn risk_rule_is_deterministic_and_reads_the_promise() {
        // Reads the promise and the inbound arrival date, and asserts at_risk, with
        // no language model in the truth path.
        assert!(RISK_RULE_SPARQL.contains("CONSTRUCT"));
        assert!(RISK_RULE_SPARQL.contains("ekg:at_risk"));
        assert!(RISK_RULE_SPARQL.contains("ekg:commitment_date"));
        assert!(RISK_RULE_SPARQL.contains("ekg:date_planned"));
        assert!(RISK_RULE_SPARQL.contains("ekg:shortfall"));
    }

    /// Normalize a SPARQL text for structural comparison: drop `#` comments and every
    /// whitespace character outside string literals and IRIs. A `#` inside a `"..."` /
    /// `'...'` literal or an `<IRI>` is content, not a comment; a `<` followed by whitespace
    /// or `=` is a comparison operator (IRIs hold no spaces), not an IRI opener.
    fn normalized(sparql: &str) -> String {
        let mut out = String::new();
        for line in sparql.lines() {
            let chars: Vec<char> = line.chars().collect();
            let (mut in_iri, mut quote) = (false, None::<char>);
            for (i, &c) in chars.iter().enumerate() {
                match (quote, c) {
                    (Some(q), _) if c == q => quote = None,
                    (None, '"' | '\'') if !in_iri => quote = Some(c),
                    (None, '<') if !in_iri => {
                        let next = chars.get(i + 1).copied();
                        in_iri = !matches!(next, None | Some('=' | ' ' | '\t'));
                    }
                    (None, '>') if in_iri => in_iri = false,
                    (None, '#') if !in_iri => break,
                    _ => {}
                }
                if quote.is_some() || in_iri || !c.is_whitespace() {
                    out.push(c);
                }
            }
        }
        out
    }

    #[test]
    fn normalization_keeps_literals_and_iris_and_drops_only_comments() {
        // `#` in an IRI and in a string literal is content; a trailing comment is dropped.
        assert_eq!(
            normalized(
                "PREFIX ekg: <https://x.example/o#>  # vocab\nFILTER(CONTAINS(STR(?n), \"#\")) # no hash"
            ),
            "PREFIXekg:<https://x.example/o#>FILTER(CONTAINS(STR(?n),\"#\"))"
        );
        // `<=` is an operator, so the comment after it is still dropped.
        assert_eq!(normalized("FILTER(?a <= ?b) # c"), "FILTER(?a<=?b)");
        assert_eq!(normalized("FILTER(?a < ?b) # c"), "FILTER(?a<?b)");
    }

    /// The text after a query's top-level `WHERE {` (normalized), i.e. its WHERE body plus
    /// the closing brace.
    fn where_body(sparql: &str) -> String {
        let norm = normalized(sparql);
        // The CONSTRUCT template contains no `WHERE{`, so the first one is the query's own.
        let at = norm.find("WHERE{").map_or(0, |i| i + "WHERE{".len());
        norm[at..].to_owned()
    }

    /// The risk SELECT's one solution modifier, normalized: the cause order.
    const CAUSE_ORDER: &str =
        "ORDERBY?soASC(IF(EXISTS{?madeByekg:produces_product?comp},1,0))DESC(?shortfall)?comp";

    /// Split a normalized SELECT WHERE body into the body without its closing brace and the
    /// trailing solution modifier (empty when there is none).
    fn select_modifier(body: &str) -> (&str, &str) {
        match body.rfind("}ORDERBY") {
            Some(at) => (&body[..at], &body[at + 1..]),
            None => (
                body.strip_suffix('}')
                    .expect("the SELECT WHERE body ends with its closing brace"),
                "",
            ),
        }
    }

    #[test]
    fn the_supply_risk_contract_scopes_stock_to_internal_locations_and_shipments_to_receipts() {
        let scopes = supply_risk_contract()
            .expect("the supply-risk contract loads")
            .source_scopes();
        assert_eq!(
            scopes.get("stock.quant"),
            Some(&vec![serde_json::json!([
                "location_id.usage",
                "=",
                "internal"
            ])]),
            "only internal stock is on hand"
        );
        assert_eq!(
            scopes.get("stock.picking"),
            Some(&vec![
                serde_json::json!(["picking_type_code", "=", "incoming"]),
                // A return goes back to the vendor even when it reuses the receipt type.
                serde_json::json!(["location_dest_id.usage", "=", "internal"]),
            ]),
            "only receipts are delivering shipments, never a return to the vendor"
        );
        assert_eq!(scopes.len(), 2, "nothing else is scoped: {scopes:?}");
    }

    #[test]
    fn construct_and_select_risk_rules_share_one_where() {
        // The CONSTRUCT is inspection-only and the SELECT is the live signal; they must never
        // diverge. The CONSTRUCT's WHERE is the SELECT's WHERE plus only the final risk-IRI
        // minting (the finding's stable IRI, which a SELECT does not need).
        let select = where_body(RISK_RULE_SELECT_SPARQL);
        let construct = where_body(RISK_RULE_SPARQL);
        // The SELECT's only extra is its trailing solution modifier (the cause order); a
        // CONSTRUCT is a graph, so it has none.
        let (shared, modifier) = select_modifier(&select);
        assert_eq!(
            modifier, CAUSE_ORDER,
            "the SELECT may add only the cause order after the shared WHERE, nothing more"
        );
        let extra = construct
            .strip_prefix(shared)
            .expect("the CONSTRUCT WHERE must start with the SELECT WHERE, verbatim");
        assert!(
            extra.starts_with("BIND(IRI(CONCAT(") && extra.ends_with("AS?risk)}"),
            "the CONSTRUCT may add only the risk-IRI minting after the shared WHERE, got: {extra}"
        );
    }

    #[test]
    fn the_risk_select_orders_each_orders_causes_purchased_first_then_by_shortfall() {
        // The one cause order: an order's first row is the cause the monitor's reason names
        // and the component the tabs light, so the SELECT, not the store's row order, must
        // fix it.
        let body = where_body(RISK_RULE_SELECT_SPARQL);
        let (_, modifier) = select_modifier(&body);
        assert_eq!(
            modifier, CAUSE_ORDER,
            "per order: purchased before made, the largest shortfall, then the component IRI"
        );
        assert!(
            !normalized(RISK_RULE_SPARQL).contains("ORDERBY"),
            "the CONSTRUCT is a graph: it carries no order"
        );
    }

    #[test]
    fn the_rules_requirement_is_the_tabs_demand_join() {
        // The rule's per-component requirement and the Suppliers tab's 30-day demand are the
        // same first-order roll-up: tied to the ordered product's own BOM tree and summed per
        // (order line, BOM line). Both rule files carry it (the lock-step test keeps them
        // equal); the tab's demand join is the same pattern under its own variable names.
        let tree = "(^ekg:produces_product/ekg:has_line/ekg:component_product)*";
        for rule in [RISK_RULE_SPARQL, RISK_RULE_SELECT_SPARQL] {
            let norm = normalized(rule);
            for term in [
                "SELECT?so?comp(SUM(?lineRequired)AS?required)",
                "SELECTDISTINCT?so?line?cline?comp?lineRequired",
                &format!("?finished{tree}?parent."),
                "?pbomekg:produces_product?parent;ekg:has_line?cline.",
                "?clineekg:component_product?comp;ekg:product_qty?compQty.",
                "}GROUPBY?so?comp}",
            ] {
                assert!(norm.contains(term), "the rule's requirement lacks {term}");
            }
            assert!(
                !norm.contains("component_product)+?comp"),
                "no untied reachability join: another product's BOM line would count"
            );
        }
        let demand = normalized(crate::tabs::queries::SUPPLIERS_DETAIL_RQ);
        for term in [
            "SELECTDISTINCT?demandLine?componentLine?stock_of?required",
            &format!("?finished{tree}?parent."),
            "?parentBomekg:produces_product?parent;ekg:has_line?componentLine.",
            "?componentLineekg:component_product?stock_of;ekg:product_qty?componentQty.",
            "SUM(?required)AS?demand",
        ] {
            assert!(demand.contains(term), "the tab's demand lacks {term}");
        }
    }

    #[test]
    fn the_risk_rule_counts_only_a_lines_open_quantity_as_inbound() {
        // Received units are on hand: per line only product_qty - qty_received is
        // inbound, and a completed receipt's ETA does not stand in for the arrival.
        for rule in [RISK_RULE_SPARQL, RISK_RULE_SELECT_SPARQL] {
            let norm = normalized(rule);
            assert!(
                norm.contains("SUM(IF(BOUND(?eta)&&?eta<=?commitment,?openQty,0))"),
                "inbound sums the open quantity"
            );
            assert!(
                norm.contains("BIND((?pq-COALESCE(?received,0))AS?openQty)"),
                "the open quantity is ordered minus received"
            );
            assert!(
                norm.contains("FILTERNOTEXISTS{?shipekg:completed_at?shipDone}FILTERNOTEXISTS{?shipekg:transport_status\"cancel\"}"),
                "only an open (not completed, not cancelled) shipment's ETA stands in for the arrival"
            );
            assert!(
                norm.contains("FILTERNOTEXISTS{?oshipekg:completed_at?oDone}FILTERNOTEXISTS{?oshipekg:transport_status\"cancel\"}"),
                "a PO whose shipments are all completed or cancelled delivers nothing more"
            );
            assert!(
                norm.contains(
                    "FILTERNOTEXISTS{?cpoekg:has_po_line?pol;ekg:lifecycle_state\"cancel\".}"
                ),
                "a cancelled purchase order delivers nothing"
            );
        }
    }

    #[test]
    fn process_contention_select_projects_the_load_bearing_terms() {
        // A plain projection over the process graph: overlapping work orders on one
        // agent, the agent's capacity, and the anchor's manufacturing order. The
        // capacity comparison is the interpreter's job, not the SPARQL's.
        let s = PROCESS_CONTENTION_SELECT_SPARQL;
        assert!(s.contains("SELECT ?wo"));
        for term in [
            "ekg:WorkOrder",
            "ekg:performedBy",
            "ekg:part_of",
            "ekg:startedAt",
            "ekg:endedAt",
            "ekg:capacity",
            "?overlapPeers",
        ] {
            assert!(s.contains(term), "contention SELECT missing {term}");
        }
        // A plain projection: no risk/monitor vocabulary leaks into the query.
        assert!(!s.contains("at_risk"));
    }

    #[test]
    fn coupling_select_projects_the_link_and_reads_only_governed_terms() {
        let s = COUPLING_SELECT_SPARQL;
        assert!(
            s.contains("SELECT ?a ?b ?kind ?capacity ?overlapPeers"),
            "the projection the interpreter reads"
        );
        assert!(s.contains("\"fulfills\"") && s.contains("\"contention\""));
        // A read: no update form, no remote or foreign graph.
        for forbidden in ["INSERT", "DELETE", "CLEAR", "SERVICE", "LOAD", "at_risk"] {
            assert!(!s.contains(forbidden), "coupling SELECT uses {forbidden}");
        }
        // Every `ekg:` term it reads is declared in the governed ontology (core + capability).
        let declared: std::collections::BTreeSet<String> = CORE_TTL
            .lines()
            .chain(ONTOLOGY_TTL.lines())
            .filter_map(|line| line.trim().strip_prefix("ekg:"))
            .map(|tail| {
                tail.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .collect();
        let read = ekg_terms(s);
        assert!(read.contains("fulfills") && read.contains("capacity"));
        for term in &read {
            assert!(declared.contains(term), "ekg:{term} is not a governed term");
        }
    }

    #[test]
    fn process_infeasibility_select_projects_the_load_bearing_terms() {
        // Per-order summed operation duration vs the planned window / commitment.
        let s = PROCESS_INFEASIBILITY_SELECT_SPARQL;
        assert!(s.contains("SELECT ?mo"));
        assert!(s.contains("SUM(?duration)"));
        for term in [
            "ekg:ManufacturingOrder",
            "ekg:builds_from",
            "ekg:operation_of",
            "ekg:duration_expected",
            "ekg:startedAt",
            "ekg:endedAt",
            "ekg:fulfills",
            "ekg:commitment_date",
        ] {
            assert!(s.contains(term), "infeasibility SELECT missing {term}");
        }
        assert!(!s.contains("at_risk"));
    }

    #[test]
    fn canonical_subject_accepts_a_valid_entity_iri_and_rebuilds_it() {
        let iri = entity_iri("sale.order", 42);
        let canonical = canonical_subject(&iri).expect("a valid entity IRI");
        assert_eq!(canonical.0, iri);
        assert!(is_canonical_entity_iri(&iri));
    }

    #[test]
    fn canonical_subject_rebuilds_leading_zeros_so_only_the_rebuilt_iri_is_canonical() {
        // `.../007` is accepted as an id and rebuilt as `.../7`, the IRI a query then carries.
        // It is not itself canonical: a node or edge endpoint spelled `.../007` is never
        // admitted, because admission is the round trip, not mere acceptance.
        let padded = format!("{ID_NAMESPACE}sale.order/007");
        assert_eq!(
            canonical_subject(&padded).expect("digits").0,
            entity_iri("sale.order", 7)
        );
        assert!(!is_canonical_entity_iri(&padded));
        assert!(is_canonical_entity_iri(&entity_iri("sale.order", 7)));
    }

    #[test]
    fn canonical_subject_rejects_crafted_or_malformed_ids() {
        // An injection payload in the id fails the digit check, so it is never interpolated.
        let crafted = format!("{ID_NAMESPACE}sale.order/1> }} UNION {{ ?s ?p ?o");
        assert!(canonical_subject(&crafted).is_none());
        // Not under the id namespace, and a non-numeric id.
        assert!(canonical_subject("not-an-iri").is_none());
        assert!(canonical_subject(&format!("{ID_NAMESPACE}sale.order/abc")).is_none());
        // A signed id is not plain digits; a provenance node is not an entity.
        assert!(canonical_subject(&format!("{ID_NAMESPACE}sale.order/+5")).is_none());
        assert!(canonical_subject(&format!("{ID_NAMESPACE}sale.order/-5")).is_none());
        assert!(!is_canonical_entity_iri(&format!(
            "{ID_NAMESPACE}sale.order/288#source"
        )));
        assert!(!is_canonical_entity_iri(&format!(
            "{ID_NAMESPACE}Sale.Order/1"
        )));
    }

    #[test]
    fn iri_scheme_matches_the_engine_seams() {
        // Must match ekg_domain::fakes so materialization and the demo fake agree.
        assert_eq!(
            entity_iri("sale.order", 42),
            "https://ekg.example/id/sale.order/42"
        );
        assert_eq!(
            named_graph("sale.order", 42),
            "https://ekg.example/graph/sale.order/42"
        );
        assert!(entity_iri("product.product", 7).starts_with(ID_NAMESPACE));
    }
}
