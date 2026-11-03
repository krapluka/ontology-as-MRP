//! Delivery judged by demand coverage, and the plan framing built on it.
//!
//! An order is delivered on time when, for every component it needs, the on-hand stock plus the
//! open inbound arriving by its promise covers its demand. Safety stock is a planning buffer:
//! the risk rule still flags a safety breach (and still opens points and scores slates), but a
//! breach alone never makes a delivery late. The judgment reuses the rule's own rows: for a
//! flagged row, demand is covered at the promise if and only if `shortfall <= safety`, since
//! `shortfall - safety = required - (on hand + inbound by the promise)`. So it cannot drift from
//! the monitor's requirement roll-up, on-hand or receipt truth.
//!
//! Everything runs against one `select` source per side: the current world for the baseline
//! ([`WorldSource`]), a what-if [`Projection`] for an option. So an expedite (which moves
//! arrivals) and a re-promise (which moves the promise) both flow through the same code. The
//! slip is always measured against the original promise, read from the current world.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use ekg_domain::{EntityUri, Plan, Projection, Result, WorldModel};

use super::money::{self, Money};
use super::queries;
use crate::ontology::{self, PROCESS_CONTENTION_SELECT_SPARQL, RISK_RULE_SELECT_SPARQL};
use crate::tabs::Day;

type Row = BTreeMap<String, String>;

/// Quantities are the source's decimals read as `f64`; compare them with a tolerance so a sum
/// that is equal in decimal is never judged short by a binary rounding residue.
const QTY_EPSILON: f64 = 1e-9;

/// The Odoo model of a sales-order subject.
const SALES_ORDER: &str = "sale.order";
/// The Odoo model of a manufacturing-order subject.
const MANUFACTURING_ORDER: &str = "mrp.production";
/// The Odoo model of a purchase-order target.
const PURCHASE_ORDER: &str = "purchase.order";
/// The Odoo model of a purchase-order-line target.
const PURCHASE_ORDER_LINE: &str = "purchase.order.line";

/// Width of a plan's cost field in a rank key: the cost with two decimals, zero-padded, so
/// lexical order is numeric order.
const COST_KEY_WIDTH: usize = 16;

/// The current world model as a `select` source, so the baseline is judged by the same code
/// that judges a what-if projection. Read-only.
pub struct WorldSource<'a>(pub &'a dyn WorldModel);

#[async_trait]
impl Projection for WorldSource<'_> {
    async fn select(&self, sparql: &str) -> Result<Vec<Row>> {
        self.0.select(sparql).await
    }
}

/// A delivery outcome, as the opaque `delivery` display value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Demand is covered and the order ships by its original promise.
    OnTime,
    /// The order ships, but after its original promise (a positive slip).
    Late,
    /// Demand is not covered and no completion date is known.
    AtRisk,
}

impl Delivery {
    /// The opaque display value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnTime => "on_time",
            Self::Late => "late",
            Self::AtRisk => "at_risk",
        }
    }
}

/// A subject's judged delivery under one source: the outcome, the projected completion (ISO,
/// as read) and the whole-day slip against the original promise. Completion and slip are
/// absent when no date is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The delivery outcome.
    pub delivery: Delivery,
    /// The projected completion date.
    pub completion: Option<String>,
    /// Whole days from the original promise to the projected completion.
    pub slip: Option<i64>,
}

/// The Odoo model segment of an entity IRI.
pub(crate) fn model_of(subject: &EntityUri) -> Option<&str> {
    subject
        .0
        .strip_prefix(ontology::ID_NAMESPACE)?
        .rsplit_once('/')
        .map(|(model, _)| model)
}

/// A bound, non-empty binding.
fn bound<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

fn number(row: &Row, key: &str) -> Option<f64> {
    bound(row, key)?.parse().ok()
}

/// Whether a risk-rule row leaves the order's demand uncovered at its promise
/// (`shortfall > safety`). A row whose quantities cannot be read counts as uncovered: its
/// presence is the signal, and a delivery is never judged on time on an unread row.
pub(crate) fn late_by_demand(row: &Row) -> bool {
    match (number(row, "shortfall"), number(row, "ss")) {
        (Some(shortfall), Some(safety)) => shortfall - safety > QTY_EPSILON,
        _ => true,
    }
}

/// One component an order is short of at its promise, by demand.
struct ShortComponent {
    comp: String,
    /// The order's requirement and the component's on-hand, when readable.
    required_and_on_hand: Option<(f64, f64)>,
}

/// The components `order` is short of by demand, from the rule's rows.
fn short_components(rows: &[Row], order: &str) -> Vec<ShortComponent> {
    rows.iter()
        .filter(|row| bound(row, "so") == Some(order) && late_by_demand(row))
        .filter_map(|row| {
            Some(ShortComponent {
                comp: bound(row, "comp")?.to_owned(),
                required_and_on_hand: number(row, "required").zip(number(row, "oh")),
            })
        })
        .collect()
}

/// A component's coverage date: accumulating the open inbound from on hand in arrival order,
/// the first arrival at which the requirement is covered. `None` when the known inbound never
/// covers it (a made component with no inbound, say).
pub(crate) fn coverage_date(
    required: f64,
    on_hand: f64,
    inbound: &[(f64, String)],
) -> Option<String> {
    let mut arrivals: Vec<&(f64, String)> = inbound.iter().collect();
    arrivals.sort_by(|a, b| a.1.cmp(&b.1));
    let mut available = on_hand;
    for (open, arrival) in arrivals {
        available += open;
        if available + QTY_EPSILON >= required {
            return Some(arrival.clone());
        }
    }
    None
}

/// One manufacturing lot fulfilling an order, as the lot judgment reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Lot {
    /// The lot's IRI (the tie-break of the start order).
    pub iri: String,
    /// The lot's quantity.
    pub qty: f64,
    /// Its planned start (ISO).
    pub start: String,
    /// Its planned end (ISO).
    pub end: String,
    /// Per component IRI, the per-unit requirement of the lot's product tree.
    pub per_unit: BTreeMap<String, f64>,
}

/// An order's lot-by-lot judgment: its projected completion (the latest lot completion,
/// `None` when any lot has no known completion) and its late units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LotJudgment {
    /// The latest lot completion, or `None` when some lot never gets its material.
    pub completion: Option<String>,
    /// The quantity of the lots completing after the promise (by whole day) or never.
    pub late_units: f64,
    /// Whole days from the promise to the latest lot completion when every lot completes at
    /// a known date after it; 0 otherwise (an undated lot is already counted in the units).
    pub late_days: i64,
}

