//! The capability-neutral risk-rule seam the monitor evaluates.
//!
//! `ekg-dynamic` names no use case, so it embeds no capability's SPARQL. A
//! [`RiskRule`] carries an opaque SELECT query plus an interpreter that turns one
//! solution row into an optional [`DriftVerdict`]. The concrete rule is injected by
//! the capability wiring (the backend).
//!
//! A [`CouplingRule`] is the same shape for grouping: an opaque SELECT plus an
//! interpreter that turns a row into an undirected [`CouplingEdge`] between two
//! subject entities. The monitor groups coupled at-risk points by these edges.

use std::collections::BTreeMap;
use std::sync::Arc;

use ekg_domain::{CouplingLink, EntityUri};
use serde::{Deserialize, Serialize};

/// A deterministic verdict that a subject has drifted off plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftVerdict {
    /// The subject entity the verdict is about.
    pub subject: EntityUri,
    /// A deterministic, human-readable reason (never model-generated).
    pub reason: String,
}

/// One SPARQL solution row: variable name to term string, as returned by
/// [`ekg_domain::WorldModel::select`].
pub type Row = BTreeMap<String, String>;

/// A row interpreter: maps one solution row to a drift verdict, or `None`.
type Interpreter = Arc<dyn Fn(&Row) -> Option<DriftVerdict> + Send + Sync>;

/// A deterministic capability rule: a SELECT query over the world model plus a
/// row interpreter. Cheap to clone (the interpreter is behind an `Arc`).
#[derive(Clone)]
pub struct RiskRule {
    sparql: String,
    interpret: Interpreter,
}

impl RiskRule {
    /// Build a rule from its SELECT query and a row interpreter.
    ///
    /// `interpret` returns `Some` for a row that indicates drift (extracting the
    /// subject IRI and a deterministic reason from the row's bindings) and `None`
    /// otherwise.
    #[must_use]
    pub fn new(
        sparql: impl Into<String>,
        interpret: impl Fn(&Row) -> Option<DriftVerdict> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sparql: sparql.into(),
            interpret: Arc::new(interpret),
        }
    }

    /// The SELECT query to run through [`ekg_domain::WorldModel::select`].
    #[must_use]
    pub fn sparql(&self) -> &str {
        &self.sparql
    }

    /// All drift verdicts across the returned solution rows.
    #[must_use]
    pub fn verdicts(&self, rows: &[Row]) -> Vec<DriftVerdict> {
        rows.iter()
            .filter_map(|row| (self.interpret)(row))
            .collect()
    }
}

// The interpreter closure is not `Debug`, so print the query only.
impl std::fmt::Debug for RiskRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiskRule")
            .field("sparql", &self.sparql)
            .finish_non_exhaustive()
    }
}

/// An undirected coupling between two subject entities, with a deterministic reason.
/// Built through [`CouplingEdge::new`], which orders the two ends, so the edge between A
/// and B equals the edge between B and A.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CouplingEdge {
    /// The lesser end (by IRI).
    pub a: EntityUri,
    /// The greater end (by IRI).
    pub b: EntityUri,
    /// Why the two are coupled (never model-generated).
    pub reason: String,
    /// The capability's opaque facts about the link (see [`CouplingLink`]). Defaulted and
    /// skipped when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
}

impl CouplingEdge {
    /// The undirected edge between `x` and `y`, or `None` for a self-loop (a subject is
    /// not coupled to itself).
    #[must_use]
    pub fn new(x: EntityUri, y: EntityUri, reason: impl Into<String>) -> Option<Self> {
        let (a, b) = match x.0.cmp(&y.0) {
            std::cmp::Ordering::Less => (x, y),
            std::cmp::Ordering::Greater => (y, x),
            std::cmp::Ordering::Equal => return None,
        };
        Some(Self {
            a,
            b,
            reason: reason.into(),
            facts: BTreeMap::new(),
        })
    }

    /// The edge carrying the capability's `facts` about it.
    #[must_use]
    pub fn with_facts(mut self, facts: BTreeMap<String, String>) -> Self {
        self.facts = facts;
        self
    }

    /// The link this edge records when its group forms.
    #[must_use]
    pub fn link(&self) -> CouplingLink {
        CouplingLink {
            a: self.a.clone(),
            b: self.b.clone(),
            reason: self.reason.clone(),
            facts: self.facts.clone(),
        }
    }
}

