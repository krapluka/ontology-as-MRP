//! The capability-neutral read-policy.
//!
//! A [`ReadPolicy`] decides, for a caller [`ReadContext`] and a candidate node or edge,
//! whether it is visible, and records what it hid as a structured [`Omission`] so a
//! grounding is never silently incomplete. The concrete roles and sensitive types are
//! data it is constructed from (config for the real model, canned values for the fake).
//!
//! It lives in the neutral core because the real `WorldModel` (`ekg-semantic`) and the
//! in-memory fake ground through the same policy, so the two cannot drift.

use std::collections::{BTreeMap, BTreeSet};

use crate::grounding::{Omission, OmissionKind, ReadContext};
use crate::ids::PolicyId;

/// Whether a candidate is visible to the caller, or hidden (and as which kind of omission,
/// under which reported type IRI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// The caller may see this node or edge.
    Visible,
    /// The caller may not; record it as an omission of `kind` with the given `type_iri`.
    Hidden {
        /// Node or edge.
        kind: OmissionKind,
        /// The reported RDF type or predicate IRI of what was hidden.
        type_iri: String,
    },
}

/// A deterministic, data-driven read-policy: a privileged role sees everything; any other
/// role has the configured sensitive edges and nodes hidden. The mapping is injected
/// (config or a test fake).
#[derive(Debug, Clone)]
pub struct ReadPolicy {
    policy_id: PolicyId,
    privileged_roles: BTreeSet<String>,
    /// Sensitive edge predicate local-name, mapped to the omission `type_iri` to report.
    sensitive_edges: BTreeMap<String, String>,
    /// Sensitive node field local-name, mapped to the omission `type_iri` to report.
    sensitive_nodes: BTreeMap<String, String>,
}

impl ReadPolicy {
    /// Build a policy from its id, the privileged roles, and the sensitive edge and node
    /// maps (each mapping a matched local-name to the omission `type_iri` it reports).
    #[must_use]
    pub fn new(
        policy_id: PolicyId,
        privileged_roles: BTreeSet<String>,
        sensitive_edges: BTreeMap<String, String>,
        sensitive_nodes: BTreeMap<String, String>,
    ) -> Self {
        Self {
            policy_id,
            privileged_roles,
            sensitive_edges,
            sensitive_nodes,
        }
    }

    /// A policy that hides nothing: the library default for a `WorldModel` constructed
    /// without an explicit policy. The running app always injects one from config.
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            policy_id: PolicyId("read-policy/permissive".to_owned()),
            privileged_roles: BTreeSet::new(),
            sensitive_edges: BTreeMap::new(),
            sensitive_nodes: BTreeMap::new(),
        }
    }

    /// The policy id, recorded on every omission it produces.
    #[must_use]
    pub fn policy_id(&self) -> &PolicyId {
        &self.policy_id
    }

    /// Classify one candidate by its predicate or field local name. A sensitive entry is
    /// hidden under its reported `type_iri`; anything else is visible.
    #[must_use]
    pub fn classify(&self, ctx: &ReadContext, local: &str) -> Visibility {
        if self.privileged_roles.contains(&ctx.role) {
            return Visibility::Visible;
        }
        if let Some(type_iri) = self.sensitive_edges.get(local) {
            return Visibility::Hidden {
                kind: OmissionKind::Edge,
                type_iri: type_iri.clone(),
            };
        }
        if let Some(type_iri) = self.sensitive_nodes.get(local) {
            return Visibility::Hidden {
                kind: OmissionKind::Node,
                type_iri: type_iri.clone(),
            };
        }
        Visibility::Visible
    }

    /// Start accumulating omissions for one grounding. Both `WorldModel::ground` impls
    /// use this so they filter and report identically.
    #[must_use]
    pub fn omissions(&self) -> OmissionTally<'_> {
        OmissionTally {
            policy: self,
            hidden: BTreeMap::new(),
        }
    }
}

/// Accumulates hidden node and edge counts while a grounding is built, then renders the
/// structured [`Omission`]s.
pub struct OmissionTally<'p> {
    policy: &'p ReadPolicy,
    /// Keyed by the reported `type_iri`; the value is (kind, count).
    hidden: BTreeMap<String, (OmissionKind, u32)>,
}

