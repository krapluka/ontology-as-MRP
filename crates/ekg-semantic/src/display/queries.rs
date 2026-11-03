//! The display queries, embedded from `ontology/` beside the rule they read with. Every entity
//! IRI a query interpolates is first rebuilt by [`ontology::canonical_subject`]; a non-canonical
//! one is refused before anything is substituted, so a crafted IRI never reaches the store.

use ekg_domain::{EngineError, EntityUri, Result};

use crate::ontology;
use crate::tabs::queries::splice;

/// Open inbound per short component, ordered by arrival.
pub const COVERAGE_RQ: &str = include_str!("../../ontology/coverage.rq");
/// The manufacturing lots fulfilling each sales order, with their component needs.
pub const ORDER_LOTS_RQ: &str = include_str!("../../ontology/order-lots.rq");
/// Every sales order's promise.
pub const PROMISES_RQ: &str = include_str!("../../ontology/promises.rq");
/// The earliest a purchase-order line can arrive.
pub const LINE_EARLIEST_ARRIVAL_RQ: &str = include_str!("../../ontology/line-earliest-arrival.rq");
/// Every build's work orders.
pub const BUILD_WORK_ORDERS_RQ: &str = include_str!("../../ontology/build-work-orders.rq");
/// A manufacturing order's latest work-order end and the orders it fulfills.
pub const PROCESS_COMPLETION_RQ: &str = include_str!("../../ontology/process-completion.rq");
/// A sales order's value and currency.
pub const ORDER_VALUE_RQ: &str = include_str!("../../ontology/order-value.rq");
/// The names each asked-about entity is labelled from.
pub const ENTITY_LABELS_RQ: &str = include_str!("../../ontology/entity-labels.rq");
/// A purchase-order line's current arrival.
pub const LINE_ARRIVAL_RQ: &str = include_str!("../../ontology/line-arrival.rq");
/// The currency of the purchase order a cost target touches.
pub const PURCHASE_CURRENCY_RQ: &str = include_str!("../../ontology/purchase-currency.rq");

/// The cost a plan's purchase target touches.
pub const PURCHASE_COST_RQ: &str = include_str!("../../ontology/purchase-cost.rq");

/// The facts a plan option's scope is counted from.
pub const PLAN_SCOPE_RQ: &str = include_str!("../../ontology/plan-scope.rq");

/// Placeholder IRI the subject replaces.
const SUBJECT: &str = "<urn:ekg:subject>";
/// Marker the component IRIs replace in `coverage.rq`'s `VALUES` block.
const COMPONENTS: &str = "{{COMPONENTS}}";
/// Marker the order filter replaces in `order-lots.rq`.
const ORDERS: &str = "{{ORDERS}}";
/// Marker the change-target IRIs replace in `plan-scope.rq`'s `VALUES` block.
const TARGETS: &str = "{{TARGETS}}";
/// Marker the entity IRIs replace in `entity-labels.rq`'s `VALUES` block.
const SUBJECTS: &str = "{{SUBJECTS}}";

/// Rebuild `iri` canonically, or refuse it.
fn canonical(iri: &str) -> Result<String> {
    ontology::canonical_subject(iri)
        .map(|c| c.0)
        .ok_or_else(|| {
            EngineError::Invariant(format!(
                "refusing to interpolate a non-canonical entity IRI into a display query: {iri:?}"
            ))
        })
}

/// `template` spliced, with its subject filled by the canonical `subject`.
fn for_subject(template: &str, subject: &EntityUri) -> Result<String> {
    let subject = canonical(&subject.0)?;
    Ok(splice(template).replace(SUBJECT, &format!("<{subject}>")))
}

/// The coverage query for the short components. `Ok(None)` for an empty set, so an empty
/// `VALUES` block is never issued.
///
/// # Errors
/// [`EngineError::Invariant`] if any component is not a canonical entity IRI.
pub fn coverage_query<'a>(components: impl IntoIterator<Item = &'a str>) -> Result<Option<String>> {
    let components = components
        .into_iter()
        .map(|iri| canonical(iri).map(|c| format!("<{c}>")))
        .collect::<Result<Vec<_>>>()?;
    if components.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        splice(COVERAGE_RQ).replace(COMPONENTS, &components.join(" ")),
    ))
}

