//! Portfolios of coupled action points: the pure grouping of live at-risk subjects into
//! connected components of their coupling edges, the deterministic portfolio identity, and
//! the durable record the portfolio object holds.
//!
//! Free of Restate, so every rule (transitivity, determinism, idempotency, generations
//! and the audit trail) can be unit-tested; [`crate::object`] wraps it durably. Subjects
//! and edges are opaque here; the injected [`crate::rule::CouplingRule`]s carry the use
//! case. Grouping commits nothing.

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{
    CouplingLink, EngineError, EntityUri, ExecutionPlan, PortfolioId, ProposedAction, Result,
    ReviewerId, Slate, short_digest,
};
use serde::{Deserialize, Serialize};

use crate::lifecycle::ReverseProgress;
use crate::rule::CouplingEdge;

/// The deterministic identity of the portfolio `members` form: `portfolio/{digest}`, the
/// digest over the sorted, de-duplicated member IRIs joined by `\n`. Independent of input
/// order; the same members always give the same id, and different members a different one.
#[must_use]
pub fn portfolio_id(members: &[EntityUri]) -> PortfolioId {
    let sorted: BTreeSet<&str> = members.iter().map(|m| m.0.as_str()).collect();
    let canonical = sorted.into_iter().collect::<Vec<_>>().join("\n");
    PortfolioId(format!("portfolio/{}", short_digest(canonical.as_bytes())))
}

/// The connected components of `live` under `edges`: an edge counts only when both of
/// its ends are live. Every live subject is in exactly one component (a subject with no live
/// peer is a component of one). Each component is sorted, and so is the list, so the output
/// is independent of input order.
#[must_use]
pub fn components(live: &[EntityUri], edges: &[CouplingEdge]) -> Vec<Vec<EntityUri>> {
    let nodes: BTreeSet<&EntityUri> = live.iter().collect();
    let index: BTreeMap<&EntityUri, usize> =
        nodes.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    let mut parent: Vec<usize> = (0..index.len()).collect();
    for edge in edges {
        if let (Some(&a), Some(&b)) = (index.get(&edge.a), index.get(&edge.b)) {
            let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
            if ra != rb {
                // Join under the lesser root. The output is sorted either way.
                let (keep, fold) = if ra < rb { (ra, rb) } else { (rb, ra) };
                parent[fold] = keep;
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<EntityUri>> = BTreeMap::new();
    for (node, i) in &index {
        let r = root(&mut parent, *i);
        groups.entry(r).or_default().push((*node).clone());
    }
    let mut out: Vec<Vec<EntityUri>> = groups.into_values().collect();
    for group in &mut out {
        group.sort();
    }
    out.sort();
    out
}

/// The union-find root of `i`, compressing the path on the way.
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// One portfolio a pass forms: its id, its sorted members, and the coupling edges inside it
/// (sorted), whose reasons say why the members are linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPortfolio {
    /// The deterministic id ([`portfolio_id`]).
    pub id: PortfolioId,
    /// The members, sorted.
    pub members: Vec<EntityUri>,
    /// The live edges between members, sorted and de-duplicated.
    pub links: Vec<CouplingEdge>,
}

impl PlannedPortfolio {
    /// The links it records when it forms: every edge, with its facts.
    #[must_use]
    pub fn coupling(&self) -> Vec<CouplingLink> {
        self.links.iter().map(CouplingEdge::link).collect()
    }

    /// Why the members are linked: each distinct edge reason, in edge order, joined by `; `.
    #[must_use]
    pub fn reason(&self) -> String {
        let mut seen = BTreeSet::new();
        self.links
            .iter()
            .filter(|l| seen.insert(l.reason.as_str()))
            .map(|l| l.reason.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// What one monitor pass does with the live flagged subjects: the portfolios to form (every
/// component of two or more) and the subjects left alone (in no portfolio).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupingPlan {
    /// The portfolios to form, sorted by their members.
    pub portfolios: Vec<PlannedPortfolio>,
    /// The live subjects with no live coupled peer, sorted.
    pub singles: Vec<EntityUri>,
}

/// The grouping plan for `live` subjects under `edges` (see [`components`]).
#[must_use]
pub fn grouping_plan(live: &[EntityUri], edges: &[CouplingEdge]) -> GroupingPlan {
    let mut plan = GroupingPlan::default();
    for component in components(live, edges) {
        if component.len() < 2 {
            plan.singles.extend(component);
            continue;
        }
        let members: BTreeSet<&EntityUri> = component.iter().collect();
        let links: BTreeSet<CouplingEdge> = edges
            .iter()
            .filter(|e| members.contains(&e.a) && members.contains(&e.b))
            .cloned()
            .collect();
        plan.portfolios.push(PlannedPortfolio {
            id: portfolio_id(&component),
            members: component,
            links: links.into_iter().collect(),
        });
    }
    plan
}

/// Whether a portfolio is current.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortfolioState {
    /// Its members are coupled and awaiting oversight.
    Active,
    /// A member left or was regrouped; the portfolio is never reused as it was. The same
    /// members forming again start a new generation.
    #[default]
    Superseded,
    /// Its own joint plan was promoted as one governed step across every member. It is
    /// not superseded by its members leaving on that promotion; the same members forming
    /// again later start a new generation.
    Promoted,
    /// Its promoted plan was undone as one governed step.
    Reversed,
    /// An operator dismissed its plan as one governed step, aborting every member. The
    /// dismissed generation is terminal; fresh points of the same members coupling again
    /// later start a new generation.
    Dismissed,
}

/// What a [`PortfolioAuditEntry`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortfolioEvent {
    /// The members formed (or re-formed) the portfolio, starting a generation.
    Formed,
    /// The portfolio stopped being current.
    Superseded,
    /// A joint slate was attached for the current generation.
    SlateAttached,
    /// A joint-slate option was promoted across every member.
    Promoted,
    /// An undo reversed only some of the promoted plan's actions; the portfolio stays
    /// promoted and the progress is recorded so a retry resumes from it.
    PartiallyReversed,
    /// The promoted plan was undone.
    Reversed,
    /// An operator dismissed the plan; every member is aborted.
    Dismissed,
}

/// One append-only audit entry of a portfolio (EU AI Act Art. 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioAuditEntry {
    /// What happened.
    pub event: PortfolioEvent,
    /// The generation it happened in.
    pub generation: u32,
    /// Why (the coupling reasons for a formation; who left and how for a supersession).
    pub reason: String,
    /// The members at that moment.
    pub members: Vec<EntityUri>,
    /// Who caused it, for a human command (promotion, undo). `None` for the monitor's
    /// formation and supersession. Defaulted and skipped when absent so older entries load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ReviewerId>,
    /// For a formation, each coupling link with its facts. `reason` still carries the
    /// reasons verbatim. Defaulted and skipped when empty, so entries without links load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<CouplingLink>,
}

/// A portfolio's promotion of one joint-slate option: the portfolio record owns it, and
/// it is the source of truth for the portfolio undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioPromotion {
    /// The generation the promotion was decided for.
    pub generation: u32,
    /// The chosen option's `rank_key` (opaque).
    pub rank_key: String,
    /// The human who promoted it.
    pub reviewer: ReviewerId,
    /// Their rationale.
    pub rationale: String,
    /// The executed plan: every receipt of the one all-or-nothing write-back.
    pub execution_plan: ExecutionPlan,
    /// When it was promoted, as Unix epoch seconds (stringly, like the point's
    /// `promoted_at`); anchors the reversal window.
    pub promoted_at: String,
    /// How far an undo has got, after a partial reverse. Defaulted and skipped when empty.
    #[serde(default, skip_serializing_if = "ReverseProgress::is_empty")]
    pub progress: ReverseProgress,
}