/// A coupling interpreter: maps one solution row to a coupling edge, or `None`.
type CouplingInterpreter = Arc<dyn Fn(&Row) -> Option<CouplingEdge> + Send + Sync>;

/// A deterministic capability coupling rule: a SELECT query over the world model plus a
/// row interpreter yielding undirected coupling edges. Injected like a [`RiskRule`]; cheap
/// to clone.
#[derive(Clone)]
pub struct CouplingRule {
    sparql: String,
    interpret: CouplingInterpreter,
}

impl CouplingRule {
    /// Build a rule from its SELECT query and a row interpreter, which returns `Some` for
    /// a row that couples two subjects and `None` otherwise (including a malformed row).
    #[must_use]
    pub fn new(
        sparql: impl Into<String>,
        interpret: impl Fn(&Row) -> Option<CouplingEdge> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sparql: sparql.into(),
            interpret: Arc::new(interpret),
        }
    }

    /// The SELECT query to run through [`ekg_domain::WorldModel::select`].
    #[must_use]
    pub fn sparql(&self) -> &str {
        &self.sparql
    }

    /// All coupling edges across the returned solution rows.
    #[must_use]
    pub fn edges(&self, rows: &[Row]) -> Vec<CouplingEdge> {
        rows.iter()
            .filter_map(|row| (self.interpret)(row))
            .collect()
    }
}

impl std::fmt::Debug for CouplingRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CouplingRule")
            .field("sparql", &self.sparql)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    /// A demo rule: a row whose `shortfall` binding parses to a positive number is
    /// a drift, keyed by the row's `entity` binding.
    fn shortfall_rule() -> RiskRule {
        RiskRule::new("SELECT ?entity ?shortfall WHERE { ... }", |row| {
            let shortfall: i64 = row.get("shortfall")?.parse().ok()?;
            if shortfall > 0 {
                Some(DriftVerdict {
                    subject: EntityUri(row.get("entity")?.clone()),
                    reason: format!("shortfall {shortfall} before commitment date"),
                })
            } else {
                None
            }
        })
    }

    fn row(entity: &str, shortfall: &str) -> Row {
        let mut r = Row::new();
        r.insert("entity".to_owned(), entity.to_owned());
        r.insert("shortfall".to_owned(), shortfall.to_owned());
        r
    }

    #[test]
    fn a_positive_shortfall_row_is_a_drift_verdict() {
        let rule = shortfall_rule();
        let verdicts = rule.verdicts(&[row("https://ekg.example/id/sale.order/42", "13")]);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(
            verdicts[0].subject.0,
            "https://ekg.example/id/sale.order/42"
        );
        assert!(verdicts[0].reason.contains("13"));
    }

    #[test]
    fn a_covered_row_yields_no_verdict() {
        let rule = shortfall_rule();
        assert!(
            rule.verdicts(&[row("https://ekg.example/id/sale.order/7", "-4")])
                .is_empty()
        );
    }

    /// A demo coupling rule: a row with `x` and `y` bindings couples the two.
    fn pair_rule() -> CouplingRule {
        CouplingRule::new("SELECT ?x ?y WHERE { ... }", |row| {
            CouplingEdge::new(
                EntityUri(row.get("x")?.clone()),
                EntityUri(row.get("y")?.clone()),
                "shared order",
            )
        })
    }

    fn pair(x: &str, y: &str) -> Row {
        let mut r = Row::new();
        r.insert("x".to_owned(), x.to_owned());
        r.insert("y".to_owned(), y.to_owned());
        r
    }

    #[test]
    fn an_interpreted_row_yields_an_undirected_edge() {
        let rule = pair_rule();
        let forward = rule.edges(&[pair("urn:b", "urn:a")]);
        let backward = rule.edges(&[pair("urn:a", "urn:b")]);
        assert_eq!(forward.len(), 1);
        assert_eq!(forward, backward, "the edge is undirected");
        assert_eq!(forward[0].a.0, "urn:a");
        assert_eq!(forward[0].b.0, "urn:b");
        assert_eq!(forward[0].reason, "shared order");
    }

    #[test]
    fn a_malformed_or_self_row_yields_no_edge() {
        let rule = pair_rule();
        let mut missing = Row::new();
        missing.insert("x".to_owned(), "urn:a".to_owned());
        assert!(rule.edges(&[missing]).is_empty(), "a missing binding");
        assert!(
            rule.edges(&[pair("urn:a", "urn:a")]).is_empty(),
            "a self-loop"
        );
    }
}
