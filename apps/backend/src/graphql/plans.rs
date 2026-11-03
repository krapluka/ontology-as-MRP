//! The plans a human oversees, as the inbox lists them. Two lists: the plans awaiting
//! oversight (one per active portfolio, planned by its joint slate, and one per awaiting point
//! in no active portfolio, planned by its own slate), and the plans a human approved, with
//! their undo windows. Every value is read from the durable state or derived by a stated
//! rule; serving either list commits nothing.
//!
//! The listing rules are pure functions over the durable records, so they can be tested
//! without Restate. The resolvers only read the records and call them.

use std::collections::{BTreeMap, BTreeSet};

use async_graphql::{
    ComplexObject, Context, Error as GqlError, ID, Result as GqlResult, SimpleObject,
};
use ekg_domain::{EntityUri, Slate};
use ekg_dynamic::{ActionPointRecord, PortfolioEvent, PortfolioRecord, PortfolioState};
use futures::future::join_all;

use super::model::{
    ActionPointView, ChangeView, PlanView, ScoreView, TraceView, change_from_receipt, display_code,
    metrics_from, option_card, with_derived_display,
};
use super::{GraphqlState, projections};

/// A portfolio a plan resolves, as its commands need it: its id and current generation.
#[derive(Debug, Clone, PartialEq, Eq, SimpleObject)]
pub struct PortfolioRef {
    /// The portfolio id (`portfolio/…`).
    pub id: ID,
    /// The generation the plan belongs to, which a portfolio command must carry.
    pub generation: i32,
}

/// One plan awaiting oversight, shown as one inbox row: the joint plan of an active portfolio,
/// or the plan of a single awaiting point in no active portfolio.
#[derive(Debug, Clone, SimpleObject)]
#[graphql(complex)]
pub struct PlanEntry {
    /// The plan id: `plan/` plus the digest of the sorted points it resolves. This is the digest
    /// that names a portfolio, so a portfolio's plan id and its portfolio id share one digest.
    pub id: ID,
    /// The plan's short display code, shown as `PL-XXXX`: `display_code` over the plan id.
    pub code: String,
    /// The points the plan resolves, each with its state and reason.
    pub resolves: Vec<ActionPointView>,
    /// The portfolio the plan resolves, or `null` for a single point.
    pub portfolio: Option<PortfolioRef>,
    /// When the plan's current slate was attached, as Unix epoch seconds; `null` while it is
    /// pending, or for a slate attached before the time was recorded.
    pub opened_at: Option<String>,
    /// Why the points need a plan: a portfolio's coupling reasons for its current generation,
    /// or the point's reason.
    pub situation: String,
    /// The slate that plans the entry (backs `plan`; not exposed directly).
    #[graphql(skip)]
    pub slate: Option<Slate>,
    /// The subjects the plan resolves, sorted (backs `plan`; not exposed directly).
    #[graphql(skip)]
    pub members: Vec<String>,
}

#[ComplexObject]
impl PlanEntry {
    /// The plan itself (its options, reasoning, lanes and framing, as a point's plan read
    /// serves them), or `null` while its slate is pending. A failure to read one entry's plan
    /// is reported on this field only; the list is still served.
    async fn plan(&self, ctx: &Context<'_>) -> GqlResult<Option<PlanView>> {
        let Some(slate) = &self.slate else {
            return Ok(None);
        };
        let gql = ctx.data::<GraphqlState>()?;
        build_plan(gql, slate, None, &self.members).await.map(Some)
    }
}

/// A plan an operator approved: a promoted portfolio, or a point a human promoted on its own,
/// with what was approved and until when it can be undone.
#[derive(Debug, Clone, SimpleObject)]
pub struct ApprovedPlan {
    /// The plan id (the same rule as a listed plan's).
    pub id: ID,
    /// The plan's short display code (the same rule as a listed plan's).
    pub code: String,
    /// The points the plan resolved.
    pub resolves: Vec<ActionPointView>,
    /// The portfolio whose plan was approved, or `null` for a single point.
    pub portfolio: Option<PortfolioRef>,
    /// The approved option's `rank_key`, when recorded (opaque).
    pub rank_key: Option<String>,
    /// The committed changes, each with its recorded inverse.
    pub changes: Vec<ChangeView>,
    /// Who approved it.
    pub reviewer: String,
    /// Their rationale.
    pub rationale: String,
    /// When it was approved, as Unix epoch seconds.
    pub promoted_at: Option<String>,
    /// When its reversal window closes, as Unix epoch seconds. An undo after it is refused.
    pub undo_until: Option<String>,
    /// Whether every committed change is reversible.
    pub reversible: bool,
    /// How many of its committed changes are still unreversed after a partial undo (0 when
    /// no undo ran).
    pub remaining: i32,
}

