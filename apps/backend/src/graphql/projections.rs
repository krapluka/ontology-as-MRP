//! The read-side plumbing behind the resolvers: reads of Restate durable state and of
//! GraphDB over SPARQL, and the read-only derivations on top (display metrics, lanes,
//! tabs). Nothing here commits.
//!
//! - Durable state: the action-point list via the admin `/query` introspection (as the
//!   REST `list` does), and one subject's record or slate via the ingress object handlers.
//! - SPARQL projections: the four read-model tabs (projected by `ekg_semantic::tabs`,
//!   marked from the monitor's own rule verdicts), the before/after work-center lanes, and
//!   the derived delivery and cost display metrics, read from the world model or a what-if
//!   projection.
//!
//! Action-point subjects originate from client input, so every subject id is validated and
//! rebuilt canonically (`ekg_semantic::ontology::canonical_subject`) before it is
//! interpolated into a SPARQL query.

use std::collections::{BTreeMap, BTreeSet};

use async_graphql::{Error as GqlError, Result as GqlResult};
use ekg_domain::{EntityUri, Plan, Slate};
use ekg_dynamic::{ActionPointRecord, PortfolioRecord, PortfolioState};
use ekg_kinetic::ATTR_COST;
use ekg_semantic::display::{self, WorldSource};
use ekg_semantic::ontology;
use ekg_semantic::tabs::{self, Tab};
use serde_json::Value;

use super::model::{Bar, Field, Lane, ProjectionRow, TabKind};
use super::{GraphqlState, PolicyDisplay};

// --- subject validation ---

/// The subject guard lives with the IRI scheme in `ekg_semantic::ontology` (so the tab
/// queries there interpolate only canonical IRIs too); re-exported for the resolvers.
pub(crate) use ekg_semantic::ontology::canonical_subject;

// --- durable-state reads ---

/// Builds an ingress URL for an `ActionPointService` handler, percent-encoding the subject
/// key (mirrors the REST `object_url`, so the subject's slashes stay one key segment).
fn object_url(ingress: &str, subject: &str, handler: &str) -> GqlResult<String> {
    service_url(ingress, "ActionPointService", subject, handler)
}

/// Builds an ingress URL for `service`'s object `key` and `handler`, percent-encoding the key
/// so its slashes stay one segment.
fn service_url(ingress: &str, service: &str, key: &str, handler: &str) -> GqlResult<String> {
    let mut url =
        reqwest::Url::parse(ingress).map_err(|e| GqlError::new(format!("bad ingress url: {e}")))?;
    url.path_segments_mut()
        .map_err(|()| GqlError::new("ingress url cannot be a base"))?
        .pop_if_empty()
        .extend([service, key, handler]);
    Ok(url.into())
}

/// Reads one subject's whole durable [`ActionPointRecord`] via the ingress `record` handler.
pub(crate) async fn fetch_record(
    state: &GraphqlState,
    subject: &str,
) -> GqlResult<Option<ActionPointRecord>> {
    let url = object_url(&state.ingress_url, subject, "record")?;
    let text = post_text(state, &url).await?;
    serde_json::from_str::<Option<ActionPointRecord>>(&text)
        .map_err(|e| GqlError::new(format!("undecodable action-point record: {e}")))
}

/// Reads one portfolio's durable [`PortfolioRecord`] via the ingress `record` handler (`None`
/// when it was never formed).
pub(crate) async fn fetch_portfolio(
    state: &GraphqlState,
    id: &str,
) -> GqlResult<Option<PortfolioRecord>> {
    let url = service_url(&state.ingress_url, "PortfolioService", id, "record")?;
    let text = post_text(state, &url).await?;
    serde_json::from_str::<Option<PortfolioRecord>>(&text)
        .map_err(|e| GqlError::new(format!("undecodable portfolio record: {e}")))
}

/// Lists every subject's durable [`ActionPointRecord`] via the admin `/query` state
/// introspection (the same read path the REST `list` uses), paired with its subject key.
pub(crate) async fn list_records(
    state: &GraphqlState,
) -> GqlResult<Vec<(String, ActionPointRecord)>> {
    list_state(state, "ActionPointService").await
}

/// Every portfolio's durable [`PortfolioRecord`], keyed by its id, via the same admin state
/// introspection.
pub(crate) async fn list_portfolios(
    state: &GraphqlState,
) -> GqlResult<BTreeMap<String, PortfolioRecord>> {
    Ok(list_state(state, "PortfolioService")
        .await?
        .into_iter()
        .collect())
}

