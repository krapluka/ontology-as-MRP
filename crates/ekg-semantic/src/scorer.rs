//! The supply-risk and process [`Scorer`] drivers.
//!
//! Given a [`Projection`] (a candidate plan simulated onto a what-if copy), a scorer runs the
//! capability's rule against it and fills an opaque [`Score`]. All metric knowledge lives
//! here; the neutral seam knows nothing of contention, capacity or ranking.
//!
//! The rules are the monitors' own (for contention, [`PROCESS_CONTENTION_SELECT_SPARQL`] with
//! the same concurrency > capacity test), so a plan is scored by the signal that opened the
//! action point. Scoring reads only the projection and commits nothing.
//!
//! Display metrics (a projected completion date, a cost) go into the same `metrics` bag but
//! are not part of the rank.
//!
//! [`Projection`]: ekg_domain::Projection
//! [`Score`]: ekg_domain::Score
//! [`Scorer`]: ekg_domain::Scorer
//! [`PROCESS_CONTENTION_SELECT_SPARQL`]: crate::ontology::PROCESS_CONTENTION_SELECT_SPARQL

use std::collections::BTreeMap;

use async_trait::async_trait;
use ekg_domain::{Plan, Projection, Result, Score, Scorer};

use crate::display::{self, queries::promises_query};
use crate::ontology::{PROCESS_CONTENTION_SELECT_SPARQL, RISK_RULE_SELECT_SPARQL};
use crate::tabs::Day;

/// Width the residual counts are zero-padded to in the `rank_key`, so lexical order of the
/// key matches numeric order (smaller is better and sorts first). Five digits is far more
/// contended anchors or overlapping peers than a plant has.
const RANK_FIELD_WIDTH: usize = 5;

/// Field width the residual shortfall is zero-padded to in the supply-risk `rank_key`,
/// formatted with a fixed 3-decimal fraction so a fractional quantity still orders
/// numerically. Nineteen is fifteen integer digits plus the `.` and three decimals.
const SHORTFALL_FIELD_WIDTH: usize = 19;

/// Decimal places kept for the residual shortfall in the `rank_key`, enough to separate
/// close fractional residuals.
const SHORTFALL_DECIMALS: usize = 3;

/// Field width the summed promise days are zero-padded to in the supply-risk `rank_key`.
/// Twelve digits covers about 27 million orders promised in the 2020s.
const PROMISE_FIELD_WIDTH: usize = 12;

/// The process resource-contention [`Scorer`]. Stateless: it runs the contention rule
/// against whatever projection it is handed.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessContentionScorer;

impl ProcessContentionScorer {
    /// Build the scorer.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// The residual contention a projection still carries, read from the contention
/// rule's projected rows by the same interpretation the monitor uses.
struct Residual {
    /// Work-order anchors still over their agent's capacity (concurrency > capacity).
    /// Zero means the signal has cleared under this projection.
    over_capacity_anchors: usize,
    /// Total overlapping peers across those anchors, so a plan that reduces contention
    /// without eliminating it still ranks ahead of one that changes nothing.
    residual_overlap: u64,
}

/// Reads the contention rule's projected rows with the predicate the monitor uses,
/// [`crate::ontology::over_capacity`], so scorer and monitor cannot drift. A row missing
/// its keying bindings, or whose peer count is unrepresentable, is skipped.
fn interpret_residual(rows: &[BTreeMap<String, String>]) -> Residual {
    let mut over_capacity_anchors = 0;
    let mut residual_overlap = 0u64;
    for row in rows {
        let (Some(capacity), Some(overlap_peers)) = (
            row.get("capacity").and_then(|c| c.parse::<f64>().ok()),
            row.get("overlapPeers").and_then(|p| p.parse::<i64>().ok()),
        ) else {
            continue;
        };
        if crate::ontology::over_capacity(capacity, overlap_peers) == Some(true) {
            over_capacity_anchors += 1;
            residual_overlap = residual_overlap.saturating_add(overlap_peers.unsigned_abs());
        }
    }
    Residual {
        over_capacity_anchors,
        residual_overlap,
    }
}

#[async_trait]
impl Scorer for ProcessContentionScorer {
    async fn score(&self, projection: &dyn Projection) -> Result<Score> {
        let rows = projection.select(PROCESS_CONTENTION_SELECT_SPARQL).await?;
        let residual = interpret_residual(&rows);

        let cleared = residual.over_capacity_anchors == 0;
        let metrics = BTreeMap::from([
            (
                "risk_left".to_owned(),
                residual.over_capacity_anchors.to_string(),
            ),
            (
                display::RESIDUAL_OVERLAP_KEY.to_owned(),
                residual.residual_overlap.to_string(),
            ),
            (
                "signal_cleared".to_owned(),
                if cleared { "true" } else { "false" }.to_owned(),
            ),
        ]);

        // Best first: fewer over-capacity anchors, then less residual overlap. Zero-padded so
        // lexical order of the key is numeric order.
        let rank_key = format!(
            "{:0width$}:{:0width$}",
            residual.over_capacity_anchors,
            residual.residual_overlap,
            width = RANK_FIELD_WIDTH,
        );

        Ok(Score { metrics, rank_key })
    }

