//! A plan option's scope: how many distinct orders, work centers and purchase orders its
//! changes touch, counted by a rule the operator can reconstruct over the world model, and a
//! multi-point plan's lead point, the one whose baseline frames it.
//!
//! The scope counts are served as neutral kind-and-count pairs whose kinds are data from a fixed
//! vocabulary ([`SCOPE_KINDS`]), never schema names. The rule:
//!
//! - order: a targeted sales or manufacturing order, and the manufacturing order a targeted
//!   work order is part of;
//! - work center: a targeted work center, the work center a targeted work order is performed
//!   by, and the work center a targeted capacity record belongs to;
//! - purchase order: a targeted purchase order, and the order a targeted line belongs to.
//!
//! A target of any other kind counts nowhere, each object counts once per kind, and a kind with
//! nothing to count is omitted. The scope is read, never estimated: a failed read omits it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{EntityUri, Result, WorldModel};

use super::queries;
use crate::ontology;

type Row = BTreeMap<String, String>;

/// The scope kind of a sales or manufacturing order.
pub const SCOPE_ORDER: &str = "order";
/// The scope kind of a work center.
pub const SCOPE_WORK_CENTER: &str = "work center";
/// The scope kind of a purchase order.
pub const SCOPE_PURCHASE_ORDER: &str = "purchase order";
/// Every scope kind, in the order a scope is served (and a row reads it).
pub const SCOPE_KINDS: [&str; 3] = [SCOPE_ORDER, SCOPE_WORK_CENTER, SCOPE_PURCHASE_ORDER];

/// One kind's count in a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCount {
    /// The kind, one of [`SCOPE_KINDS`].
    pub kind: &'static str,
    /// How many distinct objects of that kind the changes touch (never 0).
    pub count: usize,
}

fn bound<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

/// The objects one target-fact row stands for, as `(kind, object)` pairs, by the rule in the
/// module docs. An unknown class stands for nothing.
fn touched(row: &Row) -> Vec<(&'static str, String)> {
    let (Some(target), Some(class)) = (bound(row, "t"), bound(row, "class")) else {
        return Vec::new();
    };
    let own = |kind| vec![(kind, target.to_owned())];
    let linked = |kind, key| {
        bound(row, key)
            .map(|object| (kind, object.to_owned()))
            .into_iter()
            .collect::<Vec<_>>()
    };
    match class.strip_prefix(ontology::VOCAB_NAMESPACE) {
        Some("SalesOrder" | "ManufacturingOrder") => own(SCOPE_ORDER),
        Some("WorkOrder") => {
            let mut out = linked(SCOPE_ORDER, "order");
            out.extend(linked(SCOPE_WORK_CENTER, "workCenter"));
            out
        }
        Some("WorkCenter") => own(SCOPE_WORK_CENTER),
        Some("WorkCenterCapacity") => linked(SCOPE_WORK_CENTER, "capacityOf"),
        Some("PurchaseOrder") => own(SCOPE_PURCHASE_ORDER),
        Some("PurchaseOrderLine") => linked(SCOPE_PURCHASE_ORDER, "po"),
        _ => Vec::new(),
    }
}

/// Count the scope from the target-fact rows (the rule in the module docs): the distinct
/// objects per kind, in [`SCOPE_KINDS`] order, a kind with none omitted.
#[must_use]
pub fn scope_from_rows(rows: &[Row]) -> Vec<ScopeCount> {
    let mut objects: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    for (kind, object) in rows.iter().flat_map(touched) {
        objects.entry(kind).or_default().insert(object);
    }
    SCOPE_KINDS
        .iter()
        .filter_map(|kind| {
            let count = objects.get(kind).map_or(0, BTreeSet::len);
            (count > 0).then_some(ScopeCount { kind, count })
        })
        .collect()
}

/// A plan option's scope over the objects its changes `targets`, read from the world with one
/// SELECT. An empty target set has an empty scope.
///
/// # Errors
/// A read error, or a non-canonical target IRI.
pub async fn plan_scope<'a>(
    world: &dyn WorldModel,
    targets: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<ScopeCount>> {
    let Some(sparql) = queries::plan_scope_query(targets)? else {
        return Ok(Vec::new());
    };
    Ok(scope_from_rows(&world.select(&sparql).await?))
}

