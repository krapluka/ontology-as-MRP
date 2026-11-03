//! Gathers the [`PolicyContext`] the promotion policy's predicates consult.
//!
//! The [`PolicyEngine`] predicates are pure over a pre-gathered [`PolicyContext`]. The
//! world reads that fill it live in the backend because it is the only crate that depends
//! on both `ekg-semantic` and `ekg-kinetic`.
//!
//! The reads go through [`WorldModel::select`] directly, not the read-policy-filtered
//! grounding: the promotion policy is deterministic trusted infrastructure, so it sees the
//! real cost even though the same `ekg:unit_price` edge is hidden from the AI's grounding.

use ekg_domain::{EntityUri, Plan, Result, WorldModel};
use ekg_kinetic::{ATTR_COST, ATTR_PO_STATE, PolicyContext};
use ekg_semantic::ontology;

/// The Odoo model segment of a purchase-order id IRI (`…/id/purchase.order/<id>`).
const PURCHASE_ORDER_MODEL: &str = "purchase.order";
/// The Odoo model segment of a purchase-order line id IRI (`…/id/purchase.order.line/<id>`).
const PURCHASE_ORDER_LINE_MODEL: &str = "purchase.order.line";

/// A plan target that touches a purchase order: the order itself, or one of its lines
/// (an expedite targets the line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PurchaseTarget {
    /// A purchase-order header, by numeric id.
    Order(i64),
    /// A purchase-order line, by numeric id.
    Line(i64),
}

/// The purchase-order target `target` names, if it is a well-formed purchase-order or
/// purchase-order-line entity IRI (`…/id/<model>/<id>` with a plain integer id), else
/// `None`. Action targets are AI-generated and untrusted, so the id is validated too, not
/// just the namespace and model: a crafted IRI must not reach a SPARQL query. The gatherer
/// rebuilds a canonical IRI from the parsed id instead of interpolating the raw string.
fn purchase_target(target: &EntityUri) -> Option<PurchaseTarget> {
    let (model, id) = target
        .0
        .strip_prefix(ontology::ID_NAMESPACE)?
        .rsplit_once('/')?;
    let id = id.parse::<i64>().ok()?;
    match model {
        PURCHASE_ORDER_MODEL => Some(PurchaseTarget::Order(id)),
        PURCHASE_ORDER_LINE_MODEL => Some(PurchaseTarget::Line(id)),
        _ => None,
    }
}

/// A purchase-order line's own `ekg:unit_price`: the cost an expedite of that line touches.
/// `Ok(None)` means the line carries no price; `Err` means the read failed (fail closed).
async fn line_cost(world: &dyn WorldModel, line: &str) -> Result<Option<String>> {
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?cost WHERE {{ <{line}> ekg:unit_price ?cost }} LIMIT 1",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    let rows = world.select(&sparql).await?;
    let cost = rows.first().and_then(|r| r.get("cost")).map(|c| c.trim());
    Ok(cost.filter(|c| !c.is_empty()).map(str::to_owned))
}

/// The lifecycle state of the purchase order a line belongs to (`ekg:has_po_line` inverse).
/// `Ok(None)` means no order state is materialized for it; `Err` means the read failed.
async fn line_order_state(world: &dyn WorldModel, line: &str) -> Result<Option<String>> {
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?state WHERE {{ ?po ekg:has_po_line <{line}> ; ekg:lifecycle_state ?state }} LIMIT 1",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    let rows = world.select(&sparql).await?;
    let state = rows.first().and_then(|r| r.get("state")).map(|s| s.trim());
    Ok(state.filter(|s| !s.is_empty()).map(str::to_owned))
}

