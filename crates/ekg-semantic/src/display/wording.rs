//! A score's operator wording: the capability's sentence for the metrics its scorers write, so
//! a simulated trace step reads "2 orders late (15 units) · no shortfall" instead of a
//! `key=value` dump. Only known keys are worded; internal flags (`risk_left`,
//! `signal_cleared`) stay in the score, which the option metrics serve.

use std::collections::BTreeMap;

use super::{LATE_ORDERS_KEY, LATE_UNITS_KEY};

/// The residual shortfall a supply-risk score carries (the point scorer's key).
pub const RESIDUAL_SHORTFALL_KEY: &str = "residual_shortfall";
/// The residual overlap a process score carries (the point scorer's key).
pub const RESIDUAL_OVERLAP_KEY: &str = "residual_overlap";
/// The prefix a portfolio score files the supply-risk scorer's own keys under.
const SUPPLY_PREFIX: &str = "supply_risk.";
/// The prefix a portfolio score files the process scorer's own keys under.
const PROCESS_PREFIX: &str = "process.";
/// A count of nothing, as [`amount`] renders it.
const ZERO: &str = "0";

/// A non-negative count as an operator reads it: `15`, `2.5` (no trailing zeros, three
/// decimals at most). `None` for a value that is not one.
fn amount(value: &str) -> Option<String> {
    let n: f64 = value.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let rendered = format!("{:.3}", n.abs());
    Some(
        rendered
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned(),
    )
}

/// `"1 unit"` / `"15 units"`.
fn counted(rendered: &str, one: &str, many: &str) -> String {
    let noun = if rendered == "1" { one } else { many };
    format!("{rendered} {noun}")
}

/// The metric under `key`, or under its portfolio-prefixed form.
fn metric<'a>(metrics: &'a BTreeMap<String, String>, key: &str, prefix: &str) -> Option<&'a str> {
    metrics
        .get(key)
        .or_else(|| metrics.get(&format!("{prefix}{key}")))
        .map(String::as_str)
}

/// The capability's wording of a score's metrics, its clauses joined with ` · `:
/// - late orders (and units): "2 orders late (15 units)", or "no order late";
/// - residual shortfall: "5 units short", or "no shortfall";
/// - residual over-capacity overlap: "2 over-capacity overlaps", or "no over-capacity overlap".
///
/// A key read from a point scorer or its portfolio-prefixed form alike. An unknown key, or a
/// value that is not a count, is not worded; with no known key there is no wording (`None`).
#[must_use]
pub fn describe_score(metrics: &BTreeMap<String, String>) -> Option<String> {
    let mut clauses = Vec::new();
    if let Some(late) = metrics.get(LATE_ORDERS_KEY).and_then(|v| amount(v)) {
        clauses.push(if late == ZERO {
            "no order late".to_owned()
        } else {
            let orders = counted(&late, "order", "orders");
            match metrics.get(LATE_UNITS_KEY).and_then(|v| amount(v)) {
                Some(units) if units != ZERO => {
                    format!("{orders} late ({})", counted(&units, "unit", "units"))
                }
                _ => format!("{orders} late"),
            }
        });
    }
    if let Some(short) = metric(metrics, RESIDUAL_SHORTFALL_KEY, SUPPLY_PREFIX).and_then(amount) {
        clauses.push(if short == ZERO {
            "no shortfall".to_owned()
        } else {
            format!("{} short", counted(&short, "unit", "units"))
        });
    }
    if let Some(overlap) = metric(metrics, RESIDUAL_OVERLAP_KEY, PROCESS_PREFIX).and_then(amount) {
        clauses.push(if overlap == ZERO {
            "no over-capacity overlap".to_owned()
        } else {
            counted(&overlap, "over-capacity overlap", "over-capacity overlaps")
        });
    }
    (!clauses.is_empty()).then(|| clauses.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn worded(pairs: &[(&str, &str)]) -> Option<String> {
        describe_score(&metrics(pairs))
    }

    #[test]
    fn each_clause_is_worded_in_order() {
        assert_eq!(
            worded(&[
                ("late_orders", "2"),
                ("late_units", "15"),
                ("residual_shortfall", "5.5"),
                ("residual_overlap", "3"),
                ("risk_left", "2"),
                ("signal_cleared", "false"),
            ])
            .as_deref(),
            Some("2 orders late (15 units) · 5.5 units short · 3 over-capacity overlaps")
        );
    }

    #[test]
    fn zeros_have_their_own_wording() {
        assert_eq!(
            worded(&[
                ("late_orders", "0"),
                ("late_units", "0"),
                ("residual_shortfall", "0"),
                ("residual_overlap", "0"),
            ])
            .as_deref(),
            Some("no order late · no shortfall · no over-capacity overlap")
        );
        assert_eq!(
            worded(&[("late_orders", "2"), ("late_units", "0")]).as_deref(),
            Some("2 orders late"),
            "no late units, so no unit count"
        );
    }

    #[test]
    fn one_reads_singular() {
        assert_eq!(
            worded(&[
                ("late_orders", "1"),
                ("late_units", "1"),
                ("residual_shortfall", "1.0"),
                ("residual_overlap", "1"),
            ])
            .as_deref(),
            Some("1 order late (1 unit) · 1 unit short · 1 over-capacity overlap")
        );
    }

    #[test]
    fn the_portfolio_prefixed_keys_are_worded_alike() {
        assert_eq!(
            worded(&[
                ("late_orders", "0"),
                ("supply_risk.residual_shortfall", "13"),
                ("supply_risk.risk_left", "1"),
                ("process.residual_overlap", "0"),
                ("process.signal_cleared", "true"),
            ])
            .as_deref(),
            Some("no order late · 13 units short · no over-capacity overlap")
        );
    }

    #[test]
    fn with_no_known_key_there_is_no_wording() {
        assert_eq!(worded(&[]), None);
        assert_eq!(
            worded(&[("risk_left", "1"), ("signal_cleared", "false")]),
            None
        );
        // A value that is not a count is not worded.
        assert_eq!(worded(&[("late_orders", "lots")]), None);
        assert_eq!(worded(&[("residual_overlap", "-1")]), None);
    }
}
