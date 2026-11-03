//! The inbox's headline metrics: late orders now and if approved, and revenue at risk with its
//! currency, each true or absent.
//!
//! Over the points awaiting oversight: "late now" is each point's do-nothing outcome; "late if
//! approved" is its recommended option's projected outcome (or the do-nothing outcome when it
//! has none). Each counts the orders not on time, so a manufacturing order and the sales order
//! it fulfills count once. Revenue at risk is the sum of those orders' values, served only when
//! every value and currency was read and there is one currency.

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{EntityUri, Plan, Result, Simulator, WorldModel};
use futures::future::join_all;

use super::delivery::{self, Delivery, Outcome, WorldSource};
use super::money::Money;

/// One point awaiting oversight: its subject and its recommended option's plan, if any.
#[derive(Debug, Clone)]
pub struct Awaiting {
    /// The point's subject (a sales or manufacturing order).
    pub subject: EntityUri,
    /// The recommended option's plan, or `None` when the slate recommends nothing.
    pub recommended: Option<Plan>,
}

/// The recommended option's outcome for `subject` under `plan`, projected (never persisted).
async fn approved(
    world: &WorldSource<'_>,
    simulator: &dyn Simulator,
    subject: &EntityUri,
    plan: &Plan,
) -> Result<Option<Outcome>> {
    let projection = simulator.project(plan).await?;
    delivery::outcome(world, projection.as_ref(), subject).await
}

/// One awaiting point, judged: the order it stands for (`Err` when that read failed), and its
/// do-nothing and if-approved outcomes (`None` when either cannot be judged).
struct Judged {
    order: Result<Option<EntityUri>>,
    outcomes: Option<(Outcome, Outcome)>,
}

/// Judge one awaiting point against the current world and its recommended option's what-if.
async fn judge(current: &WorldSource<'_>, simulator: &dyn Simulator, point: &Awaiting) -> Judged {
    // The order a point stands for: a manufacturing order counts as the sale it fulfills.
    let order = delivery::order_of(current, &point.subject).await;
    let now = delivery::outcome(current, current, &point.subject)
        .await
        .ok()
        .flatten();
    let after = match &point.recommended {
        None => now.clone(),
        Some(plan) => approved(current, simulator, &point.subject, plan)
            .await
            .ok()
            .flatten(),
    };
    Judged {
        order,
        outcomes: now.zip(after),
    }
}

/// The headline metrics (`late_orders_now`, `late_orders`, `revenue_at_risk`, `currency`) over
/// the points awaiting oversight, each point judged concurrently. The late counts are omitted
/// together if any point's outcome cannot be judged; revenue (with its currency) is omitted if
/// any order's value or currency is unread, or the orders are in more than one currency. This
/// commits nothing: each recommended option is projected onto a throwaway what-if.
pub async fn headline_metrics(
    world: &dyn WorldModel,
    simulator: &dyn Simulator,
    awaiting: &[Awaiting],
) -> BTreeMap<String, String> {
    let current = WorldSource(world);
    let judged = join_all(
        awaiting
            .iter()
            .map(|point| judge(&current, simulator, point)),
    )
    .await;

    let (mut late_now, mut late_after) = (BTreeSet::new(), BTreeSet::new());
    let (mut all_judged, mut orders_read) = (true, true);
    let mut orders = BTreeSet::new();
    for (point, judged) in awaiting.iter().zip(judged) {
        let key = match judged.order {
            Ok(Some(order)) => {
                orders.insert(order.0.clone());
                order.0
            }
            Ok(None) => point.subject.0.clone(),
            Err(_) => {
                orders_read = false;
                point.subject.0.clone()
            }
        };
        match judged.outcomes {
            Some((now, after)) => {
                if now.delivery != Delivery::OnTime {
                    late_now.insert(key.clone());
                }
                if after.delivery != Delivery::OnTime {
                    late_after.insert(key);
                }
            }
            None => all_judged = false,
        }
    }

    let mut metrics = BTreeMap::new();
    if all_judged {
        metrics.insert(
            super::LATE_ORDERS_NOW_KEY.to_owned(),
            late_now.len().to_string(),
        );
        metrics.insert(
            super::LATE_ORDERS_KEY.to_owned(),
            late_after.len().to_string(),
        );
    }
    if orders_read && let Some((revenue, currency)) = revenue(&current, &orders).await {
        metrics.insert(super::REVENUE_AT_RISK_KEY.to_owned(), revenue.render());
        if let Some(currency) = currency {
            metrics.insert(super::CURRENCY_KEY.to_owned(), currency);
        }
    }
    metrics
}

