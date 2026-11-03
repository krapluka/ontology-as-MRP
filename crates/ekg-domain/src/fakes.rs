//! In-memory fakes for every seam.
//!
//! They let every layer be unit-tested without a live GraphDB, Odoo, NATS or
//! Hayhooks. Gated behind the `test-support` feature so they cannot end up in a
//! production wiring path.
//!
//! The IRI scheme here is a trivial demo scheme; the real one is defined in the
//! semantic layer.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::action_point::{ExecutionReceipt, PromotedAction};
use crate::error::{EngineError, Result};
use crate::events::RdfChanged;
use crate::grounding::{EvidenceEdge, Grounding, ReadContext};
use crate::grounding::{Omission, OmissionKind};
use crate::ids::{EntityUri, NamedGraph, PolicyId, Revision};
use crate::proposal::{ActionType, GroundedProposal, Plan, Proposal, ProposedAction};
use crate::read_policy::ReadPolicy;
use crate::seams::{
    ActionExecutor, CognitionProvider, GroupCandidateProposer, GroupScorer, GroupSlateProposer,
    Projection, Proposer, RdfChangePublisher, Scorer, Simulator, SlateProposer, SourceIngestor,
    WorldModel,
};
use crate::slate::{
    Need, RankedPlan, Score, ScoredCandidate, Slate, SubjectGroup, Trace, TraceStep, TraceTag,
};
use crate::source::{SourceRecord, SourceRef};

/// The demo read-policy id the fake [`InMemoryWorldModel`] enforces.
const DEMO_POLICY: &str = "read-policy/demo-v1";

/// The demo read-policy the fake grounds under by default: a non-`procurement` reader has
/// the sensitive `unit_price` edge and `supplier` node hidden (recorded as omissions). The
/// real model builds the same [`ReadPolicy`] from config.
fn demo_read_policy() -> ReadPolicy {
    ReadPolicy::new(
        PolicyId(DEMO_POLICY.to_owned()),
        BTreeSet::from(["procurement".to_owned()]),
        BTreeMap::from([("unit_price".to_owned(), "ekg:unit_price".to_owned())]),
        BTreeMap::from([("supplier".to_owned(), "ekg:Supplier".to_owned())]),
    )
}
/// Embedding width the fake [`DeterministicCognition`] returns (the real
/// provider pins 4096; the fake stays small for fast tests).
const FAKE_EMBED_DIM: usize = 8;

/// Demo IRI for an entity, e.g. `https://ekg.example/id/sale.order/42`.
fn entity_uri(source_ref: &SourceRef) -> EntityUri {
    EntityUri(format!(
        "https://ekg.example/id/{}/{}",
        source_ref.model, source_ref.id
    ))
}

/// Demo named graph for an entity (one graph per entity).
fn named_graph(source_ref: &SourceRef) -> NamedGraph {
    NamedGraph(format!(
        "https://ekg.example/graph/{}/{}",
        source_ref.model, source_ref.id
    ))
}

/// Map a poisoned lock to a typed backend error (never panic).
fn poisoned<E>(_: E) -> EngineError {
    EngineError::Backend("in-memory lock poisoned".to_owned())
}

// --- SourceIngestor ---

/// A [`SourceIngestor`] that replays pre-loaded fixture records.
#[derive(Default)]
pub struct FixtureIngestor {
    records: Mutex<HashMap<(String, String, i64), SourceRecord>>,
}

impl FixtureIngestor {
    /// Build an ingestor seeded with `records`.
    #[must_use]
    pub fn from_records(records: impl IntoIterator<Item = SourceRecord>) -> Self {
        let map = records
            .into_iter()
            .map(|r| {
                (
                    (
                        r.source_ref.system.clone(),
                        r.source_ref.model.clone(),
                        r.source_ref.id,
                    ),
                    r,
                )
            })
            .collect();
        Self {
            records: Mutex::new(map),
        }
    }
}

