//! The plant-wide cause map: which nodes cause each flagged subject, read from the capability's
//! own rule SELECTs and interpreted by the same predicates the monitor's interpreters use, so
//! causes cannot drift from the verdicts.
//!
//! - Every supply-risk row ([`ontology::RISK_RULE_SELECT_SPARQL`]) maps its sales order `?so` to
//!   its short component `?comp`, in the rule's own cause order (its `ORDER BY`: purchased
//!   before made, the largest shortfall, the IRI). The first is the point's primary cause:
//!   the component the chain lights, and the one the monitor's reason names when a point
//!   opens (a live point keeps the reason it opened with; the chain follows the current rows).
//! - Every contention row ([`ontology::PROCESS_CONTENTION_SELECT_SPARQL`]) that passes the
//!   shared [`ontology::over_capacity`] predicate maps its manufacturing order `?mo` to the
//!   contended work order `?wo`.
//! - An infeasible order has no cause node (it is its own cause).
//!
//! Capability knowledge, so it lives here; the dynamic layer only decides the at-risk set.

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{EntityUri, Result, WorldModel};

use super::{Row, queries};
use crate::ontology;

/// One flagged subject's causes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Causes {
    /// The short components, each once, in the rule's cause order (the order its rows arrive
    /// in: purchased before made, the largest shortfall, the IRI). Both the `cause` markers
    /// and the chain read this list. The first is the primary cause: the chain lights it and
    /// a newly opened point carries its reason. No second ranking is kept here.
    pub ranked: Vec<String>,
    /// Contended work orders (over their work center's capacity).
    pub work_orders: BTreeSet<String>,
}

impl Causes {
    /// Whether `iri` is one of these causes (a short component or a contended work order).
    #[must_use]
    pub fn contains(&self, iri: &str) -> bool {
        self.ranked.iter().any(|component| component == iri) || self.work_orders.contains(iri)
    }
}

/// Each subject's causes, plant-wide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CauseMap(BTreeMap<String, Causes>);

impl CauseMap {
    /// The causes of `subject`, if any rule row names it.
    #[must_use]
    pub fn of(&self, subject: &str) -> Option<&Causes> {
        self.0.get(subject)
    }

    /// Every subject with its causes, in IRI order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Causes)> {
        self.0.iter()
    }

    /// Build the map from the supply-risk SELECT rows (in the order the rule returns them) and
    /// the contention SELECT rows.
    #[must_use]
    pub fn from_rows(risk_rows: &[Row], contention_rows: &[Row]) -> Self {
        let mut map: BTreeMap<String, Causes> = BTreeMap::new();
        for row in risk_rows {
            let (Some(so), Some(comp)) = (row.get("so"), row.get("comp")) else {
                continue;
            };
            // Every returned row is already short (the rule filters shortfall > 0), and the
            // rows arrive in the rule's cause order: keep each component's first place.
            let causes = map.entry(so.clone()).or_default();
            if !causes.ranked.contains(comp) {
                causes.ranked.push(comp.clone());
            }
        }
        for row in contention_rows {
            let (Some(mo), Some(wo)) = (row.get("mo"), row.get("wo")) else {
                continue;
            };
            let capacity = row.get("capacity").and_then(|c| c.parse::<f64>().ok());
            let peers = row.get("overlapPeers").and_then(|p| p.parse::<i64>().ok());
            // The shared predicate the monitor's interpreter uses; no verdict, no cause.
            let over = capacity
                .zip(peers)
                .and_then(|(capacity, peers)| ontology::over_capacity(capacity, peers));
            if over == Some(true) {
                map.entry(mo.clone())
                    .or_default()
                    .work_orders
                    .insert(wo.clone());
            }
        }
        Self(map)
    }
}

/// Read the plant-wide cause map from the world model (the two rule SELECTs).
///
/// # Errors
/// Propagates a world-model query failure. The caller then omits `cause` (and `exposure`)
/// rather than denying a cause.
pub async fn cause_map(world: &dyn WorldModel) -> Result<CauseMap> {
    let risk = world.select(ontology::RISK_RULE_SELECT_SPARQL).await?;
    let contention = world
        .select(ontology::PROCESS_CONTENTION_SELECT_SPARQL)
        .await?;
    Ok(CauseMap::from_rows(&risk, &contention))
}