// --- the plan-id rule ---

/// The plan id over the points a plan resolves: `plan/{digest}`, the digest
/// [`ekg_dynamic::portfolio_id`] computes over the sorted, de-duplicated subjects. A portfolio's
/// plan id therefore shares its portfolio id's digest, and a single point's plan id is the
/// digest of its subject (its [`ActionPointView::digest`]).
#[must_use]
pub fn plan_id(resolves: &[EntityUri]) -> String {
    let portfolio = ekg_dynamic::portfolio_id(resolves).0;
    let digest = portfolio.strip_prefix("portfolio/").unwrap_or(&portfolio);
    format!("plan/{digest}")
}

// --- the plan builder ---

/// Build the plan view of `slate` over the points it resolves (`members`, sorted), read for
/// `asked` (a point's plan read) or for the plan's lead point (a listed plan). The baseline and
/// every option are framed through the lead point: the one point, or the member with the
/// worst baseline framing; the lanes merge every member's.
///
/// # Errors
/// A field error when a member is not a canonical entity IRI (the plan cannot be framed).
pub(crate) async fn build_plan(
    gql: &GraphqlState,
    slate: &Slate,
    asked: Option<&str>,
    members: &[String],
) -> GqlResult<PlanView> {
    let canonical = members
        .iter()
        .map(|member| {
            projections::canonical_subject(member).ok_or_else(|| {
                GqlError::new(format!(
                    "plan unavailable: {member:?} is not a canonical entity IRI"
                ))
            })
        })
        .collect::<GqlResult<Vec<EntityUri>>>()?;
    let framings: Vec<(EntityUri, BTreeMap<String, String>)> =
        join_all(canonical.iter().map(|member| async move {
            (
                member.clone(),
                projections::baseline_framing(gql, &member.0).await,
            )
        }))
        .await;
    let (lead, lead_framing) = ekg_semantic::display::lead_member(&framings)
        .and_then(|lead| framings.iter().find(|(member, _)| member == lead))
        .cloned()
        .ok_or_else(|| GqlError::new("plan unavailable: it resolves no point"))?;
    // The do-nothing baseline carries the same framing as an option, the lead point's, judged
    // on the current world. The framing only adds keys: it does not overwrite one the scorer
    // set and does not change the rank.
    let mut baseline = with_derived_display(&slate.baseline.metrics);
    for (key, value) in lead_framing {
        baseline.entry(key).or_insert(value);
    }
    let members: Vec<String> = canonical.iter().map(|member| member.0.clone()).collect();
    Ok(PlanView {
        subject: ID(asked.map_or_else(|| lead.0.clone(), str::to_owned)),
        resolves: members.iter().cloned().map(ID).collect(),
        lead: ID(lead.0.clone()),
        baseline: ScoreView {
            metrics: metrics_from(&baseline),
            rank_key: slate.baseline.rank_key.clone(),
        },
        options: slate
            .options
            .iter()
            .map(|option| option_card(option, &lead.0, &members))
            .collect(),
        trace: TraceView::from(&slate.trace),
    })
}

/// A point's plan from its durable `record` and the `portfolio` record it names (if any):
/// while the point is a member of an active portfolio, the portfolio's joint slate for its
/// current generation, resolving every member (`None` while it is pending, never the
/// member's pre-join slate); otherwise the point's own slate, resolving the point alone.
///
/// # Errors
/// A field error when a member is not a canonical entity IRI.
pub(crate) async fn point_plan(
    gql: &GraphqlState,
    subject: &str,
    record: &ActionPointRecord,
    portfolio: Option<&PortfolioRecord>,
) -> GqlResult<Option<PlanView>> {
    let Some(slate) = projections::point_slate(record, portfolio) else {
        return Ok(None);
    };
    let members = match projections::active_portfolio(record, portfolio) {
        Some(portfolio) => portfolio.members.iter().map(|m| m.0.clone()).collect(),
        None => vec![subject.to_owned()],
    };
    build_plan(gql, slate, Some(subject), &members)
        .await
        .map(Some)
}

// --- the plans awaiting oversight ---

/// Whether a record's point awaits oversight (`open`, `proposed` or `under_review`).
fn awaits(record: &ActionPointRecord) -> bool {
    record
        .point
        .as_ref()
        .is_some_and(|point| ekg_dynamic::is_pre_promotion(point.state))
}

/// The epoch seconds of a stringly time, for ordering.
fn secs(at: Option<&str>) -> Option<u64> {
    at.and_then(|at| at.parse().ok())
}

