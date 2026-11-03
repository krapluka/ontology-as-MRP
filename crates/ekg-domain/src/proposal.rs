//! Proposals: the bounded agent's output. AI proposes; it never commits.
//!
//! Only the shape of a proposed action lives here. The concrete governed verbs
//! (re-promise, expedite PO, ...) belong to the kinetic layer (`ekg-kinetic`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::grounding::{EvidenceEdge, Grounding, Omission};
use crate::ids::EntityUri;

/// A proposal paired with the omissions its grounding recorded, so the attach
/// from `open` to `proposed` can enforce disclosure itself instead of trusting the
/// proposal's own `omissions` field. The bounded agent returns this;
/// `grounding_omissions` is the source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundedProposal {
    /// The grounded proposal. A candidate: it commits nothing.
    pub proposal: Proposal,
    /// The omissions recorded by the grounding the proposal was built from.
    pub grounding_omissions: Vec<Omission>,
    /// The evidence edges of the grounding this proposal was built from, so a
    /// reasoning trace can name the governed facts the reasoning rested on. Carried
    /// here instead of re-read from the world model, which keeps the provenance and
    /// revision the proposal was built at. Empty when the grounding had no visible
    /// edge.
    ///
    /// Defaulted, so a `GroundedProposal` encoded without this field still
    /// deserializes.
    #[serde(default)]
    pub grounding_edges: Vec<EvidenceEdge>,
    /// The entities the subject's grounding references (the objects of its evidence
    /// edges). The attach gate allows a proposal to target the action point's
    /// subject or one of these members, so a point about an order may be remediated
    /// by an action on one of its work orders. A target the subject's evidence
    /// never referenced is refused.
    pub subject_members: Vec<EntityUri>,
}

/// A governed-action type identifier, e.g. `"re_promise_date"`.
///
/// Opaque here; the kinetic layer defines the concrete set, their parameters,
/// their inverses, and their reversibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionType(pub String);

/// A concrete proposed action against a target entity in a system of record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedAction {
    /// The governed verb (defined by the kinetic layer).
    pub action_type: ActionType,
    /// The entity the action targets.
    pub target: EntityUri,
    /// Action parameters (e.g. the new date), string-typed in the neutral core.
    pub params: BTreeMap<String, String>,
}

/// A neutral aggregate of N proposed governed actions to be promoted and applied
/// as one governed step.
///
/// A plan groups opaque [`ProposedAction`]s and names no verb, so a supply-risk
/// plan and a manufacturing-process plan are the same shape. The actions apply in
/// listed order; choosing a coherent order is up to whoever builds the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The proposed actions, committed (and reversed) in listed order.
    pub actions: Vec<ProposedAction>,
}

/// The bounded agent's output for an action point: a grounded explanation, a
/// bounded plan of one or more proposed governed actions, and the omissions it
/// must disclose. The language model never writes to the world model or a system of
/// record.
///
/// One explanation and one set of disclosed omissions cover all N actions: the
/// grounded reasoning is over the whole coordinated plan. A single-action proposal
/// is a 1-element `actions`, the common case.
///
/// Deserialized through [`ProposalWire`] (`#[serde(from)]`) so a persisted proposal
/// with a singular `"action"` still loads as a 1-element `actions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ProposalWire")]
pub struct Proposal {
    /// A grounded, provenance-bearing natural-language explanation.
    pub explanation: String,
    /// The proposed governed actions (proposals, not commits), promoted as one step.
    pub actions: Vec<ProposedAction>,
    /// Omissions carried structurally (not parsed from prose). A proposal that
    /// fails to reflect its grounding's omissions is rejected at promotion.
    pub omissions: Vec<Omission>,
}

/// The wire form [`Proposal`] deserializes through: accepts either `actions: [...]`
/// or a legacy singular `action`, mapping the latter to a 1-element `actions`, so an
/// [`crate::ActionPoint`] persisted with `"action"` still loads instead of stranding
/// a `proposed` point. A present `actions` wins over a legacy `action`.
#[derive(Deserialize)]
struct ProposalWire {
    explanation: String,
    #[serde(default)]
    actions: Vec<ProposedAction>,
    #[serde(default)]
    action: Option<ProposedAction>,
    #[serde(default)]
    omissions: Vec<Omission>,
}

impl From<ProposalWire> for Proposal {
    fn from(wire: ProposalWire) -> Self {
        let mut actions = wire.actions;
        // A legacy singular `action` becomes the sole action only when no `actions`
        // array was present, so a payload carrying both is not doubled.
        if actions.is_empty()
            && let Some(single) = wire.action
        {
            actions.push(single);
        }
        Self {
            explanation: wire.explanation,
            actions,
            omissions: wire.omissions,
        }
    }
}

impl Proposal {
    /// Build a single-action proposal, the common case, as a 1-element plan.
    #[must_use]
    pub fn single(explanation: String, action: ProposedAction, omissions: Vec<Omission>) -> Self {
        Self {
            explanation,
            actions: vec![action],
            omissions,
        }
    }

    /// Whether this proposal faithfully discloses every omission in the grounding
    /// it was built from. Used at the promote gate to reject a proposal that
    /// would be silently shown as complete.
    #[must_use]
    pub fn discloses_omissions_of(&self, grounding: &Grounding) -> bool {
        grounding
            .omissions
            .iter()
            .all(|o| self.omissions.contains(o))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn action(verb: &str) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            params: BTreeMap::new(),
        }
    }

    #[test]
    fn a_legacy_singular_action_deserializes_to_a_one_element_plan() {
        // A `Proposal` persisted with a singular `"action"` must still load, else a
        // `proposed` action point would be stranded.
        let legacy = r#"{
            "explanation": "expedite the battery PO",
            "action": {
                "action_type": "expedite_po",
                "target": "https://ekg.example/id/sale.order/1",
                "params": {}
            },
            "omissions": []
        }"#;
        let proposal: Proposal = serde_json::from_str(legacy).expect("legacy proposal loads");
        assert_eq!(proposal.actions.len(), 1);
        assert_eq!(proposal.actions[0].action_type.0, "expedite_po");
        assert!(proposal.omissions.is_empty());
    }

    #[test]
    fn a_current_actions_array_round_trips() {
        let proposal = Proposal {
            explanation: "coordinated remediation".to_owned(),
            actions: vec![action("expedite_po"), action("re_promise_date")],
            omissions: Vec::new(),
        };
        let json = serde_json::to_string(&proposal).expect("serialize");
        // The serialized form carries `actions`, not a singular `action`.
        assert!(json.contains("\"actions\""));
        assert!(!json.contains("\"action\":"));
        let back: Proposal = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back, proposal);
        assert_eq!(back.actions.len(), 2);
    }

    #[test]
    fn a_present_actions_array_is_not_doubled_by_a_stray_legacy_action() {
        // If both keys appear, the array wins and the singular is ignored.
        let both = r#"{
            "explanation": "e",
            "actions": [{"action_type":"a","target":"https://ekg.example/id/sale.order/1","params":{}}],
            "action": {"action_type":"b","target":"https://ekg.example/id/sale.order/1","params":{}},
            "omissions": []
        }"#;
        let proposal: Proposal = serde_json::from_str(both).expect("loads");
        assert_eq!(proposal.actions.len(), 1);
        assert_eq!(proposal.actions[0].action_type.0, "a");
    }
}