#[async_trait]
impl SourceIngestor for FixtureIngestor {
    async fn pull(&self, source_ref: &SourceRef) -> Result<SourceRecord> {
        let key = (
            source_ref.system.clone(),
            source_ref.model.clone(),
            source_ref.id,
        );
        self.records
            .lock()
            .map_err(poisoned)?
            .get(&key)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(format!("{}:{}", source_ref.model, source_ref.id)))
    }

    async fn changed_since(&self, _watermark: &str) -> Result<Vec<SourceRef>> {
        Ok(self
            .records
            .lock()
            .map_err(poisoned)?
            .values()
            .map(|r| r.source_ref.clone())
            .collect())
    }
}

// --- WorldModel ---

struct Stored {
    revision: Revision,
    record: SourceRecord,
    /// Tombstone: kept (with its revision) for stale-event protection, but excluded
    /// from reads so a retracted entity can't drive grounding or queries.
    retracted: bool,
}

/// An in-memory [`WorldModel`]: it materializes, grounds (applying an injected
/// [`ReadPolicy`], the demo by default), and answers `select` with canned rows. It
/// does not decide "risk"; that is a capability rule run by the monitor.
pub struct InMemoryWorldModel {
    entities: Mutex<HashMap<EntityUri, Stored>>,
    read_policy: ReadPolicy,
}

impl Default for InMemoryWorldModel {
    fn default() -> Self {
        Self {
            entities: Mutex::new(HashMap::new()),
            read_policy: demo_read_policy(),
        }
    }
}

#[async_trait]
impl WorldModel for InMemoryWorldModel {
    async fn materialize(&self, record: &SourceRecord) -> Result<Revision> {
        let uri = entity_uri(&record.source_ref);
        let mut entities = self.entities.lock().map_err(poisoned)?;
        let revision = entities
            .get(&uri)
            .map_or(Revision(1), |s| s.revision.next());
        entities.insert(
            uri,
            Stored {
                revision,
                record: record.clone(),
                retracted: false,
            },
        );
        Ok(revision)
    }

    async fn retract(&self, entity: &EntityUri) -> Result<Revision> {
        let mut entities = self.entities.lock().map_err(poisoned)?;
        let stored = entities
            .get_mut(entity)
            .filter(|s| !s.retracted)
            .ok_or_else(|| EngineError::NotFound(entity.0.clone()))?;
        stored.revision = stored.revision.next();
        stored.retracted = true;
        Ok(stored.revision)
    }

    async fn ground(&self, entity: &EntityUri, ctx: &ReadContext) -> Result<Grounding> {
        let entities = self.entities.lock().map_err(poisoned)?;
        let stored = entities
            .get(entity)
            .filter(|s| !s.retracted)
            .ok_or_else(|| EngineError::NotFound(entity.0.clone()))?;

        // The shared `ReadPolicy` does the filtering, so the fake and the GraphDB model
        // record the same omissions.
        let mut edges = Vec::new();
        let mut omissions = self.read_policy.omissions();
        for (key, value) in &stored.record.fields {
            if omissions.visible(ctx, key) {
                edges.push(EvidenceEdge {
                    subject: entity.clone(),
                    predicate: format!("ekg:{key}"),
                    object: value.clone(),
                    source: stored.record.source_ref.clone(),
                    revision: stored.revision,
                });
            }
        }

        Ok(Grounding {
            entity: entity.clone(),
            edges,
            omissions: omissions.into_omissions(),
        })
    }

    async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
        // The fake does not parse SPARQL: it returns one row per stored entity
        // (its fields plus an `entity` binding), enough to exercise a capability rule.
        let entities = self.entities.lock().map_err(poisoned)?;
        Ok(entities
            .iter()
            .filter(|(_, stored)| !stored.retracted)
            .map(|(uri, stored)| {
                let mut row = stored.record.fields.clone();
                row.insert("entity".to_owned(), uri.0.clone());
                row
            })
            .collect())
    }
}

impl InMemoryWorldModel {
    /// Build with a custom read-policy (a focused test injects one; `default()` uses the
    /// demo policy).
    #[must_use]
    pub fn with_read_policy(read_policy: ReadPolicy) -> Self {
        Self {
            entities: Mutex::new(HashMap::new()),
            read_policy,
        }
    }

