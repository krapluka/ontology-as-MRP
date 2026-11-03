//! The legacy arm: one trial is one attempt of a prompt-driven proposer, the path that
//! produces today's proposals, for the case's subject or for the members of a joint case.
//!
//! Such a proposer answers a refused reply and a reply without any action with the same
//! error. Which of the two it was depends on the capability's reply format, so the caller
//! injects that answer.

use std::sync::Arc;

use async_trait::async_trait;
use ekg_domain::{CandidateProposer, EngineError, GroupCandidateProposer, ScoredCandidate};

use super::agent_arm::mismatch;
use super::grade::{Arm, Trial, TrialEnd};
use super::set::PlansFor;

/// Whether the last reply proposed no action at all.
pub type Declined = Arc<dyn Fn() -> bool + Send + Sync>;

/// The proposer a [`PromptArm`] runs: a subject's, or a group's.
enum Runs {
    Subject(Arc<dyn CandidateProposer>),
    Group(Arc<dyn GroupCandidateProposer>),
}

/// An [`Arm`] over a [`CandidateProposer`], or over a [`GroupCandidateProposer`] for joint
/// cases. It keeps no run record.
pub struct PromptArm {
    runs: Runs,
    declined: Declined,
}

impl PromptArm {
    /// The arm running `proposer`; `declined` is asked after a reply was refused.
    #[must_use]
    pub fn new(proposer: Arc<dyn CandidateProposer>, declined: Declined) -> Self {
        Self {
            runs: Runs::Subject(proposer),
            declined,
        }
    }

    /// The arm running `proposer` for a group, as [`Self::new`] does for a subject.
    #[must_use]
    pub fn for_group(proposer: Arc<dyn GroupCandidateProposer>, declined: Declined) -> Self {
        Self {
            runs: Runs::Group(proposer),
            declined,
        }
    }
}