/// Judge an order lot by lot. Lots are taken in planned-start order (then IRI); per component,
/// each lot's cumulative need (its own and every earlier lot's quantity times the per-unit
/// requirement) is compared with the on-hand stock plus the open inbound in arrival order:
/// - a lot is ready for a component when on hand covers the cumulative need; otherwise its
///   material date for it is the arrival that covers it ([`coverage_date`]), or none;
/// - a lot's material date is the latest over its components (none if any has none);
/// - a lot completes at its planned end when it is ready or its material date is not after its
///   start; at material date + (end - start) when later; at no known date without one.
///
/// Late units are the quantity of the lots completing after `promise` (whole days, as
/// [`judge`] counts the slip) or at no known date. `None` when a date cannot be read.
pub(crate) fn lot_outcome(
    lots: &[Lot],
    on_hand: &BTreeMap<String, f64>,
    inbound: &BTreeMap<String, Vec<(f64, String)>>,
    promise: &str,
) -> Option<LotJudgment> {
    use crate::simulator::{epoch_to_iso, iso_to_epoch};
    let promise_day = Day::from_iso(promise)?;
    let mut ordered: Vec<&Lot> = lots.iter().collect();
    ordered.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.iri.cmp(&b.iri)));

    let mut cumulative: BTreeMap<&str, f64> = BTreeMap::new();
    let mut late_units = 0.0;
    let mut latest: Option<String> = None;
    let mut every_known = true;
    for lot in ordered {
        // The lot's material date: the latest covering arrival over the components it waits
        // for; `Some(None)` when it waits for none, `None` when one is never covered.
        let mut material: Option<Option<String>> = Some(None);
        for (comp, per_unit) in &lot.per_unit {
            let need = cumulative.entry(comp.as_str()).or_insert(0.0);
            *need += lot.qty * per_unit;
            let stock = on_hand.get(comp).copied().unwrap_or(0.0);
            if stock + QTY_EPSILON >= *need {
                continue;
            }
            let lines = inbound.get(comp).map_or(&[][..], Vec::as_slice);
            material = match (material, coverage_date(*need, stock, lines)) {
                (Some(current), Some(arrival)) => {
                    Some(Some(current.map_or(arrival.clone(), |c| c.max(arrival))))
                }
                _ => None,
            };
        }
        let completion = match material {
            None => None,
            Some(None) => Some(lot.end.clone()),
            Some(Some(arrival)) => {
                let (start, end, at) = (
                    iso_to_epoch(&lot.start)?,
                    iso_to_epoch(&lot.end)?,
                    iso_to_epoch(&arrival)?,
                );
                Some(if at <= start {
                    lot.end.clone()
                } else {
                    epoch_to_iso(at + (end - start))
                })
            }
        };
        if let Some(done) = completion {
            if Day::from_iso(&done)?.days_since(promise_day) > 0 {
                late_units += lot.qty;
            }
            latest = Some(latest.map_or(done.clone(), |l| l.max(done)));
        } else {
            late_units += lot.qty;
            every_known = false;
        }
    }
    let completion = if every_known { latest } else { None };
    let late_days = match &completion {
        Some(done) => Day::from_iso(done)?.days_since(promise_day).max(0),
        None => 0,
    };
    Some(LotJudgment {
        completion,
        late_units,
        late_days,
    })
}

/// The later of two ISO instants: by their epoch when both read, else the lexically later.
fn later(a: &str, b: &str) -> String {
    use crate::simulator::iso_to_epoch;
    let a_later = match (iso_to_epoch(a), iso_to_epoch(b)) {
        (Some(x), Some(y)) => x >= y,
        _ => a >= b,
    };
    if a_later { a } else { b }.to_owned()
}

/// The lots fulfilling each sales order under `source` (every order when `order` is `None`),
/// and every listed component's on-hand stock. A lot's planned end is the later of its order's
/// own finish and its latest work-order end.
async fn order_lots(
    source: &dyn Projection,
    order: Option<&EntityUri>,
) -> Result<(BTreeMap<String, Vec<Lot>>, BTreeMap<String, f64>)> {
    let rows = source.select(&queries::order_lots_query(order)?).await?;
    let mut lots: BTreeMap<String, BTreeMap<String, Lot>> = BTreeMap::new();
    let mut on_hand: BTreeMap<String, f64> = BTreeMap::new();
    for row in &rows {
        let (Some(so), Some(mo), Some(qty), Some(start), Some(end)) = (
            bound(row, "so"),
            bound(row, "mo"),
            number(row, "qty"),
            bound(row, "start"),
            bound(row, "end"),
        ) else {
            continue;
        };
        let lot = lots
            .entry(so.to_owned())
            .or_default()
            .entry(mo.to_owned())
            .or_insert_with(|| Lot {
                iri: mo.to_owned(),
                qty,
                start: start.to_owned(),
                end: bound(row, "woEnd").map_or_else(|| end.to_owned(), |wo| later(end, wo)),
                per_unit: BTreeMap::new(),
            });
        if let (Some(comp), Some(per_unit)) = (bound(row, "comp"), number(row, "perUnit")) {
            lot.per_unit.insert(comp.to_owned(), per_unit);
            on_hand.insert(comp.to_owned(), number(row, "onHand").unwrap_or(0.0));
        }
    }
    Ok((
        lots.into_iter()
            .map(|(so, lots)| (so, lots.into_values().collect()))
            .collect(),
        on_hand,
    ))
}

/// The open inbound of `components` under `source`, in arrival order per component.
async fn open_inbound<'a>(
    source: &dyn Projection,
    components: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, Vec<(f64, String)>>> {
    let mut inbound: BTreeMap<String, Vec<(f64, String)>> = BTreeMap::new();
    if let Some(query) = queries::coverage_query(components)? {
        for row in source.select(&query).await? {
            if let (Some(comp), Some(open), Some(arrival)) = (
                bound(&row, "comp"),
                number(&row, "open"),
                bound(&row, "arrival"),
            ) {
                inbound
                    .entry(comp.to_owned())
                    .or_default()
                    .push((open, arrival.to_owned()));
            }
        }
    }
    Ok(inbound)
}