/// Sums the `ekg:unit_price` of a purchase order's lines (via `ekg:has_po_line`), as a
/// decimal string. `Ok(None)` means the order has no priced line (a real zero cost);
/// `Err` means the read itself failed (GraphDB down or timed out). The caller must fail
/// closed and not treat an unreadable cost as "no cost".
async fn purchase_order_cost(world: &dyn WorldModel, po: &str) -> Result<Option<String>> {
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT (SUM(?p) AS ?cost) WHERE {{ <{po}> ekg:has_po_line ?line . ?line ekg:unit_price ?p }}",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    let rows = world.select(&sparql).await?;
    let cost = rows.first().and_then(|r| r.get("cost")).map(|c| c.trim());
    Ok(cost.filter(|c| !c.is_empty()).map(str::to_owned))
}

/// The purchase order's materialized lifecycle state (`ekg:lifecycle_state`). `Ok(None)`
/// means no state is materialized; `Err` means the read failed and the caller fails closed.
async fn purchase_order_state(world: &dyn WorldModel, po: &str) -> Result<Option<String>> {
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?state WHERE {{ <{po}> ekg:lifecycle_state ?state }} LIMIT 1",
        vocab = ontology::VOCAB_NAMESPACE,
    );
    let rows = world.select(&sparql).await?;
    let state = rows.first().and_then(|r| r.get("state")).map(|s| s.trim());
    Ok(state.filter(|s| !s.is_empty()).map(str::to_owned))
}

