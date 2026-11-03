//! The selected point's lit chain: one entity per column/stage, chosen deterministically, so
//! the tabs draw a single hot path.
//!
//! Two halves:
//!
//! - [`PathHops`]: the rule-independent hops from `path.rq`: customer, order, product, BOM
//!   and the fulfilling manufacturing order (for a manufacturing-order subject, the MO hop
//!   is the subject). Always served.
//! - [`Chain`]: the cause-dependent hops from `path-causes.rq`: the cause component, the
//!   purchase order following the rule's actual cause, its supplier, and the sub-assemblies
//!   and BOMs on the route from the ordered product down to the component. Known only when
//!   the rules, the cause map and the cause query could all be read.
//!
//! Pure functions over query rows, so the tests need no store.

use std::collections::BTreeSet;

use super::Row;
use super::causes::Causes;
use crate::ontology;

/// The rule-independent hops of the selected point's path (one entity each, lowest IRI).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathHops {
    /// The order's customer.
    pub customer: Option<String>,
    /// The sales order (the subject, or the order a manufacturing-order subject fulfils).
    pub order: Option<String>,
    /// The order's product (for a manufacturing-order subject, the line product it builds).
    pub product: Option<String>,
    /// The BOM producing that product (lowest IRI among its BOMs).
    pub bom: Option<String>,
    /// The open fulfilling manufacturing order building the kept product (lowest IRI; else
    /// the lowest-IRI open fulfilling MO). For an MO subject, the subject itself.
    pub mo: Option<String>,
}

impl PathHops {
    /// Read the hops from `path.rq` rows, per hop the lowest canonical IRI, tied together so a
    /// multi-line order's hops stay one connected path:
    ///
    /// - product: for a manufacturing-order subject, the line product it builds; else the
    ///   lowest-IRI line product;
    /// - BOM: the BOM such a subject builds from, else the lowest-IRI BOM producing the product;
    /// - MO: the subject itself (`self_mo`); else the lowest-IRI open fulfilling MO that builds
    ///   the product, else the lowest-IRI open fulfilling MO.
    #[must_use]
    pub fn from_rows(rows: &[Row]) -> Self {
        let lowest = |hop: &str, of: Option<&str>| {
            rows.iter()
                .filter(|row| row.get("hop").is_some_and(|h| h == hop))
                .filter(|row| of.is_none_or(|of| row.get("of").is_some_and(|o| o == of)))
                .filter_map(|row| row.get("entity"))
                .filter(|iri| ontology::is_canonical_entity_iri(iri))
                .min()
                .cloned()
        };
        let self_mo = lowest("self_mo", None);
        // A manufacturing-order subject lights the line product it builds (and the BOM it
        // builds from), so its Graph shows the product this MO makes, not another line's.
        let self_product = rows
            .iter()
            .filter(|row| row.get("hop").is_some_and(|h| h == "self_mo"))
            .filter_map(|row| row.get("of"))
            .filter(|of| is_line_product(rows, of))
            .filter(|of| ontology::is_canonical_entity_iri(of))
            .min()
            .cloned();
        let product = self_product.clone().or_else(|| lowest("product", None));
        let bom = product.as_deref().and_then(|product| {
            self_product
                .as_ref()
                .and_then(|_| lowest("self_bom", Some(product)))
                .or_else(|| lowest("bom", Some(product)))
        });
        let mo = self_mo.or_else(|| {
            product
                .as_deref()
                .and_then(|product| lowest("mo", Some(product)))
                .or_else(|| lowest("mo", None))
        });
        Self {
            customer: lowest("customer", None),
            order: lowest("order", None),
            product,
            bom,
            mo,
        }
    }
}

/// Whether `iri` is one of the order's line products (a `product` row's entity).
fn is_line_product(rows: &[Row], iri: &str) -> bool {
    rows.iter().any(|row| {
        row.get("hop").is_some_and(|h| h == "product")
            && row.get("entity").is_some_and(|e| e == iri)
    })
}

