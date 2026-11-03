//! The per-task run ledger: what each task did through the gateway, recorded by the gateway
//! itself and read once by the engine after the task.
//!
//! Per token id it keeps the count of every tool's calls by outcome, the defects of each
//! rejected `submit_plan`, and the last plan `submit_plan` accepted, normalized as the tool
//! returned it. The engine takes the plan of record from here, not from the runtime's reply:
//! a plan in the ledger passed the engine's own check, and a runtime (or a prompt injection)
//! cannot claim one it never submitted.
//!
//! The ledger is shared in-process with the engine and is not served on any route. It is
//! bounded like the scope cache: an entry expires with its token, an insert first evicts expired
//! entries and then (if still full) the entry expiring soonest, and [`RunLedger::take`] removes
//! the entry it returns.
//!
//! The engine may open a task's entry before the task's first call, to note the span the
//! task's run is traced by; the gateway hangs the spans of that task's calls under it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard};

use ekg_domain::ProposedAction;
use opentelemetry::trace::SpanContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::finding::{Finding, ReadEdge, SUBMIT_FINDING};
use super::profile::ToolError;
use super::submit::SUBMIT_PLAN;
use super::token::{TaskClaims, now_secs};

/// The default number of task runs held at once.
pub const DEFAULT_RUN_LEDGER_CAPACITY: usize = 1024;

/// The most edges kept of what one task read: far more than a run's step limit lets it read,
/// so a citation is never refused for this in practice.
const MAX_READ_EDGES: usize = 4096;

/// A plan `submit_plan` accepted: its normalized actions and its explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedPlan {
    /// The plan's actions, normalized and de-duplicated as the tool returned them.
    pub actions: Vec<ProposedAction>,
    /// The explanation for the operator.
    pub explanation: String,
}

/// What one task did through the gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    /// The count of calls per tool and outcome (`ok`, `refused`, `rejected`, `error`).
    pub tool_calls: BTreeMap<String, BTreeMap<String, u32>>,
    /// The defects of every rejected `submit_plan` or `submit_finding`, in call order.
    pub rejections: Vec<Vec<String>>,
    /// The last plan `submit_plan` accepted, if any.
    pub accepted: Option<AcceptedPlan>,
    /// The last finding `submit_finding` accepted, if any.
    pub finding: Option<Finding>,
    /// The edges the task's tools returned, as a finding may cite them.
    pub reads: BTreeSet<ReadEdge>,
}

impl TaskRun {
    /// How many calls `tool` made with `outcome`.
    #[must_use]
    pub fn calls(&self, tool: &str, outcome: &str) -> u32 {
        self.tool_calls
            .get(tool)
            .and_then(|outcomes| outcomes.get(outcome))
            .copied()
            .unwrap_or(0)
    }
}

/// One call as the gateway hands it to the ledger.
pub(crate) struct ObservedCall<'a> {
    /// The tool called.
    pub tool: &'a str,
    /// The call's outcome, as the call log records it.
    pub outcome: &'static str,
    /// The tool's result, when the tool ran (none for a refused or unscoped call).
    pub result: Option<&'a Result<Value, ToolError>>,
}

/// Task runs by token id, each held until its token expires.
pub struct RunLedger {
    capacity: usize,
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    exp: u64,
    run: TaskRun,
    /// The span the task's run is traced by, when the engine opened the entry with one.
    run_span: Option<SpanContext>,
}

/// Make room for one more entry: drop the expired ones and then, if still full, the one
/// expiring soonest.
fn make_room(entries: &mut HashMap<String, Entry>, capacity: usize, now: u64) {
    entries.retain(|_, entry| entry.exp > now);
    if entries.len() >= capacity {
        let soonest = entries
            .iter()
            .min_by_key(|(_, entry)| entry.exp)
            .map(|(jti, _)| jti.clone());
        if let Some(soonest) = soonest {
            entries.remove(&soonest);
        }
    }
}

