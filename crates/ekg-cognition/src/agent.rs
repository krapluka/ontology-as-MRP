//! The bounded proposal agent: grounds an action point's subject, asks the model
//! for an explanation and a governed action, and assembles a [`GroundedProposal`].
//!
//! The machinery is generic. The capability-specific parts, how to turn an evidence
//! subgraph into a prompt and how to parse the model's chosen action from an allowed
//! catalog, are injected via [`AgentSpec`], like the monitor's injected `RiskRule`, so
//! another capability reuses the agent unchanged.
//!
//! The model is never in the truth path: the agent only reads the world (`ground`)
//! and generates text. Its output is a candidate proposal; it writes and commits nothing.

use std::sync::Arc;

use async_trait::async_trait;
use ekg_domain::{
    CandidateProposer, CognitionProvider, EngineError, EntityUri, GroundedProposal, Grounding,
    GroupCandidateProposer, Proposal, ProposedAction, Proposer, ReadContext, Result,
    ScoredCandidate, SubjectGroup, WorldModel,
};

use crate::evidence::{self, evidenced_members, merge_grounding};
use crate::plan::{self, PlanDefect};

/// The default per-plan action bound when none is configured: a plan stays reviewable
/// in one oversight decision (EU AI Act Art. 14) and the generation loop terminates.
/// The bound is injected config, like the allowlist and cost limit, and not part of the
/// neutral `Plan` aggregate; this is only the fallback.
pub const DEFAULT_MAX_PLAN_ACTIONS: usize = 4;

/// Builds the bounded prompt from the read-policy-filtered evidence subgraph and the
/// slate loop's feedback: the candidates already tried and their opaque scores, so the
/// prompt can steer the model toward a distinct, better candidate. The feedback is
/// empty for the single-proposal path and the first slate candidate.
pub type PromptBuilder = Arc<dyn Fn(&Grounding, &[ScoredCandidate]) -> String + Send + Sync>;
/// Parses the model response into a plan of one or more governed actions from the
/// allowed catalog, or rejects it (unparseable, out-of-catalog or over-reaching). The
/// read-policy-filtered [`Grounding`] is passed so a parser can validate each chosen
/// target against the evidence the agent saw. An empty result is an abstention,
/// surfaced as an [`EngineError::InvalidProposal`] like any other reject.
pub type ActionParser =
    Arc<dyn Fn(&str, &EntityUri, &Grounding) -> Result<Vec<ProposedAction>> + Send + Sync>;

/// An optional capability hook that extends the subject's one-hop grounding: it names
/// further entities the subject's situation rests on (e.g. what causes its drift), and the
/// agent grounds each through the same `WorldModel::ground` and read context as the subject.
/// Their edges therefore pass the same read-policy filter and carry the same mandatory
/// provenance, and they join the subject's members, so a named entity becomes a legal target
/// for the attach gate. Without a hook the grounding is the subject's alone.
#[async_trait]
pub trait GroundingExtension: Send + Sync {
    /// The entities, beyond `subject`, whose evidence the agent also grounds on. An
    /// entity that is not in the world model is skipped.
    ///
    /// # Errors
    /// A world-model read failure, which fails the proposal attempt.
    async fn related(&self, subject: &EntityUri) -> Result<Vec<EntityUri>>;
}

/// The capability specialization injected into the neutral agent.
#[derive(Clone)]
pub struct AgentSpec {
    /// Role + purpose the grounding is read under (read-policy scope).
    pub read_context: ReadContext,
    /// Turns the evidence subgraph into a bounded prompt.
    pub build_prompt: PromptBuilder,
    /// Turns the model response into a governed action (or rejects it).
    pub parse_action: ActionParser,
    /// Extends the subject's grounding with related entities' evidence, if set.
    pub extend_grounding: Option<Arc<dyn GroundingExtension>>,
}

