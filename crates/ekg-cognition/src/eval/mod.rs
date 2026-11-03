//! Offline evals for whatever proposes plans: a versioned set of fixed cases, deterministic
//! graders, and pass^k per case and slice.
//!
//! - [`set`]: the eval set (a manifest and one file per case) and its loader.
//! - [`lock`]: the set's version and content digest, so a case cannot change silently.
//! - [`card`]: the dataset card's version and case list, checked against the set.
//! - [`grade`]: the [`Arm`] under evaluation, and the pure function that grades one trial from
//!   the engine's record and the plan's simulated score.
//! - [`agent_arm`]: the arm that runs the tool-using agent.
//! - [`prompt_arm`]: the legacy arm, one attempt of a prompt-driven proposer per trial.
//! - [`run`]: the loop over cases and trials, under a token bound.
//! - [`report`]: every trial's result, pass^k, and what the run was measured with.
//! - [`paired`]: two arms' reports of the same cases, side by side.
//!
//! The module names no capability: categories, profiles, metrics and action types are the
//! set's and the scorer's own strings. Building a case's world, and the arm that runs in it,
//! are injected ([`run::CaseEnv`]).

pub mod agent_arm;
pub mod card;
pub mod grade;
pub mod lock;
pub mod paired;
pub mod prompt_arm;
pub mod report;
pub mod run;
pub mod set;

pub use agent_arm::AgentArm;
pub use card::{CARD_FILE, card_defects};
pub use grade::{Arm, EngineChecks, Grade, Trial, TrialEnd, grade};
pub use lock::{LOCK_FILE, Lock, LockError, digest};
pub use paired::{ArmCase, PairVerdict, PairedCase, PairedReport};
pub use prompt_arm::{Declined, PromptArm};
pub use report::{
    CaseResult, ExperimentRef, GradeKind, Measures, Narrowing, Report, ReportedPlan, RunSettings,
    Slice, TrialResult, VersionTuple,
};
pub use run::{CaseEnv, Experiment, OpenCase, RunLimits, run_cases, run_cases_as};
pub use set::{
    CASES_DIR, Case, CaseAction, CasePlan, CaseRecord, Category, EntityRef, EvalSet, Expect,
    Forbid, Manifest, Outcome, PlansFor, ResolvedCase, SET_FILE, SetError,
};