/// The scope-facts query over a plan option's change targets. `Ok(None)` for an empty set,
/// so an empty `VALUES` block is never issued. Duplicate targets are sent once.
///
/// # Errors
/// [`EngineError::Invariant`] if any target is not a canonical entity IRI.
pub fn plan_scope_query<'a>(targets: impl IntoIterator<Item = &'a str>) -> Result<Option<String>> {
    let mut targets = targets
        .into_iter()
        .map(|iri| canonical(iri).map(|c| format!("<{c}>")))
        .collect::<Result<Vec<_>>>()?;
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        splice(PLAN_SCOPE_RQ).replace(TARGETS, &targets.join(" ")),
    ))
}

/// The order-lots query for one sales order, or for every order when `order` is `None`.
///
/// # Errors
/// [`EngineError::Invariant`] if `order` is not a canonical entity IRI.
pub fn order_lots_query(order: Option<&EntityUri>) -> Result<String> {
    let filter = match order {
        Some(order) => format!("VALUES ?so {{ <{}> }}", canonical(&order.0)?),
        None => String::new(),
    };
    Ok(splice(ORDER_LOTS_RQ).replace(ORDERS, &filter))
}

/// Every sales order's promise.
#[must_use]
pub fn promises_query() -> String {
    splice(PROMISES_RQ)
}

/// Every build's work orders, with their planned windows.
#[must_use]
pub fn build_work_orders_query() -> String {
    splice(BUILD_WORK_ORDERS_RQ)
}

/// The earliest a purchase-order line can arrive, for each line where that is known.
#[must_use]
pub fn line_earliest_arrival_query() -> String {
    splice(LINE_EARLIEST_ARRIVAL_RQ)
}

/// The process-completion query for a manufacturing order.
///
/// # Errors
/// [`EngineError::Invariant`] if `order` is not a canonical entity IRI.
pub fn process_completion_query(order: &EntityUri) -> Result<String> {
    for_subject(PROCESS_COMPLETION_RQ, order)
}

/// The value-and-currency query for a sales order.
///
/// # Errors
/// [`EngineError::Invariant`] if `order` is not a canonical entity IRI.
pub fn order_value_query(order: &EntityUri) -> Result<String> {
    for_subject(ORDER_VALUE_RQ, order)
}

/// The label query over entity ids. `Ok(None)` for an empty set, so an empty `VALUES` block
/// is never issued. Duplicate ids are sent once.
///
/// # Errors
/// [`EngineError::Invariant`] if any id is not a canonical entity IRI.
pub fn entity_labels_query<'a>(
    subjects: impl IntoIterator<Item = &'a str>,
) -> Result<Option<String>> {
    let mut subjects = subjects
        .into_iter()
        .map(|iri| canonical(iri).map(|c| format!("<{c}>")))
        .collect::<Result<Vec<_>>>()?;
    subjects.sort();
    subjects.dedup();
    if subjects.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        splice(ENTITY_LABELS_RQ).replace(SUBJECTS, &subjects.join(" ")),
    ))
}

/// The arrival query for a purchase-order line.
///
/// # Errors
/// [`EngineError::Invariant`] if `line` is not a canonical entity IRI.
pub fn line_arrival_query(line: &EntityUri) -> Result<String> {
    for_subject(LINE_ARRIVAL_RQ, line)
}

/// The purchase-cost query for a cost target (a purchase order or one of its lines).
///
/// # Errors
/// [`EngineError::Invariant`] if `target` is not a canonical entity IRI.
pub fn purchase_cost_query(target: &EntityUri) -> Result<String> {
    for_subject(PURCHASE_COST_RQ, target)
}

