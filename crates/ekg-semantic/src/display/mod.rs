//! The supply-risk capability's display framing.
//!
//! The read surface serves a plan or option's oversight framing as opaque keys, written into
//! the same [`ekg_domain::Score`] `metrics` bag the scorer fills. Every judgment behind those
//! keys lives here with the capability rule; the neutral base and the GraphQL layer only
//! call this module.
//!
//! Apart from the rank inputs the scorers call ([`plan_cost_key`], the delivery judgment),
//! nothing here scores or ranks: every key is display-only, so a derived value never changes
//! the `rank_key`. A value that cannot be read is omitted.

mod change;
mod coupling;
mod delivery;
mod headline;
mod labels;
pub mod money;
pub mod queries;
mod scope;
mod wording;

pub use change::change_from;
pub use coupling::{
    COUPLING_CAPACITY_KEY, COUPLING_CONTENTION, COUPLING_FULFILLS, COUPLING_KIND_KEY,
    COUPLING_VIA_KEY, describe_coupling,
};
pub(crate) use delivery::lateness;
pub use delivery::{
    Delivery, Outcome, WorldSource, order_framing, order_of, order_value, outcome, plan_cost,
    plan_cost_key, plan_framing,
};
pub use headline::{Awaiting, headline_metrics};
pub use labels::{change_label, entity_labels};
pub use scope::{
    SCOPE_KINDS, SCOPE_ORDER, SCOPE_PURCHASE_ORDER, SCOPE_WORK_CENTER, ScopeCount, lead_member,
    plan_scope, scope_from_rows,
};
pub use wording::{RESIDUAL_OVERLAP_KEY, RESIDUAL_SHORTFALL_KEY, describe_score};

/// The display key of a plan's delivery outcome (`on_time` / `late` / `at_risk`).
pub const DELIVERY_KEY: &str = "delivery";
/// The display key of a plan's projected completion date (ISO).
pub const PROJECTED_COMPLETION_KEY: &str = "projected_completion";
/// The display key of the whole-day slip against the original promise.
pub const DAY_SLIP_KEY: &str = "day_slip";
/// The display key of the order's value.
pub const ORDER_VALUE_KEY: &str = "order_value";
/// The display key of the currency an order value or revenue is in (ISO 4217 code).
pub const CURRENCY_KEY: &str = "currency";
/// The display key of a plan's cost.
pub const COST_KEY: &str = "cost";
/// The display key of the currency a plan's cost is in.
pub const COST_CURRENCY_KEY: &str = "cost_currency";
/// The headline key of the orders late if nothing is done.
pub const LATE_ORDERS_NOW_KEY: &str = "late_orders_now";
/// The headline key of the orders late if the recommended plans are approved; also the
/// scorer's per-projection count of orders late by demand.
pub const LATE_ORDERS_KEY: &str = "late_orders";
/// The display key for the units a plan leaves late: the quantity of an order's manufacturing
/// lots completing after its promise, summed over orders.
pub const LATE_UNITS_KEY: &str = "late_units";
/// The headline key of the revenue at risk.
pub const REVENUE_AT_RISK_KEY: &str = "revenue_at_risk";

use std::collections::BTreeMap;

/// The metric key the scorer fills with the count of residual risk anchors (at-risk
/// order/component rows, or over-capacity work-order anchors). The severity tier is derived
/// from it. Shared with `crate::scorer`.
pub const RISK_LEFT_KEY: &str = "risk_left";

/// The display key the derived severity tier is written under.
pub const SEVERITY_KEY: &str = "severity";

/// At or above this many residual risk anchors the situation is [`SEVERITY_CRITICAL`]:
/// several components short, or several anchors over capacity, is a broad miss that is hard
/// to recover.
const CRITICAL_AT: u64 = 2;

/// The severity tiers (Critical / High / Watch / On track). Opaque strings to the neutral
/// contract; the view maps them to fills.
pub const SEVERITY_CRITICAL: &str = "critical";
/// See [`SEVERITY_CRITICAL`].
pub const SEVERITY_HIGH: &str = "high";
/// See [`SEVERITY_CRITICAL`]. Not emitted by the count-based threshold.
pub const SEVERITY_WATCH: &str = "watch";
/// See [`SEVERITY_CRITICAL`].
pub const SEVERITY_ON_TRACK: &str = "on_track";

/// Derives the severity tier from the residual-risk count the scorer wrote
/// ([`RISK_LEFT_KEY`]): no residual is on track, a single residual anchor is High (one
/// promise missed), [`CRITICAL_AT`] or more is Critical. `None` when the score carries no
/// risk count, so a capability that fills no such key gets no severity.
#[must_use]
pub fn severity_tier(metrics: &BTreeMap<String, String>) -> Option<&'static str> {
    let risk_left: u64 = metrics.get(RISK_LEFT_KEY)?.parse().ok()?;
    Some(match risk_left {
        0 => SEVERITY_ON_TRACK,
        n if n >= CRITICAL_AT => SEVERITY_CRITICAL,
        _ => SEVERITY_HIGH,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(risk_left: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(RISK_LEFT_KEY.to_owned(), risk_left.to_owned())])
    }

    #[test]
    fn severity_ladders_from_the_residual_risk_count() {
        assert_eq!(severity_tier(&metrics("0")), Some(SEVERITY_ON_TRACK));
        assert_eq!(severity_tier(&metrics("1")), Some(SEVERITY_HIGH));
        assert_eq!(severity_tier(&metrics("2")), Some(SEVERITY_CRITICAL));
        assert_eq!(severity_tier(&metrics("7")), Some(SEVERITY_CRITICAL));
    }

    #[test]
    fn a_score_without_a_risk_count_gets_no_fabricated_tier() {
        assert_eq!(severity_tier(&BTreeMap::new()), None);
        // A non-numeric value is not a count, so no tier.
        assert_eq!(severity_tier(&metrics("lots")), None);
    }
}