/// The situation of a portfolio's plan: its `formed` audit entry for the current generation
/// (the capability's wording of the coupling facts it recorded, or its recorded coupling
/// reasons when it recorded no facts), else its members' reasons. The texts keep their exact
/// entity ids; the view labels them.
fn portfolio_situation(
    portfolio: &PortfolioRecord,
    points: &BTreeMap<&str, &ActionPointRecord>,
) -> String {
    portfolio
        .audit
        .iter()
        .rev()
        .find(|entry| {
            entry.event == PortfolioEvent::Formed && entry.generation == portfolio.generation
        })
        .map(|entry| {
            crate::slates::worded_coupling(&entry.links)
                .map_or_else(|| entry.reason.clone(), |sentences| sentences.join(" "))
        })
        .filter(|reason| !reason.is_empty())
        .unwrap_or_else(|| {
            portfolio
                .members
                .iter()
                .filter_map(|member| points.get(member.0.as_str())?.point.as_ref())
                .map(|point| point.reason.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        })
}

/// The plans awaiting oversight over the durable records: one entry per active portfolio
/// with a member still awaiting oversight, planned by its current joint slate (none while
/// pending); one per awaiting point in no active portfolio, planned by its own slate.
/// Every awaiting point is resolved by exactly one entry. Ordered by slate-attach time, newest
/// first, then entries without it, each group by plan id.
#[must_use]
pub fn plan_entries(
    records: &[(String, ActionPointRecord)],
    portfolios: &BTreeMap<String, PortfolioRecord>,
) -> Vec<PlanEntry> {
    let points: BTreeMap<&str, &ActionPointRecord> = records
        .iter()
        .map(|(subject, record)| (subject.as_str(), record))
        .collect();
    let mut entries = Vec::new();
    let mut covered: BTreeSet<&str> = BTreeSet::new();

    for (id, portfolio) in portfolios {
        if portfolio.state != PortfolioState::Active
            || !portfolio
                .members
                .iter()
                .any(|member| points.get(member.0.as_str()).is_some_and(|r| awaits(r)))
        {
            continue;
        }
        covered.extend(portfolio.members.iter().map(|member| member.0.as_str()));
        let slate = projections::joint_slate(portfolio).cloned();
        let opened_at = portfolio
            .slate_attached_at
            .clone()
            .filter(|_| slate.is_some());
        entries.push(PlanEntry {
            id: ID(plan_id(&portfolio.members)),
            code: display_code(&plan_id(&portfolio.members)),
            resolves: portfolio
                .members
                .iter()
                .filter_map(|member| points.get(member.0.as_str())?.point.as_ref())
                .map(ActionPointView::from)
                .collect(),
            portfolio: Some(PortfolioRef {
                id: ID(id.clone()),
                generation: i32::try_from(portfolio.generation).unwrap_or(i32::MAX),
            }),
            opened_at,
            situation: portfolio_situation(portfolio, &points),
            slate,
            members: portfolio.members.iter().map(|m| m.0.clone()).collect(),
        });
    }

    for (subject, record) in records {
        let Some(point) = record.point.as_ref().filter(|_| awaits(record)) else {
            continue;
        };
        let in_active_portfolio = record
            .portfolio
            .as_ref()
            .and_then(|id| portfolios.get(&id.0))
            .is_some_and(|portfolio| portfolio.state == PortfolioState::Active);
        if in_active_portfolio || covered.contains(subject.as_str()) {
            continue;
        }
        entries.push(PlanEntry {
            id: ID(plan_id(&[EntityUri(subject.clone())])),
            code: display_code(&plan_id(&[EntityUri(subject.clone())])),
            resolves: vec![ActionPointView::from(point)],
            portfolio: None,
            opened_at: record
                .slate_attached_at
                .clone()
                .filter(|_| record.slate.is_some()),
            situation: point.reason.clone(),
            slate: record.slate.clone(),
            members: vec![subject.clone()],
        });
    }

    entries.sort_by(|a, b| {
        match (secs(a.opened_at.as_deref()), secs(b.opened_at.as_deref())) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.id.0.cmp(&b.id.0))
    });
    entries
}

// --- the plans approved ---

/// Whether an approval at `promoted_at` is listed at `now`: approved since `since` (the start
/// of the operator's day), or earlier with its reversal window still open.
fn listed(promoted_at: u64, since: u64, window: u64, now: u64) -> bool {
    promoted_at >= since || promoted_at.saturating_add(window) > now
}

/// The `rank_key` a single point's promotion recorded in its context (`… rank_key=<key>`).
fn recorded_rank_key(context_seen: &str) -> Option<String> {
    context_seen
        .rsplit_once("rank_key=")
        .map(|(_, key)| key.split(';').next().unwrap_or(key).trim().to_owned())
        .filter(|key| !key.is_empty())
}

