//! Semantic layer: the governed RDF/OWL world model.
//!
//! Owns the real drivers for the [`WorldModel`] and [`SourceIngestor`] seams:
//! the OWL projection of Odoo's MRP model, named-graph materialization with
//! provenance, and structural read-policy with recorded omissions.
//!
//! Structural integrity is enforced in two tiers, because named-graph-per-entity
//! materialization is incremental:
//! - write-shapes ([`ontology::WRITE_SHAPES_TTL`]): own datatype/cardinality
//!   plus inlined provenance, enforced transactionally on every materialization. A
//!   structurally-malformed own entity is rejected ([`ekg_domain::EngineError::Invariant`]),
//!   and a valid write is never rejected for a not-yet-materialized referent.
//! - consistency-shapes ([`ontology::CONSISTENCY_SHAPES_TTL`]): the cross-entity
//!   constraints, validated over the settled whole graph via
//!   [`GraphDbClient::validate`] as a non-blocking governance signal, never a write
//!   gate.
//!
//! The deterministic SPARQL risk rule ("the plan") runs over the typed world model.
//!
//! The contract is defined in `ekg_domain::seams`; this crate is the seam's home
//! for the semantic layer. [`world_model`] holds the [`WorldModel`] driver over
//! GraphDB; the typed source-to-ontology mapping ([`mapping`]) turns pulled records
//! into typed entities; this crate also owns the ontology artifacts and the IRI
//! scheme ([`ontology`]).

// Docs mention product and standard names (GraphDB, Odoo, SHACL, SPARQL).
#![allow(clippy::doc_markdown)]

pub mod display;
pub mod fixture;
pub mod ingest;
pub mod mapping;
pub mod ontology;
pub mod scorer;
pub mod simulator;
pub mod tabs;
pub mod world_model;

pub use ekg_domain::{Scorer, Simulator, SourceIngestor, WorldModel};
pub use ingest::{OdooConfig, OdooIngestor};
pub use mapping::MappingContract;
pub use scorer::{ProcessContentionScorer, SupplyRiskScorer};
pub use simulator::{GraphDbProjection, GraphDbSimulator};
pub use world_model::{ConformanceReport, GraphDbClient, GraphDbWorldModel, Violation};
