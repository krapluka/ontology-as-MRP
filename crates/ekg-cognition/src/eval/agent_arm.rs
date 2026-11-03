//! The agent arm: one trial is one run of the tool-using agent for the case's subject, or for
//! a joint case the analysts of its members and one planner.
//!
//! How a trial ended comes from the engine's record of the run, like the plan itself: a plan
//! is a plan only when `submit_plan` accepted it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ekg_domain::{CandidateProposer, EngineError, GroupCandidateProposer, ScoredCandidate};

use super::grade::{Arm, Trial, TrialEnd};
use super::set::PlansFor;
use crate::agent_proposer::{AgentProposer, GroupAgentProposer, RunRecord, RunRecorder, RunRole};
use crate::reasoner::Termination;
use crate::slate::DEFAULT_MAX_CANDIDATES;

/// An arm built for a subject was given a group, or the reverse: a wiring fault, never graded.
pub(crate) fn mismatch() -> EngineError {
    EngineError::Backend(
        "the arm does not plan for what the case names (a subject or a group)".to_owned(),
    )
}

/// Keeps the records of a trial's runs. Trials may run at the same time, so each has its own.
#[derive(Default)]
struct TrialRuns(Mutex<Vec<RunRecord>>);

impl TrialRuns {
    /// The run that speaks for the trial, if one planned, and the others.
    fn take(&self) -> (Option<RunRecord>, Vec<RunRecord>) {
        let mut runs = self
            .0
            .lock()
            .map(|mut runs| std::mem::take(&mut *runs))
            .unwrap_or_default();
        let planned = |run: &RunRecord| matches!(run.role, None | Some(RunRole::Planner));
        // A planner that declined outranks one that failed: it is the agent's word.
        let speaker = runs
            .iter()
            .position(|run| planned(run) && run.termination == Termination::NoPlan)
            .or_else(|| runs.iter().rposition(planned));
        let run = speaker.map(|at| runs.remove(at));
        (run, runs)
    }

    /// Whether the last attempt stopped on an investigator that handed in nothing.
    fn awaits_finding(&self) -> bool {
        self.0.lock().is_ok_and(|runs| {
            !runs.iter().any(|run| run.role == Some(RunRole::Planner))
                && runs
                    .iter()
                    .any(|run| run.role == Some(RunRole::Analyst) && run.finding.is_none())
        })
    }
}

impl RunRecorder for TrialRuns {
    fn record(&self, record: RunRecord) {
        if let Ok(mut runs) = self.0.lock() {
            runs.push(record);
        }
    }
}

/// The proposer an [`AgentArm`] runs: a subject's, or a group's.
enum Runs {
    Subject(AgentProposer),
    Group(GroupAgentProposer),
}

/// An [`Arm`] over an [`AgentProposer`], or over a [`GroupAgentProposer`] for joint cases.
pub struct AgentArm {
    runs: Runs,
}

impl AgentArm {
    /// The arm running `proposer`. Each trial replaces the proposer's recorder with one of
    /// its own, to keep that trial's run record.
    #[must_use]
    pub fn new(proposer: AgentProposer) -> Self {
        Self {
            runs: Runs::Subject(proposer),
        }
    }

    /// The arm running `proposer` for a group, as [`Self::new`] does for a subject.
    #[must_use]
    pub fn for_group(proposer: GroupAgentProposer) -> Self {
        Self {
            runs: Runs::Group(proposer),
        }
    }
}

#[async_trait]
impl Arm for AgentArm {
    fn name(&self) -> &'static str {
        "agent"
    }

    async fn trial(&self, plans_for: &PlansFor, tried: &[ScoredCandidate]) -> Trial {
        let runs = Arc::new(TrialRuns::default());
        let recorder = Arc::clone(&runs) as Arc<dyn RunRecorder>;
        let trial_runs = Arc::clone(&runs);
        let mut round = match (&self.runs, plans_for) {
            (Runs::Subject(proposer), PlansFor::Subject(subject)) => {
                let proposer = proposer.clone().with_recorder(recorder);
                vec![proposer.propose_candidate(subject, tried).await]
            }
            (Runs::Group(proposer), PlansFor::Group(group)) => {
                let proposer = proposer.clone().fresh().with_recorder(recorder);
                // A missing finding uses up an attempt, as on a slate, and the next one runs
                // only the investigators still missing.
                let mut left = DEFAULT_MAX_CANDIDATES;
                loop {
                    let room = proposer.planners().min(left);
                    let round = proposer.propose_group_round(group, tried, room).await;
                    left = left.saturating_sub(round.len());
                    if left == 0 || !trial_runs.awaits_finding() {
                        break round;
                    }
                }
            }
            (Runs::Subject(_), PlansFor::Group(_)) | (Runs::Group(_), PlansFor::Subject(_)) => {
                vec![Err(mismatch())]
            }
        };
        // Any accepted plan makes the trial a submission; the failures of the rest are in the
        // trial's run records.
        round.sort_by_key(Result::is_err);
        let mut candidates = Vec::new();
        let mut outcome = None;
        for result in round {
            match (result, outcome.is_some()) {
                (Ok(candidate), false) => outcome = Some(Ok(candidate)),
                (Ok(candidate), true) => candidates.push(candidate.proposal),
                (Err(why), false) => outcome = Some(Err(why)),
                (Err(_), true) => {}
            }
        }
        let outcome = outcome.unwrap_or_else(|| Err(mismatch()));
        let (run, other_runs) = runs.take();
        // An investigator that handed in nothing stopped the trial: its run is what ended it,
        // and it is no deliberate no-plan, whatever it ended as.
        let stopped_by = other_runs
            .iter()
            .find(|run| run.role == Some(RunRole::Analyst) && run.finding.is_none());
        let end = match (outcome, stopped_by, run.as_ref().map(|run| run.termination)) {
            (Ok(candidate), _, _) => TrialEnd::Submitted(candidate.proposal),
            (Err(EngineError::NothingToDo(_)), _, _) | (Err(_), _, Some(Termination::NoPlan)) => {
                TrialEnd::Declined
            }
            (Err(why), Some(analyst), None) => match analyst.termination {
                Termination::Error | Termination::Submitted => TrialEnd::Errored(why.to_string()),
                Termination::NoPlan | Termination::StepLimit | Termination::Length => {
                    TrialEnd::Exhausted
                }
            },
            (Err(_), _, Some(Termination::StepLimit | Termination::Length)) => TrialEnd::Exhausted,
            // The runtime says a plan was submitted and the engine recorded none: the run
            // cannot be trusted either way, so it is not graded.
            (Err(why), _, Some(Termination::Submitted)) => TrialEnd::Errored(format!(
                "the runtime reported a submitted plan the engine did not record: {why}"
            )),
            (Err(why), _, Some(Termination::Error) | None) => TrialEnd::Errored(why.to_string()),
        };
        Trial {
            end,
            run,
            other_runs,
            candidates,
        }
    }
}
