//! Hermetic tests that the capability-neutral seams compose into the governed loop,
//! run against the in-memory fakes. They cover the whole loop (materialize, detect,
//! ground, explain, propose, promote, reverse) plus the read-policy and stale-event
//! invariants.
//!
//! Risk detection and prompt-building happen in this test (standing in for the
//! monitor and the bounded agent), not in the seams.

use std::collections::BTreeMap;

use crate::Layer;
use crate::action_point::{PromotedAction, Promotion};
use crate::error::EngineError;
use crate::events::{ChangeKind, RdfChanged};
use crate::fakes::{
    CapturingPublisher, DeterministicCognition, FixtureIngestor, InMemoryWorldModel,
    ProjectionConsumer, RecordingActionExecutor,
};
use crate::grounding::{Grounding, ReadContext};
use crate::ids::{ActionPointId, CorrelationId, ReviewerId};
use crate::proposal::{ActionType, Proposal, ProposedAction};
use crate::seams::{
    ActionExecutor, CognitionProvider, RdfChangePublisher, SourceIngestor, WorldModel,
};
use crate::source::{SourceRecord, SourceRef};

#[test]
fn all_layers_present() {
    assert_eq!(Layer::all().len(), 3);
}

/// The at-risk sales-order fixture the loop runs over.
fn at_risk_order() -> SourceRecord {
    let fields = BTreeMap::from([
        ("commitment_date".to_owned(), "2027-10-18".to_owned()),
        ("quantity".to_owned(), "10".to_owned()),
        ("at_risk".to_owned(), "true".to_owned()),
        ("unit_price".to_owned(), "1800.00".to_owned()), // sensitive edge
        ("supplier".to_owned(), "CellTech GmbH".to_owned()), // sensitive node
    ]);
    SourceRecord {
        source_ref: SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 42,
        },
        fields,
    }
}

/// Stand-in for the capability's deterministic monitor: it reads evidence
/// obtained through the neutral seam and decides risk. The real rule is SPARQL
/// owned by `ekg-semantic`; the seam itself knows nothing about "risk".
fn monitor_flags_risk(grounding: &Grounding) -> bool {
    grounding
        .edges
        .iter()
        .any(|e| e.predicate == "ekg:at_risk" && e.object == "true")
}