impl Default for RunLedger {
    fn default() -> Self {
        Self::new(DEFAULT_RUN_LEDGER_CAPACITY)
    }
}

impl std::fmt::Debug for RunLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLedger")
            .field("capacity", &self.capacity)
            .field("held", &self.len())
            .finish_non_exhaustive()
    }
}

/// Add the edges of every grounding in a tool's `result` to `reads`, however the tool nested
/// them: one grounding, or a list of them.
fn collect_edges(result: &Value, reads: &mut BTreeSet<ReadEdge>) {
    match result {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "edges" {
                    for edge in value.as_array().into_iter().flatten() {
                        if reads.len() >= MAX_READ_EDGES {
                            return;
                        }
                        if let Some(edge) = read_edge(edge) {
                            reads.insert(edge);
                        }
                    }
                } else {
                    collect_edges(value, reads);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_edges(item, reads)),
        _ => {}
    }
}

/// One `edges` item read as an edge.
fn read_edge(edge: &Value) -> Option<ReadEdge> {
    Some(ReadEdge {
        subject: edge.get("subject")?.as_str()?.to_owned(),
        predicate: edge.get("predicate")?.as_str()?.to_owned(),
        object: edge.get("object")?.as_str()?.to_owned(),
    })
}

/// A `submit_finding` result read back as the finding it accepted.
fn accepted_finding(value: &Value) -> Option<Finding> {
    serde_json::from_value(value.get("finding")?.clone()).ok()
}

/// A `submit_plan` result read back as the plan it accepted.
fn accepted_plan(value: &Value) -> Option<AcceptedPlan> {
    let actions = serde_json::from_value(value.get("actions")?.clone()).ok()?;
    let explanation = value.get("explanation")?.as_str()?.to_owned();
    Some(AcceptedPlan {
        actions,
        explanation,
    })
}

impl RunLedger {
    /// A ledger holding at most `capacity` task runs (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Record one call of the task `claims` names, at `now` (seconds since the Unix epoch).
    /// A call is counted by tool and outcome; a rejected hand-in adds its defects; an accepted
    /// one replaces what the task handed in; a read adds the edges it returned. A poisoned
    /// ledger records nothing.
    pub(crate) fn record(&self, claims: &TaskClaims, call: &ObservedCall<'_>, now: u64) {
        let Ok(mut entries) = self.lock() else {
            return;
        };
        if !entries.contains_key(&claims.jti) {
            make_room(&mut entries, self.capacity, now);
        }
        let run = &mut entries
            .entry(claims.jti.clone())
            .or_insert_with(|| Entry {
                exp: claims.exp,
                run: TaskRun::default(),
                run_span: None,
            })
            .run;
        *run.tool_calls
            .entry(call.tool.to_owned())
            .or_default()
            .entry(call.outcome.to_owned())
            .or_default() += 1;
        let hand_in = matches!(call.tool, SUBMIT_PLAN | SUBMIT_FINDING);
        match call.result {
            Some(Err(ToolError::Rejected { defects, .. })) if hand_in => {
                run.rejections.push(defects.clone());
            }
            Some(Ok(value)) if call.tool == SUBMIT_PLAN => {
                if let Some(plan) = accepted_plan(value) {
                    run.accepted = Some(plan);
                }
            }
            Some(Ok(value)) if call.tool == SUBMIT_FINDING => {
                if let Some(finding) = accepted_finding(value) {
                    run.finding = Some(finding);
                }
            }
            Some(Ok(value)) => collect_edges(value, &mut run.reads),
            Some(Err(_)) | None => {}
        }
    }

