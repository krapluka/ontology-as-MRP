//! Which slates a monitor pass needs, and producing them.
//!
//! A portfolio's members are planned by the portfolio's joint slate, not their own: a
//! point freshly opened into a portfolio formed in the same pass gets no per-point slate, a
//! formed portfolio gets one joint slate for its generation, and a point the grouping left on
//! its own gets its own slate if it has none. When a joint slate cannot be produced or
//! recorded for a reason other than the portfolio being stale, each member without a slate
//! falls back to its own, so no point is left without a plan attempt.
//!
//! The durable reads and attaches go through the [`SlateSink`] seam (the Restate ingress in
//! production, a recording fake in tests), so the routing is testable without a runtime.
//! Producing a slate commits nothing: every option is a proposal.
//!
//! A freshly opened point may also get the agent's slate run beside it, while the rollout
//! allows it. In `shadow` that run is only recorded. In `agent` the existing path's slate is
//! held until the agent's run ends: the point gets the agent's slate when the run completed
//! and the rollout lets it through, and the existing path's slate in every other case, so no
//! point is left without a slate because of the agent.
//!
//! A formed portfolio's joint slate goes through the same step. The agent's joint slate is
//! recorded only, unless the `agent` mode was entered under criteria that cover joint slates:
//! then the existing path's joint slate is held as a point's is. A member that falls back to a
//! slate of its own, and a point released from a portfolio, are never offered to the agent.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ekg_cognition::{RunGate, RunRecord, RunRecorder, RunRole, Termination};
use ekg_domain::{
    ActionPointId, CouplingLink, EntityUri, GroupSlateProposer, Need, Slate, SlateProposer,
    SubjectGroup, Trace, TraceStep, TraceTag, WorldModel,
};
use ekg_dynamic::{FormedPortfolio, NotifyOutcome};
use ekg_kinetic::PolicyEngine;
use ekg_semantic::display;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::policy_context;
use crate::promotion::Version;
use crate::rollout::{
    AGENT_ROLLOUT_KEY, Claim, Mode, RolloutClient, RolloutState, RunReport, RunVerdict, SlateKind,
};
use crate::shadow::{
    Paired, PairedJointSlate, PairedSlate, ShadowClient, ShadowGroup, ShadowOutcome, ShadowRecord,
    Shown, VersionTuple,
};

/// A portfolio's coupling as the capability words it from the facts its formation recorded,
/// or `None` when no link carries facts (a portfolio formed before facts were recorded), in
/// which case the caller keeps the recorded reasons.
#[must_use]
pub fn worded_coupling(links: &[CouplingLink]) -> Option<Vec<String>> {
    links
        .iter()
        .any(|link| !link.facts.is_empty())
        .then(|| display::describe_coupling(links))
}

/// What attaching a joint slate did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioAttach {
    /// The slate was recorded for the portfolio's current generation.
    Landed,
    /// The portfolio was superseded or re-formed: the slate was refused (409).
    Stale,
    /// The attach failed for another reason.
    Failed,
}

/// The durable reads and attaches the slate step makes. Every method is best-effort and
/// logs its own failure; none commits anything.
#[async_trait]
pub trait SlateSink: Send + Sync {
    /// Whether `subject`'s point already carries a slate. `None` when it cannot be read.
    async fn point_has_slate(&self, subject: &EntityUri) -> Option<bool>;
    /// Whether portfolio `id` is active at `generation`. `None` when it cannot be read.
    async fn portfolio_current(&self, id: &str, generation: u32) -> Option<bool>;
    /// Attaches a slate to `subject`'s point; returns whether it landed.
    async fn attach_point_slate(&self, subject: &EntityUri, slate: &Slate) -> bool;
    /// Which point `subject` has and when its slate was attached. `None` when the point
    /// cannot be read or carries no slate.
    async fn point_attach(&self, subject: &EntityUri) -> Option<PointAttach>;
    /// Attaches a joint slate to portfolio `id` for `generation`.
    async fn attach_portfolio_slate(
        &self,
        id: &str,
        generation: u32,
        slate: &Slate,
    ) -> PortfolioAttach;
    /// When the joint slate portfolio `id` carries for `generation` was attached. `None` when
    /// the portfolio cannot be read or carries no slate for that generation.
    async fn portfolio_attach(&self, id: &str, generation: u32) -> Option<String>;
}

/// The point a slate was attached to, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointAttach {
    /// The point's id.
    pub action_point: ActionPointId,
    /// Which of the subject's points it is, counted from 1.
    pub point_seq: u32,
    /// When its slate was attached, as the point's record holds it.
    pub attached_at: String,
}

/// One slate the bridge runs for a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlateTask {
    /// The joint slate of a formed or re-formed portfolio.
    Portfolio(FormedPortfolio),
    /// The per-point slate of a freshly opened point that is in no formed portfolio.
    Point(EntityUri),
    /// The per-point slate of a point released to stand alone, if it has none.
    Released(EntityUri),
}

/// The slates a pass needs: with `joint` slates wired, one per formed portfolio, and a
/// per-point slate for each freshly opened subject that is not a member of one; without them,
/// a per-point slate for every freshly opened subject. Every released subject not already
/// covered gets a conditional per-point slate.
#[must_use]
pub fn slate_tasks(outcome: &NotifyOutcome, joint: bool) -> Vec<SlateTask> {
    let members: BTreeSet<&EntityUri> = if joint {
        outcome.formed.iter().flat_map(|p| &p.members).collect()
    } else {
        BTreeSet::new()
    };
    let mut tasks: Vec<SlateTask> = Vec::new();
    if joint {
        tasks.extend(outcome.formed.iter().cloned().map(SlateTask::Portfolio));
    }
    for subject in &outcome.opened {
        if !members.contains(subject) {
            tasks.push(SlateTask::Point(subject.clone()));
        }
    }
    for subject in &outcome.released {
        if !members.contains(subject) && !outcome.opened.contains(subject) {
            tasks.push(SlateTask::Released(subject.clone()));
        }
    }
    tasks
}

/// Fills each option's `needs` from the policy: gathers the cost and supplier context for the
/// option's plan, evaluates it, and records the reasons it cannot be auto-applied. Fails
/// closed: an unreadable context becomes a `context_unavailable` need instead of an empty
/// list.
pub async fn fill_needs(slate: &mut Slate, policy: &PolicyEngine, world: &dyn WorldModel) {
    for option in &mut slate.options {
        match policy_context::gather(world, &option.plan).await {
            Ok(ctx) => option.needs = policy.evaluate(&option.plan, &ctx).needs,
            Err(why) => {
                option.needs = vec![Need {
                    code: "context_unavailable".to_owned(),
                    detail: format!("policy context could not be read: {why}"),
                }];
            }
        }
    }
}

/// Runs a point's slate loop and fills the slate's policy needs. It attaches nothing.
///
/// # Errors
/// Why the slate could not be produced.
pub async fn prompt_slate(
    proposer: &dyn SlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    subject: &EntityUri,
) -> Result<Slate, String> {
    let mut slate = match proposer.propose_slate(subject).await {
        Ok(slate) => slate,
        Err(why) => {
            eprintln!(
                "warning: slate agent failed for {} ({why}); point gets no slate",
                subject.0
            );
            return Err(format!("the slate could not be produced: {why}"));
        }
    };
    fill_needs(&mut slate, policy, world).await;
    Ok(slate)
}

/// Attaches `slate` to `subject`'s point.
///
/// # Errors
/// That the attach did not land.
pub async fn attach_slate(
    sink: &dyn SlateSink,
    subject: &EntityUri,
    slate: &Slate,
) -> Result<(), String> {
    if !sink.attach_point_slate(subject, slate).await {
        eprintln!("warning: slate attach did not land for {}", subject.0);
        return Err("the slate attach did not land".to_owned());
    }
    Ok(())
}

/// Runs a point's slate loop and attaches the slate, with its policy needs. Best-effort: a
/// generation or attach failure logs and leaves the point without a slate. Returns the slate
/// as attached.
///
/// # Errors
/// Why the point got no slate: it could not be produced, or the attach did not land.
pub async fn point_slate(
    sink: &dyn SlateSink,
    proposer: &dyn SlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    subject: &EntityUri,
) -> Result<Slate, String> {
    let slate = prompt_slate(proposer, policy, world, subject).await?;
    attach_slate(sink, subject, &slate).await?;
    Ok(slate)
}

/// A released point's slate: its own, only when it carries none (an unreadable record is
/// logged and skipped).
pub async fn released_slate(
    sink: &dyn SlateSink,
    proposer: &dyn SlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    subject: &EntityUri,
) {
    match sink.point_has_slate(subject).await {
        Some(false) => {
            let _ = point_slate(sink, proposer, policy, world, subject).await;
        }
        Some(true) => {}
        None => eprintln!(
            "warning: could not read {}'s point; its slate is left as it is",
            subject.0
        ),
    }
}

/// What the joint slate step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortfolioSlateOutcome {
    /// The joint slate landed for the generation.
    Attached,
    /// The portfolio was already stale (before the model call, or at attach): nothing more.
    Stale,
    /// The joint slate could not be produced or recorded: these members carry no slate and
    /// fall back to their own.
    Fallback(Vec<EntityUri>),
}

/// The group a formed portfolio's joint slate plans for: its members, with the coupling as the
/// capability words it, or the recorded reasons when no link carries facts.
fn joint_group(formed: &FormedPortfolio) -> SubjectGroup {
    let worded = worded_coupling(&formed.coupling);
    SubjectGroup {
        key: formed.id.0.clone(),
        members: formed.members.clone(),
        worded: worded.is_some(),
        links: worded.unwrap_or_else(|| formed.links.clone()),
    }
}

/// Whether a joint slate is still wanted for `formed`: `Ok` when it is active at its
/// generation, the step's outcome otherwise (stale, or unreadable and so a fallback).
async fn joint_wanted(
    sink: &dyn SlateSink,
    formed: &FormedPortfolio,
) -> Result<(), PortfolioSlateOutcome> {
    let id = formed.id.0.as_str();
    match sink.portfolio_current(id, formed.generation).await {
        Some(true) => Ok(()),
        Some(false) => {
            println!(
                "portfolio {id} is no longer current at generation {}; joint slate skipped",
                formed.generation
            );
            Err(PortfolioSlateOutcome::Stale)
        }
        None => {
            eprintln!(
                "warning: portfolio {id} could not be read; members fall back to their own slates"
            );
            Err(fallback(sink, formed).await)
        }
    }
}

/// Runs a portfolio's joint slate loop and fills the slate's policy needs. It attaches
/// nothing.
///
/// # Errors
/// Why the joint slate could not be produced.
async fn prompt_joint_slate(
    proposer: &dyn GroupSlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    formed: &FormedPortfolio,
) -> Result<Slate, String> {
    let mut slate = match proposer.propose_group_slate(&joint_group(formed)).await {
        Ok(slate) => slate,
        Err(why) => {
            eprintln!("warning: joint slate failed for {} ({why})", formed.id.0);
            return Err(format!("the joint slate could not be produced: {why}"));
        }
    };
    fill_needs(&mut slate, policy, world).await;
    Ok(slate)
}

/// Attaches `slate` to `formed` for its generation and says what that did. A stale attach
/// (409) ends the step; any other failure names the members without a slate for the fallback.
async fn attach_joint_slate(
    sink: &dyn SlateSink,
    formed: &FormedPortfolio,
    slate: &Slate,
) -> PortfolioSlateOutcome {
    let id = formed.id.0.as_str();
    match sink
        .attach_portfolio_slate(id, formed.generation, slate)
        .await
    {
        PortfolioAttach::Landed => PortfolioSlateOutcome::Attached,
        PortfolioAttach::Stale => {
            println!(
                "joint slate for {id} generation {} arrived stale; refused",
                formed.generation
            );
            PortfolioSlateOutcome::Stale
        }
        PortfolioAttach::Failed => {
            eprintln!(
                "warning: joint slate attach failed for {id}; members fall back to their own"
            );
            fallback(sink, formed).await
        }
    }
}

/// The joint slate step for a formed portfolio: skip a portfolio that is no longer active at
/// its generation (no model call is wasted), produce the joint slate, fill its needs, and
/// attach it for the generation. A stale attach (409) ends the step; any other failure names
/// the members without a slate for the fallback.
pub async fn portfolio_slate(
    sink: &dyn SlateSink,
    proposer: &dyn GroupSlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    formed: &FormedPortfolio,
) -> PortfolioSlateOutcome {
    if let Err(outcome) = joint_wanted(sink, formed).await {
        return outcome;
    }
    recorded_joint_slate(sink, proposer, policy, world, formed)
        .await
        .0
}

/// Produces and attaches the joint slate of `formed`: what that did, and the slate as recorded
/// with the portfolio, or why none was.
async fn recorded_joint_slate(
    sink: &dyn SlateSink,
    proposer: &dyn GroupSlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    formed: &FormedPortfolio,
) -> (PortfolioSlateOutcome, Result<Slate, String>) {
    let slate = match prompt_joint_slate(proposer, policy, world, formed).await {
        Ok(slate) => slate,
        Err(why) => {
            eprintln!(
                "warning: members of {} fall back to their own slates",
                formed.id.0
            );
            return (fallback(sink, formed).await, Err(why));
        }
    };
    let outcome = attach_joint_slate(sink, formed, &slate).await;
    let recorded = match &outcome {
        PortfolioSlateOutcome::Attached => Ok(slate),
        PortfolioSlateOutcome::Stale => Err(format!(
            "the joint slate for generation {} arrived stale and was refused",
            formed.generation
        )),
        PortfolioSlateOutcome::Fallback(_) => {
            Err("the joint slate could not be recorded with the portfolio".to_owned())
        }
    };
    (outcome, recorded)
}

/// The members of `formed` whose point carries no slate (an unreadable one included, so it
/// still gets a plan attempt).
async fn fallback(sink: &dyn SlateSink, formed: &FormedPortfolio) -> PortfolioSlateOutcome {
    let mut members = Vec::new();
    for member in &formed.members {
        if sink.point_has_slate(member).await != Some(true) {
            members.push(member.clone());
        }
    }
    PortfolioSlateOutcome::Fallback(members)
}

/// The [`SlateSink`] over the Restate ingress: the action-point and portfolio objects'
/// `record` and `attach_slate` handlers.
pub struct IngressSink {
    client: reqwest::Client,
    ingress: String,
}

impl IngressSink {
    /// A sink over the ingress at `ingress`.
    #[must_use]
    pub fn new(client: reqwest::Client, ingress: impl Into<String>) -> Self {
        Self {
            client,
            ingress: ingress.into(),
        }
    }

