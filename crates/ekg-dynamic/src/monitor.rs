//! The deterministic monitor step: given a world-model change, re-evaluate the
//! injected [`RiskRule`]s and decide which subjects have drifted.
//!
//! Plain orchestration over the [`WorldModel`] seam, with no Restate, so it can be
//! unit-tested against an in-memory world. The durable wrapper in [`crate::object`]
//! calls this, then opens an action point per drifted subject and, with coupling
//! rules injected, groups the coupled points awaiting oversight ([`coupling_edges`],
//! [`plan_grouping`]).

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{
    ActionPointState, CouplingLink, EntityUri, PortfolioId, Projection, RdfChanged, Result,
    WorldModel,
};
use futures::future::try_join_all;
use serde::{Deserialize, Serialize};

use crate::lifecycle::is_pre_promotion;
use crate::portfolio::{FormOutcome, FormReceipt, GroupingPlan, PlannedPortfolio, grouping_plan};
use crate::rule::{CouplingEdge, CouplingRule, DriftVerdict, RiskRule};

/// The monitor's pass on a change: [`verdicts`] over the world model, returning the
/// union of the drifted subjects the injected `rules` flag.
///
/// The `event` triggers the pass but does not filter its result. Drift is not local to
/// the changed entity: a change to a shared dependency (an inbound shipment's ETA, a
/// component's stock) shifts the risk of the downstream subjects that depend on it, and
/// scoping to `event.entity_uri` would miss those (a shipment is never itself the
/// at-risk subject). Opening is idempotent per subject (the action-point object de-dupes
/// and the revision guard drops redelivery), so re-evaluating the whole set on each
/// change is safe, and a subject flagged by more than one rule ends up on one action
/// point.
///
/// # Errors
/// As [`verdicts`].
pub async fn evaluate(
    world: &dyn WorldModel,
    rules: &[RiskRule],
    _event: &RdfChanged,
) -> Result<Vec<DriftVerdict>> {
    verdicts(world, rules).await
}

/// The rules' verdicts over the world model: the union of what every rule flags, with no
/// change event. The read side (the operator tabs) calls it with the monitor's own rule
/// set, so a tab's at-risk markers match what the monitor would decide.
///
/// Detection is deterministic: each rule's SELECT runs through [`WorldModel::select`] (the
/// complete, unfiltered world model) and no language model is consulted.
///
/// The rule reads run concurrently, so latency is that of the slowest rule, not the sum.
/// The monitor drives this inside a single durable step, so the joined result is one
/// journal entry and replays deterministically.
///
/// # Errors
/// Propagates any [`ekg_domain::EngineError`] from a world-model query. The first rule read
/// to fail aborts the join, so the pass fails as a whole; a partial verdict set would read
/// as "not at risk" for the failed rule's subjects.
pub async fn verdicts(world: &dyn WorldModel, rules: &[RiskRule]) -> Result<Vec<DriftVerdict>> {
    let per_rule = try_join_all(rules.iter().map(|rule| async move {
        let rows = world.select(rule.sparql()).await?;
        Ok::<_, ekg_domain::EngineError>(rule.verdicts(&rows))
    }))
    .await?;
    Ok(per_rule.into_iter().flatten().collect())
}

/// The rules' verdicts over a projection (a non-authoritative what-if), fanned out like
/// [`verdicts`]: the monitor's own rules run through [`Projection::select`]. A slate
/// scorer uses it to ask which subjects the monitor would still flag under a plan.
///
/// # Errors
/// As [`verdicts`]: the first rule read to fail fails the whole call.
pub async fn projected_verdicts(
    projection: &dyn Projection,
    rules: &[RiskRule],
) -> Result<Vec<DriftVerdict>> {
    let per_rule = try_join_all(rules.iter().map(|rule| async move {
        let rows = projection.select(rule.sparql()).await?;
        Ok::<_, ekg_domain::EngineError>(rule.verdicts(&rows))
    }))
    .await?;
    Ok(per_rule.into_iter().flatten().collect())
}