    fn describe(&self, score: &Score) -> Option<String> {
        display::describe_score(&score.metrics)
    }
}

/// The supply-risk [`Scorer`]. Stateless, like [`ProcessContentionScorer`], with a
/// different rule.
///
/// The rule is the supply-risk monitor's, [`RISK_RULE_SELECT_SPARQL`]. Every row it returns
/// is an at-risk `(order, component)` (the SELECT filters `shortfall > 0`), so a projected
/// state with no rows has cleared the signal.
///
/// The rank key is `late:late_units:promise:late_days:risk_left:residual`, best first:
/// - `late`: the orders the projection leaves late (the delivery judgment of
///   `crate::display`: lot by lot for an order fulfilled by manufacturing lots, else by
///   demand), so a plan that brings every order in on time comes first;
/// - `late_units`: the units of those orders' lots completing after the promise, so among
///   equally late plans the one delivering more units on time comes first (a split counts);
/// - `promise`: the sum of every sales order's promise in whole days. Every candidate and the
///   baseline project one base world, so the sum differs only by the days a plan moves
///   promises: a plan that moves a promise later ranks behind one that moves none;
/// - `late_days`: the whole days the late orders' lots complete after their promise, summed.
///   It comes after `promise`, so a re-promise cannot buy a better rank by shrinking the slip;
/// - then the rule's residual: at-risk rows, then residual shortfall.
///
/// [`Scorer::score_plan`] appends the plan's cost as the last tie-breaker
/// ([`display::plan_cost_key`]); [`Scorer::score`] keeps the key without it.
///
/// So an on-time expedite outranks a clearing re-promise, and both outrank doing nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct SupplyRiskScorer;

impl SupplyRiskScorer {
    /// Build the scorer.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// The residual supply risk a projection still carries, read from the risk rule's
/// projected rows.
struct RiskResidual {
    /// At-risk `(order, component)` rows still short under this projection. Zero means the
    /// shortfall signal has cleared.
    risk_left: usize,
    /// Residual shortfall summed across those rows, so a plan that reduces shortfall
    /// without eliminating it still ranks ahead of one that changes nothing.
    residual_shortfall: f64,
}

/// Reads the risk rule's projected rows: each row is an at-risk order/component (the
/// SELECT filters `shortfall > 0`), so the row count is `risk_left` and the sum of
/// `?shortfall` is the residual. A row whose `shortfall` does not parse still counts as
/// at-risk but adds nothing to the residual sum, so it can never clear the signal.
fn interpret_risk_residual(rows: &[BTreeMap<String, String>]) -> RiskResidual {
    let mut residual_shortfall = 0.0;
    for row in rows {
        if let Some(shortfall) = row.get("shortfall").and_then(|s| s.parse::<f64>().ok()) {
            residual_shortfall += shortfall.max(0.0);
        }
    }
    RiskResidual {
        risk_left: rows.len(),
        residual_shortfall,
    }
}

/// Sum of every sales order's promise in whole days since the epoch, from the promises
/// query's rows. A promise that cannot be read as a day contributes nothing (no plan the
/// scorer compares can move it either).
fn promise_days(rows: &[BTreeMap<String, String>]) -> u64 {
    rows.iter()
        .filter_map(|row| row.get("commitment"))
        .filter_map(|commitment| Day::from_iso(commitment))
        .filter_map(|day| u64::try_from(day.days_since(Day::EPOCH)).ok())
        .fold(0, u64::saturating_add)
}

#[async_trait]
impl Scorer for SupplyRiskScorer {
    async fn score(&self, projection: &dyn Projection) -> Result<Score> {
        let rows = projection.select(RISK_RULE_SELECT_SPARQL).await?;
        let residual = interpret_risk_residual(&rows);
        let (late, late_units, late_days) = display::lateness(projection, &rows).await?;
        let promises = promise_days(&projection.select(&promises_query()).await?);

        let cleared = residual.risk_left == 0;
        let metrics = BTreeMap::from([
            (display::LATE_ORDERS_KEY.to_owned(), late.to_string()),
            (display::LATE_UNITS_KEY.to_owned(), late_units.to_string()),
            ("risk_left".to_owned(), residual.risk_left.to_string()),
            (
                display::RESIDUAL_SHORTFALL_KEY.to_owned(),
                residual.residual_shortfall.to_string(),
            ),
            (
                "signal_cleared".to_owned(),
                if cleared { "true" } else { "false" }.to_owned(),
            ),
        ]);

        // Best first: fewer late orders, then fewer late units, then promises moved less far,
        // then fewer days late, then fewer at-risk rows, then less residual shortfall. Every
        // field is zero-padded to a fixed width (the late units and the shortfall with a fixed
        // 3-decimal fraction; both are non-negative) so lexical order of the key is numeric
        // order. No float-to-int cast, so a huge residual cannot wrap.
        let late_days = late_days.max(0);
        let rank_key = format!(
            "{late:0risk_width$}:{late_units:0short_width$.prec$}:{promises:0promise_width$}:\
             {late_days:0promise_width$}:{:0risk_width$}:{:0short_width$.prec$}",
            residual.risk_left,
            residual.residual_shortfall,
            risk_width = RANK_FIELD_WIDTH,
            promise_width = PROMISE_FIELD_WIDTH,
            short_width = SHORTFALL_FIELD_WIDTH,
            prec = SHORTFALL_DECIMALS,
        );

        Ok(Score { metrics, rank_key })
    }