/// Gathers the cost and purchase-order-state attributes the promotion predicates consult,
/// for every target in `plan` that touches a purchase order: a purchase order (its lines'
/// summed price, its state), or a purchase-order line (the line's own price, which is the
/// cost the change touches, and the state of the order it belongs to).
///
/// A reachable target with no materialized order state records an empty state, so the
/// supplier-confirmed predicate gates it instead of passing vacuously. A target touching no
/// purchase order contributes nothing: it carries no order-line cost and touches no
/// supplier order, so its cost and supplier predicates pass.
///
/// # Errors
/// Returns the world-model read error if a cost or state cannot be read. A partial context
/// could let a cost or supplier predicate pass vacuously and auto-promote a write-back, so
/// the caller treats a gather error as human-gated.
pub async fn gather(world: &dyn WorldModel, plan: &Plan) -> Result<PolicyContext> {
    let mut ctx = PolicyContext::new();
    for action in &plan.actions {
        // The queries take a canonical IRI rebuilt from the validated id, not the raw target.
        let (cost, state) = match purchase_target(&action.target) {
            None => continue,
            Some(PurchaseTarget::Order(id)) => {
                let po = ontology::entity_iri(PURCHASE_ORDER_MODEL, id);
                (
                    purchase_order_cost(world, &po).await?,
                    purchase_order_state(world, &po).await?,
                )
            }
            Some(PurchaseTarget::Line(id)) => {
                let line = ontology::entity_iri(PURCHASE_ORDER_LINE_MODEL, id);
                (
                    line_cost(world, &line).await?,
                    line_order_state(world, &line).await?,
                )
            }
        };
        if let Some(cost) = cost {
            ctx.set(action.target.clone(), ATTR_COST, cost);
        }
        // No materialized order state is recorded as an empty string, which gates the plan.
        ctx.set(
            action.target.clone(),
            ATTR_PO_STATE,
            state.unwrap_or_default(),
        );
    }
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ekg_domain::{
        ActionType, EntityUri, Grounding, ProposedAction, ReadContext, Result, Revision,
        SourceRecord, WorldModel,
    };
    use ekg_semantic::ontology;

    use super::*;

    /// Canned rows, looked up by a substring of the query.
    type CannedRows = Vec<(String, Vec<BTreeMap<String, String>>)>;

    /// A `WorldModel` whose `select` replays canned rows per query substring, enough to
    /// exercise the gatherer's SPARQL and row handling without a live GraphDB. The gatherer
    /// only uses `select`; the other methods are unreachable in these tests.
    struct CannedWorld {
        rows: Mutex<CannedRows>,
    }

    impl CannedWorld {
        fn new(rows: Vec<(&str, Vec<BTreeMap<String, String>>)>) -> Self {
            Self {
                rows: Mutex::new(rows.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            }
        }
    }

    #[async_trait]
    impl WorldModel for CannedWorld {
        async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
            unreachable!("gatherer never materializes")
        }
        async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
            unreachable!("gatherer never retracts")
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
            unreachable!("gatherer reads via select, never ground")
        }
        async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            let rows = self.rows.lock().expect("lock");
            for (needle, out) in rows.iter() {
                if sparql.contains(needle.as_str()) {
                    return Ok(out.clone());
                }
            }
            Ok(Vec::new())
        }
    }

    fn row(key: &str, value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(key.to_owned(), value.to_owned())])
    }

    fn expedite(po: &EntityUri) -> ProposedAction {
        ProposedAction {
            action_type: ActionType("expedite_po".to_owned()),
            target: po.clone(),
            params: BTreeMap::new(),
        }
    }

    /// A `WorldModel` whose `select` always fails, standing in for an unreachable GraphDB.
    struct ErroringWorld;

    #[async_trait]
    impl WorldModel for ErroringWorld {
        async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
            unreachable!()
        }
        async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
            unreachable!()
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
            unreachable!()
        }
        async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            Err(ekg_domain::EngineError::Backend(
                "graphdb unreachable".to_owned(),
            ))
        }
    }

    #[tokio::test]
    async fn gathers_cost_and_state_for_a_purchase_order_target() {
        let po = EntityUri(ontology::entity_iri("purchase.order", 42));
        let world = CannedWorld::new(vec![
            ("SUM(?p)", vec![row("cost", "320.0")]),
            ("ekg:lifecycle_state", vec![row("state", "purchase")]),
        ]);
        let ctx = gather(
            &world,
            &Plan {
                actions: vec![expedite(&po)],
            },
        )
        .await
        .expect("gather ok");
        assert_eq!(ctx.get(&po, ATTR_COST), Some("320.0"));
        assert_eq!(ctx.get(&po, ATTR_PO_STATE), Some("purchase"));
    }

    #[tokio::test]
    async fn ignores_a_non_purchase_order_target() {
        let wo = EntityUri(ontology::entity_iri("mrp.workorder", 7));
        let action = ProposedAction {
            action_type: ActionType("reschedule_work_order".to_owned()),
            target: wo.clone(),
            params: BTreeMap::new(),
        };
        // A non-PO target is never queried, so it gets no attributes.
        let world = CannedWorld::new(vec![("SUM(?p)", vec![row("cost", "999.0")])]);
        let ctx = gather(
            &world,
            &Plan {
                actions: vec![action],
            },
        )
        .await
        .expect("gather ok");
        assert_eq!(ctx.get(&wo, ATTR_COST), None);
        assert_eq!(ctx.get(&wo, ATTR_PO_STATE), None);
    }

    #[tokio::test]
    async fn a_reachable_po_with_no_priced_line_has_no_cost_but_an_empty_gating_state() {
        let po = EntityUri(ontology::entity_iri("purchase.order", 5));
        // SUM over no line yields an empty binding; no state row either.
        let world = CannedWorld::new(vec![("SUM(?p)", vec![row("cost", "")])]);
        let ctx = gather(
            &world,
            &Plan {
                actions: vec![expedite(&po)],
            },
        )
        .await
        .expect("gather ok");
        assert_eq!(ctx.get(&po, ATTR_COST), None, "no priced line, so no cost");
        // A reachable PO with no materialized state records an empty state, so the
        // supplier-confirmed predicate gates it.
        assert_eq!(ctx.get(&po, ATTR_PO_STATE), Some(""));
    }

    #[tokio::test]
    async fn fails_closed_when_a_purchase_order_read_errors() {
        // A read error for a PO target must propagate, so the caller human-gates instead
        // of auto-promoting on a partial context.
        let po = EntityUri(ontology::entity_iri("purchase.order", 9));
        let result = gather(
            &ErroringWorld,
            &Plan {
                actions: vec![expedite(&po)],
            },
        )
        .await;
        assert!(result.is_err(), "an unreadable PO cost/state fails closed");
    }

    #[tokio::test]
    async fn a_crafted_non_numeric_target_id_is_never_queried() {
        // An AI-generated target with an injection payload in the id must not reach SPARQL:
        // the id fails the numeric parse, so the target is ignored (no attributes gathered).
        let crafted = EntityUri(format!(
            "{}purchase.order/1> }} UNION {{ ?s ?p ?o",
            ontology::ID_NAMESPACE
        ));
        // ErroringWorld fails if it is queried at all.
        let ctx = gather(
            &ErroringWorld,
            &Plan {
                actions: vec![expedite(&crafted)],
            },
        )
        .await
        .expect("a crafted target is skipped, never queried");
        assert_eq!(ctx.get(&crafted, ATTR_COST), None);
        assert_eq!(ctx.get(&crafted, ATTR_PO_STATE), None);
    }

    fn line_target(id: i64) -> EntityUri {
        EntityUri(ontology::entity_iri("purchase.order.line", id))
    }

    #[tokio::test]
    async fn a_line_target_reads_its_own_price_and_its_orders_state() {
        let line = line_target(4);
        let world = CannedWorld::new(vec![
            ("ekg:unit_price ?cost", vec![row("cost", "320.0")]),
            (
                "?po ekg:has_po_line <https://ekg.example/id/purchase.order.line/4>",
                vec![row("state", "draft")],
            ),
        ]);
        let ctx = gather(
            &world,
            &Plan {
                actions: vec![expedite(&line)],
            },
        )
        .await
        .expect("gather ok");
        assert_eq!(ctx.get(&line, ATTR_COST), Some("320.0"));
        assert_eq!(ctx.get(&line, ATTR_PO_STATE), Some("draft"));
    }

    #[tokio::test]
    async fn a_line_expedite_is_gated_by_its_order_and_its_price() {
        use ekg_kinetic::{CostWithinLimit, PolicyEngine, SupplierConfirmed};
        let line = line_target(4);
        let engine = PolicyEngine::with_predicates(
            "p/v1",
            ["expedite_po".to_owned()].into(),
            vec![
                std::sync::Arc::new(CostWithinLimit::new(Some(250.0))),
                std::sync::Arc::new(SupplierConfirmed::new(["purchase".to_owned()].into())),
            ],
        );
        let policy = |ctx: &PolicyContext| {
            engine.evaluate(
                &Plan {
                    actions: vec![expedite(&line)],
                },
                ctx,
            )
        };
        // An unconfirmed order over the limit: one reason each.
        let world = CannedWorld::new(vec![
            ("ekg:unit_price ?cost", vec![row("cost", "320.0")]),
            ("ekg:has_po_line <", vec![row("state", "draft")]),
        ]);
        let ctx = gather(
            &world,
            &Plan {
                actions: vec![expedite(&line)],
            },
        )
        .await
        .expect("gather ok");
        let decision = policy(&ctx);
        let codes: Vec<&str> = decision.needs.iter().map(|n| n.code.as_str()).collect();
        assert!(codes.contains(&"supplier_unconfirmed"), "{codes:?}");
        assert!(codes.contains(&"cost_over_limit"), "{codes:?}");
        // A line with no order state is gated too, through its empty state.
        let world = CannedWorld::new(vec![("ekg:unit_price ?cost", vec![row("cost", "10.0")])]);
        let ctx = gather(
            &world,
            &Plan {
                actions: vec![expedite(&line)],
            },
        )
        .await
        .expect("gather ok");
        assert_eq!(ctx.get(&line, ATTR_PO_STATE), Some(""));
        let codes: Vec<String> = policy(&ctx).needs.into_iter().map(|n| n.code).collect();
        assert_eq!(codes, ["supplier_unconfirmed"]);
    }

    #[tokio::test]
    async fn a_line_target_fails_closed_when_its_reads_error() {
        let result = gather(
            &ErroringWorld,
            &Plan {
                actions: vec![expedite(&line_target(4))],
            },
        )
        .await;
        assert!(result.is_err());
    }
}
