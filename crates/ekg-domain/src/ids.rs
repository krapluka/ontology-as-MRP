//! Neutral identifiers used across the engine seams.
//!
//! Thin newtypes over owned strings and ints: no bare `String` parameters in the
//! contracts, and no RDF or UUID dependency.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable IRI of a governed entity in the world model,
/// e.g. `https://ekg.example/id/sale.order/42`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityUri(pub String);

/// The source model segment of an entity id IRI (`…/id/<model>/<id>`), e.g.
/// `sale.order` or `mrp.workorder`; `None` for a value that is not an entity id
/// IRI (a literal, or a foreign IRI). The one parse every model check in the engine
/// shares.
#[must_use]
pub fn entity_model(iri: &str) -> Option<&str> {
    iri.split_once("/id/")
        .and_then(|(_, tail)| tail.split_once('/'))
        .map(|(model, _id)| model)
        .filter(|model| !model.is_empty())
}

impl EntityUri {
    /// The source model segment of this entity id IRI (`…/id/<model>/<id>`), or
    /// `None` when it is not an entity id IRI. See [`entity_model`].
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        entity_model(&self.0)
    }
}

/// The named graph an entity's triples live in. One graph per entity keeps
/// per-entity provenance and revisioning isolated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamedGraph(pub String);

/// A per-entity monotonic revision. Consumers reject events whose revision is
/// not greater than the last they applied for that entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Revision(pub u64);

impl Revision {
    /// The revision immediately after this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Identifier of a read/access policy, cited by every recorded omission.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PolicyId(pub String);

/// Opaque identifier of an action point.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionPointId(pub String);

/// Identity of the human reviewer exercising oversight over a promotion.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewerId(pub String);

/// Correlation id threading one governed episode (open, propose, promote,
/// execute, reverse) across the audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub String);

/// Identity of a portfolio of coupled action points, `portfolio/{digest}`: derived from
/// its member subjects alone, so the same members always form the same portfolio and a
/// different set never does.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PortfolioId(pub String);

/// The FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// The FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// How many lowercase hex characters a [`short_digest`] carries.
pub const SHORT_DIGEST_HEX_CHARS: usize = 12;

/// A short, stable digest of `bytes`: the first [`SHORT_DIGEST_HEX_CHARS`] lowercase hex
/// characters of their 64-bit FNV-1a hash. Deterministic across runs and platforms; not a
/// security hash. Shared by the slate's option keys and the portfolio ids.
#[must_use]
pub fn short_digest(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    let mut hex = format!("{hash:016x}");
    hex.truncate(SHORT_DIGEST_HEX_CHARS);
    hex
}

impl fmt::Display for PortfolioId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EntityUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for NamedGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn the_short_digest_is_twelve_lowercase_hex_characters_of_fnv1a() {
        // Published FNV-1a 64 vectors: "" is cbf29ce484222325, "a" is af63dc4c8601ec8c.
        // Pinning them pins every digest built on the helper.
        assert_eq!(short_digest(b""), "cbf29ce48422");
        assert_eq!(short_digest(b"a"), "af63dc4c8601");
        let digest = short_digest(b"https://ekg.example/id/sale.order/1");
        assert_eq!(digest.len(), SHORT_DIGEST_HEX_CHARS);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            digest,
            short_digest(b"https://ekg.example/id/sale.order/1"),
            "stable"
        );
    }
}