/// Every `record` of `service`'s objects via the admin `/query` state introspection, paired
/// with its key. A record that does not decode is skipped.
async fn list_state<T: serde::de::DeserializeOwned>(
    state: &GraphqlState,
    service: &str,
) -> GqlResult<Vec<(String, T)>> {
    let query = format!(
        "SELECT service_key, value_utf8 FROM state \
         WHERE service_name = '{service}' AND key = 'record'"
    );
    let resp = state
        .http
        .post(format!("{}/query", state.admin_url.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(serde_json::json!({ "query": query }).to_string())
        .send()
        .await
        .map_err(|e| GqlError::new(format!("restate admin unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err(GqlError::new(format!(
            "restate admin /query returned {}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| GqlError::new(e.to_string()))?;
    let parsed: Value = serde_json::from_str(&text).map_err(|e| GqlError::new(e.to_string()))?;
    let mut out = Vec::new();
    for row in parsed
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = row
            .get("service_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(value) = row.get("value_utf8").and_then(Value::as_str)
            && let Ok(record) = serde_json::from_str::<T>(value)
        {
            out.push((key.to_owned(), record));
        }
    }
    Ok(out)
}

/// POSTs to an ingress object-handler URL (no body) and returns the response text.
async fn post_text(state: &GraphqlState, url: &str) -> GqlResult<String> {
    let resp = state
        .http
        .post(url)
        .send()
        .await
        .map_err(|e| GqlError::new(format!("restate ingress unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err(GqlError::new(format!(
            "action-point read returned {}",
            resp.status()
        )));
    }
    resp.text().await.map_err(|e| GqlError::new(e.to_string()))
}

// --- read-model tabs ---

/// Projects a read-model tab for `subject` as neutral rows. The tab logic lives in
/// `ekg_semantic::tabs`; this layer supplies the at-risk set and maps the rows. The at-risk
/// set is the monitor's own rule set evaluated through [`ekg_dynamic::monitor::verdicts`],
/// so a tab shows what the monitor would decide at read time. When the rules cannot be
/// evaluated the tab is still served with every risk marker omitted, which is not the same
/// as marking the rows not at risk. `subject` must already be validated.
pub(crate) async fn read_model(
    state: &GraphqlState,
    subject: &EntityUri,
    tab: TabKind,
) -> GqlResult<Vec<ProjectionRow>> {
    let at_risk: Option<BTreeSet<String>> = match ekg_dynamic::monitor::verdicts(
        state.world.as_ref(),
        &state.rules,
    )
    .await
    {
        Ok(verdicts) => Some(verdicts.into_iter().map(|v| v.subject.0).collect()),
        Err(err) => {
            eprintln!(
                "warning: read-model {tab:?}: the drift rules could not be evaluated, risk markers \
                 omitted ({err})"
            );
            None
        }
    };
    let projection = tabs::project(
        state.world.as_ref(),
        subject,
        semantic_tab(tab),
        at_risk.as_ref(),
    )
    .await
    .map_err(|e| GqlError::new(format!("read-model projection failed: {e}")))?;
    for warning in &projection.warnings {
        eprintln!("warning: read-model {tab:?}: {warning}");
    }
    Ok(projection.rows.iter().map(row_to_projection).collect())
}

/// The semantic layer's tab for a GraphQL tab kind.
fn semantic_tab(tab: TabKind) -> Tab {
    match tab {
        TabKind::Timeline => Tab::Timeline,
        TabKind::Graph => Tab::Graph,
        TabKind::Flow => Tab::Flow,
        TabKind::Suppliers => Tab::Suppliers,
    }
}

/// Maps one SPARQL solution row to a neutral [`ProjectionRow`] (key-ordered by `BTreeMap`).
fn row_to_projection(row: &BTreeMap<String, String>) -> ProjectionRow {
    ProjectionRow {
        fields: row
            .iter()
            .map(|(key, value)| Field {
                key: key.clone(),
                value: value.clone(),
            })
            .collect(),
    }
}

// --- derived display metrics ---

/// Derives an option's display metrics. The judgments are the capability's
/// (`ekg_semantic::display`); this layer only supplies the reads:
///
/// - the plan framing over one what-if projection of the plan against the current world:
///   `delivery`, `projected_completion` and `day_slip` against the original promise, and the
///   order's `order_value` with its `currency`;
/// - the plan cost with its `cost_currency`, from the purchase-order costs the policy-context
///   gatherer reads (the same costs the auto-apply predicate judges).
///
/// A key that cannot be read is omitted; a display metric never fails the query. The caller
/// only fills a bag key the scorer left unset, so the ranking (`rank_key`) is unaffected.
pub(crate) async fn derive_display_metrics(
    state: &GraphqlState,
    plan: &Plan,
    subject: &str,
) -> GqlResult<BTreeMap<String, String>> {
    let mut derived = BTreeMap::new();
    if let Ok(ctx) = crate::policy_context::gather(state.world.as_ref(), plan).await {
        let costs: Vec<(EntityUri, String)> = plan
            .actions
            .iter()
            .filter_map(|action| {
                ctx.get(&action.target, ATTR_COST)
                    .map(|cost| (action.target.clone(), cost.to_owned()))
            })
            .collect();
        derived.extend(display::plan_cost(state.world.as_ref(), &costs).await);
    }
    let Some(subject) = canonical_subject(subject) else {
        return Ok(derived);
    };
    let world = WorldSource(state.world.as_ref());
    match state.simulator.project(plan).await {
        Ok(projection) => {
            derived.extend(display::plan_framing(&world, projection.as_ref(), &subject).await);
        }
        // No what-if: the projected keys are omitted, but the order's value and currency are
        // current-world facts and are still served.
        Err(err) => {
            eprintln!("warning: display metrics: the option could not be projected: {err}");
            derived.extend(display::order_framing(&world, &subject).await);
        }
    }
    Ok(derived)
}

/// The do-nothing baseline's display framing: the same framing an option carries, judged on
/// the current world as it stands, so no projection is needed. Empty for an ill-formed subject.
pub(crate) async fn baseline_framing(
    state: &GraphqlState,
    subject: &str,
) -> BTreeMap<String, String> {
    let Some(subject) = canonical_subject(subject) else {
        return BTreeMap::new();
    };
    let world = WorldSource(state.world.as_ref());
    display::plan_framing(&world, &world, &subject).await
}

/// The display labels of `ids`, composed by the capability from the materialized names: in
/// request order, each id once, only the ids the world can name. A foreign or non-canonical id
/// gets no label and is never interpolated.
///
/// # Errors
/// A world-model read failure.
pub(crate) async fn entity_labels(
    state: &GraphqlState,
    ids: &[String],
) -> GqlResult<Vec<(EntityUri, String)>> {
    display::entity_labels(state.world.as_ref(), ids)
        .await
        .map_err(|e| GqlError::new(format!("labels unavailable: {e}")))
}

/// A change object's display label, composed by the capability from the materialized names.
/// `None` when the object is ill-formed or unnamed, or the read fails.
pub(crate) async fn change_label(state: &GraphqlState, target: &str) -> Option<String> {
    let target = canonical_subject(target)?;
    display::change_label(state.world.as_ref(), &target)
        .await
        .ok()
        .flatten()
}

/// A change's "from": the current value of the fact its verb moves, as the capability reads
/// it (an expedite's from is its line's arrival). `None` when the target is ill-formed, the
/// verb moves no fact, the fact is unset, or the read fails.
pub(crate) async fn change_from(state: &GraphqlState, target: &str, verb: &str) -> Option<String> {
    let target = canonical_subject(target)?;
    display::change_from(state.world.as_ref(), verb, &target)
        .await
        .ok()
        .flatten()
}

// --- the joint-slate rule ---

/// The point's portfolio when it is active: `portfolio` is the record `record.portfolio`
/// names (the caller looked it up), and it counts only while it is active. A point in no
/// active portfolio is planned on its own.
pub(crate) fn active_portfolio<'a>(
    record: &ActionPointRecord,
    portfolio: Option<&'a PortfolioRecord>,
) -> Option<&'a PortfolioRecord> {
    let named = record.portfolio.as_ref()?;
    portfolio.filter(|portfolio| {
        portfolio.state == PortfolioState::Active && portfolio.id.as_ref() == Some(named)
    })
}

/// An active portfolio's current joint slate: the one attached for its current
/// generation. `None` while it is pending (a slate of an earlier generation is never current).
pub(crate) fn joint_slate(portfolio: &PortfolioRecord) -> Option<&Slate> {
    portfolio
        .slate
        .as_ref()
        .filter(|_| portfolio.slate_generation == Some(portfolio.generation))
}

/// The slate that plans a point, in every read: while the point is a member of an active
/// portfolio, the portfolio's joint slate for its current generation (none while it is
/// pending, and never the per-point slate the member carried from before it joined);
/// otherwise the point's own slate.
pub(crate) fn point_slate<'a>(
    record: &'a ActionPointRecord,
    portfolio: Option<&'a PortfolioRecord>,
) -> Option<&'a Slate> {
    match active_portfolio(record, portfolio) {
        Some(portfolio) => joint_slate(portfolio),
        None => record.slate.as_ref(),
    }
}

