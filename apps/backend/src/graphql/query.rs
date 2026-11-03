//! The `Query` root, the only root type. Every field is a read that commits nothing: it
//! draws from the durable action-point state and GraphDB projections.

use std::time::{SystemTime, UNIX_EPOCH};

use async_graphql::{Context, ID, Object, Result as GqlResult};
use ekg_dynamic::ActionPointRecord;

use super::model::{
    ActionPointStateView, ActionPointView, AppliedChange, EntityLabel, Metric, PlanView,
    PolicyView, ReadModelView, TabKind, applied_display, change_from_receipt, metrics_from,
};
use super::plans::{self, ApprovedPlan, PlanEntry};
use super::{GraphqlState, projections};

/// The most entity ids one `labels` request may ask about.
pub const MAX_LABEL_IDS: usize = 200;

/// The read-only query root.
pub struct QueryRoot;

#[Object(name = "Query")]
impl QueryRoot {
    /// The action-point list, optionally filtered to one lifecycle `state`. Reads the
    /// durable state via the admin introspection (as the REST list does); commits nothing.
    async fn action_points(
        &self,
        ctx: &Context<'_>,
        state: Option<ActionPointStateView>,
    ) -> GqlResult<Vec<ActionPointView>> {
        let gql = ctx.data::<GraphqlState>()?;
        let mut points: Vec<ActionPointView> = projections::list_records(gql)
            .await?
            .iter()
            .filter_map(|(_, record)| record.point.as_ref().map(ActionPointView::from))
            .collect();
        if let Some(state) = state {
            points.retain(|point| point.state == state);
        }
        Ok(points)
    }

    /// One action point's plan for `subject`: the do-nothing baseline, each ranked option
    /// (its changes, `needs`, opaque display metrics, scope, recommended flag, reasoning-trace
    /// slice), and the slate-level trace. While the point is a member of an active portfolio,
    /// its plan is the portfolio's joint slate for the current generation, resolving every
    /// member; otherwise its own slate. `None` if the subject has no point or its plan is
    /// still pending, never a member's pre-join per-point slate.
    async fn plan(&self, ctx: &Context<'_>, subject: ID) -> GqlResult<Option<PlanView>> {
        let gql = ctx.data::<GraphqlState>()?;
        // Validate and canonicalise the subject before it reaches any read.
        let Some(canonical) = projections::canonical_subject(&subject.0) else {
            return Ok(None);
        };
        let Some(record) = projections::fetch_record(gql, &canonical.0).await? else {
            return Ok(None);
        };
        let portfolio = match &record.portfolio {
            Some(id) => projections::fetch_portfolio(gql, &id.0).await?,
            None => None,
        };
        plans::point_plan(gql, &canonical.0, &record, portfolio.as_ref()).await
    }

    /// The plans awaiting oversight, one inbox row each: the joint plan of every active
    /// portfolio, and the plan of every awaiting point in no active portfolio, newest slate
    /// first. A plan whose slate is pending is listed with no plan; a plan that cannot be
    /// read is reported on its own `plan` field. Commits nothing.
    async fn plans(&self, ctx: &Context<'_>) -> GqlResult<Vec<PlanEntry>> {
        let gql = ctx.data::<GraphqlState>()?;
        let records = projections::list_records(gql).await?;
        let portfolios = projections::list_portfolios(gql).await?;
        Ok(plans::plan_entries(&records, &portfolios))
    }

    /// The plans approved by a human: promoted portfolios, and points a human promoted on
    /// their own, approved since `since` (Unix epoch seconds, the start of the operator's
    /// local day; the server's UTC day when omitted) or with their reversal window still
    /// open. Each carries when its undo window closes. Commits nothing.
    async fn approved_plans(
        &self,
        ctx: &Context<'_>,
        since: Option<String>,
    ) -> GqlResult<Vec<ApprovedPlan>> {
        let gql = ctx.data::<GraphqlState>()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let since = match since {
            Some(since) => since
                .trim()
                .parse::<u64>()
                .map_err(|_| async_graphql::Error::new("since must be Unix epoch seconds"))?,
            None => plans::utc_day_start(now),
        };
        let records = projections::list_records(gql).await?;
        let portfolios = projections::list_portfolios(gql).await?;
        Ok(plans::approved_plans(
            &records,
            &portfolios,
            gql.policy.undo_window.as_secs(),
            since,
            now,
        ))
    }

    /// A read-model tab projection (Timeline / Graph / Flow / Suppliers): the plant-wide
    /// picture, in which `subject` only selects which path, risk and cause markers are lit.
    /// Non-authoritative: reading it mutates nothing. An ill-formed subject yields no rows;
    /// a well-formed subject the world does not know yields the plant-wide rows with nothing
    /// lit. Neither is an error.
    async fn read_model(
        &self,
        ctx: &Context<'_>,
        subject: ID,
        tab: TabKind,
    ) -> GqlResult<ReadModelView> {
        let gql = ctx.data::<GraphqlState>()?;
        let rows = match projections::canonical_subject(&subject.0) {
            Some(canonical) => projections::read_model(gql, &canonical, tab).await?,
            None => Vec::new(),
        };
        Ok(ReadModelView { subject, tab, rows })
    }