    /// The demo entity IRI for a source reference (mirrors materialization).
    #[must_use]
    pub fn uri_of(source_ref: &SourceRef) -> EntityUri {
        entity_uri(source_ref)
    }

    /// The demo named graph for a source reference.
    #[must_use]
    pub fn graph_of(source_ref: &SourceRef) -> NamedGraph {
        named_graph(source_ref)
    }
}

// --- RdfChangePublisher ---

/// A [`RdfChangePublisher`] that captures published events for assertions.
#[derive(Default)]
pub struct CapturingPublisher {
    events: Mutex<Vec<RdfChanged>>,
}

#[async_trait]
impl RdfChangePublisher for CapturingPublisher {
    async fn publish(&self, event: &RdfChanged) -> Result<()> {
        self.events.lock().map_err(poisoned)?.push(event.clone());
        Ok(())
    }
}

impl CapturingPublisher {
    /// A snapshot of everything published so far.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the internal lock is poisoned.
    pub fn published(&self) -> Result<Vec<RdfChanged>> {
        Ok(self.events.lock().map_err(poisoned)?.clone())
    }
}

// --- CognitionProvider ---

/// A deterministic [`CognitionProvider`]: no network, stable output. `generate`
/// echoes its prompt so tests can assert on what the agent asked for.
#[derive(Default)]
pub struct DeterministicCognition;

#[async_trait]
impl CognitionProvider for DeterministicCognition {
    async fn generate(&self, prompt: &str) -> Result<String> {
        Ok(format!("[fake-llm] {prompt}"))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v: Vec<f32> = text.bytes().take(FAKE_EMBED_DIM).map(f32::from).collect();
        v.resize(FAKE_EMBED_DIM, 0.0);
        Ok(v)
    }
}

// --- ActionExecutor ---

/// An [`ActionExecutor`] that records executions/reversals instead of writing
/// to a system of record.
#[derive(Default)]
pub struct RecordingActionExecutor {
    executed: Mutex<Vec<ProposedAction>>,
    reversed: Mutex<Vec<ProposedAction>>,
}

#[async_trait]
impl ActionExecutor for RecordingActionExecutor {
    async fn execute(&self, promoted: &PromotedAction) -> Result<ExecutionReceipt> {
        let action = &promoted.action;
        self.executed.lock().map_err(poisoned)?.push(action.clone());
        Ok(ExecutionReceipt {
            action: action.clone(),
            external_ref: format!("fake://odoo/{}", action.target),
            // The write is recorded against the revision the promotion was based
            // on, so a conditional reverse can detect a later source change.
            source_revision: promoted.basis_revision,
            // Reversibility is a property the kinetic layer sets per verb; the
            // fake marks its writes reversible so the reverse path is testable.
            reversible: true,
        })
    }

    async fn reverse(&self, receipt: &ExecutionReceipt) -> Result<()> {
        if !receipt.reversible {
            return Err(EngineError::NotReversible(receipt.external_ref.clone()));
        }
        self.reversed
            .lock()
            .map_err(poisoned)?
            .push(receipt.action.clone());
        Ok(())
    }
}

impl RecordingActionExecutor {
    /// Actions committed so far.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the internal lock is poisoned.
    pub fn executed(&self) -> Result<Vec<ProposedAction>> {
        Ok(self.executed.lock().map_err(poisoned)?.clone())
    }

    /// Actions reversed so far.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the internal lock is poisoned.
    pub fn reversed(&self) -> Result<Vec<ProposedAction>> {
        Ok(self.reversed.lock().map_err(poisoned)?.clone())
    }
}

// --- consumer-side dedup ---

/// A minimal event consumer that applies `RdfChanged` and rejects stale
/// (out-of-order) revisions per entity, so at-least-once delivery never applies
/// an older materialization after a newer one. Not a seam; a fake to exercise the
/// invariant.
#[derive(Default)]
pub struct ProjectionConsumer {
    applied: Mutex<HashMap<EntityUri, Revision>>,
}