/// The bounded proposal agent over its two seams and the injected spec.
#[derive(Clone)]
pub struct BoundedAgent {
    world: Arc<dyn WorldModel>,
    provider: Arc<dyn CognitionProvider>,
    spec: AgentSpec,
    /// The maximum number of actions a produced plan may carry. A parse of more than
    /// this is rejected, not truncated, so a plan never exceeds what one oversight
    /// decision can review.
    max_actions: usize,
}

impl BoundedAgent {
    /// Wire the agent over the world-model + cognition seams and a capability spec,
    /// bounding plans at [`DEFAULT_MAX_PLAN_ACTIONS`].
    #[must_use]
    pub fn new(
        world: Arc<dyn WorldModel>,
        provider: Arc<dyn CognitionProvider>,
        spec: AgentSpec,
    ) -> Self {
        Self::with_max_actions(world, provider, spec, DEFAULT_MAX_PLAN_ACTIONS)
    }

    /// Wire the agent with an explicit per-plan action bound (config-injected). A bound
    /// of zero would forbid every plan, so it is clamped to at least one.
    #[must_use]
    pub fn with_max_actions(
        world: Arc<dyn WorldModel>,
        provider: Arc<dyn CognitionProvider>,
        spec: AgentSpec,
        max_actions: usize,
    ) -> Self {
        Self {
            world,
            provider,
            spec,
            max_actions: max_actions.max(1),
        }
    }

    /// Ground `subject` under this agent's read context: its read-policy-filtered
    /// grounding (hidden facts surface as omissions), extended by the capability's hook
    /// with each related entity grounded under the same read context and merged in.
    ///
    /// # Errors
    /// [`EngineError::NotFound`] if the subject is unknown; a world-model read failure.
    pub async fn ground(&self, subject: &EntityUri) -> Result<GroundedSubject> {
        evidence::ground_extended(
            self.world.as_ref(),
            &self.spec.read_context,
            self.spec.extend_grounding.as_deref(),
            subject,
        )
        .await
    }
}

/// A subject's grounding as the bounded agent reads it (see [`BoundedAgent::ground`]):
/// the merged, read-policy-filtered evidence and the entities the capability's hook
/// added to it.
#[derive(Debug, Clone)]
pub struct GroundedSubject {
    /// The subject's grounding, with every extended entity's evidence merged in.
    pub grounding: Grounding,
    /// The entities the hook grounded beyond the subject, in the order it named them.
    pub extended: Vec<EntityUri>,
}

#[async_trait]
impl Proposer for BoundedAgent {
    async fn propose(&self, subject: &EntityUri) -> Result<GroundedProposal> {
        // The single-proposal path is the first candidate with no feedback.
        self.propose_candidate(subject, &[]).await
    }
}

#[async_trait]
impl CandidateProposer for BoundedAgent {
    async fn propose_candidate(
        &self,
        subject: &EntityUri,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        // Read-policy-filtered grounding (hidden facts surface as omissions), extended
        // by the capability's hook.
        let GroundedSubject {
            grounding,
            extended,
        } = self.ground(subject).await?;
        // The prompt is built from the evidence subgraph and the revise feedback (the
        // plans already tried and their scores), not from the raw store.
        let prompt = (self.spec.build_prompt)(&grounding, feedback);
        // The only model call; it writes nothing.
        let explanation = self.provider.generate(&prompt).await?;
        // Parse into a plan of one or more governed actions; an unparseable/
        // out-of-catalog response is rejected rather than coerced, so the model cannot
        // widen its authority. The grounding is passed so a parser can check each
        // target against the evidence.
        let actions = (self.spec.parse_action)(&explanation, subject, &grounding)?;
        let actions = bounded_plan(actions, self.max_actions)?;
        // The entities the subject's evidence references (see `evidenced_members`). The
        // attach gate uses these to allow a proposal to target the subject or one of its
        // evidenced members (e.g. a work order of an order).
        let subject_members = evidenced_members(extended, &grounding);
        let proposal = Proposal {
            explanation,
            actions,
            omissions: grounding.omissions.clone(),
        };
        Ok(GroundedProposal {
            proposal,
            grounding_omissions: grounding.omissions,
            // Carry the evidence edges so the slate loop can emit Graph trace steps
            // naming the governed facts this candidate rested on.
            grounding_edges: grounding.edges,
            subject_members,
        })
    }
}

