//! The pure row builder: structural tab rows in, served rows out, with the selected point's
//! markers and the Rust-derived fields.
//!
//! - `at_risk`: the entity is in the rules' current at-risk set;
//! - `on_path`: the entity is the point's chain entity for its column/stage. The
//!   rule-independent hops (customer, order, the ordered product, its BOM, MO) are always
//!   known; the cause-dependent hops only when the at-risk set, the cause map and the cause
//!   query were all read. Cause-dependent are the component, supplier and purchase-order
//!   hops, and in the Product and BOM column every row other than the ordered product and its
//!   BOM: the sub-assemblies and BOMs on the route down to a deep cause, or a made chain
//!   component, are lit only through the chain, so without it their status is unknown. A
//!   purchase-order row that names a `product` (Timeline `inbound`, Flow `on_order`, Suppliers
//!   `open_po`: one row per PO and product) is on the path only when it is the chain PO and
//!   that product is the chain component; the PO's other lines are not;
//! - `cause`: every short component (and contended work order) of the point, and among
//!   purchase orders only the chain PO's row for the chain component, and only when its line
//!   arrives after the commitment;
//! - `sourcing` (single / primary / backup), `exposure`, and the on-time-delivery ratio `otd`.
//!
//! A marker whose inputs could not be read is omitted, never served as `"false"`. Only canonical
//! entity IRIs are admitted as entities, Graph nodes and both edge endpoints.

use std::collections::{BTreeMap, BTreeSet};

use super::Row;
use super::Tab;
use super::causes::{CauseMap, Causes};
use super::chain::{Chain, PathHops};
use crate::ontology;

/// Sourcing role: the component's only sourcing link.
pub const SOURCING_SINGLE: &str = "single";
/// Sourcing role: the lowest-`sequence` of several links.
pub const SOURCING_PRIMARY: &str = "primary";
/// Sourcing role: any other link.
pub const SOURCING_BACKUP: &str = "backup";
/// The sourcing roles the Rust layer adds (the tab-row vocabulary declares them).
pub const SOURCING_ROLES: [&str; 3] = [SOURCING_SINGLE, SOURCING_PRIMARY, SOURCING_BACKUP];

/// The marker keys the builder adds to entity rows.
pub const MARKER_KEYS: [&str; 3] = ["at_risk", "on_path", "cause"];
/// On-time-delivery evidence keys the Suppliers query projects and the builder consumes.
pub const OTD_EVIDENCE_KEYS: [&str; 2] = ["completed", "on_time"];

/// One sourcing link: a supplier sources a component at a `sequence` (lower = preferred).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcingLink {
    /// The supplier.
    pub supplier: String,
    /// The sourced component.
    pub component: String,
    /// Its sequence, when declared.
    pub sequence: Option<i64>,
}

impl SourcingLink {
    /// Read the links from `sourcing.rq` rows (skipping a row missing its keys).
    #[must_use]
    pub fn from_rows(rows: &[Row]) -> Vec<Self> {
        rows.iter()
            .filter_map(|row| {
                Some(Self {
                    supplier: row.get("supplier")?.clone(),
                    component: row.get("component")?.clone(),
                    sequence: row.get("sequence").and_then(|s| s.parse().ok()),
                })
            })
            .collect()
    }
}

/// The sourcing roles ranked from the links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcingRoles {
    /// Keyed by (supplier, component): that supplier's role for that component.
    pub per_link: BTreeMap<(String, String), &'static str>,
    /// Per supplier: `single` if it single-sources any component, else `primary` if it is
    /// primary for any, else `backup`.
    pub per_supplier: BTreeMap<String, &'static str>,
}

/// Rank each component's links by `sequence` (unsequenced last, ties by supplier IRI): one
/// link is `single`, the first of several `primary`, the rest `backup`.
#[must_use]
pub fn sourcing_roles(links: &[SourcingLink]) -> SourcingRoles {
    let mut by_component: BTreeMap<&str, Vec<&SourcingLink>> = BTreeMap::new();
    for link in links {
        by_component.entry(&link.component).or_default().push(link);
    }
    let mut roles = SourcingRoles::default();
    for (component, mut ranked) in by_component {
        ranked.sort_by(|a, b| {
            (a.sequence.is_none(), a.sequence, &a.supplier).cmp(&(
                b.sequence.is_none(),
                b.sequence,
                &b.supplier,
            ))
        });
        // One link per supplier (its best-ranked one), even when another supplier's link sorts
        // between two of its own; `dedup_by` would only drop adjacent duplicates.
        let mut seen = BTreeSet::new();
        ranked.retain(|link| seen.insert(link.supplier.as_str()));
        let single = ranked.len() == 1;
        for (rank, link) in ranked.iter().enumerate() {
            let role = match (single, rank) {
                (true, _) => SOURCING_SINGLE,
                (false, 0) => SOURCING_PRIMARY,
                _ => SOURCING_BACKUP,
            };
            roles
                .per_link
                .insert((link.supplier.clone(), component.to_owned()), role);
        }
    }
    for ((supplier, _), role) in &roles.per_link {
        let best = roles
            .per_supplier
            .entry(supplier.clone())
            .or_insert(SOURCING_BACKUP);
        let rank = |r: &str| SOURCING_ROLES.iter().position(|x| *x == r);
        if rank(role) < rank(best) {
            *best = role;
        }
    }
    roles
}

/// The selected point's marker inputs. `None` means "could not be read": the matching
/// markers are then omitted.
#[derive(Debug, Clone, Copy)]
pub struct Markers<'a> {
    /// The (canonical) subject IRI of the selected point.
    pub subject: &'a str,
    /// The rules' current at-risk set.
    pub at_risk: Option<&'a BTreeSet<String>>,
    /// The plant-wide cause map.
    pub causes: Option<&'a CauseMap>,
    /// The rule-independent path hops (always read).
    pub path: &'a PathHops,
    /// The cause-dependent chain (`None` when the cause query could not be read).
    pub chain: Option<&'a Chain>,
}

/// Which column/stage an entity row sits in. Decides how its `on_path` is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Hop {
    Customer,
    Order,
    Product,
    Bom,
    Mo,
    Component,
    Supplier,
    Po,
    /// An entity no chain hop can ever be (e.g. an unknown kind): never on the path.
    Other,
}

impl Hop {
    fn cause_dependent(self) -> bool {
        matches!(self, Self::Component | Self::Supplier | Self::Po)
    }
}

