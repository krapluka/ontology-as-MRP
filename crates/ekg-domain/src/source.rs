//! Source-system references and pulled records.
//!
//! The neutral core carries source facts as string fields; typed mapping into
//! RDF is the semantic layer's job (a YAML data contract), so no RDF or JSON
//! dependency leaks in here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A reference to a single record in a source system of record.
///
/// This is the thin-notify payload. It is untrusted: the authoritative fact is
/// always re-read via [`crate::SourceIngestor::pull`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRef {
    /// Source system, e.g. `"odoo"`.
    pub system: String,
    /// Source model, e.g. `"sale.order"`.
    pub model: String,
    /// Source primary key.
    pub id: i64,
}

/// An authoritative record fat-pulled from a source system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecord {
    /// Where the record came from: the provenance root (`ekg:wasDerivedFrom`).
    pub source_ref: SourceRef,
    /// Field values keyed by source field name. Kept as strings in the neutral
    /// core; the semantic layer applies typing and the RDF projection.
    pub fields: BTreeMap<String, String>,
}