/// Turn a parsed action list into a bounded plan by the shared plan rules
/// ([`plan::bounded_plan`]): an empty parse is an abstention, exact-duplicate actions are
/// dropped, and an over-long plan is rejected rather than truncated, so the generation
/// loop stays terminating.
///
/// An empty parse is surfaced as a reject so both parser styles (Err or empty Vec) agree.
fn bounded_plan(actions: Vec<ProposedAction>, max_actions: usize) -> Result<Vec<ProposedAction>> {
    plan::bounded_plan(actions, max_actions).map_err(|defect| {
        EngineError::InvalidProposal(match defect {
            PlanDefect::Empty => {
                "agent abstained: the response carried no governed action".to_owned()
            }
            PlanDefect::OverBound { actions, max } => {
                format!("proposed plan has {actions} actions, over the per-plan maximum of {max}")
            }
        })
    })
}

/// Routes a group member to the bounded agent of its own capability, so each member is
/// grounded under its own read context and grounding extension.
pub type MemberRouter = Arc<dyn Fn(&EntityUri) -> Arc<BoundedAgent> + Send + Sync>;
/// Builds the one joint prompt for a group from the group itself (its members and
/// links), the members' merged grounding, and the revise feedback.
pub type GroupPromptBuilder =
    Arc<dyn Fn(&SubjectGroup, &Grounding, &[ScoredCandidate]) -> String + Send + Sync>;
/// Parses the joint model response into a plan of governed actions against the merged
/// grounding, or rejects it (unparseable / out-of-catalog / over-reaching).
pub type GroupActionParser =
    Arc<dyn Fn(&str, &Grounding) -> Result<Vec<ProposedAction>> + Send + Sync>;

/// The neutral group proposal agent: one plan across a group of coupled subjects.
///
/// Each member is grounded by the bounded agent of its own capability (the injected
/// [`MemberRouter`]), under its own read context and grounding extension, so each
/// member's evidence is filtered and its omissions disclosed as for its own point. The
/// groundings are merged (identical edges once, omissions summed per type), one joint
/// prompt is asked, and the reply is parsed, de-duplicated and bounded as a point plan
/// is. Every action must target an entity some member's grounding evidences (or a
/// member itself). It names no capability and commits nothing.
#[derive(Clone)]
pub struct GroupAgent {
    router: MemberRouter,
    provider: Arc<dyn CognitionProvider>,
    build_prompt: GroupPromptBuilder,
    parse_actions: GroupActionParser,
    max_actions: usize,
}

impl GroupAgent {
    /// Wire the group agent: the member router, the provider, the joint prompt builder
    /// and parser, and the per-plan action bound (clamped to at least one).
    #[must_use]
    pub fn new(
        router: MemberRouter,
        provider: Arc<dyn CognitionProvider>,
        build_prompt: GroupPromptBuilder,
        parse_actions: GroupActionParser,
        max_actions: usize,
    ) -> Self {
        Self {
            router,
            provider,
            build_prompt,
            parse_actions,
            max_actions: max_actions.max(1),
        }
    }
}

