//! The coupling wording: why a portfolio's members are coupled, as sentences an operator can
//! read, worded from the facts each coupling link recorded when the portfolio formed. It is
//! never worded from the link's reason text, and never model-generated.
//!
//! The facts are the capability's own ([`COUPLING_KIND_KEY`] and the keys beside it); the
//! backend's coupling interpreter fills them, and the neutral base carries them opaquely.
//! Texts keep the raw entity ids; a view labels them.

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{CouplingLink, EntityUri};

use super::delivery::model_of;

/// The fact naming what kind of link a coupling is.
pub const COUPLING_KIND_KEY: &str = "kind";
/// The fact naming the work center two contending orders share.
pub const COUPLING_VIA_KEY: &str = "via";
/// The fact naming that work center's capacity.
pub const COUPLING_CAPACITY_KEY: &str = "capacity";
/// The kind of a link between a manufacturing order and the sales order it fulfils.
pub const COUPLING_FULFILLS: &str = "fulfills";
/// The kind of a link between two orders booked on one work center beyond its capacity.
pub const COUPLING_CONTENTION: &str = "contention";

/// Two coupled orders, by IRI.
type Pair<'a> = (&'a EntityUri, &'a EntityUri);

/// The Odoo model of a manufacturing order.
const MANUFACTURING_ORDER: &str = "mrp.production";
/// The Odoo model of a sales order.
const SALES_ORDER: &str = "sale.order";

/// Word a portfolio's coupling `links` as sentences, deterministically:
///
/// - the orders contending for one work center are named together, one sentence per connected
///   group of them, however many pairs contend: "{m1}, {m2} and {m3} are booked on {via} at
///   the same time, beyond its capacity of {n}.";
/// - each fulfils link gives "{mo} builds {so}.";
/// - any other link, or one whose facts are missing, keeps its recorded reason.
///
/// Sentences come ordered by work center, then by order, then the kept reasons; members are
/// listed in IRI order, and a repeated sentence is given once.
#[must_use]
pub fn describe_coupling(links: &[CouplingLink]) -> Vec<String> {
    // Per work center: its capacity and the contending pairs on it.
    let mut centers: BTreeMap<&str, (String, Vec<Pair<'_>>)> = BTreeMap::new();
    let mut builds: BTreeSet<Pair<'_>> = BTreeSet::new();
    let mut kept: Vec<&str> = Vec::new();
    for link in links {
        let fact = |key: &str| link.facts.get(key).map(String::as_str);
        match fact(COUPLING_KIND_KEY) {
            Some(COUPLING_CONTENTION) => {
                if let (Some(via), Some(capacity)) =
                    (fact(COUPLING_VIA_KEY), fact(COUPLING_CAPACITY_KEY))
                {
                    centers
                        .entry(via)
                        .or_insert_with(|| (whole(capacity), Vec::new()))
                        .1
                        .push((&link.a, &link.b));
                    continue;
                }
            }
            Some(COUPLING_FULFILLS) => {
                if let Some(pair) = order_roles(link) {
                    builds.insert(pair);
                    continue;
                }
            }
            _ => {}
        }
        kept.push(&link.reason);
    }

    let mut sentences = Vec::new();
    for (via, (capacity, pairs)) in &centers {
        for group in groups(pairs) {
            sentences.push(format!(
                "{} are booked on {via} at the same time, beyond its capacity of {capacity}.",
                enumerate(&group)
            ));
        }
    }
    for (mo, so) in builds {
        sentences.push(format!("{mo} builds {so}."));
    }
    for reason in kept {
        let reason = reason.to_owned();
        if !sentences.contains(&reason) {
            sentences.push(reason);
        }
    }
    sentences
}

/// A fulfils link's `(manufacturing order, sales order)`, told apart by the models in their
/// IRIs; `None` when the two are not one of each.
fn order_roles(link: &CouplingLink) -> Option<Pair<'_>> {
    match (model_of(&link.a)?, model_of(&link.b)?) {
        (MANUFACTURING_ORDER, SALES_ORDER) => Some((&link.a, &link.b)),
        (SALES_ORDER, MANUFACTURING_ORDER) => Some((&link.b, &link.a)),
        _ => None,
    }
}

/// The connected groups of `pairs`, each sorted, in order of their first member.
fn groups<'a>(pairs: &[Pair<'a>]) -> Vec<Vec<&'a EntityUri>> {
    let mut groups: Vec<BTreeSet<&EntityUri>> = Vec::new();
    for (a, b) in pairs {
        let mut merged: BTreeSet<&EntityUri> = BTreeSet::from([*a, *b]);
        groups.retain(|group| {
            if group.contains(a) || group.contains(b) {
                merged.extend(group.iter().copied());
                false
            } else {
                true
            }
        });
        groups.push(merged);
    }
    let mut groups: Vec<Vec<&EntityUri>> = groups
        .into_iter()
        .map(|group| group.into_iter().collect())
        .collect();
    groups.sort();
    groups
}