/// How many orders a projection leaves late, how many units late, and how many days late. An
/// order fulfilled by manufacturing lots is late when its lots leave late units (judged against
/// the projection's own promise, as the scorer ranks: the promise a plan moves is ranked by the
/// scorer's promise field), and adds the whole days its latest lot completes after that
/// promise. Any other order is late when the risk rule's `rows` leave its demand uncovered, and
/// adds no days (nor does an order with an undated lot): both are already counted in the late
/// orders and units.
///
/// # Errors
/// A read error from the source.
pub(crate) async fn lateness(source: &dyn Projection, rows: &[Row]) -> Result<(usize, f64, i64)> {
    let (lots, on_hand) = order_lots(source, None).await?;
    let promises = promises(source).await?;
    let components: BTreeSet<&str> = lots
        .values()
        .flatten()
        .flat_map(|lot| lot.per_unit.keys().map(String::as_str))
        .collect();
    let inbound = open_inbound(source, components).await?;
    let mut late: BTreeSet<&str> = BTreeSet::new();
    let mut late_units = 0.0;
    let mut late_days = 0i64;
    for (so, order_lots) in &lots {
        let judged = promises
            .get(so)
            .and_then(|promise| lot_outcome(order_lots, &on_hand, &inbound, promise));
        match judged {
            Some(judged) if judged.late_units <= QTY_EPSILON => {}
            Some(judged) => {
                late.insert(so.as_str());
                late_units += judged.late_units;
                late_days = late_days.saturating_add(judged.late_days);
            }
            // An unreadable promise or date is never read as on time.
            None => {
                late.insert(so.as_str());
                late_units += order_lots.iter().map(|lot| lot.qty).sum::<f64>();
            }
        }
    }
    for row in rows.iter().filter(|row| late_by_demand(row)) {
        if let Some(so) = bound(row, "so").filter(|so| !lots.contains_key(*so)) {
            late.insert(so);
        }
    }
    Ok((late.len(), late_units, late_days))
}

/// Every sales order's promise under `source`, keyed by order IRI.
async fn promises(source: &dyn Projection) -> Result<BTreeMap<String, String>> {
    let rows = source.select(&queries::promises_query()).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                bound(row, "so")?.to_owned(),
                bound(row, "commitment")?.to_owned(),
            ))
        })
        .collect())
}

/// Judge an outcome from its completion and the original promise: `on_time` when the completion
/// is known and not after the promise, `late` when after it, `at_risk` when no completion is
/// known. `None` when a date cannot be read (never a guessed day count).
fn judge(original_promise: &str, completion: Option<String>) -> Option<Outcome> {
    let Some(completion) = completion else {
        return Some(Outcome {
            delivery: Delivery::AtRisk,
            completion: None,
            slip: None,
        });
    };
    let slip = Day::from_iso(&completion)?.days_since(Day::from_iso(original_promise)?);
    Some(Outcome {
        delivery: if slip > 0 {
            Delivery::Late
        } else {
            Delivery::OnTime
        },
        completion: Some(completion),
        slip: Some(slip),
    })
}

/// A sales order's delivery under `projected`, against its promise in `original`. `Ok(None)`
/// for an order with no (readable) promise: it has nothing to be late against.
async fn order_outcome(
    original: &dyn Projection,
    projected: &dyn Projection,
    order: &EntityUri,
) -> Result<Option<Outcome>> {
    let Some(original_promise) = promises(original).await?.remove(&order.0) else {
        return Ok(None);
    };
    // An order fulfilled by manufacturing lots is judged lot by lot.
    let (lots, on_hand) = order_lots(projected, Some(order)).await?;
    if let Some(order_lots) = lots.get(&order.0).filter(|lots| !lots.is_empty()) {
        let components: BTreeSet<&str> = order_lots
            .iter()
            .flat_map(|lot| lot.per_unit.keys().map(String::as_str))
            .collect();
        let inbound = open_inbound(projected, components).await?;
        // The order completes with its latest lot, so a late lot makes it late and a lot that
        // never gets its material leaves it at risk.
        return Ok(
            lot_outcome(order_lots, &on_hand, &inbound, &original_promise)
                .and_then(|judged| judge(&original_promise, judged.completion)),
        );
    }
    let projected_promise = promises(projected).await?.remove(&order.0);
    let rules = projected.select(RISK_RULE_SELECT_SPARQL).await?;
    let short = short_components(&rules, &order.0);

    let completion = if short.is_empty() {
        // Demand is covered at the (projected) promise: the order ships by it.
        projected_promise
    } else {
        let components: BTreeSet<&str> = short.iter().map(|c| c.comp.as_str()).collect();
        let inbound = open_inbound(projected, components).await?;
        // The latest coverage date over the short components; none if any is never covered.
        short
            .iter()
            .try_fold(None::<String>, |latest, component| {
                let (required, on_hand) = component.required_and_on_hand?;
                let lines = inbound.get(&component.comp).map_or(&[][..], Vec::as_slice);
                let covered = coverage_date(required, on_hand, lines)?;
                Some(Some(latest.map_or(covered.clone(), |l| l.max(covered))))
            })
            .flatten()
    };
    Ok(judge(&original_promise, completion))
}

/// A manufacturing order's delivery under `projected`: its completion is the latest end of its
/// work orders, measured against the earliest original promise of the sales orders it
/// fulfills. With no promised order to measure against, no slip is served and delivery falls
/// back to the contention judgment: `on_time` while no work order of it is over capacity, else
/// `at_risk`.
async fn process_outcome(
    original: &dyn Projection,
    projected: &dyn Projection,
    order: &EntityUri,
) -> Result<Option<Outcome>> {
    let rows = projected
        .select(&queries::process_completion_query(order)?)
        .await?;
    let completion = rows
        .iter()
        .find_map(|row| bound(row, "maxEnd"))
        .map(str::to_owned);
    let fulfilled: BTreeSet<&str> = rows.iter().filter_map(|row| bound(row, "so")).collect();
    let promises = promises(original).await?;
    let promise = fulfilled
        .iter()
        .filter_map(|so| promises.get(*so))
        .min_by_key(|p| Day::from_iso(p));

    if let (Some(promise), Some(end)) = (promise, &completion) {
        return Ok(judge(promise, Some(end.clone())));
    }
    let contended = projected
        .select(PROCESS_CONTENTION_SELECT_SPARQL)
        .await?
        .iter()
        .filter(|row| bound(row, "mo") == Some(order.0.as_str()))
        .any(|row| {
            let capacity = number(row, "capacity");
            let peers = bound(row, "overlapPeers").and_then(|p| p.parse::<i64>().ok());
            capacity
                .zip(peers)
                .and_then(|(c, p)| ontology::over_capacity(c, p))
                == Some(true)
        });
    Ok(Some(Outcome {
        delivery: if contended {
            Delivery::AtRisk
        } else {
            Delivery::OnTime
        },
        completion,
        slip: None,
    }))
}