#[async_trait]
impl Arm for PromptArm {
    fn name(&self) -> &'static str {
        "legacy"
    }

    async fn trial(&self, plans_for: &PlansFor, tried: &[ScoredCandidate]) -> Trial {
        let outcome = match (&self.runs, plans_for) {
            (Runs::Subject(proposer), PlansFor::Subject(subject)) => {
                proposer.propose_candidate(subject, tried).await
            }
            (Runs::Group(proposer), PlansFor::Group(group)) => {
                proposer.propose_group_candidate(group, tried).await
            }
            (Runs::Subject(_), PlansFor::Group(_)) | (Runs::Group(_), PlansFor::Subject(_)) => {
                Err(mismatch())
            }
        };
        let end = match outcome {
            Ok(candidate) => TrialEnd::Submitted(candidate.proposal),
            Err(EngineError::InvalidProposal(_)) if (self.declined)() => TrialEnd::Declined,
            Err(EngineError::InvalidProposal(why)) => TrialEnd::Rejected(why),
            Err(why) => TrialEnd::Errored(why.to_string()),
        };
        Trial {
            end,
            run: None,
            other_runs: Vec::new(),
            candidates: Vec::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use ekg_domain::{
        ActionType, EntityUri, GroundedProposal, Plan, Proposal, ProposedAction, Result, Score,
        SubjectGroup,
    };

    use super::*;

    /// Answers every attempt with one scripted outcome and keeps the feedback it was given.
    struct Scripted {
        outcome: fn() -> Result<GroundedProposal>,
        seen: Mutex<Vec<ScoredCandidate>>,
    }

    #[async_trait]
    impl CandidateProposer for Scripted {
        async fn propose_candidate(
            &self,
            _subject: &EntityUri,
            feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            *self.seen.lock().expect("seen") = feedback.to_vec();
            (self.outcome)()
        }
    }

    fn order() -> PlansFor {
        PlansFor::Subject(EntityUri("x:order/1".to_owned()))
    }

    fn action() -> ProposedAction {
        ProposedAction {
            action_type: ActionType("move".to_owned()),
            target: EntityUri("x:order/1".to_owned()),
            params: std::collections::BTreeMap::new(),
        }
    }

    // A scripted outcome has the proposer's own return type.
    #[allow(clippy::unnecessary_wraps)]
    fn accepted() -> Result<GroundedProposal> {
        Ok(GroundedProposal {
            proposal: Proposal {
                explanation: "Moved.".to_owned(),
                actions: vec![action()],
                omissions: Vec::new(),
            },
            grounding_omissions: Vec::new(),
            grounding_edges: Vec::new(),
            subject_members: Vec::new(),
        })
    }

    fn refused() -> Result<GroundedProposal> {
        Err(EngineError::InvalidProposal("no such verb".to_owned()))
    }

    async fn end(outcome: fn() -> Result<GroundedProposal>, declined: bool) -> TrialEnd {
        let proposer = Arc::new(Scripted {
            outcome,
            seen: Mutex::new(Vec::new()),
        });
        let arm = PromptArm::new(proposer, Arc::new(move || declined));
        let trial = arm.trial(&order(), &[]).await;
        assert!(trial.run.is_none(), "the arm keeps no run record");
        trial.end
    }

    #[tokio::test]
    async fn an_accepted_candidate_is_a_submitted_plan() {
        let TrialEnd::Submitted(proposal) = end(accepted, false).await else {
            panic!("a plan was expected");
        };
        assert_eq!(proposal.actions, [action()]);
    }

    #[tokio::test]
    async fn a_refused_reply_declines_only_when_it_proposed_no_action() {
        assert_eq!(end(refused, true).await, TrialEnd::Declined);
        assert_eq!(
            end(refused, false).await,
            TrialEnd::Rejected("no such verb".to_owned())
        );
    }

    #[tokio::test]
    async fn a_backend_failure_is_an_error_whatever_the_last_reply_was() {
        let down = || Err(EngineError::Backend("the provider is down".to_owned()));
        for declined in [true, false] {
            assert!(
                matches!(end(down, declined).await, TrialEnd::Errored(why) if why.contains("down"))
            );
        }
    }

    #[tokio::test]
    async fn the_tried_plans_reach_the_proposer_under_the_legacy_name() {
        let proposer = Arc::new(Scripted {
            outcome: accepted,
            seen: Mutex::new(Vec::new()),
        });
        let arm = PromptArm::new(
            Arc::clone(&proposer) as Arc<dyn CandidateProposer>,
            Arc::new(|| false),
        );
        assert_eq!(arm.name(), "legacy");
        let tried = vec![ScoredCandidate {
            plan: Plan {
                actions: vec![action()],
            },
            score: Score {
                metrics: std::collections::BTreeMap::new(),
                rank_key: "2".to_owned(),
            },
        }];
        arm.trial(&order(), &tried).await;
        assert_eq!(*proposer.seen.lock().expect("seen"), tried);
    }

    #[async_trait]
    impl GroupCandidateProposer for Scripted {
        async fn propose_group_candidate(
            &self,
            _group: &SubjectGroup,
            feedback: &[ScoredCandidate],
        ) -> Result<GroundedProposal> {
            *self.seen.lock().expect("seen") = feedback.to_vec();
            (self.outcome)()
        }
    }

    #[tokio::test]
    async fn a_group_arm_plans_for_a_group_and_is_not_graded_on_a_subject() {
        let proposer = Arc::new(Scripted {
            outcome: accepted,
            seen: Mutex::new(Vec::new()),
        });
        let arm = PromptArm::for_group(proposer, Arc::new(|| false));
        let group = PlansFor::Group(SubjectGroup {
            key: "case/a".to_owned(),
            members: vec![
                EntityUri("x:order/1".to_owned()),
                EntityUri("x:mo/2".to_owned()),
            ],
            links: vec!["the one builds the other".to_owned()],
            worded: false,
        });
        assert!(matches!(
            arm.trial(&group, &[]).await.end,
            TrialEnd::Submitted(_)
        ));
        assert!(matches!(
            arm.trial(&order(), &[]).await.end,
            TrialEnd::Errored(why) if why.contains("a subject or a group")
        ));
    }
}