/// "a", "a and b", "a, b and c".
fn enumerate(members: &[&EntityUri]) -> String {
    match members {
        [] => String::new(),
        [only] => only.0.clone(),
        [init @ .., last] => format!(
            "{} and {}",
            init.iter()
                .map(|m| m.0.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            last.0
        ),
    }
}

/// A capacity as a whole number when it is one (`1.0` becomes `1`), else as recorded.
fn whole(capacity: &str) -> String {
    match capacity.trim().parse::<f64>() {
        #[allow(clippy::cast_possible_truncation)]
        Ok(n) if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 => (n as i64).to_string(),
        _ => capacity.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology;

    fn mo(id: i64) -> EntityUri {
        EntityUri(ontology::entity_iri("mrp.production", id))
    }

    fn so(id: i64) -> EntityUri {
        EntityUri(ontology::entity_iri("sale.order", id))
    }

    fn wc(id: i64) -> String {
        ontology::entity_iri("mrp.workcenter", id)
    }

    fn ordered(x: EntityUri, y: EntityUri) -> (EntityUri, EntityUri) {
        if x <= y { (x, y) } else { (y, x) }
    }

    fn contention(x: EntityUri, y: EntityUri, via: &str, capacity: &str) -> CouplingLink {
        let (a, b) = ordered(x, y);
        CouplingLink {
            reason: format!("linked: {a} and {b} contend for work center {via}"),
            a,
            b,
            facts: BTreeMap::from([
                (COUPLING_KIND_KEY.to_owned(), COUPLING_CONTENTION.to_owned()),
                (COUPLING_VIA_KEY.to_owned(), via.to_owned()),
                (COUPLING_CAPACITY_KEY.to_owned(), capacity.to_owned()),
            ]),
        }
    }

    fn fulfils(order: EntityUri, sale: EntityUri) -> CouplingLink {
        let reason = format!("linked: {order} fulfils {sale}");
        let (a, b) = ordered(order, sale);
        CouplingLink {
            a,
            b,
            reason,
            facts: BTreeMap::from([(COUPLING_KIND_KEY.to_owned(), COUPLING_FULFILLS.to_owned())]),
        }
    }

    /// The seeded portfolio: three orders on Assembly Line (capacity 1), and MO1 builds S00001.
    fn seeded() -> Vec<CouplingLink> {
        let mut links = vec![
            contention(mo(1), mo(2), &wc(1), "1"),
            contention(mo(1), mo(3), &wc(1), "1"),
            contention(mo(2), mo(3), &wc(1), "1"),
            fulfils(mo(1), so(1)),
        ];
        links.sort();
        links
    }

    #[test]
    fn the_seeded_portfolio_reads_as_two_sentences() {
        let sentences = describe_coupling(&seeded());
        assert_eq!(
            sentences,
            [
                format!(
                    "{}, {} and {} are booked on {} at the same time, beyond its capacity of 1.",
                    mo(1).0,
                    mo(2).0,
                    mo(3).0,
                    wc(1)
                ),
                format!("{} builds {}.", mo(1).0, so(1).0),
            ]
        );
        for sentence in &sentences {
            assert!(
                !sentence.contains("linked:"),
                "no pair is repeated: {sentence}"
            );
        }
    }

    #[test]
    fn each_work_center_and_each_unconnected_group_gets_its_own_sentence() {
        let links = vec![
            contention(mo(3), mo(4), &wc(2), "2.0"),
            contention(mo(1), mo(2), &wc(1), "1"),
            contention(mo(5), mo(6), &wc(1), "1"),
        ];
        assert_eq!(
            describe_coupling(&links),
            [
                format!(
                    "{} and {} are booked on {} at the same time, beyond its capacity of 1.",
                    mo(1).0,
                    mo(2).0,
                    wc(1)
                ),
                format!(
                    "{} and {} are booked on {} at the same time, beyond its capacity of 1.",
                    mo(5).0,
                    mo(6).0,
                    wc(1)
                ),
                format!(
                    "{} and {} are booked on {} at the same time, beyond its capacity of 2.",
                    mo(3).0,
                    mo(4).0,
                    wc(2)
                ),
            ]
        );
    }

    #[test]
    fn a_link_without_facts_keeps_its_reason() {
        let mut bare = contention(mo(1), mo(2), &wc(1), "1");
        bare.facts.clear();
        let mut half = contention(mo(3), mo(4), &wc(1), "1");
        half.facts.remove(COUPLING_CAPACITY_KEY);
        let odd = CouplingLink {
            facts: BTreeMap::from([(COUPLING_KIND_KEY.to_owned(), "shared_supplier".to_owned())]),
            ..fulfils(mo(5), so(5))
        };
        let sentences = describe_coupling(&[bare.clone(), half.clone(), odd.clone(), bare.clone()]);
        assert_eq!(
            sentences,
            [bare.reason, half.reason, odd.reason],
            "once each"
        );
    }

    #[test]
    fn the_wording_is_stable_whatever_the_link_order() {
        let mut links = seeded();
        let forward = describe_coupling(&links);
        links.reverse();
        assert_eq!(describe_coupling(&links), forward);
        links.swap(0, 2);
        assert_eq!(describe_coupling(&links), forward);
    }

    #[test]
    fn a_fractional_capacity_is_kept_as_recorded() {
        assert_eq!(whole("1"), "1");
        assert_eq!(whole("3.0"), "3");
        assert_eq!(whole("1.5"), "1.5");
        assert_eq!(whole("n/a"), "n/a");
    }
}