/// The plans a human approved, over the durable records: every promoted portfolio (a
/// reversed one is done), and every point a human promoted on its own (not through a
/// portfolio, and not by the auto-apply policy) that is still promoted. A member promoted
/// through its portfolio is represented by the portfolio. Listed when approved since `since`
/// or while its reversal window (`window` seconds) is open at `now`. Newest first.
#[must_use]
pub fn approved_plans(
    records: &[(String, ActionPointRecord)],
    portfolios: &BTreeMap<String, PortfolioRecord>,
    window: u64,
    since: u64,
    now: u64,
) -> Vec<ApprovedPlan> {
    let points: BTreeMap<&str, &ActionPointRecord> = records
        .iter()
        .map(|(subject, record)| (subject.as_str(), record))
        .collect();
    let mut approved = Vec::new();

    for (id, portfolio) in portfolios {
        let Some(promotion) = portfolio
            .promotion
            .as_ref()
            .filter(|_| portfolio.state == PortfolioState::Promoted)
        else {
            continue;
        };
        let Some(at) = secs(Some(&promotion.promoted_at)) else {
            continue;
        };
        if !listed(at, since, window, now) {
            continue;
        }
        let receipts = &promotion.execution_plan.receipts;
        approved.push(ApprovedPlan {
            id: ID(plan_id(&portfolio.members)),
            code: display_code(&plan_id(&portfolio.members)),
            resolves: portfolio
                .members
                .iter()
                .filter_map(|member| points.get(member.0.as_str())?.point.as_ref())
                .map(ActionPointView::from)
                .collect(),
            portfolio: Some(PortfolioRef {
                id: ID(id.clone()),
                generation: i32::try_from(promotion.generation).unwrap_or(i32::MAX),
            }),
            rank_key: Some(promotion.rank_key.clone()),
            changes: receipts.iter().map(change_from_receipt).collect(),
            reviewer: promotion.reviewer.0.clone(),
            rationale: promotion.rationale.clone(),
            promoted_at: Some(promotion.promoted_at.clone()),
            undo_until: Some(at.saturating_add(window).to_string()),
            reversible: receipts.iter().all(|r| r.reversible),
            remaining: i32::try_from(
                receipts
                    .len()
                    .saturating_sub(promotion.progress.reversed.len()),
            )
            .unwrap_or(i32::MAX),
        });
    }

    for (subject, record) in records {
        let promoted = record
            .point
            .as_ref()
            .filter(|point| point.state == ekg_domain::ActionPointState::Promoted);
        let (Some(point), Some(promotion), Some(plan)) =
            (promoted, &record.promotion, &record.execution_plan)
        else {
            continue;
        };
        if record.promoted_via.is_some() || crate::action_point::promoted_by_policy(record) {
            continue;
        }
        let Some(at) = secs(record.promoted_at.as_deref()) else {
            continue;
        };
        if !listed(at, since, window, now) {
            continue;
        }
        approved.push(ApprovedPlan {
            id: ID(plan_id(&[EntityUri(subject.clone())])),
            code: display_code(&plan_id(&[EntityUri(subject.clone())])),
            resolves: vec![ActionPointView::from(point)],
            portfolio: None,
            rank_key: recorded_rank_key(&promotion.context_seen),
            changes: plan.receipts.iter().map(change_from_receipt).collect(),
            reviewer: promotion.reviewer.0.clone(),
            rationale: promotion.rationale.clone(),
            promoted_at: record.promoted_at.clone(),
            undo_until: Some(at.saturating_add(window).to_string()),
            reversible: plan.receipts.iter().all(|r| r.reversible),
            remaining: i32::try_from(
                plan.receipts
                    .len()
                    .saturating_sub(record.reverse_progress.reversed.len()),
            )
            .unwrap_or(i32::MAX),
        });
    }

    approved.sort_by(|a, b| {
        secs(b.promoted_at.as_deref())
            .cmp(&secs(a.promoted_at.as_deref()))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    approved
}

/// The start of the UTC day `now` falls in, as Unix epoch seconds. This is the approved
/// list's "today" when the client names no day of its own.
#[must_use]
pub const fn utc_day_start(now: u64) -> u64 {
    now - now % 86_400
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema};
    use ekg_domain::{
        ActionPointState, ActionType, ExecutionPlan, ExecutionReceipt, PortfolioId, Promotion,
        ProposedAction, ReviewerId, Revision, short_digest,
    };
    use ekg_dynamic::{PortfolioPromotion, PromotedVia, ReverseProgress, portfolio_id};
    use ekg_semantic::ontology;

    use super::*;
    use crate::graphql::projections::tests::{
        canned_state, expedite, member, notify_plan, portfolio, record, so,
    };

    fn mo() -> String {
        ontology::entity_iri("mrp.production", 1)
    }

    fn lone() -> String {
        ontology::entity_iri("sale.order", 9)
    }

    fn stamped(mut rec: (String, ActionPointRecord), at: &str) -> (String, ActionPointRecord) {
        rec.1.slate_attached_at = Some(at.to_owned());
        rec
    }

    /// The canned active portfolio of the SO and MO (generation 2, joint slate for 2), keyed
    /// by the id its record carries, attached at `at`.
    fn active(at: Option<&str>) -> BTreeMap<String, PortfolioRecord> {
        let (id, mut rec) = portfolio(&[&mo(), &so()], 2, expedite());
        rec.slate_attached_at = at.map(str::to_owned);
        rec.audit.push(ekg_dynamic::PortfolioAuditEntry {
            event: PortfolioEvent::Formed,
            generation: 2,
            reason: "MO/1 fulfils SO/1".to_owned(),
            members: rec.members.clone(),
            reviewer: None,
            links: Vec::new(),
        });
        BTreeMap::from([(id, rec)])
    }

    fn records() -> Vec<(String, ActionPointRecord)> {
        vec![
            member(&so(), Some(notify_plan(&so()))),
            member(&mo(), None),
            stamped(
                record(&lone(), ActionPointState::Proposed, Some(expedite())),
                "500",
            ),
        ]
    }

    fn ids(entries: &[PlanEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.id.0.as_str()).collect()
    }

    #[test]
    fn a_portfolio_and_a_single_point_are_listed_as_two_plans() {
        let entries = plan_entries(&records(), &active(Some("900")));
        assert_eq!(entries.len(), 2, "{:?}", ids(&entries));
        let joint = &entries[0];
        let members = [EntityUri(mo()), EntityUri(so())];
        assert_eq!(joint.id.0, plan_id(&members));
        assert_eq!(
            joint.portfolio,
            Some(PortfolioRef {
                id: ID("portfolio/0123456789ab".to_owned()),
                generation: 2
            })
        );
        let resolved: Vec<&str> = joint
            .resolves
            .iter()
            .map(|p| p.subject.0.as_str())
            .collect();
        assert_eq!(resolved, [mo().as_str(), so().as_str()]);
        assert_eq!(joint.situation, "MO/1 fulfils SO/1");
        assert_eq!(joint.opened_at.as_deref(), Some("900"));
        assert_eq!(
            joint
                .slate
                .as_ref()
                .and_then(|s| s.recommended())
                .map(|o| &o.plan),
            Some(&expedite()),
            "planned by the joint slate, not a member's earlier one"
        );
        let single = &entries[1];
        assert_eq!(
            joint.code,
            display_code(&joint.id.0),
            "the served display code"
        );
        assert_ne!(joint.code, single.code);
        assert_eq!(single.portfolio, None);
        assert_eq!(single.members, [lone()]);
        assert_eq!(single.situation, "short");
        assert_eq!(single.opened_at.as_deref(), Some("500"));
    }

    #[test]
    fn a_portfolio_formed_with_coupling_facts_is_worded_as_sentences() {
        let mut portfolios = active(Some("900"));
        let rec = portfolios.values_mut().next().expect("one portfolio");
        let formed = rec.audit.last_mut().expect("the formation");
        formed.reason = format!("linked: {} fulfils {}", mo(), so());
        formed.links = vec![ekg_domain::CouplingLink {
            a: EntityUri(mo()),
            b: EntityUri(so()),
            reason: formed.reason.clone(),
            facts: BTreeMap::from([("kind".to_owned(), "fulfills".to_owned())]),
        }];
        let entries = plan_entries(&records(), &portfolios);
        assert_eq!(entries[0].situation, format!("{} builds {}.", mo(), so()));
        let rec = portfolios.values().next().expect("one portfolio");
        assert_eq!(
            rec.audit.last().map(|e| e.reason.clone()),
            Some(format!("linked: {} fulfils {}", mo(), so())),
            "the audit keeps its exact reason"
        );
    }

    #[test]
    fn the_plan_id_is_reproducible() {
        let members = [EntityUri(so()), EntityUri(mo())];
        let portfolio = portfolio_id(&members).0;
        let plan = plan_id(&members);
        assert_eq!(
            plan.strip_prefix("plan/"),
            portfolio.strip_prefix("portfolio/"),
            "a portfolio's plan id shares its digest"
        );
        // A single point's plan id is its own digest.
        let point = ekg_domain::ActionPoint {
            id: ekg_domain::ActionPointId("ap".to_owned()),
            subject: EntityUri(lone()),
            state: ActionPointState::Proposed,
            reason: "r".to_owned(),
            proposal: None,
        };
        let view = ActionPointView::from(&point);
        assert_eq!(view.digest, short_digest(lone().as_bytes()));
        assert_eq!(
            plan_id(&[EntityUri(lone())]),
            format!("plan/{}", view.digest)
        );
    }

    #[test]
    fn a_portfolio_that_is_not_active_is_not_listed_and_its_points_stand_alone() {
        for state in [PortfolioState::Superseded, PortfolioState::Dismissed] {
            let mut portfolios = active(Some("900"));
            for rec in portfolios.values_mut() {
                rec.state = state;
            }
            let entries = plan_entries(&records(), &portfolios);
            assert!(
                entries.iter().all(|e| e.portfolio.is_none()),
                "{state:?} is not listed"
            );
            assert_eq!(entries.len(), 3, "every awaiting point stands alone");
            let so_entry = entries
                .iter()
                .find(|e| e.members == [so()])
                .expect("the SO alone");
            assert_eq!(
                so_entry
                    .slate
                    .as_ref()
                    .and_then(|s| s.recommended())
                    .map(|o| &o.plan),
                Some(&notify_plan(&so())),
                "planned by its own slate again"
            );
        }
    }

    #[test]
    fn a_pending_plan_is_listed_and_ordered_after_the_timed_ones() {
        // The joint slate on record is for generation 1: the portfolio is pending.
        let (id, mut pending) = portfolio(&[&mo(), &so()], 1, expedite());
        pending.slate_attached_at = Some("999".to_owned());
        let entries = plan_entries(&records(), &BTreeMap::from([(id, pending)]));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].members, [lone()], "the timed plan first");
        assert!(entries[1].slate.is_none(), "pending: no plan");
        assert_eq!(
            entries[1].opened_at, None,
            "no time without a current slate"
        );
    }

    #[test]
    fn newer_plans_list_first_then_untimed_by_id() {
        let other = ontology::entity_iri("sale.order", 8);
        let third = ontology::entity_iri("sale.order", 7);
        let recs = vec![
            stamped(
                record(&lone(), ActionPointState::Proposed, Some(expedite())),
                "100",
            ),
            stamped(
                record(&other, ActionPointState::Open, Some(expedite())),
                "2000",
            ),
            record(&third, ActionPointState::UnderReview, Some(expedite())),
            record(&so(), ActionPointState::Proposed, Some(expedite())),
        ];
        let entries = plan_entries(&recs, &BTreeMap::new());
        let untimed = {
            let mut ids = vec![
                plan_id(&[EntityUri(third.clone())]),
                plan_id(&[EntityUri(so())]),
            ];
            ids.sort();
            ids
        };
        assert_eq!(
            ids(&entries),
            [
                plan_id(&[EntityUri(other)]).as_str(),
                plan_id(&[EntityUri(lone())]).as_str(),
                untimed[0].as_str(),
                untimed[1].as_str(),
            ]
        );
    }

    #[test]
    fn resolved_points_are_not_listed() {
        let recs = vec![
            record(&so(), ActionPointState::Promoted, Some(expedite())),
            record(&lone(), ActionPointState::Aborted, Some(expedite())),
        ];
        assert!(plan_entries(&recs, &BTreeMap::new()).is_empty());
    }

    /// A query root serving fixed entries, so an entry's `plan` resolver runs for real.
    struct Fixed(Vec<PlanEntry>);

    #[Object]
    impl Fixed {
        async fn plans(&self) -> Vec<PlanEntry> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn one_unreadable_plan_does_not_fail_the_list() {
        let recs = vec![
            record(&so(), ActionPointState::Proposed, Some(expedite())),
            record("not an iri", ActionPointState::Proposed, Some(expedite())),
        ];
        let entries = plan_entries(&recs, &BTreeMap::new());
        assert_eq!(entries.len(), 2);
        let schema = Schema::build(Fixed(entries), EmptyMutation, EmptySubscription)
            .data(canned_state())
            .finish();
        let resp = schema
            .execute("{ plans { id plan { lead resolves options { rankKey } } } }")
            .await;
        assert_eq!(resp.errors.len(), 1, "{:?}", resp.errors);
        let path = format!("{:?}", resp.errors[0].path);
        assert!(
            path.contains("plan"),
            "the error is on one entry's plan: {path}"
        );
        let data = resp.data.into_json().expect("json");
        let plans = data["plans"].as_array().expect("listed");
        assert_eq!(plans.len(), 2, "both entries are listed: {data}");
        assert_eq!(
            plans.iter().filter(|p| p["plan"].is_null()).count(),
            1,
            "only the unreadable entry has no plan: {data}"
        );
        assert!(
            plans
                .iter()
                .any(|p| p["plan"]["lead"].as_str() == Some(so().as_str())),
            "{data}"
        );
    }

    // --- the member plan read ---

    #[tokio::test]
    async fn a_members_plan_is_its_portfolios_joint_slate() {
        let (_, rec) = member(&so(), Some(notify_plan(&so())));
        let portfolios = active(Some("900"));
        let joint = portfolios.values().next().expect("one");
        let plan = point_plan(&canned_state(), &so(), &rec, Some(joint))
            .await
            .expect("read")
            .expect("the joint slate");
        let resolves: Vec<&str> = plan.resolves.iter().map(|id| id.0.as_str()).collect();
        assert_eq!(resolves, [mo().as_str(), so().as_str()], "every member");
        assert_eq!(plan.subject.0, so(), "read for the member asked");
        assert_eq!(plan.options[0].plan, expedite(), "the joint option");
    }

    #[tokio::test]
    async fn a_member_awaiting_its_joint_slate_has_no_plan_yet() {
        let (_, rec) = member(&so(), Some(notify_plan(&so())));
        let (_, pending) = portfolio(&[&mo(), &so()], 1, expedite());
        assert!(
            point_plan(&canned_state(), &so(), &rec, Some(&pending))
                .await
                .expect("read")
                .is_none(),
            "never the member's pre-join slate"
        );
        // Once its portfolio is superseded, the point's own slate plans it.
        let mut superseded = pending;
        superseded.state = PortfolioState::Superseded;
        let own = point_plan(&canned_state(), &so(), &rec, Some(&superseded))
            .await
            .expect("read")
            .expect("its own slate");
        assert_eq!(own.options[0].plan, notify_plan(&so()));
        assert_eq!(own.resolves.len(), 1);
    }

    // --- framing through the lead point ---

    #[tokio::test]
    async fn a_multi_point_plan_is_framed_through_its_lead_point() {
        let (_, joint) = portfolio(&[&mo(), &so()], 2, expedite());
        let slate = joint.slate.expect("slate");
        let plan = build_plan(&canned_state(), &slate, None, &[mo(), so()])
            .await
            .expect("built");
        // The SO is 24 days late; the MO has no known slip in the canned world.
        assert_eq!(plan.lead.0, so());
        assert_eq!(plan.subject.0, so(), "a listed plan is read for its lead");
        let slip = plan
            .baseline
            .metrics
            .iter()
            .find(|m| m.key == "day_slip")
            .map(|m| m.value.as_str());
        assert_eq!(slip, Some("24"), "the lead's baseline framing");
        let card = &plan.options[0];
        assert_eq!(
            card.subject,
            so(),
            "options are framed through the lead too"
        );
        assert_eq!(card.members, [mo(), so()], "the lanes span every member");
    }

    // --- the approved plans ---

    const WINDOW: u64 = 100;

    fn receipt(n: i64) -> ExecutionReceipt {
        ExecutionReceipt {
            action: ProposedAction {
                action_type: ActionType("expedite_po".to_owned()),
                target: EntityUri(ontology::entity_iri("purchase.order.line", n)),
                params: BTreeMap::new(),
            },
            external_ref: format!("odoo://{n}"),
            source_revision: Revision(1),
            reversible: true,
        }
    }

    fn promoted_point(
        subject: &str,
        reviewer: &str,
        at: &str,
        via: Option<PromotedVia>,
    ) -> (String, ActionPointRecord) {
        let (key, mut rec) = record(subject, ActionPointState::Promoted, Some(expedite()));
        rec.promotion = Some(Promotion {
            action_point: ekg_domain::ActionPointId(format!("action-point/{subject}")),
            correlation_id: ekg_domain::CorrelationId("c".to_owned()),
            reviewer: ReviewerId(reviewer.to_owned()),
            rationale: "why".to_owned(),
            context_seen: "saw it; promoted slate option rank_key=00001~abc".to_owned(),
        });
        rec.execution_plan = Some(ExecutionPlan {
            actions: Vec::new(),
            receipts: vec![receipt(1), receipt(2)],
        });
        rec.promoted_at = Some(at.to_owned());
        rec.promoted_via = via;
        (key, rec)
    }

    fn promoted_portfolio(at: &str, reversed: &[usize]) -> BTreeMap<String, PortfolioRecord> {
        let mut portfolios = active(Some("900"));
        for rec in portfolios.values_mut() {
            rec.state = PortfolioState::Promoted;
            rec.promotion = Some(PortfolioPromotion {
                generation: 2,
                rank_key: "A".to_owned(),
                reviewer: ReviewerId("planner-1".to_owned()),
                rationale: "approve plan A".to_owned(),
                execution_plan: ExecutionPlan {
                    actions: Vec::new(),
                    receipts: vec![receipt(1), receipt(2)],
                },
                promoted_at: at.to_owned(),
                progress: ReverseProgress {
                    reversed: reversed.iter().copied().collect(),
                    refused: Vec::new(),
                },
            });
        }
        portfolios
    }

    fn via() -> PromotedVia {
        PromotedVia {
            portfolio: PortfolioId("portfolio/0123456789ab".to_owned()),
            generation: 2,
            rank_key: "A".to_owned(),
        }
    }

    #[test]
    fn a_promoted_portfolio_is_listed_once_and_not_its_members() {
        let recs = vec![
            promoted_point(&so(), "planner-1", "1000", Some(via())),
            promoted_point(&mo(), "planner-1", "1000", Some(via())),
        ];
        let approved = approved_plans(&recs, &promoted_portfolio("1000", &[]), WINDOW, 900, 1050);
        assert_eq!(approved.len(), 1, "{approved:?}");
        let plan = &approved[0];
        assert_eq!(plan.id.0, plan_id(&[EntityUri(mo()), EntityUri(so())]));
        assert_eq!(plan.resolves.len(), 2);
        assert_eq!(plan.rank_key.as_deref(), Some("A"));
        assert_eq!(plan.reviewer, "planner-1");
        assert_eq!(plan.promoted_at.as_deref(), Some("1000"));
        assert_eq!(plan.undo_until.as_deref(), Some("1100"));
        assert_eq!(plan.changes.len(), 2);
        assert_eq!(plan.remaining, 2);
    }

    #[test]
    fn a_lone_human_approval_is_listed_and_a_policy_one_is_not() {
        let recs = vec![
            promoted_point(&lone(), "planner-1", "1000", None),
            promoted_point(&so(), "policy:local-autopromote/v1", "1000", None),
        ];
        let approved = approved_plans(&recs, &BTreeMap::new(), WINDOW, 900, 1050);
        assert_eq!(approved.len(), 1, "{approved:?}");
        assert_eq!(approved[0].portfolio, None);
        assert_eq!(approved[0].resolves[0].subject.0, lone());
        assert_eq!(approved[0].rank_key.as_deref(), Some("00001~abc"));
    }

    #[test]
    fn today_and_open_windows_are_listed_and_closed_earlier_ones_are_not() {
        // "Today" starts at 10_000. Approved yesterday, window closed: not listed.
        let closed = vec![promoted_point(&lone(), "planner-1", "9000", None)];
        assert!(approved_plans(&closed, &BTreeMap::new(), WINDOW, 10_000, 10_050).is_empty());
        // Approved yesterday, window still open at now: listed.
        let open = vec![promoted_point(&lone(), "planner-1", "9990", None)];
        assert_eq!(
            approved_plans(&open, &BTreeMap::new(), WINDOW, 10_000, 10_050).len(),
            1
        );
        // Approved today, window closed: still listed today.
        let today = vec![promoted_point(&lone(), "planner-1", "10001", None)];
        assert_eq!(
            approved_plans(&today, &BTreeMap::new(), WINDOW, 10_000, 20_000).len(),
            1
        );
    }

    #[test]
    fn a_partial_undo_says_what_remains_and_a_reversed_plan_is_done() {
        let approved = approved_plans(&[], &promoted_portfolio("1000", &[1]), WINDOW, 900, 1050);
        assert_eq!(approved[0].remaining, 1);
        let mut reversed = promoted_portfolio("1000", &[0, 1]);
        for rec in reversed.values_mut() {
            rec.state = PortfolioState::Reversed;
        }
        assert!(approved_plans(&[], &reversed, WINDOW, 900, 1050).is_empty());
        // A point partly reversed on its own.
        let (key, mut rec) = promoted_point(&lone(), "planner-1", "1000", None);
        rec.reverse_progress.reversed.insert(0);
        let approved = approved_plans(&[(key, rec)], &BTreeMap::new(), WINDOW, 900, 1050);
        assert_eq!(approved[0].remaining, 1);
    }

    #[test]
    fn the_default_day_is_the_utc_day() {
        assert_eq!(utc_day_start(86_400 * 3 + 5), 86_400 * 3);
        assert_eq!(utc_day_start(0), 0);
    }
}