impl OmissionTally<'_> {
    /// Classify one candidate: returns `true` when it is visible (the caller keeps the
    /// edge), `false` when it was hidden (recorded here as an omission).
    pub fn visible(&mut self, ctx: &ReadContext, local: &str) -> bool {
        match self.policy.classify(ctx, local) {
            Visibility::Visible => true,
            Visibility::Hidden { kind, type_iri } => {
                let entry = self.hidden.entry(type_iri).or_insert((kind, 0));
                entry.1 += 1;
                false
            }
        }
    }

    /// Render the accumulated omissions, stamped with the policy id and ordered by
    /// `type_iri`.
    #[must_use]
    pub fn into_omissions(self) -> Vec<Omission> {
        self.hidden
            .into_iter()
            .map(|(type_iri, (kind, count))| Omission {
                kind,
                type_iri,
                count,
                policy_id: self.policy.policy_id.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn demo() -> ReadPolicy {
        ReadPolicy::new(
            PolicyId("read-policy/demo-v1".to_owned()),
            BTreeSet::from(["procurement".to_owned()]),
            BTreeMap::from([("unit_price".to_owned(), "ekg:unit_price".to_owned())]),
            BTreeMap::from([("supplier".to_owned(), "ekg:Supplier".to_owned())]),
        )
    }

    fn ctx(role: &str) -> ReadContext {
        ReadContext {
            role: role.to_owned(),
            purpose: "delivery-promise".to_owned(),
        }
    }

    #[test]
    fn a_privileged_role_sees_everything() {
        let p = demo();
        assert_eq!(
            p.classify(&ctx("procurement"), "unit_price"),
            Visibility::Visible
        );
        assert_eq!(
            p.classify(&ctx("procurement"), "supplier"),
            Visibility::Visible
        );
    }

    #[test]
    fn a_non_privileged_role_hides_the_sensitive_edge_and_node() {
        let p = demo();
        assert_eq!(
            p.classify(&ctx("planner"), "unit_price"),
            Visibility::Hidden {
                kind: OmissionKind::Edge,
                type_iri: "ekg:unit_price".to_owned()
            }
        );
        assert_eq!(
            p.classify(&ctx("planner"), "supplier"),
            Visibility::Hidden {
                kind: OmissionKind::Node,
                type_iri: "ekg:Supplier".to_owned()
            }
        );
    }

    #[test]
    fn a_non_sensitive_field_is_visible() {
        assert_eq!(
            demo().classify(&ctx("planner"), "commitment_date"),
            Visibility::Visible
        );
    }

    #[test]
    fn a_permissive_policy_hides_nothing() {
        let p = ReadPolicy::permissive();
        assert_eq!(
            p.classify(&ctx("planner"), "unit_price"),
            Visibility::Visible
        );
        assert_eq!(p.classify(&ctx("planner"), "supplier"), Visibility::Visible);
    }

    #[test]
    fn the_tally_aggregates_counts_kinds_and_the_policy_id() {
        let p = demo();
        let c = ctx("planner");
        let mut om = p.omissions();
        // Two hidden unit_price edges, one hidden supplier node, one visible field.
        assert!(!om.visible(&c, "unit_price"));
        assert!(!om.visible(&c, "unit_price"));
        assert!(!om.visible(&c, "supplier"));
        assert!(om.visible(&c, "commitment_date"));
        let omissions = om.into_omissions();
        assert_eq!(omissions.len(), 2, "one edge omission + one node omission");
        let edge = omissions
            .iter()
            .find(|o| o.kind == OmissionKind::Edge)
            .expect("edge");
        assert_eq!(edge.type_iri, "ekg:unit_price");
        assert_eq!(edge.count, 2);
        assert_eq!(edge.policy_id, PolicyId("read-policy/demo-v1".to_owned()));
        let node = omissions
            .iter()
            .find(|o| o.kind == OmissionKind::Node)
            .expect("node");
        assert_eq!(node.type_iri, "ekg:Supplier");
        assert_eq!(node.count, 1);
    }
}