/// The entity key and hop of a structural row, or `None` for a row that names no entity (a
/// Graph edge).
fn classify(tab: Tab, row: &Row) -> Option<(&'static str, Hop)> {
    let kind = row.get("kind").map(String::as_str);
    match tab {
        Tab::Timeline => Some((
            "entity",
            match kind {
                Some("promise" | "ready") => Hop::Order,
                Some("inbound") => Hop::Po,
                _ => Hop::Other,
            },
        )),
        Tab::Graph => match kind {
            Some("node") => Some((
                "node",
                match row.get("role").map(String::as_str) {
                    Some("Customer") => Hop::Customer,
                    Some("SalesOrder") => Hop::Order,
                    Some("Product") => Hop::Product,
                    Some("BOM") => Hop::Bom,
                    Some("Component") => Hop::Component,
                    Some("Supplier") => Hop::Supplier,
                    Some("PurchaseOrder") => Hop::Po,
                    _ => Hop::Other,
                },
            )),
            _ => None,
        },
        Tab::Flow => Some((
            "entity",
            match row.get("stage").map(String::as_str) {
                Some("suppliers") => Hop::Supplier,
                Some("on_order") => Hop::Po,
                Some("in_stock") => Hop::Component,
                Some("in_production") => Hop::Mo,
                Some("promised") => Hop::Order,
                _ => Hop::Other,
            },
        )),
        Tab::Suppliers => match kind {
            Some("supplier") => Some(("supplier", Hop::Supplier)),
            Some("open_po") => Some(("po", Hop::Po)),
            Some("stock") => Some(("stock_of", Hop::Component)),
            Some("dependent_order") => Some(("order", Hop::Order)),
            _ => Some(("supplier", Hop::Other)),
        },
    }
}

fn flag(value: bool) -> String {
    if value { "true" } else { "false" }.to_owned()
}

/// What the markers inputs let the builder know.
struct Knowledge<'a> {
    markers: &'a Markers<'a>,
    /// The subject's causes (empty when it has none); `None` when the causes are unknown.
    subject_causes: Option<Causes>,
    /// The cause-dependent chain, only when every input it depends on was read.
    chain: Option<&'a Chain>,
}

impl<'a> Knowledge<'a> {
    fn new(markers: &'a Markers<'a>) -> Self {
        let causes_known = markers.at_risk.is_some() && markers.causes.is_some();
        let subject_causes = causes_known.then(|| {
            markers
                .causes
                .and_then(|map| map.of(markers.subject))
                .cloned()
                .unwrap_or_default()
        });
        Self {
            markers,
            subject_causes,
            chain: markers.chain.filter(|_| causes_known),
        }
    }

    /// Whether a PO row is the chain PO's row: the chain PO, and, when the row is one line
    /// product of it (`product` bound), that product is the chain component.
    fn chain_po_row(chain: &Chain, entity: &str, product: Option<&str>) -> bool {
        chain.po.as_deref() == Some(entity)
            && product.is_none_or(|product| chain.component.as_deref() == Some(product))
    }

    fn on_path(&self, hop: Hop, entity: &str, product: Option<&str>) -> Option<bool> {
        let path = self.markers.path;
        let is = |hop: &Option<String>| hop.as_deref() == Some(entity);
        if hop.cause_dependent() {
            let chain = self.chain?;
            return Some(match hop {
                Hop::Component => is(&chain.component),
                Hop::Supplier => is(&chain.supplier),
                _ => Self::chain_po_row(chain, entity, product),
            });
        }
        match hop {
            Hop::Customer => Some(is(&path.customer)),
            Hop::Order => Some(is(&path.order)),
            Hop::Mo => Some(is(&path.mo)),
            // The ordered product and its BOM are rule-independent. Any other Product/BOM row
            // (a sub-assembly or its BOM on the route down to a deep cause, or a made chain
            // component) is lit only through the chain: without it, its status is unknown.
            Hop::Product if is(&path.product) => Some(true),
            Hop::Bom if is(&path.bom) => Some(true),
            Hop::Product => self.chain.map(|chain| {
                chain.route.contains(entity) || chain.component.as_deref() == Some(entity)
            }),
            Hop::Bom => self.chain.map(|chain| chain.route.contains(entity)),
            _ => Some(false),
        }
    }

    fn cause(&self, hop: Hop, entity: &str, product: Option<&str>) -> Option<bool> {
        let causes = self.subject_causes.as_ref()?;
        if hop == Hop::Po {
            let chain = self.chain?;
            return Some(chain.po_late && Self::chain_po_row(chain, entity, product));
        }
        Some(causes.contains(entity))
    }

    /// Mark `row` and return the `on_path` it was served (the Graph lights its edges from the
    /// same value).
    fn mark(&self, row: &mut Row, hop: Hop, entity: &str) -> Option<bool> {
        let product = row.get("product").map(String::as_str);
        let at_risk = self.markers.at_risk.map(|set| set.contains(entity));
        let on_path = self.on_path(hop, entity, product);
        let cause = self.cause(hop, entity, product);
        for (key, value) in [("at_risk", at_risk), ("on_path", on_path), ("cause", cause)] {
            if let Some(value) = value {
                row.insert(key.to_owned(), flag(value));
            }
        }
        on_path
    }
}

/// Build the served rows of `tab` from its structural rows, the sourcing links, and the
/// selected point's marker inputs. Pure; the row order of the structural queries is kept.
#[must_use]
pub fn build_rows(
    tab: Tab,
    structural: Vec<Row>,
    sourcing: &[SourcingLink],
    markers: &Markers<'_>,
) -> Vec<Row> {
    let knowledge = Knowledge::new(markers);
    let roles = sourcing_roles(sourcing);
    let exposure = (tab == Tab::Suppliers)
        .then(|| exposure(&structural, sourcing, markers))
        .flatten();

    let mut out = Vec::with_capacity(structural.len());
    let mut edges = Vec::new();
    // Per Graph node, its on_path per role: a partner typed both Customer and Supplier is served
    // as two nodes, and each edge end is resolved to the role its predicate names.
    let mut node_on_path: BTreeMap<String, BTreeMap<Hop, Option<bool>>> = BTreeMap::new();

    for mut row in structural {
        let Some((key, hop)) = classify(tab, &row) else {
            edges.push(row);
            continue;
        };
        let Some(entity) = row
            .get(key)
            .filter(|iri| ontology::is_canonical_entity_iri(iri))
            .cloned()
        else {
            continue; // only canonical entity IRIs are served as entities
        };
        let on_path = knowledge.mark(&mut row, hop, &entity);
        if tab == Tab::Graph {
            node_on_path
                .entry(entity.clone())
                .or_default()
                .insert(hop, on_path);
        }
        derive_fields(tab, &mut row, &entity, &roles, exposure.as_ref());
        out.push(row);
    }

    // Graph edges: only between two admitted nodes; lit when both ends are on the path.
    for mut edge in edges {
        let (Some(s), Some(o)) = (edge.get("s"), edge.get("o")) else {
            continue;
        };
        let predicate = edge.get("p").map_or("", String::as_str);
        let end = |iri: &str, side: Side| {
            node_on_path
                .get(iri)
                .map(|roles| end_on_path(roles, endpoint_hop(predicate, side)))
        };
        let (Some(s_on), Some(o_on)) = (end(s, Side::Subject), end(o, Side::Object)) else {
            continue;
        };
        let lit = match (s_on, o_on) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        };
        if let Some(lit) = lit {
            edge.insert("on_path".to_owned(), flag(lit));
        }
        out.push(edge);
    }
    out
}

/// Which end of a Graph edge.
#[derive(Clone, Copy)]
enum Side {
    Subject,
    Object,
}