#[async_trait]
impl GroupCandidateProposer for GroupAgent {
    async fn propose_group_candidate(
        &self,
        group: &SubjectGroup,
        feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        let Some(first) = group.members.first() else {
            return Err(EngineError::InvalidProposal(format!(
                "group {} has no members",
                group.key
            )));
        };
        // Ground every member through its own capability's agent, and merge. The merged
        // grounding is about the first member; its edges carry their own subjects.
        let mut merged = Grounding {
            entity: first.clone(),
            edges: Vec::new(),
            omissions: Vec::new(),
        };
        let mut members: Vec<EntityUri> = Vec::new();
        for member in &group.members {
            let grounded = (self.router)(member).ground(member).await?;
            if !members.contains(member) {
                members.push(member.clone());
            }
            for entity in evidenced_members(grounded.extended, &grounded.grounding) {
                if !members.contains(&entity) {
                    members.push(entity);
                }
            }
            merge_grounding(&mut merged, grounded.grounding);
        }
        // One joint prompt and one generation, the only model call; it writes nothing.
        let prompt = (self.build_prompt)(group, &merged, feedback);
        let explanation = self.provider.generate(&prompt).await?;
        let actions = (self.parse_actions)(&explanation, &merged)?;
        let actions = bounded_plan(actions, self.max_actions)?;
        // An action on an entity no member's grounding evidences is refused: the model
        // cannot widen its reach beyond what the group saw.
        if let Some(outside) = actions.iter().find(|a| !members.contains(&a.target)) {
            return Err(EngineError::InvalidProposal(format!(
                "{} targets {}, outside every member's grounding",
                outside.action_type.0, outside.target
            )));
        }
        let proposal = Proposal {
            explanation,
            actions,
            omissions: merged.omissions.clone(),
        };
        Ok(GroundedProposal {
            proposal,
            grounding_omissions: merged.omissions,
            grounding_edges: merged.edges,
            subject_members: members,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::collections::BTreeMap;

    use ekg_domain::fakes::InMemoryWorldModel;
    use ekg_domain::{
        ActionType, CognitionProvider, EngineError, ProposedAction, Result, SourceRecord, SourceRef,
    };

    use super::*;

    /// A stub provider returning a fixed response (no network).
    struct StubProvider(&'static str);
    #[async_trait]
    impl CognitionProvider for StubProvider {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            Ok(self.0.to_owned())
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Ok(Vec::new())
        }
    }

    /// A demo spec: the prompt lists the evidence, and the parser accepts one verb
    /// ("expedite") from its catalog, rejecting anything else.
    fn spec(role: &str) -> AgentSpec {
        AgentSpec {
            read_context: ReadContext {
                role: role.to_owned(),
                purpose: "delivery-promise".to_owned(),
            },
            build_prompt: Arc::new(|g: &Grounding, tried: &[ScoredCandidate]| {
                format!(
                    "evidence edges: {}; {} already tried; propose an action",
                    g.edges.len(),
                    tried.len()
                )
            }),
            parse_action: Arc::new(|text: &str, subject: &EntityUri, _grounding: &Grounding| {
                // The demo parser emits one action per catalog verb it finds in the
                // reply, so a reply naming several verbs yields a multi-action plan.
                let verb_action = |verb: &str| ProposedAction {
                    action_type: ActionType(verb.to_owned()),
                    target: subject.clone(),
                    params: BTreeMap::new(),
                };
                let actions: Vec<ProposedAction> = ["expedite_po", "re_promise_date"]
                    .into_iter()
                    // Map each catalog verb to a keyword the reply may contain.
                    .filter(|verb| match *verb {
                        "expedite_po" => text.contains("expedite"),
                        "re_promise_date" => text.contains("re-promise"),
                        _ => false,
                    })
                    .map(verb_action)
                    .collect();
                if actions.is_empty() {
                    return Err(EngineError::InvalidProposal(
                        "no action in response".to_owned(),
                    ));
                }
                Ok(actions)
            }),
            extend_grounding: None,
        }
    }

    async fn world_with_supplier() -> (Arc<InMemoryWorldModel>, EntityUri) {
        let world = Arc::new(InMemoryWorldModel::default());
        let mut fields = BTreeMap::new();
        fields.insert("state".to_owned(), "late".to_owned());
        // A sensitive `supplier` field, omitted for a non-procurement reader.
        fields.insert("supplier".to_owned(), "CellTech".to_owned());
        let record = SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: "sale.order".to_owned(),
                id: 1,
            },
            fields,
        };
        world.materialize(&record).await.expect("materialize");
        let subject = InMemoryWorldModel::uri_of(&record.source_ref);
        (world, subject)
    }

    #[tokio::test]
    async fn a_grounded_proposal_is_produced_and_carries_omissions() {
        let (world, subject) = world_with_supplier().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("please expedite the battery PO")),
            spec("planner"), // not procurement, so the supplier is omitted
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        // The single-action path is a 1-element plan.
        assert_eq!(out.proposal.actions.len(), 1);
        assert_eq!(out.proposal.actions[0].action_type.0, "expedite_po");
        assert_eq!(out.proposal.actions[0].target, subject);
        // The grounding hid the supplier node, so it is disclosed as an omission.
        assert!(!out.grounding_omissions.is_empty());
        assert_eq!(out.proposal.omissions, out.grounding_omissions);
        // The agent carries the grounding's visible evidence edges so the slate loop can
        // emit Graph trace steps; returning `Vec::new()` here would disable them in
        // production. The visible `state` edge is present; the omitted `supplier` node
        // is not an edge.
        assert!(
            !out.grounding_edges.is_empty(),
            "the real proposer carries the grounding's evidence edges for Graph steps"
        );
        assert!(
            out.grounding_edges
                .iter()
                .any(|edge| edge.predicate == "ekg:state"),
            "the visible evidence edge is carried through"
        );
    }