impl ProjectionConsumer {
    /// Apply an event, updating the per-entity high-water mark.
    ///
    /// # Errors
    /// [`EngineError::StaleRevision`] if the event's revision is not greater than
    /// the last applied for its entity; [`EngineError::Backend`] on a poisoned lock.
    pub fn apply(&self, event: &RdfChanged) -> Result<()> {
        let mut applied = self.applied.lock().map_err(poisoned)?;
        if let Some(last) = applied.get(&event.entity_uri)
            && event.revision <= *last
        {
            return Err(EngineError::StaleRevision {
                entity: event.entity_uri.0.clone(),
                incoming: event.revision.0,
                applied: last.0,
            });
        }
        applied.insert(event.entity_uri.clone(), event.revision);
        Ok(())
    }
}

// --- Proposer ---

/// A [`Proposer`] that returns a fixed, well-formed proposal for any subject:
/// enough to exercise the propose stage (and the attach from `open` to `proposed`)
/// without a live model.
#[derive(Default)]
pub struct CannedProposer;

#[async_trait]
impl Proposer for CannedProposer {
    async fn propose(&self, subject: &EntityUri) -> Result<GroundedProposal> {
        let proposal = Proposal::single(
            format!("[fake] {subject} is off plan; a governed action is proposed"),
            ProposedAction {
                action_type: ActionType("notify_owner".to_owned()),
                target: subject.clone(),
                params: BTreeMap::new(),
            },
            Vec::new(),
        );
        Ok(GroundedProposal {
            proposal,
            grounding_omissions: Vec::new(),
            // The fake grounds on nothing concrete, so it carries no evidence edges.
            grounding_edges: Vec::new(),
            // The fake targets the subject itself, so it needs no members.
            subject_members: Vec::new(),
        })
    }
}

// --- Simulator ---

/// An in-memory [`Simulator`] over a canned base of entity fields. `project` clones
/// the base, overlays each plan action's params onto its target entity's fields, and
/// answers `select` from the projected copy. Enough to exercise a capability's own rule
/// against a what-if without a live triple store. The seeded base is never mutated.
#[derive(Default, Clone)]
pub struct InMemorySimulator {
    base: HashMap<EntityUri, BTreeMap<String, String>>,
}

impl InMemorySimulator {
    /// Seed one entity of base state (its fields, keyed by the entity's URI). The
    /// entity is echoed back as an `entity` binding on every projected `select` row.
    #[must_use]
    pub fn with_entity(mut self, entity: EntityUri, fields: BTreeMap<String, String>) -> Self {
        self.base.insert(entity, fields);
        self
    }
}

#[async_trait]
impl Simulator for InMemorySimulator {
    async fn project(&self, plan: &Plan) -> Result<Box<dyn Projection>> {
        // Clone the base: a projection never mutates the seeded state, which stands in
        // for the authoritative one.
        let mut projected = self.base.clone();
        for action in &plan.actions {
            let fields = projected.entry(action.target.clone()).or_default();
            for (key, value) in &action.params {
                fields.insert(key.clone(), value.clone());
            }
        }
        Ok(Box::new(InMemoryProjection { projected }))
    }
}

/// The queryable handle [`InMemorySimulator`] hands back. Like
/// [`InMemoryWorldModel`]'s `select`, it returns one row per entity (its fields plus an
/// `entity` binding) without parsing SPARQL.
struct InMemoryProjection {
    projected: HashMap<EntityUri, BTreeMap<String, String>>,
}

#[async_trait]
impl Projection for InMemoryProjection {
    async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
        Ok(self
            .projected
            .iter()
            .map(|(uri, fields)| {
                let mut row = fields.clone();
                row.insert("entity".to_owned(), uri.0.clone());
                row
            })
            .collect())
    }
}

// --- Scorer ---

/// A [`Scorer`] that fills a [`Score`] from projected rows: it counts the rows whose
/// `field` still holds `bad_value` (a stand-in "signal still present" marker) and
/// ranks fewer-flagged first. The metric keys (`flagged`, `rows`) are demo labels;
/// the real supply-risk and process scorers live in `ekg-semantic`.
pub struct FakeScorer {
    field: String,
    bad_value: String,
}

impl FakeScorer {
    /// A scorer whose (fake) rule flags a projected row when `field` equals
    /// `bad_value`. A projection where fewer rows are flagged scores better.
    #[must_use]
    pub fn flagging(field: impl Into<String>, bad_value: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            bad_value: bad_value.into(),
        }
    }
}

