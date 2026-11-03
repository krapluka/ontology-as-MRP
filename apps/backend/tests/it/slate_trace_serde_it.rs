//! Serialization contract for the reasoning trace at the app boundary.
//!
//! The backend serves the durable `Slate` as-is (no separate DTO), so the JSON a client
//! of `/api` consumes is the domain type's serde form. This test needs no live stack and
//! pins that contract: an enriched slate serializes its `trace` as `snake_case` with the
//! three tag kinds and the `rejected` flag. `slate_e2e_it` covers the full round-trip
//! through the live handler.

#![allow(clippy::expect_used)]

use std::collections::BTreeMap;

use ekg_domain::{
    ActionType, Plan, ProposedAction, RankedPlan, Score, Slate, Trace, TraceStep, TraceTag,
};

fn score(rank: &str) -> Score {
    Score {
        metrics: BTreeMap::from([("risk_left".to_owned(), "0".to_owned())]),
        rank_key: rank.to_owned(),
    }
}

fn step(tag: TraceTag, rejected: bool) -> TraceStep {
    TraceStep {
        tag,
        text: "why".to_owned(),
        rejected,
    }
}

#[test]
fn a_slate_serializes_its_reasoning_trace_as_snake_case_json() {
    let option = RankedPlan {
        plan: Plan {
            actions: vec![ProposedAction {
                action_type: ActionType("reschedule_work_order".to_owned()),
                target: ekg_domain::EntityUri("https://ekg.example/id/mrp.workorder/1".to_owned()),
                params: BTreeMap::new(),
            }],
        },
        score: score("00000"),
        recommended: true,
        needs: Vec::new(),
        omissions: Vec::new(),
        trace: Trace::from_steps(vec![
            step(TraceTag::Graph, false),
            step(TraceTag::Assumption, false),
            step(TraceTag::Simulated, false),
        ]),
    };
    let slate = Slate {
        baseline: score("00001"),
        options: vec![option],
        trace: Trace::from_steps(vec![
            step(TraceTag::Simulated, false), // baseline
            step(TraceTag::Graph, false),
            step(TraceTag::Assumption, false),
            step(TraceTag::Simulated, false), // admitted
            step(TraceTag::Simulated, true),  // rejected, retained
        ]),
    };

    let json = serde_json::to_string(&slate).expect("serialize slate");

    // The trace, its tags and its flags are present, snake_case, at both levels.
    assert!(json.contains("\"trace\""), "slate carries a trace: {json}");
    assert!(json.contains("\"simulated\""));
    assert!(json.contains("\"graph\""));
    assert!(json.contains("\"assumption\""));
    assert!(json.contains("\"rejected\":true"));
    assert!(json.contains("\"open_assumptions\""));

    // And it round-trips back to the same value.
    let back: Slate = serde_json::from_str(&json).expect("deserialize slate");
    assert_eq!(back, slate);
    assert_eq!(back.trace.summary.rejected, 1);
}
