//! The `RdfChanged` fan-out event.

use serde::{Deserialize, Serialize};

use crate::ids::{EntityUri, NamedGraph, Revision};
use crate::source::SourceRef;

/// The kind of change applied to an entity's world-model graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The entity's named graph was created.
    Created,
    /// The entity's named graph was updated in place.
    Updated,
    /// The entity's named graph was retracted (source record gone).
    Retracted,
}

impl ChangeKind {
    /// The `<kind>` token used in the subject `ekg.rdf.v1.<model>.<kind>`.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Retracted => "retracted",
        }
    }
}

/// An RDF-change event, published on the subject-addressed fan-out backbone so
/// independent consumers (monitor, vector projection, viz) subscribe without polling
/// the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdfChanged {
    /// The entity whose graph changed.
    pub entity_uri: EntityUri,
    /// The named graph that changed.
    pub named_graph: NamedGraph,
    /// What kind of change occurred.
    pub change_kind: ChangeKind,
    /// The source record the change derived from.
    pub source: SourceRef,
    /// Per-entity monotonic revision; consumers reject non-increasing revisions.
    pub revision: Revision,
}

impl RdfChanged {
    /// The subject this event is published on: `ekg.rdf.v1.<model>.<kind>`.
    #[must_use]
    pub fn subject(&self) -> String {
        format!(
            "ekg.rdf.v1.{}.{}",
            self.source.model,
            self.change_kind.as_token()
        )
    }
}