/// The entities of a supply-risk point's cause chain, the chain the tabs light, in order: its
/// short components (the rule's cause order), then per component each open inbound line, its
/// purchase order, the PO's vendor and the open shipments delivering it. Empty when no rule row
/// names `subject`. A read that only names entities: the proposer grounds each one under its
/// own read policy.
///
/// # Errors
/// Propagates a world-model query failure.
pub async fn cause_chain(world: &dyn WorldModel, subject: &EntityUri) -> Result<Vec<EntityUri>> {
    let risk = world.select(ontology::RISK_RULE_SELECT_SPARQL).await?;
    let map = CauseMap::from_rows(&risk, &[]);
    let Some(causes) = map.of(&subject.0) else {
        return Ok(Vec::new());
    };
    let Some(query) = queries::cause_chain_query(causes.ranked.iter().map(String::as_str))? else {
        return Ok(Vec::new());
    };
    let rows = world.select(&query).await?;
    Ok(chain_entities(&causes.ranked, &rows))
}

/// The entities a manufacturing-order point's process remediation may act on beyond its work
/// orders: the work centers its work orders run on, then those centers' capacity records, each
/// canonical entity IRI once, in query order. Empty for a subject with no work order. A read of
/// the world model; it commits nothing.
///
/// # Errors
/// A world-model read error, or a non-canonical subject IRI.
pub async fn process_context(
    world: &dyn WorldModel,
    subject: &EntityUri,
) -> Result<Vec<EntityUri>> {
    let rows = world
        .select(&queries::process_context_query(subject)?)
        .await?;
    let mut out: Vec<EntityUri> = Vec::new();
    for key in ["wc", "rec"] {
        for iri in rows.iter().filter_map(|row| row.get(key)) {
            if ontology::is_canonical_entity_iri(iri) && !out.iter().any(|e| e.0 == *iri) {
                out.push(EntityUri(iri.clone()));
            }
        }
    }
    Ok(out)
}

/// The model of a work centre's entity IRI.
const WORK_CENTRE_MODEL: &str = "mrp.workcenter";

/// Every work centre in the world model: canonical `mrp.workcenter` entity IRIs, each once,
/// ordered by IRI (as the query's `ORDER BY`, kept here too so the order never depends on the
/// store). Identifiers only, so a caller grounds each under its own read context. A row that is
/// not a canonical work-centre IRI is skipped. A read of the world model; it commits nothing.
///
/// # Errors
/// A world-model read error.
pub async fn work_centres(world: &dyn WorldModel) -> Result<Vec<EntityUri>> {
    let rows = world.select(&queries::work_centres_query()).await?;
    let mut out: Vec<EntityUri> = Vec::new();
    for iri in rows.iter().filter_map(|row| row.get("entity")) {
        if ontology::is_canonical_entity_iri(iri)
            && ekg_domain::entity_model(iri) == Some(WORK_CENTRE_MODEL)
            && !out.iter().any(|e| e.0 == *iri)
        {
            out.push(EntityUri(iri.clone()));
        }
    }
    out.sort();
    Ok(out)
}

/// The model of a work order's entity IRI.
const WORK_ORDER_MODEL: &str = "mrp.workorder";

/// The work orders scheduled on `work_centre`, of every order: canonical `mrp.workorder` entity
/// IRIs, each once, ordered by IRI. Identifiers only, so a caller grounds each under its own
/// read context. Empty for anything that is not a work centre. A read of the world model; it
/// commits nothing.
///
/// # Errors
/// A world-model read error, or a non-canonical IRI.
pub async fn work_centre_schedule(
    world: &dyn WorldModel,
    work_centre: &EntityUri,
) -> Result<Vec<EntityUri>> {
    if ekg_domain::entity_model(&work_centre.0) != Some(WORK_CENTRE_MODEL) {
        return Ok(Vec::new());
    }
    let rows = world
        .select(&queries::work_centre_schedule_query(work_centre)?)
        .await?;
    let mut out: Vec<EntityUri> = Vec::new();
    for iri in rows.iter().filter_map(|row| row.get("entity")) {
        if ontology::is_canonical_entity_iri(iri)
            && ekg_domain::entity_model(iri) == Some(WORK_ORDER_MODEL)
            && !out.iter().any(|e| e.0 == *iri)
        {
            out.push(EntityUri(iri.clone()));
        }
    }
    out.sort();
    Ok(out)
}