/// The points awaiting oversight, each with its plan's recommended option, the plan chosen
/// by [`point_slate`]. A point whose plan is still pending is not listed.
pub(crate) fn awaiting_oversight(
    records: &[(String, ActionPointRecord)],
    portfolios: &BTreeMap<String, PortfolioRecord>,
) -> Vec<display::Awaiting> {
    records
        .iter()
        .filter_map(|(subject, record)| {
            let point = record.point.as_ref()?;
            if !ekg_dynamic::is_pre_promotion(point.state) {
                return None;
            }
            let portfolio = record
                .portfolio
                .as_ref()
                .and_then(|id| portfolios.get(&id.0));
            let slate = point_slate(record, portfolio)?;
            Some(display::Awaiting {
                subject: canonical_subject(subject)?,
                recommended: slate.recommended().map(|option| option.plan.clone()),
            })
        })
        .collect()
}

/// The headline metrics over `records`, judged by the capability over the current world and a
/// what-if projection per recommended option. Factored from the resolver so it runs without
/// Restate.
pub(crate) async fn headline_metrics(
    state: &GraphqlState,
    records: &[(String, ActionPointRecord)],
    portfolios: &BTreeMap<String, PortfolioRecord>,
) -> BTreeMap<String, String> {
    display::headline_metrics(
        state.world.as_ref(),
        state.simulator.as_ref(),
        &awaiting_oversight(records, portfolios),
    )
    .await
}

// --- before/after lanes ---

/// One work-order bar read from a lane query, before row-packing.
#[derive(Clone)]
struct RawBar {
    work_center: String,
    work_order: String,
    start: String,
    end: String,
    capacity: Option<f64>,
}

/// Derives the before/after per-work-center [`Lane`]s for an option: the current world-model
/// schedule ("before") and the schedule under the option's proposed changes ("after", read
/// from a what-if projection). A plan over several points runs one lane query per point and
/// merges the results by work center, so a bar two points share appears once. Only a
/// manufacturing order yields lanes; an ill-formed subject, a non-process subject or an
/// empty plan yields none.
pub(crate) async fn option_lanes(
    state: &GraphqlState,
    subjects: &[String],
    plan: &Plan,
) -> GqlResult<Vec<Lane>> {
    let queries: Vec<String> = subjects
        .iter()
        .filter_map(|subject| canonical_subject(subject))
        .map(|subject| lanes_sparql(&subject))
        .collect();
    if queries.is_empty() {
        return Ok(Vec::new());
    }

    // "Before": the world model as it stands.
    let mut before_rows = Vec::new();
    for sparql in &queries {
        before_rows.extend(
            state
                .world
                .select(sparql)
                .await
                .map_err(|e| GqlError::new(format!("lane read failed: {e}")))?,
        );
    }
    let before = merge_bars(parse_bars(&before_rows));

    // "After": the same queries over one what-if projection of the option's plan.
    let after = match state.simulator.project(plan).await {
        Ok(projection) => {
            let mut rows = Vec::new();
            for sparql in &queries {
                rows.extend(
                    projection
                        .select(sparql)
                        .await
                        .map_err(|e| GqlError::new(format!("projected lane read failed: {e}")))?,
                );
            }
            merge_bars(parse_bars(&rows))
        }
        // Without a projection the "after" mirrors "before": there is no changed schedule
        // to read.
        Err(_) => before.clone(),
    };

    Ok(build_lanes(&before, &after))
}

/// Merges the bars several points' lane queries read: a work order read by two points (they
/// share its work center) keeps its first reading, so it appears once.
fn merge_bars(bars: Vec<RawBar>) -> Vec<RawBar> {
    let mut seen = BTreeSet::new();
    bars.into_iter()
        .filter(|bar| seen.insert((bar.work_center.clone(), bar.work_order.clone())))
        .collect()
}

// --- scope ---

/// An option's scope over the objects its changes target, as the capability counts it
/// (`ekg_semantic::display::plan_scope`). A failed read is logged and yields no scope.
pub(crate) async fn option_scope(state: &GraphqlState, plan: &Plan) -> Vec<display::ScopeCount> {
    let targets = plan.actions.iter().map(|action| action.target.0.as_str());
    match display::plan_scope(state.world.as_ref(), targets).await {
        Ok(scope) => scope,
        Err(err) => {
            eprintln!(
                "warning: plan scope: the change targets could not be read, scope omitted ({err})"
            );
            Vec::new()
        }
    }
}

/// The lane query: every work order on a work centre the subject order uses, with its
/// window and the agent's capacity. Runs unchanged against the world model ("before") and
/// a projection ("after"). `subject` is already canonical.
fn lanes_sparql(subject: &EntityUri) -> String {
    let vocab = ontology::VOCAB_NAMESPACE;
    let s = &subject.0;
    format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?wc ?wo ?start ?end ?capacity WHERE {{\n\
           {{ SELECT DISTINCT ?wc WHERE {{ ?swo ekg:part_of <{s}> ; ekg:performedBy ?wc }} }}\n\
           ?wo ekg:performedBy ?wc ; ekg:startedAt ?start ; ekg:endedAt ?end .\n\
           OPTIONAL {{ ?wc ekg:capacity ?capacity }}\n\
         }} ORDER BY ?wc ?start"
    )
}

/// Reads the raw bars from lane-query rows, skipping any row missing its keying bindings.
fn parse_bars(rows: &[BTreeMap<String, String>]) -> Vec<RawBar> {
    rows.iter()
        .filter_map(|row| {
            Some(RawBar {
                work_center: row.get("wc")?.clone(),
                work_order: row.get("wo")?.clone(),
                start: row.get("start")?.clone(),
                end: row.get("end")?.clone(),
                capacity: row.get("capacity").and_then(|c| c.parse::<f64>().ok()),
            })
        })
        .collect()
}

/// Assembles [`Lane`]s from the before/after bars: groups by work centre, packs overlapping
/// bars into rows, and flags an "after" bar as `changed` when the same work order's window
/// or work centre differs from "before".
fn build_lanes(before: &[RawBar], after: &[RawBar]) -> Vec<Lane> {
    // A work order's "before" position, keyed by work order id, for change detection.
    let before_pos: BTreeMap<&str, (&str, &str, &str)> = before
        .iter()
        .map(|b| {
            (
                b.work_order.as_str(),
                (b.work_center.as_str(), b.start.as_str(), b.end.as_str()),
            )
        })
        .collect();

    // Every work centre that appears in either schedule, ordered.
    let mut centres: Vec<&str> = before
        .iter()
        .chain(after.iter())
        .map(|b| b.work_center.as_str())
        .collect();
    centres.sort_unstable();
    centres.dedup();

    centres
        .into_iter()
        .map(|wc| {
            let capacity = before
                .iter()
                .chain(after.iter())
                .find(|b| b.work_center == wc)
                .and_then(|b| b.capacity);
            let before_bars = pack_rows(before.iter().filter(|b| b.work_center == wc), |_| false);
            let after_bars = pack_rows(after.iter().filter(|b| b.work_center == wc), |b| {
                before_pos.get(b.work_order.as_str()).is_none_or(|pos| {
                    *pos != (b.work_center.as_str(), b.start.as_str(), b.end.as_str())
                })
            });
            Lane {
                work_center: async_graphql::ID(wc.to_owned()),
                capacity,
                before: before_bars,
                after: after_bars,
            }
        })
        .collect()
}

