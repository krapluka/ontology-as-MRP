//! The per-plan rules every plan path shares: the bounded agents' free-text path
//! ([`crate::agent`]) and the gateway's `submit_plan` tool ([`crate::tools::submit`]) bound a
//! plan the same way, so the two can never drift apart.
//!
//! A plan is non-empty. Duplicate actions (same verb, target and params) are dropped,
//! preserving order, because committing the identical governed action twice in one plan would
//! be a double write for a non-idempotent verb. The de-duplicated plan is rejected, never
//! truncated, when it is over the per-plan bound, so a plan never exceeds what one oversight
//! decision reviews (EU AI Act Art. 14).

use std::fmt;

use ekg_domain::ProposedAction;

/// Why a plan is out of bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanDefect {
    /// The plan has no action.
    Empty,
    /// After removing duplicates, the plan has more actions than the bound.
    OverBound {
        /// The de-duplicated plan's length.
        actions: usize,
        /// The per-plan bound.
        max: usize,
    },
}

impl fmt::Display for PlanDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the plan has no actions"),
            Self::OverBound { actions, max } => write!(
                f,
                "the plan has {actions} distinct actions, over the per-plan maximum of {max}"
            ),
        }
    }
}

/// `actions` with duplicates removed, in their first-seen order.
pub(crate) fn dedup(actions: Vec<ProposedAction>) -> Vec<ProposedAction> {
    let mut deduped: Vec<ProposedAction> = Vec::with_capacity(actions.len());
    for action in actions {
        if !deduped.contains(&action) {
            deduped.push(action);
        }
    }
    deduped
}

/// Turn `actions` into a bounded plan: refused when empty, duplicates dropped, and refused
/// when the de-duplicated plan has more than `max_actions` actions.
///
/// # Errors
/// [`PlanDefect::Empty`] or [`PlanDefect::OverBound`].
pub(crate) fn bounded_plan(
    actions: Vec<ProposedAction>,
    max_actions: usize,
) -> Result<Vec<ProposedAction>, PlanDefect> {
    if actions.is_empty() {
        return Err(PlanDefect::Empty);
    }
    let deduped = dedup(actions);
    if deduped.len() > max_actions {
        return Err(PlanDefect::OverBound {
            actions: deduped.len(),
            max: max_actions,
        });
    }
    Ok(deduped)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ekg_domain::{ActionType, EntityUri};

    use super::*;

    fn action(verb: &str, id: u32) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(verb.to_owned()),
            target: EntityUri(format!("https://ekg.example/id/sale.order/{id}")),
            params: BTreeMap::new(),
        }
    }

    #[test]
    fn an_empty_plan_is_refused() {
        assert_eq!(bounded_plan(Vec::new(), 4), Err(PlanDefect::Empty));
    }

    #[test]
    fn exact_duplicates_are_dropped_in_order_before_the_bound_applies() {
        let plan = vec![
            action("a", 1),
            action("b", 1),
            action("a", 1),
            action("a", 2),
        ];
        assert_eq!(
            bounded_plan(plan, 3),
            Ok(vec![action("a", 1), action("b", 1), action("a", 2)])
        );
    }

    #[test]
    fn an_over_bound_plan_is_refused_not_truncated() {
        let plan = vec![action("a", 1), action("a", 2), action("a", 3)];
        assert_eq!(
            bounded_plan(plan, 2),
            Err(PlanDefect::OverBound { actions: 3, max: 2 })
        );
    }
}
