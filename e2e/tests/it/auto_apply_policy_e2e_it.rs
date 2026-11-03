//! Gated live e2e: the auto-apply policy over the real materialized world model.
//!
//! Materializes a purchase order (a confirmed `state`) with a priced line (`price_unit`)
//! into a live GraphDB. Then, for an expedite of that line (the verb's target),
//! `policy_context::gather` reads the line's cost and its order's state via SPARQL and
//! `PolicyEngine::evaluate` decides. Asserts:
//!   * a €320 plan under a €250 limit is human-gated with a `cost_over_limit` need;
//!   * a compliant plan (under the limit, PO confirmed) auto-promotes with no needs;
//!   * a plan touching a `draft` PO is human-gated with `supplier_unconfirmed`.
//!
//! No Odoo write-back and no model call: this checks the policy decision over real
//! materialized RDF (the `maybe_auto_promote` and slate wiring is covered by the sibling
//! e2e). Ignored by default; skips instead of failing if GraphDB is unreachable.
//!
//! ```text
//! just podman-local-up
//! cargo test -p ekg-e2e --test it auto_apply_policy_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;

use ekg_backend::policy_context;
use ekg_domain::{ActionType, EntityUri, Plan, ProposedAction, WorldModel};
use ekg_kinetic::{
    CostWithinLimit, NoPromisedDateMove, PolicyEngine, PromotionPredicate, PromotionTier,
    SupplierConfirmed,
};
use ekg_semantic::{GraphDbClient, GraphDbWorldModel, ontology};

use common::{provision_repo, rec};

const REPO: &str = "ekg_auto_apply_policy_e2e_it";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// The three capability predicates, configured for the test (limit and confirmed set).
fn predicates(limit: f64) -> Vec<Arc<dyn PromotionPredicate>> {
    vec![
        Arc::new(NoPromisedDateMove),
        Arc::new(CostWithinLimit::new(Some(limit))),
        Arc::new(SupplierConfirmed::new(
            ["purchase".to_owned(), "done".to_owned()]
                .into_iter()
                .collect(),
        )),
    ]
}

fn expedite(line: &EntityUri) -> Plan {
    Plan {
        actions: vec![ProposedAction {
            action_type: ActionType("expedite_po".to_owned()),
            target: line.clone(),
            params: BTreeMap::from([("new_date".to_owned(), "2027-11-07 00:00:00".to_owned())]),
        }],
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a reachable GraphDB (run with --ignored)"]
async fn the_widened_policy_gates_and_auto_promotes_over_real_materialized_orders() {
    let graphdb_url = env_or("GRAPHDB_URL", "http://localhost:7200");
    let http = reqwest::Client::new();
    match provision_repo(&http, &graphdb_url, REPO).await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("SKIP: GraphDB unreachable at {graphdb_url}");
            return;
        }
        Err(why) => {
            eprintln!("SKIP: GraphDB not ready: {why}");
            return;
        }
    }

    let world = Arc::new(GraphDbWorldModel::new(
        GraphDbClient::new(&graphdb_url, REPO).expect("client"),
        ontology::supply_risk_contract().expect("contract"),
    ));

    // A confirmed purchase order with a single €320 line, and a draft one with a €60 line.
    let base = common::unique_base();
    let confirmed_po = base;
    let confirmed_line = base + 1;
    let draft_po = base + 2;
    let draft_line = base + 3;
    for record in [
        rec(
            "purchase.order",
            confirmed_po,
            &[
                ("state", "purchase"),
                ("order_line", &format!("[{confirmed_line}]")),
            ],
        ),
        rec(
            "purchase.order.line",
            confirmed_line,
            &[("price_unit", "320.0")],
        ),
        rec(
            "purchase.order",
            draft_po,
            &[
                ("state", "draft"),
                ("order_line", &format!("[{draft_line}]")),
            ],
        ),
        rec("purchase.order.line", draft_line, &[("price_unit", "60.0")]),
    ] {
        world.materialize(&record).await.expect("materialize");
    }

    // The expedite targets the line; its order's state and the line's price gate it.
    let confirmed = EntityUri(ontology::entity_iri("purchase.order.line", confirmed_line));
    let draft = EntityUri(ontology::entity_iri("purchase.order.line", draft_line));

    // The gatherer reads the materialized cost and state via SPARQL.
    let ctx = policy_context::gather(world.as_ref(), &expedite(&confirmed))
        .await
        .expect("gather confirmed");
    assert_eq!(
        ctx.get(&confirmed, ekg_kinetic::ATTR_COST).map(str::trim),
        Some("320.0"),
        "the gatherer reads the expedited line's price from real RDF"
    );
    assert_eq!(
        ctx.get(&confirmed, ekg_kinetic::ATTR_PO_STATE),
        Some("purchase"),
        "the gatherer reads the line's purchase order's materialized lifecycle state"
    );

    // (1) €320 over a €250 limit: human-gated with cost_over_limit.
    let strict = PolicyEngine::with_predicates(
        "test-auto-apply/v1",
        ["expedite_po".to_owned()].into_iter().collect(),
        predicates(250.0),
    );
    let decision = strict.evaluate(&expedite(&confirmed), &ctx);
    assert_eq!(decision.tier, PromotionTier::HumanGated);
    assert!(
        decision.needs.iter().any(|n| n.code == "cost_over_limit"),
        "the €320 plan is gated with cost_over_limit: {:?}",
        decision.needs
    );

    // (2) Same plan under a €500 limit: every predicate passes, so it auto-promotes with
    // no needs.
    let lenient = PolicyEngine::with_predicates(
        "test-auto-apply/v1",
        ["expedite_po".to_owned()].into_iter().collect(),
        predicates(500.0),
    );
    let decision = lenient.evaluate(&expedite(&confirmed), &ctx);
    assert_eq!(
        decision.tier,
        PromotionTier::AutoPromote,
        "a compliant plan (under limit, PO confirmed, reversible, allowlisted) auto-promotes: {:?}",
        decision.needs
    );
    assert!(decision.needs.is_empty());

    // (3) A draft PO (€60, under the €500 limit): gated with supplier_unconfirmed only.
    let draft_ctx = policy_context::gather(world.as_ref(), &expedite(&draft))
        .await
        .expect("gather draft");
    assert_eq!(
        draft_ctx.get(&draft, ekg_kinetic::ATTR_PO_STATE),
        Some("draft")
    );
    let decision = lenient.evaluate(&expedite(&draft), &draft_ctx);
    assert_eq!(decision.tier, PromotionTier::HumanGated);
    let codes: Vec<&str> = decision.needs.iter().map(|n| n.code.as_str()).collect();
    assert_eq!(
        codes,
        vec!["supplier_unconfirmed"],
        "a draft PO under the limit is gated only on supplier confirmation"
    );

    // Drop the throwaway repo.
    let _ = http
        .delete(format!("{graphdb_url}/rest/repositories/{REPO}"))
        .send()
        .await;
}
