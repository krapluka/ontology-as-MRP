//! The promotion policy.
//!
//! Given a plan of [`ProposedAction`]s, decide whether a human must promote it (the
//! default) or a human-authored policy has pre-authorized automatic promotion. A plan
//! auto-promotes only when every action passes every predicate; otherwise the decision is
//! human-gated and carries one structured [`Need`] per failed predicate, so oversight
//! sees why it waits (EU AI Act Art. 14) and the reasons are audited (Art. 12).
//!
//! The engine and the [`PromotionPredicate`] trait name no metric: the engine composes
//! an injected set of predicates, and the allowlist, cost limit and confirmed-state set
//! are configuration. The concrete predicates (cost, promised date, supplier) are
//! capability logic and live in this crate beside the concrete verbs. Every predicate
//! is a pure function of the action and a pre-gathered [`PolicyContext`]; the caller
//! (the capability gatherer, wired in the backend) does the world reads that fill it,
//! so the engine is synchronous and testable without a store.
//!
//! An action auto-promotes only if its verb is reversible, even when it is allowlisted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use ekg_domain::{EntityUri, Need, Plan, ProposedAction};

use crate::verbs::Verb;

/// Whether a proposed action needs meaningful human oversight or may be promoted
/// automatically under a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionTier {
    /// The default: a human must promote (the governed write-back waits for oversight).
    HumanGated,
    /// A human-authored policy pre-authorized automatic promotion of this action.
    AutoPromote,
}

/// Per-entity attributes a predicate may consult, gathered by the capability from the
/// world model before the policy decision. Like the `metrics` of [`ekg_domain::Score`],
/// the engine treats every value as opaque; a concrete predicate reads the keys it
/// defines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyContext {
    attributes: HashMap<EntityUri, BTreeMap<String, String>>,
}

impl PolicyContext {
    /// An empty context (every attribute absent).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an opaque attribute for `entity` (builder-style, chainable).
    #[must_use]
    pub fn with(
        mut self,
        entity: EntityUri,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.set(entity, key, value);
        self
    }

    /// Record an opaque attribute for `entity`.
    pub fn set(&mut self, entity: EntityUri, key: impl Into<String>, value: impl Into<String>) {
        self.attributes
            .entry(entity)
            .or_default()
            .insert(key.into(), value.into());
    }

    /// The opaque value of `key` for `entity`, if gathered.
    #[must_use]
    pub fn get(&self, entity: &EntityUri, key: &str) -> Option<&str> {
        self.attributes
            .get(entity)
            .and_then(|m| m.get(key))
            .map(String::as_str)
    }
}

/// Given an action and the gathered [`PolicyContext`], return a [`Need`] when the action
/// fails this predicate, or `None` when it passes. The trait names no metric; a concrete
/// predicate is capability logic.
pub trait PromotionPredicate: Send + Sync {
    /// Evaluate the predicate for one action: `None` when it passes, `Some(need)` with
    /// the structured reason when it fails.
    fn check(&self, action: &ProposedAction, ctx: &PolicyContext) -> Option<Need>;
}

/// The promotion decision: the tier, plus one structured [`Need`] per failed
/// predicate (empty when the plan auto-promotes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionDecision {
    /// Auto-promote iff every action passed every predicate.
    pub tier: PromotionTier,
    /// The reasons the plan is human-gated (empty on `AutoPromote`).
    pub needs: Vec<Need>,
}

/// A deterministic, config-driven promotion policy. Carries a policy id/version for
/// provenance (recorded as the promoter of an auto-promotion), the allowlist of
/// auto-promotable action types, and the injected capability predicates, all of which
/// must pass.
#[derive(Clone)]
pub struct PolicyEngine {
    policy_id: String,
    auto: BTreeSet<String>,
    predicates: Vec<Arc<dyn PromotionPredicate>>,
}

impl PolicyEngine {
    /// Build a policy from its id/version and the allowlist of auto-promotable action
    /// types, with no extra predicates (only the built-in allowlist and reversibility
    /// gates). An empty allowlist means everything is human-gated.
    #[must_use]
    pub fn new(policy_id: impl Into<String>, auto: BTreeSet<String>) -> Self {
        Self {
            policy_id: policy_id.into(),
            auto,
            predicates: Vec::new(),
        }
    }