    /// The changes an agent auto-applied under policy, each with the criteria it
    /// satisfied and the window within which it may still be reversed. An
    /// auto-applied change is a promoted plan whose promoter is the policy (reviewer
    /// `policy:<id>`), still in the `promoted` state (a reversed one is done). The changes
    /// carry their recorded inverse, so the governed undo can reverse them.
    async fn auto_applied(&self, ctx: &Context<'_>) -> GqlResult<Vec<AppliedChange>> {
        let gql = ctx.data::<GraphqlState>()?;
        let criteria = projections::policy_criteria(&gql.policy);
        let window = gql.policy.undo_window.as_secs();
        let mut applied = Vec::new();
        for (subject, record) in projections::list_records(gql).await? {
            if !is_auto_applied(&record) {
                continue;
            }
            let Some(plan) = &record.execution_plan else {
                continue;
            };
            let action_point = record
                .point
                .as_ref()
                .map_or_else(|| subject.clone(), |point| point.id.0.clone());
            let undo_until = record
                .promoted_at
                .as_deref()
                .and_then(|at| at.parse::<u64>().ok())
                .map(|at| (at + window).to_string());
            let changes: Vec<_> = plan.receipts.iter().map(change_from_receipt).collect();
            let metrics = applied_display(&changes);
            applied.push(AppliedChange {
                code: super::model::display_code(&format!("applied/{action_point}")),
                action_point_code: super::model::display_code(&action_point),
                action_point: ID(action_point),
                subject: ID(subject),
                changes,
                criteria: criteria.clone(),
                applied_at: record.promoted_at.clone(),
                undo_until,
                reversible: plan.receipts.iter().all(|r| r.reversible),
                metrics,
            });
        }
        Ok(applied)
    }

    /// The inbox's headline metrics as an opaque, capability-filled KPI list: late orders
    /// now and if approved (`late_orders_now` / `late_orders`), and `revenue_at_risk` with its
    /// `currency`. Each is taken over the points awaiting oversight and judged by the
    /// capability. A metric is true or absent: a count or sum that cannot be read is omitted,
    /// never served as zero. The keys are opaque, the contract does not enumerate them, so
    /// the base stays capability-neutral. A non-authoritative read: computing it commits
    /// nothing.
    async fn headline_metrics(&self, ctx: &Context<'_>) -> GqlResult<Vec<Metric>> {
        let gql = ctx.data::<GraphqlState>()?;
        let records = projections::list_records(gql).await?;
        // A portfolio member is judged through its portfolio's joint slate, so the
        // portfolio records are read beside the points.
        let portfolios = projections::list_portfolios(gql).await?;
        Ok(metrics_from(
            &projections::headline_metrics(gql, &records, &portfolios).await,
        ))
    }

    /// The display labels of the entity ids a client shows: for each id the world can name,
    /// the label the capability composes from the materialized names, in request order and
    /// each id once. An unnamed, foreign or non-canonical id gets no label, never a guessed
    /// one. At most 200 ids per request. A display projection: served texts keep their ids,
    /// and reading labels commits nothing.
    async fn labels(&self, ctx: &Context<'_>, ids: Vec<ID>) -> GqlResult<Vec<EntityLabel>> {
        if ids.len() > MAX_LABEL_IDS {
            return Err(async_graphql::Error::new(format!(
                "labels asks about at most {MAX_LABEL_IDS} ids, got {}",
                ids.len()
            )));
        }
        let gql = ctx.data::<GraphqlState>()?;
        let ids: Vec<String> = ids.into_iter().map(|id| id.0).collect();
        Ok(projections::entity_labels(gql, &ids)
            .await?
            .into_iter()
            .map(|(id, label)| EntityLabel {
                id: ID(id.0),
                label,
            })
            .collect())
    }

    /// The active auto-apply policy as the operator sees it: the criteria a change must
    /// meet, the reversal window, and the pre-authorized verbs.
    async fn policy(&self, ctx: &Context<'_>) -> GqlResult<PolicyView> {
        let gql = ctx.data::<GraphqlState>()?;
        let secs = gql.policy.undo_window.as_secs();
        Ok(PolicyView {
            policy_id: gql.policy.policy_id.clone(),
            criteria: projections::policy_criteria(&gql.policy),
            cost_limit_currency: gql.policy.cost_limit_currency.clone(),
            undo_window: format!("reversible for {}h after apply", secs / 3600),
            auto_apply_actions: gql.policy.auto_apply_actions.clone(),
        })
    }
}

/// Whether a durable record is an auto-applied change still open to undo: promoted, by the
/// policy, with an executed plan recorded. Shares the promoter predicate with the `undo`
/// command ([`crate::action_point::promoted_by_policy`]) so the list and undo cannot drift.
fn is_auto_applied(record: &ActionPointRecord) -> bool {
    let promoted = record
        .point
        .as_ref()
        .is_some_and(|point| point.state == ekg_domain::ActionPointState::Promoted);
    promoted && crate::action_point::promoted_by_policy(record) && record.execution_plan.is_some()
}