    /// [`Scorer::score`] with the plan's cost appended as the last tie-breaker, so a cheaper
    /// plan ranks first only when everything else is equal.
    async fn score_plan(&self, plan: &Plan, projection: &dyn Projection) -> Result<Score> {
        let mut score = self.score(projection).await?;
        let cost = display::plan_cost_key(projection, plan).await?;
        score.rank_key = format!("{}:{cost}", score.rank_key);
        Ok(score)
    }

    fn describe(&self, score: &Score) -> Option<String> {
        display::describe_score(&score.metrics)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use ekg_domain::{Projection, Result};

    use super::{ProcessContentionScorer, Scorer, SupplyRiskScorer};

    /// A fake projection returning canned contention-rule rows. It ignores the SPARQL text,
    /// like the in-memory fakes.
    struct CannedProjection(Vec<BTreeMap<String, String>>);

    #[async_trait]
    impl Projection for CannedProjection {
        async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            Ok(self.0.clone())
        }
    }

    /// One contention-rule row: an anchor on a capacity-`cap` agent with `peers`
    /// overlapping peers.
    fn row(cap: &str, peers: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "wo".to_owned(),
                "https://ekg.example/id/mrp.workorder/1".to_owned(),
            ),
            (
                "wc".to_owned(),
                "https://ekg.example/id/mrp.workcenter/1".to_owned(),
            ),
            (
                "mo".to_owned(),
                "https://ekg.example/id/mrp.production/1".to_owned(),
            ),
            ("capacity".to_owned(), cap.to_owned()),
            ("overlapPeers".to_owned(), peers.to_owned()),
        ])
    }

    #[tokio::test]
    async fn a_cleared_projection_scores_zero_risk_and_outranks_a_contended_one() {
        let scorer = ProcessContentionScorer::new();

        // No rows: no anchor is over capacity.
        let cleared = scorer
            .score(&CannedProjection(Vec::new()))
            .await
            .expect("score");
        assert_eq!(
            cleared.metrics.get("risk_left").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            cleared.metrics.get("signal_cleared").map(String::as_str),
            Some("true")
        );

        // A capacity-1 agent with one overlapping peer: concurrency 2 > 1.
        let contended = scorer
            .score(&CannedProjection(vec![row("1", "1")]))
            .await
            .expect("score");
        assert_eq!(
            contended.metrics.get("risk_left").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            contended.metrics.get("signal_cleared").map(String::as_str),
            Some("false")
        );

        assert!(
            cleared.rank_key < contended.rank_key,
            "a plan that clears the contention must outrank one that leaves it"
        );
    }

    #[tokio::test]
    async fn a_within_capacity_agent_is_not_counted_as_contention() {
        let scorer = ProcessContentionScorer::new();
        // Capacity 2 with one overlapping peer: concurrency 2, not over capacity.
        let score = scorer
            .score(&CannedProjection(vec![row("2", "1")]))
            .await
            .expect("score");
        assert_eq!(
            score.metrics.get("risk_left").map(String::as_str),
            Some("0")
        );
    }

    #[tokio::test]
    async fn reducing_but_not_clearing_contention_ranks_between() {
        let scorer = ProcessContentionScorer::new();
        // Two contended anchors, heavy overlap.
        let worse = scorer
            .score(&CannedProjection(vec![row("1", "3"), row("1", "2")]))
            .await
            .expect("score");
        // One contended anchor, lighter overlap: a partial improvement.
        let better = scorer
            .score(&CannedProjection(vec![row("1", "1")]))
            .await
            .expect("score");
        // Fully cleared.
        let best = scorer
            .score(&CannedProjection(Vec::new()))
            .await
            .expect("score");
        assert!(best.rank_key < better.rank_key);
        assert!(better.rank_key < worse.rank_key);
    }

    #[tokio::test]
    async fn the_process_scorer_words_its_score() {
        let scorer = ProcessContentionScorer::new();
        let contended = scorer
            .score(&CannedProjection(vec![row("1", "3"), row("1", "2")]))
            .await
            .expect("score");
        assert_eq!(
            scorer.describe(&contended).as_deref(),
            Some("5 over-capacity overlaps")
        );
        let cleared = scorer
            .score(&CannedProjection(Vec::new()))
            .await
            .expect("score");
        assert_eq!(
            scorer.describe(&cleared).as_deref(),
            Some("no over-capacity overlap")
        );
    }

    /// A supply-risk projection: the rule's rows, and every order's promise (so the scorer's
    /// two reads are answered separately, by query).
    struct SupplyProjection {
        risk: Vec<BTreeMap<String, String>>,
        promises: Vec<(&'static str, &'static str)>,
        lots: Vec<BTreeMap<String, String>>,
        inbound: Vec<(&'static str, &'static str)>,
        /// The cost every purchase target reads as.
        cost: Option<&'static str>,
    }

    #[async_trait]
    impl Projection for SupplyProjection {
        async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            if sparql == crate::display::queries::promises_query() {
                return Ok(self
                    .promises
                    .iter()
                    .map(|(so, c)| {
                        BTreeMap::from([
                            ("so".to_owned(), (*so).to_owned()),
                            ("commitment".to_owned(), (*c).to_owned()),
                        ])
                    })
                    .collect());
            }
            if sparql.contains("SELECT ?so ?mo ?qty ?start ?end ?comp ?perUnit ?onHand ?woEnd") {
                return Ok(self.lots.clone());
            }
            if sparql.contains("(SUM(?price) AS ?cost)") {
                return Ok(self
                    .cost
                    .iter()
                    .map(|cost| BTreeMap::from([("cost".to_owned(), (*cost).to_owned())]))
                    .collect());
            }
            if sparql.contains("SELECT ?comp ?open ?arrival") {
                return Ok(self
                    .inbound
                    .iter()
                    .map(|(open, arrival)| {
                        BTreeMap::from([
                            ("comp".to_owned(), COMP.to_owned()),
                            ("open".to_owned(), (*open).to_owned()),
                            ("arrival".to_owned(), (*arrival).to_owned()),
                        ])
                    })
                    .collect());
            }
            Ok(self.risk.clone())
        }
    }

    const COMP: &str = "https://ekg.example/id/product.product/101";

    const SO_1: &str = "https://ekg.example/id/sale.order/1";
    const PROMISE: &str = "2027-10-22T00:00:00";

    /// Score `risk` rows with the flagship's promise (unmoved).
    async fn score_rows(risk: Vec<BTreeMap<String, String>>) -> ekg_domain::Score {
        score_with(risk, PROMISE).await
    }

    async fn score_with(
        risk: Vec<BTreeMap<String, String>>,
        promise: &'static str,
    ) -> ekg_domain::Score {
        SupplyRiskScorer::new()
            .score(&SupplyProjection {
                risk,
                promises: vec![(SO_1, promise)],
                lots: Vec::new(),
                inbound: Vec::new(),
                cost: None,
            })
            .await
            .expect("score")
    }

    /// One risk-rule row: an at-risk (order, component) with the given `shortfall` and no
    /// safety stock (so it is also short by demand).
    fn risk_row(shortfall: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("so".to_owned(), SO_1.to_owned()),
            (
                "comp".to_owned(),
                "https://ekg.example/id/product.product/101".to_owned(),
            ),
            ("shortfall".to_owned(), shortfall.to_owned()),
            ("ss".to_owned(), "0".to_owned()),
            ("commitment".to_owned(), PROMISE.to_owned()),
        ])
    }

    /// A row whose shortfall is only the safety buffer: flagged by the rule, demand covered.
    fn safety_only_row(shortfall: &str, safety: &str) -> BTreeMap<String, String> {
        let mut row = risk_row(shortfall);
        row.insert("ss".to_owned(), safety.to_owned());
        row
    }

    #[tokio::test]
    async fn an_on_time_expedite_outranks_a_clearing_re_promise_and_both_beat_the_baseline() {
        // Baseline: the order is short by demand (late 1).
        let baseline = score_rows(vec![risk_row("13")]).await;
        // Expedite: demand covered by the promise, the safety breach stays flagged (late 0).
        let expedite = score_rows(vec![safety_only_row("3", "5")]).await;
        // Re-promise: clears the rule by moving the promise 25 days later (late 0).
        let re_promise = score_with(Vec::new(), "2027-11-16T00:00:00").await;
        assert_eq!(
            expedite.metrics.get("late_orders").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            re_promise.metrics.get("late_orders").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            baseline.metrics.get("late_orders").map(String::as_str),
            Some("1")
        );
        assert!(
            expedite.rank_key < re_promise.rank_key,
            "the on-time expedite must be recommended over the late re-promise, even though \
             the re-promise clears the rule: {} vs {}",
            expedite.rank_key,
            re_promise.rank_key
        );
        assert!(
            re_promise.rank_key < baseline.rank_key,
            "both beat doing nothing"
        );
    }

    #[tokio::test]
    async fn plans_that_move_no_promise_rank_by_residual_as_before() {
        let fewer = score_rows(vec![risk_row("13")]).await;
        let more = score_rows(vec![risk_row("1"), risk_row("1")]).await;
        assert!(fewer.rank_key < more.rank_key, "fewer at-risk rows first");
        let smaller = score_rows(vec![risk_row("3")]).await;
        assert!(
            smaller.rank_key < fewer.rank_key,
            "then less residual shortfall"
        );
    }

    #[tokio::test]
    async fn rank_keys_stay_lexically_numeric() {
        // Every field is fixed-width, so the key sorts as its numbers do.
        let key = score_rows(vec![risk_row("3.5")]).await.rank_key;
        let fields: Vec<&str> = key.split(':').collect();
        assert_eq!(fields.len(), 6, "{key}");
        assert_eq!(
            fields.iter().map(|f| f.len()).collect::<Vec<_>>(),
            vec![5, 19, 12, 12, 5, 19],
            "{key}"
        );
        let far = score_with(vec![risk_row("3.5")], "2099-12-31T00:00:00")
            .await
            .rank_key;
        assert_eq!(far.len(), key.len(), "a later promise never widens the key");
        assert!(key < far);
    }

    #[tokio::test]
    async fn a_cleared_projection_scores_zero_risk_and_outranks_an_at_risk_one() {
        // No rows: no order is still short.
        let cleared = score_rows(Vec::new()).await;
        assert_eq!(
            cleared.metrics.get("risk_left").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            cleared.metrics.get("signal_cleared").map(String::as_str),
            Some("true")
        );

        // At risk: one order still short by 13 components.
        let at_risk = score_rows(vec![risk_row("13")]).await;
        assert_eq!(
            at_risk.metrics.get("risk_left").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            at_risk.metrics.get("signal_cleared").map(String::as_str),
            Some("false")
        );

        assert!(
            cleared.rank_key < at_risk.rank_key,
            "a plan that clears the shortfall must outrank one that leaves it"
        );
    }

    #[tokio::test]
    async fn a_smaller_residual_outranks_a_larger_one_at_equal_risk_left() {
        // Both leave one order at risk, but one leaves less shortfall and ranks ahead.
        let smaller = score_rows(vec![risk_row("3")]).await;
        let larger = score_rows(vec![risk_row("13")]).await;
        assert_eq!(
            smaller.metrics.get("risk_left").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            larger.metrics.get("risk_left").map(String::as_str),
            Some("1")
        );
        assert!(
            smaller.rank_key < larger.rank_key,
            "at equal risk_left, a smaller residual shortfall must outrank a larger one"
        );
    }

    #[tokio::test]
    async fn a_fractional_residual_orders_by_value_not_text_length() {
        // Shortfall is `(required + safety) - available` over Odoo float quantities, so a
        // fractional residual is reachable. At equal risk_left the smaller residual must rank
        // first, ordered by value, not string length (`3.05 < 3.5`), and equal values must tie
        // (`3.5 == 3.50`).
        let smaller = score_rows(vec![risk_row("3.05")]).await;
        let larger = score_rows(vec![risk_row("3.5")]).await;
        assert!(
            smaller.rank_key < larger.rank_key,
            "a 3.05 residual must outrank a 3.5 residual at equal risk_left"
        );
        let as_written = score_rows(vec![risk_row("3.5")]).await;
        let padded = score_rows(vec![risk_row("3.50")]).await;
        assert_eq!(
            as_written.rank_key, padded.rank_key,
            "3.5 and 3.50 are the same residual and must produce the same rank_key"
        );
    }

    #[tokio::test]
    async fn fewer_at_risk_orders_dominate_the_residual_tiebreak() {
        // One order at risk with a large shortfall still outranks two orders each with a
        // tiny shortfall: risk_left is the primary key, residual only the tiebreak.
        let one_big = score_rows(vec![risk_row("99")]).await;
        let two_small = score_rows(vec![risk_row("1"), risk_row("1")]).await;
        assert!(
            one_big.rank_key < two_small.rank_key,
            "fewer at-risk orders must dominate the residual-shortfall tiebreak"
        );
    }

    /// A lot row fulfilling `SO_1`: `qty` units of a product needing one `COMP` each, with 30
    /// on hand.
    fn lot_row(mo: i64, qty: &str, start: &str, end: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("so".to_owned(), SO_1.to_owned()),
            (
                "mo".to_owned(),
                format!("https://ekg.example/id/mrp.production/{mo}"),
            ),
            ("qty".to_owned(), qty.to_owned()),
            ("start".to_owned(), start.to_owned()),
            ("end".to_owned(), end.to_owned()),
            ("comp".to_owned(), COMP.to_owned()),
            ("perUnit".to_owned(), "1".to_owned()),
            ("onHand".to_owned(), "30".to_owned()),
        ])
    }

    async fn score_lots(lots: Vec<BTreeMap<String, String>>) -> ekg_domain::Score {
        SupplyRiskScorer::new()
            .score(&SupplyProjection {
                risk: Vec::new(),
                promises: vec![(SO_1, "2027-10-26T00:00:00")],
                lots,
                inbound: vec![("10", "2027-10-27T08:00:00")],
                cost: None,
            })
            .await
            .expect("score")
    }

    #[tokio::test]
    async fn fewer_late_units_rank_first_among_equally_late_plans() {
        // One lot of 40 waiting for the 21 Oct arrival: 40 late units.
        let whole = score_lots(vec![lot_row(
            1,
            "40",
            "2027-10-22T08:00:00",
            "2027-10-23T08:00:00",
        )])
        .await;
        // Split 30 + 10: the 30 ship on time from stock, only the 10 wait.
        let split = score_lots(vec![
            lot_row(1, "30", "2027-10-21T08:00:00", "2027-10-22T08:00:00"),
            lot_row(2, "10", "2027-10-24T08:00:00", "2027-10-25T08:00:00"),
        ])
        .await;
        for (score, units) in [(&whole, "40"), (&split, "10")] {
            assert_eq!(
                score.metrics.get("late_orders").map(String::as_str),
                Some("1")
            );
            assert_eq!(
                score.metrics.get("late_units").map(String::as_str),
                Some(units)
            );
        }
        assert!(
            split.rank_key < whole.rank_key,
            "{} vs {}",
            split.rank_key,
            whole.rank_key
        );
    }

    #[tokio::test]
    async fn the_supply_risk_scorer_words_its_score() {
        let scorer = SupplyRiskScorer::new();
        let short = score_rows(vec![risk_row("13")]).await;
        assert_eq!(
            scorer.describe(&short).as_deref(),
            Some("1 order late · 13 units short")
        );
        let cleared = score_rows(Vec::new()).await;
        assert_eq!(
            scorer.describe(&cleared).as_deref(),
            Some("no order late · no shortfall")
        );
    }

    // --- days late ---

    /// The rush order's one lot of 5 (material on hand), finishing 1 Oct 12:00, with its work
    /// order ending `wo_end`; `promise` is the projection's promise and `risk` the rule's rows.
    async fn score_rush(
        wo_end: &str,
        promise: &'static str,
        risk: Vec<BTreeMap<String, String>>,
    ) -> ekg_domain::Score {
        let mut lot = lot_row(3, "5", "2027-10-06T08:00:00", "2027-10-07T12:00:00");
        lot.insert("woEnd".to_owned(), wo_end.to_owned());
        SupplyRiskScorer::new()
            .score(&SupplyProjection {
                risk,
                promises: vec![(SO_1, promise)],
                lots: vec![lot],
                inbound: Vec::new(),
                cost: None,
            })
            .await
            .expect("score")
    }

    /// The `late_days` field of a supply-risk rank key.
    fn late_days_field(key: &str) -> &str {
        key.split(':').nth(3).expect("six fields")
    }

    #[tokio::test]
    async fn a_plan_that_delays_an_order_further_ranks_behind_one_that_does_not() {
        const PROMISED: &str = "2027-10-06T00:00:00";
        // One day late, 23 short; four days late (the work order moved), only 13 short.
        let one_day = score_rush("2027-10-07T12:00:00", PROMISED, vec![risk_row("23")]).await;
        let four_days = score_rush("2027-10-10T12:00:00", PROMISED, vec![risk_row("13")]).await;
        for score in [&one_day, &four_days] {
            assert_eq!(
                score.metrics.get("late_orders").map(String::as_str),
                Some("1")
            );
            assert_eq!(
                score.metrics.get("late_units").map(String::as_str),
                Some("5")
            );
        }
        assert_eq!(
            late_days_field(&one_day.rank_key).parse::<i64>().ok(),
            Some(1)
        );
        assert_eq!(
            late_days_field(&four_days.rank_key).parse::<i64>().ok(),
            Some(4)
        );
        assert!(
            one_day.rank_key < four_days.rank_key,
            "fewer days late first, even with more shortfall: {} vs {}",
            one_day.rank_key,
            four_days.rank_key
        );
    }

    #[tokio::test]
    async fn a_re_promise_cannot_buy_a_better_rank_through_a_smaller_slip() {
        // Both complete 4 Oct. One keeps the 30 Sep promise (4 days late); the other moves it
        // to 2 Oct (2 days late against the moved promise).
        let kept = score_rush("2027-10-10T12:00:00", "2027-10-06T00:00:00", Vec::new()).await;
        let moved = score_rush("2027-10-10T12:00:00", "2027-10-08T00:00:00", Vec::new()).await;
        assert!(late_days_field(&moved.rank_key) < late_days_field(&kept.rank_key));
        assert!(
            kept.rank_key < moved.rank_key,
            "moving no promise ranks first: {} vs {}",
            kept.rank_key,
            moved.rank_key
        );
    }

    #[tokio::test]
    async fn orders_with_no_known_completion_add_no_days() {
        // 40 units, 30 on hand, 10 more arrive 21 Oct: the lot completes 22 Oct, 2 days late.
        let dated = score_lots(vec![lot_row(
            1,
            "40",
            "2027-10-22T08:00:00",
            "2027-10-23T08:00:00",
        )])
        .await;
        assert_eq!(
            dated.metrics.get("late_orders").map(String::as_str),
            Some("1")
        );
        // Without the 21 Oct arrival the 40 are never covered: the lot has no completion.
        let undated = SupplyRiskScorer::new()
            .score(&SupplyProjection {
                risk: Vec::new(),
                promises: vec![(SO_1, "2027-10-26T00:00:00")],
                lots: vec![lot_row(
                    1,
                    "40",
                    "2027-10-22T08:00:00",
                    "2027-10-23T08:00:00",
                )],
                inbound: Vec::new(),
                cost: None,
            })
            .await
            .expect("score");
        assert_eq!(
            late_days_field(&undated.rank_key).parse::<i64>().ok(),
            Some(0)
        );
        assert_eq!(
            late_days_field(&dated.rank_key).parse::<i64>().ok(),
            Some(2)
        );
    }

    // --- cost last ---

    /// Score `plan` over a projection leaving `risk` rows, where every purchase target costs
    /// `cost`.
    async fn score_priced(
        plan: &ekg_domain::Plan,
        risk: Vec<BTreeMap<String, String>>,
        cost: &'static str,
    ) -> ekg_domain::Score {
        SupplyRiskScorer::new()
            .score_plan(
                plan,
                &SupplyProjection {
                    risk,
                    promises: vec![(SO_1, PROMISE)],
                    lots: Vec::new(),
                    inbound: Vec::new(),
                    cost: Some(cost),
                },
            )
            .await
            .expect("score")
    }

    fn expedite() -> ekg_domain::Plan {
        ekg_domain::Plan {
            actions: vec![ekg_domain::ProposedAction {
                action_type: ekg_domain::ActionType("expedite_purchase_line".to_owned()),
                target: ekg_domain::EntityUri(
                    "https://ekg.example/id/purchase.order.line/7".to_owned(),
                ),
                params: BTreeMap::new(),
            }],
        }
    }

    fn raise_capacity() -> ekg_domain::Plan {
        ekg_domain::Plan {
            actions: vec![ekg_domain::ProposedAction {
                action_type: ekg_domain::ActionType("raise_work_center_capacity".to_owned()),
                target: ekg_domain::EntityUri("https://ekg.example/id/mrp.workcenter/1".to_owned()),
                params: BTreeMap::new(),
            }],
        }
    }

    #[tokio::test]
    async fn cost_breaks_a_full_tie_and_only_a_full_tie() {
        let paid = score_priced(&expedite(), vec![risk_row("13")], "320.0").await;
        let free = score_priced(&raise_capacity(), vec![risk_row("13")], "320.0").await;
        assert!(
            paid.rank_key.ends_with(":0000000000320.00"),
            "{}",
            paid.rank_key
        );
        assert!(
            free.rank_key.ends_with(":0000000000000.00"),
            "{}",
            free.rank_key
        );
        assert!(
            free.rank_key < paid.rank_key,
            "the cheaper plan first at a full tie"
        );
        // Less residual shortfall outweighs any cost.
        let paid_better = score_priced(&expedite(), vec![risk_row("3")], "320.0").await;
        assert!(paid_better.rank_key < free.rank_key);
        // The plain score keeps its key without the cost.
        let plain = score_rows(vec![risk_row("13")]).await;
        assert_eq!(
            format!("{}:0000000000000.00", plain.rank_key),
            free.rank_key
        );
    }

    #[tokio::test]
    async fn an_unreadable_cost_ranks_last_among_ties() {
        let unreadable = score_priced(&expedite(), vec![risk_row("13")], "n/a").await;
        let paid = score_priced(&expedite(), vec![risk_row("13")], "99999.0").await;
        assert!(paid.rank_key < unreadable.rank_key);
    }
}