    /// `POST {ingress}/{object}/{key}/{handler}` with an optional JSON body: the status and
    /// the body, or `None` when the ingress is unreachable.
    async fn call(
        &self,
        object: &str,
        key: &str,
        handler: &str,
        body: Option<Vec<u8>>,
    ) -> Option<(u16, String)> {
        let mut url = reqwest::Url::parse(&self.ingress).ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend([object, key, handler]);
        let mut request = self.client.post(url);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        match request.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                Some((status, resp.text().await.unwrap_or_default()))
            }
            Err(why) => {
                eprintln!(
                    "warning: slate step: ingress unreachable for {object}/{key}/{handler} ({why})"
                );
                None
            }
        }
    }

    /// A handler's JSON answer, or `None` on a failure.
    async fn read(&self, object: &str, key: &str) -> Option<Value> {
        let (status, text) = self.call(object, key, "record", None).await?;
        if !(200..300).contains(&status) {
            eprintln!("warning: slate step: {object}/{key}/record answered HTTP {status}");
            return None;
        }
        serde_json::from_str(&text).ok()
    }
}

#[async_trait]
impl SlateSink for IngressSink {
    async fn point_has_slate(&self, subject: &EntityUri) -> Option<bool> {
        let record = self.read("ActionPointService", &subject.0).await?;
        Some(record.get("slate").is_some_and(|slate| !slate.is_null()))
    }

    async fn portfolio_current(&self, id: &str, generation: u32) -> Option<bool> {
        let record = self.read("PortfolioService", id).await?;
        Some(
            record["state"] == "active"
                && record["generation"].as_u64() == Some(u64::from(generation)),
        )
    }

    async fn attach_point_slate(&self, subject: &EntityUri, slate: &Slate) -> bool {
        let Ok(body) = serde_json::to_vec(slate) else {
            eprintln!(
                "warning: slate step could not encode a slate for {}",
                subject.0
            );
            return false;
        };
        match self
            .call("ActionPointService", &subject.0, "attach_slate", Some(body))
            .await
        {
            Some((status, _)) if (200..300).contains(&status) => true,
            Some((status, _)) => {
                eprintln!(
                    "warning: slate step: slate attach rejected for {} (HTTP {status})",
                    subject.0
                );
                false
            }
            None => false,
        }
    }

    async fn point_attach(&self, subject: &EntityUri) -> Option<PointAttach> {
        let record = self.read("ActionPointService", &subject.0).await?;
        let record: ekg_dynamic::ActionPointRecord = serde_json::from_value(record).ok()?;
        Some(PointAttach {
            action_point: record.point.as_ref()?.id.clone(),
            point_seq: record.points_opened,
            attached_at: record.slate_attached_at?,
        })
    }

    async fn attach_portfolio_slate(
        &self,
        id: &str,
        generation: u32,
        slate: &Slate,
    ) -> PortfolioAttach {
        let Ok(body) = serde_json::to_vec(&json!({ "generation": generation, "slate": slate }))
        else {
            eprintln!("warning: slate step could not encode the joint slate for {id}");
            return PortfolioAttach::Failed;
        };
        match self
            .call("PortfolioService", id, "attach_slate", Some(body))
            .await
        {
            Some((status, _)) if (200..300).contains(&status) => PortfolioAttach::Landed,
            Some((409, _)) => PortfolioAttach::Stale,
            Some((status, text)) => {
                eprintln!(
                    "warning: slate step: joint slate attach for {id} answered HTTP {status}: {text}"
                );
                PortfolioAttach::Failed
            }
            None => PortfolioAttach::Failed,
        }
    }

    async fn portfolio_attach(&self, id: &str, generation: u32) -> Option<String> {
        let record = self.read("PortfolioService", id).await?;
        let record: ekg_dynamic::PortfolioRecord = serde_json::from_value(record).ok()?;
        record
            .slate_attached_at
            .filter(|_| record.slate_generation == Some(generation))
    }
}

// --- shadow slates ---

/// The rollout calls a shadow slate makes (the `AgentRollout` object over the ingress in
/// production). Best-effort: an unreachable rollout claims nothing and allows nothing.
#[async_trait]
pub trait Rollout: Send + Sync {
    /// Claims one shadow slate of `kind`; `None` when the rollout cannot be reached.
    async fn claim_slate(&self, kind: SlateKind) -> Option<Claim>;
    /// The rollout state; `None` when it cannot be read.
    async fn state(&self) -> Option<RolloutState>;
    /// Reports the tokens a shadow slate's runs used.
    async fn report_tokens(&self, tokens: u64);
    /// Reports an agent run whose slate would be its point's, and asks whether it may be.
    /// `None` when the rollout cannot be reached.
    async fn report_run(&self, report: &RunReport) -> Option<RunVerdict>;
    /// The key the rollout runs under.
    fn key(&self) -> String;
}

/// Where shadow records go (the `AgentShadow` object in production), never the point.
#[async_trait]
pub trait ShadowStore: Send + Sync {
    /// Keeps `record`; returns whether it landed.
    async fn record(&self, record: &ShadowRecord) -> bool;
}

#[async_trait]
impl Rollout for RolloutClient {
    async fn claim_slate(&self, kind: SlateKind) -> Option<Claim> {
        RolloutClient::claim_slate(self, kind)
            .await
            .map_err(|why| eprintln!("warning: shadow: the rollout could not be claimed ({why})"))
            .ok()
    }

    async fn state(&self) -> Option<RolloutState> {
        self.get()
            .await
            .map_err(|why| eprintln!("warning: shadow: the rollout could not be read ({why})"))
            .ok()
    }

    async fn report_tokens(&self, tokens: u64) {
        if let Err(why) = RolloutClient::report_tokens(self, tokens).await {
            eprintln!("warning: shadow: {tokens} tokens could not be reported ({why})");
        }
    }

    async fn report_run(&self, report: &RunReport) -> Option<RunVerdict> {
        RolloutClient::report_run(self, report)
            .await
            .map_err(|why| eprintln!("warning: agent: a run could not be reported ({why})"))
            .ok()
    }

    fn key(&self) -> String {
        RolloutClient::key(self).to_owned()
    }
}

#[async_trait]
impl ShadowStore for ShadowClient {
    async fn record(&self, record: &ShadowRecord) -> bool {
        match ShadowClient::record(self, record).await {
            Ok(()) => true,
            Err(why) => {
                eprintln!(
                    "warning: shadow: the record for {} did not land ({why})",
                    record.key()
                );
                false
            }
        }
    }
}

/// The per-run hooks a shadow slate's agent proposers are built with: the kill-switch gate
/// and the recorder of each candidate's run.
#[derive(Clone)]
pub struct ShadowRun {
    /// Asked before every candidate.
    pub gate: Arc<dyn RunGate>,
    /// Keeps every candidate's run record.
    pub recorder: Arc<dyn RunRecorder>,
}

/// What the slate step did with a point or a portfolio, as the agent's run records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    /// The existing path's slate with the point or portfolio it belongs to, or why there is
    /// none.
    pub paired: Result<Paired, String>,
    /// Which slate the point or portfolio got, in `agent` mode.
    pub shown: Option<Shown>,
}

/// What an agent slate is run for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowTarget {
    /// A freshly opened point's subject.
    Point(EntityUri),
    /// A formed portfolio, at the generation its joint slate is for.
    Portfolio {
        /// The members and their coupling.
        group: SubjectGroup,
        /// The generation.
        generation: u32,
    },
}

impl ShadowTarget {
    fn kind(&self) -> SlateKind {
        match self {
            Self::Point(_) => SlateKind::Point,
            Self::Portfolio { .. } => SlateKind::Joint,
        }
    }

    /// The subject a record of this is filed with: the point's, or the first member.
    fn subject(&self) -> EntityUri {
        match self {
            Self::Point(subject) => subject.clone(),
            Self::Portfolio { group, .. } => group
                .members
                .first()
                .cloned()
                .unwrap_or_else(|| EntityUri(group.key.clone())),
        }
    }

    /// The portfolio a joint record names.
    fn group(&self) -> Option<ShadowGroup> {
        match self {
            Self::Point(_) => None,
            Self::Portfolio { group, generation } => Some(ShadowGroup {
                portfolio: group.key.clone(),
                generation: *generation,
                members: group.members.clone(),
            }),
        }
    }

    /// What the existing path's slate was for, in a reason.
    fn noun(&self) -> &'static str {
        match self {
            Self::Point(_) => "point",
            Self::Portfolio { .. } => "portfolio",
        }
    }
}

/// How an agent run learns what the slate step attached.
pub type PairedHandover = tokio::sync::oneshot::Receiver<Attached>;

/// What an agent run answers the slate step that holds the existing path's slate for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDecision {
    /// The run completed and the rollout let it through: this is the point's slate.
    Show {
        /// The agent's slate, as produced.
        slate: Slate,
        /// What it ran with.
        version: VersionTuple,
    },
    /// The point gets the existing path's slate.
    Fallback {
        /// Why.
        reason: String,
    },
    /// The claim was granted in `shadow`: the run only records.
    Shadow,
}

/// The slate step's wait for an agent run in `agent` mode.
pub struct Hold {
    /// Where the run answers.
    pub decided: tokio::sync::oneshot::Sender<AgentDecision>,
    /// When the run is abandoned, so that its answer still reaches the step in time.
    pub deadline: tokio::time::Instant,
}

/// Builds the agent's (routed) slate proposer for one shadow run.
pub type ShadowSlates = Arc<dyn Fn(ShadowRun) -> Arc<dyn SlateProposer> + Send + Sync>;

/// Builds the agent's joint slate proposer for one shadow run.
pub type JointShadowSlates = Arc<dyn Fn(ShadowRun) -> Arc<dyn GroupSlateProposer> + Send + Sync>;

/// The shadow branch of the slate step: the rollout, the shadow store, the agent slate
/// builders, shadow's own concurrency limit, and the outer guards.
#[derive(Clone)]
pub struct ShadowStep {
    /// The rollout mode a shadow slate claims and checks.
    pub rollout: Arc<dyn Rollout>,
    /// Where its records go.
    pub store: Arc<dyn ShadowStore>,
    /// The agent slate, per run.
    pub slates: ShadowSlates,
    /// The agent's joint slate, per run.
    pub joint: JointShadowSlates,
    /// Shadow's own permits (`EKG_AGENT_SHADOW_CONCURRENCY`), apart from the legacy path's.
    pub permits: Arc<Semaphore>,
    /// How long a point's shadow slate may run before it is abandoned (recorded `failed`).
    pub guard: std::time::Duration,
    /// The same for a portfolio's: a joint run has a cap of its own, so its slate has a guard
    /// of its own.
    pub joint_guard: std::time::Duration,
}

impl ShadowStep {
    /// The guard over the agent's slate for `target`.
    fn guard_for(&self, target: &ShadowTarget) -> std::time::Duration {
        match target {
            ShadowTarget::Point(_) => self.guard,
            ShadowTarget::Portfolio { .. } => self.joint_guard,
        }
    }
}

/// The kill switch for one shadow slate: it proceeds while the rollout still allows the epoch
/// the slate was claimed in, and remembers a stop. An unreadable rollout stops it (fail closed).
struct EpochGate {
    rollout: Arc<dyn Rollout>,
    epoch: u32,
    stopped: AtomicBool,
}

#[async_trait]
impl RunGate for EpochGate {
    async fn proceed(&self) -> bool {
        let allowed = self
            .rollout
            .state()
            .await
            .is_some_and(|state| state.allows(self.epoch));
        if !allowed {
            self.stopped.store(true, Ordering::SeqCst);
        }
        allowed
    }
}

/// Collects a shadow slate's run records.
#[derive(Default)]
struct CollectedRuns(Mutex<Vec<RunRecord>>);