/// The purchase-currency query for a cost target (a purchase order or one of its lines).
///
/// # Errors
/// [`EngineError::Invariant`] if `target` is not a canonical entity IRI.
pub fn purchase_currency_query(target: &EntityUri) -> Result<String> {
    for_subject(PURCHASE_CURRENCY_RQ, target)
}

/// Every display query as a read builds it, with its file name. The tab-query
/// well-formedness test (`crate::tabs::queries`) runs over this list.
#[cfg(test)]
pub(crate) fn composed_for_test() -> Vec<(&'static str, String)> {
    #![allow(clippy::expect_used)]
    let order = EntityUri(ontology::entity_iri("sale.order", 1));
    let mo = EntityUri(ontology::entity_iri("mrp.production", 2));
    let line = EntityUri(ontology::entity_iri("purchase.order.line", 3));
    let component = ontology::entity_iri("product.product", 4);
    vec![
        (
            "coverage.rq",
            coverage_query([component.as_str()])
                .expect("canonical components")
                .expect("a non-empty component set"),
        ),
        ("promises.rq", promises_query()),
        ("line-earliest-arrival.rq", line_earliest_arrival_query()),
        (
            "order-lots.rq",
            order_lots_query(Some(&order)).expect("canonical"),
        ),
        (
            "order-lots.rq (every order)",
            order_lots_query(None).expect("no filter"),
        ),
        (
            "process-completion.rq",
            process_completion_query(&mo).expect("canonical"),
        ),
        (
            "order-value.rq",
            order_value_query(&order).expect("canonical"),
        ),
        (
            "entity-labels.rq",
            entity_labels_query([line.0.as_str(), mo.0.as_str()])
                .expect("canonical subjects")
                .expect("a non-empty subject set"),
        ),
        (
            "line-arrival.rq",
            line_arrival_query(&line).expect("canonical"),
        ),
        (
            "purchase-cost.rq",
            purchase_cost_query(&line).expect("canonical"),
        ),
        (
            "purchase-currency.rq",
            purchase_currency_query(&line).expect("canonical"),
        ),
        (
            "plan-scope.rq",
            plan_scope_query([line.0.as_str(), mo.0.as_str()])
                .expect("canonical targets")
                .expect("a non-empty target set"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn a_non_canonical_iri_is_refused_before_substitution() {
        let hostile = EntityUri("https://ekg.example/id/sale.order/1> } DROP ALL #".to_owned());
        assert!(order_value_query(&hostile).is_err());
        assert!(entity_labels_query([hostile.0.as_str()]).is_err());
        assert!(line_arrival_query(&hostile).is_err());
        assert!(process_completion_query(&hostile).is_err());
        assert!(purchase_currency_query(&hostile).is_err());
        assert!(order_lots_query(Some(&hostile)).is_err());
        assert!(coverage_query([hostile.0.as_str()]).is_err());
        assert!(plan_scope_query([hostile.0.as_str()]).is_err());
        // A provenance node is not an entity either.
        let source = EntityUri(format!("{}#source", ontology::entity_iri("sale.order", 1)));
        assert!(order_value_query(&source).is_err());
    }

    #[test]
    fn an_empty_component_set_issues_no_coverage_query() {
        assert_eq!(coverage_query([]).expect("no components"), None);
        assert_eq!(plan_scope_query([]).expect("no targets"), None);
        assert_eq!(entity_labels_query([]).expect("no subjects"), None);
    }

    #[test]
    fn the_coverage_query_uses_the_one_open_inbound_definition() {
        let q = coverage_query([ontology::entity_iri("product.product", 4).as_str()])
            .expect("canonical")
            .expect("non-empty");
        assert!(q.contains(crate::tabs::queries::OPEN_INBOUND_FRAGMENT));
        assert!(q.contains("VALUES ?comp { <https://ekg.example/id/product.product/4> }"));
        let arrival =
            line_arrival_query(&EntityUri(ontology::entity_iri("purchase.order.line", 3)))
                .expect("canonical");
        assert!(arrival.contains(crate::tabs::queries::OPEN_INBOUND_FRAGMENT));
    }
}