/// One portfolio a pass formed or re-formed: what a joint slate for it plans over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormedPortfolio {
    /// The portfolio's id (derived from its members).
    pub id: PortfolioId,
    /// The generation it formed under (the one its joint slate is attached for).
    pub generation: u32,
    /// Its members, sorted.
    pub members: Vec<EntityUri>,
    /// Why the members are linked: each distinct coupling reason, in edge order.
    pub links: Vec<String>,
    /// Each coupling link with its facts, as the formation recorded them. Defaulted so an
    /// outcome journaled before links carried facts still loads.
    #[serde(default)]
    pub coupling: Vec<CouplingLink>,
}

/// What one monitor pass did: the subjects whose points it freshly opened, the portfolios
/// it formed or re-formed (not the unchanged ones), and the subjects it released from a
/// portfolio to stand alone. The bridge plans from it; `ekg-dynamic` never calls cognition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyOutcome {
    /// The subjects whose points were freshly opened by this pass.
    pub opened: Vec<EntityUri>,
    /// The portfolios formed or re-formed by this pass, each at its new generation.
    #[serde(default)]
    pub formed: Vec<FormedPortfolio>,
    /// The subjects this pass left on their own that had been in a portfolio.
    #[serde(default)]
    pub released: Vec<EntityUri>,
}

/// Assemble a pass's [`NotifyOutcome`] from its parts: the freshly `opened` subjects, each
/// planned portfolio with the receipt its formation returned, and each single with the
/// portfolio its assignment left (if any). A portfolio is listed only when it was formed or
/// reactivated; a single only when it left a portfolio. Pure.
#[must_use]
pub fn notify_outcome(
    opened: Vec<EntityUri>,
    formations: &[(PlannedPortfolio, FormReceipt)],
    singles: &[(EntityUri, Option<PortfolioId>)],
) -> NotifyOutcome {
    let formed = formations
        .iter()
        .filter(|(_, receipt)| receipt.outcome != FormOutcome::Unchanged)
        .map(|(planned, receipt)| {
            let mut seen = BTreeSet::new();
            FormedPortfolio {
                id: planned.id.clone(),
                generation: receipt.generation,
                members: planned.members.clone(),
                links: planned
                    .links
                    .iter()
                    .filter(|link| seen.insert(link.reason.as_str()))
                    .map(|link| link.reason.clone())
                    .collect(),
                coupling: planned.coupling(),
            }
        })
        .collect();
    let released = singles
        .iter()
        .filter(|(_, left)| left.is_some())
        .map(|(subject, _)| subject.clone())
        .collect();
    NotifyOutcome {
        opened,
        formed,
        released,
    }
}