/// Order the cause-chain rows into entities: every component first (the rule's cause order),
/// then per component its lines' `line`, `po`, `vendor` and `ship`, each canonical entity
/// IRI once.
fn chain_entities(ranked: &[String], rows: &[Row]) -> Vec<EntityUri> {
    let mut out: Vec<EntityUri> = Vec::new();
    let mut push = |iri: &str| {
        if ontology::is_canonical_entity_iri(iri) && !out.iter().any(|e| e.0 == iri) {
            out.push(EntityUri(iri.to_owned()));
        }
    };
    for component in ranked {
        push(component);
    }
    for component in ranked {
        for row in rows
            .iter()
            .filter(|row| row.get("cause") == Some(component))
        {
            for key in ["line", "po", "vendor", "ship"] {
                if let Some(iri) = row.get(key) {
                    push(iri);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }

    #[test]
    fn supply_risk_rows_map_an_order_to_its_short_components() {
        let so = iri("sale.order", 1);
        let (battery, brake) = (iri("product.product", 4), iri("product.product", 9));
        let risk = vec![
            row(&[("so", &so), ("comp", &battery), ("shortfall", "13")]),
            row(&[("so", &so), ("comp", &brake), ("shortfall", "10.0")]),
            // A second row for the same (order, component) keeps its first place.
            row(&[("so", &so), ("comp", &battery), ("shortfall", "4")]),
        ];
        let map = CauseMap::from_rows(&risk, &[]);
        let causes = map.of(&so).expect("the order has causes");
        assert_eq!(
            causes.ranked,
            [battery.clone(), brake],
            "the rule's own order"
        );
        assert!(causes.work_orders.is_empty());
        assert!(causes.contains(&battery));
        assert!(
            map.of(&iri("sale.order", 2)).is_none(),
            "an unflagged order has none"
        );
    }

    #[test]
    fn the_causes_keep_the_rules_order_not_a_ranking_of_their_own() {
        // The rule's order: the purchased battery (short 13) before the made Drive
        // Subassembly (short 10). After the battery's fix the purchased battery (short 3)
        // still comes before the made Drive (short 10), though the Drive then has the larger
        // shortfall and the lower IRI.
        let so = iri("sale.order", 1);
        let (drive, battery) = (iri("product.product", 2), iri("product.product", 4));
        for (battery_short, drive_short) in [("13", "10"), ("3", "10")] {
            let risk = vec![
                row(&[
                    ("so", &so),
                    ("comp", &battery),
                    ("shortfall", battery_short),
                ]),
                row(&[("so", &so), ("comp", &drive), ("shortfall", drive_short)]),
            ];
            let map = CauseMap::from_rows(&risk, &[]);
            let causes = map.of(&so).expect("the order has causes");
            assert_eq!(causes.ranked, [battery.clone(), drive.clone()]);
        }
    }

    #[test]
    fn contention_rows_over_capacity_map_an_mo_to_its_contended_work_orders() {
        let mo = iri("mrp.production", 3);
        let (wo1, wo2) = (iri("mrp.workorder", 1), iri("mrp.workorder", 2));
        let contention = vec![
            // capacity 1, one peer: concurrency 2 > 1, over capacity.
            row(&[
                ("mo", &mo),
                ("wo", &wo1),
                ("capacity", "1"),
                ("overlapPeers", "1"),
            ]),
            row(&[
                ("mo", &mo),
                ("wo", &wo2),
                ("capacity", "1.0"),
                ("overlapPeers", "2"),
            ]),
        ];
        let map = CauseMap::from_rows(&[], &contention);
        let causes = map.of(&mo).expect("the contended MO has causes");
        assert_eq!(causes.work_orders, BTreeSet::from([wo1, wo2]));
        assert!(causes.ranked.is_empty());
    }

    #[test]
    fn work_within_capacity_is_no_cause() {
        let mo = iri("mrp.production", 3);
        // capacity 2, one peer: concurrency 2, within capacity. The agent runs both.
        let contention = vec![
            row(&[
                ("mo", &mo),
                ("wo", &iri("mrp.workorder", 1)),
                ("capacity", "2"),
                ("overlapPeers", "1"),
            ]),
            // An unreadable capacity yields no verdict, so no cause either.
            row(&[
                ("mo", &mo),
                ("wo", &iri("mrp.workorder", 2)),
                ("capacity", "n/a"),
                ("overlapPeers", "5"),
            ]),
        ];
        let map = CauseMap::from_rows(&[], &contention);
        assert!(map.of(&mo).is_none(), "within capacity, so no cause node");
        assert_eq!(map, CauseMap::default());
    }

    #[test]
    fn the_cause_chain_lists_components_then_each_ones_inbound_entities() {
        let (battery, drive) = (iri("product.product", 4), iri("product.product", 7));
        let (line, po, vendor, ship) = (
            iri("purchase.order.line", 11),
            iri("purchase.order", 10),
            iri("res.partner", 3),
            iri("stock.picking", 12),
        );
        let rows = vec![
            row(&[
                ("cause", &battery),
                ("line", &line),
                ("po", &po),
                ("vendor", &vendor),
                ("ship", &ship),
                ("arrival", "2027-11-15T08:00:00"),
            ]),
            // a literal or crafted value never becomes an entity
            row(&[("cause", &battery), ("line", "not an iri")]),
        ];
        let chain = chain_entities(&[battery.clone(), drive.clone()], &rows);
        let iris: Vec<&str> = chain.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            iris,
            [
                battery.as_str(),
                drive.as_str(),
                line.as_str(),
                po.as_str(),
                vendor.as_str(),
                ship.as_str()
            ]
        );
    }
}