/// A subject's delivery under `projected`, against its original promise in `original`: a sales
/// order by demand coverage, a manufacturing order by its schedule. `Ok(None)` for any other
/// subject, or one with nothing to be late against.
///
/// # Errors
/// A read error from either source, or a non-canonical IRI.
pub async fn outcome(
    original: &dyn Projection,
    projected: &dyn Projection,
    subject: &EntityUri,
) -> Result<Option<Outcome>> {
    match model_of(subject) {
        Some(SALES_ORDER) => order_outcome(original, projected, subject).await,
        Some(MANUFACTURING_ORDER) => process_outcome(original, projected, subject).await,
        _ => Ok(None),
    }
}

/// The sales order whose value and promise a subject stands for: a sales order itself, or the
/// (lowest-IRI) sales order a manufacturing order fulfills. `Ok(None)` for anything else.
///
/// # Errors
/// A read error, or a non-canonical IRI.
pub async fn order_of(source: &dyn Projection, subject: &EntityUri) -> Result<Option<EntityUri>> {
    match model_of(subject) {
        Some(SALES_ORDER) => Ok(ontology::canonical_subject(&subject.0)),
        Some(MANUFACTURING_ORDER) => {
            let rows = source
                .select(&queries::process_completion_query(subject)?)
                .await?;
            Ok(rows
                .iter()
                .filter_map(|row| bound(row, "so"))
                .min()
                .and_then(ontology::canonical_subject))
        }
        _ => Ok(None),
    }
}

/// A sales order's value and the currency it is recorded in, each `None` when unread.
///
/// # Errors
/// A read error, or a non-canonical IRI.
pub async fn order_value(
    source: &dyn Projection,
    order: &EntityUri,
) -> Result<(Option<Money>, Option<String>)> {
    let rows = source.select(&queries::order_value_query(order)?).await?;
    let row = rows.first();
    Ok((
        row.and_then(|r| bound(r, "value")).and_then(Money::parse),
        row.and_then(|r| bound(r, "currency")).map(str::to_owned),
    ))
}

/// The capability's plan framing for `subject` under `projected`: `delivery`,
/// `projected_completion` and `day_slip` against the original promise in `original`, and the
/// fulfilled order's `order_value` with its `currency`. The baseline passes the current world as
/// both sources. Display-only: a key whose read fails is omitted, and nothing here ranks.
pub async fn plan_framing(
    original: &dyn Projection,
    projected: &dyn Projection,
    subject: &EntityUri,
) -> BTreeMap<String, String> {
    let mut framing = BTreeMap::new();
    if let Ok(Some(judged)) = outcome(original, projected, subject).await {
        framing.insert(
            super::DELIVERY_KEY.to_owned(),
            judged.delivery.as_str().to_owned(),
        );
        if let Some(completion) = judged.completion {
            framing.insert(super::PROJECTED_COMPLETION_KEY.to_owned(), completion);
        }
        if let Some(slip) = judged.slip {
            framing.insert(super::DAY_SLIP_KEY.to_owned(), slip.to_string());
        }
    }
    framing.extend(order_framing(original, subject).await);
    framing
}

/// The subject's order `order_value` with its `currency`, read from the current world `source`:
/// the part of the framing that needs no projection. Omitted when unread.
pub async fn order_framing(
    source: &dyn Projection,
    subject: &EntityUri,
) -> BTreeMap<String, String> {
    let mut framing = BTreeMap::new();
    if let Ok(Some(order)) = order_of(source, subject).await
        && let Ok((value, currency)) = order_value(source, &order).await
    {
        if let Some(value) = value {
            framing.insert(super::ORDER_VALUE_KEY.to_owned(), value.render());
        }
        if let Some(currency) = currency {
            framing.insert(super::CURRENCY_KEY.to_owned(), currency);
        }
    }
    framing
}

/// A plan's display cost: the exact sum of the costs its purchase targets touch, with the
/// touched purchase orders' currency as `cost_currency`. The cost is omitted when a value is
/// unreadable or the orders are in different currencies; the currency alone is omitted when a
/// target's currency is unread. `costs` are the `(target, cost)` pairs the policy context
/// gathered, the same cost the auto-apply predicate judges.
pub async fn plan_cost(
    world: &dyn WorldModel,
    costs: &[(EntityUri, String)],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(total) = money::sum(costs.iter().map(|(_, cost)| cost.as_str())) else {
        return out;
    };
    let mut currencies = BTreeSet::new();
    let mut unread = false;
    for (target, _) in costs {
        let currency = match queries::purchase_currency_query(target) {
            Ok(query) => world.select(&query).await.ok().and_then(|rows| {
                rows.first()
                    .and_then(|row| bound(row, "currency"))
                    .map(str::to_owned)
            }),
            Err(_) => None,
        };
        match currency {
            Some(currency) => {
                currencies.insert(currency);
            }
            None => unread = true,
        }
    }
    if currencies.len() > 1 {
        return out;
    }
    out.insert(super::COST_KEY.to_owned(), total.render());
    if let (false, Some(currency)) = (unread, currencies.pop_first()) {
        out.insert(super::COST_CURRENCY_KEY.to_owned(), currency);
    }
    out
}