    /// Build a policy with injected capability predicates (cost, promised date,
    /// supplier), checked in addition to the built-in allowlist and reversibility gates.
    #[must_use]
    pub fn with_predicates(
        policy_id: impl Into<String>,
        auto: BTreeSet<String>,
        predicates: Vec<Arc<dyn PromotionPredicate>>,
    ) -> Self {
        Self {
            policy_id: policy_id.into(),
            auto,
            predicates,
        }
    }

    /// The policy id/version, recorded as the promoter of an auto-promotion so the
    /// audit trail attributes it to the policy, not a person (EU AI Act Art. 12).
    #[must_use]
    pub fn policy_id(&self) -> &str {
        &self.policy_id
    }

    /// Evaluate one action against the built-in gates and the injected predicates,
    /// collecting a [`Need`] per failure.
    fn needs_for(&self, action: &ProposedAction, ctx: &PolicyContext) -> Vec<Need> {
        let mut needs = Vec::new();
        if !self.auto.contains(&action.action_type.0) {
            needs.push(Need {
                code: "not_allowlisted".to_owned(),
                detail: format!(
                    "action type {:?} is not in the auto-promotion allowlist",
                    action.action_type.0
                ),
            });
        }
        // A non-reversible (or unknown) verb can never auto-promote.
        if !Verb::parse(&action.action_type).is_ok_and(Verb::reversible) {
            needs.push(Need {
                code: "not_reversible".to_owned(),
                detail: format!(
                    "action type {:?} is not a known single-step-reversible verb",
                    action.action_type.0
                ),
            });
        }
        for predicate in &self.predicates {
            if let Some(need) = predicate.check(action, ctx) {
                needs.push(need);
            }
        }
        needs
    }

    /// Decide the promotion for a whole `plan`: `AutoPromote` iff every action passes
    /// every predicate; otherwise `HumanGated` with one [`Need`] per failure across the
    /// plan. `ctx` carries the capability-gathered attributes the predicates consult.
    #[must_use]
    pub fn evaluate(&self, plan: &Plan, ctx: &PolicyContext) -> PromotionDecision {
        let mut needs = Vec::new();
        for action in &plan.actions {
            needs.extend(self.needs_for(action, ctx));
        }
        let tier = if needs.is_empty() {
            PromotionTier::AutoPromote
        } else {
            PromotionTier::HumanGated
        };
        PromotionDecision { tier, needs }
    }

    /// Decide the promotion tier for a single `action` given `ctx`: [`Self::evaluate`]
    /// for a one-action plan, without the needs.
    #[must_use]
    pub fn tier(&self, action: &ProposedAction, ctx: &PolicyContext) -> PromotionTier {
        if self.needs_for(action, ctx).is_empty() {
            PromotionTier::AutoPromote
        } else {
            PromotionTier::HumanGated
        }
    }
}

// --- concrete capability predicates ---
//
// These name cost, promised-date and supplier concepts, so they are capability logic and
// live in this crate beside the concrete verbs, never in the neutral core. Each is pure
// over the action and the pre-gathered `PolicyContext`.

/// The context attribute key holding an entity's summed order-line cost (a decimal string),
/// gathered by the capability from the world model.
pub const ATTR_COST: &str = "cost";
/// The context attribute key holding a purchase order's materialized lifecycle state.
pub const ATTR_PO_STATE: &str = "po_state";

/// Fails when an action moves a customer promised date, i.e. its verb writes
/// `sale.order.commitment_date` (`re_promise_date`). Pure over the action.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPromisedDateMove;

impl PromotionPredicate for NoPromisedDateMove {
    fn check(&self, action: &ProposedAction, _ctx: &PolicyContext) -> Option<Need> {
        let moves_promise = Verb::parse(&action.action_type)
            .ok()
            .and_then(Verb::field_write)
            == Some(("sale.order", "commitment_date"));
        moves_promise.then(|| Need {
            code: "promised_date_move".to_owned(),
            detail: "moves a customer promised date (sale.order.commitment_date)".to_owned(),
        })
    }
}

/// Fails any action that changes the plant's structure: a split of a manufacturing
/// order or a raise of a work center's capacity. Such a plan always waits for a human,
/// even when both verbs are allowlisted. Pure over the action.
#[derive(Debug, Clone, Copy, Default)]
pub struct StructuralChange;