/// How bad a delivery outcome reads, worst first: late, at risk, on time, then unknown.
fn delivery_rank(framing: &BTreeMap<String, String>) -> u8 {
    match framing.get(super::DELIVERY_KEY).map(String::as_str) {
        Some("late") => 0,
        Some("at_risk") => 1,
        Some("on_time") => 2,
        _ => 3,
    }
}

/// A framing's known day slip, if any.
fn slip(framing: &BTreeMap<String, String>) -> Option<i64> {
    framing.get(super::DAY_SLIP_KEY)?.parse().ok()
}

/// Order two framings worst first: the larger known slip, a known slip before an unknown one,
/// then the worse delivery outcome.
fn worse_first(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> Ordering {
    match (slip(a), slip(b)) {
        (Some(x), Some(y)) if x != y => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => delivery_rank(a).cmp(&delivery_rank(b)),
    }
}

/// The lead point of a plan over its points' baseline framings: the point with the worst
/// baseline. That is the largest known day slip, with an unknown slip ranked below any known
/// one, then an at-risk delivery before an on-time one, then the point id. `None` for no
/// points.
#[must_use]
pub fn lead_member(framings: &[(EntityUri, BTreeMap<String, String>)]) -> Option<&EntityUri> {
    framings
        .iter()
        .min_by(|(sa, fa), (sb, fb)| worse_first(fa, fb).then_with(|| sa.cmp(sb)))
        .map(|(subject, _)| subject)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use async_trait::async_trait;
    use ekg_domain::{EngineError, Grounding, ReadContext, Revision, SourceRecord};

    use super::*;

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }

    fn class(name: &str) -> String {
        format!("{}{name}", ontology::VOCAB_NAMESPACE)
    }

    fn fact(target: &str, class_name: &str, links: &[(&str, &str)]) -> Row {
        let mut row = Row::from([
            ("t".to_owned(), target.to_owned()),
            ("class".to_owned(), class(class_name)),
        ]);
        for (key, value) in links {
            row.insert((*key).to_owned(), (*value).to_owned());
        }
        row
    }

    fn counts(scope: &[ScopeCount]) -> Vec<(&'static str, usize)> {
        scope.iter().map(|c| (c.kind, c.count)).collect()
    }

    /// A sample plan: split MO/1; move MO/2's work orders on Assembly and Paint; expedite one
    /// line of P/7.
    fn pl_0412() -> Vec<Row> {
        let (assembly, paint) = (iri("mrp.workcenter", 1), iri("mrp.workcenter", 2));
        let mo2 = iri("mrp.production", 2);
        vec![
            fact(&iri("mrp.production", 1), "ManufacturingOrder", &[]),
            fact(
                &iri("mrp.workorder", 21),
                "WorkOrder",
                &[("order", &mo2), ("workCenter", &assembly)],
            ),
            fact(
                &iri("mrp.workorder", 22),
                "WorkOrder",
                &[("order", &mo2), ("workCenter", &paint)],
            ),
            fact(
                &iri("purchase.order.line", 5),
                "PurchaseOrderLine",
                &[("po", &iri("purchase.order", 7))],
            ),
        ]
    }

    #[test]
    fn a_plan_touches_two_orders_two_work_centers_and_one_po() {
        assert_eq!(
            counts(&scope_from_rows(&pl_0412())),
            [
                (SCOPE_ORDER, 2),
                (SCOPE_WORK_CENTER, 2),
                (SCOPE_PURCHASE_ORDER, 1)
            ]
        );
    }

    #[test]
    fn two_changes_on_one_object_count_once() {
        let mo = iri("mrp.production", 1);
        let rows = vec![
            // Split the order, and move its own work order on the same work center twice.
            fact(&mo, "ManufacturingOrder", &[]),
            fact(
                &iri("mrp.workorder", 11),
                "WorkOrder",
                &[("order", &mo), ("workCenter", &iri("mrp.workcenter", 1))],
            ),
            fact(
                &iri("mrp.workorder", 12),
                "WorkOrder",
                &[("order", &mo), ("workCenter", &iri("mrp.workcenter", 1))],
            ),
            // Two lines of one purchase order, and the order itself.
            fact(
                &iri("purchase.order.line", 1),
                "PurchaseOrderLine",
                &[("po", &iri("purchase.order", 7))],
            ),
            fact(
                &iri("purchase.order.line", 2),
                "PurchaseOrderLine",
                &[("po", &iri("purchase.order", 7))],
            ),
            fact(&iri("purchase.order", 7), "PurchaseOrder", &[]),
            // A capacity record of the same work center.
            fact(
                &iri("mrp.workcenter.capacity", 3),
                "WorkCenterCapacity",
                &[("capacityOf", &iri("mrp.workcenter", 1))],
            ),
        ];
        assert_eq!(
            counts(&scope_from_rows(&rows)),
            [
                (SCOPE_ORDER, 1),
                (SCOPE_WORK_CENTER, 1),
                (SCOPE_PURCHASE_ORDER, 1)
            ]
        );
    }

    #[test]
    fn an_unknown_kind_is_not_counted_and_an_empty_kind_is_omitted() {
        let rows = vec![
            fact(&iri("res.partner", 8), "Party", &[]),
            fact(&iri("sale.order", 1), "SalesOrder", &[]),
            // A line whose order is not materialized counts nowhere, never guessed.
            fact(&iri("purchase.order.line", 1), "PurchaseOrderLine", &[]),
            // A row with no class counts nowhere.
            Row::from([("t".to_owned(), iri("sale.order", 2))]),
        ];
        assert_eq!(counts(&scope_from_rows(&rows)), [(SCOPE_ORDER, 1)]);
        assert!(scope_from_rows(&[]).is_empty());
    }

    /// A world answering the scope query with canned rows (and recording the query).
    struct Canned(Vec<Row>);

    #[async_trait]
    impl WorldModel for Canned {
        async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
            Err(EngineError::Backend("read-only".to_owned()))
        }
        async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
            Err(EngineError::Backend("read-only".to_owned()))
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
            Err(EngineError::Backend("read-only".to_owned()))
        }
        async fn select(&self, sparql: &str) -> Result<Vec<Row>> {
            assert!(sparql.contains("VALUES ?t {"), "{sparql}");
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn the_scope_is_read_with_one_select_over_the_targets() {
        let world = Canned(pl_0412());
        let line = iri("purchase.order.line", 5);
        let scope = plan_scope(&world, [line.as_str()]).await.expect("read");
        assert_eq!(scope.len(), 3);
        assert!(plan_scope(&world, []).await.expect("no targets").is_empty());
        assert!(
            plan_scope(&world, ["https://evil.example/x"])
                .await
                .is_err()
        );
    }

    fn framing(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn subject(model: &str, id: i64) -> EntityUri {
        EntityUri(iri(model, id))
    }

    #[test]
    fn the_lead_is_the_largest_known_slip() {
        let points = [
            (subject("sale.order", 1), framing(&[("day_slip", "4")])),
            (subject("sale.order", 2), framing(&[("day_slip", "9")])),
            (subject("sale.order", 3), framing(&[("day_slip", "-2")])),
        ];
        assert_eq!(lead_member(&points), Some(&subject("sale.order", 2)));
    }

    #[test]
    fn an_unknown_slip_ranks_below_a_known_one() {
        // The sales order 4 days late leads the MO with no known slip.
        let points = [
            (
                subject("mrp.production", 1),
                framing(&[("delivery", "at_risk")]),
            ),
            (
                subject("sale.order", 1),
                framing(&[("day_slip", "4"), ("delivery", "late")]),
            ),
        ];
        assert_eq!(lead_member(&points), Some(&subject("sale.order", 1)));
    }

    #[test]
    fn at_risk_leads_on_time_when_the_slips_tie() {
        let points = [
            (
                subject("sale.order", 1),
                framing(&[("day_slip", "0"), ("delivery", "on_time")]),
            ),
            (
                subject("sale.order", 2),
                framing(&[("day_slip", "0"), ("delivery", "at_risk")]),
            ),
        ];
        assert_eq!(lead_member(&points), Some(&subject("sale.order", 2)));
        // With no slip at all, the delivery outcome decides too.
        let unknown = [
            (
                subject("sale.order", 1),
                framing(&[("delivery", "on_time")]),
            ),
            (
                subject("sale.order", 2),
                framing(&[("delivery", "at_risk")]),
            ),
        ];
        assert_eq!(lead_member(&unknown), Some(&subject("sale.order", 2)));
    }

    #[test]
    fn a_full_tie_falls_back_to_the_point_id() {
        let points = [
            (subject("sale.order", 2), framing(&[("day_slip", "3")])),
            (subject("sale.order", 1), framing(&[("day_slip", "3")])),
            (subject("mrp.production", 9), BTreeMap::new()),
        ];
        assert_eq!(lead_member(&points), Some(&subject("sale.order", 1)));
        assert_eq!(lead_member(&[]), None);
    }
}