    /// Open the entry of the task `claims` names before its first call, noting `run_span` as
    /// the span its run is traced by, at `now`. The entry is like any other: calls are
    /// recorded into it, it expires with the token, and it can be evicted. A task that already
    /// has an entry keeps its counts.
    pub fn open(&self, claims: &TaskClaims, run_span: SpanContext, now: u64) {
        let Ok(mut entries) = self.lock() else {
            return;
        };
        if !entries.contains_key(&claims.jti) {
            make_room(&mut entries, self.capacity, now);
        }
        entries
            .entry(claims.jti.clone())
            .or_insert_with(|| Entry {
                exp: claims.exp,
                run: TaskRun::default(),
                run_span: None,
            })
            .run_span = Some(run_span);
    }

    /// The span the run of token `jti` is traced by, when its entry was opened with one.
    #[must_use]
    pub fn run_span(&self, jti: &str) -> Option<SpanContext> {
        self.lock().ok()?.get(jti)?.run_span.clone()
    }

    /// What the task of token `jti` did so far, leaving its entry in place (`None` before its
    /// first recorded call or opening).
    #[must_use]
    pub fn so_far(&self, jti: &str) -> Option<TaskRun> {
        Some(self.lock().ok()?.get(jti)?.run.clone())
    }

    /// Take the run of token `jti`: it is returned once and removed. An expired run is
    /// dropped, not returned.
    #[must_use]
    pub fn take(&self, jti: &str) -> Option<TaskRun> {
        self.take_at(jti, now_secs().unwrap_or(0))
    }

    /// [`Self::take`] against an explicit clock.
    #[must_use]
    pub fn take_at(&self, jti: &str, now: u64) -> Option<TaskRun> {
        let entry = self.lock().ok()?.remove(jti)?;
        (entry.exp > now).then_some(entry.run)
    }