impl PromotionPredicate for StructuralChange {
    fn check(&self, action: &ProposedAction, _ctx: &PolicyContext) -> Option<Need> {
        let detail = match Verb::parse(&action.action_type).ok()? {
            Verb::SplitProduction => "splits a manufacturing order",
            Verb::RaiseWorkCenterCapacity => "raises a work center's capacity",
            Verb::RePromiseDate
            | Verb::ExpeditePo
            | Verb::NotifyOwner
            | Verb::RescheduleWorkOrder
            | Verb::ReassignWorkCenter => return None,
        };
        Some(Need {
            code: "structural_change".to_owned(),
            detail: detail.to_owned(),
        })
    }
}

/// Fails when the cost gathered for the action's target exceeds the configured limit. A
/// target with no gathered cost (e.g. a work-order reschedule) passes: it carries no cost.
/// When no limit is configured the predicate fails any action that carries a cost
/// (fail-closed: a cost-bearing plan is never auto-promoted without an explicit limit).
#[derive(Debug, Clone, Copy)]
pub struct CostWithinLimit {
    limit: Option<f64>,
}

impl CostWithinLimit {
    /// A predicate gating on `limit` (in the same currency unit the gatherer records), or
    /// fail-closed when `None`.
    #[must_use]
    pub fn new(limit: Option<f64>) -> Self {
        Self { limit }
    }
}

impl PromotionPredicate for CostWithinLimit {
    fn check(&self, action: &ProposedAction, ctx: &PolicyContext) -> Option<Need> {
        // No gathered cost means the target carries none (e.g. a non-PO action), so it
        // passes. A cost that is present but unparseable fails closed.
        let raw = ctx.get(&action.target, ATTR_COST)?;
        let Ok(cost) = raw.parse::<f64>() else {
            // A distinct code: the cost was not over the limit, it could not be read. An
            // oversight chip can show that reason without parsing the detail text.
            return Some(Need {
                code: "cost_unreadable".to_owned(),
                detail: format!("cost {raw:?} could not be interpreted"),
            });
        };
        match self.limit {
            Some(limit) if cost <= limit => None,
            Some(limit) => Some(Need {
                code: "cost_over_limit".to_owned(),
                detail: format!("cost {cost:.2} exceeds the limit {limit:.2}"),
            }),
            None => Some(Need {
                code: "cost_over_limit".to_owned(),
                detail: format!("cost {cost:.2} but no auto-apply cost limit is configured"),
            }),
        }
    }
}

/// Fails when the purchase order the action touches is not in a confirmed lifecycle state.
/// An action touching no purchase order (no `po_state` gathered) passes vacuously. Reads a
/// built-in source state; a demonstrator stand-in for supplier confirmation, not a distinct
/// vendor acknowledgment.
#[derive(Debug, Clone)]
pub struct SupplierConfirmed {
    confirmed_states: BTreeSet<String>,
}

impl SupplierConfirmed {
    /// A predicate treating any of `confirmed_states` as a confirmed purchase order.
    #[must_use]
    pub fn new(confirmed_states: BTreeSet<String>) -> Self {
        Self { confirmed_states }
    }
}