/// The cause-dependent hops of the selected point's path. `Default` (all `None`) is the
/// chain of a point with no cause component (an infeasible or contended order, or a rule
/// that has cleared): nothing on those columns is on its path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chain {
    /// The point's primary cause: the first canonical component in the rule's cause order
    /// (purchased before made, the largest shortfall, the IRI). It is the component the
    /// monitor's reason names.
    pub component: Option<String>,
    /// The chosen purchase order's vendor. Only when the component has no open inbound line
    /// at all, its lowest-`sequence` sourcing supplier.
    pub supplier: Option<String>,
    /// The purchase order following the rule's actual cause, if the component has any open
    /// inbound line.
    pub po: Option<String>,
    /// Whether the chosen PO's line arrives after the commitment (only then is it a cause).
    pub po_late: bool,
    /// The BOMs and sub-assemblies on the BOM route from the path's ordered product down to
    /// the component, so a cause two or more BOM levels deep draws no gap. One BOM per product
    /// on the way (the path's BOM hop for the ordered product, else the lowest IRI); for a
    /// component directly in the product's BOM the route is just that BOM. The route forks only
    /// when one BOM uses the component through two sub-assemblies; both are then on the path.
    pub route: BTreeSet<String>,
}

/// One open inbound line candidate from `path-causes.rq`.
struct Line<'a> {
    po: &'a str,
    vendor: Option<&'a str>,
    arrival: &'a str,
    late: bool,
}

/// Choose the chain from the point's causes and the `path-causes.rq` rows:
///
/// 1. the first canonical component in the rule's cause order ([`Causes::ranked`]: the
///    risk SELECT's `ORDER BY`, purchased before made, the largest shortfall, the IRI).
///    There is no second ranking: a point opening at this moment carries the same first
///    row's reason, so the inbox and the lit chain agree when the point opens (a live point
///    keeps its opening reason while the chain follows the current rows), and a purchased
///    cause keeps the supply columns lit while one is short;
/// 2. among all its open inbound lines, whatever the vendor, prefer those arriving after
///    the commitment (the ones the rule could not count), earliest first; otherwise the
///    earliest arrival (ties: lowest PO IRI);
/// 3. the supplier is that PO's vendor; only when the component has no open inbound line does
///    it fall back to the lowest-`sequence` sourcing link (unsequenced last, ties: lowest IRI),
///    lighting no PO;
/// 4. the route: walking the `route` steps down from `path`'s ordered product, every BOM and
///    sub-assembly that leads to the component (see [`Chain::route`]).
#[must_use]
pub fn choose_chain(causes: &Causes, path: &PathHops, rows: &[Row]) -> Chain {
    let Some(component) = causes
        .ranked
        .iter()
        .find(|iri| ontology::is_canonical_entity_iri(iri))
    else {
        return Chain::default();
    };

    let of_component = |hop: &'static str| {
        rows.iter().filter(move |row| {
            row.get("hop").is_some_and(|h| h == hop)
                && row.get("cause").is_some_and(|c| c == component)
        })
    };

    let line = of_component("line")
        .filter_map(|row| {
            let po = row
                .get("po")
                .filter(|iri| ontology::is_canonical_entity_iri(iri))?;
            Some(Line {
                po,
                vendor: row
                    .get("vendor")
                    .map(String::as_str)
                    .filter(|iri| ontology::is_canonical_entity_iri(iri)),
                arrival: row.get("arrival").map_or("", String::as_str),
                late: row.get("late").is_some_and(|v| v == "true" || v == "1"),
            })
        })
        .min_by(|a, b| {
            // Late lines first (`!late` sorts false before true), then the earliest arrival.
            (!a.late, a.arrival, a.po).cmp(&(!b.late, b.arrival, b.po))
        });

    let route = route(of_component("route"), component, path);

    if let Some(line) = line {
        return Chain {
            component: Some(component.clone()),
            supplier: line.vendor.map(str::to_owned),
            po: Some(line.po.to_owned()),
            po_late: line.late,
            route,
        };
    }

    let fallback = of_component("link")
        .filter_map(|row| {
            let supplier = row
                .get("supplier")
                .filter(|iri| ontology::is_canonical_entity_iri(iri))?;
            let sequence = row.get("sequence").and_then(|s| s.parse::<i64>().ok());
            Some((sequence.is_none(), sequence, supplier))
        })
        .min()
        .map(|(_, _, supplier)| supplier.clone());

    Chain {
        component: Some(component.clone()),
        supplier: fallback,
        po: None,
        po_late: false,
        route,
    }
}