#[async_trait]
impl Scorer for FakeScorer {
    async fn score(&self, projection: &dyn Projection) -> Result<Score> {
        // Like the other fakes, this ignores the SPARQL text.
        let rows = projection.select("<fake rule>").await?;
        let flagged = rows
            .iter()
            .filter(|row| row.get(&self.field).map(String::as_str) == Some(self.bad_value.as_str()))
            .count();
        let metrics = BTreeMap::from([
            ("flagged".to_owned(), flagged.to_string()),
            ("rows".to_owned(), rows.len().to_string()),
        ]);
        // Zero-padded, so fewer flagged rows sort first (best first).
        Ok(Score {
            metrics,
            rank_key: format!("{flagged:06}"),
        })
    }
}

// --- SlateProposer ---

/// A [`SlateProposer`] that returns a fixed, well-formed ranked slate for any
/// subject: a do-nothing baseline plus two ordered candidate plans, the first
/// flagged `recommended`, each carrying a demo omission so the disclosure invariant
/// is exercised. It holds no world model or executor, so producing a slate mutates
/// nothing.
#[derive(Default)]
pub struct CannedSlateProposer;

#[async_trait]
impl SlateProposer for CannedSlateProposer {
    async fn propose_slate(&self, subject: &EntityUri) -> Result<Slate> {
        // A demo omission every candidate discloses (as a single proposal must).
        let omission = Omission {
            kind: OmissionKind::Node,
            type_iri: "ekg:Supplier".to_owned(),
            count: 1,
            policy_id: PolicyId(DEMO_POLICY.to_owned()),
        };
        let candidate = |verb: &str, rank: u32, recommended: bool| RankedPlan {
            plan: Plan {
                actions: vec![ProposedAction {
                    action_type: ActionType(verb.to_owned()),
                    target: subject.clone(),
                    params: BTreeMap::new(),
                }],
            },
            score: Score {
                metrics: BTreeMap::from([("flagged".to_owned(), rank.to_string())]),
                rank_key: format!("{rank:06}"),
            },
            recommended,
            needs: if recommended {
                Vec::new()
            } else {
                vec![Need {
                    code: "manual_review".to_owned(),
                    detail: "second option needs a human before it can apply".to_owned(),
                }]
            },
            omissions: vec![omission.clone()],
            // A demo per-candidate trace: one simulated step and one assumption.
            trace: Trace::from_steps(vec![
                TraceStep {
                    tag: TraceTag::Simulated,
                    text: format!("[fake] {verb} scored rank {rank}"),
                    rejected: false,
                },
                TraceStep {
                    tag: TraceTag::Assumption,
                    text: "[fake] a supplier node was hidden by read-policy".to_owned(),
                    rejected: false,
                },
            ]),
        };
        let options = vec![
            candidate("expedite_po", 0, true),
            candidate("notify_owner", 1, false),
        ];
        // A demo slate-level trace: the do-nothing baseline plus each option's steps.
        let mut steps = vec![TraceStep {
            tag: TraceTag::Simulated,
            text: "[fake] do-nothing baseline scored rank 9".to_owned(),
            rejected: false,
        }];
        for option in &options {
            steps.extend(option.trace.steps.iter().cloned());
        }
        Ok(Slate {
            // The do-nothing baseline scores worst (the signal is still present).
            baseline: Score {
                metrics: BTreeMap::from([("flagged".to_owned(), "9".to_owned())]),
                rank_key: format!("{:06}", 9),
            },
            options,
            trace: Trace::from_steps(steps),
        })
    }
}

// --- Group seams ---

/// A [`GroupCandidateProposer`] that returns one fixed plan for any group: a
/// `notify_owner` on every member, in member order. Its `subject_members` is the
/// group's members, so every action targets an entity the group grounds on. It holds
/// no world model or executor, so it commits nothing.
#[derive(Default)]
pub struct CannedGroupProposer;