impl PromotionPredicate for SupplierConfirmed {
    fn check(&self, action: &ProposedAction, ctx: &PolicyContext) -> Option<Need> {
        let state = ctx.get(&action.target, ATTR_PO_STATE)?;
        (!self.confirmed_states.contains(state)).then(|| Need {
            code: "supplier_unconfirmed".to_owned(),
            detail: format!("purchase order is not in a confirmed state (state: {state:?})"),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;

    use ekg_domain::{ActionType, EntityUri};

    use super::*;

    fn action(action_type: &str, target: &str) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(action_type.to_owned()),
            target: EntityUri(target.to_owned()),
            params: BTreeMap::new(),
        }
    }

    fn plan(actions: Vec<ProposedAction>) -> Plan {
        Plan { actions }
    }

    fn allowlisting(types: &[&str]) -> PolicyEngine {
        PolicyEngine::new(
            "test-policy/v1",
            types.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    const SUBJECT: &str = "https://ekg.example/id/purchase.order/1";

    #[test]
    fn an_empty_policy_gates_every_action_on_a_human() {
        let policy = PolicyEngine::new("test-policy/v1", BTreeSet::new());
        let decision = policy.evaluate(
            &plan(vec![action("notify_owner", SUBJECT)]),
            &PolicyContext::new(),
        );
        assert_eq!(decision.tier, PromotionTier::HumanGated);
        assert!(decision.needs.iter().any(|n| n.code == "not_allowlisted"));
        assert_eq!(policy.policy_id(), "test-policy/v1");
    }

    #[test]
    fn an_allowlisted_reversible_verb_auto_promotes_with_no_needs() {
        let policy = allowlisting(&["notify_owner"]);
        let decision = policy.evaluate(
            &plan(vec![action("notify_owner", SUBJECT)]),
            &PolicyContext::new(),
        );
        assert_eq!(decision.tier, PromotionTier::AutoPromote);
        assert!(decision.needs.is_empty());
    }

    #[test]
    fn an_unknown_verb_never_auto_promotes_even_if_allowlisted() {
        let policy = allowlisting(&["delete_everything"]);
        let decision = policy.evaluate(
            &plan(vec![action("delete_everything", SUBJECT)]),
            &PolicyContext::new(),
        );
        assert_eq!(decision.tier, PromotionTier::HumanGated);
        assert!(decision.needs.iter().any(|n| n.code == "not_reversible"));
    }

    #[test]
    fn a_plan_auto_promotes_only_when_every_action_passes() {
        // One allowlisted action and one that is not: gated, with the reason attributed
        // to the failing action.
        let policy = allowlisting(&["notify_owner"]);
        let decision = policy.evaluate(
            &plan(vec![
                action("notify_owner", SUBJECT),
                action("re_promise_date", "https://ekg.example/id/sale.order/9"),
            ]),
            &PolicyContext::new(),
        );
        assert_eq!(decision.tier, PromotionTier::HumanGated);
        assert!(decision.needs.iter().any(|n| n.code == "not_allowlisted"));
    }

    #[test]
    fn no_promised_date_move_predicate_flags_re_promise_date() {
        let pred = NoPromisedDateMove;
        assert!(
            pred.check(
                &action("re_promise_date", "https://ekg.example/id/sale.order/9"),
                &PolicyContext::new()
            )
            .is_some(),
            "re_promise_date moves the customer promised date"
        );
        assert!(
            pred.check(
                &action(
                    "reschedule_work_order",
                    "https://ekg.example/id/mrp.workorder/2"
                ),
                &PolicyContext::new()
            )
            .is_none(),
            "a work-order reschedule does not move a customer promise"
        );
    }

    #[test]
    fn cost_predicate_flags_over_limit_and_passes_when_absent() {
        let pred = CostWithinLimit::new(Some(250.0));
        let over = PolicyContext::new().with(EntityUri(SUBJECT.to_owned()), ATTR_COST, "320.00");
        let need = pred
            .check(&action("expedite_po", SUBJECT), &over)
            .expect("over limit");
        assert_eq!(need.code, "cost_over_limit");
        assert!(need.detail.contains("320.00") && need.detail.contains("250.00"));
        // Under the limit passes; a target with no gathered cost passes.
        let under = PolicyContext::new().with(EntityUri(SUBJECT.to_owned()), ATTR_COST, "60.00");
        assert!(
            pred.check(&action("expedite_po", SUBJECT), &under)
                .is_none()
        );
        assert!(
            pred.check(&action("expedite_po", SUBJECT), &PolicyContext::new())
                .is_none()
        );
    }

    #[test]
    fn cost_predicate_fails_closed_on_an_unparseable_cost() {
        // A present but unparseable cost must gate (never pass), under a distinct code so
        // oversight sees the reason.
        let pred = CostWithinLimit::new(Some(250.0));
        let bad =
            PolicyContext::new().with(EntityUri(SUBJECT.to_owned()), ATTR_COST, "not-a-number");
        let need = pred
            .check(&action("expedite_po", SUBJECT), &bad)
            .expect("an unparseable cost fails closed");
        assert_eq!(need.code, "cost_unreadable");
    }

    #[test]
    fn supplier_predicate_flags_unconfirmed_and_passes_when_no_po() {
        let pred = SupplierConfirmed::new(["purchase".to_owned(), "done".to_owned()].into());
        let confirmed =
            PolicyContext::new().with(EntityUri(SUBJECT.to_owned()), ATTR_PO_STATE, "purchase");
        assert!(
            pred.check(&action("expedite_po", SUBJECT), &confirmed)
                .is_none()
        );
        let draft =
            PolicyContext::new().with(EntityUri(SUBJECT.to_owned()), ATTR_PO_STATE, "draft");
        let need = pred
            .check(&action("expedite_po", SUBJECT), &draft)
            .expect("unconfirmed");
        assert_eq!(need.code, "supplier_unconfirmed");
        // No PO state gathered (e.g. a reschedule): passes vacuously.
        assert!(
            pred.check(
                &action("reschedule_work_order", SUBJECT),
                &PolicyContext::new()
            )
            .is_none()
        );
    }

    #[test]
    fn the_widened_engine_collects_one_need_per_failed_predicate() {
        let policy = PolicyEngine::with_predicates(
            "test-policy/v1",
            ["expedite_po".to_owned()].into(),
            vec![
                Arc::new(NoPromisedDateMove),
                Arc::new(CostWithinLimit::new(Some(250.0))),
                Arc::new(SupplierConfirmed::new(["purchase".to_owned()].into())),
            ],
        );
        // An allowlisted, reversible expedite whose PO is over the limit and unconfirmed:
        // two needs (cost and supplier), no allowlist, reversibility or date needs.
        let ctx = PolicyContext::new()
            .with(EntityUri(SUBJECT.to_owned()), ATTR_COST, "320.00")
            .with(EntityUri(SUBJECT.to_owned()), ATTR_PO_STATE, "draft");
        let decision = policy.evaluate(&plan(vec![action("expedite_po", SUBJECT)]), &ctx);
        assert_eq!(decision.tier, PromotionTier::HumanGated);
        let codes: BTreeSet<&str> = decision.needs.iter().map(|n| n.code.as_str()).collect();
        assert_eq!(
            codes,
            BTreeSet::from(["cost_over_limit", "supplier_unconfirmed"])
        );

        // A compliant expedite (under limit, confirmed) auto-promotes with no needs.
        let ok = PolicyContext::new()
            .with(EntityUri(SUBJECT.to_owned()), ATTR_COST, "60.00")
            .with(EntityUri(SUBJECT.to_owned()), ATTR_PO_STATE, "purchase");
        let decision = policy.evaluate(&plan(vec![action("expedite_po", SUBJECT)]), &ok);
        assert_eq!(decision.tier, PromotionTier::AutoPromote);
        assert!(decision.needs.is_empty());
    }

    #[test]
    fn a_structural_change_is_human_gated_even_when_allowlisted() {
        let engine = PolicyEngine::with_predicates(
            "p/v1",
            [
                "split_production",
                "raise_work_center_capacity",
                "reschedule_work_order",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            vec![Arc::new(StructuralChange)],
        );
        let ctx = PolicyContext::new();
        for (verb, detail) in [
            ("split_production", "splits a manufacturing order"),
            (
                "raise_work_center_capacity",
                "raises a work center's capacity",
            ),
        ] {
            let decision = engine.evaluate(
                &plan(vec![
                    action(
                        "reschedule_work_order",
                        "https://ekg.example/id/mrp.workorder/1",
                    ),
                    action(verb, "https://ekg.example/id/mrp.production/1"),
                ]),
                &ctx,
            );
            assert_eq!(decision.tier, PromotionTier::HumanGated, "{verb}");
            assert_eq!(decision.needs.len(), 1, "{verb}: {:?}", decision.needs);
            assert_eq!(decision.needs[0].code, "structural_change");
            assert_eq!(decision.needs[0].detail, detail);
        }
        // A plan without a structural verb is unaffected.
        let existing = engine.evaluate(
            &plan(vec![action(
                "reschedule_work_order",
                "https://ekg.example/id/mrp.workorder/1",
            )]),
            &ctx,
        );
        assert_eq!(existing.tier, PromotionTier::AutoPromote);
        assert!(existing.needs.is_empty());
    }
}