/// The role a Graph edge's predicate gives one of its ends, where that end is a partner that
/// can be typed both Customer and Supplier: `for_customer` points at the Customer,
/// `supplies_product` starts from the Supplier, `from_supplier` points at the Supplier. Every
/// other end has one role (a product's role is positional, an order's its class).
fn endpoint_hop(predicate: &str, side: Side) -> Option<Hop> {
    let local = predicate.rsplit(['#', '/']).next().unwrap_or(predicate);
    match (local, side) {
        ("for_customer", Side::Object) => Some(Hop::Customer),
        ("supplies_product", Side::Subject) | ("from_supplier", Side::Object) => {
            Some(Hop::Supplier)
        }
        _ => None,
    }
}

/// An edge end's on_path: the named role's own marker when the node serves that role;
/// otherwise its roles merged (a known `true` wins, any unknown keeps it unknown).
fn end_on_path(roles: &BTreeMap<Hop, Option<bool>>, hop: Option<Hop>) -> Option<bool> {
    if let Some(on_path) = hop.and_then(|hop| roles.get(&hop)) {
        return *on_path;
    }
    roles
        .values()
        .copied()
        .reduce(|seen, next| match (seen, next) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        })
        .flatten()
}

/// The per-tab derived fields: sourcing roles, the OTD ratio and exposure.
fn derive_fields(
    tab: Tab,
    row: &mut Row,
    entity: &str,
    roles: &SourcingRoles,
    exposure: Option<&Exposure>,
) {
    match (
        tab,
        row.get("kind").map(String::as_str),
        row.get("stage").map(String::as_str),
    ) {
        (Tab::Flow, _, Some("suppliers")) => {
            if let Some(role) = roles.per_supplier.get(entity) {
                row.insert("sourcing".to_owned(), (*role).to_owned());
            }
        }
        (Tab::Suppliers, Some("supplier"), _) => {
            if let Some(role) = roles.per_supplier.get(entity) {
                row.insert("sourcing".to_owned(), (*role).to_owned());
            }
            // OTD is on-time over completed. Without a completed receipt there is no figure.
            let completed = row.get("completed").and_then(|v| v.parse::<f64>().ok());
            let on_time = row.get("on_time").and_then(|v| v.parse::<f64>().ok());
            if let (Some(completed), Some(on_time)) = (completed, on_time)
                && completed > 0.0
            {
                row.insert("otd".to_owned(), format!("{:.4}", on_time / completed));
            }
            for key in OTD_EVIDENCE_KEYS {
                row.remove(key);
            }
            if let Some(exposure) = exposure.filter(|e| !e.unknown.contains(entity)) {
                let value = exposure.totals.get(entity).copied().unwrap_or(0.0);
                row.insert("exposure".to_owned(), format!("{value:.2}"));
            }
        }
        (Tab::Suppliers, Some("stock"), _) => {
            let role = row
                .get("supplier")
                .and_then(|s| roles.per_link.get(&(s.clone(), entity.to_owned())));
            if let Some(role) = role {
                row.insert("sourcing".to_owned(), (*role).to_owned());
            }
        }
        _ => {}
    }
}

/// Per-supplier exposure, and the suppliers whose total cannot be known.
struct Exposure {
    /// Per supplier, the summed order value of the at-risk orders whose cause it sources.
    totals: BTreeMap<String, f64>,
    /// Suppliers sourcing the cause of an at-risk order whose value the world model lacks:
    /// their exposure is omitted rather than served understated.
    unknown: BTreeSet<String>,
}