#[tokio::test]
async fn governed_loop_composes_over_fakes() -> Result<(), EngineError> {
    let ingestor = FixtureIngestor::from_records([at_risk_order()]);
    let world = InMemoryWorldModel::default();
    let bus = CapturingPublisher::default();
    let cognition = DeterministicCognition;
    let executor = RecordingActionExecutor::default();

    let source_ref = SourceRef {
        system: "odoo".to_owned(),
        model: "sale.order".to_owned(),
        id: 42,
    };

    // Pull the authoritative record, then materialize it.
    let record = ingestor.pull(&source_ref).await?;
    let revision = world.materialize(&record).await?;
    let entity = InMemoryWorldModel::uri_of(&source_ref);

    // Publish the RdfChanged event on its subject.
    let event = RdfChanged {
        entity_uri: entity.clone(),
        named_graph: InMemoryWorldModel::graph_of(&source_ref),
        change_kind: ChangeKind::Created,
        source: source_ref.clone(),
        revision,
    };
    bus.publish(&event).await?;
    assert_eq!(event.subject(), "ekg.rdf.v1.sale.order.created");
    assert_eq!(bus.published()?.len(), 1);

    // A generic SELECT is available for capability rules.
    let rows = world.select("SELECT * WHERE { ?s ?p ?o }").await?;
    assert_eq!(rows.len(), 1, "one materialized entity");

    // Grounding under read-policy: a planner cannot see commercial terms or the
    // supplier node, so both surface as recorded omissions.
    let planner = ReadContext {
        role: "planner".to_owned(),
        purpose: "delivery-promise".to_owned(),
    };
    let grounding = world.ground(&entity, &planner).await?;
    assert!(grounding.has_omissions());
    assert_eq!(grounding.omissions.len(), 2, "one edge + one node hidden");
    assert_eq!(
        grounding.edges.len(),
        3,
        "three non-sensitive fields remain"
    );

    // The monitor decides risk from that evidence, not the seam.
    assert!(
        monitor_flags_risk(&grounding),
        "fixture is engineered at risk"
    );

    // A privileged role sees everything, so there are no omissions.
    let procurement = ReadContext {
        role: "procurement".to_owned(),
        purpose: "sourcing".to_owned(),
    };
    let full = world.ground(&entity, &procurement).await?;
    assert!(!full.has_omissions());
    assert_eq!(full.edges.len(), 5);

    // The bounded agent builds a prompt (disclosing omissions) and asks the
    // provider-neutral boundary to generate. The model never sees the graph.
    let prompt = format!(
        "Explain the risk for {}. Evidence edges: {}. Omissions withheld: {}.",
        grounding.entity,
        grounding.edges.len(),
        grounding.omissions.len(),
    );
    let explanation = cognition.generate(&prompt).await?;
    assert!(explanation.contains("Omissions withheld: 2"));

    // The agent proposes and never commits. A proposal must disclose the
    // grounding's omissions, or it is rejected at the gate.
    let action = ProposedAction {
        action_type: ActionType("re_promise_date".to_owned()),
        target: entity.clone(),
        params: BTreeMap::from([("new_date".to_owned(), "2027-11-15".to_owned())]),
    };
    let honest = Proposal::single(explanation, action.clone(), grounding.omissions.clone());
    assert!(honest.discloses_omissions_of(&grounding));

    let silent = Proposal::single("looks fine".to_owned(), action.clone(), Vec::new());
    assert!(
        !silent.discloses_omissions_of(&grounding),
        "a proposal hiding omissions must be rejectable"
    );

    // A human promotes, then the governed action executes and can be reversed.
    let promoted = PromotedAction {
        action: action.clone(),
        promotion: Promotion {
            action_point: ActionPointId("ap-42".to_owned()),
            correlation_id: CorrelationId("corr-42".to_owned()),
            reviewer: ReviewerId("reviewer-1".to_owned()),
            rationale: "expedite covers the promise".to_owned(),
            context_seen: "2 omissions disclosed".to_owned(),
        },
        basis_revision: revision,
    };
    let receipt = executor.execute(&promoted).await?;
    assert_eq!(executor.executed()?.len(), 1);
    assert!(receipt.reversible);
    assert_eq!(
        receipt.source_revision, revision,
        "receipt carries the basis"
    );
    executor.reverse(&receipt).await?;
    assert_eq!(executor.reversed()?.len(), 1);

    // Embeddings feed the non-authoritative projection.
    let vector = cognition.embed("battery pack 500wh").await?;
    assert_eq!(vector.len(), 8);

    Ok(())
}