/// The sum of `orders`' values in their one currency; `None` if a value or currency is
/// unread or the currencies differ. No orders gives zero with no currency.
async fn revenue(
    current: &WorldSource<'_>,
    orders: &BTreeSet<String>,
) -> Option<(Money, Option<String>)> {
    let mut total = Money::default();
    let mut currencies = BTreeSet::new();
    for order in orders {
        let (value, currency) = delivery::order_value(current, &EntityUri(order.clone()))
            .await
            .ok()?;
        total = total.checked_add(value?)?;
        currencies.insert(currency?);
    }
    if currencies.len() > 1 {
        return None;
    }
    Some((total, currencies.pop_first()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use ekg_domain::{
        ActionType, EngineError, Grounding, Projection, ProposedAction, ReadContext, Revision,
        SourceRecord,
    };

    use super::super::delivery::tests::{Canned, comp, rule_row, so};
    use super::*;

    type Row = BTreeMap<String, String>;

    /// A world over a canned source, plus the per-plan projections the simulator hands out
    /// (keyed by the plan's first target).
    struct CannedWorld(Canned);

    #[async_trait]
    impl WorldModel for CannedWorld {
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
            self.0.select(sparql).await
        }
    }

    struct CannedSimulator(BTreeMap<String, Canned>);

    #[async_trait]
    impl Simulator for CannedSimulator {
        async fn project(&self, plan: &Plan) -> Result<Box<dyn Projection>> {
            let key = &plan.actions[0].target.0;
            let projected = self.0.get(key).cloned().expect("a canned projection");
            Ok(Box::new(projected))
        }
    }

    fn plan(target: &str) -> Plan {
        Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("any".to_owned()),
                target: EntityUri(target.to_owned()),
                params: BTreeMap::new(),
            }],
        }
    }

    const PROMISE_1: &str = "2027-10-22T00:00:00";
    const PROMISE_2: &str = "2027-10-26T00:00:00";

    /// Two orders, both late if nothing is done (inbound after each promise).
    fn world() -> Canned {
        Canned {
            risk: vec![
                rule_row(&so(1), &comp(4), 10.0, 2.0, 5.0, 0.0),
                rule_row(&so(2), &comp(6), 4.0, 0.0, 0.0, 0.0),
            ],
            promises: vec![(so(1), PROMISE_1.to_owned()), (so(2), PROMISE_2.to_owned())],
            inbound: vec![
                (comp(4), 10.0, "2027-11-15T08:00:00".to_owned()),
                (comp(6), 4.0, "2027-11-07T08:00:00".to_owned()),
            ],
            values: BTreeMap::from([
                (so(1), ("20700.10".to_owned(), "USD".to_owned())),
                (so(2), ("27600.20".to_owned(), "USD".to_owned())),
            ]),
            ..Canned::default()
        }
    }

    fn simulator() -> CannedSimulator {
        let base = world();
        // Order 1: an expedite brings its demand in by the promise (only order 2's row stays).
        let expedited = Canned {
            risk: vec![base.risk[1].clone()],
            ..base.clone()
        };
        // Order 2: a re-promise to 10 Nov clears its row by moving the promise later.
        let re_promised = Canned {
            risk: vec![base.risk[0].clone()],
            promises: vec![
                (so(1), PROMISE_1.to_owned()),
                (so(2), "2027-11-16T00:00:00".to_owned()),
            ],
            ..base
        };
        CannedSimulator(BTreeMap::from([(so(1), expedited), (so(2), re_promised)]))
    }

    fn awaiting() -> Vec<Awaiting> {
        vec![
            Awaiting {
                subject: EntityUri(so(1)),
                recommended: Some(plan(&so(1))),
            },
            Awaiting {
                subject: EntityUri(so(2)),
                recommended: Some(plan(&so(2))),
            },
        ]
    }

    fn get<'a>(metrics: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
        metrics.get(key).map(String::as_str)
    }

    #[tokio::test]
    async fn a_re_promise_counts_as_late_if_approved_and_revenue_is_exact() {
        let world = CannedWorld(world());
        let metrics = headline_metrics(&world, &simulator(), &awaiting()).await;
        assert_eq!(get(&metrics, "late_orders_now"), Some("2"));
        assert_eq!(
            get(&metrics, "late_orders"),
            Some("1"),
            "the re-promised order is late"
        );
        assert_eq!(get(&metrics, "revenue_at_risk"), Some("48300.30"));
        assert_eq!(get(&metrics, "currency"), Some("USD"));
    }

    #[tokio::test]
    async fn revenue_is_omitted_on_a_missing_value_or_mixed_currencies() {
        let mut missing = world();
        missing.values.remove(&so(2));
        let metrics = headline_metrics(&CannedWorld(missing), &simulator(), &awaiting()).await;
        assert_eq!(
            get(&metrics, "revenue_at_risk"),
            None,
            "never a partial sum"
        );
        assert_eq!(get(&metrics, "currency"), None);
        assert_eq!(
            get(&metrics, "late_orders_now"),
            Some("2"),
            "the counts stay served"
        );

        let mut mixed = world();
        mixed
            .values
            .insert(so(2), ("27600.20".to_owned(), "EUR".to_owned()));
        let metrics = headline_metrics(&CannedWorld(mixed), &simulator(), &awaiting()).await;
        assert_eq!(get(&metrics, "revenue_at_risk"), None);
    }

    #[tokio::test]
    async fn a_point_without_a_recommendation_counts_its_do_nothing_outcome() {
        let points = vec![Awaiting {
            subject: EntityUri(so(1)),
            recommended: None,
        }];
        let world = CannedWorld(world());
        let metrics = headline_metrics(&world, &simulator(), &points).await;
        assert_eq!(get(&metrics, "late_orders_now"), Some("1"));
        assert_eq!(get(&metrics, "late_orders"), Some("1"));
    }

    #[tokio::test]
    async fn an_mo_counts_its_fulfilled_order_once() {
        let mo = crate::ontology::entity_iri("mrp.production", 7);
        let mut base = world();
        base.process = vec![BTreeMap::from([
            ("maxEnd".to_owned(), "2027-10-21T16:00:00".to_owned()),
            ("so".to_owned(), so(1)),
        ])];
        let points = vec![
            Awaiting {
                subject: EntityUri(so(1)),
                recommended: None,
            },
            Awaiting {
                subject: EntityUri(mo),
                recommended: None,
            },
        ];
        let metrics = headline_metrics(&CannedWorld(base), &simulator(), &points).await;
        assert_eq!(
            get(&metrics, "revenue_at_risk"),
            Some("20700.10"),
            "S1 once"
        );
        assert_eq!(get(&metrics, "late_orders_now"), Some("1"), "one order");
    }
}