    /// How many task runs are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |entries| entries.len())
    }

    /// Whether no task run is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, Entry>>, ()> {
        self.entries.lock().map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use serde_json::json;

    use super::*;
    use crate::tools::finding::MemberState;
    use crate::tools::testing::claims_for;

    fn order() -> ekg_domain::EntityUri {
        ekg_domain::EntityUri("https://ekg.example/id/sale.order/1".to_owned())
    }

    fn plan(date: &str) -> Value {
        json!({
            "accepted": true,
            "actions": [{
                "action_type": "move_date",
                "target": order().0,
                "params": { "new_date": date }
            }],
            "explanation": format!("move to {date}"),
        })
    }

    fn call<'a>(
        tool: &'a str,
        outcome: &'static str,
        result: Option<&'a Result<Value, ToolError>>,
    ) -> ObservedCall<'a> {
        ObservedCall {
            tool,
            outcome,
            result,
        }
    }

    #[test]
    fn calls_are_counted_per_tool_and_outcome() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        let ok: Result<Value, ToolError> = Ok(json!({}));
        let missing: Result<Value, ToolError> = Err(ToolError::NotFound("x".to_owned()));
        ledger.record(&claims, &call("ground_entity", "ok", Some(&ok)), 10);
        ledger.record(&claims, &call("ground_entity", "ok", Some(&ok)), 10);
        ledger.record(&claims, &call("ground_entity", "error", Some(&missing)), 10);
        ledger.record(&claims, &call("related_entities", "refused", None), 10);
        let run = ledger.take_at(&claims.jti, 20).expect("run");
        assert_eq!(run.calls("ground_entity", "ok"), 2);
        assert_eq!(run.calls("ground_entity", "error"), 1);
        assert_eq!(run.calls("related_entities", "refused"), 1);
        assert_eq!(run.calls("submit_plan", "ok"), 0);
        assert!(run.rejections.is_empty() && run.accepted.is_none());
    }

    #[test]
    fn rejected_submissions_keep_their_defects_in_order() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        let first: Result<Value, ToolError> = Err(ToolError::Rejected {
            defects: vec!["a".to_owned(), "b".to_owned()],
            guidance: Some("the catalog".to_owned()),
        });
        let second: Result<Value, ToolError> = Err(ToolError::Rejected {
            defects: vec!["c".to_owned()],
            guidance: None,
        });
        ledger.record(&claims, &call(SUBMIT_PLAN, "rejected", Some(&first)), 10);
        ledger.record(&claims, &call(SUBMIT_PLAN, "rejected", Some(&second)), 10);
        let run = ledger.take_at(&claims.jti, 20).expect("run");
        assert_eq!(run.calls(SUBMIT_PLAN, "rejected"), 2);
        assert_eq!(
            run.rejections,
            vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]]
        );
        assert!(run.accepted.is_none(), "nothing was accepted");
    }

    #[test]
    fn the_last_accepted_plan_wins_and_a_later_rejection_does_not_replace_it() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        let first: Result<Value, ToolError> = Ok(plan("2027-11-07 00:00:00"));
        let second: Result<Value, ToolError> = Ok(plan("2027-11-08 00:00:00"));
        let rejected: Result<Value, ToolError> = Err(ToolError::Rejected {
            defects: vec!["no".to_owned()],
            guidance: None,
        });
        ledger.record(&claims, &call(SUBMIT_PLAN, "ok", Some(&first)), 10);
        ledger.record(&claims, &call(SUBMIT_PLAN, "ok", Some(&second)), 10);
        ledger.record(&claims, &call(SUBMIT_PLAN, "rejected", Some(&rejected)), 10);
        let run = ledger.take_at(&claims.jti, 20).expect("run");
        let accepted = run.accepted.expect("accepted");
        assert_eq!(accepted.explanation, "move to 2027-11-08 00:00:00");
        assert_eq!(accepted.actions.len(), 1);
        assert_eq!(
            accepted.actions[0]
                .params
                .get("new_date")
                .map(String::as_str),
            Some("2027-11-08 00:00:00")
        );
        assert_eq!(run.rejections, vec![vec!["no".to_owned()]]);
    }

    #[test]
    fn take_returns_a_run_once() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        ledger.record(&claims, &call("ground_entity", "ok", None), 10);
        assert!(ledger.take_at(&claims.jti, 20).is_some());
        assert!(ledger.take_at(&claims.jti, 20).is_none(), "read once");
        assert!(ledger.is_empty());
    }

    #[test]
    fn what_a_task_did_so_far_can_be_read_without_taking_it() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        assert!(ledger.so_far(&claims.jti).is_none(), "nothing recorded yet");
        ledger.record(&claims, &call("ground_entity", "ok", None), 10);
        for _ in 0..2 {
            let run = ledger.so_far(&claims.jti).expect("the run so far");
            assert_eq!(run.calls("ground_entity", "ok"), 1);
        }
        let run = ledger
            .take_at(&claims.jti, 20)
            .expect("still there to take");
        assert_eq!(run.calls("ground_entity", "ok"), 1);
    }

    #[test]
    fn an_expired_run_is_dropped() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 100);
        ledger.record(&claims, &call("ground_entity", "ok", None), 10);
        assert!(ledger.take_at(&claims.jti, 100).is_none(), "expired");
        assert!(ledger.is_empty(), "and dropped");
        // An expired entry is also evicted by the next insert.
        let stale = claims_for(&[order()], 50);
        let fresh = claims_for(&[order()], 900);
        ledger.record(&stale, &call("ground_entity", "ok", None), 10);
        ledger.record(&fresh, &call("ground_entity", "ok", None), 60);
        assert_eq!(ledger.len(), 1);
        assert!(ledger.take_at(&fresh.jti, 60).is_some());
    }

    #[test]
    fn the_ledger_is_bounded_evicting_the_soonest_to_expire() {
        let ledger = RunLedger::new(2);
        let soon = claims_for(&[order()], 500);
        let late = claims_for(&[order()], 900);
        let newest = claims_for(&[order()], 950);
        for claims in [&soon, &late, &newest] {
            ledger.record(claims, &call("ground_entity", "ok", None), 10);
        }
        assert_eq!(ledger.len(), 2);
        assert!(ledger.take_at(&soon.jti, 10).is_none(), "evicted");
        assert!(ledger.take_at(&late.jti, 10).is_some());
        assert!(ledger.take_at(&newest.jti, 10).is_some());
    }

    fn run_span(span: &str) -> SpanContext {
        use opentelemetry::trace::{SpanId, TraceFlags, TraceId, TraceState};
        SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace"),
            SpanId::from_hex(span).expect("span"),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        )
    }

    #[test]
    fn an_opened_entry_notes_the_run_span_and_is_the_one_calls_are_recorded_in() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        let ok: Result<Value, ToolError> = Ok(json!({}));
        assert!(ledger.run_span(&claims.jti).is_none(), "nothing opened yet");
        ledger.open(&claims, run_span("00000000000000b1"), 10);
        assert_eq!(ledger.len(), 1);
        ledger.record(&claims, &call("ground_entity", "ok", Some(&ok)), 10);
        ledger.record(&claims, &call("ground_entity", "ok", Some(&ok)), 10);
        assert_eq!(ledger.len(), 1, "one entry, opened and recorded into");
        assert_eq!(
            ledger
                .run_span(&claims.jti)
                .expect("span")
                .span_id()
                .to_string(),
            "00000000000000b1"
        );
        let run = ledger.take_at(&claims.jti, 20).expect("run");
        assert_eq!(run.calls("ground_entity", "ok"), 2);
        assert!(
            ledger.run_span(&claims.jti).is_none(),
            "removed with the entry"
        );
    }

    #[test]
    fn the_counts_of_a_run_are_the_same_with_and_without_an_opened_entry() {
        let ok: Result<Value, ToolError> = Ok(json!({}));
        let accepted: Result<Value, ToolError> = Ok(plan("2027-11-08"));
        let run = |opened: bool| {
            let ledger = RunLedger::default();
            let claims = claims_for(&[order()], 1_000);
            if opened {
                ledger.open(&claims, run_span("00000000000000b1"), 10);
            }
            ledger.record(&claims, &call("ground_entity", "ok", Some(&ok)), 10);
            ledger.record(&claims, &call(SUBMIT_PLAN, "ok", Some(&accepted)), 10);
            ledger.take_at(&claims.jti, 20).expect("run")
        };
        assert_eq!(run(true), run(false));
    }

    #[test]
    fn an_entry_the_gateway_made_notes_no_run_span() {
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 1_000);
        let ok: Result<Value, ToolError> = Ok(json!({}));
        ledger.record(&claims, &call("ground_entity", "ok", Some(&ok)), 10);
        assert!(ledger.run_span(&claims.jti).is_none());
        // Opening it afterwards keeps what was counted.
        ledger.open(&claims, run_span("00000000000000b1"), 10);
        assert!(ledger.run_span(&claims.jti).is_some());
        assert_eq!(
            ledger
                .take_at(&claims.jti, 20)
                .expect("run")
                .calls("ground_entity", "ok"),
            1
        );
    }

    #[test]
    fn an_opened_entry_expires_and_is_evicted_like_any_other() {
        let ok: Result<Value, ToolError> = Ok(json!({}));
        // Expired: dropped, not returned.
        let ledger = RunLedger::default();
        let claims = claims_for(&[order()], 50);
        ledger.open(&claims, run_span("00000000000000b1"), 10);
        assert!(ledger.take_at(&claims.jti, 100).is_none());

        // Evicted by a newer task at capacity, then made again by a call: no span is noted.
        let ledger = RunLedger::new(1);
        let (first, second) = (claims_for(&[order()], 500), claims_for(&[order()], 900));
        ledger.open(&first, run_span("00000000000000b1"), 10);
        ledger.open(&second, run_span("00000000000000b2"), 10);
        assert_eq!(ledger.len(), 1);
        assert!(ledger.run_span(&first.jti).is_none(), "evicted");
        assert!(ledger.run_span(&second.jti).is_some());
        ledger.record(&first, &call("ground_entity", "ok", Some(&ok)), 10);
        assert!(
            ledger.run_span(&first.jti).is_none(),
            "made again without one"
        );
        assert_eq!(
            ledger
                .take_at(&first.jti, 20)
                .expect("run")
                .calls("ground_entity", "ok"),
            1
        );
    }

    fn read_call(ledger: &RunLedger, claims: &TaskClaims, tool: &str, result: Value) {
        let result = Ok(result);
        let call = ObservedCall {
            tool,
            outcome: "ok",
            result: Some(&result),
        };
        ledger.record(claims, &call, claims.exp - 1);
    }

    fn grounded(subject: &str, object: &str) -> Value {
        json!({
            "entity": subject,
            "edges": [{
                "subject": subject,
                "predicate": "ekg:state",
                "object": object,
                "source": { "system": "odoo" },
                "revision": 1
            }],
            "omissions": []
        })
    }

    #[test]
    fn a_task_keeps_the_edges_of_a_grounding() {
        let ledger = RunLedger::new(4);
        let claims = claims_for(&[order()], 100);
        read_call(&ledger, &claims, "ground_entity", grounded("urn:a", "late"));
        let run = ledger.so_far(&claims.jti).expect("run");
        let edge = run.reads.iter().next().expect("edge");
        assert_eq!(
            (edge.subject.as_str(), edge.object.as_str()),
            ("urn:a", "late")
        );
    }

    #[test]
    fn a_task_keeps_the_edges_of_a_listing() {
        let ledger = RunLedger::new(4);
        let claims = claims_for(&[order()], 100);
        let listing = json!({
            "work_centre": "urn:c",
            "work_orders": [grounded("urn:a", "1"), grounded("urn:b", "2")]
        });
        read_call(&ledger, &claims, "list_work_centre_schedule", listing);
        assert_eq!(ledger.so_far(&claims.jti).expect("run").reads.len(), 2);
    }

    #[test]
    fn a_hand_in_adds_no_reads() {
        let ledger = RunLedger::new(4);
        let claims = claims_for(&[order()], 100);
        read_call(&ledger, &claims, SUBMIT_PLAN, plan("2026-11-02"));
        assert!(ledger.so_far(&claims.jti).expect("run").reads.is_empty());
    }

    #[test]
    fn what_a_task_reads_is_bounded() {
        let mut reads = BTreeSet::new();
        let edges: Vec<Value> = (0..MAX_READ_EDGES + 10)
            .map(|n| json!({ "subject": "s", "predicate": "p", "object": n.to_string() }))
            .collect();
        collect_edges(&json!({ "edges": edges }), &mut reads);
        assert_eq!(reads.len(), MAX_READ_EDGES);
    }

    #[test]
    fn an_accepted_finding_and_a_rejected_one_are_recorded() {
        let ledger = RunLedger::new(4);
        let claims = claims_for(&[order()], 100);
        let rejected = Err(ToolError::Rejected {
            defects: vec!["no cause".to_owned()],
            guidance: None,
        });
        let call = ObservedCall {
            tool: SUBMIT_FINDING,
            outcome: "rejected",
            result: Some(&rejected),
        };
        ledger.record(&claims, &call, claims.exp - 1);
        let finding = json!({
            "accepted": true,
            "finding": { "member": order().0, "state": "on_plan", "causes": [] }
        });
        read_call(&ledger, &claims, SUBMIT_FINDING, finding);
        let run = ledger.take_at(&claims.jti, claims.exp - 1).expect("run");
        assert_eq!(run.rejections, vec![vec!["no cause".to_owned()]]);
        assert_eq!(run.finding.expect("finding").state, MemberState::OnPlan);
        assert!(run.accepted.is_none());
    }
}
