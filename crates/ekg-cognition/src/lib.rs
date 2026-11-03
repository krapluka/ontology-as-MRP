//! Cognition boundary: provider-neutral and cross-cutting (not one of the three core
//! layers).
//!
//! - [`provider`]: the [`CognitionProvider`] driver over an OpenAI-compatible endpoint,
//!   through the Hayhooks runtime by default or directly. The embedding dimension is pinned
//!   by config.
//! - [`agent`]: the bounded proposal agent. It grounds on a read-policy-filtered evidence
//!   subgraph and calls `generate` to produce an explanation and a proposed action. It never
//!   writes to the world model or a system of record.
//! - [`slate`]: composes the bounded agent with the [`Simulator`] and an injected [`Scorer`]
//!   into a propose, simulate, observe, revise loop that emits a ranked remediation slate.
//!
//! The capability specialization (prompt, action catalog, the scorer's rule) is injected, so
//! the crate names no use case.
//!
//! [`tools`] is the agent tool gateway: the read-only MCP surface a runtime agent reads the
//! world model through, with an engine-minted task token, under the profile's read context
//! and the task's scope.
//!
//! [`skills`] defines agent skills: the format of a skill (the repetitive procedure an
//! agent follows, as editable text), its content digest, and its validation against the
//! registered tool profiles.
//!
//! The tool-using agent runs as a reasoning step: the neutral [`reasoner`] seam with the
//! served runtime's implementation, and the [`agent_proposer`], a candidate proposer that
//! runs the agent through the gateway and takes the plan the gateway's run ledger recorded
//! as accepted, never the runtime's word.
//!
//! [`eval`] measures a plan proposer offline: a versioned set of fixed cases, deterministic
//! graders over the engine's record and the simulated outcome.
//!
//! Depends only on `ekg_domain`: the durable Restate wiring and the concrete
//! specialization live in the backend.

// Type/seam names naturally echo their module.
#![allow(clippy::module_name_repetitions)]
// Docs reference product and standard names (Hayhooks, OpenAI, SPARQL, ...).
#![allow(clippy::doc_markdown)]

pub mod agent;
pub mod agent_proposer;
pub mod eval;
mod evidence;
mod plan;
pub mod provider;
pub mod reasoner;
pub mod skills;
pub mod slate;
mod telemetry;
pub mod tools;

pub use ekg_domain::{CognitionProvider, Proposer, Scorer, Simulator, SlateProposer};

pub use agent::{
    ActionParser, AgentSpec, BoundedAgent, DEFAULT_MAX_PLAN_ACTIONS, GroundedSubject,
    GroundingExtension, GroupActionParser, GroupAgent, GroupPromptBuilder, MemberRouter,
    PromptBuilder,
};
pub use agent_proposer::{
    AgentLimits, AgentProposer, AlwaysProceed, AnalystRouter, AnalystTaskBuilder, DiscardRuns,
    GroupAgentProposer, GroupTaskBuilder, MemberAnalysts, RunGate, RunRecord, RunRecorder, RunRole,
    SkillPins, TaskBuilder,
};
pub use provider::{
    CognitionConfig, CognitionLimits, CognitionRoute, OpenAiCompatCognition,
    RUNTIME_GENERATE_PIPELINE,
};
pub use reasoner::{
    DEFAULT_ANALYST_RUN_CAP, DEFAULT_JOINT_RUN_CAP, DEFAULT_MAX_AGENT_STEPS,
    DEFAULT_MAX_ANALYST_STEPS, DEFAULT_MAX_JOINT_AGENT_STEPS, RUNTIME_AGENT_PIPELINE, Reasoner,
    ReasoningTask, RunReport, RuntimeReasoner, Termination,
};
pub use slate::{DEFAULT_MAX_CANDIDATES, DEFAULT_SLATE_BUDGET, GroupSlateAgent, SlateAgent};