impl RunRecorder for CollectedRuns {
    fn record(&self, record: RunRecord) {
        if let Ok(mut runs) = self.0.lock() {
            // A shadow record keeps how a run went, not what the model wrote.
            runs.push(RunRecord {
                closing_answer: None,
                ..record
            });
        }
    }
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// What of `guard` is kept back at its end, twice: once for the rollout to answer a run that
/// was cut off, once for the attach. So a held slate lands inside the guard the step runs under.
fn hold_reserve(guard: std::time::Duration) -> std::time::Duration {
    (guard / 4).min(std::time::Duration::from_secs(10))
}

/// The trace step that says the agent produced a point's slate, and with which versions. The
/// slate step adds it, so it does not depend on what the model wrote.
#[must_use]
pub fn agent_trace_step(version: &VersionTuple) -> TraceStep {
    agent_step(version, "point")
}

/// [`agent_trace_step`] for the slate of a `what` (a point, or a portfolio).
fn agent_step(version: &VersionTuple, what: &str) -> TraceStep {
    let skills: Vec<String> = version
        .skills
        .iter()
        .map(|(name, version)| format!("{name}@{version}"))
        .collect();
    TraceStep {
        tag: TraceTag::Assumption,
        text: format!(
            "This slate was produced by the tool-using agent (engine {}, model {}, skills {}). \
             The prompt-based slate for the same {what} is recorded beside it.",
            version.engine,
            if version.model.is_empty() {
                "none reported"
            } else {
                &version.model
            },
            if skills.is_empty() {
                "none".to_owned()
            } else {
                skills.join(" ")
            }
        ),
        rejected: false,
    }
}

/// The version tuple of a shadow run: the engine's version, the model the runs reported,
/// and the skill pins and tools their tokens carried.
fn version_of(runs: &[RunRecord]) -> VersionTuple {
    VersionTuple {
        engine: env!("CARGO_PKG_VERSION").to_owned(),
        model: runs
            .iter()
            .map(|run| run.model.as_str())
            .find(|model| !model.is_empty())
            .unwrap_or_default()
            .to_owned(),
        skills: runs
            .iter()
            .flat_map(|run| run.skills.clone())
            .collect::<BTreeMap<_, _>>(),
        tools: runs
            .iter()
            .find(|run| matches!(run.role, None | Some(RunRole::Planner)))
            .or(runs.first())
            .map(|run| run.tools.clone())
            .unwrap_or_default(),
    }
}

/// Runs one agent slate for `target` under shadow's own permit: claims it (a refusal runs no
/// agent), runs the agent's slate under the guard with a gate on the claimed epoch, records the
/// outcome in the shadow store (`completed`, `failed`, or `aborted` when the gate stopped it)
/// and reports its tokens. It takes no [`SlateSink`], so it cannot fill needs or touch a point
/// or a portfolio; what the slate step attached is handed to it through `paired`, which it
/// waits for after its own run. Returns the record it wrote, if any.
///
/// With `hold`, the slate step is holding the existing path's slate for this run. The run then
/// reports to the rollout before it answers, and the rollout decides whether its slate may be
/// the one shown.
pub async fn shadow_slate(
    step: &ShadowStep,
    target: &ShadowTarget,
    paired: PairedHandover,
    hold: Option<Hold>,
) -> Option<ShadowRecord> {
    let Ok(_permit) = step.permits.acquire().await else {
        return None;
    };
    let rollout = Some(step.rollout.key()).filter(|key| key != AGENT_ROLLOUT_KEY);
    let (epoch, slate_no, authoritative) = match step.rollout.claim_slate(target.kind()).await? {
        Claim::Granted {
            epoch,
            slate,
            authoritative,
        } => (epoch, slate, authoritative),
        // The mode is no longer `agent`: there is no run to record.
        Claim::Refused { reason } if reason == "legacy" => return None,
        Claim::Refused { reason } => {
            return Some(not_started(step, target, paired, hold?, &reason, rollout).await);
        }
    };
    // A claim granted as a recorded-only run only records, whatever the step expected.
    let hold = match hold {
        Some(hold) if !authoritative => {
            let _ = hold.decided.send(AgentDecision::Shadow);
            None
        }
        hold => hold,
    };
    let gate = Arc::new(EpochGate {
        rollout: Arc::clone(&step.rollout),
        epoch,
        stopped: AtomicBool::new(false),
    });
    let runs = Arc::new(CollectedRuns::default());
    let hooks = ShadowRun {
        gate: Arc::clone(&gate) as Arc<dyn RunGate>,
        recorder: Arc::clone(&runs) as Arc<dyn RunRecorder>,
    };
    let started_at = unix_now();
    let run = async {
        match target {
            ShadowTarget::Point(subject) => (step.slates)(hooks).propose_slate(subject).await,
            ShadowTarget::Portfolio { group, .. } => {
                (step.joint)(hooks).propose_group_slate(group).await
            }
        }
    };
    let guard = step.guard_for(target);
    let produced = match &hold {
        Some(hold) => tokio::time::timeout_at(hold.deadline, run).await,
        None => tokio::time::timeout(guard, run).await,
    };
    let ended_at = unix_now();
    let stopped = gate.stopped.load(Ordering::SeqCst);
    let (outcome, reason, slate) = run_ending(produced, stopped, hold.is_some(), guard);
    let runs = runs.0.lock().map(|runs| runs.clone()).unwrap_or_default();
    let tokens = runs
        .iter()
        .map(|run| run.input_tokens.saturating_add(run.output_tokens))
        .fold(0_u64, u64::saturating_add);
    let mut record = ShadowRecord {
        run_id: format!("e{epoch}-s{slate_no}"),
        subject: target.subject(),
        group: target.group(),
        started_at,
        ended_at,
        outcome,
        reason,
        slate,
        version: version_of(&runs),
        runs,
        paired: None,
        paired_missing: None,
        decision: None,
        shown: None,
        epoch: Some(epoch),
        rollout,
    };
    let answered = hold.is_some();
    if let Some(hold) = hold {
        // The step may have given up waiting; it then says so in what it hands over.
        let _ = hold
            .decided
            .send(decide(step.rollout.as_ref(), &record, epoch, tokens).await);
    }
    let attached = handed_over(paired).await;
    record.shown = match attached.shown {
        // The mode became `agent` between the step's read and this claim.
        None if authoritative && !answered => Some(Shown::Legacy {
            reason: format!(
                "the {}'s slate was attached before the mode was agent",
                target.noun()
            ),
        }),
        shown => shown,
    };
    record.paired = attached.paired.clone().ok();
    record.paired_missing = attached.paired.err();
    step.store.record(&record).await;
    if !answered {
        report_recorded_only(step.rollout.as_ref(), target, &record, epoch, tokens).await;
    }
    Some(record)
}

/// Reports a run that was only recorded. A point's run reports its tokens. A joint run reports
/// as a run: it still counts for the version and the failures in `agent` mode, and in `shadow`
/// the rollout takes the tokens and nothing else.
async fn report_recorded_only(
    rollout: &dyn Rollout,
    target: &ShadowTarget,
    record: &ShadowRecord,
    epoch: u32,
    tokens: u64,
) {
    match target {
        ShadowTarget::Point(_) => rollout.report_tokens(tokens).await,
        ShadowTarget::Portfolio { .. } => {
            let _ = rollout.report_run(&run_report(record, epoch, tokens)).await;
        }
    }
}

/// How an agent slate's run ended: what it `produced` within its time, whether the gate
/// `stopped` it, and for a run that ran out of time whether the step was holding a slate for it.
fn run_ending(
    produced: Result<ekg_domain::Result<Slate>, tokio::time::error::Elapsed>,
    stopped: bool,
    held: bool,
    guard: std::time::Duration,
) -> (ShadowOutcome, Option<String>, Option<Slate>) {
    match produced {
        Ok(Ok(slate)) if stopped => (
            ShadowOutcome::Aborted,
            Some("the rollout mode was switched; no further candidate ran".to_owned()),
            Some(slate),
        ),
        Ok(Ok(slate)) => (ShadowOutcome::Completed, None, Some(slate)),
        Ok(Err(why)) => (ShadowOutcome::Failed, Some(why.to_string()), None),
        Err(_) => (
            ShadowOutcome::Failed,
            Some(if held {
                "the agent's slate was not ready within its guard".to_owned()
            } else {
                format!("the shadow slate exceeded {guard:?}")
            }),
            None,
        ),
    }
}

/// Whether any candidate of `record` ran the agent to an end. The slate loop sets a failed
/// candidate aside and still returns a slate, so a slate alone does not show that the agent
/// ran: when every candidate run ended in an error (the runtime or the model was unreachable),
/// the run failed. An analyst's run does not show it either: only a run that planned does.
fn agent_ran(runs: &[RunRecord]) -> bool {
    runs.iter()
        .filter(|run| matches!(run.role, None | Some(RunRole::Planner)))
        .any(|run| run.termination != Termination::Error)
}

/// What the run of `record`, claimed in `epoch`, reports to the rollout.
fn run_report(record: &ShadowRecord, epoch: u32, tokens: u64) -> RunReport {
    RunReport {
        epoch,
        tokens,
        completed: record.outcome == ShadowOutcome::Completed && agent_ran(&record.runs),
        version: Version {
            engine: record.version.engine.clone(),
            model: record.version.model.clone(),
            skills: record.version.skills.clone(),
        },
    }
}

/// Reports the run of `record` to the rollout and says whether its slate may be the point's.
/// The rollout is asked before anything is attached: it may roll the mode back on this very
/// report.
///
/// A run none of whose candidates ran the agent to an end has failed ([`agent_ran`]), and its
/// empty slate is not the agent's answer.
async fn decide(
    rollout: &dyn Rollout,
    record: &ShadowRecord,
    epoch: u32,
    tokens: u64,
) -> AgentDecision {
    let ran = agent_ran(&record.runs);
    let verdict = rollout.report_run(&run_report(record, epoch, tokens)).await;
    let why = record.reason.as_deref();
    let reason = match (record.outcome, record.slate.as_ref(), verdict) {
        (ShadowOutcome::Completed, Some(slate), Some(RunVerdict::Proceed)) if ran => {
            return AgentDecision::Show {
                slate: slate.clone(),
                version: record.version.clone(),
            };
        }
        (_, _, Some(RunVerdict::RolledBack { reason })) => {
            format!("the mode was rolled back: {reason}")
        }
        (ShadowOutcome::Aborted, ..) => format!(
            "the agent's run was stopped: {}",
            why.unwrap_or("the rollout mode was switched")
        ),
        (ShadowOutcome::Failed, ..) => format!(
            "the agent's run failed: {}",
            why.unwrap_or("no slate was produced")
        ),
        (ShadowOutcome::Completed, ..) if !ran => {
            "the agent's run failed: every candidate run ended in an error".to_owned()
        }
        (ShadowOutcome::Completed, _, Some(RunVerdict::Stale)) => {
            "the rollout mode was switched before the agent's slate was ready".to_owned()
        }
        (ShadowOutcome::Completed, _, None) => {
            "the rollout could not be asked whether the agent's slate may be shown".to_owned()
        }
        (ShadowOutcome::Completed, ..) => {
            "the rollout did not let the agent's slate through".to_owned()
        }
    };
    AgentDecision::Fallback { reason }
}

/// The record of a point or portfolio in `agent` mode whose agent run was not allowed to start:
/// the window closed or a budget is spent. The step is told at once, so the existing path's
/// slate is the one shown.
async fn not_started(
    step: &ShadowStep,
    target: &ShadowTarget,
    paired: PairedHandover,
    hold: Hold,
    refusal: &str,
    rollout: Option<String>,
) -> ShadowRecord {
    let now = unix_now();
    let reason = format!("no agent run was allowed to start: {refusal}");
    let _ = hold.decided.send(AgentDecision::Fallback {
        reason: reason.clone(),
    });
    let attached = handed_over(paired).await;
    let record = ShadowRecord {
        run_id: format!("refused-{now}"),
        subject: target.subject(),
        group: target.group(),
        started_at: now,
        ended_at: now,
        outcome: ShadowOutcome::Aborted,
        reason: Some(reason),
        slate: None,
        version: version_of(&[]),
        runs: Vec::new(),
        paired: attached.paired.clone().ok(),
        paired_missing: attached.paired.err(),
        decision: None,
        shown: attached.shown,
        epoch: None,
        rollout,
    };
    step.store.record(&record).await;
    record
}

/// What the slate step attached. The sender is dropped with the step's task, so this wait ends
/// with it.
async fn handed_over(paired: PairedHandover) -> Attached {
    paired.await.unwrap_or_else(|_| Attached {
        paired: Err("the existing path's slate was abandoned".to_owned()),
        shown: None,
    })
}

/// The existing path's slate as a shadow record pairs it: with the point it is attached to.
async fn paired_slate(
    sink: &dyn SlateSink,
    subject: &EntityUri,
    slate: Result<Slate, String>,
) -> Result<Paired, String> {
    let slate = slate?;
    let point = sink
        .point_attach(subject)
        .await
        .ok_or("the point could not be read after the attach")?;
    Ok(Paired::Point(PairedSlate {
        action_point: point.action_point,
        point_seq: point.point_seq,
        slate,
        attached_at: point.attached_at,
    }))
}

/// The existing path's joint slate as a shadow record pairs it: with the portfolio and the
/// generation it was produced for, and when that generation's slate was attached.
async fn paired_joint_slate(
    sink: &dyn SlateSink,
    formed: &FormedPortfolio,
    slate: Result<Slate, String>,
) -> Result<Paired, String> {
    let slate = slate?;
    let attached_at = sink
        .portfolio_attach(&formed.id.0, formed.generation)
        .await
        .ok_or("the portfolio could not be read after the attach")?;
    Ok(Paired::Joint(PairedJointSlate {
        portfolio: formed.id.0.clone(),
        generation: formed.generation,
        slate,
        attached_at,
    }))
}

/// A freshly opened point's slates: its legacy slate and, when shadow is wired, the agent's
/// slate in its own spawned task. The agent's task is handed no sink. Returns that task, if one
/// was spawned.
///
/// - In `legacy` and `shadow` the legacy slate attaches as soon as it is ready, so a slow or
///   failing agent run cannot delay or change it.
/// - In `agent` the legacy slate is held until the agent's run answers or the wait runs out,
///   and then exactly one of the two is attached: the agent's when its run completed and the
///   rollout let it through, the legacy one otherwise. The legacy slate is recorded with the
///   run either way.
pub async fn point_slates(
    sink: &dyn SlateSink,
    proposer: &dyn SlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    subject: &EntityUri,
    shadow: Option<&ShadowStep>,
) -> Option<tokio::task::JoinHandle<Option<ShadowRecord>>> {
    let Some(shadow) = shadow else {
        let _ = point_slate(sink, proposer, policy, world, subject).await;
        return None;
    };
    let started = tokio::time::Instant::now();
    // An unreadable rollout holds nothing: the point gets its legacy slate as always.
    let holding = shadow
        .rollout
        .state()
        .await
        .is_some_and(|state| matches!(state.mode, Mode::Agent { .. }));
    let reserve = hold_reserve(shadow.guard);
    let (handover, paired) = tokio::sync::oneshot::channel();
    let (decided, decision) = tokio::sync::oneshot::channel();
    let hold = holding.then(|| Hold {
        decided,
        deadline: started + shadow.guard.saturating_sub(reserve * 2),
    });
    let handle = {
        let (shadow, target) = (shadow.clone(), ShadowTarget::Point(subject.clone()));
        tokio::spawn(async move { shadow_slate(&shadow, &target, paired, hold).await })
    };
    if !holding {
        let attached = point_slate(sink, proposer, policy, world, subject).await;
        // An agent run that ended early has dropped its end; nothing waits on this.
        let _ = handover.send(Attached {
            paired: paired_slate(sink, subject, attached).await,
            shown: None,
        });
        return Some(handle);
    }

    let prompt = prompt_slate(proposer, policy, world, subject).await;
    let wait = shadow.guard.saturating_sub(reserve);
    let fallback = |reason: &str| {
        Some(Shown::Legacy {
            reason: reason.to_owned(),
        })
    };
    let (agent, mut shown) = match tokio::time::timeout_at(started + wait, decision).await {
        Ok(Ok(AgentDecision::Show { slate, version })) => (Some((slate, version)), None),
        Ok(Ok(AgentDecision::Fallback { reason })) => (None, fallback(&reason)),
        Ok(Ok(AgentDecision::Shadow)) => (None, None),
        Ok(Err(_)) => (None, fallback("no agent run started")),
        Err(_) => (
            None,
            fallback(&format!("the agent's slate was not ready within {wait:?}")),
        ),
    };
    let mut agent_attached = false;
    if let Some((mut slate, version)) = agent {
        fill_needs(&mut slate, policy, world).await;
        let mut steps = std::mem::take(&mut slate.trace.steps);
        steps.push(agent_trace_step(&version));
        slate.trace = Trace::from_steps(steps);
        match attach_slate(sink, subject, &slate).await {
            Ok(()) => {
                agent_attached = true;
                shown = Some(Shown::Agent);
            }
            Err(why) => shown = fallback(&format!("the agent's slate was not attached: {why}")),
        }
    }
    // With the agent's slate on the point, the legacy slate is only recorded.
    let recorded = match prompt {
        Ok(slate) if !agent_attached => attach_slate(sink, subject, &slate).await.map(|()| slate),
        prompt => prompt,
    };
    let _ = handover.send(Attached {
        paired: paired_slate(sink, subject, recorded).await,
        shown,
    });
    Some(handle)
}

/// A formed portfolio's joint slates: the existing path's and, when shadow is wired, the
/// agent's joint slate in its own spawned task, which is handed no sink. Returns what the step
/// did for the portfolio, and that task, if one was spawned.
///
/// - In `legacy` and `shadow`, and in `agent` entered under criteria that state no joint
///   bounds, the existing path's joint slate is recorded with the portfolio as soon as it is
///   ready, with its fallback, and the agent's is only recorded in the shadow store.
/// - In `agent` entered under criteria that cover joint slates, the existing path's joint slate
///   is held until the agent's run answers or the wait runs out, and then one of the two is
///   recorded with the portfolio: the agent's when its run completed and the rollout let it
///   through, the existing one otherwise. When neither can be, the members fall back.
pub async fn portfolio_slates(
    sink: &dyn SlateSink,
    joint: &dyn GroupSlateProposer,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    formed: &FormedPortfolio,
    shadow: Option<&ShadowStep>,
) -> (
    PortfolioSlateOutcome,
    Option<tokio::task::JoinHandle<Option<ShadowRecord>>>,
) {
    if let Err(outcome) = joint_wanted(sink, formed).await {
        return (outcome, None);
    }
    let Some(shadow) = shadow else {
        let (outcome, _) = recorded_joint_slate(sink, joint, policy, world, formed).await;
        return (outcome, None);
    };
    let started = tokio::time::Instant::now();
    // An unreadable rollout holds nothing: the portfolio gets the existing joint slate as always.
    let (agent_mode, covered) = shadow
        .rollout
        .state()
        .await
        .map_or((false, false), |state| {
            (
                matches!(state.mode, Mode::Agent { .. }),
                state.covers_joint(),
            )
        });
    let holding = agent_mode && covered;
    let reserve = hold_reserve(shadow.joint_guard);
    let (handover, paired) = tokio::sync::oneshot::channel();
    let (decided, decision) = tokio::sync::oneshot::channel();
    let hold = holding.then(|| Hold {
        decided,
        deadline: started + shadow.joint_guard.saturating_sub(reserve * 2),
    });
    let handle = {
        let shadow = shadow.clone();
        let target = ShadowTarget::Portfolio {
            group: joint_group(formed),
            generation: formed.generation,
        };
        tokio::spawn(async move { shadow_slate(&shadow, &target, paired, hold).await })
    };
    if !holding {
        let (outcome, recorded) = recorded_joint_slate(sink, joint, policy, world, formed).await;
        // In `agent` mode the record says why the agent's joint slate was not the one shown.
        let shown =
            (agent_mode && outcome == PortfolioSlateOutcome::Attached).then(|| Shown::Legacy {
                reason: "the criteria the agent mode was entered under do not cover joint slates"
                    .to_owned(),
            });
        // An agent run that ended early has dropped its end; nothing waits on this.
        let _ = handover.send(Attached {
            paired: paired_joint_slate(sink, formed, recorded).await,
            shown,
        });
        return (outcome, Some(handle));
    }

    let prompt = prompt_joint_slate(joint, policy, world, formed).await;
    let wait = shadow.joint_guard.saturating_sub(reserve);
    let fallback_to = |reason: &str| {
        Some(Shown::Legacy {
            reason: reason.to_owned(),
        })
    };
    let (agent, shown) = match tokio::time::timeout_at(started + wait, decision).await {
        Ok(Ok(AgentDecision::Show { slate, version })) => (Some((slate, version)), None),
        Ok(Ok(AgentDecision::Fallback { reason })) => (None, fallback_to(&reason)),
        Ok(Ok(AgentDecision::Shadow)) => (None, None),
        Ok(Err(_)) => (None, fallback_to("no agent run started")),
        Err(_) => (
            None,
            fallback_to(&format!("the agent's slate was not ready within {wait:?}")),
        ),
    };
    let (outcome, attached) =
        one_joint_slate(sink, policy, world, formed, prompt, agent, shown).await;
    let _ = handover.send(attached);
    (outcome, Some(handle))
}

/// The ending of a held joint slate step: records the agent's joint slate with the portfolio
/// when there is one (`agent`), the existing path's (`prompt`) otherwise, and says what the
/// step did and what the agent's run is told. `shown` is why the agent's is not there.
async fn one_joint_slate(
    sink: &dyn SlateSink,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    formed: &FormedPortfolio,
    prompt: Result<Slate, String>,
    agent: Option<(Slate, VersionTuple)>,
    mut shown: Option<Shown>,
) -> (PortfolioSlateOutcome, Attached) {
    let stale = || {
        Err(format!(
            "the joint slate for generation {} arrived stale and was refused",
            formed.generation
        ))
    };
    let mut outcome = None;
    if let Some((mut slate, version)) = agent {
        fill_needs(&mut slate, policy, world).await;
        // The trace still opens with why the members are coupled; the agent's step follows it.
        let mut steps = std::mem::take(&mut slate.trace.steps);
        steps.insert(steps.len().min(1), agent_step(&version, "portfolio"));
        slate.trace = Trace::from_steps(steps);
        match sink
            .attach_portfolio_slate(&formed.id.0, formed.generation, &slate)
            .await
        {
            PortfolioAttach::Landed => {
                shown = Some(Shown::Agent);
                outcome = Some(PortfolioSlateOutcome::Attached);
            }
            // The portfolio moved on: the existing path's slate would be refused as well.
            PortfolioAttach::Stale => {
                println!(
                    "the agent's joint slate for {} generation {} arrived stale; refused",
                    formed.id.0, formed.generation
                );
                outcome = Some(PortfolioSlateOutcome::Stale);
            }
            PortfolioAttach::Failed => {
                shown = Some(Shown::Legacy {
                    reason: "the agent's joint slate was not attached".to_owned(),
                });
            }
        }
    }
    // With the agent's slate on the portfolio, the existing path's is only recorded with the run.
    let (outcome, recorded) = match (outcome, prompt) {
        (Some(PortfolioSlateOutcome::Stale), _) => (PortfolioSlateOutcome::Stale, stale()),
        (Some(outcome), prompt) => (outcome, prompt),
        (None, Ok(slate)) => {
            let outcome = attach_joint_slate(sink, formed, &slate).await;
            let recorded = match &outcome {
                PortfolioSlateOutcome::Attached => Ok(slate),
                PortfolioSlateOutcome::Stale => stale(),
                PortfolioSlateOutcome::Fallback(_) => {
                    Err("the joint slate could not be recorded with the portfolio".to_owned())
                }
            };
            (outcome, recorded)
        }
        (None, Err(why)) => (fallback(sink, formed).await, Err(why)),
    };
    // Only a slate that reached the portfolio was shown.
    if outcome != PortfolioSlateOutcome::Attached {
        shown = None;
    }
    let attached = Attached {
        paired: paired_joint_slate(sink, formed, recorded).await,
        shown,
    };
    (outcome, attached)
}

/// Runs one slate task and returns the portfolio members that fall back to a slate of their
/// own. A freshly opened point's slate and a formed portfolio's joint slate are offered to the
/// agent, through `agent`. A released point's slate always comes from the existing path, and so
/// does the slate of a member that falls back, whatever the rollout mode.
pub async fn run_task(
    task: &SlateTask,
    sink: &dyn SlateSink,
    proposer: &dyn SlateProposer,
    joint: Option<&dyn GroupSlateProposer>,
    policy: &PolicyEngine,
    world: &dyn WorldModel,
    agent: Option<&ShadowStep>,
) -> Vec<EntityUri> {
    match task {
        SlateTask::Point(subject) => {
            // The agent's task is spawned separately, so the caller's guard never cuts it
            // short.
            let _agent = point_slates(sink, proposer, policy, world, subject, agent).await;
        }
        SlateTask::Released(subject) => {
            released_slate(sink, proposer, policy, world, subject).await;
        }
        SlateTask::Portfolio(formed) => {
            let Some(joint) = joint else {
                return Vec::new();
            };
            // As for a point: the agent's task is spawned separately.
            let (outcome, _agent) =
                portfolio_slates(sink, joint, policy, world, formed, agent).await;
            if let PortfolioSlateOutcome::Fallback(members) = outcome {
                return members;
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    use ekg_domain::fakes::{CannedGroupSlateProposer, CannedSlateProposer, InMemoryWorldModel};
    use ekg_domain::{EngineError, PortfolioId, Result, Score, Trace};

    use super::*;

    fn uri(s: &str) -> EntityUri {
        EntityUri(format!("https://ekg.example/id/{s}"))
    }

    fn formed() -> FormedPortfolio {
        FormedPortfolio {
            id: PortfolioId("portfolio/0123456789ab".to_owned()),
            generation: 3,
            members: vec![uri("mrp.production/1"), uri("sale.order/1")],
            links: vec!["linked: MO/1 fulfils SO/1".to_owned()],
            coupling: Vec::new(),
        }
    }

    /// A recording sink: which points already carry a slate, whether the portfolio is
    /// current, and what the attach answers.
    struct Recording {
        with_slate: BTreeSet<EntityUri>,
        current: Option<bool>,
        attach: PortfolioAttach,
        /// Whether the portfolio cannot be read after an attach.
        unreadable_after: bool,
        points: Mutex<Vec<EntityUri>>,
        portfolios: Mutex<Vec<(String, u32, Slate)>>,
    }

    impl Recording {
        fn new(current: Option<bool>, attach: PortfolioAttach) -> Self {
            Self {
                with_slate: BTreeSet::new(),
                current,
                attach,
                unreadable_after: false,
                points: Mutex::new(Vec::new()),
                portfolios: Mutex::new(Vec::new()),
            }
        }

        fn points(&self) -> Vec<EntityUri> {
            self.points.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl SlateSink for Recording {
        async fn point_has_slate(&self, subject: &EntityUri) -> Option<bool> {
            Some(self.with_slate.contains(subject))
        }
        async fn portfolio_current(&self, _id: &str, _generation: u32) -> Option<bool> {
            self.current
        }
        async fn attach_point_slate(&self, subject: &EntityUri, _slate: &Slate) -> bool {
            self.points.lock().expect("lock").push(subject.clone());
            true
        }
        async fn point_attach(&self, _subject: &EntityUri) -> Option<PointAttach> {
            None
        }
        async fn attach_portfolio_slate(
            &self,
            id: &str,
            generation: u32,
            slate: &Slate,
        ) -> PortfolioAttach {
            self.portfolios
                .lock()
                .expect("lock")
                .push((id.to_owned(), generation, slate.clone()));
            self.attach
        }
        async fn portfolio_attach(&self, _id: &str, _generation: u32) -> Option<String> {
            (self.attach == PortfolioAttach::Landed && !self.unreadable_after)
                .then(|| JOINT_ATTACHED_AT.to_owned())
        }
    }

    /// When the [`Recording`] sink says a portfolio's joint slate was attached.
    const JOINT_ATTACHED_AT: &str = "1700000100";

    /// A group proposer that always fails (the model is unavailable).
    struct Unavailable;
    #[async_trait]
    impl GroupSlateProposer for Unavailable {
        async fn propose_group_slate(&self, _group: &SubjectGroup) -> Result<Slate> {
            Err(EngineError::Unavailable("model down".to_owned()))
        }
    }

    fn policy() -> PolicyEngine {
        PolicyEngine::new("p/v1", BTreeSet::new())
    }

    /// A group proposer that records the group it is asked to plan, then answers canned.
    #[derive(Default)]
    struct Capturing(Mutex<Vec<SubjectGroup>>);
    #[async_trait]
    impl GroupSlateProposer for Capturing {
        async fn propose_group_slate(&self, group: &SubjectGroup) -> Result<Slate> {
            self.0.lock().expect("lock").push(group.clone());
            CannedGroupSlateProposer.propose_group_slate(group).await
        }
    }

    async fn planned_group(formed: &FormedPortfolio) -> SubjectGroup {
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let proposer = Capturing::default();
        portfolio_slate(
            &sink,
            &proposer,
            &policy(),
            &InMemoryWorldModel::default(),
            formed,
        )
        .await;
        let groups = proposer.0.lock().expect("lock").clone();
        groups.into_iter().next().expect("one group planned")
    }

    #[tokio::test]
    async fn the_joint_slate_plans_over_the_worded_coupling() {
        let mut with_facts = formed();
        with_facts.coupling = vec![CouplingLink {
            a: uri("mrp.production/1"),
            b: uri("sale.order/1"),
            reason: "linked: MO/1 fulfils SO/1".to_owned(),
            facts: BTreeMap::from([("kind".to_owned(), "fulfills".to_owned())]),
        }];
        let group = planned_group(&with_facts).await;
        assert!(group.worded);
        assert_eq!(
            group.links,
            [format!(
                "{} builds {}.",
                uri("mrp.production/1").0,
                uri("sale.order/1").0
            )]
        );
        // A portfolio formed before facts were recorded keeps its recorded reasons.
        let group = planned_group(&formed()).await;
        assert!(!group.worded);
        assert_eq!(group.links, formed().links);
    }

    fn outcome() -> NotifyOutcome {
        NotifyOutcome {
            opened: vec![uri("mrp.production/1"), uri("sale.order/2")],
            formed: vec![formed()],
            released: vec![uri("sale.order/3")],
        }
    }

    #[test]
    fn an_opened_member_of_a_formed_portfolio_gets_no_point_slate() {
        let tasks = slate_tasks(&outcome(), true);
        assert_eq!(
            tasks,
            vec![
                SlateTask::Portfolio(formed()),
                SlateTask::Point(uri("sale.order/2")),
                SlateTask::Released(uri("sale.order/3")),
            ]
        );
        // Every opened subject, member or not, still gets its proposal: the proposal path
        // runs over `opened`, which the slate routing does not touch.
        assert_eq!(outcome().opened.len(), 2);
        // Without joint slates wired, every opened point gets its own.
        let tasks = slate_tasks(&outcome(), false);
        assert!(tasks.contains(&SlateTask::Point(uri("mrp.production/1"))));
        assert!(!tasks.iter().any(|t| matches!(t, SlateTask::Portfolio(_))));
    }

    #[tokio::test]
    async fn a_formed_portfolio_gets_one_joint_slate_attached_with_its_generation() {
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let world = InMemoryWorldModel::default();
        let outcome = portfolio_slate(
            &sink,
            &CannedGroupSlateProposer,
            &policy(),
            &world,
            &formed(),
        )
        .await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        let attached = sink.portfolios.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].0, "portfolio/0123456789ab");
        assert_eq!(attached[0].1, 3, "attached for the formation's generation");
        assert!(attached[0].2.trace.steps[0].text.starts_with("Linked: "));
        // The policy's needs were filled (nothing is allowlisted here).
        assert!(attached[0].2.options.iter().all(|o| !o.needs.is_empty()));
        assert!(sink.points().is_empty(), "no member gets its own slate");
    }

    #[tokio::test]
    async fn a_stale_attach_ends_without_a_fallback() {
        let sink = Recording::new(Some(true), PortfolioAttach::Stale);
        let world = InMemoryWorldModel::default();
        let outcome = portfolio_slate(
            &sink,
            &CannedGroupSlateProposer,
            &policy(),
            &world,
            &formed(),
        )
        .await;
        assert_eq!(outcome, PortfolioSlateOutcome::Stale);
        // Nor is a portfolio already superseded planned at all.
        let superseded = Recording::new(Some(false), PortfolioAttach::Landed);
        let outcome = portfolio_slate(
            &superseded,
            &CannedGroupSlateProposer,
            &policy(),
            &world,
            &formed(),
        )
        .await;
        assert_eq!(outcome, PortfolioSlateOutcome::Stale);
        assert!(superseded.portfolios.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn a_generation_failure_falls_back_to_member_point_slates() {
        let mut sink = Recording::new(Some(true), PortfolioAttach::Landed);
        sink.with_slate.insert(uri("sale.order/1"));
        let world = InMemoryWorldModel::default();
        let outcome = portfolio_slate(&sink, &Unavailable, &policy(), &world, &formed()).await;
        let PortfolioSlateOutcome::Fallback(members) = outcome else {
            panic!("expected a fallback, got {outcome:?}");
        };
        assert_eq!(
            members,
            [uri("mrp.production/1")],
            "only the member without a slate"
        );
        for member in &members {
            let _ = point_slate(&sink, &CannedSlateProposer, &policy(), &world, member).await;
        }
        assert_eq!(sink.points(), [uri("mrp.production/1")]);
        // A failed attach falls back too.
        let failed = Recording::new(Some(true), PortfolioAttach::Failed);
        let outcome = portfolio_slate(
            &failed,
            &CannedGroupSlateProposer,
            &policy(),
            &world,
            &formed(),
        )
        .await;
        assert!(matches!(outcome, PortfolioSlateOutcome::Fallback(m) if m.len() == 2));
    }

    #[tokio::test]
    async fn a_released_point_without_a_slate_gets_one() {
        let mut sink = Recording::new(Some(true), PortfolioAttach::Landed);
        sink.with_slate.insert(uri("sale.order/4"));
        let world = InMemoryWorldModel::default();
        for subject in [uri("sale.order/3"), uri("sale.order/4")] {
            released_slate(&sink, &CannedSlateProposer, &policy(), &world, &subject).await;
        }
        assert_eq!(
            sink.points(),
            [uri("sale.order/3")],
            "the point that already has a slate keeps it"
        );
    }

    #[test]
    fn a_joint_slate_body_carries_the_generation() {
        let slate = Slate {
            baseline: Score {
                metrics: BTreeMap::new(),
                rank_key: "9".to_owned(),
            },
            options: Vec::new(),
            trace: Trace::default(),
        };
        let body = json!({ "generation": 3, "slate": slate });
        let decoded: ekg_dynamic::AttachPortfolioSlate =
            serde_json::from_value(body).expect("the handler's input");
        assert_eq!(decoded.generation, 3);
    }

    // --- shadow slates ---

    /// What the [`Attached`] sink answers.
    #[derive(Default, Clone, Copy, PartialEq, Eq)]
    enum Answers {
        /// The attach lands and the point reads.
        #[default]
        Lands,
        /// The attach is refused.
        Refused,
        /// The attach lands, the point cannot be read afterwards.
        Unreadable,
    }

    /// A sink that keeps every point slate it was handed.
    #[derive(Default)]
    struct Attached(Mutex<Vec<(EntityUri, Slate)>>, Answers);

    /// The point the [`Attached`] sink reads.
    fn attach() -> PointAttach {
        PointAttach {
            action_point: ActionPointId("action-point/sale.order/1".to_owned()),
            point_seq: 3,
            attached_at: "1700000000".to_owned(),
        }
    }

    #[async_trait]
    impl SlateSink for Attached {
        async fn point_has_slate(&self, _subject: &EntityUri) -> Option<bool> {
            Some(false)
        }
        async fn portfolio_current(&self, _id: &str, _generation: u32) -> Option<bool> {
            Some(true)
        }
        async fn attach_point_slate(&self, subject: &EntityUri, slate: &Slate) -> bool {
            if self.1 == Answers::Refused {
                return false;
            }
            self.0
                .lock()
                .expect("lock")
                .push((subject.clone(), slate.clone()));
            true
        }
        async fn point_attach(&self, _subject: &EntityUri) -> Option<PointAttach> {
            (self.1 == Answers::Lands).then(attach)
        }
        async fn attach_portfolio_slate(
            &self,
            _id: &str,
            _generation: u32,
            _slate: &Slate,
        ) -> PortfolioAttach {
            PortfolioAttach::Landed
        }
        async fn portfolio_attach(&self, _id: &str, _generation: u32) -> Option<String> {
            None
        }
    }

    /// A rollout in one mode that grants (or refuses) the claim. Before the claim its state is
    /// the mode's (the slate step's read); after it, each state read answers `allows` from a
    /// script (allowing once it runs out). It sums the tokens reported, keeps every run report
    /// and answers it with `verdict`.
    struct FakeRollout {
        mode: crate::rollout::Mode,
        claim: Claim,
        claimed: AtomicBool,
        allows: Mutex<std::collections::VecDeque<bool>>,
        tokens: std::sync::atomic::AtomicU64,
        verdict: Option<RunVerdict>,
        reports: Mutex<Vec<RunReport>>,
        /// What each claim was for.
        kinds: Mutex<Vec<SlateKind>>,
        /// Whether the `agent` mode was entered under criteria that cover joint slates.
        covers_joint: bool,
    }

    const BOUNDS: (u64, u32, u64) = (u64::MAX, 9, 9);

    impl FakeRollout {
        fn new(mode: crate::rollout::Mode, claim: Claim, allows: &[bool]) -> Self {
            Self {
                mode,
                claim,
                claimed: AtomicBool::new(false),
                allows: Mutex::new(allows.iter().copied().collect()),
                tokens: std::sync::atomic::AtomicU64::new(0),
                verdict: None,
                reports: Mutex::new(Vec::new()),
                kinds: Mutex::new(Vec::new()),
                covers_joint: false,
            }
        }
        fn granted(authoritative: bool) -> Claim {
            Claim::Granted {
                epoch: 1,
                slate: 1,
                authoritative,
            }
        }
        fn granting(allows: &[bool]) -> Arc<Self> {
            let (until, max_slates, max_tokens) = BOUNDS;
            let mode = crate::rollout::Mode::Shadow {
                until,
                max_slates,
                max_tokens,
            };
            Arc::new(Self::new(mode, Self::granted(false), allows))
        }
        fn refusing() -> Arc<Self> {
            let claim = Claim::Refused {
                reason: "legacy".to_owned(),
            };
            Arc::new(Self::new(crate::rollout::Mode::Legacy, claim, &[]))
        }
        /// `agent` mode: the claim is `claim`, and a run report is answered with `verdict`.
        fn agent(claim: Claim, verdict: Option<RunVerdict>, allows: &[bool]) -> Arc<Self> {
            let (until, max_slates, max_tokens) = BOUNDS;
            let mode = crate::rollout::Mode::Agent {
                until,
                max_slates,
                max_tokens,
            };
            let mut rollout = Self::new(mode, claim, allows);
            rollout.verdict = verdict;
            Arc::new(rollout)
        }
        fn reports(&self) -> Vec<RunReport> {
            self.reports.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl Rollout for FakeRollout {
        async fn claim_slate(&self, kind: SlateKind) -> Option<Claim> {
            self.claimed.store(true, Ordering::SeqCst);
            self.kinds.lock().expect("lock").push(kind);
            Some(self.claim.clone())
        }
        async fn state(&self) -> Option<RolloutState> {
            let allow = !self.claimed.load(Ordering::SeqCst)
                || self
                    .allows
                    .lock()
                    .expect("lock")
                    .pop_front()
                    .unwrap_or(true);
            let mut state = RolloutState::default();
            if allow && self.mode != crate::rollout::Mode::Legacy {
                // One switch, so the state's epoch is the claim's.
                state.mode = self.mode.clone();
                state.history.push(crate::rollout::ModeChange {
                    mode: self.mode.clone(),
                    by: "t".to_owned(),
                    reason: "t".to_owned(),
                    at: 0,
                    criteria: None,
                    signoff: None,
                });
                if self.covers_joint {
                    cover_joint(&mut state);
                }
            }
            Some(state)
        }
        async fn report_tokens(&self, tokens: u64) {
            self.tokens.fetch_add(tokens, Ordering::SeqCst);
        }
        async fn report_run(&self, report: &RunReport) -> Option<RunVerdict> {
            self.reports.lock().expect("lock").push(report.clone());
            self.verdict.clone()
        }
        fn key(&self) -> String {
            AGENT_ROLLOUT_KEY.to_owned()
        }
    }

    /// Makes `state`'s last switch one entered under a sign-off for criteria with joint bounds.
    fn cover_joint(state: &mut RolloutState) {
        let document = "version: 2\n\
                        eval: { set_version: 3, min_trials: 3, min_passed: 1 }\n\
                        shadow: { min_compared: 1 }\n\
                        joint: { eval: { min_passed: 1 }, shadow: { min_compared: 1 } }\n";
        let criteria = crate::promotion::Criteria::parse(document).expect("criteria");
        let version = Version {
            engine: env!("CARGO_PKG_VERSION").to_owned(),
            model: "agent-model".to_owned(),
            skills: BTreeMap::new(),
        };
        state.registrations.push(crate::rollout::Registration {
            criteria,
            digest: "joint-criteria".to_owned(),
            by: "a".to_owned(),
            reason: "r".to_owned(),
            at: 0,
        });
        state.signoffs.push(crate::rollout::Signoff {
            criteria: "joint-criteria".to_owned(),
            report: "report".to_owned(),
            verdict: crate::promotion::Verdict {
                criteria: Some("joint-criteria".to_owned()),
                met: true,
                checks: Vec::new(),
                eval_missing: None,
                shadow: crate::promotion::ShadowEvidence::default(),
                eval_version: Some(version.clone()),
                shadow_versions: Vec::new(),
            },
            version,
            by: "b".to_owned(),
            reason: "r".to_owned(),
            at: 7,
        });
        if let Some(entry) = state.history.last_mut() {
            entry.signoff = Some(7);
        }
    }

    #[derive(Default)]
    struct Store(Mutex<Vec<ShadowRecord>>);

    #[async_trait]
    impl ShadowStore for Store {
        async fn record(&self, record: &ShadowRecord) -> bool {
            self.0.lock().expect("lock").push(record.clone());
            true
        }
    }

    /// The agent's shadow slate, faked: per candidate it asks the gate, then records a run; it
    /// fails, hangs, or answers a slate marked `agent`.
    struct FakeAgentSlate {
        run: ShadowRun,
        candidates: usize,
        fail: bool,
        hang: bool,
        /// How long the slate takes to come back.
        delay: std::time::Duration,
        /// Every candidate run ends in an error, and the slate loop still returns a slate.
        errored: bool,
    }

    #[test]
    fn a_shadow_slate_keeps_a_run_without_what_the_agent_said_last() {
        let runs = CollectedRuns::default();
        runs.record(RunRecord {
            termination: ekg_cognition::Termination::NoPlan,
            closing_answer: Some("Nothing on order helps.".to_owned()),
            ..agent_run()
        });
        let kept = runs.0.lock().expect("runs");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].termination, ekg_cognition::Termination::NoPlan);
        assert_eq!(kept[0].closing_answer, None);
    }

    #[test]
    fn an_analysts_run_does_not_show_that_the_agent_ran() {
        let analyst = RunRecord {
            role: Some(RunRole::Analyst),
            tools: vec!["submit_finding".to_owned()],
            skills: BTreeMap::from([("analyse-member".to_owned(), "a1".to_owned())]),
            ..agent_run()
        };
        let planner = RunRecord {
            role: Some(RunRole::Planner),
            tools: vec!["submit_plan".to_owned()],
            termination: Termination::Error,
            skills: BTreeMap::from([("plan-portfolio".to_owned(), "p1".to_owned())]),
            ..agent_run()
        };
        let version = version_of(&[analyst.clone(), planner.clone()]);
        assert_eq!(version.tools, ["submit_plan"], "the planner's");
        assert_eq!(version.skills.len(), 2, "every run's pins");
        assert!(!agent_ran(std::slice::from_ref(&analyst)), "no run planned");
        assert!(
            !agent_ran(&[analyst.clone(), planner.clone()]),
            "the planner failed"
        );
        let handed_in = RunRecord {
            termination: Termination::Submitted,
            ..planner
        };
        assert!(agent_ran(&[analyst, handed_in]));
    }

    fn agent_run() -> RunRecord {
        RunRecord {
            termination: ekg_cognition::Termination::Submitted,
            steps: 3,
            input_tokens: 100,
            output_tokens: 20,
            model: "agent-model".to_owned(),
            elapsed_ms: 1_000,
            tool_calls: BTreeMap::new(),
            rejections: Vec::new(),
            skills: BTreeMap::from([("propose-plan".to_owned(), "v1".to_owned())]),
            tools: vec!["ground_entity".to_owned(), "submit_plan".to_owned()],
            trace_id: "t".to_owned(),
            closing_answer: None,
            role: None,
            finding: None,
        }
    }

    #[async_trait]
    impl SlateProposer for FakeAgentSlate {
        async fn propose_slate(&self, _subject: &EntityUri) -> Result<Slate> {
            for _ in 0..self.candidates {
                if !self.run.gate.proceed().await {
                    continue;
                }
                let mut run = agent_run();
                if self.errored {
                    run.termination = Termination::Error;
                    run.model = String::new();
                }
                self.run.recorder.record(run);
            }
            if self.fail {
                return Err(EngineError::Unavailable("model down".to_owned()));
            }
            if self.hang {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(self.delay).await;
            Ok(agent_slate())
        }
    }

    #[async_trait]
    impl GroupSlateProposer for FakeAgentSlate {
        async fn propose_group_slate(&self, group: &SubjectGroup) -> Result<Slate> {
            let mut slate = self.propose_slate(&uri("sale.order/1")).await?;
            // As the group slate loop does: the trace opens with why the members are coupled.
            let mut steps = vec![TraceStep {
                tag: TraceTag::Graph,
                text: format!("Linked: {}", group.links.join("; ")),
                rejected: false,
            }];
            steps.extend(std::mem::take(&mut slate.trace.steps));
            slate.trace = Trace::from_steps(steps);
            Ok(slate)
        }
    }

    /// The slate the faked agent answers: one option without needs, marked `agent`.
    fn agent_slate() -> Slate {
        let plan = ekg_domain::Plan {
            actions: vec![ekg_domain::ProposedAction {
                action_type: ekg_domain::ActionType("agent_verb".to_owned()),
                target: uri("sale.order/1"),
                params: BTreeMap::new(),
            }],
        };
        Slate {
            baseline: Score {
                metrics: BTreeMap::new(),
                rank_key: "agent".to_owned(),
            },
            options: vec![ekg_domain::RankedPlan {
                plan,
                score: Score {
                    metrics: BTreeMap::new(),
                    rank_key: "agent-1".to_owned(),
                },
                recommended: true,
                needs: Vec::new(),
                omissions: Vec::new(),
                trace: Trace::from_steps(Vec::new()),
            }],
            trace: Trace::from_steps(vec![TraceStep {
                tag: TraceTag::Simulated,
                text: "[agent] agent_verb scored".to_owned(),
                rejected: false,
            }]),
        }
    }

    /// A shadow step over `rollout` and `store` whose agent slate runs `candidates` candidates
    /// (or fails), counting how often it was built.
    fn shadow_step(
        rollout: Arc<FakeRollout>,
        store: Arc<Store>,
        candidates: usize,
        fail: bool,
        built: Arc<std::sync::atomic::AtomicUsize>,
    ) -> ShadowStep {
        let joint_built = Arc::clone(&built);
        ShadowStep {
            rollout,
            store,
            slates: Arc::new(move |run| {
                built.fetch_add(1, Ordering::SeqCst);
                Arc::new(FakeAgentSlate {
                    run,
                    candidates,
                    fail,
                    hang: false,
                    delay: std::time::Duration::ZERO,
                    errored: false,
                }) as Arc<dyn SlateProposer>
            }),
            joint: Arc::new(move |run| {
                joint_built.fetch_add(1, Ordering::SeqCst);
                Arc::new(FakeAgentSlate {
                    run,
                    candidates,
                    fail,
                    hang: false,
                    delay: std::time::Duration::ZERO,
                    errored: false,
                }) as Arc<dyn GroupSlateProposer>
            }),
            permits: Arc::new(Semaphore::new(1)),
            guard: std::time::Duration::from_secs(5),
            joint_guard: std::time::Duration::from_secs(5),
        }
    }

    /// The slate the point gets without shadow: the legacy slate with its needs filled.
    async fn legacy_slate(subject: &EntityUri) -> Slate {
        let without = Attached::default();
        run_point(&without, subject, None).await;
        let attached = without.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        attached[0].1.clone()
    }

    /// Runs a point's slates with `shadow`, waiting for the shadow task.
    async fn run_point(
        sink: &Attached,
        subject: &EntityUri,
        shadow: Option<&ShadowStep>,
    ) -> Option<ShadowRecord> {
        run_point_with(sink, &CannedSlateProposer, subject, shadow).await
    }

    /// [`run_point`] with the existing path's slate from `proposer`.
    async fn run_point_with(
        sink: &Attached,
        proposer: &dyn SlateProposer,
        subject: &EntityUri,
        shadow: Option<&ShadowStep>,
    ) -> Option<ShadowRecord> {
        let world = InMemoryWorldModel::default();
        let handle = point_slates(sink, proposer, &policy(), &world, subject, shadow).await;
        match handle {
            Some(handle) => handle.await.expect("shadow task"),
            None => None,
        }
    }

    /// An existing path whose slate cannot be produced.
    struct Failing;

    #[async_trait]
    impl SlateProposer for Failing {
        async fn propose_slate(&self, _subject: &EntityUri) -> Result<Slate> {
            Err(EngineError::Unavailable("model down".to_owned()))
        }
    }

    /// A shadow step that completes, and its store.
    fn completing_step() -> (ShadowStep, Arc<Store>) {
        let store = Arc::new(Store::default());
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(
            FakeRollout::granting(&[]),
            Arc::clone(&store),
            1,
            false,
            built,
        );
        (step, store)
    }

    #[tokio::test]
    async fn the_point_slate_step_returns_the_slate_as_attached_or_why_there_is_none() {
        let (subject, world) = (uri("sale.order/1"), InMemoryWorldModel::default());
        let sink = Attached::default();
        let slate = point_slate(&sink, &CannedSlateProposer, &policy(), &world, &subject)
            .await
            .expect("a slate");
        assert_eq!(sink.0.lock().expect("lock")[0].1, slate);

        let why = point_slate(&sink, &Failing, &policy(), &world, &subject)
            .await
            .expect_err("no slate");
        assert!(why.starts_with("the slate could not be produced"), "{why}");

        let refusing = Attached(Mutex::default(), Answers::Refused);
        let why = point_slate(&refusing, &CannedSlateProposer, &policy(), &world, &subject)
            .await
            .expect_err("no slate");
        assert_eq!(why, "the slate attach did not land");
    }

    #[tokio::test]
    async fn the_shadow_record_carries_the_slate_the_point_got() {
        let subject = uri("sale.order/1");
        let (step, store) = completing_step();
        let sink = Attached::default();
        let record = run_point(&sink, &subject, Some(&step))
            .await
            .expect("a shadow record");

        let paired = record.paired.clone().expect("a paired slate");
        let paired = paired.as_point().expect("a point's").clone();
        let point = attach();
        assert_eq!(paired.slate, sink.0.lock().expect("lock")[0].1);
        assert_eq!(paired.action_point, point.action_point);
        assert_eq!(paired.point_seq, point.point_seq);
        assert_eq!(paired.attached_at, point.attached_at);
        assert_eq!(record.paired_missing, None);
        assert_eq!(record.decision, None);
        // Apart from the agent's own slate.
        assert_ne!(record.slate.as_ref(), Some(&paired.slate));
        assert_eq!(store.0.lock().expect("lock").clone(), vec![record]);
    }

    #[tokio::test]
    async fn a_failed_existing_slate_is_named_and_the_agents_slate_is_still_recorded() {
        let subject = uri("sale.order/1");
        let (step, _store) = completing_step();
        let record = run_point_with(&Attached::default(), &Failing, &subject, Some(&step))
            .await
            .expect("a shadow record");
        assert_eq!(record.paired, None);
        let why = record.paired_missing.expect("a reason");
        assert!(why.starts_with("the slate could not be produced"), "{why}");
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert!(record.slate.is_some());
    }

    #[tokio::test]
    async fn a_refused_attach_and_an_unreadable_point_are_named() {
        let subject = uri("sale.order/1");
        for (answers, reason) in [
            (Answers::Refused, "the slate attach did not land"),
            (
                Answers::Unreadable,
                "the point could not be read after the attach",
            ),
        ] {
            let (step, _store) = completing_step();
            let sink = Attached(Mutex::default(), answers);
            let record = run_point(&sink, &subject, Some(&step))
                .await
                .expect("a shadow record");
            assert_eq!(record.paired, None);
            assert_eq!(record.paired_missing.as_deref(), Some(reason));
        }
    }

    #[tokio::test]
    async fn an_abandoned_existing_slate_is_named() {
        let (step, _store) = completing_step();
        let (handover, paired) = tokio::sync::oneshot::channel();
        drop(handover);
        let target = ShadowTarget::Point(uri("sale.order/1"));
        let record = shadow_slate(&step, &target, paired, None)
            .await
            .expect("a shadow record");
        assert_eq!(record.paired, None);
        assert_eq!(
            record.paired_missing.as_deref(),
            Some("the existing path's slate was abandoned")
        );
    }

    #[tokio::test]
    async fn in_shadow_mode_the_point_slate_attaches_unchanged_and_a_shadow_record_is_written() {
        let subject = uri("sale.order/1");
        let (rollout, store) = (FakeRollout::granting(&[]), Arc::new(Store::default()));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(Arc::clone(&rollout), Arc::clone(&store), 2, false, built);
        let sink = Attached::default();
        let record = run_point(&sink, &subject, Some(&step))
            .await
            .expect("a shadow record");

        // The point carries the same legacy slate as without shadow.
        let attached = sink.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1, legacy_slate(&subject).await);

        // The agent's slate is in the shadow store only.
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.subject, subject);
        assert_eq!(
            record.slate.as_ref().map(|s| s.baseline.rank_key.as_str()),
            Some("agent")
        );
        assert_eq!(record.runs.len(), 2);
        assert_eq!(record.version.model, "agent-model");
        assert_eq!(record.version.skills["propose-plan"], "v1");
        assert_eq!(record.version.tools, vec!["ground_entity", "submit_plan"]);
        assert_eq!(record.version.engine, env!("CARGO_PKG_VERSION"));
        assert_eq!(store.0.lock().expect("lock").clone(), vec![record]);
        assert_eq!(rollout.tokens.load(Ordering::SeqCst), 240, "2 runs x 120");
    }

    #[tokio::test]
    async fn a_failing_agent_leaves_the_attached_slate_unchanged() {
        let subject = uri("mrp.production/1");
        let store = Arc::new(Store::default());
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(
            FakeRollout::granting(&[]),
            Arc::clone(&store),
            1,
            true,
            built,
        );
        let sink = Attached::default();
        let record = run_point(&sink, &subject, Some(&step))
            .await
            .expect("a record");
        let attached = sink.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1, legacy_slate(&subject).await);
        assert_eq!(record.outcome, ShadowOutcome::Failed);
        assert!(record.slate.is_none());
        assert!(
            record
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("model down")),
            "{record:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_claim_runs_no_agent() {
        let subject = uri("sale.order/1");
        let store = Arc::new(Store::default());
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(
            FakeRollout::refusing(),
            Arc::clone(&store),
            1,
            false,
            Arc::clone(&built),
        );
        let sink = Attached::default();
        assert!(run_point(&sink, &subject, Some(&step)).await.is_none());
        assert_eq!(built.load(Ordering::SeqCst), 0, "no agent was built");
        assert!(store.0.lock().expect("lock").is_empty());
        assert_eq!(sink.0.lock().expect("lock").len(), 1, "the legacy slate");
    }

    #[tokio::test]
    async fn a_gate_switched_to_legacy_mid_slate_records_aborted() {
        let subject = uri("sale.order/1");
        let store = Arc::new(Store::default());
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // The first candidate runs; then the mode is switched.
        let step = shadow_step(
            FakeRollout::granting(&[true, false, false]),
            Arc::clone(&store),
            3,
            false,
            built,
        );
        let sink = Attached::default();
        let record = run_point(&sink, &subject, Some(&step))
            .await
            .expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Aborted);
        assert_eq!(record.runs.len(), 1, "no candidate ran after the switch");
        assert_eq!(sink.0.lock().expect("lock").len(), 1, "the legacy slate");
    }

    // --- agent mode: one slate step, two endings ---

    /// A step in `agent` mode over `rollout` whose agent slate runs `candidates` candidates and
    /// then fails, hangs or answers.
    fn agent_step(
        rollout: &Arc<FakeRollout>,
        store: &Arc<Store>,
        candidates: usize,
        mode: &'static str,
    ) -> ShadowStep {
        let (fail, hang, errored) = (mode == "fail", mode == "hang", mode == "errored");
        // "slow" comes back, later than a short guard allows.
        let delay = std::time::Duration::from_millis(if mode == "slow" { 600 } else { 0 });
        let guard = std::time::Duration::from_millis(if hang { 200 } else { 5_000 });
        ShadowStep {
            rollout: Arc::clone(rollout) as Arc<dyn Rollout>,
            store: Arc::clone(store) as Arc<dyn ShadowStore>,
            slates: Arc::new(move |run| {
                Arc::new(FakeAgentSlate {
                    run,
                    candidates,
                    fail,
                    hang,
                    delay,
                    errored,
                }) as Arc<dyn SlateProposer>
            }),
            joint: Arc::new(move |run| {
                Arc::new(FakeAgentSlate {
                    run,
                    candidates,
                    fail,
                    hang,
                    delay,
                    errored,
                }) as Arc<dyn GroupSlateProposer>
            }),
            permits: Arc::new(Semaphore::new(1)),
            guard,
            joint_guard: guard,
        }
    }

    /// What one point got in `agent` mode: the slates attached, the record, the run reports.
    struct Ending {
        attached: Vec<Slate>,
        record: Option<ShadowRecord>,
        reports: Vec<RunReport>,
    }

    impl Ending {
        /// The one slate the point got.
        fn slate(&self) -> &Slate {
            assert_eq!(self.attached.len(), 1, "exactly one slate is attached");
            &self.attached[0]
        }
        fn shown(&self) -> Option<Shown> {
            self.record.as_ref().and_then(|record| record.shown.clone())
        }
        /// Why the existing path's slate was shown.
        fn fallback(&self) -> String {
            match self.shown() {
                Some(Shown::Legacy { reason }) => reason,
                other => panic!("expected a fallback, got {other:?}"),
            }
        }
    }

    async fn agent_point(
        rollout: Arc<FakeRollout>,
        candidates: usize,
        mode: &'static str,
    ) -> Ending {
        let (subject, store) = (uri("sale.order/1"), Arc::new(Store::default()));
        let step = agent_step(&rollout, &store, candidates, mode);
        let sink = Attached::default();
        let record = run_point(&sink, &subject, Some(&step)).await;
        assert_eq!(store.0.lock().expect("lock").first(), record.as_ref());
        let attached = sink.0.lock().expect("lock").clone();
        Ending {
            attached: attached.into_iter().map(|(_, slate)| slate).collect(),
            record,
            reports: rollout.reports(),
        }
    }

    fn authoritative(verdict: RunVerdict, allows: &[bool]) -> Arc<FakeRollout> {
        FakeRollout::agent(FakeRollout::granted(true), Some(verdict), allows)
    }

    #[tokio::test]
    async fn in_agent_mode_a_completed_run_the_rollout_lets_through_is_the_points_slate() {
        let rollout = authoritative(RunVerdict::Proceed, &[]);
        let ending = agent_point(Arc::clone(&rollout), 2, "ok").await;
        let slate = ending.slate();
        assert_eq!(slate.baseline.rank_key, "agent");
        assert_eq!(ending.shown(), Some(Shown::Agent));

        // The rollout was asked, with the run's version and tokens, before the attach.
        assert_eq!(ending.reports.len(), 1);
        let report = &ending.reports[0];
        assert!(report.completed);
        assert_eq!((report.epoch, report.tokens), (1, 240));
        assert_eq!(report.version.model, "agent-model");
        assert_eq!(report.version.skills["propose-plan"], "v1");
        assert_eq!(
            rollout.tokens.load(Ordering::SeqCst),
            0,
            "the tokens go with the run report, not twice"
        );

        // The record holds both slates: the agent's as produced, the existing path's as paired.
        let record = ending.record.clone().expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.slate, Some(agent_slate()));
        assert_eq!(record.epoch, Some(1));
        assert_eq!(record.rollout, None, "the engine's own rollout");
        let paired = record.paired.expect("the existing path's slate");
        assert_eq!(*paired.slate(), legacy_slate(&uri("sale.order/1")).await);
        assert_eq!(paired.attached_at(), attach().attached_at);
    }

    #[tokio::test]
    async fn the_agents_slate_gets_its_policy_needs_and_a_trace_step_naming_its_versions() {
        let ending = agent_point(authoritative(RunVerdict::Proceed, &[]), 1, "ok").await;
        let slate = ending.slate();
        // Nothing is allowlisted here, so the policy leaves a need on the agent's option too.
        assert!(agent_slate().options[0].needs.is_empty());
        assert!(!slate.options[0].needs.is_empty(), "{:?}", slate.options);

        let step = slate.trace.steps.last().expect("the added step");
        assert_eq!(step.tag, TraceTag::Assumption);
        assert!(!step.rejected);
        assert_eq!(
            step.text,
            format!(
                "This slate was produced by the tool-using agent (engine {}, model agent-model, \
                 skills propose-plan@v1). The prompt-based slate for the same point is recorded \
                 beside it.",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(slate.trace.steps.len(), 2, "the agent's own step is kept");
        assert_eq!(slate.trace.summary.open_assumptions, 1);
        assert_eq!(slate.trace.summary.simulated, 1);

        let unknown = agent_trace_step(&VersionTuple::default());
        assert!(
            unknown.text.contains("model none reported, skills none"),
            "{}",
            unknown.text
        );
    }

    #[tokio::test]
    async fn in_agent_mode_a_refused_claim_shows_the_existing_slate_and_runs_no_agent() {
        for reason in ["budget_exhausted", "window_closed"] {
            let claim = Claim::Refused {
                reason: reason.to_owned(),
            };
            let ending = agent_point(FakeRollout::agent(claim, None, &[]), 1, "ok").await;
            assert_eq!(
                ending.slate(),
                &legacy_slate(&uri("sale.order/1")).await,
                "{reason}"
            );
            assert_eq!(
                ending.fallback(),
                format!("no agent run was allowed to start: {reason}")
            );
            let record = ending.record.expect("a record");
            assert_eq!(record.outcome, ShadowOutcome::Aborted);
            assert!(record.runs.is_empty() && record.slate.is_none());
            assert!(record.paired.is_some(), "the slate the point got");
            assert!(ending.reports.is_empty(), "no run, so no run report");
        }
        // A mode switched back since the step read it writes no record.
        let legacy = Claim::Refused {
            reason: "legacy".to_owned(),
        };
        let ending = agent_point(FakeRollout::agent(legacy, None, &[]), 1, "ok").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        assert!(ending.record.is_none());
    }

    #[tokio::test]
    async fn in_agent_mode_a_failed_run_shows_the_existing_slate_and_is_reported() {
        let ending = agent_point(authoritative(RunVerdict::Fallback, &[]), 1, "fail").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        let why = ending.fallback();
        assert!(
            why.starts_with("the agent's run failed:") && why.contains("model down"),
            "{why}"
        );
        assert_eq!(ending.reports.len(), 1);
        assert!(!ending.reports[0].completed);
        let record = ending.record.expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Failed);
        assert!(record.paired.is_some());
    }

    #[tokio::test]
    async fn in_agent_mode_a_slate_whose_runs_all_errored_is_a_failed_run() {
        // The rollout would let a completed run through; it is told this one failed.
        let rollout = authoritative(RunVerdict::Fallback, &[]);
        let ending = agent_point(rollout, 2, "errored").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        assert_eq!(
            ending.fallback(),
            "the agent's run failed: every candidate run ended in an error"
        );
        assert!(!ending.reports[0].completed, "counted as a failed run");
        // Nor is it shown when the rollout answers as for a completed run.
        let ending = agent_point(authoritative(RunVerdict::Proceed, &[]), 2, "errored").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
    }

    #[tokio::test]
    async fn in_agent_mode_a_stopped_run_shows_the_existing_slate() {
        // The first candidate runs; then the mode is switched, and the report is stale.
        let rollout = authoritative(RunVerdict::Stale, &[true, false, false]);
        let ending = agent_point(rollout, 3, "ok").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        let why = ending.fallback();
        assert!(why.starts_with("the agent's run was stopped:"), "{why}");
        assert!(!ending.reports[0].completed);
        let record = ending.record.expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Aborted);
        assert_eq!(record.runs.len(), 1, "no candidate ran after the switch");
    }

    #[tokio::test]
    async fn in_agent_mode_a_run_over_its_guard_shows_the_existing_slate() {
        let ending = agent_point(authoritative(RunVerdict::Fallback, &[]), 1, "hang").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        let why = ending.fallback();
        assert!(why.contains("was not ready within its guard"), "{why}");
        assert!(!ending.reports[0].completed, "counted as a failed run");
        assert_eq!(
            ending.record.expect("a record").outcome,
            ShadowOutcome::Failed
        );
    }

    #[tokio::test]
    async fn in_agent_mode_a_rolled_back_run_shows_the_existing_slate() {
        let rolled_back = RunVerdict::RolledBack {
            reason: "skill propose-plan: v1, v2".to_owned(),
        };
        let ending = agent_point(authoritative(rolled_back, &[]), 1, "ok").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        assert_eq!(
            ending.fallback(),
            "the mode was rolled back: skill propose-plan: v1, v2"
        );
        let record = ending.record.expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.slate, Some(agent_slate()), "still recorded");
    }

    #[tokio::test]
    async fn in_agent_mode_a_stale_or_unanswered_report_shows_the_existing_slate() {
        let stale = authoritative(RunVerdict::Stale, &[]);
        let unanswered = FakeRollout::agent(FakeRollout::granted(true), None, &[]);
        for (rollout, reason) in [
            (
                stale,
                "the rollout mode was switched before the agent's slate was ready",
            ),
            (
                unanswered,
                "the rollout could not be asked whether the agent's slate may be shown",
            ),
        ] {
            let ending = agent_point(rollout, 1, "ok").await;
            assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
            assert_eq!(ending.fallback(), reason);
        }
    }

    #[tokio::test]
    async fn in_agent_mode_a_run_that_never_answers_does_not_hold_the_point() {
        let (subject, store) = (uri("sale.order/1"), Arc::new(Store::default()));
        let rollout = authoritative(RunVerdict::Proceed, &[]);
        let mut step = agent_step(&rollout, &store, 1, "ok");
        // Every permit is taken: the agent's run never starts.
        step.permits = Arc::new(Semaphore::new(0));
        step.guard = std::time::Duration::from_millis(200);
        let (sink, world) = (Attached::default(), InMemoryWorldModel::default());
        let handle = point_slates(
            &sink,
            &CannedSlateProposer,
            &policy(),
            &world,
            &subject,
            Some(&step),
        )
        .await
        .expect("the agent's task");
        let attached = sink.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1, legacy_slate(&subject).await);
        handle.abort();
    }