/// Walk the component's `route` steps (parent product, BOM, child) down from the path's
/// ordered product: at each product one BOM (the path's BOM hop at the top, else the lowest
/// IRI among its BOMs that lead to the component), and every sub-assembly that BOM passes on
/// the way to `component`. The query only returns steps that lead to the component, so
/// everything reached is on a route to it. Without a product hop there is no route. A
/// product hop whose tree does not reach the component (a multi-line order whose cause sits
/// under another line's product, since the lowest-IRI product is kept) has no route either.
fn route<'r>(
    steps: impl Iterator<Item = &'r Row>,
    component: &str,
    path: &PathHops,
) -> BTreeSet<String> {
    let canonical = |row: &'r Row, key: &str| {
        row.get(key)
            .map(String::as_str)
            .filter(|iri| ontology::is_canonical_entity_iri(iri))
    };
    let steps: Vec<(&str, &str, &str)> = steps
        .filter_map(|row| {
            Some((
                canonical(row, "parent")?,
                canonical(row, "bom")?,
                canonical(row, "child")?,
            ))
        })
        .collect();
    let mut route = BTreeSet::new();
    let Some(product) = path.product.as_deref() else {
        return route;
    };
    let (mut seen, mut frontier) = (BTreeSet::from([product]), vec![product]);
    while let Some(parent) = frontier.pop() {
        let boms: BTreeSet<&str> = steps
            .iter()
            .filter(|(from, _, _)| *from == parent)
            .map(|(_, bom, _)| *bom)
            .collect();
        let top_bom = path
            .bom
            .as_deref()
            .filter(|bom| parent == product && boms.contains(bom));
        let Some(bom) = top_bom.or_else(|| boms.first().copied()) else {
            continue;
        };
        route.insert(bom.to_owned());
        for &(from, via, child) in &steps {
            if from == parent && via == bom && child != component && seen.insert(child) {
                route.insert(child.to_owned());
                frontier.push(child);
            }
        }
    }
    route
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }

    /// Causes in the rule's order (as its rows arrive).
    fn causes(ranked: &[&str]) -> Causes {
        Causes {
            ranked: ranked.iter().map(|iri| (*iri).to_owned()).collect(),
            ..Causes::default()
        }
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn line(comp: &str, po: &str, vendor: &str, arrival: &str, late: bool) -> Row {
        row(&[
            ("hop", "line"),
            ("cause", comp),
            ("po", po),
            ("vendor", vendor),
            ("arrival", arrival),
            ("late", if late { "true" } else { "false" }),
        ])
    }

    fn link(comp: &str, supplier: &str, sequence: Option<&str>) -> Row {
        let mut r = row(&[("hop", "link"), ("cause", comp), ("supplier", supplier)]);
        if let Some(seq) = sequence {
            r.insert("sequence".to_owned(), seq.to_owned());
        }
        r
    }

    #[test]
    fn a_late_backup_line_beats_an_on_time_primary_line() {
        // The rule counted the primary's on-time line; what it could not count is the backup's
        // late line, so the chain follows the actual cause: the backup and its late PO.
        let comp = iri("product.product", 4);
        let (primary, backup) = (iri("res.partner", 6), iri("res.partner", 9));
        let (po_on_time, po_late) = (iri("purchase.order", 1), iri("purchase.order", 2));
        let rows = vec![
            line(&comp, &po_on_time, &primary, "2027-10-07T00:00:00", false),
            line(&comp, &po_late, &backup, "2027-11-15T00:00:00", true),
            link(&comp, &primary, Some("1")),
            link(&comp, &backup, Some("2")),
        ];
        let chain = choose_chain(&causes(&[&comp]), &PathHops::default(), &rows);
        assert_eq!(chain.component.as_deref(), Some(comp.as_str()));
        assert_eq!(chain.po.as_deref(), Some(po_late.as_str()));
        assert_eq!(chain.supplier.as_deref(), Some(backup.as_str()));
        assert!(chain.po_late, "the late line is a cause");
    }

    #[test]
    fn with_nothing_late_the_earliest_line_is_lit_but_is_not_a_cause() {
        let comp = iri("product.product", 4);
        let rows = vec![
            line(
                &comp,
                &iri("purchase.order", 7),
                &iri("res.partner", 6),
                "2027-10-11",
                false,
            ),
            line(
                &comp,
                &iri("purchase.order", 3),
                &iri("res.partner", 8),
                "2027-10-08",
                false,
            ),
        ];
        let chain = choose_chain(&causes(&[&comp]), &PathHops::default(), &rows);
        assert_eq!(chain.po, Some(iri("purchase.order", 3)), "earliest arrival");
        assert_eq!(
            chain.supplier,
            Some(iri("res.partner", 8)),
            "that PO's vendor"
        );
        assert!(
            !chain.po_late,
            "a quantity shortfall points at no late supplier"
        );
    }

    #[test]
    fn several_late_lines_pick_the_earliest_late_one() {
        let comp = iri("product.product", 4);
        let rows = vec![
            line(
                &comp,
                &iri("purchase.order", 5),
                &iri("res.partner", 6),
                "2027-12-07",
                true,
            ),
            line(
                &comp,
                &iri("purchase.order", 6),
                &iri("res.partner", 6),
                "2027-11-07",
                true,
            ),
            line(
                &comp,
                &iri("purchase.order", 1),
                &iri("res.partner", 6),
                "2027-09-07",
                false,
            ),
        ];
        let chain = choose_chain(&causes(&[&comp]), &PathHops::default(), &rows);
        assert_eq!(chain.po, Some(iri("purchase.order", 6)));
        assert!(chain.po_late);
    }

    #[test]
    fn with_nothing_open_the_lowest_sequence_supplier_is_lit_and_no_po() {
        let comp = iri("product.product", 4);
        let rows = vec![
            link(&comp, &iri("res.partner", 9), Some("2")),
            link(&comp, &iri("res.partner", 3), None),
            link(&comp, &iri("res.partner", 6), Some("1")),
        ];
        let chain = choose_chain(&causes(&[&comp]), &PathHops::default(), &rows);
        assert_eq!(chain.component, Some(comp));
        assert_eq!(
            chain.supplier,
            Some(iri("res.partner", 6)),
            "lowest sequence"
        );
        assert_eq!(chain.po, None, "no PO is lit when nothing is open");
        assert!(!chain.po_late);
    }

    #[test]
    fn the_rules_first_cause_is_chosen_never_a_ranking_of_its_own() {
        // The rule orders purchased before made: after the battery's governed fix the made
        // Drive (product 2) has the larger shortfall and the lower IRI, yet the rule still lists
        // the purchased battery first, and the chain follows the rule, not a largest-shortfall or
        // IRI pick of its own.
        let (drive, battery) = (iri("product.product", 2), iri("product.product", 4));
        let chain = choose_chain(&causes(&[&battery, &drive]), &PathHops::default(), &[]);
        assert_eq!(chain.component, Some(battery.clone()));
        // Whatever the rule lists first is lit, even a smaller shortfall.
        let chain = choose_chain(&causes(&[&drive, &battery]), &PathHops::default(), &[]);
        assert_eq!(chain.component, Some(drive));
        // Lines of another cause component never leak into the chosen one's PO.
        let rows = vec![line(
            &iri("product.product", 5),
            &iri("purchase.order", 9),
            &iri("res.partner", 1),
            "2027-10-07",
            true,
        )];
        let chain = choose_chain(&causes(&[&battery]), &PathHops::default(), &rows);
        assert_eq!(chain.po, None);
    }

    #[test]
    fn the_chain_lights_the_component_the_monitors_reason_names() {
        // The flagship shape: the rule finds the order short of the made Drive Subassembly (10)
        // and the purchased battery (13). The monitor opens the point with the first row's
        // reason; the chain must light the same component.
        let so = iri("sale.order", 1);
        let (drive, battery) = (iri("product.product", 2), iri("product.product", 4));
        let po = iri("purchase.order", 1);
        // The rows as the rule's ORDER BY returns them: purchased first.
        let rule_rows: Vec<Row> = [(&battery, "13"), (&drive, "10")]
            .iter()
            .map(|(comp, shortfall)| row(&[("so", &so), ("comp", comp), ("shortfall", shortfall)]))
            .collect();
        let reason_names = rule_rows
            .iter()
            .find(|r| r.get("so") == Some(&so))
            .and_then(|r| r.get("comp"))
            .expect("the monitor's first verdict");
        let map = super::super::causes::CauseMap::from_rows(&rule_rows, &[]);
        let rows = vec![line(
            &battery,
            &po,
            &iri("res.partner", 6),
            "2027-11-15",
            true,
        )];
        let chain = choose_chain(map.of(&so).expect("causes"), &PathHops::default(), &rows);
        assert_eq!(chain.component.as_ref(), Some(reason_names));
        assert_eq!(chain.po, Some(po));
    }

    #[test]
    fn choose_chain_admits_only_canonical_entity_iris() {
        let comp = iri("product.product", 4);
        let provenance = |iri: &str| format!("{iri}#source");
        // A non-canonical cause is skipped for the next canonical one.
        let chain = choose_chain(&causes(&[&provenance(&comp)]), &PathHops::default(), &[]);
        assert_eq!(
            chain,
            Chain::default(),
            "no canonical cause, so empty chain"
        );
        let (po, vendor, linked) = (
            iri("purchase.order", 1),
            iri("res.partner", 6),
            iri("res.partner", 7),
        );
        let rows = vec![
            // A provenance PO is never a chain PO …
            line(&comp, &provenance(&po), &vendor, "2027-10-07", true),
            // … a provenance vendor never the supplier (the PO itself still is lit) …
            line(
                &comp,
                &iri("purchase.order", 2),
                &provenance(&vendor),
                "2027-10-08",
                false,
            ),
        ];
        let chain = choose_chain(
            &causes(&[&provenance(&comp), &comp]),
            &PathHops::default(),
            &rows,
        );
        assert_eq!(chain.component, Some(comp.clone()));
        assert_eq!(chain.po, Some(iri("purchase.order", 2)));
        assert_eq!(chain.supplier, None);
        // … and a provenance link supplier never the fallback supplier.
        let rows = vec![
            link(&comp, &provenance(&linked), Some("1")),
            link(&comp, &vendor, Some("2")),
        ];
        let chain = choose_chain(&causes(&[&comp]), &PathHops::default(), &rows);
        assert_eq!(chain.supplier, Some(vendor));
    }

    #[test]
    fn no_cause_component_means_an_empty_chain() {
        assert_eq!(
            choose_chain(&Causes::default(), &PathHops::default(), &[]),
            Chain::default()
        );
    }

    #[test]
    fn an_mo_subject_is_its_own_mo_hop() {
        let (subject, sibling) = (iri("mrp.production", 7), iri("mrp.production", 2));
        let rows = vec![
            row(&[("hop", "order"), ("entity", &iri("sale.order", 1))]),
            row(&[("hop", "customer"), ("entity", &iri("res.partner", 11))]),
            row(&[("hop", "product"), ("entity", &iri("product.product", 1))]),
            row(&[
                ("hop", "bom"),
                ("entity", &iri("mrp.bom", 1)),
                ("of", &iri("product.product", 1)),
            ]),
            row(&[("hop", "mo"), ("entity", &sibling)]),
            row(&[("hop", "mo"), ("entity", &subject)]),
            row(&[("hop", "self_mo"), ("entity", &subject)]),
        ];
        let hops = PathHops::from_rows(&rows);
        assert_eq!(hops.mo, Some(subject), "an MO subject lights itself");
        assert_eq!(hops.order, Some(iri("sale.order", 1)));
        // For an order subject (no self_mo) the lowest-IRI fulfilling MO is the hop.
        let hops = PathHops::from_rows(&rows[..6]);
        assert_eq!(hops.mo, Some(sibling));
    }

    #[test]
    fn path_hops_admit_only_canonical_entity_iris() {
        let rows = vec![
            row(&[
                ("hop", "order"),
                ("entity", "https://ekg.example/id/sale.order/1#source"),
            ]),
            row(&[("hop", "customer"), ("entity", "urn:x")]),
        ];
        assert_eq!(PathHops::from_rows(&rows), PathHops::default());
    }

    fn step(comp: &str, parent: &str, bom: &str, child: &str) -> Row {
        row(&[
            ("hop", "route"),
            ("cause", comp),
            ("parent", parent),
            ("bom", bom),
            ("child", child),
        ])
    }

    #[test]
    fn a_deep_cause_lights_the_sub_assembly_route_down_to_it() {
        // E-Bike (1) is produced by BOM 1, which uses Drive (2), produced by BOM 2, which uses
        // Motor (5); the battery (4) sits directly in BOM 1. The motor is the cause: the Drive
        // and its BOM join the lit path.
        let (ebike, drive, battery, motor) = (
            iri("product.product", 1),
            iri("product.product", 2),
            iri("product.product", 4),
            iri("product.product", 5),
        );
        let (bom1, bom2) = (iri("mrp.bom", 1), iri("mrp.bom", 2));
        let path = PathHops {
            product: Some(ebike.clone()),
            bom: Some(bom1.clone()),
            ..PathHops::default()
        };
        let rows = vec![
            step(&motor, &ebike, &bom1, &drive),
            step(&motor, &drive, &bom2, &motor),
            // Another cause's steps never leak into the chosen one's route.
            step(&battery, &ebike, &bom1, &battery),
            // A step not reachable from the ordered product (another line's product).
            step(
                &motor,
                &iri("product.product", 9),
                &iri("mrp.bom", 9),
                &motor,
            ),
        ];
        let chain = choose_chain(&causes(&[&motor]), &path, &rows);
        assert_eq!(chain.component, Some(motor));
        assert_eq!(
            chain.route,
            BTreeSet::from([bom1.clone(), drive.clone(), bom2]),
            "the sub-assembly and both BOMs, and nothing off the route"
        );
        // A component directly in the product's BOM has only that BOM on its route.
        let chain = choose_chain(&causes(&[&battery]), &path, &rows);
        assert_eq!(chain.route, BTreeSet::from([bom1.clone()]));
        // A made cause (the rule lists it only when no purchased cause is short): the
        // route reaches it, and it is the chain component itself.
        let rows = vec![step(&drive, &ebike, &bom1, &drive)];
        let chain = choose_chain(&causes(&[&drive]), &path, &rows);
        assert_eq!(chain.component, Some(drive));
        assert_eq!(chain.route, BTreeSet::from([bom1]));
        // Without the ordered product there is nothing to walk from.
        let chain = choose_chain(&causes(&[&battery]), &PathHops::default(), &rows);
        assert!(chain.route.is_empty());
    }

    #[test]
    fn the_bom_hop_is_the_bom_of_the_chosen_product() {
        // A two-line order: products 3 and 10. The kept product is the lowest IRI string
        // (`…/10` sorts before `…/3`); the kept BOM must be its BOM (9), even though BOM 1
        // (producing product 3) has the lower IRI. Otherwise the hops are disconnected.
        let (p10, p3) = (iri("product.product", 10), iri("product.product", 3));
        let rows = vec![
            row(&[("hop", "product"), ("entity", &p3)]),
            row(&[("hop", "product"), ("entity", &p10)]),
            row(&[("hop", "bom"), ("entity", &iri("mrp.bom", 1)), ("of", &p3)]),
            row(&[("hop", "bom"), ("entity", &iri("mrp.bom", 9)), ("of", &p10)]),
        ];
        let hops = PathHops::from_rows(&rows);
        assert_eq!(hops.product, Some(p10));
        assert_eq!(hops.bom, Some(iri("mrp.bom", 9)));
    }

    #[test]
    fn the_route_takes_one_bom_per_product() {
        // The E-Bike has two BOMs that both lead to the motor (BOM 1 and BOM 3, the path's BOM
        // hop); the Drive has two as well (BOM 2 and BOM 4). The route takes the path's BOM at
        // the top and the lowest IRI below, never a fork of dead ends.
        let (ebike, drive, motor) = (
            iri("product.product", 1),
            iri("product.product", 2),
            iri("product.product", 5),
        );
        let bom = |id| iri("mrp.bom", id);
        let path = PathHops {
            product: Some(ebike.clone()),
            bom: Some(bom(3)),
            ..PathHops::default()
        };
        let rows = vec![
            step(&motor, &ebike, &bom(1), &drive),
            step(&motor, &ebike, &bom(3), &drive),
            step(&motor, &drive, &bom(4), &motor),
            step(&motor, &drive, &bom(2), &motor),
        ];
        let chain = choose_chain(&causes(&[&motor]), &path, &rows);
        assert_eq!(
            chain.route,
            BTreeSet::from([bom(3), drive.clone(), bom(2)]),
            "the path's BOM at the top, the lowest-IRI BOM below"
        );
        // Without a usable path BOM the top takes the lowest IRI too.
        let path = PathHops {
            bom: Some(bom(9)),
            ..path
        };
        let chain = choose_chain(&causes(&[&motor]), &path, &rows);
        assert_eq!(chain.route, BTreeSet::from([bom(1), drive, bom(2)]));
    }

    #[test]
    fn the_mo_hop_builds_the_kept_product() {
        // A two-line order (products 10 and 3; `…/10` is kept). MO 12 builds product 3, MO 15
        // builds product 10: the lit MO is the one building the lit product, not the lowest.
        let (p10, p3) = (iri("product.product", 10), iri("product.product", 3));
        let (mo12, mo15) = (iri("mrp.production", 12), iri("mrp.production", 15));
        let order_rows = vec![
            row(&[("hop", "order"), ("entity", &iri("sale.order", 1))]),
            row(&[("hop", "product"), ("entity", &p3)]),
            row(&[("hop", "product"), ("entity", &p10)]),
            row(&[("hop", "bom"), ("entity", &iri("mrp.bom", 1)), ("of", &p3)]),
            row(&[("hop", "bom"), ("entity", &iri("mrp.bom", 9)), ("of", &p10)]),
            row(&[("hop", "mo"), ("entity", &mo12), ("of", &p3)]),
            row(&[("hop", "mo"), ("entity", &mo15), ("of", &p10)]),
        ];
        let hops = PathHops::from_rows(&order_rows);
        assert_eq!(hops.product, Some(p10.clone()));
        assert_eq!(
            hops.mo,
            Some(mo15.clone()),
            "the MO building the kept product"
        );
        // No MO builds the kept product: the lowest open fulfilling MO still lights.
        let hops = PathHops::from_rows(&order_rows[..6]);
        assert_eq!(hops.mo, Some(mo12.clone()));

        // MO 12 as the subject: it lights itself, the line product it builds and its own BOM,
        // not the lowest-IRI line product and that product's BOM.
        let mut mo_rows = order_rows.clone();
        mo_rows.push(row(&[("hop", "self_mo"), ("entity", &mo12), ("of", &p3)]));
        mo_rows.push(row(&[
            ("hop", "self_bom"),
            ("entity", &iri("mrp.bom", 4)),
            ("of", &p3),
        ]));
        let hops = PathHops::from_rows(&mo_rows);
        assert_eq!(hops.mo, Some(mo12.clone()));
        assert_eq!(hops.product, Some(p3.clone()));
        assert_eq!(
            hops.bom,
            Some(iri("mrp.bom", 4)),
            "the BOM the MO builds from"
        );
        // An MO subject building something that is no line product keeps the order's hops.
        let mut other = order_rows;
        other.push(row(&[
            ("hop", "self_mo"),
            ("entity", &mo12),
            ("of", &iri("product.product", 77)),
        ]));
        let hops = PathHops::from_rows(&other);
        assert_eq!(hops.product, Some(p10));
        assert_eq!(hops.bom, Some(iri("mrp.bom", 9)));
        assert_eq!(hops.mo, Some(mo12));
    }
}