/// Greedily packs a lane's bars into stacking rows (a bar goes in the first row whose last
/// bar ends at or before this bar's start), marking each with `changed` via the predicate.
fn pack_rows<'a>(
    bars: impl Iterator<Item = &'a RawBar>,
    changed: impl Fn(&RawBar) -> bool,
) -> Vec<Bar> {
    let mut sorted: Vec<&RawBar> = bars.collect();
    sorted.sort_by(|a, b| a.start.cmp(&b.start));
    // The end time occupying each row, so a later bar can reuse a freed row.
    let mut row_ends: Vec<String> = Vec::new();
    sorted
        .into_iter()
        .map(|bar| {
            let row = row_ends
                .iter()
                .position(|end| end.as_str() <= bar.start.as_str())
                .unwrap_or_else(|| {
                    row_ends.push(String::new());
                    row_ends.len() - 1
                });
            row_ends[row].clone_from(&bar.end);
            Bar {
                work_order: async_graphql::ID(bar.work_order.clone()),
                start: bar.start.clone(),
                end: bar.end.clone(),
                row: i32::try_from(row).unwrap_or(i32::MAX),
                changed: changed(bar),
            }
        })
        .collect()
}

// --- policy display ---

/// The criteria the auto-apply policy enforces, as the operator sees them.
#[must_use]
pub(crate) fn policy_criteria(policy: &PolicyDisplay) -> Vec<super::model::Criterion> {
    use super::model::Criterion;
    let mut criteria = vec![
        Criterion {
            rule: "cost_within_limit".to_owned(),
            value: policy
                .cost_limit
                .map_or_else(|| "unset".to_owned(), |limit| format!("{limit:.2}")),
        },
        Criterion {
            rule: "no_promised_date_move".to_owned(),
            value: "true".to_owned(),
        },
        Criterion {
            rule: "single_step_reversible".to_owned(),
            value: "true".to_owned(),
        },
    ];
    if !policy.confirmed_states.is_empty() {
        criteria.push(Criterion {
            rule: "supplier_confirmed".to_owned(),
            value: policy.confirmed_states.join(", "),
        });
    }
    criteria
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn build_lanes_flags_a_moved_bar_and_packs_overlaps_into_rows() {
        let wc = "https://ekg.example/id/mrp.workcenter/1".to_owned();
        let raw = |wo: &str, start: &str, end: &str| RawBar {
            work_center: wc.clone(),
            work_order: wo.to_owned(),
            start: start.to_owned(),
            end: end.to_owned(),
            capacity: Some(1.0),
        };
        // Before: two overlapping work orders on one capacity-1 centre (contention).
        let before = vec![
            raw("wo1", "2027-10-11T08:00:00", "2027-10-11T12:00:00"),
            raw("wo2", "2027-10-11T09:00:00", "2027-10-11T13:00:00"),
        ];
        // After: wo2 rescheduled clear of wo1, to 12:00-16:00.
        let after = vec![
            raw("wo1", "2027-10-11T08:00:00", "2027-10-11T12:00:00"),
            raw("wo2", "2027-10-11T12:00:00", "2027-10-11T16:00:00"),
        ];
        let lanes = build_lanes(&before, &after);
        assert_eq!(lanes.len(), 1, "one work centre");
        let lane = &lanes[0];
        assert_eq!(lane.capacity, Some(1.0));
        // Before overlaps, so the second bar stacks to row 1; after is sequential, row 0.
        assert_eq!(lane.before.iter().map(|b| b.row).max(), Some(1));
        assert!(
            lane.after.iter().all(|b| b.row == 0),
            "after packs into one row"
        );
        // wo2 moved, so its "after" bar is `changed`; wo1 is not.
        let wo2 = lane
            .after
            .iter()
            .find(|b| b.work_order.0.ends_with("wo2"))
            .expect("wo2");
        assert!(wo2.changed, "the rescheduled bar is flagged changed");
        let wo1 = lane
            .after
            .iter()
            .find(|b| b.work_order.0.ends_with("wo1"))
            .expect("wo1");
        assert!(!wo1.changed, "the untouched bar is not flagged");
    }

    /// A world (and its what-if) answering each manufacturing order's lane query from canned
    /// bars: MO/1 runs wo1 on WC1 and wo2 on WC2; MO/2 runs wo3 on WC1, so WC1's lane holds
    /// wo1 and wo3 whichever order it is read for. Any other subject has no work orders.
    struct LaneWorld;

    impl LaneWorld {
        fn answer(sparql: &str) -> Rows {
            let wc = |n: i64| ontology::entity_iri("mrp.workcenter", n);
            let bar = |w: i64, wo: i64, start: &str, end: &str| {
                pairs(&[
                    ("wc", &wc(w)),
                    ("wo", &ontology::entity_iri("mrp.workorder", wo)),
                    ("start", start),
                    ("end", end),
                    ("capacity", "1"),
                ])
            };
            let wc1 = vec![
                bar(1, 1, "2027-10-19T08:00:00", "2027-10-19T12:00:00"),
                bar(1, 3, "2027-10-20T08:00:00", "2027-10-20T12:00:00"),
            ];
            if sparql.contains(&format!("<{}>", ontology::entity_iri("mrp.production", 1))) {
                let mut rows = wc1;
                rows.push(bar(2, 2, "2027-10-21T08:00:00", "2027-10-21T12:00:00"));
                return rows;
            }
            if sparql.contains(&format!("<{}>", ontology::entity_iri("mrp.production", 2))) {
                return wc1;
            }
            Vec::new()
        }
    }

    #[async_trait]
    impl WorldModel for LaneWorld {
        async fn materialize(&self, _record: &SourceRecord) -> DomainResult<Revision> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn retract(&self, _entity: &EntityUri) -> DomainResult<Revision> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> DomainResult<Grounding> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn select(&self, sparql: &str) -> DomainResult<Rows> {
            Ok(Self::answer(sparql))
        }
    }

    #[async_trait]
    impl Projection for LaneWorld {
        async fn select(&self, sparql: &str) -> DomainResult<Rows> {
            Ok(Self::answer(sparql))
        }
    }

    #[async_trait]
    impl Simulator for LaneWorld {
        async fn project(&self, _plan: &Plan) -> DomainResult<Box<dyn Projection>> {
            Ok(Box::new(Self))
        }
    }

    fn lane_state() -> GraphqlState {
        GraphqlState {
            world: Arc::new(LaneWorld),
            simulator: Arc::new(LaneWorld),
            ..canned_state()
        }
    }

    fn bars(lane: &Lane) -> Vec<String> {
        lane.before
            .iter()
            .map(|bar| bar.work_order.0.clone())
            .collect()
    }

    #[tokio::test]
    async fn two_manufacturing_members_merge_their_lanes_with_no_duplicate_bar() {
        let members = [
            ontology::entity_iri("mrp.production", 1),
            ontology::entity_iri("mrp.production", 2),
        ];
        let lanes = option_lanes(&lane_state(), &members, &expedite())
            .await
            .expect("lanes");
        let centers: Vec<&str> = lanes.iter().map(|l| l.work_center.0.as_str()).collect();
        assert_eq!(
            centers,
            [
                ontology::entity_iri("mrp.workcenter", 1),
                ontology::entity_iri("mrp.workcenter", 2)
            ]
        );
        assert_eq!(
            bars(&lanes[0]),
            [
                ontology::entity_iri("mrp.workorder", 1),
                ontology::entity_iri("mrp.workorder", 3)
            ],
            "the shared work center's bars appear once"
        );
        assert_eq!(lanes[0].after.len(), 2, "after merges the same way");
    }

    #[tokio::test]
    async fn a_sales_order_member_adds_no_lane_to_its_manufacturing_peers() {
        let mo = ontology::entity_iri("mrp.production", 2);
        let alone = option_lanes(&lane_state(), std::slice::from_ref(&mo), &expedite())
            .await
            .expect("lanes");
        let with_order = option_lanes(&lane_state(), &[mo, so()], &expedite())
            .await
            .expect("lanes");
        assert_eq!(with_order.len(), alone.len());
        assert_eq!(
            bars(&with_order[0]),
            bars(&alone[0]),
            "the MO's lanes, unchanged"
        );
        // Only non-process or ill-formed subjects: no lanes.
        assert!(
            option_lanes(&lane_state(), &[so(), "not an iri".to_owned()], &expedite())
                .await
                .expect("lanes")
                .is_empty()
        );
    }

    #[test]
    fn the_simulator_mirrors_the_kinetic_field_writes_of_every_verb() {
        // The what-if projection writes on the same target model and the same fields as the
        // governed write-back, for every verb in the catalog, including both the start and
        // the finish of a reschedule.
        for verb in ekg_kinetic::Verb::ALL {
            let simulated = ekg_semantic::simulator::verb_write(verb.name()).map(|w| {
                (
                    w.model,
                    w.fields.iter().map(|(field, _)| *field).collect::<Vec<_>>(),
                )
            });
            let kinetic = verb
                .field_write()
                .map(|(model, _)| (model, verb.target_fields().to_vec()));
            assert_eq!(simulated, kinetic, "{}", verb.name());
        }
    }

    #[test]
    fn policy_criteria_lists_the_governed_predicates() {
        let policy = PolicyDisplay {
            policy_id: "p/v1".to_owned(),
            cost_limit: Some(250.0),
            cost_limit_currency: Some("USD".to_owned()),
            confirmed_states: vec!["purchase".to_owned()],
            auto_apply_actions: vec!["expedite_po".to_owned()],
            undo_window: Duration::from_secs(3600),
        };
        let criteria = policy_criteria(&policy);
        assert!(
            criteria
                .iter()
                .any(|c| c.rule == "cost_within_limit" && c.value == "250.00")
        );
        assert!(criteria.iter().any(|c| c.rule == "single_step_reversible"));
        assert!(criteria.iter().any(|c| c.rule == "supplier_confirmed"));
    }

    // --- hermetic coverage for the capability display wiring ---
    // The judgments are unit-tested in `ekg_semantic::display`; these check that this layer
    // feeds them the right reads (the current world for the original promise, the what-if for
    // the option, the gathered costs), using canned doubles that answer by SPARQL shape.

    use std::sync::Arc;

    use async_trait::async_trait;
    use ekg_domain::{
        ActionPoint, ActionPointId, ActionPointState, ActionType, EngineError, Grounding, Plan,
        Projection, ProposedAction, RankedPlan, ReadContext, Result as DomainResult, Revision,
        Score, Simulator, Slate, SourceRecord, Trace, WorldModel,
    };

    type Rows = Vec<BTreeMap<String, String>>;

    fn row(key: &str, value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(key.to_owned(), value.to_owned())])
    }

    fn pairs(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const PROMISE: &str = "2027-10-22T00:00:00";

    pub(crate) fn so() -> String {
        ontology::entity_iri("sale.order", 1)
    }
    fn battery() -> String {
        ontology::entity_iri("product.product", 4)
    }
    fn line() -> String {
        ontology::entity_iri("purchase.order.line", 3)
    }

    /// The seeded flagship as a canned `select` source: S00001 needs 10 batteries, has 2 on
    /// hand and a safety stock of 5, and P00001's line of 10 arrives `arrival`.
    struct Flagship {
        arrival: &'static str,
        currency: &'static str,
    }

    impl Flagship {
        fn answer(&self, sparql: &str) -> Rows {
            let inbound_by_promise = if self.arrival <= PROMISE { 10.0 } else { 0.0 };
            if sparql == ontology::RISK_RULE_SELECT_SPARQL {
                let shortfall = 10.0 + 5.0 - (2.0 + inbound_by_promise);
                return vec![pairs(&[
                    ("so", &so()),
                    ("comp", &battery()),
                    ("required", "10"),
                    ("oh", "2"),
                    ("ss", "5"),
                    ("shortfall", &shortfall.to_string()),
                    ("commitment", PROMISE),
                ])];
            }
            if sparql.contains("SELECT ?so ?commitment") {
                return vec![pairs(&[("so", &so()), ("commitment", PROMISE)])];
            }
            if sparql.contains("SELECT ?comp ?open ?arrival") {
                return vec![pairs(&[
                    ("comp", &battery()),
                    ("open", "10"),
                    ("arrival", self.arrival),
                ])];
            }
            if sparql.contains("SELECT ?value ?currency") {
                return vec![pairs(&[("value", "20700.00"), ("currency", "USD")])];
            }
            if sparql.contains("SELECT ?cost WHERE") {
                return vec![row("cost", "320.0")]; // the line's price (policy context)
            }
            if sparql.contains("SELECT ?state WHERE") {
                return vec![row("state", "purchase")];
            }
            if sparql.contains("SELECT ?currency") {
                return vec![row("currency", self.currency)];
            }
            if sparql.contains("?capWcName") {
                return vec![
                    pairs(&[
                        ("s", &line()),
                        ("poName", "P00001"),
                        ("qty", "10"),
                        ("productName", "Battery Pack 500Wh"),
                    ]),
                    pairs(&[("s", &so()), ("name", "S00001")]),
                    // An entity the world knows but cannot name.
                    pairs(&[("s", &battery())]),
                ];
            }
            if sparql.contains("SELECT ?arrival ?planned") {
                return vec![pairs(&[
                    ("arrival", self.arrival),
                    ("planned", "2027-11-15T08:00:00"),
                ])];
            }
            Vec::new()
        }
    }

    /// The current world: the line arrives 9 Nov, after the promise.
    struct CannedWorld(Flagship);

    #[async_trait]
    impl WorldModel for CannedWorld {
        async fn materialize(&self, _record: &SourceRecord) -> DomainResult<Revision> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn retract(&self, _entity: &EntityUri) -> DomainResult<Revision> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> DomainResult<Grounding> {
            Err(EngineError::Backend("unused".to_owned()))
        }
        async fn select(&self, sparql: &str) -> DomainResult<Rows> {
            Ok(self.0.answer(sparql))
        }
    }

    /// A simulator whose what-if is the expedite: the line arrives 6 Oct.
    struct CannedSimulator;
    struct CannedProjection(Flagship);

    #[async_trait]
    impl Simulator for CannedSimulator {
        async fn project(&self, _plan: &Plan) -> DomainResult<Box<dyn Projection>> {
            Ok(Box::new(CannedProjection(Flagship {
                arrival: "2027-10-12T08:00:00",
                currency: "USD",
            })))
        }
    }

    #[async_trait]
    impl Projection for CannedProjection {
        async fn select(&self, sparql: &str) -> DomainResult<Rows> {
            Ok(self.0.answer(sparql))
        }
    }

    fn canned_state_with(currency: &'static str) -> GraphqlState {
        GraphqlState {
            http: reqwest::Client::new(),
            ingress_url: "http://unused".to_owned(),
            admin_url: "http://unused".to_owned(),
            world: Arc::new(CannedWorld(Flagship {
                arrival: "2027-11-15T08:00:00",
                currency,
            })),
            simulator: Arc::new(CannedSimulator),
            rules: crate::drift::monitor_rules(),
            policy: PolicyDisplay {
                policy_id: "t".to_owned(),
                cost_limit: None,
                cost_limit_currency: None,
                confirmed_states: Vec::new(),
                auto_apply_actions: Vec::new(),
                undo_window: Duration::from_secs(0),
            },
        }
    }

    pub(crate) fn canned_state() -> GraphqlState {
        canned_state_with("USD")
    }

    pub(crate) fn expedite() -> Plan {
        Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("expedite_po".to_owned()),
                target: EntityUri(line()),
                params: BTreeMap::from([("new_date".to_owned(), "2027-10-12 08:00:00".to_owned())]),
            }],
        }
    }

    fn get<'a>(metrics: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
        metrics.get(key).map(String::as_str)
    }

    #[tokio::test]
    async fn an_options_framing_is_judged_on_its_what_if_against_the_original_promise() {
        let metrics = derive_display_metrics(&canned_state(), &expedite(), &so())
            .await
            .expect("derive");
        // The expedite covers the demand by the promise while the safety breach stays flagged.
        assert_eq!(get(&metrics, "delivery"), Some("on_time"));
        assert_eq!(get(&metrics, "projected_completion"), Some(PROMISE));
        assert_eq!(get(&metrics, "day_slip"), Some("0"));
        assert_eq!(get(&metrics, "order_value"), Some("20700.00"));
        assert_eq!(get(&metrics, "currency"), Some("USD"));
        // The gathered line cost, in the touched order's currency.
        assert_eq!(get(&metrics, "cost"), Some("320.00"));
        assert_eq!(get(&metrics, "cost_currency"), Some("USD"));
    }

    #[tokio::test]
    async fn the_baseline_carries_the_do_nothing_framing() {
        let framing = baseline_framing(&canned_state(), &so()).await;
        assert_eq!(get(&framing, "delivery"), Some("late"));
        assert_eq!(
            get(&framing, "projected_completion"),
            Some("2027-11-15T08:00:00")
        );
        assert_eq!(get(&framing, "day_slip"), Some("24"), "16 Oct to 9 Nov");
        assert_eq!(get(&framing, "order_value"), Some("20700.00"));
        // An ill-formed subject gets no framing.
        assert!(
            baseline_framing(&canned_state(), "not an iri")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_cost_in_mixed_currencies_is_omitted() {
        // Two lines of POs in different currencies.
        struct Mixed;
        #[async_trait]
        impl WorldModel for Mixed {
            async fn materialize(&self, _r: &SourceRecord) -> DomainResult<Revision> {
                Err(EngineError::Backend("unused".to_owned()))
            }
            async fn retract(&self, _e: &EntityUri) -> DomainResult<Revision> {
                Err(EngineError::Backend("unused".to_owned()))
            }
            async fn ground(&self, _e: &EntityUri, _c: &ReadContext) -> DomainResult<Grounding> {
                Err(EngineError::Backend("unused".to_owned()))
            }
            async fn select(&self, sparql: &str) -> DomainResult<Rows> {
                let eur = sparql.contains("purchase.order.line/9>");
                Ok(vec![row("currency", if eur { "EUR" } else { "USD" })])
            }
        }
        let costs = vec![
            (EntityUri(line()), "320.0".to_owned()),
            (
                EntityUri(ontology::entity_iri("purchase.order.line", 9)),
                "80.0".to_owned(),
            ),
        ];
        assert!(display::plan_cost(&Mixed, &costs).await.is_empty());
        let one = display::plan_cost(&Mixed, &costs[..1]).await;
        assert_eq!(get(&one, "cost"), Some("320.00"));
        assert_eq!(get(&one, "cost_currency"), Some("USD"));
    }

    #[tokio::test]
    async fn a_change_reads_its_label_and_true_from() {
        let state = canned_state();
        assert_eq!(
            change_label(&state, &line()).await.as_deref(),
            Some("P00001 · 10 × Battery Pack 500Wh")
        );
        // An expedite's from is the line's arrival; a notify has none.
        assert_eq!(
            change_from(&state, &line(), "expedite_po").await.as_deref(),
            Some("2027-11-15T08:00:00")
        );
        assert!(change_from(&state, &so(), "notify_owner").await.is_none());
        assert!(
            change_label(&state, "https://evil.example/x")
                .await
                .is_none()
        );
    }

    /// A durable record for `subject` in `state`, with a slate recommending `plan` (if any).
    pub(crate) fn record(
        subject: &str,
        state: ActionPointState,
        plan: Option<Plan>,
    ) -> (String, ActionPointRecord) {
        let options = plan
            .map(|plan| {
                vec![RankedPlan {
                    plan,
                    score: Score {
                        metrics: BTreeMap::new(),
                        rank_key: "0".to_owned(),
                    },
                    recommended: true,
                    needs: Vec::new(),
                    omissions: Vec::new(),
                    trace: Trace::default(),
                }]
            })
            .unwrap_or_default();
        (
            subject.to_owned(),
            ActionPointRecord {
                point: Some(ActionPoint {
                    id: ActionPointId(format!("ap:{subject}")),
                    subject: EntityUri(subject.to_owned()),
                    state,
                    reason: "short".to_owned(),
                    proposal: None,
                }),
                slate: Some(Slate {
                    baseline: Score {
                        metrics: BTreeMap::new(),
                        rank_key: "1".to_owned(),
                    },
                    options,
                    trace: Trace::default(),
                }),
                ..ActionPointRecord::default()
            },
        )
    }

    #[test]
    fn only_in_flight_points_with_a_slate_await_oversight() {
        let records = vec![
            record(&so(), ActionPointState::Proposed, Some(expedite())),
            record(
                &ontology::entity_iri("sale.order", 2),
                ActionPointState::Open,
                None,
            ),
            record(
                &ontology::entity_iri("sale.order", 3),
                ActionPointState::Promoted,
                None,
            ),
            (
                ontology::entity_iri("sale.order", 4),
                ActionPointRecord::default(),
            ),
        ];
        let awaiting = awaiting_oversight(&records, &BTreeMap::new());
        assert_eq!(
            awaiting.len(),
            2,
            "promoted and slate-less points are not awaiting"
        );
        assert_eq!(awaiting[0].recommended, Some(expedite()));
        assert_eq!(
            awaiting[1].recommended, None,
            "no recommendation, so do nothing"
        );
    }

    /// An active portfolio of `members` at generation 2, carrying a joint slate recommending
    /// `plan` for generation `slate_generation`.
    pub(crate) fn portfolio(
        members: &[&str],
        slate_generation: u32,
        plan: Plan,
    ) -> (String, ekg_dynamic::PortfolioRecord) {
        let (_, joint) = record(members[0], ActionPointState::Proposed, Some(plan));
        let id = "portfolio/0123456789ab".to_owned();
        (
            id.clone(),
            ekg_dynamic::PortfolioRecord {
                id: Some(ekg_domain::PortfolioId(id)),
                members: members.iter().map(|m| EntityUri((*m).to_owned())).collect(),
                state: PortfolioState::Active,
                generation: 2,
                audit: Vec::new(),
                slate: joint.slate,
                slate_generation: Some(slate_generation),
                promotion: None,
                slate_attached_at: None,
            },
        )
    }

    /// A point in `portfolio/0123456789ab`, carrying its own (earlier) slate recommending `own`.
    pub(crate) fn member(subject: &str, own: Option<Plan>) -> (String, ActionPointRecord) {
        let (key, mut rec) = record(subject, ActionPointState::Proposed, own);
        rec.portfolio = Some(ekg_domain::PortfolioId("portfolio/0123456789ab".to_owned()));
        (key, rec)
    }

    pub(crate) fn notify_plan(subject: &str) -> Plan {
        Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("notify_owner".to_owned()),
                target: EntityUri(subject.to_owned()),
                params: BTreeMap::from([("message".to_owned(), "hi".to_owned())]),
            }],
        }
    }

    #[test]
    fn a_member_is_judged_by_its_portfolios_plan_not_its_earlier_slate() {
        let mo = ontology::entity_iri("mrp.production", 1);
        let records = vec![member(&so(), Some(notify_plan(&so()))), member(&mo, None)];
        let portfolios = BTreeMap::from([portfolio(&[&so(), &mo], 2, expedite())]);
        let awaiting = awaiting_oversight(&records, &portfolios);
        assert_eq!(
            awaiting.len(),
            2,
            "both members await, through the joint slate"
        );
        assert!(
            awaiting.iter().all(|a| a.recommended == Some(expedite())),
            "the joint plan, never the earlier point slate: {awaiting:?}"
        );
    }

    #[test]
    fn a_member_whose_joint_slate_is_pending_awaits_it() {
        // The joint slate on record is for generation 1; the portfolio is at 2.
        let records = vec![member(&so(), Some(notify_plan(&so())))];
        let portfolios = BTreeMap::from([portfolio(&[&so()], 1, expedite())]);
        assert!(awaiting_oversight(&records, &portfolios).is_empty());
    }

    #[test]
    fn a_point_in_no_active_portfolio_is_judged_by_its_own_slate() {
        // Its portfolio is superseded (or unknown): its own slate is its plan.
        let records = vec![member(&so(), Some(notify_plan(&so())))];
        let (id, mut superseded) = portfolio(&[&so()], 2, expedite());
        superseded.state = PortfolioState::Superseded;
        for portfolios in [BTreeMap::from([(id, superseded)]), BTreeMap::new()] {
            let awaiting = awaiting_oversight(&records, &portfolios);
            assert_eq!(awaiting.len(), 1);
            assert_eq!(awaiting[0].recommended, Some(notify_plan(&so())));
        }
    }

    #[tokio::test]
    async fn the_headline_counts_a_member_through_the_joint_slate() {
        // The member's earlier notify would leave it late; the joint expedite brings it in.
        let records = vec![member(&so(), Some(notify_plan(&so())))];
        let portfolios = BTreeMap::from([portfolio(&[&so()], 2, expedite())]);
        let metrics = headline_metrics(&canned_state(), &records, &portfolios).await;
        assert_eq!(get(&metrics, "late_orders_now"), Some("1"));
        assert_eq!(get(&metrics, "late_orders"), Some("0"), "{metrics:?}");
    }

    #[tokio::test]
    async fn the_headline_serves_late_now_and_if_approved_and_exact_revenue() {
        let records = vec![record(&so(), ActionPointState::Proposed, Some(expedite()))];
        let metrics = headline_metrics(&canned_state(), &records, &BTreeMap::new()).await;
        assert_eq!(get(&metrics, "late_orders_now"), Some("1"));
        assert_eq!(
            get(&metrics, "late_orders"),
            Some("0"),
            "the expedite brings it in"
        );
        assert_eq!(get(&metrics, "revenue_at_risk"), Some("20700.00"));
        assert_eq!(get(&metrics, "currency"), Some("USD"));
    }

    // --- read-model tabs ---
    // The tab logic itself is tested in `ekg_semantic::tabs` (hermetic) and `tabs_it` (live);
    // these cover the backend wiring: the rule verdicts become the at-risk set, a rule failure
    // omits the markers instead of denying risk, and serving a tab never writes.

    /// A world that answers every query with the same canned rows, can fail the supply-risk
    /// rule SELECT, and panics on any write, so a test that serves a tab fails if it takes a
    /// mutation path. Its supply-risk rule flags `sale.order/42`, and its process-contention
    /// rule flags `mrp.production/3` (one over-capacity row), so each capability's verdict is
    /// observable.
    struct ReadOnlyTabWorld {
        rows: Vec<BTreeMap<String, String>>,
        fail_rules: bool,
    }

    #[async_trait]
    impl WorldModel for ReadOnlyTabWorld {
        async fn materialize(&self, _record: &SourceRecord) -> DomainResult<Revision> {
            panic!("serving a read-model tab must never materialize");
        }
        async fn retract(&self, _entity: &EntityUri) -> DomainResult<Revision> {
            panic!("serving a read-model tab must never retract");
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> DomainResult<Grounding> {
            panic!("serving a read-model tab must never ground");
        }
        async fn select(&self, sparql: &str) -> DomainResult<Vec<BTreeMap<String, String>>> {
            if self.fail_rules && sparql == ontology::RISK_RULE_SELECT_SPARQL {
                return Err(EngineError::Backend("rule read failed".to_owned()));
            }
            if sparql == ontology::RISK_RULE_SELECT_SPARQL {
                // The rule flags sale.order/42 short of product 4.
                return Ok(vec![BTreeMap::from([
                    ("so".to_owned(), ontology::entity_iri("sale.order", 42)),
                    (
                        "comp".to_owned(),
                        ontology::entity_iri("product.product", 4),
                    ),
                    ("shortfall".to_owned(), "10".to_owned()),
                ])]);
            }
            if sparql == ontology::PROCESS_CONTENTION_SELECT_SPARQL {
                // Work order 1 at a capacity-1 work center with one overlapping peer:
                // concurrency 2 > 1, so the contention rule flags manufacturing order 3 (the
                // bindings `drift::process_contention_rule` interprets).
                return Ok(vec![BTreeMap::from([
                    ("mo".to_owned(), ontology::entity_iri("mrp.production", 3)),
                    ("wo".to_owned(), ontology::entity_iri("mrp.workorder", 1)),
                    ("wc".to_owned(), ontology::entity_iri("mrp.workcenter", 9)),
                    ("capacity".to_owned(), "1".to_owned()),
                    ("overlapPeers".to_owned(), "1".to_owned()),
                ])]);
            }
            Ok(self.rows.clone())
        }
    }

    fn state_with_world(rows: Vec<BTreeMap<String, String>>, fail_rules: bool) -> GraphqlState {
        GraphqlState {
            http: reqwest::Client::new(),
            ingress_url: "http://unused".to_owned(),
            admin_url: "http://unused".to_owned(),
            world: Arc::new(ReadOnlyTabWorld { rows, fail_rules }),
            simulator: Arc::new(CannedSimulator),
            rules: crate::drift::monitor_rules(),
            policy: PolicyDisplay {
                policy_id: "t".to_owned(),
                cost_limit: None,
                cost_limit_currency: None,
                confirmed_states: Vec::new(),
                auto_apply_actions: Vec::new(),
                undo_window: Duration::from_secs(0),
            },
        }
    }

    fn node_row(iri: &str, role: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("kind".to_owned(), "node".to_owned()),
            ("node".to_owned(), iri.to_owned()),
            ("role".to_owned(), role.to_owned()),
        ])
    }

    fn field(row: &ProjectionRow, key: &str) -> Option<String> {
        row.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.clone())
    }

    #[tokio::test]
    async fn the_at_risk_marker_is_the_monitors_current_verdict() {
        let flagged = ontology::entity_iri("sale.order", 42);
        let other = ontology::entity_iri("sale.order", 7);
        let state = state_with_world(
            vec![
                node_row(&flagged, "SalesOrder"),
                node_row(&other, "SalesOrder"),
            ],
            false,
        );
        let rows = read_model(&state, &EntityUri(flagged.clone()), TabKind::Graph)
            .await
            .expect("graph projected");
        let at_risk = |iri: &str| {
            rows.iter()
                .find(|r| field(r, "node").as_deref() == Some(iri))
                .and_then(|r| field(r, "at_risk"))
        };
        assert_eq!(at_risk(&flagged).as_deref(), Some("true"));
        assert_eq!(at_risk(&other).as_deref(), Some("false"));
    }

    #[tokio::test]
    async fn a_process_rule_verdict_lights_the_tab_marker() {
        // The tab evaluates the monitor's whole rule set, not only supply risk: a manufacturing
        // order flagged by the contention rule reads at risk in the flow's production stage.
        let flagged = ontology::entity_iri("mrp.production", 3);
        let other = ontology::entity_iri("mrp.production", 4);
        let stage_row = |mo: &str| {
            BTreeMap::from([
                ("entity".to_owned(), mo.to_owned()),
                ("stage".to_owned(), "in_production".to_owned()),
            ])
        };
        let state = state_with_world(vec![stage_row(&flagged), stage_row(&other)], false);
        let rows = read_model(&state, &EntityUri(flagged.clone()), TabKind::Flow)
            .await
            .expect("flow projected");
        let at_risk = |iri: &str| {
            rows.iter()
                .find(|r| field(r, "entity").as_deref() == Some(iri))
                .and_then(|r| field(r, "at_risk"))
        };
        assert_eq!(
            at_risk(&flagged).as_deref(),
            Some("true"),
            "the process rule's verdict marks its manufacturing order"
        );
        assert_eq!(at_risk(&other).as_deref(), Some("false"));
    }

    #[tokio::test]
    async fn an_unevaluable_rule_set_serves_the_tab_without_risk_markers() {
        let subject = ontology::entity_iri("sale.order", 42);
        let state = state_with_world(vec![node_row(&subject, "SalesOrder")], true);
        let rows = read_model(&state, &EntityUri(subject.clone()), TabKind::Graph)
            .await
            .expect("the tab is still served");
        assert_eq!(rows.len(), 1);
        assert!(field(&rows[0], "at_risk").is_none(), "never a denied risk");
        assert!(field(&rows[0], "cause").is_none());
    }

    #[tokio::test]
    async fn a_tab_read_serves_the_same_neutral_rows_for_either_capability() {
        // The same tab types and rows serve a supply-risk plan and a process plan, differing
        // only in opaque row values. (The SDL-level neutrality is tested in graphql::mod.)
        let so = ontology::entity_iri("sale.order", 42);
        let product = ontology::entity_iri("product.product", 3);
        let sr = read_model(
            &state_with_world(vec![node_row(&so, "SalesOrder")], false),
            &EntityUri(so.clone()),
            TabKind::Graph,
        )
        .await
        .expect("supply-risk graph");
        let pr = read_model(
            &state_with_world(vec![node_row(&product, "Product")], false),
            &EntityUri(ontology::entity_iri("mrp.production", 3)),
            TabKind::Graph,
        )
        .await
        .expect("process graph");
        let keys = |rows: &[ProjectionRow]| -> Vec<String> {
            rows[0].fields.iter().map(|f| f.key.clone()).collect()
        };
        assert_eq!(
            keys(&sr),
            keys(&pr),
            "the two capabilities share the row shape"
        );
        assert_eq!(field(&sr[0], "role").as_deref(), Some("SalesOrder"));
        assert_eq!(field(&pr[0], "role").as_deref(), Some("Product"));
    }

    #[tokio::test]
    async fn serving_a_tab_takes_no_write_path() {
        // The world's write methods panic, so a passing run over all four tabs shows each
        // is a pure read.
        let subject = EntityUri(ontology::entity_iri("sale.order", 42));
        for tab in [
            TabKind::Timeline,
            TabKind::Graph,
            TabKind::Flow,
            TabKind::Suppliers,
        ] {
            let state = state_with_world(vec![row("kind", "node")], false);
            read_model(&state, &subject, tab).await.expect("read a tab");
        }
    }
}