    #[tokio::test]
    async fn an_unparseable_response_is_rejected() {
        let (world, subject) = world_with_supplier().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("I am not sure what to do")),
            spec("planner"),
        );
        assert!(agent.propose(&subject).await.is_err());
    }

    #[tokio::test]
    async fn a_multi_action_response_parses_to_a_plan_of_n_actions() {
        // A reply naming two coordinated verbs yields a 2-action plan, the whole plan
        // carried by one grounded proposal.
        let (world, subject) = world_with_supplier().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider(
                "expedite the battery PO, then re-promise the delivery date",
            )),
            spec("planner"),
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        assert_eq!(out.proposal.actions.len(), 2);
        assert_eq!(out.proposal.actions[0].action_type.0, "expedite_po");
        assert_eq!(out.proposal.actions[1].action_type.0, "re_promise_date");
    }

    #[tokio::test]
    async fn an_over_max_plan_is_rejected_not_truncated() {
        // With the bound set to 1, a two-action reply is rejected outright:
        // a plan never exceeds what one oversight decision can review.
        let (world, subject) = world_with_supplier().await;
        let agent = BoundedAgent::with_max_actions(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider(
                "expedite the battery PO, then re-promise the delivery date",
            )),
            spec("planner"),
            1,
        );
        assert!(
            agent.propose(&subject).await.is_err(),
            "an over-max plan is rejected, not silently truncated"
        );
    }

    #[tokio::test]
    async fn exact_duplicate_actions_are_de_duplicated_within_the_plan() {
        // A parser that returns the same action twice (same verb+target+params) must not
        // yield a plan that commits it twice: the agent de-dupes to one.
        let (world, subject) = world_with_supplier().await;
        let dup_subject = subject.clone();
        let spec = AgentSpec {
            read_context: ReadContext {
                role: "planner".to_owned(),
                purpose: "delivery-promise".to_owned(),
            },
            build_prompt: Arc::new(|_g: &Grounding, _t: &[ScoredCandidate]| "prompt".to_owned()),
            parse_action: Arc::new(move |_text, _subject, _grounding| {
                let action = ProposedAction {
                    action_type: ActionType("expedite_po".to_owned()),
                    target: dup_subject.clone(),
                    params: BTreeMap::new(),
                };
                Ok(vec![action.clone(), action])
            }),
            extend_grounding: None,
        };
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("anything")),
            spec,
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        assert_eq!(
            out.proposal.actions.len(),
            1,
            "an exact-duplicate action is committed only once"
        );
    }

    #[tokio::test]
    async fn an_empty_parse_is_an_abstention() {
        // The parser finds no catalog verb, so the parse is empty and the agent abstains
        // (an error the bridge treats as leave-open) instead of returning a zero-action
        // plan.
        let (world, subject) = world_with_supplier().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("no safe remediation is available")),
            spec("planner"),
        );
        assert!(agent.propose(&subject).await.is_err());
    }

    /// A hook naming fixed related entities.
    struct Related(Vec<EntityUri>);
    #[async_trait]
    impl GroundingExtension for Related {
        async fn related(&self, _subject: &EntityUri) -> Result<Vec<EntityUri>> {
            Ok(self.0.clone())
        }
    }

    /// The supplier world plus a purchase-order line (with a sensitive `supplier` field and
    /// a product link) that the subject's one-hop grounding never reaches.
    async fn world_with_a_related_line() -> (Arc<InMemoryWorldModel>, EntityUri, EntityUri) {
        let (world, subject) = world_with_supplier().await;
        let mut fields = BTreeMap::new();
        fields.insert("date_planned".to_owned(), "2027-11-15 08:00:00".to_owned());
        fields.insert("supplier".to_owned(), "CellTech".to_owned());
        let record = SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: "purchase.order.line".to_owned(),
                id: 9,
            },
            fields,
        };
        world.materialize(&record).await.expect("materialize line");
        let line = InMemoryWorldModel::uri_of(&record.source_ref);
        (world, subject, line)
    }

    /// A parser that proposes one expedite on the first grounded purchase-order line.
    fn line_spec(extend: Option<Arc<dyn GroundingExtension>>) -> AgentSpec {
        AgentSpec {
            extend_grounding: extend,
            parse_action: Arc::new(|_text, subject: &EntityUri, g: &Grounding| {
                let target = g
                    .edges
                    .iter()
                    .map(|edge| edge.subject.clone())
                    .find(|s| s != subject)
                    .unwrap_or_else(|| subject.clone());
                Ok(vec![ProposedAction {
                    action_type: ActionType("expedite_po".to_owned()),
                    target,
                    params: BTreeMap::new(),
                }])
            }),
            ..spec("planner")
        }
    }

    #[tokio::test]
    async fn an_extended_entity_is_grounded_and_becomes_a_legal_target() {
        let (world, subject, line) = world_with_a_related_line().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("expedite")),
            line_spec(Some(Arc::new(Related(vec![line.clone()])))),
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        assert!(
            out.grounding_edges
                .iter()
                .any(|e| e.subject == line && e.predicate == "ekg:date_planned"),
            "the related line's evidence is grounded: {:?}",
            out.grounding_edges
        );
        assert!(
            out.subject_members.contains(&line),
            "the grounded line is a legal target"
        );
        assert_eq!(out.proposal.actions[0].target, line);
    }

    #[tokio::test]
    async fn an_extended_entitys_policy_hidden_facts_stay_hidden() {
        let (world, subject, line) = world_with_a_related_line().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("expedite")),
            line_spec(Some(Arc::new(Related(vec![line.clone()])))),
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        assert!(
            !out.grounding_edges
                .iter()
                .any(|e| e.predicate.ends_with("supplier") || e.object == "CellTech"),
            "the line's supplier is hidden from a planner like the subject's"
        );
        // Both the subject's and the line's hidden supplier are disclosed, in one entry.
        let supplier: Vec<_> = out
            .grounding_omissions
            .iter()
            .filter(|o| o.type_iri.contains("upplier"))
            .collect();
        assert_eq!(supplier.len(), 1, "{:?}", out.grounding_omissions);
        assert_eq!(supplier[0].count, 2);
    }

    #[tokio::test]
    async fn without_a_hook_the_grounding_is_the_subjects_alone() {
        let (world, subject, line) = world_with_a_related_line().await;
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("expedite")),
            line_spec(None),
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        assert!(out.grounding_edges.iter().all(|e| e.subject == subject));
        assert!(!out.subject_members.contains(&line));
        assert_eq!(out.proposal.actions[0].target, subject);
    }

    #[tokio::test]
    async fn a_related_entity_missing_from_the_world_is_skipped() {
        let (world, subject) = world_with_supplier().await;
        let ghost = EntityUri("https://ekg.example/id/purchase.order.line/404".to_owned());
        let agent = BoundedAgent::new(
            world as Arc<dyn WorldModel>,
            Arc::new(StubProvider("expedite")),
            line_spec(Some(Arc::new(Related(vec![ghost.clone()])))),
        );
        let out = agent.propose(&subject).await.expect("propose ok");
        assert!(!out.subject_members.contains(&ghost));
    }

    // --- the group agent ---

    /// A world with a sales order and a manufacturing order, each carrying a sensitive
    /// `supplier` field; the order references a purchase-order line by IRI.
    async fn group_world() -> (Arc<InMemoryWorldModel>, EntityUri, EntityUri, EntityUri) {
        let world = Arc::new(InMemoryWorldModel::default());
        let line = EntityUri("https://ekg.example/id/purchase.order.line/9".to_owned());
        let record = |model: &str, id: i64, extra: (&str, &str)| SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: model.to_owned(),
                id,
            },
            fields: BTreeMap::from([
                ("state".to_owned(), "late".to_owned()),
                ("supplier".to_owned(), "CellTech".to_owned()),
                (extra.0.to_owned(), extra.1.to_owned()),
            ]),
        };
        let order = record("sale.order", 1, ("short_line", &line.0));
        let production = record("mrp.production", 1, ("product_qty", "40"));
        world.materialize(&order).await.expect("order");
        world.materialize(&production).await.expect("production");
        (
            world,
            InMemoryWorldModel::uri_of(&order.source_ref),
            InMemoryWorldModel::uri_of(&production.source_ref),
            line,
        )
    }

    fn agent_for(world: &Arc<InMemoryWorldModel>, role: &str) -> Arc<BoundedAgent> {
        Arc::new(BoundedAgent::new(
            Arc::clone(world) as Arc<dyn WorldModel>,
            Arc::new(StubProvider("unused")),
            spec(role),
        ))
    }

    /// A group agent whose router sends `mrp.*` to `process_role` and everything else to
    /// `supply_role`, and whose parser targets each listed entity with one action.
    fn group_agent(
        world: &Arc<InMemoryWorldModel>,
        supply_role: &str,
        process_role: &str,
        targets: Vec<EntityUri>,
        max_actions: usize,
    ) -> GroupAgent {
        let supply = agent_for(world, supply_role);
        let process = agent_for(world, process_role);
        GroupAgent::new(
            Arc::new(move |member: &EntityUri| {
                if member.model().is_some_and(|m| m.starts_with("mrp.")) {
                    Arc::clone(&process)
                } else {
                    Arc::clone(&supply)
                }
            }),
            Arc::new(StubProvider("expedite + reschedule")),
            Arc::new(
                |group: &SubjectGroup, g: &Grounding, _t: &[ScoredCandidate]| {
                    format!("{} members, {} edges", group.members.len(), g.edges.len())
                },
            ),
            Arc::new(move |_text: &str, _g: &Grounding| {
                Ok(targets
                    .iter()
                    .enumerate()
                    .map(|(i, target)| ProposedAction {
                        action_type: ActionType(format!("verb_{i}")),
                        target: target.clone(),
                        params: BTreeMap::new(),
                    })
                    .collect())
            }),
            max_actions,
        )
    }

    fn two_member_group(order: &EntityUri, production: &EntityUri) -> SubjectGroup {
        SubjectGroup {
            key: "portfolio/abc".to_owned(),
            members: vec![order.clone(), production.clone()],
            links: vec!["linked: MO/1 fulfils SO/1".to_owned()],
            worded: false,
        }
    }

    #[tokio::test]
    async fn each_member_is_grounded_by_its_own_routed_agent_under_its_own_read_context() {
        let (world, order, production, _line) = group_world().await;
        // The order is read as a planner (supplier hidden), the production as procurement
        // (supplier visible).
        let agent = group_agent(&world, "planner", "procurement", vec![order.clone()], 4);
        let out = agent
            .propose_group_candidate(&two_member_group(&order, &production), &[])
            .await
            .expect("candidate");
        let supplier_of = |entity: &EntityUri| {
            out.grounding_edges
                .iter()
                .any(|e| e.subject == *entity && e.predicate == "ekg:supplier")
        };
        assert!(
            !supplier_of(&order),
            "the planner read hides the order's supplier"
        );
        assert!(supplier_of(&production), "the procurement read shows it");
        let supplier: Vec<_> = out
            .grounding_omissions
            .iter()
            .filter(|o| o.type_iri.contains("upplier"))
            .collect();
        assert_eq!(supplier.len(), 1);
        assert_eq!(supplier[0].count, 1, "only the order's supplier was hidden");
    }

    #[tokio::test]
    async fn the_merged_omissions_are_summed_per_type() {
        let (world, order, production, _line) = group_world().await;
        let agent = group_agent(&world, "planner", "planner", vec![order.clone()], 4);
        let out = agent
            .propose_group_candidate(&two_member_group(&order, &production), &[])
            .await
            .expect("candidate");
        let supplier: Vec<_> = out
            .grounding_omissions
            .iter()
            .filter(|o| o.type_iri.contains("upplier"))
            .collect();
        assert_eq!(supplier.len(), 1, "one entry per hidden type");
        assert_eq!(supplier[0].count, 2, "both members' hidden suppliers");
        assert_eq!(out.proposal.omissions, out.grounding_omissions);
    }

    #[tokio::test]
    async fn an_over_long_group_plan_is_refused() {
        let (world, order, production, _line) = group_world().await;
        let agent = group_agent(
            &world,
            "planner",
            "planner",
            vec![order.clone(), production.clone()],
            1,
        );
        let err = agent
            .propose_group_candidate(&two_member_group(&order, &production), &[])
            .await
            .expect_err("over the bound");
        assert!(
            matches!(&err, EngineError::InvalidProposal(why) if why.contains("per-plan maximum")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_action_outside_every_members_grounding_is_refused() {
        let (world, order, production, _line) = group_world().await;
        let stranger = EntityUri("https://ekg.example/id/purchase.order.line/404".to_owned());
        let agent = group_agent(&world, "planner", "planner", vec![stranger], 4);
        let err = agent
            .propose_group_candidate(&two_member_group(&order, &production), &[])
            .await
            .expect_err("outside the grounding");
        assert!(
            matches!(&err, EngineError::InvalidProposal(why) if why.contains("outside every member")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_group_subject_members_are_the_union() {
        let (world, order, production, line) = group_world().await;
        // The evidenced line (referenced only by the order) is a legal target.
        let agent = group_agent(
            &world,
            "planner",
            "planner",
            vec![line.clone(), production.clone()],
            4,
        );
        let out = agent
            .propose_group_candidate(&two_member_group(&order, &production), &[])
            .await
            .expect("candidate");
        for entity in [&order, &production, &line] {
            assert!(
                out.subject_members.contains(entity),
                "{entity} in {:?}",
                out.subject_members
            );
        }
        assert_eq!(out.proposal.actions.len(), 2);
    }
}