/// A plan's cost as a rank-key field: per action, a purchase line target adds its own
/// `ekg:unit_price` and a purchase order target its lines' summed prices (the costs the policy
/// context gathers and [`plan_cost`] serves), read from `projection`, which carries the base
/// facts. Summed as decimals and rendered with two decimals, zero-padded to a fixed width. A
/// price that cannot be read as money (or a total that does not fit) gives the all-9s field, so
/// the plan sorts last among otherwise-equal plans. A target that touches no purchase order
/// adds nothing.
///
/// # Errors
/// A read error from the projection.
pub async fn plan_cost_key(projection: &dyn Projection, plan: &Plan) -> Result<String> {
    let unreadable = || "9".repeat(COST_KEY_WIDTH);
    let mut total = Money::default();
    for action in &plan.actions {
        if !matches!(
            model_of(&action.target),
            Some(PURCHASE_ORDER | PURCHASE_ORDER_LINE)
        ) {
            continue;
        }
        // An AI-generated target is rebuilt canonically, never interpolated as given.
        let Some(target) = ontology::canonical_subject(&action.target.0) else {
            continue;
        };
        let rows = projection
            .select(&queries::purchase_cost_query(&target)?)
            .await?;
        let Some(cost) = rows.first().and_then(|row| bound(row, "cost")) else {
            continue;
        };
        match Money::parse(cost).and_then(|cost| total.checked_add(cost)) {
            Some(sum) => total = sum,
            None => return Ok(unreadable()),
        }
    }
    let rendered = total.render();
    if total < Money::default() || rendered.len() > COST_KEY_WIDTH {
        return Ok(unreadable());
    }
    Ok(format!("{rendered:0>COST_KEY_WIDTH$}"))
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    /// A canned `select` source answering by query shape (the delivery judgment's reads only).
    #[derive(Default, Clone)]
    pub(crate) struct Canned {
        pub risk: Vec<Row>,
        pub promises: Vec<(String, String)>,
        pub inbound: Vec<(String, f64, String)>,
        pub process: Vec<Row>,
        pub contention: Vec<Row>,
        pub values: BTreeMap<String, (String, String)>,
        pub lots: Vec<Row>,
        /// Per purchase target IRI, its cost literal.
        pub costs: BTreeMap<String, String>,
    }

    #[async_trait]
    impl Projection for Canned {
        async fn select(&self, sparql: &str) -> Result<Vec<Row>> {
            let row = |pairs: &[(&str, &str)]| -> Row {
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect()
            };
            if sparql == RISK_RULE_SELECT_SPARQL {
                return Ok(self.risk.clone());
            }
            if sparql == PROCESS_CONTENTION_SELECT_SPARQL {
                return Ok(self.contention.clone());
            }
            if sparql == queries::promises_query() {
                return Ok(self
                    .promises
                    .iter()
                    .map(|(so, c)| row(&[("so", so), ("commitment", c)]))
                    .collect());
            }
            if sparql.contains("SELECT ?comp ?open ?arrival") {
                return Ok(self
                    .inbound
                    .iter()
                    .filter(|(comp, _, _)| sparql.contains(&format!("<{comp}>")))
                    .map(|(comp, open, arrival)| {
                        row(&[
                            ("comp", comp),
                            ("open", &open.to_string()),
                            ("arrival", arrival),
                        ])
                    })
                    .collect());
            }
            if sparql.contains("SELECT ?so ?mo ?qty ?start ?end ?comp ?perUnit ?onHand ?woEnd") {
                return Ok(self
                    .lots
                    .iter()
                    .filter(|lot| {
                        !sparql.contains("VALUES ?so")
                            || lot
                                .get("so")
                                .is_some_and(|so| sparql.contains(&format!("<{so}>")))
                    })
                    .cloned()
                    .collect());
            }
            if sparql.contains("(SUM(?price) AS ?cost)") {
                return Ok(self
                    .costs
                    .iter()
                    .filter(|(target, _)| sparql.contains(&format!("<{target}> ekg:unit_price")))
                    .map(|(_, cost)| row(&[("cost", cost)]))
                    .collect());
            }
            if sparql.contains("SELECT ?maxEnd ?so") {
                return Ok(self.process.clone());
            }
            if sparql.contains("SELECT ?value ?currency") {
                return Ok(self
                    .values
                    .iter()
                    .filter(|(so, _)| sparql.contains(&format!("<{so}>")))
                    .map(|(_, (v, c))| row(&[("value", v), ("currency", c)]))
                    .collect());
            }
            Ok(Vec::new())
        }
    }

    pub(crate) fn so(id: i64) -> String {
        ontology::entity_iri("sale.order", id)
    }

    pub(crate) fn comp(id: i64) -> String {
        ontology::entity_iri("product.product", id)
    }

    /// A rule row: `order` short of `component` with the given quantities; the rule's shortfall
    /// is `required + safety - (on hand + inbound by the promise)`.
    pub(crate) fn rule_row(
        order: &str,
        component: &str,
        required: f64,
        on_hand: f64,
        safety: f64,
        inbound_by_promise: f64,
    ) -> Row {
        let shortfall = required + safety - (on_hand + inbound_by_promise);
        BTreeMap::from([
            ("so".to_owned(), order.to_owned()),
            ("comp".to_owned(), component.to_owned()),
            ("required".to_owned(), required.to_string()),
            ("oh".to_owned(), on_hand.to_string()),
            ("ss".to_owned(), safety.to_string()),
            ("shortfall".to_owned(), shortfall.to_string()),
        ])
    }

    const PROMISE: &str = "2027-10-22T00:00:00";

    /// The seeded flagship shape: demand 10 of the battery, 2 on hand, safety 5, a line of 10
    /// arriving 9 Nov (after the 16 Oct promise).
    fn flagship() -> Canned {
        Canned {
            risk: vec![rule_row(&so(1), &comp(4), 10.0, 2.0, 5.0, 0.0)],
            promises: vec![(so(1), PROMISE.to_owned())],
            inbound: vec![(comp(4), 10.0, "2027-11-15T08:00:00".to_owned())],
            ..Canned::default()
        }
    }

    async fn judged(original: &Canned, projected: &Canned) -> Outcome {
        outcome(original, projected, &EntityUri(so(1)))
            .await
            .expect("reads")
            .expect("a promised order")
    }

    #[tokio::test]
    async fn doing_nothing_is_late_until_the_inbound_covers_the_demand() {
        let world = flagship();
        let o = judged(&world, &world).await;
        assert_eq!(o.delivery, Delivery::Late);
        assert_eq!(o.completion.as_deref(), Some("2027-11-15T08:00:00"));
        assert_eq!(o.slip, Some(24), "16 Oct to 9 Nov");
    }

    #[tokio::test]
    async fn an_expedite_covering_the_demand_is_on_time_while_safety_stays_unmet() {
        // The line arrives 6 Oct, before the promise: the rule still flags the safety breach
        // (shortfall 3, within safety 5), but demand is covered.
        let world = flagship();
        let projected = Canned {
            risk: vec![rule_row(&so(1), &comp(4), 10.0, 2.0, 5.0, 10.0)],
            inbound: vec![(comp(4), 10.0, "2027-10-12T08:00:00".to_owned())],
            ..flagship()
        };
        let o = judged(&world, &projected).await;
        assert_eq!(o.delivery, Delivery::OnTime);
        assert_eq!(o.completion.as_deref(), Some(PROMISE));
        assert_eq!(o.slip, Some(0));
    }

    #[tokio::test]
    async fn a_re_promise_is_late_by_the_days_it_moves_the_promise() {
        // Re-promised from 16 Oct to 10 Nov: the inbound (9 Nov) covers it, so the rule clears.
        let world = flagship();
        let projected = Canned {
            risk: Vec::new(),
            promises: vec![(so(1), "2027-11-16T00:00:00".to_owned())],
            ..flagship()
        };
        let o = judged(&world, &projected).await;
        assert_eq!(o.delivery, Delivery::Late);
        assert_eq!(o.completion.as_deref(), Some("2027-11-16T00:00:00"));
        assert_eq!(o.slip, Some(25));
    }

    #[tokio::test]
    async fn uncoverable_demand_is_at_risk_with_no_day_count() {
        // A made sub-assembly with no inbound, next to the battery.
        let world = Canned {
            risk: vec![
                rule_row(&so(1), &comp(4), 10.0, 2.0, 5.0, 0.0),
                rule_row(&so(1), &comp(9), 10.0, 0.0, 0.0, 0.0),
            ],
            ..flagship()
        };
        let o = judged(&world, &world).await;
        assert_eq!(o.delivery, Delivery::AtRisk);
        assert_eq!(
            (o.completion, o.slip),
            (None, None),
            "never a fabricated date"
        );
    }

    #[tokio::test]
    async fn the_coverage_date_is_the_first_cumulative_arrival_that_covers() {
        // 2 on hand + 3 (1 Nov) = 5, short of 10; + 6 (20 Nov) = 11, so covered 20 Nov.
        let lines = vec![
            (6.0, "2027-11-26T00:00:00".to_owned()),
            (3.0, "2027-11-07T00:00:00".to_owned()),
        ];
        assert_eq!(
            coverage_date(10.0, 2.0, &lines).as_deref(),
            Some("2027-11-26T00:00:00")
        );
        assert_eq!(
            coverage_date(10.0, 2.0, &lines[1..]),
            None,
            "5 never covers 10"
        );
    }

    #[tokio::test]
    async fn an_mo_is_measured_against_its_fulfilled_orders_promise() {
        let mo = EntityUri(ontology::entity_iri("mrp.production", 7));
        let process = |end: &str| Canned {
            promises: vec![(so(1), PROMISE.to_owned())],
            process: vec![BTreeMap::from([
                ("maxEnd".to_owned(), end.to_owned()),
                ("so".to_owned(), so(1)),
            ])],
            ..Canned::default()
        };
        let before = process("2027-10-21T16:00:00");
        let after = process("2027-10-24T12:00:00");
        let o = outcome(&before, &after, &mo)
            .await
            .expect("reads")
            .expect("an MO");
        assert_eq!(o.delivery, Delivery::Late);
        assert_eq!(o.slip, Some(2), "16 Oct to 18 Oct");
        assert_eq!(o.completion.as_deref(), Some("2027-10-24T12:00:00"));
        let o = outcome(&before, &before, &mo)
            .await
            .expect("reads")
            .expect("an MO");
        assert_eq!((o.delivery, o.slip), (Delivery::OnTime, Some(-1)));
    }

    #[tokio::test]
    async fn an_mo_with_no_promise_falls_back_to_contention_with_no_slip() {
        let mo = EntityUri(ontology::entity_iri("mrp.production", 7));
        let contended = Canned {
            process: vec![BTreeMap::from([(
                "maxEnd".to_owned(),
                "2027-10-24T12:00:00".to_owned(),
            )])],
            contention: vec![BTreeMap::from([
                ("mo".to_owned(), mo.0.clone()),
                ("capacity".to_owned(), "1".to_owned()),
                ("overlapPeers".to_owned(), "1".to_owned()),
            ])],
            ..Canned::default()
        };
        let o = outcome(&contended, &contended, &mo)
            .await
            .expect("reads")
            .expect("an MO");
        assert_eq!((o.delivery, o.slip), (Delivery::AtRisk, None));
        let cleared = Canned {
            contention: Vec::new(),
            ..contended.clone()
        };
        let o = outcome(&contended, &cleared, &mo)
            .await
            .expect("reads")
            .expect("an MO");
        assert_eq!((o.delivery, o.slip), (Delivery::OnTime, None));
    }

    #[tokio::test]
    async fn the_framing_serves_the_order_value_in_its_currency() {
        let world = Canned {
            values: BTreeMap::from([(so(1), ("20700.1".to_owned(), "USD".to_owned()))]),
            ..flagship()
        };
        let framing = plan_framing(&world, &world, &EntityUri(so(1))).await;
        let get = |k: &str| framing.get(k).map(String::as_str);
        assert_eq!(get("delivery"), Some("late"));
        assert_eq!(get("day_slip"), Some("24"));
        assert_eq!(get("projected_completion"), Some("2027-11-15T08:00:00"));
        assert_eq!(get("order_value"), Some("20700.10"));
        assert_eq!(get("currency"), Some("USD"));
    }

    #[tokio::test]
    async fn late_orders_count_distinct_orders_short_by_demand() {
        let rows = vec![
            rule_row(&so(1), &comp(4), 10.0, 2.0, 5.0, 0.0), // late
            rule_row(&so(1), &comp(5), 10.0, 2.0, 5.0, 0.0), // same order
            rule_row(&so(2), &comp(4), 10.0, 2.0, 5.0, 10.0), // safety only
        ];
        // No order has lots, so every order is judged by demand.
        let (late, late_units, late_days) =
            lateness(&Canned::default(), &rows).await.expect("reads");
        assert_eq!(late, 1);
        assert!(late_units.abs() < QTY_EPSILON);
        assert_eq!(late_days, 0, "an order judged by demand adds no days");
    }

    #[tokio::test]
    async fn an_order_with_lots_is_late_by_its_lots_not_its_demand_rows() {
        let world = Canned {
            promises: vec![(so(1), LOT_PROMISE.to_owned())],
            lots: vec![
                lot_row(
                    &so(1),
                    1,
                    "30",
                    "2027-10-21T08:00:00",
                    "2027-10-22T08:00:00",
                ),
                lot_row(
                    &so(1),
                    2,
                    "10",
                    "2027-10-24T08:00:00",
                    "2027-10-25T08:00:00",
                ),
            ],
            inbound: vec![(comp(4), 10.0, "2027-10-27T08:00:00".to_owned())],
            ..Canned::default()
        };
        // The demand row would count so/1 once; its lots leave 10 late units.
        let rows = vec![rule_row(&so(1), &comp(4), 40.0, 30.0, 0.0, 0.0)];
        let (late, late_units, late_days) = lateness(&world, &rows).await.expect("reads");
        assert_eq!(late, 1);
        assert!((late_units - 10.0).abs() < QTY_EPSILON, "{late_units}");
        assert_eq!(late_days, 2, "the 10 complete 22 Oct, 2 days after 20 Oct");
    }

    // --- lot-level delivery ---

    const LOT_PROMISE: &str = "2027-10-26T00:00:00";

    fn lot(id: i64, qty: f64, start: &str, end: &str) -> Lot {
        Lot {
            iri: ontology::entity_iri("mrp.production", id),
            qty,
            start: start.to_owned(),
            end: end.to_owned(),
            per_unit: BTreeMap::from([(comp(4), 1.0)]),
        }
    }

    /// 30 controllers on hand; the other units arrive per `arrivals`.
    fn judge_lots(lots: &[Lot], arrivals: &[(f64, &str)]) -> LotJudgment {
        let on_hand = BTreeMap::from([(comp(4), 30.0)]);
        let inbound = BTreeMap::from([(
            comp(4),
            arrivals
                .iter()
                .map(|(q, a)| (*q, (*a).to_owned()))
                .collect::<Vec<_>>(),
        )]);
        lot_outcome(lots, &on_hand, &inbound, LOT_PROMISE).expect("readable")
    }

    #[test]
    fn one_lot_waiting_for_material_makes_the_whole_order_late() {
        let judged = judge_lots(
            &[lot(1, 40.0, "2027-10-22T08:00:00", "2027-10-23T08:00:00")],
            &[(10.0, "2027-10-27T08:00:00")],
        );
        assert_eq!(judged.completion.as_deref(), Some("2027-10-28T08:00:00"));
        assert!((judged.late_units - 40.0).abs() < QTY_EPSILON, "{judged:?}");
        let outcome = judge(LOT_PROMISE, judged.completion).expect("judged");
        assert_eq!(outcome.delivery, Delivery::Late);
        assert_eq!(outcome.slip, Some(2));
    }

    #[test]
    fn a_split_lot_with_an_expedite_delivers_every_unit_on_time() {
        let lots = [
            lot(1, 30.0, "2027-10-21T08:00:00", "2027-10-22T08:00:00"),
            lot(2, 10.0, "2027-10-24T08:00:00", "2027-10-25T08:00:00"),
        ];
        let judged = judge_lots(&lots, &[(10.0, "2027-10-24T08:00:00")]);
        assert_eq!(judged.completion.as_deref(), Some("2027-10-25T08:00:00"));
        assert!(judged.late_units.abs() < QTY_EPSILON, "{judged:?}");
        let outcome = judge(LOT_PROMISE, judged.completion).expect("judged");
        assert_eq!(outcome.delivery, Delivery::OnTime);
    }

    #[test]
    fn a_split_alone_delivers_the_covered_part_on_time() {
        let lots = [
            lot(1, 30.0, "2027-10-21T08:00:00", "2027-10-22T08:00:00"),
            lot(2, 10.0, "2027-10-24T08:00:00", "2027-10-25T08:00:00"),
        ];
        let judged = judge_lots(&lots, &[(10.0, "2027-10-27T08:00:00")]);
        assert_eq!(judged.completion.as_deref(), Some("2027-10-28T08:00:00"));
        assert!((judged.late_units - 10.0).abs() < QTY_EPSILON, "{judged:?}");
        let outcome = judge(LOT_PROMISE, judged.completion).expect("judged");
        assert_eq!(outcome.delivery, Delivery::Late);
        assert_eq!(outcome.slip, Some(2));
    }

    #[test]
    fn on_hand_alone_covering_the_lots_is_ready_and_never_covered_is_at_risk() {
        let ready = judge_lots(
            &[lot(1, 30.0, "2027-10-22T08:00:00", "2027-10-23T08:00:00")],
            &[],
        );
        assert_eq!(ready.completion.as_deref(), Some("2027-10-23T08:00:00"));
        assert!(ready.late_units.abs() < QTY_EPSILON);
        let never = judge_lots(
            &[lot(1, 40.0, "2027-10-22T08:00:00", "2027-10-23T08:00:00")],
            &[(5.0, "2027-10-24T08:00:00")],
        );
        assert_eq!(never.completion, None);
        assert!((never.late_units - 40.0).abs() < QTY_EPSILON);
        assert_eq!(
            judge(LOT_PROMISE, never.completion).map(|o| o.delivery),
            Some(Delivery::AtRisk)
        );
    }

    #[test]
    fn lots_are_allocated_in_start_order() {
        // Listed late-first: the earlier lot (by start) still gets the stock on hand.
        let lots = [
            lot(2, 10.0, "2027-10-24T08:00:00", "2027-10-25T08:00:00"),
            lot(1, 30.0, "2027-10-21T08:00:00", "2027-10-22T08:00:00"),
        ];
        let judged = judge_lots(&lots, &[(10.0, "2027-10-27T08:00:00")]);
        assert!(
            (judged.late_units - 10.0).abs() < QTY_EPSILON,
            "the later lot of 10 waits"
        );
    }

    fn lot_row(so: &str, mo: i64, qty: &str, start: &str, end: &str) -> Row {
        BTreeMap::from([
            ("so".to_owned(), so.to_owned()),
            ("mo".to_owned(), ontology::entity_iri("mrp.production", mo)),
            ("qty".to_owned(), qty.to_owned()),
            ("start".to_owned(), start.to_owned()),
            ("end".to_owned(), end.to_owned()),
            ("comp".to_owned(), comp(4)),
            ("perUnit".to_owned(), "1".to_owned()),
            ("onHand".to_owned(), "30".to_owned()),
        ])
    }

    #[tokio::test]
    async fn an_order_with_lots_is_judged_lot_by_lot() {
        let world = Canned {
            promises: vec![(so(1), LOT_PROMISE.to_owned())],
            lots: vec![lot_row(
                &so(1),
                1,
                "40",
                "2027-10-22T08:00:00",
                "2027-10-23T08:00:00",
            )],
            inbound: vec![(comp(4), 10.0, "2027-10-27T08:00:00".to_owned())],
            ..Canned::default()
        };
        let judged = judged(&world, &world).await;
        assert_eq!(judged.delivery, Delivery::Late);
        assert_eq!(judged.completion.as_deref(), Some("2027-10-28T08:00:00"));
        assert_eq!(judged.slip, Some(2));
    }

    // --- work orders move the lot ---

    /// The seeded rush order: promised 30 Sep, one lot of 5 whose order finishes 1 Oct 12:00,
    /// with its material on hand; `wo_end` is its latest work-order end, if it has any.
    fn rush(wo_end: Option<&str>) -> Canned {
        let mut row = lot_row(&so(3), 3, "5", "2027-10-06T08:00:00", "2027-10-07T12:00:00");
        if let Some(end) = wo_end {
            row.insert("woEnd".to_owned(), end.to_owned());
        }
        Canned {
            promises: vec![(so(3), "2027-10-06T00:00:00".to_owned())],
            lots: vec![row],
            ..Canned::default()
        }
    }

    #[tokio::test]
    async fn a_work_order_moved_past_the_orders_finish_makes_the_lot_late() {
        let world = rush(Some("2027-10-07T12:00:00"));
        let moved = rush(Some("2027-10-10T12:00:00"));
        let order = EntityUri(so(3));
        let judge = |projected: Canned| {
            let world = world.clone();
            let order = order.clone();
            async move {
                outcome(&world, &projected, &order)
                    .await
                    .expect("reads")
                    .expect("a promised order")
            }
        };
        let baseline = judge(world.clone()).await;
        assert_eq!(
            (baseline.delivery, baseline.slip),
            (Delivery::Late, Some(1))
        );
        let rescheduled = judge(moved.clone()).await;
        assert_eq!(rescheduled.delivery, Delivery::Late);
        assert_eq!(
            rescheduled.slip,
            Some(4),
            "the lot completes with its work order"
        );
        assert_eq!(
            rescheduled.completion.as_deref(),
            Some("2027-10-10T12:00:00")
        );
        let (late, late_units, late_days) = lateness(&moved, &[]).await.expect("reads");
        assert_eq!(late, 1);
        assert!((late_units - 5.0).abs() < QTY_EPSILON, "{late_units}");
        assert_eq!(late_days, 4);
        let (_, _, baseline_days) = lateness(&world, &[]).await.expect("reads");
        assert_eq!(baseline_days, 1);
    }

    #[tokio::test]
    async fn an_order_without_work_orders_keeps_its_own_finish() {
        let o = judged_rush(rush(None)).await;
        assert_eq!(o.completion.as_deref(), Some("2027-10-07T12:00:00"));
        assert_eq!(o.slip, Some(1));
        // A work order ending before the order's own finish never moves the lot earlier.
        let o = judged_rush(rush(Some("2027-10-06T16:00:00"))).await;
        assert_eq!(o.completion.as_deref(), Some("2027-10-07T12:00:00"));
    }

    async fn judged_rush(world: Canned) -> Outcome {
        outcome(&world, &world, &EntityUri(so(3)))
            .await
            .expect("reads")
            .expect("a promised order")
    }

    #[test]
    fn an_undated_lot_adds_no_days() {
        let never = judge_lots(
            &[lot(1, 40.0, "2027-10-22T08:00:00", "2027-10-23T08:00:00")],
            &[(5.0, "2027-10-24T08:00:00")],
        );
        assert_eq!(never.completion, None);
        assert_eq!(never.late_days, 0);
        let on_time = judge_lots(
            &[lot(1, 30.0, "2027-10-22T08:00:00", "2027-10-23T08:00:00")],
            &[],
        );
        assert_eq!(
            on_time.late_days, 0,
            "an early completion is never negative days"
        );
    }

    // --- the plan's cost as a rank field ---

    fn action(target: String) -> ekg_domain::ProposedAction {
        ekg_domain::ProposedAction {
            action_type: ekg_domain::ActionType("expedite_purchase_line".to_owned()),
            target: EntityUri(target),
            params: BTreeMap::new(),
        }
    }

    fn plan_of(targets: &[String]) -> Plan {
        Plan {
            actions: targets.iter().cloned().map(action).collect(),
        }
    }

    fn line(id: i64) -> String {
        ontology::entity_iri("purchase.order.line", id)
    }

    #[tokio::test]
    async fn the_cost_key_of_the_seeded_expedite_is_its_served_cost() {
        let world = Canned {
            costs: BTreeMap::from([(line(7), "320.0".to_owned())]),
            ..Canned::default()
        };
        let key = plan_cost_key(&world, &plan_of(&[line(7)]))
            .await
            .expect("reads");
        assert_eq!(key, "0000000000320.00");
        // The served cost sums the same literal the policy context gathers.
        let served = money::sum(["320.0"]).expect("money").render();
        assert_eq!(key.trim_start_matches('0'), served);
    }

    #[tokio::test]
    async fn the_cost_key_sums_purchase_targets_and_ignores_the_rest() {
        let order = ontology::entity_iri("purchase.order", 2);
        let world = Canned {
            costs: BTreeMap::from([
                (line(7), "320.0".to_owned()),
                (order.clone(), "15.5".to_owned()),
            ]),
            ..Canned::default()
        };
        let work_order = ontology::entity_iri("mrp.workorder", 3);
        let key = plan_cost_key(&world, &plan_of(&[line(7), order, work_order]))
            .await
            .expect("reads");
        assert_eq!(key, "0000000000335.50");
        let free = plan_cost_key(&world, &plan_of(&[])).await.expect("reads");
        assert_eq!(free, "0000000000000.00");
        assert!(free < key, "a free plan sorts first");
    }

    #[tokio::test]
    async fn an_unreadable_price_gives_the_last_cost_key() {
        let world = Canned {
            costs: BTreeMap::from([(line(7), "3.2E2".to_owned())]),
            ..Canned::default()
        };
        let key = plan_cost_key(&world, &plan_of(&[line(7)]))
            .await
            .expect("reads");
        assert_eq!(key, "9".repeat(16));
        assert!(key.as_str() > "9999999999999.99", "after any readable cost");
    }
}