#[async_trait]
impl GroupCandidateProposer for CannedGroupProposer {
    async fn propose_group_candidate(
        &self,
        group: &SubjectGroup,
        _feedback: &[ScoredCandidate],
    ) -> Result<GroundedProposal> {
        let actions = group
            .members
            .iter()
            .map(|member| ProposedAction {
                action_type: ActionType("notify_owner".to_owned()),
                target: member.clone(),
                params: BTreeMap::new(),
            })
            .collect();
        Ok(GroundedProposal {
            proposal: Proposal {
                explanation: format!("[fake] one plan for group {}", group.key),
                actions,
                omissions: Vec::new(),
            },
            grounding_omissions: Vec::new(),
            grounding_edges: Vec::new(),
            subject_members: group.members.clone(),
        })
    }
}

/// A [`GroupScorer`] that counts the group's members whose projected `field` still
/// holds `bad_value` and ranks fewer-flagged first. Rows about entities outside the
/// group are ignored. As with [`FakeScorer`], `members_flagged` is a demo label.
pub struct FakeGroupScorer {
    field: String,
    bad_value: String,
}

impl FakeGroupScorer {
    /// A group scorer whose (fake) rule flags a member when its projected `field`
    /// equals `bad_value`.
    #[must_use]
    pub fn flagging(field: impl Into<String>, bad_value: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            bad_value: bad_value.into(),
        }
    }
}

#[async_trait]
impl GroupScorer for FakeGroupScorer {
    async fn score_group(
        &self,
        projection: &dyn Projection,
        group: &SubjectGroup,
    ) -> Result<Score> {
        let rows = projection.select("<fake rule>").await?;
        let flagged = rows
            .iter()
            .filter(|row| {
                row.get("entity")
                    .is_some_and(|entity| group.members.iter().any(|m| m.0 == *entity))
                    && row.get(&self.field).map(String::as_str) == Some(self.bad_value.as_str())
            })
            .count();
        Ok(Score {
            metrics: BTreeMap::from([("members_flagged".to_owned(), flagged.to_string())]),
            rank_key: format!("{flagged:06}"),
        })
    }
}

/// A [`GroupSlateProposer`] that returns a fixed, well-formed slate for any group: a
/// do-nothing baseline and one recommended option (a `notify_owner` on every member),
/// with a trace that opens with a `Graph` "Linked" step naming the members and links.
/// It commits nothing.
#[derive(Default)]
pub struct CannedGroupSlateProposer;

#[async_trait]
impl GroupSlateProposer for CannedGroupSlateProposer {
    async fn propose_group_slate(&self, group: &SubjectGroup) -> Result<Slate> {
        let members = group
            .members
            .iter()
            .map(|m| m.0.as_str())
            .collect::<Vec<_>>()
            .join(" and ");
        let linked = TraceStep {
            tag: TraceTag::Graph,
            text: format!("Linked: {members}. {}", group.links.join("; ")),
            rejected: false,
        };
        let simulated = TraceStep {
            tag: TraceTag::Simulated,
            text: "[fake] one plan across the group scored rank 0".to_owned(),
            rejected: false,
        };
        let option = RankedPlan {
            plan: Plan {
                actions: group
                    .members
                    .iter()
                    .map(|member| ProposedAction {
                        action_type: ActionType("notify_owner".to_owned()),
                        target: member.clone(),
                        params: BTreeMap::new(),
                    })
                    .collect(),
            },
            score: Score {
                metrics: BTreeMap::from([("members_flagged".to_owned(), "0".to_owned())]),
                rank_key: format!("{:06}", 0),
            },
            recommended: true,
            needs: Vec::new(),
            omissions: Vec::new(),
            trace: Trace::from_steps(vec![simulated.clone()]),
        };
        let baseline = TraceStep {
            tag: TraceTag::Simulated,
            text: "[fake] do-nothing baseline scored rank 9".to_owned(),
            rejected: false,
        };
        Ok(Slate {
            baseline: Score {
                metrics: BTreeMap::from([(
                    "members_flagged".to_owned(),
                    group.members.len().to_string(),
                )]),
                rank_key: format!("{:06}", 9),
            },
            options: vec![option],
            trace: Trace::from_steps(vec![linked, baseline, simulated]),
        })
    }
}