/// What was decided on a portfolio, or that its promotion was undone, as reported to whoever
/// listens for outcomes. It repeats what the record holds at that moment and adds the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioOutcome {
    /// The portfolio the outcome is of.
    pub portfolio: PortfolioId,
    /// The generation it was decided for.
    pub generation: u32,
    /// What happened: `promoted`, `dismissed`, `superseded`, `partially_reversed` or
    /// `reversed`.
    pub event: PortfolioEvent,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
    /// Who caused it (`None` for a supersession, which the monitor causes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ReviewerId>,
    /// The executed actions of a promotion, in order; empty for every other event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ProposedAction>,
    /// The promoted option's `rank_key` (opaque), for a promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_key: Option<String>,
    /// When the joint slate the portfolio carried for that generation was attached, if it
    /// carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate_attached_at: Option<String>,
}

/// The outcome of [`PortfolioRecord::form`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormOutcome {
    /// The portfolio was formed for the first time (generation 1).
    Formed,
    /// It was already active with these members; nothing changed.
    Unchanged,
    /// It was superseded (or held other members) and is active again under a new generation.
    Reactivated,
}

/// What `PortfolioService.form` did, and the generation the portfolio is at afterwards:
/// the one a joint slate for this formation must be attached for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormReceipt {
    /// Whether the call formed, reactivated or left the portfolio unchanged.
    pub outcome: FormOutcome,
    /// The portfolio's generation after the call.
    pub generation: u32,
}

/// The durable state of one portfolio object, keyed by its [`PortfolioId`]. A record that
/// was never formed is the default (generation 0, no members).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioRecord {
    /// The portfolio's id; `None` until it first forms.
    pub id: Option<PortfolioId>,
    /// The member subjects, sorted.
    pub members: Vec<EntityUri>,
    /// Whether it is current.
    pub state: PortfolioState,
    /// The current generation: 1 on first formation, +1 on every re-formation.
    pub generation: u32,
    /// The append-only audit trail.
    pub audit: Vec<PortfolioAuditEntry>,
    /// The joint slate attached for [`Self::slate_generation`], if any. It is the
    /// portfolio's current slate only while the portfolio is active at that
    /// generation; a re-formation clears it, and a supersession keeps it as history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate: Option<Slate>,
    /// The generation the stored slate was produced for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate_generation: Option<u32>,
    /// The promotion of this portfolio's own plan, once promoted. Kept as history if the
    /// members later form again. Defaulted and skipped when absent so older records load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion: Option<PortfolioPromotion>,
    /// When the current slate was attached, as Unix epoch seconds (stringly, like
    /// `promoted_at`); overwritten by every attach and cleared with the slate on a
    /// re-formation. Display-only: it drives no transition. Defaulted and skipped when
    /// absent so older records load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slate_attached_at: Option<String>,
}

impl PortfolioRecord {
    /// The outcome to report after a transition, stamped `at`: `Some` when the last thing
    /// that happened to the portfolio is a promotion, a dismissal, a supersession or an undo
    /// (in whole or in part), with the reviewer that audit entry names.
    #[must_use]
    pub fn outcome(&self, at: u64) -> Option<PortfolioOutcome> {
        let entry = self.audit.last()?;
        let promoted = match entry.event {
            PortfolioEvent::Promoted => true,
            PortfolioEvent::Dismissed
            | PortfolioEvent::Superseded
            | PortfolioEvent::PartiallyReversed
            | PortfolioEvent::Reversed => false,
            PortfolioEvent::Formed | PortfolioEvent::SlateAttached => return None,
        };
        let promotion = self.promotion.as_ref().filter(|_| promoted);
        Some(PortfolioOutcome {
            portfolio: self.id.clone()?,
            generation: entry.generation,
            event: entry.event,
            at,
            reviewer: entry.reviewer.clone(),
            actions: promotion
                .map(|promotion| promotion.execution_plan.actions.clone())
                .unwrap_or_default(),
            rank_key: promotion.map(|promotion| promotion.rank_key.clone()),
            slate_attached_at: self
                .slate_attached_at
                .clone()
                .filter(|_| self.slate_generation == Some(entry.generation)),
        })
    }