#[test]
fn domain_types_round_trip_json_in_snake_case() -> Result<(), serde_json::Error> {
    // Events cross the NATS wire; action points are Restate virtual-object state
    // and API payloads. Both must (de)serialize, with enums in snake_case.
    let event = RdfChanged {
        entity_uri: crate::EntityUri("https://ekg.example/id/sale.order/42".to_owned()),
        named_graph: crate::NamedGraph("https://ekg.example/graph/sale.order/42".to_owned()),
        change_kind: ChangeKind::Created,
        source: SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 42,
        },
        revision: crate::Revision(7),
    };
    let json = serde_json::to_string(&event)?;
    assert!(
        json.contains(r#""change_kind":"created""#),
        "enums are snake_case"
    );
    assert!(json.contains(r#""revision":7"#), "newtype is transparent");
    let back: RdfChanged = serde_json::from_str(&json)?;
    assert_eq!(back, event);

    let point = crate::ActionPoint {
        id: crate::ActionPointId("ap-1".to_owned()),
        subject: event.entity_uri.clone(),
        state: crate::ActionPointState::UnderReview,
        reason: "promise not coverable".to_owned(),
        proposal: None,
    };
    let json = serde_json::to_string(&point)?;
    assert!(json.contains(r#""state":"under_review""#));
    assert_eq!(serde_json::from_str::<crate::ActionPoint>(&json)?, point);

    Ok(())
}

#[tokio::test]
async fn stale_events_do_not_overwrite_newer_state() -> Result<(), EngineError> {
    let world = InMemoryWorldModel::default();
    let consumer = ProjectionConsumer::default();
    let record = at_risk_order();
    let source_ref = record.source_ref.clone();
    let entity = InMemoryWorldModel::uri_of(&source_ref);

    let make_event = |revision| RdfChanged {
        entity_uri: entity.clone(),
        named_graph: InMemoryWorldModel::graph_of(&source_ref),
        change_kind: ChangeKind::Updated,
        source: source_ref.clone(),
        revision,
    };

    let r1 = world.materialize(&record).await?;
    consumer.apply(&make_event(r1))?;

    // A replay at the same revision is rejected (at-least-once or out-of-order delivery).
    let replay = consumer.apply(&make_event(r1));
    assert!(matches!(replay, Err(EngineError::StaleRevision { .. })));

    // A newer revision applies.
    let r2 = world.materialize(&record).await?;
    assert!(r2 > r1);
    consumer.apply(&make_event(r2))?;

    Ok(())
}

/// Atomic plan orchestration (`execute_plan` / `reverse_plan`) exercised over a
/// scripted fake executor: a clean plan, a rolled-back mid-plan failure, a
/// compound partial-application, and a best-effort reverse.
mod plan_orchestration {
    #![allow(clippy::expect_used)]
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::action_point::{
        ExecutionPlan, ExecutionReceipt, PromotedAction, PromotedPlan, Promotion,
    };
    use crate::error::{EngineError, Result};
    use crate::ids::{ActionPointId, CorrelationId, EntityUri, ReviewerId, Revision};
    use crate::proposal::{ActionType, Plan, ProposedAction};
    use crate::seams::ActionExecutor;

    /// A fake [`ActionExecutor`] with injectable failure: `execute` fails for one
    /// target, and `reverse` refuses (a watermark conflict) for a configured set.
    /// That is enough to drive the rollback and best-effort-reverse paths.
    #[derive(Default)]
    struct ScriptedActionExecutor {
        fail_execute_on: Option<EntityUri>,
        refuse_reverse_on: BTreeSet<String>,
        executed: Mutex<Vec<ProposedAction>>,
        reversed: Mutex<Vec<ProposedAction>>,
    }

    #[async_trait]
    impl ActionExecutor for ScriptedActionExecutor {
        async fn execute(&self, promoted: &PromotedAction) -> Result<ExecutionReceipt> {
            let action = &promoted.action;
            if self.fail_execute_on.as_ref() == Some(&action.target) {
                return Err(EngineError::Backend(format!(
                    "scripted write failure for {}",
                    action.target
                )));
            }
            self.executed
                .lock()
                .map_err(|_| EngineError::Backend("poisoned".to_owned()))?
                .push(action.clone());
            Ok(ExecutionReceipt {
                action: action.clone(),
                external_ref: format!("scripted://{}", action.target),
                source_revision: promoted.basis_revision,
                reversible: true,
            })
        }

        async fn reverse(&self, receipt: &ExecutionReceipt) -> Result<()> {
            if self.refuse_reverse_on.contains(&receipt.action.target.0) {
                return Err(EngineError::ReverseConflict {
                    external_ref: receipt.external_ref.clone(),
                    executed: receipt.source_revision.0,
                    found: receipt.source_revision.0 + 1,
                });
            }
            self.reversed
                .lock()
                .map_err(|_| EngineError::Backend("poisoned".to_owned()))?
                .push(receipt.action.clone());
            Ok(())
        }
    }

    impl ScriptedActionExecutor {
        fn executed(&self) -> Vec<ProposedAction> {
            self.executed.lock().expect("lock").clone()
        }
        fn reversed(&self) -> Vec<ProposedAction> {
            self.reversed.lock().expect("lock").clone()
        }
    }

    fn target(i: u32) -> EntityUri {
        EntityUri(format!("https://ekg.example/id/sale.order/{i}"))
    }

    fn action(i: u32) -> ProposedAction {
        ProposedAction {
            action_type: ActionType(format!("verb_{i}")),
            target: target(i),
            params: BTreeMap::new(),
        }
    }

    /// A promoted plan of `n` actions sharing one promotion.
    fn promoted_plan(n: u32) -> PromotedPlan {
        PromotedPlan {
            plan: Plan {
                actions: (0..n).map(action).collect(),
            },
            promotion: Promotion {
                action_point: ActionPointId("ap-plan".to_owned()),
                correlation_id: CorrelationId("corr-plan".to_owned()),
                reviewer: ReviewerId("planner-1".to_owned()),
                rationale: "slate approved".to_owned(),
                context_seen: "evidence for N actions".to_owned(),
            },
            basis_revision: Revision(1_700_000_000),
        }
    }

    #[test]
    fn promoted_actions_carry_the_single_shared_promotion() {
        let plan = promoted_plan(3);
        let derived = plan.promoted_actions();
        assert_eq!(derived.len(), 3);
        // Every derived per-action promotion is the plan's single promotion and basis.
        for (i, promoted) in derived.iter().enumerate() {
            assert_eq!(promoted.promotion, plan.promotion, "one shared promotion");
            assert_eq!(promoted.basis_revision, plan.basis_revision);
            assert_eq!(
                promoted.action, plan.plan.actions[i],
                "listed order preserved"
            );
        }
    }

    #[tokio::test]
    async fn a_clean_plan_applies_all_actions_and_records_n_receipts() {
        let exec = ScriptedActionExecutor::default();
        let plan = promoted_plan(3);
        let executed = exec.execute_plan(&plan).await.expect("clean plan applies");
        assert_eq!(executed.receipts.len(), 3, "one receipt per action");
        assert_eq!(executed.actions.len(), 3);
        assert_eq!(exec.executed().len(), 3);
        assert!(exec.reversed().is_empty(), "no rollback on success");
    }

    #[tokio::test]
    async fn a_mid_plan_failure_rolls_back_the_already_applied_actions() {
        let exec = ScriptedActionExecutor {
            fail_execute_on: Some(target(2)),
            ..Default::default()
        };
        let plan = promoted_plan(4);
        let err = exec
            .execute_plan(&plan)
            .await
            .expect_err("a mid-plan failure is returned");
        // The original per-action failure surfaces: the rollback was clean, so there is
        // no compound error.
        assert!(matches!(err, EngineError::Backend(_)), "got {err:?}");
        // Actions 0 and 1 committed, then both were reversed in reverse order.
        assert_eq!(exec.executed(), vec![action(0), action(1)]);
        assert_eq!(
            exec.reversed(),
            vec![action(1), action(0)],
            "rolled back in reverse order, leaving nothing applied"
        );
    }

    #[tokio::test]
    async fn a_rollback_that_hits_a_conflict_surfaces_a_compound_error() {
        // Fail at action 2 and refuse to reverse action 0 (a concurrent edit advanced it
        // past our write), so the plan is partially applied.
        let exec = ScriptedActionExecutor {
            fail_execute_on: Some(target(2)),
            refuse_reverse_on: BTreeSet::from([target(0).0]),
            ..Default::default()
        };
        let plan = promoted_plan(3);
        let err = exec
            .execute_plan(&plan)
            .await
            .expect_err("a partial application is surfaced");
        match err {
            EngineError::PlanPartiallyApplied { unreversed, .. } => {
                assert_eq!(unreversed, 1, "action 0's rollback was refused");
            }
            other => panic!("expected PlanPartiallyApplied, got {other:?}"),
        }
        // Action 1 was rolled back; action 0 could not be, so it stays applied.
        assert_eq!(exec.reversed(), vec![action(1)]);
    }

    #[tokio::test]
    async fn reverse_plan_undoes_in_reverse_order_reporting_per_action_conflicts() {
        // Build an executed plan of 3, then reverse with action 1 refusing.
        let exec = ScriptedActionExecutor {
            refuse_reverse_on: BTreeSet::from([target(1).0]),
            ..Default::default()
        };
        let receipts: Vec<ExecutionReceipt> = (0..3)
            .map(|i| ExecutionReceipt {
                action: action(i),
                external_ref: format!("scripted://{}", target(i)),
                source_revision: Revision(1_700_000_000),
                reversible: true,
            })
            .collect();
        let executed = ExecutionPlan {
            actions: (0..3).map(action).collect(),
            receipts,
        };
        let err = exec
            .reverse_plan(&executed)
            .await
            .expect_err("one action refuses its reverse");
        match err {
            EngineError::PlanReverseIncomplete {
                total, unreversed, ..
            } => {
                assert_eq!(total, 3);
                assert_eq!(unreversed, 1, "only action 1 refused");
            }
            other => panic!("expected PlanReverseIncomplete, got {other:?}"),
        }
        // Reverse order 2, (1 refused), 0: the rest are still undone.
        assert_eq!(exec.reversed(), vec![action(2), action(0)]);
    }

    /// An executed plan of `n` actions with one receipt each.
    fn executed_plan(n: u32) -> ExecutionPlan {
        ExecutionPlan {
            actions: (0..n).map(action).collect(),
            receipts: (0..n)
                .map(|i| ExecutionReceipt {
                    action: action(i),
                    external_ref: format!("scripted://{}", target(i)),
                    source_revision: Revision(1_700_000_000),
                    reversible: true,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn reverse_plan_remaining_reports_each_receipt_and_never_stops_on_a_refusal() {
        let exec = ScriptedActionExecutor {
            refuse_reverse_on: BTreeSet::from([target(2).0]),
            ..Default::default()
        };
        let outcome = exec
            .reverse_plan_remaining(&executed_plan(4), &BTreeSet::new())
            .await
            .expect("a refusal is an outcome, not an error");
        // Reverse order 3, (2 refused), 1, 0.
        assert_eq!(outcome.reversed, vec![3, 1, 0]);
        assert_eq!(outcome.refused.len(), 1);
        let (index, reason) = &outcome.refused[0];
        assert_eq!(*index, 2);
        assert!(
            reason.starts_with("verb_2 on https://ekg.example/id/sale.order/2 ("),
            "the refusal names the action: {reason}"
        );
        assert_eq!(exec.reversed(), vec![action(3), action(1), action(0)]);
    }

    #[tokio::test]
    async fn reverse_plan_remaining_skips_the_receipts_already_reversed() {
        let exec = ScriptedActionExecutor::default();
        let outcome = exec
            .reverse_plan_remaining(&executed_plan(3), &BTreeSet::from([0, 2]))
            .await
            .expect("resume");
        assert_eq!(outcome.reversed, vec![1], "only the remaining receipt");
        assert!(outcome.refused.is_empty());
        assert_eq!(exec.reversed(), vec![action(1)]);
    }

    #[tokio::test]
    async fn reverse_plan_remaining_refuses_a_progress_over_another_plan() {
        let exec = ScriptedActionExecutor::default();
        let err = exec
            .reverse_plan_remaining(&executed_plan(2), &BTreeSet::from([2]))
            .await
            .expect_err("index 2 is not in a plan of 2");
        assert!(matches!(err, EngineError::Invariant(_)), "{err:?}");
        assert!(exec.reversed().is_empty(), "nothing reversed");
    }

    #[tokio::test]
    async fn reverse_plan_still_maps_a_refusal_to_plan_reverse_incomplete() {
        let exec = ScriptedActionExecutor {
            refuse_reverse_on: BTreeSet::from([target(0).0, target(1).0]),
            ..Default::default()
        };
        let err = exec
            .reverse_plan(&executed_plan(3))
            .await
            .expect_err("two refusals");
        let EngineError::PlanReverseIncomplete {
            total,
            unreversed,
            details,
        } = err
        else {
            panic!("expected PlanReverseIncomplete, got {err:?}");
        };
        assert_eq!((total, unreversed), (3, 2));
        assert!(
            details.starts_with("verb_1 on ") && details.contains("; verb_0 on "),
            "the refusals in reverse order: {details}"
        );
        exec.reverse_plan(&executed_plan(0))
            .await
            .expect("an empty plan reverses cleanly");
    }
}

#[tokio::test]
async fn retracted_entity_is_excluded_from_reads() -> Result<(), EngineError> {
    let world = InMemoryWorldModel::default();
    let record = at_risk_order();
    let source_ref = record.source_ref.clone();
    let entity = InMemoryWorldModel::uri_of(&source_ref);

    world.materialize(&record).await?;
    assert_eq!(world.select("SELECT *").await?.len(), 1);

    world.retract(&entity).await?;

    // A retracted entity does not ground or appear in queries, so it cannot drive
    // detections, but its revision is retained for stale-event protection.
    assert!(matches!(
        world
            .ground(
                &entity,
                &ReadContext {
                    role: "planner".to_owned(),
                    purpose: "delivery-promise".to_owned(),
                },
            )
            .await,
        Err(EngineError::NotFound(_))
    ));
    assert_eq!(world.select("SELECT *").await?.len(), 0);

    Ok(())
}

/// The Simulator seam: a hypothetical plan is projected onto a non-authoritative
/// read model, and a capability's own rule (a test-local overlap check here,
/// standing in for `PROCESS_CONTENTION_SELECT_SPARQL`) reads the outcome from it.
/// The seam never learns the rule; the base is never mutated.
mod simulator {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;

    use crate::fakes::InMemorySimulator;
    use crate::ids::EntityUri;
    use crate::proposal::{ActionType, Plan, ProposedAction};
    use crate::seams::Simulator;

    fn wo(wc: &str, start: &str, end: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("work_center".to_owned(), wc.to_owned()),
            ("startedAt".to_owned(), start.to_owned()),
            ("endedAt".to_owned(), end.to_owned()),
        ])
    }

    /// A capacity-1 work center with two overlapping work orders, so the base is in
    /// contention.
    fn contended_sim() -> InMemorySimulator {
        InMemorySimulator::default()
            .with_entity(EntityUri("wo/1".to_owned()), wo("wc/1", "10", "20"))
            .with_entity(EntityUri("wo/2".to_owned()), wo("wc/1", "15", "25"))
    }

    /// The capability's own rule, run over projected rows: the entities that still
    /// overlap another on the same work center (concurrency > capacity 1).
    fn contended_subjects(rows: &[BTreeMap<String, String>]) -> Vec<String> {
        let parse = |r: &BTreeMap<String, String>, k: &str| r[k].parse::<i64>().unwrap_or_default();
        let mut hits = Vec::new();
        for a in rows {
            let overlaps = rows.iter().any(|b| {
                b["entity"] != a["entity"]
                    && b["work_center"] == a["work_center"]
                    && parse(a, "startedAt") < parse(b, "endedAt")
                    && parse(b, "startedAt") < parse(a, "endedAt")
            });
            if overlaps {
                hits.push(a["entity"].clone());
            }
        }
        hits.sort();
        hits
    }

    fn reschedule(target: &str, start: &str, end: &str) -> Plan {
        Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("reschedule_work_order".to_owned()),
                target: EntityUri(target.to_owned()),
                params: BTreeMap::from([
                    ("startedAt".to_owned(), start.to_owned()),
                    ("endedAt".to_owned(), end.to_owned()),
                ]),
            }],
        }
    }

    #[tokio::test]
    async fn a_plan_that_resolves_the_contention_clears_the_rule_under_projection() {
        let sim = contended_sim();
        // Move wo/2 clear of wo/1's window: the capability's rule should see no contention.
        let projection = sim
            .project(&reschedule("wo/2", "30", "40"))
            .await
            .expect("projection");
        let rows = projection
            .select("<capability rule SPARQL>")
            .await
            .expect("projection");
        assert!(
            contended_subjects(&rows).is_empty(),
            "rescheduling out of the window should clear the signal under projection"
        );
    }

    #[tokio::test]
    async fn a_plan_that_does_not_resolve_the_contention_still_fires() {
        let sim = contended_sim();
        // Still overlaps wo/1's 10 to 20 window, so the signal must remain.
        let projection = sim
            .project(&reschedule("wo/2", "18", "28"))
            .await
            .expect("projection");
        let rows = projection
            .select("<capability rule SPARQL>")
            .await
            .expect("projection");
        assert_eq!(
            contended_subjects(&rows),
            vec!["wo/1".to_owned(), "wo/2".to_owned()],
            "an insufficient plan leaves the contention observable"
        );
    }

    #[tokio::test]
    async fn projecting_never_mutates_the_base() {
        let sim = contended_sim();
        // A clearing projection first...
        let _ = sim
            .project(&reschedule("wo/2", "30", "40"))
            .await
            .expect("projection");
        // ...then an empty plan must still reproduce the original contended base, so
        // the earlier projection changed nothing authoritative.
        let baseline = sim
            .project(&Plan { actions: vec![] })
            .await
            .expect("projection");
        let rows = baseline
            .select("<capability rule SPARQL>")
            .await
            .expect("projection");
        assert_eq!(
            contended_subjects(&rows),
            vec!["wo/1".to_owned(), "wo/2".to_owned()],
            "the seeded base is untouched by any projection"
        );
    }
}

/// The Scorer and Slate seams: the observe and rank stages of the cognition loop.
/// A capability's own rule (a fake here) scores a projected hypothetical into an
/// opaque [`Score`]; a slate carries ordered candidates and a do-nothing baseline and
/// commits nothing.
mod slate {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;

    use crate::fakes::{CannedSlateProposer, FakeScorer, InMemorySimulator};
    use crate::ids::EntityUri;
    use crate::proposal::{ActionType, Plan, ProposedAction};
    use crate::seams::{Scorer, Simulator, SlateProposer};

    /// One capacity-strained entity: a `signal=on` field stands in for "the drift
    /// signal is present". A plan that flips it to `off` clears the fake rule.
    fn flagged_sim() -> InMemorySimulator {
        InMemorySimulator::default().with_entity(
            EntityUri("wo/1".to_owned()),
            BTreeMap::from([("signal".to_owned(), "on".to_owned())]),
        )
    }

    /// A plan that sets `signal=off` on the entity, clearing the fake rule under
    /// projection.
    fn clearing_plan() -> Plan {
        Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("reschedule_work_order".to_owned()),
                target: EntityUri("wo/1".to_owned()),
                params: BTreeMap::from([("signal".to_owned(), "off".to_owned())]),
            }],
        }
    }

    #[tokio::test]
    async fn a_plan_that_clears_the_fake_rule_outranks_the_do_nothing_baseline() {
        let sim = flagged_sim();
        let scorer = FakeScorer::flagging("signal", "on");

        // Baseline: score the empty plan (do nothing). The signal is still present.
        let baseline_proj = sim.project(&Plan { actions: vec![] }).await.expect("proj");
        let baseline = scorer.score(baseline_proj.as_ref()).await.expect("score");

        // A candidate that clears the signal under projection.
        let candidate_proj = sim.project(&clearing_plan()).await.expect("proj");
        let candidate = scorer.score(candidate_proj.as_ref()).await.expect("score");

        assert_eq!(
            baseline.metrics.get("flagged").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            candidate.metrics.get("flagged").map(String::as_str),
            Some("0")
        );
        // Best-first: the clearing candidate's rank_key sorts before the baseline's.
        assert!(
            candidate.rank_key < baseline.rank_key,
            "a plan that clears the signal must outrank doing nothing"
        );
    }

    #[tokio::test]
    async fn the_plan_aware_scores_default_to_the_plain_ones() {
        use crate::fakes::FakeGroupScorer;
        use crate::seams::GroupScorer;
        use crate::slate::SubjectGroup;

        let sim = flagged_sim();
        let plan = clearing_plan();
        let projection = sim.project(&plan).await.expect("proj");
        let scorer = FakeScorer::flagging("signal", "on");
        assert_eq!(
            scorer
                .score_plan(&plan, projection.as_ref())
                .await
                .expect("score"),
            scorer.score(projection.as_ref()).await.expect("score"),
        );

        let group = SubjectGroup {
            key: "portfolio/abc".to_owned(),
            members: vec![EntityUri("wo/1".to_owned())],
            links: vec![],
            worded: false,
        };
        let group_scorer = FakeGroupScorer::flagging("signal", "on");
        assert_eq!(
            group_scorer
                .score_group_plan(&plan, projection.as_ref(), &group)
                .await
                .expect("score"),
            group_scorer
                .score_group(projection.as_ref(), &group)
                .await
                .expect("score"),
        );
    }

    #[tokio::test]
    async fn scoring_a_projection_mutates_no_authoritative_state() {
        let sim = flagged_sim();
        let scorer = FakeScorer::flagging("signal", "on");
        // Score a clearing projection first...
        let cleared = sim.project(&clearing_plan()).await.expect("proj");
        let _ = scorer.score(cleared.as_ref()).await.expect("score");
        // ...then the do-nothing baseline must still see the original signal, so
        // neither the projection nor the scorer touched the seeded base.
        let baseline_proj = sim.project(&Plan { actions: vec![] }).await.expect("proj");
        let baseline = scorer.score(baseline_proj.as_ref()).await.expect("score");
        assert_eq!(
            baseline.metrics.get("flagged").map(String::as_str),
            Some("1"),
            "the seeded base is untouched by projecting or scoring"
        );
    }

    #[tokio::test]
    async fn a_slate_carries_ordered_options_and_a_baseline_each_disclosing_omissions() {
        let subject = EntityUri("https://ekg.example/id/sale.order/1".to_owned());
        let slate = CannedSlateProposer
            .propose_slate(&subject)
            .await
            .expect("slate");

        // A do-nothing baseline is always present.
        assert!(!slate.baseline.rank_key.is_empty());
        // Options are ordered best-first by rank_key.
        assert!(slate.options.len() >= 2);
        assert!(
            slate
                .options
                .windows(2)
                .all(|w| w[0].score.rank_key <= w[1].score.rank_key),
            "options must be ordered best-first"
        );
        // Every option beats the baseline and discloses the omissions of the
        // grounding it was built from.
        for option in &slate.options {
            assert!(option.score.rank_key < slate.baseline.rank_key);
            assert!(
                !option.omissions.is_empty(),
                "each candidate must carry its grounding's omissions"
            );
        }
        // At most one option is recommended, and it is the best-ranked (first).
        let recommended = slate.options.iter().filter(|o| o.recommended).count();
        assert_eq!(recommended, 1, "exactly one recommended option");
        assert!(
            slate.options[0].recommended,
            "the best-ranked option is recommended"
        );
        assert_eq!(slate.recommended().map(|r| r.recommended), Some(true));
    }

    #[tokio::test]
    async fn the_group_fakes_answer_for_a_group_of_members() {
        use crate::fakes::{CannedGroupProposer, CannedGroupSlateProposer, FakeGroupScorer};
        use crate::seams::{GroupCandidateProposer, GroupScorer, GroupSlateProposer};
        use crate::slate::{SubjectGroup, TraceTag};

        let group = SubjectGroup {
            key: "portfolio/abc".to_owned(),
            members: vec![EntityUri("wo/1".to_owned()), EntityUri("so/1".to_owned())],
            links: vec!["linked: wo/1 fulfils so/1".to_owned()],
            worded: false,
        };

        // The proposer plans across every member and names them all as members.
        let grounded = CannedGroupProposer
            .propose_group_candidate(&group, &[])
            .await
            .expect("candidate");
        assert_eq!(grounded.proposal.actions.len(), 2);
        assert_eq!(grounded.subject_members, group.members);

        // The scorer counts only flagged members: wo/1 is flagged, an outsider is not
        // counted, and so/1 is not in the projection.
        let sim = flagged_sim().with_entity(
            EntityUri("outsider".to_owned()),
            BTreeMap::from([("signal".to_owned(), "on".to_owned())]),
        );
        let projection = sim.project(&Plan { actions: vec![] }).await.expect("proj");
        let score = FakeGroupScorer::flagging("signal", "on")
            .score_group(projection.as_ref(), &group)
            .await
            .expect("score");
        assert_eq!(
            score.metrics.get("members_flagged").map(String::as_str),
            Some("1")
        );

        // The slate opens with the Linked step naming every member and link.
        let slate = CannedGroupSlateProposer
            .propose_group_slate(&group)
            .await
            .expect("slate");
        let first = slate.trace.steps.first().expect("a first step");
        assert_eq!(first.tag, TraceTag::Graph);
        assert!(
            first.text.starts_with("Linked: wo/1 and so/1."),
            "{}",
            first.text
        );
        assert!(first.text.contains("fulfils"), "{}", first.text);
        assert_eq!(slate.options.len(), 1);
        assert!(slate.options[0].score.rank_key < slate.baseline.rank_key);
    }
}