/// Exposure per supplier: the sum of distinct order values over the at-risk orders whose cause
/// component (from the cause map, not any BOM component) the supplier sources. `None`
/// (omitted) unless both the at-risk set and the cause map were read; a supplier whose sum
/// would need a missing order value is omitted too.
fn exposure(
    structural: &[Row],
    sourcing: &[SourcingLink],
    markers: &Markers<'_>,
) -> Option<Exposure> {
    let (at_risk, causes) = (markers.at_risk?, markers.causes?);
    let values: BTreeMap<&str, f64> = structural
        .iter()
        .filter(|row| row.get("kind").is_some_and(|k| k == "dependent_order"))
        .filter_map(|row| {
            Some((
                row.get("order")?.as_str(),
                row.get("order_value")?.parse::<f64>().ok()?,
            ))
        })
        .collect();
    let sourced: BTreeSet<(&str, &str)> = sourcing
        .iter()
        .map(|link| (link.supplier.as_str(), link.component.as_str()))
        .collect();
    let mut exposure = Exposure {
        totals: BTreeMap::new(),
        unknown: BTreeSet::new(),
    };
    for (order, order_causes) in causes.iter() {
        if !at_risk.contains(order) {
            continue;
        }
        let suppliers: BTreeSet<&str> = sourced
            .iter()
            .filter(|(_, component)| order_causes.contains(component))
            .map(|(supplier, _)| *supplier)
            .collect();
        let Some(&value) = values.get(order.as_str()) else {
            // No value in the world model: these suppliers' totals are unknown, never an
            // understated sum.
            exposure
                .unknown
                .extend(suppliers.into_iter().map(str::to_owned));
            continue;
        };
        for supplier in suppliers {
            *exposure.totals.entry(supplier.to_owned()).or_insert(0.0) += value;
        }
    }
    Some(exposure)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::too_many_lines)]
    use super::*;

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// The flagship world as ids: VeloMax (partner 11) orders sale.order/1 of the E-Bike
    /// (product 1, BOM 1); it is short of the battery (product 4) sourced from CellTech
    /// (partner 6, single) whose PO 1 arrives late; the frame (product 3) comes from FrameCo
    /// (partner 7) and is not short. MO 1 fulfils the order.
    struct World {
        so: String,
        customer: String,
        product: String,
        bom: String,
        mo: String,
        battery: String,
        frame: String,
        celltech: String,
        frameco: String,
        po: String,
    }

    fn world() -> World {
        World {
            so: iri("sale.order", 1),
            customer: iri("res.partner", 11),
            product: iri("product.product", 1),
            bom: iri("mrp.bom", 1),
            mo: iri("mrp.production", 1),
            battery: iri("product.product", 4),
            frame: iri("product.product", 3),
            celltech: iri("res.partner", 6),
            frameco: iri("res.partner", 7),
            po: iri("purchase.order", 1),
        }
    }

    fn path(w: &World) -> PathHops {
        PathHops {
            customer: Some(w.customer.clone()),
            order: Some(w.so.clone()),
            product: Some(w.product.clone()),
            bom: Some(w.bom.clone()),
            mo: Some(w.mo.clone()),
        }
    }

    fn cause_map(w: &World) -> CauseMap {
        CauseMap::from_rows(
            &[row(&[
                ("so", &w.so),
                ("comp", &w.battery),
                ("shortfall", "13"),
            ])],
            &[],
        )
    }

    fn late_chain(w: &World) -> Chain {
        Chain {
            component: Some(w.battery.clone()),
            supplier: Some(w.celltech.clone()),
            po: Some(w.po.clone()),
            po_late: true,
            route: BTreeSet::new(),
        }
    }

    fn graph_rows(w: &World) -> Vec<Row> {
        vec![
            row(&[
                ("kind", "node"),
                ("node", &w.customer),
                ("role", "Customer"),
            ]),
            row(&[("kind", "node"), ("node", &w.so), ("role", "SalesOrder")]),
            row(&[("kind", "node"), ("node", &w.product), ("role", "Product")]),
            row(&[("kind", "node"), ("node", &w.bom), ("role", "BOM")]),
            row(&[
                ("kind", "node"),
                ("node", &w.battery),
                ("role", "Component"),
            ]),
            row(&[("kind", "node"), ("node", &w.frame), ("role", "Component")]),
            row(&[
                ("kind", "node"),
                ("node", &w.celltech),
                ("role", "Supplier"),
            ]),
            row(&[("kind", "node"), ("node", &w.frameco), ("role", "Supplier")]),
            row(&[("kind", "node"), ("node", &w.po), ("role", "PurchaseOrder")]),
            row(&[
                ("kind", "edge"),
                ("s", &w.so),
                ("p", "for_customer"),
                ("o", &w.customer),
            ]),
            row(&[
                ("kind", "edge"),
                ("s", &w.po),
                ("p", "from_supplier"),
                ("o", &w.celltech),
            ]),
            row(&[
                ("kind", "edge"),
                ("s", &w.frameco),
                ("p", "supplies"),
                ("o", &w.frame),
            ]),
        ]
    }

    fn node<'r>(rows: &'r [Row], iri: &str) -> &'r Row {
        rows.iter()
            .find(|r| r.get("node").is_some_and(|n| n == iri))
            .expect("node served")
    }

    fn get<'r>(row: &'r Row, key: &str) -> Option<&'r str> {
        row.get(key).map(String::as_str)
    }

    #[test]
    fn the_full_chain_is_lit_and_the_causes_filled() {
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
        for lit in [
            &w.customer,
            &w.so,
            &w.product,
            &w.bom,
            &w.battery,
            &w.celltech,
            &w.po,
        ] {
            assert_eq!(get(node(&rows, lit), "on_path"), Some("true"), "{lit} lit");
        }
        for dim in [&w.frame, &w.frameco] {
            assert_eq!(get(node(&rows, dim), "on_path"), Some("false"), "{dim} dim");
        }
        assert_eq!(get(node(&rows, &w.so), "at_risk"), Some("true"));
        assert_eq!(get(node(&rows, &w.battery), "at_risk"), Some("false"));
        assert_eq!(get(node(&rows, &w.battery), "cause"), Some("true"), "short");
        assert_eq!(
            get(node(&rows, &w.po), "cause"),
            Some("true"),
            "late chain PO"
        );
        assert_eq!(get(node(&rows, &w.frame), "cause"), Some("false"));
        // Edges: lit between two lit nodes, dim when an end is off the path.
        let edge = |s: &str| {
            rows.iter()
                .find(|r| get(r, "kind") == Some("edge") && get(r, "s") == Some(s))
                .expect("edge served")
        };
        assert_eq!(get(edge(&w.so), "on_path"), Some("true"));
        assert_eq!(get(edge(&w.po), "on_path"), Some("true"));
        assert_eq!(get(edge(&w.frameco), "on_path"), Some("false"));
    }

    #[test]
    fn an_unreadable_rule_set_omits_every_risk_marker_and_the_cause_dependent_path() {
        let w = world();
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let markers = Markers {
            subject: &w.so,
            at_risk: None,
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
        for r in rows.iter().filter(|r| get(r, "kind") == Some("node")) {
            assert!(!r.contains_key("at_risk"), "never a denied risk: {r:?}");
            assert!(
                !r.contains_key("cause"),
                "no cause without the rules: {r:?}"
            );
        }
        for rule_independent in [&w.customer, &w.so, &w.product, &w.bom] {
            assert_eq!(get(node(&rows, rule_independent), "on_path"), Some("true"));
        }
        for cause_dependent in [&w.battery, &w.frame, &w.celltech, &w.frameco, &w.po] {
            assert!(!node(&rows, cause_dependent).contains_key("on_path"));
        }
        // An edge with an unknown end is not denied either.
        let po_edge = rows
            .iter()
            .find(|r| get(r, "s") == Some(w.po.as_str()))
            .expect("edge");
        assert!(!po_edge.contains_key("on_path"));
    }

    #[test]
    fn an_unreadable_cause_map_or_cause_query_omits_the_cause_dependent_path() {
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        // (a) the cause map could not be read; (b) the cause query could not be read.
        for (causes, chain) in [(None, Some(&chain)), (Some(&causes), None)] {
            let markers = Markers {
                subject: &w.so,
                at_risk: Some(&at_risk),
                causes,
                path: &path,
                chain,
            };
            let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
            for cause_dependent in [&w.battery, &w.celltech, &w.po] {
                assert!(
                    !node(&rows, cause_dependent).contains_key("on_path"),
                    "{cause_dependent}: path status unknown, so omitted"
                );
            }
            assert!(
                !node(&rows, &w.po).contains_key("cause"),
                "PO cause unknown"
            );
            for rule_independent in [&w.customer, &w.so, &w.product, &w.bom] {
                assert_eq!(get(node(&rows, rule_independent), "on_path"), Some("true"));
            }
            assert_eq!(
                get(node(&rows, &w.so), "at_risk"),
                Some("true"),
                "set still read"
            );
        }
        // With only the cause query unreadable the short component is still a known cause.
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: None,
        };
        let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
        assert_eq!(get(node(&rows, &w.battery), "cause"), Some("true"));
    }

    #[test]
    fn an_available_but_empty_cause_map_serves_the_cause_dependent_path_as_false() {
        let w = world();
        let at_risk = BTreeSet::new();
        let causes = CauseMap::default();
        let (path, chain) = (path(&w), Chain::default());
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
        for cause_dependent in [&w.battery, &w.frame, &w.celltech, &w.po] {
            assert_eq!(get(node(&rows, cause_dependent), "on_path"), Some("false"));
            assert_eq!(get(node(&rows, cause_dependent), "cause"), Some("false"));
        }
        assert_eq!(
            get(node(&rows, &w.so), "at_risk"),
            Some("false"),
            "rule cleared"
        );
    }

    #[test]
    fn an_on_time_fallback_po_is_on_the_path_but_not_a_cause() {
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let path = path(&w);
        let chain = Chain {
            po_late: false,
            ..late_chain(&w)
        };
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
        assert_eq!(get(node(&rows, &w.po), "on_path"), Some("true"));
        assert_eq!(get(node(&rows, &w.po), "cause"), Some("false"));
    }

    #[test]
    fn a_process_points_contended_work_orders_are_causes_in_production() {
        let mo = iri("mrp.production", 3);
        let (wo_contended, wo_free) = (iri("mrp.workorder", 1), iri("mrp.workorder", 9));
        let at_risk = BTreeSet::from([mo.clone()]);
        let causes = CauseMap::from_rows(
            &[],
            &[row(&[
                ("mo", &mo),
                ("wo", &wo_contended),
                ("capacity", "1"),
                ("overlapPeers", "1"),
            ])],
        );
        let path = PathHops {
            mo: Some(mo.clone()),
            ..PathHops::default()
        };
        let chain = Chain::default();
        let markers = Markers {
            subject: &mo,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let flow = vec![
            row(&[("stage", "in_production"), ("entity", &mo)]),
            row(&[
                ("stage", "in_production"),
                ("entity", &wo_contended),
                ("part_of", &mo),
            ]),
            row(&[("stage", "in_production"), ("entity", &wo_free)]),
        ];
        let rows = build_rows(Tab::Flow, flow, &[], &markers);
        let by = |e: &str| {
            rows.iter()
                .find(|r| get(r, "entity") == Some(e))
                .expect("served")
        };
        assert_eq!(
            get(by(&mo), "on_path"),
            Some("true"),
            "an MO point lights itself"
        );
        assert_eq!(get(by(&mo), "at_risk"), Some("true"));
        assert_eq!(get(by(&wo_contended), "cause"), Some("true"));
        assert_eq!(get(by(&wo_free), "cause"), Some("false"));
        assert_eq!(get(by(&wo_contended), "on_path"), Some("false"));
    }

    #[test]
    fn only_canonical_entity_iris_are_nodes_and_edge_endpoints() {
        let w = world();
        let provenance = format!("{}#source", w.so);
        let (path, chain) = (path(&w), Chain::default());
        let markers = Markers {
            subject: &w.so,
            at_risk: None,
            causes: None,
            path: &path,
            chain: Some(&chain),
        };
        let structural = vec![
            row(&[("kind", "node"), ("node", &w.so), ("role", "SalesOrder")]),
            row(&[
                ("kind", "node"),
                ("node", &provenance),
                ("role", "SalesOrder"),
            ]),
            row(&[
                ("kind", "node"),
                ("node", "https://ekg.example/ontology#SalesOrder"),
                ("role", "SalesOrder"),
            ]),
            row(&[
                ("kind", "edge"),
                ("s", &w.so),
                ("p", "was_derived_from"),
                ("o", &provenance),
            ]),
            row(&[
                ("kind", "edge"),
                ("s", &w.so),
                ("p", "for_customer"),
                ("o", &w.customer),
            ]),
        ];
        let rows = build_rows(Tab::Graph, structural, &[], &markers);
        assert_eq!(rows.len(), 1, "only the canonical order survives: {rows:?}");
        assert!(rows.iter().all(|r| !r.values().any(|v| v.contains('#'))));
    }

    #[test]
    fn sourcing_ranks_single_primary_and_backup() {
        let (battery, motor) = (iri("product.product", 4), iri("product.product", 5));
        let (celltech, motorworks, driveline) = (
            iri("res.partner", 6),
            iri("res.partner", 8),
            iri("res.partner", 9),
        );
        let links = vec![
            SourcingLink {
                supplier: celltech.clone(),
                component: battery.clone(),
                sequence: Some(1),
            },
            SourcingLink {
                supplier: driveline.clone(),
                component: motor.clone(),
                sequence: Some(2),
            },
            SourcingLink {
                supplier: motorworks.clone(),
                component: motor.clone(),
                sequence: Some(1),
            },
        ];
        let roles = sourcing_roles(&links);
        assert_eq!(roles.per_supplier[&celltech], SOURCING_SINGLE);
        assert_eq!(roles.per_supplier[&motorworks], SOURCING_PRIMARY);
        assert_eq!(roles.per_supplier[&driveline], SOURCING_BACKUP);
        assert_eq!(
            roles.per_link[&(driveline.clone(), motor.clone())],
            SOURCING_BACKUP
        );
        // A supplier single for one component and backup for another is `single`.
        let mut more = links.clone();
        more.push(SourcingLink {
            supplier: celltech.clone(),
            component: motor.clone(),
            sequence: Some(3),
        });
        let roles = sourcing_roles(&more);
        assert_eq!(roles.per_supplier[&celltech], SOURCING_SINGLE);
        assert_eq!(roles.per_link[&(celltech, motor)], SOURCING_BACKUP);
    }

    #[test]
    fn suppliers_rows_carry_sourcing_otd_only_with_receipts_and_cause_scoped_exposure() {
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        // The order is short of the battery only; it also needs the frame (FrameCo).
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let sourcing = vec![
            SourcingLink {
                supplier: w.celltech.clone(),
                component: w.battery.clone(),
                sequence: Some(1),
            },
            SourcingLink {
                supplier: w.frameco.clone(),
                component: w.frame.clone(),
                sequence: Some(1),
            },
        ];
        let structural = vec![
            row(&[
                ("kind", "supplier"),
                ("supplier", &w.celltech),
                ("lead_time", "45"),
                ("completed", "4"),
                ("on_time", "3"),
            ]),
            row(&[
                ("kind", "supplier"),
                ("supplier", &w.frameco),
                ("lead_time", "14"),
            ]),
            row(&[
                ("kind", "stock"),
                ("supplier", &w.celltech),
                ("stock_of", &w.battery),
            ]),
            row(&[
                ("kind", "dependent_order"),
                ("supplier", &w.celltech),
                ("order", &w.so),
                ("order_value", "18000.0"),
            ]),
            row(&[
                ("kind", "dependent_order"),
                ("supplier", &w.frameco),
                ("order", &w.so),
                ("order_value", "18000.0"),
            ]),
        ];
        let rows = build_rows(Tab::Suppliers, structural, &sourcing, &markers);
        let celltech = &rows[0];
        assert_eq!(get(celltech, "otd"), Some("0.7500"));
        assert!(!celltech.contains_key("completed") && !celltech.contains_key("on_time"));
        assert_eq!(get(celltech, "sourcing"), Some(SOURCING_SINGLE));
        assert_eq!(
            get(celltech, "exposure"),
            Some("18000.00"),
            "sources the cause"
        );
        assert_eq!(get(celltech, "on_path"), Some("true"));
        let frameco = &rows[1];
        assert!(
            !frameco.contains_key("otd"),
            "no receipts, so no OTD figure"
        );
        assert_eq!(
            get(frameco, "exposure"),
            Some("0.00"),
            "a supplier of a non-cause component carries no exposure for the order"
        );
        assert_eq!(get(frameco, "on_path"), Some("false"));
        assert_eq!(
            get(&rows[2], "sourcing"),
            Some(SOURCING_SINGLE),
            "stock row role"
        );
        assert_eq!(get(&rows[2], "cause"), Some("true"));
        assert_eq!(
            get(&rows[3], "at_risk"),
            Some("true"),
            "dependent order marked"
        );
        // Without the at-risk set exposure is not served (not even as 0).
        let markers = Markers {
            at_risk: None,
            ..markers
        };
        let rows = build_rows(
            Tab::Suppliers,
            vec![row(&[("kind", "supplier"), ("supplier", &w.celltech)])],
            &sourcing,
            &markers,
        );
        assert!(!rows[0].contains_key("exposure"));
        assert!(
            !rows[0].contains_key("on_path"),
            "supplier hop is cause-dependent"
        );
    }

    fn get_by<'r>(rows: &'r [Row], pairs: &[(&str, &str)]) -> &'r Row {
        rows.iter()
            .find(|r| pairs.iter().all(|(k, v)| get(r, k) == Some(*v)))
            .unwrap_or_else(|| panic!("a row with {pairs:?} in {rows:?}"))
    }

    /// The flagship markers with every input known or, with `readable` false, the rules
    /// unreadable (no at-risk set, so no cause map or chain either).
    fn flagship<'a>(
        w: &'a World,
        at_risk: &'a BTreeSet<String>,
        causes: &'a CauseMap,
        path: &'a PathHops,
        chain: &'a Chain,
        readable: bool,
    ) -> Markers<'a> {
        Markers {
            subject: &w.so,
            at_risk: readable.then_some(at_risk),
            causes: readable.then_some(causes),
            path,
            chain: readable.then_some(chain),
        }
    }

    #[test]
    fn a_two_line_chain_po_lights_only_its_cause_line() {
        // The chain PO also carries a frame line: one row per (PO, product) on Timeline,
        // Flow and Suppliers. Only the battery row is the chain's and the late cause.
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let markers = flagship(&w, &at_risk, &causes, &path, &chain, true);
        let po_rows = |tab: Tab| -> Vec<Row> {
            let line = |product: &str| match tab {
                Tab::Timeline => {
                    row(&[("kind", "inbound"), ("entity", &w.po), ("product", product)])
                }
                Tab::Flow => row(&[
                    ("stage", "on_order"),
                    ("entity", &w.po),
                    ("product", product),
                ]),
                _ => row(&[
                    ("kind", "open_po"),
                    ("supplier", &w.celltech),
                    ("po", &w.po),
                    ("product", product),
                ]),
            };
            vec![line(&w.battery), line(&w.frame)]
        };
        for tab in [Tab::Timeline, Tab::Flow, Tab::Suppliers] {
            let rows = build_rows(tab, po_rows(tab), &[], &markers);
            let cause_line = get_by(&rows, &[("product", &w.battery)]);
            assert_eq!(get(cause_line, "on_path"), Some("true"), "{tab:?}");
            assert_eq!(get(cause_line, "cause"), Some("true"), "{tab:?}");
            let other_line = get_by(&rows, &[("product", &w.frame)]);
            assert_eq!(
                get(other_line, "on_path"),
                Some("false"),
                "{tab:?}: not the cause line"
            );
            assert_eq!(
                get(other_line, "cause"),
                Some("false"),
                "{tab:?}: not the cause line"
            );
        }
        // The Graph draws one node per PO: the PO node itself is still lit and the cause.
        let rows = build_rows(Tab::Graph, graph_rows(&w), &[], &markers);
        assert_eq!(get(node(&rows, &w.po), "on_path"), Some("true"));
        assert_eq!(get(node(&rows, &w.po), "cause"), Some("true"));
    }

    #[test]
    fn the_timeline_maps_orders_and_inbound_to_their_hops() {
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let other = iri("sale.order", 2);
        let other_po = iri("purchase.order", 9);
        let structural = || {
            vec![
                row(&[("kind", "promise"), ("entity", &w.so)]),
                row(&[("kind", "ready"), ("entity", &w.so)]),
                row(&[("kind", "promise"), ("entity", &other)]),
                row(&[
                    ("kind", "inbound"),
                    ("entity", &w.po),
                    ("product", &w.battery),
                ]),
                row(&[
                    ("kind", "inbound"),
                    ("entity", &other_po),
                    ("product", &w.battery),
                ]),
            ]
        };
        let known = flagship(&w, &at_risk, &causes, &path, &chain, true);
        let rows = build_rows(Tab::Timeline, structural(), &[], &known);
        let at = |kind: &str, entity: &str| get_by(&rows, &[("kind", kind), ("entity", entity)]);
        for kind in ["promise", "ready"] {
            assert_eq!(get(at(kind, &w.so), "on_path"), Some("true"), "{kind}");
            assert_eq!(get(at(kind, &w.so), "at_risk"), Some("true"), "{kind}");
            assert_eq!(get(at(kind, &w.so), "cause"), Some("false"), "{kind}");
        }
        assert_eq!(get(at("promise", &other), "on_path"), Some("false"));
        assert_eq!(get(at("promise", &other), "at_risk"), Some("false"));
        assert_eq!(get(at("inbound", &w.po), "on_path"), Some("true"));
        assert_eq!(
            get(at("inbound", &w.po), "cause"),
            Some("true"),
            "the late chain PO"
        );
        assert_eq!(get(at("inbound", &other_po), "on_path"), Some("false"));
        assert_eq!(get(at("inbound", &other_po), "cause"), Some("false"));
        // The rules unreadable: the order hops keep on_path; inbound omits it; no risk markers.
        let unknown = flagship(&w, &at_risk, &causes, &path, &chain, false);
        let rows = build_rows(Tab::Timeline, structural(), &[], &unknown);
        let at = |kind: &str, entity: &str| get_by(&rows, &[("kind", kind), ("entity", entity)]);
        assert_eq!(get(at("promise", &w.so), "on_path"), Some("true"));
        assert_eq!(get(at("ready", &w.so), "on_path"), Some("true"));
        for r in &rows {
            assert!(
                !r.contains_key("at_risk") && !r.contains_key("cause"),
                "{r:?}"
            );
        }
        assert!(
            !at("inbound", &w.po).contains_key("on_path"),
            "path status unknown"
        );
        assert!(
            !at("inbound", &other_po).contains_key("on_path"),
            "path status unknown"
        );
    }

    #[test]
    fn the_suppliers_detail_rows_map_to_their_hops() {
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let sourcing = vec![
            SourcingLink {
                supplier: w.celltech.clone(),
                component: w.battery.clone(),
                sequence: Some(1),
            },
            SourcingLink {
                supplier: w.frameco.clone(),
                component: w.frame.clone(),
                sequence: Some(1),
            },
        ];
        let other_po = iri("purchase.order", 9);
        let structural = || {
            vec![
                row(&[
                    ("kind", "open_po"),
                    ("supplier", &w.celltech),
                    ("po", &w.po),
                    ("product", &w.battery),
                ]),
                row(&[
                    ("kind", "open_po"),
                    ("supplier", &w.frameco),
                    ("po", &other_po),
                    ("product", &w.frame),
                ]),
                row(&[
                    ("kind", "stock"),
                    ("supplier", &w.celltech),
                    ("stock_of", &w.battery),
                ]),
                row(&[
                    ("kind", "stock"),
                    ("supplier", &w.frameco),
                    ("stock_of", &w.frame),
                ]),
            ]
        };
        let known = flagship(&w, &at_risk, &causes, &path, &chain, true);
        let rows = build_rows(Tab::Suppliers, structural(), &sourcing, &known);
        let open_po = |po: &str| get_by(&rows, &[("kind", "open_po"), ("po", po)]);
        let stock = |of: &str| get_by(&rows, &[("kind", "stock"), ("stock_of", of)]);
        assert_eq!(get(open_po(&w.po), "on_path"), Some("true"));
        assert_eq!(
            get(open_po(&w.po), "cause"),
            Some("true"),
            "the late chain PO"
        );
        assert_eq!(get(open_po(&other_po), "on_path"), Some("false"));
        assert_eq!(get(open_po(&other_po), "cause"), Some("false"));
        assert_eq!(get(stock(&w.battery), "on_path"), Some("true"));
        assert_eq!(get(stock(&w.battery), "cause"), Some("true"), "short");
        assert_eq!(get(stock(&w.frame), "on_path"), Some("false"));
        assert_eq!(get(stock(&w.frame), "cause"), Some("false"));
        assert_eq!(get(stock(&w.frame), "sourcing"), Some(SOURCING_SINGLE));
        // The rules unreadable: every one of these hops is cause-dependent, so nothing is denied.
        let unknown = flagship(&w, &at_risk, &causes, &path, &chain, false);
        let rows = build_rows(Tab::Suppliers, structural(), &sourcing, &unknown);
        for r in &rows {
            for key in MARKER_KEYS {
                assert!(!r.contains_key(key), "{key} unknown: {r:?}");
            }
            if get(r, "kind") == Some("stock") {
                assert!(r.contains_key("sourcing"), "the role needs no rule: {r:?}");
            }
        }
    }

    #[test]
    fn a_deep_cause_is_one_lit_path_through_its_sub_assembly() {
        // BOM 1 builds the E-Bike from Drive (a sub-assembly) and the frame; BOM 2 builds
        // Drive from the motor (the cause).
        let w = world();
        let (drive, motor, bom2) = (
            iri("product.product", 2),
            iri("product.product", 5),
            iri("mrp.bom", 2),
        );
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = CauseMap::from_rows(
            &[row(&[("so", &w.so), ("comp", &motor), ("shortfall", "3")])],
            &[],
        );
        let path = path(&w);
        let chain = Chain {
            component: Some(motor.clone()),
            route: BTreeSet::from([w.bom.clone(), drive.clone(), bom2.clone()]),
            ..Chain::default()
        };
        let edge =
            |s: &str, p: &str, o: &str| row(&[("kind", "edge"), ("s", s), ("p", p), ("o", o)]);
        let structural = || {
            vec![
                row(&[("kind", "node"), ("node", &w.product), ("role", "Product")]),
                row(&[("kind", "node"), ("node", &w.bom), ("role", "BOM")]),
                row(&[("kind", "node"), ("node", &drive), ("role", "Product")]),
                row(&[("kind", "node"), ("node", &bom2), ("role", "BOM")]),
                row(&[("kind", "node"), ("node", &motor), ("role", "Component")]),
                row(&[("kind", "node"), ("node", &w.frame), ("role", "Component")]),
                edge(&w.bom, "produces_product", &w.product),
                edge(&w.bom, "component_product", &drive),
                edge(&bom2, "produces_product", &drive),
                edge(&bom2, "component_product", &motor),
                edge(&w.bom, "component_product", &w.frame),
            ]
        };
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let rows = build_rows(Tab::Graph, structural(), &[], &markers);
        for lit in [&w.product, &w.bom, &drive, &bom2, &motor] {
            assert_eq!(get(node(&rows, lit), "on_path"), Some("true"), "{lit}");
        }
        assert_eq!(get(node(&rows, &w.frame), "on_path"), Some("false"));
        let edge_lit = |s: &str, o: &str| {
            rows.iter()
                .find(|r| get(r, "s") == Some(s) && get(r, "o") == Some(o))
                .and_then(|r| get(r, "on_path"))
        };
        for (s, o) in [
            (&w.bom, &w.product),
            (&w.bom, &drive),
            (&bom2, &drive),
            (&bom2, &motor),
        ] {
            assert_eq!(edge_lit(s, o), Some("true"), "{s} -> {o}: no gap");
        }
        assert_eq!(edge_lit(&w.bom, &w.frame), Some("false"));
        // Without the chain the route is unknown: the ordered product and its BOM stay lit,
        // while the sub-assembly and its BOM, both cause-dependent, serve no on_path at all
        // (never an off-path "false" for a node that is in fact on the path), and neither
        // does an edge touching them.
        let markers = Markers {
            chain: None,
            ..markers
        };
        let rows = build_rows(Tab::Graph, structural(), &[], &markers);
        assert_eq!(get(node(&rows, &w.product), "on_path"), Some("true"));
        assert_eq!(get(node(&rows, &w.bom), "on_path"), Some("true"));
        for unknown in [&drive, &bom2, &motor] {
            assert!(!node(&rows, unknown).contains_key("on_path"), "{unknown}");
        }
        assert_eq!(edge_lit(&w.bom, &w.product), Some("true"));
        for (s, o) in [(&w.bom, &drive), (&bom2, &drive), (&bom2, &motor)] {
            let edge = rows
                .iter()
                .find(|r| get(r, "s") == Some(s) && get(r, "o") == Some(o))
                .expect("edge served");
            assert!(
                !edge.contains_key("on_path"),
                "{s} -> {o}: unknown, not denied"
            );
        }
    }

    #[test]
    fn a_made_chain_component_is_lit_in_the_product_column() {
        // Only a made cause is short (the rule lists purchased causes first, so this is the
        // chain component only when no purchased cause is short).
        let w = world();
        let drive = iri("product.product", 2);
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = CauseMap::from_rows(
            &[row(&[("so", &w.so), ("comp", &drive), ("shortfall", "10")])],
            &[],
        );
        let path = path(&w);
        let chain = Chain {
            component: Some(drive.clone()),
            route: BTreeSet::from([w.bom.clone()]),
            ..Chain::default()
        };
        let markers = Markers {
            subject: &w.so,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let structural = vec![
            row(&[("kind", "node"), ("node", &w.bom), ("role", "BOM")]),
            row(&[("kind", "node"), ("node", &drive), ("role", "Product")]),
            row(&[
                ("kind", "edge"),
                ("s", &w.bom),
                ("p", "component_product"),
                ("o", &drive),
            ]),
        ];
        let rows = build_rows(Tab::Graph, structural, &[], &markers);
        assert_eq!(get(node(&rows, &drive), "on_path"), Some("true"));
        assert_eq!(get(node(&rows, &drive), "cause"), Some("true"));
        assert_eq!(
            get(&rows[2], "on_path"),
            Some("true"),
            "BOM to made cause is lit"
        );
    }

    #[test]
    fn a_supplier_keeps_its_best_link_even_between_other_suppliers_links() {
        // A/1, B/2, A/3 for one component: A is primary (its best link), B backup. A
        // non-adjacent duplicate of A must not overwrite A's role.
        let comp = iri("product.product", 4);
        let (a, b) = (iri("res.partner", 1), iri("res.partner", 2));
        let link = |supplier: &str, sequence| SourcingLink {
            supplier: supplier.to_owned(),
            component: comp.clone(),
            sequence: Some(sequence),
        };
        let roles = sourcing_roles(&[link(&a, 1), link(&b, 2), link(&a, 3)]);
        assert_eq!(roles.per_link[&(a.clone(), comp.clone())], SOURCING_PRIMARY);
        assert_eq!(roles.per_link[&(b.clone(), comp)], SOURCING_BACKUP);
        assert_eq!(roles.per_supplier[&a], SOURCING_PRIMARY);
    }

    #[test]
    fn exposure_is_omitted_where_an_at_risk_orders_value_is_missing() {
        // CellTech sources the cause of two at-risk orders: sale.order/2 (€5,000) and the
        // flagship order, ingested without its value. A partial €5,000 would understate it,
        // so its exposure is omitted, while the known sum is served once both values exist.
        let w = world();
        let so2 = iri("sale.order", 2);
        let at_risk = BTreeSet::from([w.so.clone(), so2.clone()]);
        let causes = CauseMap::from_rows(
            &[
                row(&[("so", &w.so), ("comp", &w.battery), ("shortfall", "13")]),
                row(&[("so", &so2), ("comp", &w.battery), ("shortfall", "2")]),
            ],
            &[],
        );
        let (path, chain) = (path(&w), late_chain(&w));
        let markers = flagship(&w, &at_risk, &causes, &path, &chain, true);
        let sourcing = vec![
            SourcingLink {
                supplier: w.celltech.clone(),
                component: w.battery.clone(),
                sequence: Some(1),
            },
            SourcingLink {
                supplier: w.frameco.clone(),
                component: w.frame.clone(),
                sequence: Some(1),
            },
        ];
        let structural = |flagship_value: Option<&str>| {
            let mut flagship = row(&[
                ("kind", "dependent_order"),
                ("supplier", &w.celltech),
                ("order", &w.so),
            ]);
            if let Some(value) = flagship_value {
                flagship.insert("order_value".to_owned(), value.to_owned());
            }
            vec![
                row(&[("kind", "supplier"), ("supplier", &w.celltech)]),
                row(&[("kind", "supplier"), ("supplier", &w.frameco)]),
                flagship,
                row(&[
                    ("kind", "dependent_order"),
                    ("supplier", &w.celltech),
                    ("order", &so2),
                    ("order_value", "5000.0"),
                ]),
            ]
        };
        let rows = build_rows(Tab::Suppliers, structural(None), &sourcing, &markers);
        assert!(
            !rows[0].contains_key("exposure"),
            "CellTech's sum would need the missing value: omitted, not understated as 5000"
        );
        assert_eq!(
            get(&rows[1], "exposure"),
            Some("0.00"),
            "FrameCo sources no cause: its 0.00 is known"
        );
        let rows = build_rows(
            Tab::Suppliers,
            structural(Some("18000.0")),
            &sourcing,
            &markers,
        );
        assert_eq!(
            get(&rows[0], "exposure"),
            Some("23000.00"),
            "both values known"
        );
    }

    #[test]
    fn a_dual_role_partner_lights_each_edge_by_the_role_it_names() {
        // VeloMax is the path's customer and a supplier of the chain component, but not the
        // chain's supplier. Its customer edge is lit; its sourcing edge is not.
        let w = world();
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let (path, chain) = (path(&w), late_chain(&w));
        let markers = flagship(&w, &at_risk, &causes, &path, &chain, true);
        let ns = "https://ekg.example/ontology#";
        let structural = vec![
            row(&[("kind", "node"), ("node", &w.so), ("role", "SalesOrder")]),
            row(&[
                ("kind", "node"),
                ("node", &w.customer),
                ("role", "Customer"),
            ]),
            row(&[
                ("kind", "node"),
                ("node", &w.customer),
                ("role", "Supplier"),
            ]),
            row(&[
                ("kind", "node"),
                ("node", &w.battery),
                ("role", "Component"),
            ]),
            row(&[
                ("kind", "edge"),
                ("s", &w.so),
                ("p", &format!("{ns}for_customer")),
                ("o", &w.customer),
            ]),
            row(&[
                ("kind", "edge"),
                ("s", &w.customer),
                ("p", &format!("{ns}supplies_product")),
                ("o", &w.battery),
            ]),
        ];
        let rows = build_rows(Tab::Graph, structural, &[], &markers);
        let role_row = |role: &str| get_by(&rows, &[("node", &w.customer), ("role", role)]);
        assert_eq!(get(role_row("Customer"), "on_path"), Some("true"));
        assert_eq!(get(role_row("Supplier"), "on_path"), Some("false"));
        let edge = |p: &str| {
            rows.iter()
                .find(|r| get(r, "p").is_some_and(|x| x.ends_with(p)))
                .and_then(|r| get(r, "on_path"))
        };
        assert_eq!(edge("for_customer"), Some("true"));
        assert_eq!(
            edge("supplies_product"),
            Some("false"),
            "the partner's Supplier role is off the path"
        );
    }

    #[test]
    fn a_lit_vendor_without_a_sourcing_link_carries_no_sourcing_detail() {
        // The chain follows the rule's actual cause: the late PO's vendor, even when that
        // vendor has no sourcing link for the component. The Suppliers derivations read the
        // links only, so that lit vendor serves no sourcing role and no exposure credit.
        // This test pins that known limitation.
        let w = world();
        let vendor = iri("res.partner", 12);
        let at_risk = BTreeSet::from([w.so.clone()]);
        let causes = cause_map(&w);
        let rows = vec![row(&[
            ("hop", "line"),
            ("cause", &w.battery),
            ("po", &w.po),
            ("vendor", &vendor),
            ("arrival", "2027-11-15"),
            ("late", "true"),
        ])];
        let path = path(&w);
        let chain =
            super::super::chain::choose_chain(causes.of(&w.so).expect("causes"), &path, &rows);
        assert_eq!(chain.supplier.as_deref(), Some(vendor.as_str()));
        let markers = flagship(&w, &at_risk, &causes, &path, &chain, true);
        let sourcing = vec![SourcingLink {
            supplier: w.celltech.clone(),
            component: w.battery.clone(),
            sequence: Some(1),
        }];
        let structural = vec![
            row(&[("kind", "supplier"), ("supplier", &vendor)]),
            row(&[("kind", "supplier"), ("supplier", &w.celltech)]),
            row(&[
                ("kind", "dependent_order"),
                ("supplier", &w.celltech),
                ("order", &w.so),
                ("order_value", "18000.0"),
            ]),
        ];
        let rows = build_rows(Tab::Suppliers, structural, &sourcing, &markers);
        let lit = &rows[0];
        assert_eq!(
            get(lit, "on_path"),
            Some("true"),
            "the actual cause's vendor"
        );
        assert!(
            !lit.contains_key("sourcing"),
            "no link, so no sourcing role"
        );
        assert_eq!(
            get(lit, "exposure"),
            Some("0.00"),
            "no link, so no exposure credit"
        );
        assert_eq!(get(&rows[1], "on_path"), Some("false"));
        assert_eq!(get(&rows[1], "exposure"), Some("18000.00"));
        // The Graph lights the vendor and its PO; no Component-to-Supplier edge exists to light.
        let graph = vec![
            row(&[("kind", "node"), ("node", &vendor), ("role", "Supplier")]),
            row(&[("kind", "node"), ("node", &w.po), ("role", "PurchaseOrder")]),
            row(&[
                ("kind", "edge"),
                ("s", &w.po),
                ("p", "from_supplier"),
                ("o", &vendor),
            ]),
        ];
        let rows = build_rows(Tab::Graph, graph, &[], &markers);
        assert_eq!(get(node(&rows, &vendor), "on_path"), Some("true"));
        assert_eq!(get(node(&rows, &w.po), "on_path"), Some("true"));
        assert_eq!(get(&rows[2], "on_path"), Some("true"));
    }
}