    /// Form the portfolio `id` of `members` for `reason`, recording its coupling `links`
    /// with their facts. The first formation is generation 1; forming an active portfolio
    /// with the same members is a no-op; forming a superseded one (or one holding other
    /// members, i.e. a digest collision) starts a new generation. Every change is audited.
    pub fn form(
        &mut self,
        id: PortfolioId,
        members: Vec<EntityUri>,
        reason: String,
        links: Vec<CouplingLink>,
    ) -> FormOutcome {
        let mut members = members;
        members.sort();
        members.dedup();
        if self.state == PortfolioState::Active && self.members == members {
            return FormOutcome::Unchanged;
        }
        let outcome = if self.generation == 0 {
            FormOutcome::Formed
        } else {
            FormOutcome::Reactivated
        };
        self.id = Some(id);
        self.generation = self.generation.saturating_add(1);
        // A new generation starts without a slate: an earlier generation's slate is never
        // the current one.
        self.slate = None;
        self.slate_generation = None;
        self.slate_attached_at = None;
        self.members = members;
        self.state = PortfolioState::Active;
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::Formed,
            generation: self.generation,
            reason,
            members: self.members.clone(),
            reviewer: None,
            links,
        });
        outcome
    }

    /// Supersede an active portfolio for `reason`, auditing the transition. Idempotent: a
    /// portfolio that is already superseded (or was never formed) is unchanged. Returns
    /// whether it changed.
    pub fn supersede(&mut self, reason: String) -> bool {
        if self.state != PortfolioState::Active {
            return false;
        }
        self.state = PortfolioState::Superseded;
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::Superseded,
            generation: self.generation,
            reason,
            members: self.members.clone(),
            reviewer: None,
            links: Vec::new(),
        });
        true
    }

    /// Attach the joint `slate` produced for `generation` at `attached_at` (Unix epoch
    /// seconds, the handler's journaled clock read), auditing it. Commits nothing: it
    /// changes no membership, state or generation.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] (the handler's 409) when the portfolio is not active, or
    /// when `generation` is not its current generation. A stale slate never lands, and
    /// the audit trail is unchanged.
    pub fn attach_slate(
        &mut self,
        generation: u32,
        slate: Slate,
        attached_at: String,
    ) -> Result<()> {
        if self.state != PortfolioState::Active {
            return Err(EngineError::Invariant(format!(
                "a joint slate attaches only to an active portfolio (generation {generation} \
                 arrived for a superseded one)"
            )));
        }
        if generation != self.generation {
            return Err(EngineError::Invariant(format!(
                "a joint slate for generation {generation} is stale: the portfolio is at \
                 generation {}",
                self.generation
            )));
        }
        let trace = &slate.trace.summary;
        let reason = format!(
            "joint slate attached: {} option(s); trace across all candidates: {} simulated, \
             {} rejected, {} open assumption(s)",
            slate.options.len(),
            trace.simulated,
            trace.rejected,
            trace.open_assumptions
        );
        self.slate = Some(slate);
        self.slate_generation = Some(generation);
        self.slate_attached_at = Some(attached_at);
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::SlateAttached,
            generation,
            reason,
            members: self.members.clone(),
            reviewer: None,
            links: Vec::new(),
        });
        Ok(())
    }

    /// Record the promotion of one option of this portfolio's joint slate as one governed
    /// step across every member: store it, audit it with the reviewer, and move from
    /// `active` to `promoted`. This is the final gate of a portfolio promotion: it refuses
    /// unless the portfolio is active at the promotion's generation with that option on the
    /// slate for it.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] (the handler's 409) when the portfolio is not active, the
    /// generation is not its current one, it has no slate for that generation, or the
    /// option is not on it. Nothing is recorded.
    pub fn promote(&mut self, promotion: PortfolioPromotion) -> Result<()> {
        let generation = promotion.generation;
        if self.state != PortfolioState::Active {
            return Err(EngineError::Invariant(format!(
                "portfolio is {:?}; only an active portfolio's plan can be promoted",
                self.state
            )));
        }
        if generation != self.generation {
            return Err(EngineError::Invariant(format!(
                "the promotion was decided for generation {generation}, but the portfolio is \
                 at generation {}",
                self.generation
            )));
        }
        let option = self
            .slate
            .as_ref()
            .filter(|_| self.slate_generation == Some(generation))
            .ok_or_else(|| {
                EngineError::Invariant(format!(
                    "the portfolio has no joint slate for generation {generation}"
                ))
            })?
            .options
            .iter()
            .find(|option| option.score.rank_key == promotion.rank_key)
            .ok_or_else(|| {
                EngineError::Invariant(format!(
                    "option {:?} is not on the joint slate for generation {generation}",
                    promotion.rank_key
                ))
            })?;
        if !promotion.progress.is_empty() {
            return Err(EngineError::Invariant(
                "a new promotion carries no reverse progress".to_owned(),
            ));
        }
        let reason = format!(
            "promoted option {} ({} action(s)) across {} member(s): {}",
            promotion.rank_key,
            option.plan.actions.len(),
            self.members.len(),
            promotion.rationale
        );
        self.state = PortfolioState::Promoted;
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::Promoted,
            generation,
            reason,
            members: self.members.clone(),
            reviewer: Some(promotion.reviewer.clone()),
            links: Vec::new(),
        });
        self.promotion = Some(promotion);
        Ok(())
    }

    /// Record an undo by `reviewer` that reversed only some of the promoted plan's
    /// actions: store `progress` so a retry reverses only the rest and audit it
    /// (`partially_reversed`, with the counts and refusals). The portfolio stays `promoted`.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] when the portfolio is not promoted, or `progress` is not a
    /// valid successor of the recorded one ([`ReverseProgress::check`]).
    pub fn record_partial_reverse(
        &mut self,
        reviewer: ReviewerId,
        progress: ReverseProgress,
    ) -> Result<()> {
        let promotion = self.promoted_mut("record a partial reverse")?;
        let total = promotion.execution_plan.receipts.len();
        progress.check(&promotion.progress, total)?;
        let reason = progress.summary(total);
        let generation = promotion.generation;
        promotion.progress = progress;
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::PartiallyReversed,
            generation,
            reason,
            members: self.members.clone(),
            reviewer: Some(reviewer),
            links: Vec::new(),
        });
        Ok(())
    }

    /// Record the undo of the promoted plan by `reviewer` for `reason`: every receipt is
    /// marked reversed, and the portfolio moves from `promoted` to `reversed`, audited.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] when the portfolio is not promoted.
    pub fn reverse(&mut self, reviewer: ReviewerId, reason: String) -> Result<()> {
        let promotion = self.promoted_mut("reverse")?;
        let total = promotion.execution_plan.receipts.len();
        let generation = promotion.generation;
        promotion.progress = ReverseProgress {
            reversed: (0..total).collect(),
            refused: Vec::new(),
        };
        self.state = PortfolioState::Reversed;
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::Reversed,
            generation,
            reason,
            members: self.members.clone(),
            reviewer: Some(reviewer),
            links: Vec::new(),
        });
        Ok(())
    }

    /// Record the dismissal of this portfolio's plan by `reviewer` for `reason` as one
    /// governed step: move from `active` to `dismissed` and audit it with the reviewer, the
    /// reason, the generation and the members. The caller has already claimed every member
    /// and aborts them after this record. Commits nothing to any system of record.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] (the handler's 409) when the portfolio is not active
    /// (dismissed already, superseded, promoted or reversed) or `generation` is not its
    /// current one. Nothing is recorded.
    pub fn dismiss(&mut self, generation: u32, reviewer: ReviewerId, reason: String) -> Result<()> {
        if self.state != PortfolioState::Active {
            return Err(EngineError::Invariant(format!(
                "portfolio is {:?}; only an active portfolio's plan can be dismissed",
                self.state
            )));
        }
        if generation != self.generation {
            return Err(EngineError::Invariant(format!(
                "the dismissal was decided for generation {generation}, but the portfolio is \
                 at generation {}",
                self.generation
            )));
        }
        self.state = PortfolioState::Dismissed;
        self.audit.push(PortfolioAuditEntry {
            event: PortfolioEvent::Dismissed,
            generation,
            reason,
            members: self.members.clone(),
            reviewer: Some(reviewer),
            links: Vec::new(),
        });
        Ok(())
    }

    /// The promotion of a `promoted` portfolio, for `what` to act on.
    fn promoted_mut(&mut self, what: &str) -> Result<&mut PortfolioPromotion> {
        if self.state != PortfolioState::Promoted {
            return Err(EngineError::Invariant(format!(
                "portfolio is {:?}; only a promoted portfolio can {what}",
                self.state
            )));
        }
        self.promotion.as_mut().ok_or_else(|| {
            EngineError::Invariant("a promoted portfolio without its promotion".to_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn s(n: &str) -> EntityUri {
        EntityUri(format!("https://ekg.example/id/{n}"))
    }

    fn edge(x: &str, y: &str) -> CouplingEdge {
        CouplingEdge::new(s(x), s(y), format!("{x}~{y}")).expect("distinct ends")
    }

    #[test]
    fn a_coupled_pair_forms_one_portfolio() {
        let plan = grouping_plan(
            &[s("sale.order/1"), s("mrp.production/1")],
            &[edge("sale.order/1", "mrp.production/1")],
        );
        assert_eq!(plan.portfolios.len(), 1);
        assert_eq!(
            plan.portfolios[0].members,
            [s("mrp.production/1"), s("sale.order/1")]
        );
        assert_eq!(plan.portfolios[0].reason(), "sale.order/1~mrp.production/1");
        assert!(plan.singles.is_empty());
    }

    #[test]
    fn coupling_is_transitive() {
        let plan = grouping_plan(
            &[s("a/1"), s("b/1"), s("c/1")],
            &[edge("a/1", "b/1"), edge("b/1", "c/1")],
        );
        assert_eq!(plan.portfolios.len(), 1, "{plan:?}");
        assert_eq!(plan.portfolios[0].members, [s("a/1"), s("b/1"), s("c/1")]);
        assert_eq!(plan.portfolios[0].links.len(), 2);
    }

    #[test]
    fn an_edge_with_a_non_live_end_is_ignored() {
        let plan = grouping_plan(
            &[s("a/1"), s("b/1")],
            &[edge("a/1", "x/9"), edge("b/1", "x/9")],
        );
        assert!(
            plan.portfolios.is_empty(),
            "the resolved peer does not bridge them"
        );
        assert_eq!(plan.singles, [s("a/1"), s("b/1")]);
    }

    #[test]
    fn a_lone_subject_is_single() {
        let plan = grouping_plan(&[s("a/1"), s("b/1"), s("c/1")], &[edge("a/1", "b/1")]);
        assert_eq!(plan.portfolios.len(), 1);
        assert_eq!(plan.singles, [s("c/1")]);
    }

    #[test]
    fn the_plan_and_the_id_are_independent_of_input_order() {
        let live = [s("c/1"), s("a/1"), s("b/1"), s("d/1"), s("e/1")];
        let edges = [edge("c/1", "b/1"), edge("a/1", "b/1"), edge("e/1", "d/1")];
        let mut live_rev = live.clone();
        live_rev.reverse();
        let mut edges_rev = edges.clone();
        edges_rev.reverse();
        let plan = grouping_plan(&live, &edges);
        assert_eq!(plan, grouping_plan(&live_rev, &edges_rev));
        assert_eq!(plan.portfolios.len(), 2);
        assert_eq!(
            portfolio_id(&[s("a/1"), s("b/1"), s("c/1")]),
            portfolio_id(&[s("c/1"), s("a/1"), s("b/1"), s("a/1")])
        );
    }

    #[test]
    fn the_same_members_give_the_same_id_and_different_members_a_different_one() {
        let ab = portfolio_id(&[s("a/1"), s("b/1")]);
        assert_eq!(ab, portfolio_id(&[s("a/1"), s("b/1")]));
        assert_ne!(ab, portfolio_id(&[s("a/1"), s("b/1"), s("c/1")]));
        assert_ne!(ab, portfolio_id(&[s("a/1"), s("c/1")]));
        let hex = ab.0.strip_prefix("portfolio/").expect("prefixed");
        assert_eq!(hex.len(), 12);
    }

    fn formed() -> PortfolioRecord {
        let mut rec = PortfolioRecord::default();
        let members = vec![s("b/1"), s("a/1")];
        assert_eq!(
            rec.form(
                portfolio_id(&members),
                members,
                "linked".to_owned(),
                Vec::new()
            ),
            FormOutcome::Formed
        );
        rec
    }

    #[test]
    fn the_first_form_creates_an_active_generation_one_and_audits_it() {
        let rec = formed();
        assert_eq!(rec.state, PortfolioState::Active);
        assert_eq!(rec.generation, 1);
        assert_eq!(rec.members, [s("a/1"), s("b/1")], "sorted");
        assert_eq!(rec.audit.len(), 1);
        assert_eq!(rec.audit[0].event, PortfolioEvent::Formed);
        assert_eq!(rec.audit[0].reason, "linked");
    }

    fn contention_link() -> CouplingLink {
        CouplingLink {
            a: s("a/1"),
            b: s("b/1"),
            reason: "a/1 and b/1 contend".to_owned(),
            facts: BTreeMap::from([
                ("kind".to_owned(), "contention".to_owned()),
                ("via".to_owned(), "wc/1".to_owned()),
            ]),
        }
    }

    #[test]
    fn a_formation_records_its_links_with_their_facts() {
        let mut rec = PortfolioRecord::default();
        let members = vec![s("a/1"), s("b/1")];
        rec.form(
            portfolio_id(&members),
            members,
            "a/1 and b/1 contend".to_owned(),
            vec![contention_link()],
        );
        assert_eq!(
            rec.audit[0].reason, "a/1 and b/1 contend",
            "the exact reason"
        );
        assert_eq!(rec.audit[0].links, [contention_link()]);
    }

    #[test]
    fn an_entry_written_before_links_loads_with_none() {
        let json = r#"{"event":"formed","generation":1,"reason":"linked","members":["a/1","b/1"]}"#;
        let entry: PortfolioAuditEntry = serde_json::from_str(json).expect("older entry loads");
        assert!(entry.links.is_empty());
        let back = serde_json::to_string(&entry).expect("serialize");
        assert!(!back.contains("links"), "no links are skipped: {back}");
    }

    /// An entry with links must load in an older build whose entry lacks the field.
    #[test]
    fn an_entry_with_links_loads_without_the_field() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct EntryBeforeLinks {
            event: PortfolioEvent,
            generation: u32,
            reason: String,
            members: Vec<EntityUri>,
            #[serde(default)]
            reviewer: Option<ReviewerId>,
        }
        let entry = PortfolioAuditEntry {
            event: PortfolioEvent::Formed,
            generation: 1,
            reason: "a/1 and b/1 contend".to_owned(),
            members: vec![s("a/1"), s("b/1")],
            reviewer: None,
            links: vec![contention_link()],
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"links\""), "{json}");
        let old: EntryBeforeLinks = serde_json::from_str(&json).expect("the old shape loads");
        assert_eq!(old.reason, entry.reason);
        assert_eq!(old.members, entry.members);
    }

    #[test]
    fn forming_an_active_portfolio_again_is_a_no_op() {
        let mut rec = formed();
        let members = vec![s("a/1"), s("b/1")];
        let before = rec.clone();
        assert_eq!(
            rec.form(
                portfolio_id(&members),
                members,
                "again".to_owned(),
                Vec::new()
            ),
            FormOutcome::Unchanged
        );
        assert_eq!(rec, before);
    }

    #[test]
    fn supersede_audits_once_and_is_idempotent() {
        let mut rec = formed();
        assert!(rec.supersede("member a/1 aborted".to_owned()));
        assert!(!rec.supersede("member b/1 regrouped".to_owned()));
        assert_eq!(rec.state, PortfolioState::Superseded);
        assert_eq!(rec.audit.len(), 2);
        assert_eq!(rec.audit[1].event, PortfolioEvent::Superseded);
        assert_eq!(rec.audit[1].reason, "member a/1 aborted");
        assert!(!PortfolioRecord::default().supersede("never formed".to_owned()));
    }

    #[test]
    fn forming_after_supersede_reactivates_as_a_new_generation() {
        let mut rec = formed();
        rec.supersede("member c/1 joined".to_owned());
        let members = vec![s("a/1"), s("b/1")];
        assert_eq!(
            rec.form(
                portfolio_id(&members),
                members,
                "linked".to_owned(),
                Vec::new()
            ),
            FormOutcome::Reactivated
        );
        assert_eq!(rec.state, PortfolioState::Active);
        assert_eq!(rec.generation, 2);
        let events: Vec<_> = rec.audit.iter().map(|e| (e.event, e.generation)).collect();
        assert_eq!(
            events,
            [
                (PortfolioEvent::Formed, 1),
                (PortfolioEvent::Superseded, 1),
                (PortfolioEvent::Formed, 2)
            ]
        );
    }

    fn slate(options: usize) -> Slate {
        use ekg_domain::{Plan, RankedPlan, Score, Trace};
        let score = |key: &str| Score {
            metrics: BTreeMap::new(),
            rank_key: key.to_owned(),
        };
        Slate {
            baseline: score("9"),
            options: (0..options)
                .map(|i| RankedPlan {
                    plan: Plan {
                        actions: Vec::new(),
                    },
                    score: score(&i.to_string()),
                    recommended: i == 0,
                    needs: Vec::new(),
                    omissions: Vec::new(),
                    trace: Trace::default(),
                })
                .collect(),
            trace: Trace::default(),
        }
    }

    #[test]
    fn a_slate_attaches_to_an_active_portfolio_at_its_generation_and_is_audited() {
        let mut rec = formed();
        rec.attach_slate(1, slate(2), "1000".to_owned())
            .expect("attach");
        assert_eq!(rec.slate_generation, Some(1));
        assert_eq!(rec.slate_attached_at.as_deref(), Some("1000"));
        assert_eq!(rec.slate.as_ref().map(|s| s.options.len()), Some(2));
        let last = rec.audit.last().expect("an entry");
        assert_eq!(last.event, PortfolioEvent::SlateAttached);
        assert_eq!(last.generation, 1);
        assert!(
            last.reason.starts_with("joint slate attached: 2 option(s)"),
            "{}",
            last.reason
        );
        assert_eq!(last.members, rec.members);
        // Attaching changes no membership, state or generation.
        assert_eq!(rec.state, PortfolioState::Active);
        assert_eq!(rec.generation, 1);
    }

    #[test]
    fn a_superseded_portfolio_refuses_a_slate() {
        let mut rec = formed();
        rec.supersede("member a/1 promoted".to_owned());
        let before = rec.clone();
        let err = rec
            .attach_slate(1, slate(1), "1000".to_owned())
            .expect_err("superseded");
        assert!(matches!(err, EngineError::Invariant(_)), "{err}");
        assert_eq!(rec, before, "nothing recorded");
    }

    #[test]
    fn a_wrong_generation_refuses_a_slate() {
        let mut rec = formed();
        let before = rec.clone();
        for stale in [0, 2] {
            let err = rec
                .attach_slate(stale, slate(1), "1000".to_owned())
                .expect_err("wrong generation");
            assert!(matches!(err, EngineError::Invariant(_)), "{err}");
        }
        assert_eq!(rec, before, "nothing recorded");
    }

    #[test]
    fn a_reactivation_clears_the_slate() {
        let mut rec = formed();
        rec.attach_slate(1, slate(1), "1000".to_owned())
            .expect("attach");
        rec.supersede("member c/1 joined".to_owned());
        assert!(
            rec.slate.is_some(),
            "a supersession keeps the slate as history"
        );
        let members = vec![s("a/1"), s("b/1")];
        rec.form(
            portfolio_id(&members),
            members,
            "linked".to_owned(),
            Vec::new(),
        );
        assert_eq!(rec.generation, 2);
        assert!(
            rec.slate.is_none(),
            "no current slate until generation 2 attaches"
        );
        assert!(rec.slate_generation.is_none());
        assert!(
            rec.slate_attached_at.is_none(),
            "the time goes with the slate"
        );
        rec.attach_slate(2, slate(1), "2000".to_owned())
            .expect("attach generation 2");
        assert_eq!(rec.slate_generation, Some(2));
        assert_eq!(
            rec.slate_attached_at.as_deref(),
            Some("2000"),
            "overwritten"
        );
    }

    #[test]
    fn a_legacy_record_without_the_slate_fields_loads() {
        let legacy = r#"{
            "id": "portfolio/0123456789ab",
            "members": ["https://ekg.example/id/a/1"],
            "state": "active",
            "generation": 1,
            "audit": []
        }"#;
        let rec: PortfolioRecord = serde_json::from_str(legacy).expect("legacy loads");
        assert!(rec.slate.is_none());
        assert!(rec.slate_generation.is_none());
        assert!(rec.slate_attached_at.is_none());
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(!json.contains("slate"), "absent fields stay absent: {json}");
    }

    // --- promotion ---

    fn reviewer() -> ReviewerId {
        ReviewerId("planner-1".to_owned())
    }

    /// An active generation-1 portfolio with a two-option joint slate ("0", "1").
    fn slated() -> PortfolioRecord {
        let mut rec = formed();
        rec.attach_slate(1, slate(2), "1000".to_owned())
            .expect("attach");
        rec
    }

    fn promotion_of(generation: u32, rank_key: &str, receipts: usize) -> PortfolioPromotion {
        use ekg_domain::{ActionType, ExecutionReceipt, ProposedAction, Revision};
        let action = ProposedAction {
            action_type: ActionType("expedite_po".to_owned()),
            target: s("purchase.order/7"),
            params: BTreeMap::new(),
        };
        PortfolioPromotion {
            generation,
            rank_key: rank_key.to_owned(),
            reviewer: reviewer(),
            rationale: "approve plan A".to_owned(),
            execution_plan: ExecutionPlan {
                actions: vec![action.clone(); receipts],
                receipts: vec![
                    ExecutionReceipt {
                        action,
                        external_ref: "odoo://x".to_owned(),
                        source_revision: Revision(1),
                        reversible: true,
                    };
                    receipts
                ],
            },
            promoted_at: "1000".to_owned(),
            progress: ReverseProgress::default(),
        }
    }

    #[test]
    fn promote_records_the_promotion_and_audits_it() {
        let mut rec = slated();
        rec.promote(promotion_of(1, "1", 2)).expect("promote");
        assert_eq!(rec.state, PortfolioState::Promoted);
        assert_eq!(
            rec.promotion.as_ref().map(|p| p.rank_key.as_str()),
            Some("1")
        );
        let last = rec.audit.last().expect("an entry");
        assert_eq!(last.event, PortfolioEvent::Promoted);
        assert_eq!(last.generation, 1);
        assert_eq!(last.reviewer, Some(reviewer()));
        assert_eq!(
            last.reason,
            "promoted option 1 (0 action(s)) across 2 member(s): approve plan A"
        );
    }

    #[test]
    fn promote_needs_an_active_portfolio_at_the_named_generation_with_the_option() {
        // Stale generation.
        let mut rec = slated();
        let before = rec.clone();
        for (refused, why) in [
            (promotion_of(2, "1", 1), "is at generation 1"),
            (promotion_of(0, "1", 1), "is at generation 1"),
            (promotion_of(1, "nope", 1), "is not on the joint slate"),
        ] {
            let err = rec.promote(refused).expect_err("refused");
            assert!(matches!(err, EngineError::Invariant(_)), "{err}");
            assert!(err.to_string().contains(why), "the reason names it: {err}");
        }
        assert_eq!(rec, before, "nothing recorded");
        // No slate for the generation.
        let mut unslated = formed();
        let err = unslated
            .promote(promotion_of(1, "0", 1))
            .expect_err("no slate");
        assert!(err.to_string().contains("no joint slate"), "{err}");
        // Superseded.
        let mut superseded = slated();
        superseded.supersede("member a/1 aborted".to_owned());
        let err = superseded
            .promote(promotion_of(1, "0", 1))
            .expect_err("superseded");
        assert!(err.to_string().contains("Superseded"), "{err}");
        // Already promoted.
        let mut twice = slated();
        twice.promote(promotion_of(1, "0", 1)).expect("first");
        assert!(twice.promote(promotion_of(1, "0", 1)).is_err());
    }

    #[test]
    fn a_late_supersede_leaves_a_promoted_portfolio_promoted() {
        let mut rec = slated();
        rec.promote(promotion_of(1, "0", 1)).expect("promote");
        let before = rec.clone();
        assert!(!rec.supersede("member a/1 promoted".to_owned()));
        assert_eq!(rec, before);
        assert!(
            !rec.audit
                .iter()
                .any(|e| e.event == PortfolioEvent::Superseded),
            "no supersession audited"
        );
    }

    #[test]
    fn a_partial_reverse_is_audited_and_the_portfolio_stays_promoted() {
        let mut rec = slated();
        rec.promote(promotion_of(1, "0", 2)).expect("promote");
        let progress = ReverseProgress {
            reversed: [1].into_iter().collect(),
            refused: vec![crate::lifecycle::RefusedReverse {
                index: 0,
                reason: "expedite_po on po/7 (source changed)".to_owned(),
            }],
        };
        rec.record_partial_reverse(reviewer(), progress.clone())
            .expect("recorded");
        assert_eq!(rec.state, PortfolioState::Promoted);
        assert_eq!(rec.promotion.as_ref().map(|p| &p.progress), Some(&progress));
        let last = rec.audit.last().expect("an entry");
        assert_eq!(last.event, PortfolioEvent::PartiallyReversed);
        assert_eq!(
            last.reason,
            "partially reversed: 1 of 2 action(s) reversed, 1 remain unreversed: \
             expedite_po on po/7 (source changed)"
        );
        // A progress over another plan is refused.
        let stray = ReverseProgress {
            reversed: [0, 1, 2].into_iter().collect(),
            refused: Vec::new(),
        };
        assert!(rec.record_partial_reverse(reviewer(), stray).is_err());
        // Not promoted: refused.
        assert!(
            slated()
                .record_partial_reverse(reviewer(), ReverseProgress::default())
                .is_err()
        );
    }

    #[test]
    fn reverse_moves_promoted_to_reversed_and_audits_it() {
        let mut rec = slated();
        assert!(
            rec.reverse(reviewer(), "why".to_owned()).is_err(),
            "not promoted"
        );
        rec.promote(promotion_of(1, "0", 2)).expect("promote");
        rec.reverse(reviewer(), "supplier fell through".to_owned())
            .expect("reverse");
        assert_eq!(rec.state, PortfolioState::Reversed);
        assert_eq!(
            rec.promotion.as_ref().map(|p| p.progress.reversed.len()),
            Some(2),
            "every receipt is recorded reversed"
        );
        let last = rec.audit.last().expect("an entry");
        assert_eq!(last.event, PortfolioEvent::Reversed);
        assert_eq!(last.reason, "supplier fell through");
        assert_eq!(last.reviewer, Some(reviewer()));
        assert!(rec.reverse(reviewer(), "again".to_owned()).is_err());
    }

    #[test]
    fn forming_again_after_a_promotion_starts_a_new_generation() {
        for undo in [false, true] {
            let mut rec = slated();
            rec.promote(promotion_of(1, "0", 1)).expect("promote");
            if undo {
                rec.reverse(reviewer(), "why".to_owned()).expect("reverse");
            }
            let members = vec![s("a/1"), s("b/1")];
            assert_eq!(
                rec.form(
                    portfolio_id(&members),
                    members,
                    "linked".to_owned(),
                    Vec::new()
                ),
                FormOutcome::Reactivated
            );
            assert_eq!(rec.state, PortfolioState::Active);
            assert_eq!(rec.generation, 2);
            assert!(rec.slate.is_none());
            assert!(rec.promotion.is_some(), "the promotion stays as history");
        }
    }

    #[test]
    fn a_legacy_record_without_a_promotion_or_reviewers_loads() {
        let legacy = r#"{
            "id": "portfolio/0123456789ab",
            "members": ["https://ekg.example/id/a/1"],
            "state": "superseded",
            "generation": 1,
            "audit": [{"event": "formed", "generation": 1, "reason": "linked",
                       "members": ["https://ekg.example/id/a/1"]}]
        }"#;
        let rec: PortfolioRecord = serde_json::from_str(legacy).expect("legacy loads");
        assert!(rec.promotion.is_none());
        assert_eq!(rec.audit[0].reviewer, None);
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(
            !json.contains("promotion") && !json.contains("reviewer"),
            "absent fields stay absent: {json}"
        );
    }

    // --- dismissal ---

    #[test]
    fn dismiss_moves_an_active_portfolio_to_dismissed_and_audits_it() {
        let mut rec = slated();
        rec.dismiss(1, reviewer(), "customer cancelled".to_owned())
            .expect("dismiss");
        assert_eq!(rec.state, PortfolioState::Dismissed);
        let last = rec.audit.last().expect("an entry");
        assert_eq!(last.event, PortfolioEvent::Dismissed);
        assert_eq!(last.generation, 1);
        assert_eq!(last.reason, "customer cancelled");
        assert_eq!(last.reviewer, Some(reviewer()));
        assert_eq!(last.members, [s("a/1"), s("b/1")]);
        // A pending portfolio (no slate yet) is dismissed too.
        let mut unslated = formed();
        unslated
            .dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss without a slate");
    }

    #[test]
    fn dismiss_refuses_a_stale_generation_and_every_state_but_active() {
        let mut rec = slated();
        let before = rec.clone();
        for stale in [0, 2] {
            let err = rec
                .dismiss(stale, reviewer(), "why".to_owned())
                .expect_err("stale");
            assert!(err.to_string().contains("is at generation 1"), "{err}");
        }
        assert_eq!(rec, before, "nothing recorded");

        let mut superseded = slated();
        superseded.supersede("member a/1 aborted".to_owned());
        let mut promoted = slated();
        promoted.promote(promotion_of(1, "0", 1)).expect("promote");
        let mut reversed = promoted.clone();
        reversed
            .reverse(reviewer(), "why".to_owned())
            .expect("reverse");
        let mut dismissed = slated();
        dismissed
            .dismiss(1, reviewer(), "first".to_owned())
            .expect("first");
        for (mut rec, state) in [
            (superseded, "Superseded"),
            (promoted, "Promoted"),
            (reversed, "Reversed"),
            (dismissed, "Dismissed"),
            (PortfolioRecord::default(), "Superseded"),
        ] {
            let before = rec.clone();
            let err = rec
                .dismiss(1, reviewer(), "why".to_owned())
                .expect_err("not active");
            assert!(err.to_string().contains(state), "{err}");
            assert_eq!(rec, before, "nothing recorded");
        }
    }

    #[test]
    fn a_dismissed_generation_takes_no_slate_promotion_or_supersession() {
        let mut rec = formed();
        rec.dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss");
        let before = rec.clone();
        assert!(rec.attach_slate(1, slate(1), "1000".to_owned()).is_err());
        assert!(rec.promote(promotion_of(1, "0", 1)).is_err());
        assert!(!rec.supersede("member a/1 aborted".to_owned()));
        assert_eq!(rec, before, "nothing recorded");
    }

    #[test]
    fn fresh_members_coupling_again_after_a_dismissal_start_a_new_generation() {
        let mut rec = slated();
        rec.dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss");
        let members = vec![s("a/1"), s("b/1")];
        assert_eq!(
            rec.form(
                portfolio_id(&members),
                members,
                "linked".to_owned(),
                Vec::new()
            ),
            FormOutcome::Reactivated
        );
        assert_eq!(rec.state, PortfolioState::Active);
        assert_eq!(rec.generation, 2);
        assert!(rec.slate.is_none() && rec.slate_attached_at.is_none());
        assert!(
            rec.audit
                .iter()
                .any(|e| e.event == PortfolioEvent::Dismissed && e.generation == 1),
            "the dismissal stays in the audit trail"
        );
    }

    #[test]
    fn a_decided_portfolio_reports_its_outcome_and_an_undecided_one_reports_none() {
        assert_eq!(formed().outcome(7), None, "formed");
        assert_eq!(slated().outcome(7), None, "a slate is no decision");

        let mut rec = slated();
        rec.promote(promotion_of(1, "1", 2)).expect("promote");
        let promoted = rec.outcome(1_005).expect("promoted");
        assert_eq!(promoted.portfolio, rec.id.clone().expect("an id"));
        assert_eq!((promoted.generation, promoted.at), (1, 1_005));
        assert_eq!(promoted.event, PortfolioEvent::Promoted);
        assert_eq!(promoted.reviewer, Some(reviewer()));
        assert_eq!(promoted.rank_key.as_deref(), Some("1"));
        assert_eq!(promoted.actions.len(), 2, "the executed actions");
        assert_eq!(promoted.slate_attached_at, rec.slate_attached_at);

        // An undo in part, then in whole: the promotion's actions are not repeated.
        let part = ReverseProgress {
            reversed: BTreeSet::from([1]),
            refused: Vec::new(),
        };
        rec.record_partial_reverse(reviewer(), part).expect("part");
        let partly = rec.outcome(1_010).expect("partly reversed");
        assert_eq!(partly.event, PortfolioEvent::PartiallyReversed);
        assert!(partly.actions.is_empty() && partly.rank_key.is_none());
        rec.reverse(reviewer(), "why".to_owned()).expect("reverse");
        let reversed = rec.outcome(1_020).expect("reversed");
        assert_eq!(reversed.event, PortfolioEvent::Reversed);
        assert_eq!((reversed.generation, reversed.at), (1, 1_020));

        let mut rec = slated();
        rec.dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss");
        let dismissed = rec.outcome(9).expect("dismissed");
        assert_eq!(dismissed.event, PortfolioEvent::Dismissed);
        assert_eq!(dismissed.reviewer, Some(reviewer()));
        assert!(dismissed.actions.is_empty());

        let mut rec = slated();
        assert!(rec.supersede("member a/1 aborted".to_owned()));
        let superseded = rec.outcome(9).expect("superseded");
        assert_eq!(superseded.event, PortfolioEvent::Superseded);
        assert_eq!(superseded.reviewer, None, "nobody decided it");
        assert_eq!(superseded.generation, 1);
    }

    #[test]
    fn an_outcome_names_the_generation_it_was_decided_for_not_a_later_one() {
        let mut rec = slated();
        rec.dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss");
        let members = vec![s("a/1"), s("b/1")];
        rec.form(
            portfolio_id(&members),
            members,
            "linked".to_owned(),
            Vec::new(),
        );
        assert_eq!(rec.generation, 2);
        assert_eq!(rec.outcome(9), None, "generation 2 is undecided");
        assert!(rec.supersede("member a/1 aborted".to_owned()));
        let outcome = rec.outcome(9).expect("superseded");
        assert_eq!(outcome.generation, 2);
        assert_eq!(
            outcome.slate_attached_at, None,
            "generation 2 never carried a slate"
        );
    }

    #[test]
    fn a_portfolio_outcome_round_trips_and_leaves_out_what_it_does_not_have() {
        let mut rec = slated();
        rec.dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss");
        let outcome = rec.outcome(9).expect("dismissed");
        let json = serde_json::to_value(&outcome).expect("json");
        assert_eq!(json["event"], "dismissed");
        assert!(json.get("actions").is_none() && json.get("rank_key").is_none());
        let back: PortfolioOutcome = serde_json::from_value(json).expect("back");
        assert_eq!(back, outcome);
    }

    #[test]
    fn a_dismissed_record_round_trips() {
        let mut rec = slated();
        rec.dismiss(1, reviewer(), "why".to_owned())
            .expect("dismiss");
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(json.contains(r#""state":"dismissed""#), "{json}");
        assert!(json.contains(r#""event":"dismissed""#), "{json}");
        let back: PortfolioRecord = serde_json::from_str(&json).expect("loads");
        assert_eq!(back, rec);
    }
}
