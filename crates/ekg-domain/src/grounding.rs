//! Grounding: the evidence subgraph an agent reasons over, plus the structural
//! read-policy's recorded omissions.

use serde::{Deserialize, Serialize};

use crate::ids::{EntityUri, PolicyId, Revision};
use crate::source::SourceRef;

/// The caller context that read-policy is scoped by: role and purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadContext {
    /// The caller's role, e.g. `"planner"`.
    pub role: String,
    /// Why the caller is reading, e.g. `"delivery-promise"`.
    pub purpose: String,
}

/// Whether an omission hid a node or an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionKind {
    /// A node the caller may not see was excluded.
    Node,
    /// An edge the caller may not traverse was excluded.
    Edge,
}

/// A record that read-policy hid part of the graph, so a grounding result is never
/// silently incomplete. Carried as structured data, never parsed from model prose,
/// and recorded in the audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omission {
    /// Node or edge.
    pub kind: OmissionKind,
    /// The RDF type or predicate IRI that was hidden.
    pub type_iri: String,
    /// How many were hidden.
    pub count: u32,
    /// The policy that hid them.
    pub policy_id: PolicyId,
}

/// One evidence edge in the grounding subgraph (subject, predicate, object), with
/// provenance back to the governed source record it derived from.
///
/// Provenance is mandatory: every edge an agent grounds on traces to a `source` at
/// a specific `revision`, so a proposal's evidence is attributable and reproducible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEdge {
    /// The subject entity.
    pub subject: EntityUri,
    /// The predicate IRI.
    pub predicate: String,
    /// The object (IRI or literal, rendered as a string in the neutral core).
    pub object: String,
    /// The source record this edge was derived from (`ekg:wasDerivedFrom`).
    pub source: SourceRef,
    /// The entity revision the edge was read at (reproducibility).
    pub revision: Revision,
}

/// The evidence subgraph an agent grounds on, plus any omissions read-policy
/// imposed while building it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grounding {
    /// The entity the grounding is about.
    pub entity: EntityUri,
    /// Visible evidence edges.
    pub edges: Vec<EvidenceEdge>,
    /// What policy hid (empty when nothing was hidden).
    pub omissions: Vec<Omission>,
}

impl Grounding {
    /// Whether read-policy hid anything from this grounding.
    #[must_use]
    pub fn has_omissions(&self) -> bool {
        !self.omissions.is_empty()
    }
}