/// The distinct subjects a pass flags, sorted (a subject flagged by more than one rule
/// appears once).
#[must_use]
pub fn flagged_subjects(verdicts: &[DriftVerdict]) -> Vec<EntityUri> {
    verdicts
        .iter()
        .map(|v| v.subject.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The coupling edges the injected `rules` yield over the world model, fanned out
/// concurrently like [`verdicts`]. `None` when any rule read fails: the failure is logged
/// and the pass skips grouping instead of failing, since detection does not depend on it
/// and the next pass repairs a missed regrouping. A partial edge set is never returned,
/// so a failed rule cannot split a portfolio.
pub async fn coupling_edges(
    world: &dyn WorldModel,
    rules: &[CouplingRule],
) -> Option<Vec<CouplingEdge>> {
    let per_rule = try_join_all(rules.iter().map(|rule| async move {
        let rows = world.select(rule.sparql()).await?;
        Ok::<_, ekg_domain::EngineError>(rule.edges(&rows))
    }))
    .await;
    match per_rule {
        Ok(edges) => Some(edges.into_iter().flatten().collect()),
        Err(err) => {
            eprintln!("coupling read failed; grouping skipped for this pass: {err}");
            None
        }
    }
}

/// The pass's grouping plan: the `flagged` subjects whose point awaits oversight
/// (`states`: each flagged subject's point state, `None` when no point exists) grouped into
/// the connected components of `edges` (see [`grouping_plan`]).
#[must_use]
pub fn plan_grouping(
    flagged: &[EntityUri],
    states: &BTreeMap<EntityUri, Option<ActionPointState>>,
    edges: &[CouplingEdge],
) -> GroupingPlan {
    let live: Vec<EntityUri> = flagged
        .iter()
        .filter(|s| {
            states
                .get(*s)
                .copied()
                .flatten()
                .is_some_and(is_pre_promotion)
        })
        .cloned()
        .collect();
    grouping_plan(&live, edges)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use ekg_domain::fakes::InMemoryWorldModel;
    use ekg_domain::{
        ChangeKind, EntityUri, NamedGraph, RdfChanged, Revision, SourceRecord, SourceRef,
    };

    use super::*;
    use crate::rule::{DriftVerdict, RiskRule};

    /// The rule the fake world exercises: [`InMemoryWorldModel::select`] returns
    /// one row per entity (its fields plus an `entity` binding), so the demo rule
    /// flags any row carrying a positive `shortfall` field.
    fn shortfall_rule() -> RiskRule {
        RiskRule::new("SELECT ?entity ?shortfall WHERE { ... }", |row| {
            let shortfall: i64 = row.get("shortfall")?.parse().ok()?;
            if shortfall > 0 {
                Some(DriftVerdict {
                    subject: EntityUri(row.get("entity")?.clone()),
                    reason: format!("shortfall {shortfall}"),
                })
            } else {
                None
            }
        })
    }

    fn record(model: &str, id: i64, shortfall: &str) -> SourceRecord {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("shortfall".to_owned(), shortfall.to_owned());
        SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: model.to_owned(),
                id,
            },
            fields,
        }
    }

    fn event_for(source_ref: &SourceRef, revision: u64) -> RdfChanged {
        RdfChanged {
            entity_uri: InMemoryWorldModel::uri_of(source_ref),
            named_graph: NamedGraph("g".to_owned()),
            change_kind: ChangeKind::Updated,
            source: source_ref.clone(),
            revision: Revision(revision),
        }
    }

    #[tokio::test]
    async fn evaluate_returns_the_full_at_risk_set_regardless_of_the_changed_entity() {
        let world = InMemoryWorldModel::default();
        let at_risk = record("sale.order", 42, "13");
        let healthy = record("sale.order", 7, "-2");
        world
            .materialize(&at_risk)
            .await
            .expect("materialize at-risk");
        world
            .materialize(&healthy)
            .await
            .expect("materialize healthy");

        // Even when the healthy entity is what changed, the at-risk order must
        // still be reported: drift is not scoped to the changed entity.
        let verdicts = evaluate(
            &world,
            &[shortfall_rule()],
            &event_for(&healthy.source_ref, 1),
        )
        .await
        .expect("evaluate ok");
        let subjects: Vec<_> = verdicts.iter().map(|v| v.subject.clone()).collect();
        assert_eq!(
            subjects,
            vec![InMemoryWorldModel::uri_of(&at_risk.source_ref)],
            "exactly the at-risk order is flagged"
        );
    }

    #[tokio::test]
    async fn a_world_with_no_drift_yields_no_verdicts() {
        let world = InMemoryWorldModel::default();
        let healthy = record("sale.order", 7, "-2");
        world
            .materialize(&healthy)
            .await
            .expect("materialize healthy");

        let verdicts = evaluate(
            &world,
            &[shortfall_rule()],
            &event_for(&healthy.source_ref, 1),
        )
        .await
        .expect("evaluate ok");
        assert!(verdicts.is_empty());
    }

    /// A second rule: flags any row whose `shortfall` is positive and even, with
    /// its own reason, so the two rules can flag the same subject independently.
    fn even_shortfall_rule() -> RiskRule {
        RiskRule::new("SELECT ?entity ?shortfall WHERE { ... }", |row| {
            let shortfall: i64 = row.get("shortfall")?.parse().ok()?;
            if shortfall > 0 && shortfall % 2 == 0 {
                Some(DriftVerdict {
                    subject: EntityUri(row.get("entity")?.clone()),
                    reason: format!("even shortfall {shortfall}"),
                })
            } else {
                None
            }
        })
    }

    #[tokio::test]
    async fn evaluate_unions_the_verdicts_of_every_injected_rule() {
        let world = InMemoryWorldModel::default();
        // shortfall 12: flagged by both rules (positive and even).
        let both = record("sale.order", 42, "12");
        // shortfall 7: flagged only by the positive-shortfall rule (odd).
        let positive_only = record("sale.order", 7, "7");
        world.materialize(&both).await.expect("materialize both");
        world
            .materialize(&positive_only)
            .await
            .expect("materialize positive-only");

        let verdicts = evaluate(
            &world,
            &[shortfall_rule(), even_shortfall_rule()],
            &event_for(&both.source_ref, 1),
        )
        .await
        .expect("evaluate ok");

        // The union carries every rule's verdicts: the even order appears once per
        // matching rule (two), the odd order once, three in total.
        let both_uri = InMemoryWorldModel::uri_of(&both.source_ref);
        let positive_uri = InMemoryWorldModel::uri_of(&positive_only.source_ref);
        assert_eq!(
            verdicts.len(),
            3,
            "both rules' verdicts are unioned: {verdicts:?}"
        );
        assert_eq!(
            verdicts.iter().filter(|v| v.subject == both_uri).count(),
            2,
            "the subject matching both rules is flagged by each"
        );
        assert!(
            verdicts.iter().any(|v| v.subject == positive_uri),
            "the subject matching only one rule is still flagged"
        );
    }

    #[tokio::test]
    async fn verdicts_are_the_current_union_without_a_change_event() {
        // The read side has no triggering event: `verdicts` returns what `evaluate`
        // returns for any event.
        let world = InMemoryWorldModel::default();
        let at_risk = record("sale.order", 42, "12");
        let healthy = record("sale.order", 7, "-1");
        world.materialize(&at_risk).await.expect("materialize");
        world.materialize(&healthy).await.expect("materialize");
        let rules = [shortfall_rule(), even_shortfall_rule()];

        let current = verdicts(&world, &rules).await.expect("verdicts");
        let evaluated = evaluate(&world, &rules, &event_for(&healthy.source_ref, 1))
            .await
            .expect("evaluate");
        assert_eq!(current, evaluated, "evaluate delegates to verdicts");
        let subject = InMemoryWorldModel::uri_of(&at_risk.source_ref);
        assert!(current.iter().all(|v| v.subject == subject), "{current:?}");
        assert_eq!(current.len(), 2, "one verdict per matching rule");
        assert!(verdicts(&world, &[]).await.expect("no rules").is_empty());
    }

    /// The SPARQL a [`FailingRuleWorld`] refuses to answer.
    const FAILING_SPARQL: &str = "SELECT ?entity WHERE { FAIL }";

    /// A world whose read of [`FAILING_SPARQL`] fails; every other call is the in-memory world.
    struct FailingRuleWorld(InMemoryWorldModel);

    #[async_trait::async_trait]
    impl WorldModel for FailingRuleWorld {
        async fn materialize(&self, record: &SourceRecord) -> Result<Revision> {
            self.0.materialize(record).await
        }
        async fn retract(&self, entity: &EntityUri) -> Result<Revision> {
            self.0.retract(entity).await
        }
        async fn ground(
            &self,
            entity: &EntityUri,
            ctx: &ekg_domain::ReadContext,
        ) -> Result<ekg_domain::Grounding> {
            self.0.ground(entity, ctx).await
        }
        async fn select(
            &self,
            sparql: &str,
        ) -> Result<Vec<std::collections::BTreeMap<String, String>>> {
            if sparql == FAILING_SPARQL {
                return Err(ekg_domain::EngineError::Backend(
                    "rule read failed".to_owned(),
                ));
            }
            self.0.select(sparql).await
        }
    }

    #[tokio::test]
    async fn one_failing_rule_read_fails_the_whole_pass() {
        // A pass that dropped the failed rule would serve a partial verdict set, and its
        // subjects would read as "not at risk". The pass must fail as a whole instead.
        let world = FailingRuleWorld(InMemoryWorldModel::default());
        let at_risk = record("sale.order", 42, "12");
        world.materialize(&at_risk).await.expect("materialize");
        let failing = RiskRule::new(FAILING_SPARQL, |_| None);
        let rules = [shortfall_rule(), failing];

        // The rule's own error, unchanged: the durable step retries only a `Backend` error, so
        // a wrapped or remapped one would turn a transient read failure terminal.
        let unchanged = |r: &Result<Vec<DriftVerdict>>| matches!(r, Err(ekg_domain::EngineError::Backend(m)) if m == "rule read failed");
        let current = verdicts(&world, &rules).await;
        assert!(
            unchanged(&current),
            "a rule read failure fails the verdicts with the rule's error: {current:?}"
        );
        let pass = evaluate(&world, &rules, &event_for(&at_risk.source_ref, 1)).await;
        assert!(
            unchanged(&pass),
            "and the monitor's pass fails with the same error: {pass:?}"
        );
        // The healthy rule alone still answers.
        assert_eq!(
            verdicts(&world, &[shortfall_rule()])
                .await
                .expect("verdicts")
                .len(),
            1
        );
    }

    /// A coupling rule over the fake world: every row with a positive `shortfall` couples
    /// its entity to a fixed peer, `…/mrp.production/1`.
    fn couple_to_peer() -> CouplingRule {
        CouplingRule::new("SELECT ?entity WHERE { COUPLE }", |row| {
            let shortfall: i64 = row.get("shortfall")?.parse().ok()?;
            (shortfall > 0).then_some(())?;
            CouplingEdge::new(
                EntityUri(row.get("entity")?.clone()),
                EntityUri("https://ekg.example/id/mrp.production/1".to_owned()),
                "fulfils",
            )
        })
    }

    fn uri(s: &str) -> EntityUri {
        EntityUri(format!("https://ekg.example/id/{s}"))
    }

    #[tokio::test]
    async fn the_pass_groups_the_live_coupled_subjects() {
        let world = InMemoryWorldModel::default();
        world
            .materialize(&record("sale.order", 42, "12"))
            .await
            .expect("materialize");
        let verdicts = verdicts(&world, &[shortfall_rule(), even_shortfall_rule()])
            .await
            .expect("verdicts");
        let mut flagged = flagged_subjects(&verdicts);
        assert_eq!(flagged, [uri("sale.order/42")], "one subject, two rules");
        flagged.push(uri("mrp.production/1"));
        let edges = coupling_edges(&world, &[couple_to_peer()])
            .await
            .expect("edges");
        let states = BTreeMap::from([
            (uri("sale.order/42"), Some(ActionPointState::Proposed)),
            (uri("mrp.production/1"), Some(ActionPointState::Open)),
        ]);
        let plan = plan_grouping(&flagged, &states, &edges);
        assert_eq!(plan.portfolios.len(), 1);
        assert_eq!(
            plan.portfolios[0].members,
            [uri("mrp.production/1"), uri("sale.order/42")]
        );

        // A point that already left oversight (promoted) is not grouped; its peer is alone.
        let resolved = BTreeMap::from([
            (uri("sale.order/42"), Some(ActionPointState::Promoted)),
            (uri("mrp.production/1"), Some(ActionPointState::Open)),
        ]);
        let plan = plan_grouping(&flagged, &resolved, &edges);
        assert!(plan.portfolios.is_empty());
        assert_eq!(plan.singles, [uri("mrp.production/1")]);
    }

    #[tokio::test]
    async fn a_coupling_read_failure_yields_no_grouping_and_the_verdicts_unchanged() {
        let world = FailingRuleWorld(InMemoryWorldModel::default());
        world
            .materialize(&record("sale.order", 42, "12"))
            .await
            .expect("materialize");
        let rules = [shortfall_rule()];
        let before = verdicts(&world, &rules).await.expect("verdicts");
        let failing = CouplingRule::new(FAILING_SPARQL, |_| None);
        assert_eq!(
            coupling_edges(&world, &[couple_to_peer(), failing]).await,
            None,
            "one failed coupling read skips grouping (no partial edge set)"
        );
        assert_eq!(
            verdicts(&world, &rules).await.expect("verdicts"),
            before,
            "detection is untouched by the coupling failure"
        );
        assert_eq!(coupling_edges(&world, &[]).await, Some(Vec::new()));
    }

    /// A projection answering every SELECT with fixed rows (the rules parse no SPARQL).
    struct CannedProjection(Vec<BTreeMap<String, String>>);
    #[async_trait::async_trait]
    impl Projection for CannedProjection {
        async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn projected_verdicts_equal_the_verdicts_over_the_same_rows() {
        let world = InMemoryWorldModel::default();
        for (id, shortfall) in [(1, "4"), (2, "-1"), (3, "6")] {
            world
                .materialize(&record("sale.order", id, shortfall))
                .await
                .expect("materialize");
        }
        let rules = [shortfall_rule(), even_shortfall_rule()];
        let rows = world.select("any").await.expect("rows");
        let projection = CannedProjection(rows);
        let mut over_world = verdicts(&world, &rules).await.expect("verdicts");
        let mut over_projection = projected_verdicts(&projection, &rules)
            .await
            .expect("projected");
        over_world.sort_by(|a, b| (&a.subject, &a.reason).cmp(&(&b.subject, &b.reason)));
        over_projection.sort_by(|a, b| (&a.subject, &a.reason).cmp(&(&b.subject, &b.reason)));
        assert!(!over_world.is_empty());
        assert_eq!(over_projection, over_world);
    }

    fn planned(members: &[&str], reasons: &[&str]) -> PlannedPortfolio {
        let members: Vec<EntityUri> = members.iter().map(|m| uri(m)).collect();
        PlannedPortfolio {
            id: crate::portfolio::portfolio_id(&members),
            links: reasons
                .iter()
                .map(|r| {
                    CouplingEdge::new(members[0].clone(), members[1].clone(), *r).expect("edge")
                })
                .collect(),
            members,
        }
    }

    #[test]
    fn the_outcome_lists_formed_and_reactivated_portfolios_not_unchanged_ones() {
        let formed = planned(&["a/1", "b/1"], &["b fulfils a", "b fulfils a"]);
        let reactivated = planned(&["c/1", "d/1"], &["c and d contend"]);
        let unchanged = planned(&["e/1", "f/1"], &["f fulfils e"]);
        let receipt = |outcome, generation| FormReceipt {
            outcome,
            generation,
        };
        let outcome = notify_outcome(
            vec![uri("a/1")],
            &[
                (formed.clone(), receipt(FormOutcome::Formed, 1)),
                (reactivated.clone(), receipt(FormOutcome::Reactivated, 3)),
                (unchanged, receipt(FormOutcome::Unchanged, 2)),
            ],
            &[],
        );
        assert_eq!(outcome.opened, [uri("a/1")]);
        assert_eq!(outcome.formed.len(), 2, "{outcome:?}");
        assert_eq!(outcome.formed[0].id, formed.id);
        assert_eq!(outcome.formed[0].generation, 1);
        assert_eq!(outcome.formed[0].members, formed.members);
        assert_eq!(outcome.formed[0].links, ["b fulfils a"], "distinct reasons");
        assert_eq!(outcome.formed[1].id, reactivated.id);
        assert_eq!(outcome.formed[1].generation, 3);
        assert_eq!(outcome.formed[1].links, ["c and d contend"]);
    }

    #[test]
    fn a_formed_portfolio_carries_its_links_with_their_facts() {
        let mut formed = planned(&["a/1", "b/1"], &["a and b contend"]);
        formed.links[0].facts = BTreeMap::from([("kind".to_owned(), "contention".to_owned())]);
        let outcome = notify_outcome(
            Vec::new(),
            &[(
                formed.clone(),
                FormReceipt {
                    outcome: FormOutcome::Formed,
                    generation: 1,
                },
            )],
            &[],
        );
        let coupling = &outcome.formed[0].coupling;
        assert_eq!(coupling.len(), 1, "{outcome:?}");
        assert_eq!(coupling[0].a, uri("a/1"));
        assert_eq!(coupling[0].b, uri("b/1"));
        assert_eq!(coupling[0].reason, "a and b contend");
        assert_eq!(
            coupling[0].facts.get("kind").map(String::as_str),
            Some("contention")
        );
    }

    #[test]
    fn the_outcome_lists_released_subjects_only_when_they_left_a_portfolio() {
        let left = Some(ekg_domain::PortfolioId("portfolio/0123456789ab".to_owned()));
        let outcome = notify_outcome(
            Vec::new(),
            &[],
            &[(uri("a/1"), None), (uri("b/1"), left), (uri("c/1"), None)],
        );
        assert_eq!(outcome.released, [uri("b/1")]);
        assert!(outcome.formed.is_empty());
    }
}