    #[tokio::test]
    async fn a_claim_granted_in_shadow_while_the_step_held_only_records() {
        // The step read `agent`; by the claim the mode was `shadow`.
        let rollout = FakeRollout::agent(FakeRollout::granted(false), None, &[]);
        let ending = agent_point(Arc::clone(&rollout), 1, "ok").await;
        assert_eq!(ending.slate(), &legacy_slate(&uri("sale.order/1")).await);
        assert_eq!(ending.shown(), None);
        assert!(ending.reports.is_empty());
        assert_eq!(rollout.tokens.load(Ordering::SeqCst), 120);
    }

    #[tokio::test]
    async fn a_failed_existing_slate_still_leaves_the_agents_slate_on_the_point() {
        let (subject, store) = (uri("sale.order/1"), Arc::new(Store::default()));
        let rollout = authoritative(RunVerdict::Proceed, &[]);
        let step = agent_step(&rollout, &store, 1, "ok");
        let sink = Attached::default();
        let record = run_point_with(&sink, &Failing, &subject, Some(&step))
            .await
            .expect("a record");
        let attached = sink.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1.baseline.rank_key, "agent");
        assert_eq!(record.shown, Some(Shown::Agent));
        assert!(record.paired.is_none() && record.paired_missing.is_some());
    }

    // --- shadow is unchanged ---

    /// A sink that tells when the point's slate is attached.
    struct Announcing(Attached, tokio::sync::Notify);

    #[async_trait]
    impl SlateSink for Announcing {
        async fn point_has_slate(&self, subject: &EntityUri) -> Option<bool> {
            self.0.point_has_slate(subject).await
        }
        async fn portfolio_current(&self, id: &str, generation: u32) -> Option<bool> {
            self.0.portfolio_current(id, generation).await
        }
        async fn attach_point_slate(&self, subject: &EntityUri, slate: &Slate) -> bool {
            let landed = self.0.attach_point_slate(subject, slate).await;
            self.1.notify_one();
            landed
        }
        async fn point_attach(&self, subject: &EntityUri) -> Option<PointAttach> {
            self.0.point_attach(subject).await
        }
        async fn attach_portfolio_slate(
            &self,
            id: &str,
            generation: u32,
            slate: &Slate,
        ) -> PortfolioAttach {
            self.0.attach_portfolio_slate(id, generation, slate).await
        }
        async fn portfolio_attach(&self, id: &str, generation: u32) -> Option<String> {
            self.0.portfolio_attach(id, generation).await
        }
    }

    /// An agent slate that ends only once the point's slate is attached.
    struct AfterTheAttach(Arc<Announcing>);

    #[async_trait]
    impl SlateProposer for AfterTheAttach {
        async fn propose_slate(&self, _subject: &EntityUri) -> Result<Slate> {
            self.0.1.notified().await;
            Ok(agent_slate())
        }
    }

    #[tokio::test]
    async fn in_shadow_mode_the_existing_slate_attaches_before_the_agents_run_ends() {
        let subject = uri("sale.order/1");
        let sink = Arc::new(Announcing(Attached::default(), tokio::sync::Notify::new()));
        let store = Arc::new(Store::default());
        let step = ShadowStep {
            rollout: FakeRollout::granting(&[]),
            store: Arc::clone(&store) as Arc<dyn ShadowStore>,
            slates: {
                let sink = Arc::clone(&sink);
                Arc::new(move |_run| {
                    Arc::new(AfterTheAttach(Arc::clone(&sink))) as Arc<dyn SlateProposer>
                })
            },
            joint: Arc::new(|_run| panic!("no joint slate is run here")),
            permits: Arc::new(Semaphore::new(1)),
            guard: std::time::Duration::from_secs(5),
            joint_guard: std::time::Duration::from_secs(5),
        };
        let world = InMemoryWorldModel::default();
        // Were the slate held for the agent's run, neither would ever end.
        let ran = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let handle = point_slates(
                sink.as_ref(),
                &CannedSlateProposer,
                &policy(),
                &world,
                &subject,
                Some(&step),
            )
            .await;
            handle.expect("the shadow task").await.expect("joined")
        })
        .await;
        let record = ran.expect("the attach did not wait").expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.shown, None);
        let attached = sink.0.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1, legacy_slate(&subject).await);
    }

    // --- only a point's own slate is offered to the agent ---

    /// A rollout that must never be asked.
    struct Untouched;

    #[async_trait]
    impl Rollout for Untouched {
        async fn claim_slate(&self, _kind: SlateKind) -> Option<Claim> {
            panic!("no agent slate may be claimed here")
        }
        async fn state(&self) -> Option<RolloutState> {
            panic!("the rollout may not be read here")
        }
        async fn report_tokens(&self, _tokens: u64) {
            panic!("no agent run may report here")
        }
        async fn report_run(&self, _report: &RunReport) -> Option<RunVerdict> {
            panic!("no agent run may report here")
        }
        fn key(&self) -> String {
            AGENT_ROLLOUT_KEY.to_owned()
        }
    }

    /// An agent step whose rollout and agent proposer panic when used.
    fn forbidden_agent() -> ShadowStep {
        ShadowStep {
            rollout: Arc::new(Untouched),
            store: Arc::new(Store::default()),
            slates: Arc::new(|_run| panic!("no agent slate may be built here")),
            joint: Arc::new(|_run| panic!("no agent joint slate may be built here")),
            permits: Arc::new(Semaphore::new(1)),
            guard: std::time::Duration::from_secs(5),
            joint_guard: std::time::Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn a_released_point_and_a_fallback_member_start_no_agent_run() {
        let world = InMemoryWorldModel::default();
        let agent = forbidden_agent();
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let fallback = run_task(
            &SlateTask::Released(uri("sale.order/3")),
            &sink,
            &CannedSlateProposer,
            Some(&CannedGroupSlateProposer),
            &policy(),
            &world,
            Some(&agent),
        )
        .await;
        assert!(fallback.is_empty());
        assert_eq!(sink.points(), [uri("sale.order/3")], "its own legacy slate");

        // A joint slate that fails names its members. Only the joint slate was offered to the
        // agent: the members' own slates are run by the caller without it.
        let (rollout, store) = (FakeRollout::granting(&[]), Arc::new(Store::default()));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(Arc::clone(&rollout), store, 1, false, Arc::clone(&built));
        let fallback = run_task(
            &SlateTask::Portfolio(formed()),
            &sink,
            &CannedSlateProposer,
            Some(&Unavailable),
            &policy(),
            &world,
            Some(&step),
        )
        .await;
        assert_eq!(fallback, formed().members);
        assert_eq!(sink.points(), [uri("sale.order/3")], "no member slate here");
        // Each member's own slate, as the caller runs it: with no agent step at all.
        for member in &fallback {
            run_task(
                &SlateTask::Point(member.clone()),
                &sink,
                &CannedSlateProposer,
                None,
                &policy(),
                &world,
                None,
            )
            .await;
        }
        assert_eq!(sink.points().len(), 3);
        // The agent's own task was spawned apart; give it its turn to claim.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(*rollout.kinds.lock().expect("lock"), [SlateKind::Joint]);
    }

    // --- a portfolio's joint slate goes through the same step ---

    /// A portfolio's joint slates with `shadow`, waiting for the agent's task.
    async fn run_portfolio(
        sink: &Recording,
        joint: &dyn GroupSlateProposer,
        shadow: Option<&ShadowStep>,
    ) -> (PortfolioSlateOutcome, Option<ShadowRecord>) {
        let world = InMemoryWorldModel::default();
        let (outcome, handle) =
            portfolio_slates(sink, joint, &policy(), &world, &formed(), shadow).await;
        let record = match handle {
            Some(handle) => handle.await.expect("the agent's task"),
            None => None,
        };
        (outcome, record)
    }

    /// The joint slate the portfolio gets without the agent, with its needs filled.
    async fn legacy_joint_slate() -> Slate {
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, _) = run_portfolio(&sink, &CannedGroupSlateProposer, None).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        let attached = sink.portfolios.lock().expect("lock");
        attached[0].2.clone()
    }

    fn joint_slates(sink: &Recording) -> Vec<Slate> {
        let attached = sink.portfolios.lock().expect("lock");
        attached.iter().map(|(_, _, slate)| slate.clone()).collect()
    }

    #[tokio::test]
    async fn in_legacy_mode_a_portfolio_gets_its_joint_slate_and_no_agent_run_starts() {
        let store = Arc::new(Store::default());
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(
            FakeRollout::refusing(),
            Arc::clone(&store),
            1,
            false,
            Arc::clone(&built),
        );
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink), [legacy_joint_slate().await]);
        assert_eq!(record, None);
        assert_eq!(built.load(Ordering::SeqCst), 0, "no agent slate was built");
        assert!(store.0.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn in_shadow_mode_a_portfolio_keeps_its_joint_slate_and_the_agents_is_only_recorded() {
        let (rollout, store) = (FakeRollout::granting(&[]), Arc::new(Store::default()));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(Arc::clone(&rollout), Arc::clone(&store), 2, false, built);
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        let legacy = legacy_joint_slate().await;
        assert_eq!(
            joint_slates(&sink),
            std::slice::from_ref(&legacy),
            "only the existing one"
        );

        let record = record.expect("a joint shadow record");
        let group = record.group.clone().expect("it names its portfolio");
        assert_eq!(group.portfolio, formed().id.0);
        assert_eq!(group.generation, 3);
        assert_eq!(group.members, formed().members);
        assert_eq!(record.key(), formed().id.0, "kept under the portfolio");
        assert_eq!(record.subject, formed().members[0]);
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.runs.len(), 2, "one run record per candidate");
        assert_eq!(record.shown, None);
        let agent = record.slate.clone().expect("the agent's joint slate");
        assert_eq!(agent.baseline.rank_key, "agent");
        assert_eq!(
            record.paired,
            Some(Paired::Joint(PairedJointSlate {
                portfolio: formed().id.0,
                generation: 3,
                slate: legacy,
                attached_at: JOINT_ATTACHED_AT.to_owned(),
            }))
        );
        assert_eq!(store.0.lock().expect("lock").clone(), vec![record]);

        // It claimed as a joint slate, and its tokens went to the rollout with its run.
        assert_eq!(*rollout.kinds.lock().expect("lock"), [SlateKind::Joint]);
        let reports = rollout.reports();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].completed);
        assert_eq!(reports[0].tokens, 240);
        assert_eq!(
            rollout.tokens.load(Ordering::SeqCst),
            0,
            "not counted twice"
        );
    }

    #[tokio::test]
    async fn a_failing_joint_agent_run_changes_nothing_for_the_portfolio() {
        let (rollout, store) = (FakeRollout::granting(&[]), Arc::new(Store::default()));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(Arc::clone(&rollout), store, 1, true, built);
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink), [legacy_joint_slate().await]);
        let record = record.expect("the failure is recorded");
        assert_eq!(record.outcome, ShadowOutcome::Failed);
        assert_eq!(record.slate, None);
        assert!(record.paired.is_some(), "the slate the portfolio got");
        assert!(!rollout.reports()[0].completed);
    }

    #[tokio::test]
    async fn a_refused_joint_claim_starts_no_agent_run_and_the_portfolio_is_unchanged() {
        let (until, max_slates, max_tokens) = BOUNDS;
        let mode = crate::rollout::Mode::Shadow {
            until,
            max_slates,
            max_tokens,
        };
        let refused = Claim::Refused {
            reason: "budget_exhausted".to_owned(),
        };
        let rollout = Arc::new(FakeRollout::new(mode, refused, &[]));
        let store = Arc::new(Store::default());
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = shadow_step(rollout, Arc::clone(&store), 1, false, Arc::clone(&built));
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink), [legacy_joint_slate().await]);
        assert_eq!(record, None);
        assert_eq!(built.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_existing_joint_slate_that_fell_back_or_arrived_stale_is_named_in_the_record() {
        for (attach, joint, outcome, reason) in [
            (
                PortfolioAttach::Landed,
                &Unavailable as &dyn GroupSlateProposer,
                PortfolioSlateOutcome::Fallback(formed().members),
                "the joint slate could not be produced",
            ),
            (
                PortfolioAttach::Stale,
                &CannedGroupSlateProposer,
                PortfolioSlateOutcome::Stale,
                "arrived stale",
            ),
        ] {
            let (step, _store) = completing_step();
            let sink = Recording::new(Some(true), attach);
            let (did, record) = run_portfolio(&sink, joint, Some(&step)).await;
            assert_eq!(did, outcome);
            let record = record.expect("a record");
            assert_eq!(record.paired, None);
            let why = record.paired_missing.expect("a reason");
            assert!(why.contains(reason), "{why}");
            assert!(record.slate.is_some(), "the agent's is still recorded");
        }
    }

    #[tokio::test]
    async fn a_portfolio_that_is_stale_before_the_step_starts_no_agent_run() {
        let agent = forbidden_agent();
        let sink = Recording::new(Some(false), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&agent)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Stale);
        assert_eq!(record, None);
        assert!(joint_slates(&sink).is_empty());
    }

    /// `agent` mode, entered under criteria that cover joint slates or not.
    fn joint_rollout(
        claim: Claim,
        verdict: Option<RunVerdict>,
        allows: &[bool],
        covers_joint: bool,
    ) -> Arc<FakeRollout> {
        let (until, max_slates, max_tokens) = BOUNDS;
        let mode = crate::rollout::Mode::Agent {
            until,
            max_slates,
            max_tokens,
        };
        let mut rollout = FakeRollout::new(mode, claim, allows);
        rollout.verdict = verdict;
        rollout.covers_joint = covers_joint;
        Arc::new(rollout)
    }

    fn covered(verdict: RunVerdict, allows: &[bool]) -> Arc<FakeRollout> {
        joint_rollout(FakeRollout::granted(true), Some(verdict), allows, true)
    }

    #[tokio::test]
    async fn in_agent_mode_without_joint_coverage_the_agents_joint_slate_is_only_recorded() {
        // The rollout grants a joint claim as a recorded-only run when the criteria do not
        // cover joint slates.
        let rollout = joint_rollout(
            FakeRollout::granted(false),
            Some(RunVerdict::Proceed),
            &[],
            false,
        );
        let store = Arc::new(Store::default());
        let step = agent_step(&rollout, &store, 1, "ok");
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink), [legacy_joint_slate().await]);
        let record = record.expect("a record");
        assert!(record.slate.is_some());
        let Some(Shown::Legacy { reason }) = &record.shown else {
            panic!("the existing path's was shown, got {:?}", record.shown);
        };
        assert!(reason.contains("do not cover joint slates"), "{reason}");
        // Its run still reports, so its version and its failures count.
        assert_eq!(rollout.reports().len(), 1);
    }

    #[tokio::test]
    async fn in_agent_mode_with_joint_coverage_the_agents_joint_slate_is_the_portfolios() {
        let rollout = covered(RunVerdict::Proceed, &[]);
        let store = Arc::new(Store::default());
        let step = agent_step(&rollout, &store, 2, "ok");
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        let attached = sink.portfolios.lock().expect("lock").clone();
        assert_eq!(
            attached.len(),
            1,
            "exactly one joint slate reaches the portfolio"
        );
        let (id, generation, slate) = &attached[0];
        assert_eq!((id.as_str(), *generation), (formed().id.0.as_str(), 3));
        assert_eq!(slate.baseline.rank_key, "agent");
        assert!(
            !slate.options[0].needs.is_empty(),
            "its policy needs are filled"
        );

        // The trace still opens with the coupling; the agent's step follows it.
        let steps = &slate.trace.steps;
        assert_eq!(steps[0].tag, TraceTag::Graph);
        assert!(steps[0].text.starts_with("Linked: "), "{}", steps[0].text);
        assert_eq!(steps[1].tag, TraceTag::Assumption);
        assert!(
            steps[1].text.contains("produced by the tool-using agent")
                && steps[1].text.contains("model agent-model")
                && steps[1].text.contains("propose-plan@v1")
                && steps[1].text.contains("for the same portfolio"),
            "{}",
            steps[1].text
        );

        let record = record.expect("a record");
        assert_eq!(record.shown, Some(Shown::Agent));
        assert_eq!(record.epoch, Some(1));
        let paired = record.paired.expect("the existing path's joint slate");
        assert_eq!(*paired.slate(), legacy_joint_slate().await);
        assert_eq!(paired.attached_at(), JOINT_ATTACHED_AT);
        assert_eq!(rollout.reports().len(), 1, "asked before the attach");
        assert_eq!(*rollout.kinds.lock().expect("lock"), [SlateKind::Joint]);
    }

    #[tokio::test]
    async fn a_joint_slate_slower_than_a_points_guard_is_kept_under_its_own() {
        let rollout = covered(RunVerdict::Proceed, &[]);
        let store = Arc::new(Store::default());
        let mut step = agent_step(&rollout, &store, 1, "slow");
        step.guard = std::time::Duration::from_millis(200);
        step.joint_guard = std::time::Duration::from_secs(5);
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink)[0].baseline.rank_key, "agent");
        let record = record.expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Completed);
        assert_eq!(record.shown, Some(Shown::Agent));
    }

    #[tokio::test]
    async fn a_joint_slate_over_its_own_guard_shows_the_existing_joint_slate() {
        let rollout = covered(RunVerdict::Fallback, &[]);
        let store = Arc::new(Store::default());
        let mut step = agent_step(&rollout, &store, 1, "slow");
        // Shorter than the slate takes, and shorter than a point's: the joint guard decides.
        step.guard = std::time::Duration::from_secs(5);
        step.joint_guard = std::time::Duration::from_millis(200);
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink), [legacy_joint_slate().await]);
        let record = record.expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Failed);
        let Some(Shown::Legacy { reason }) = &record.shown else {
            panic!("the existing path's was shown, got {:?}", record.shown);
        };
        assert!(reason.contains("was not ready within"), "{reason}");
    }

    #[tokio::test]
    async fn a_points_slate_keeps_its_guard_whatever_the_joint_guard_is() {
        let (subject, store) = (uri("sale.order/1"), Arc::new(Store::default()));
        let rollout = authoritative(RunVerdict::Fallback, &[]);
        let mut step = agent_step(&rollout, &store, 1, "slow");
        step.guard = std::time::Duration::from_millis(200);
        step.joint_guard = std::time::Duration::from_secs(5);
        let sink = Attached::default();
        let record = run_point(&sink, &subject, Some(&step)).await;
        let attached = sink.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1, legacy_slate(&subject).await);
        assert_eq!(record.expect("a record").outcome, ShadowOutcome::Failed);
    }

    #[tokio::test]
    async fn in_agent_mode_each_way_the_joint_run_does_not_make_it_shows_the_existing_slate() {
        let budget = Claim::Refused {
            reason: "budget_exhausted".to_owned(),
        };
        let rolled_back = RunVerdict::RolledBack {
            reason: "skill propose-plan: v1, v2".to_owned(),
        };
        for (rollout, how, reason) in [
            (
                covered(RunVerdict::Fallback, &[]),
                "fail",
                "the agent's run failed",
            ),
            (
                covered(RunVerdict::Proceed, &[false]),
                "ok",
                "the agent's run was stopped",
            ),
            (
                covered(RunVerdict::Proceed, &[]),
                "hang",
                "was not ready within",
            ),
            (
                joint_rollout(budget, None, &[], true),
                "ok",
                "no agent run was allowed to start: budget_exhausted",
            ),
            (
                covered(rolled_back, &[]),
                "ok",
                "the mode was rolled back: skill propose-plan: v1, v2",
            ),
        ] {
            let store = Arc::new(Store::default());
            let step = agent_step(&rollout, &store, 1, how);
            let sink = Recording::new(Some(true), PortfolioAttach::Landed);
            let (outcome, record) =
                run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
            assert_eq!(outcome, PortfolioSlateOutcome::Attached, "{reason}");
            assert_eq!(
                joint_slates(&sink),
                [legacy_joint_slate().await],
                "{reason}"
            );
            let record = record.expect("a record");
            let Some(Shown::Legacy { reason: why }) = &record.shown else {
                panic!(
                    "{reason}: the existing path's was shown, got {:?}",
                    record.shown
                );
            };
            assert!(why.contains(reason), "{reason}: {why}");
            assert!(record.paired.is_some(), "{reason}");
        }
    }

    #[tokio::test]
    async fn a_stale_agent_joint_slate_is_refused_and_nothing_else_is_attached() {
        let rollout = covered(RunVerdict::Proceed, &[]);
        let store = Arc::new(Store::default());
        let step = agent_step(&rollout, &store, 1, "ok");
        let sink = Recording::new(Some(true), PortfolioAttach::Stale);
        let (outcome, record) = run_portfolio(&sink, &CannedGroupSlateProposer, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Stale);
        assert_eq!(
            joint_slates(&sink).len(),
            1,
            "one refused attach, no second try"
        );
        let record = record.expect("a record");
        assert_eq!(record.shown, None, "no slate reached the portfolio");
        assert_eq!(record.paired, None);
        assert!(
            record
                .paired_missing
                .as_deref()
                .is_some_and(|why| why.contains("arrived stale")),
            "{:?}",
            record.paired_missing
        );
    }

    #[tokio::test]
    async fn when_neither_joint_slate_can_be_recorded_the_members_fall_back() {
        let rollout = covered(RunVerdict::Fallback, &[]);
        let store = Arc::new(Store::default());
        let step = agent_step(&rollout, &store, 1, "fail");
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &Unavailable, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Fallback(formed().members));
        assert!(joint_slates(&sink).is_empty());
        let record = record.expect("a record");
        assert_eq!(record.outcome, ShadowOutcome::Failed);
        assert_eq!(record.shown, None);
        assert!(record.paired.is_none() && record.paired_missing.is_some());
    }

    #[tokio::test]
    async fn a_failed_existing_joint_slate_still_leaves_the_agents_on_the_portfolio() {
        let rollout = covered(RunVerdict::Proceed, &[]);
        let store = Arc::new(Store::default());
        let step = agent_step(&rollout, &store, 1, "ok");
        let sink = Recording::new(Some(true), PortfolioAttach::Landed);
        let (outcome, record) = run_portfolio(&sink, &Unavailable, Some(&step)).await;
        assert_eq!(outcome, PortfolioSlateOutcome::Attached);
        assert_eq!(joint_slates(&sink)[0].baseline.rank_key, "agent");
        let record = record.expect("a record");
        assert_eq!(record.shown, Some(Shown::Agent));
        assert!(record.paired.is_none() && record.paired_missing.is_some());
    }

    #[tokio::test]
    async fn a_fresh_point_is_the_only_task_offered_to_the_agent() {
        let (subject, store) = (uri("sale.order/1"), Arc::new(Store::default()));
        let rollout = authoritative(RunVerdict::Proceed, &[]);
        let step = agent_step(&rollout, &store, 1, "ok");
        let (sink, world) = (Attached::default(), InMemoryWorldModel::default());
        run_task(
            &SlateTask::Point(subject.clone()),
            &sink,
            &CannedSlateProposer,
            None,
            &policy(),
            &world,
            Some(&step),
        )
        .await;
        let attached = sink.0.lock().expect("lock").clone();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1.baseline.rank_key, "agent");
    }
}
