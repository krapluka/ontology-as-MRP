//! The agent's rollout mode: a governed, durable setting in one Restate object, with a kill
//! switch, a time window and a budget.
//!
//! - `legacy` (the default): no agent run starts; every plan path runs without the agent.
//! - `shadow`: for each point that gets a legacy point slate, the bridge may also run the
//!   agent's slate (recorded apart, never shown to the operator) until the window ends, the
//!   maximum number of shadow slates has started, or the reported tokens reach the maximum.
//!   When a bound is reached, shadow stops by itself and the state says why.
//!
//! - `agent`: the same runs under the same bounds, and a point gets the agent's slate as its
//!   slate when the run completed. A portfolio gets the agent's joint slate only when the
//!   sign-off the mode was entered under is for criteria that state joint bounds; otherwise
//!   its joint run is recorded only, as in `shadow`. It is entered only with a sign-off for the active criteria,
//!   and it switches itself back to `legacy` when a run reports another version than the signed
//!   one, when runs fail or promotions are reversed more often than the criteria allow, and
//!   when other criteria are registered.
//!
//! Every switch needs who and why, and is appended to the history. The history is the audit
//! trail: Restate's own journal is a recovery log with a bounded retention, not an audit
//! record. Setting `legacy` is the kill switch: no new agent run starts, and a run in flight
//! stops before its next candidate, because each candidate asks [`RolloutState::allows`] for
//! the epoch its slate was claimed in, and any switch starts a new epoch. A rollback is such a
//! switch, made by `trigger:<name>`.
//!
//! The state also holds the promotion criteria registered so far, with who registered them, why
//! and when, and every sign-off taken against a verdict on them. A switch to `shadow` records
//! the digest of the registration active at that moment, so a window is evidence for the
//! criteria it was opened under and for no others. Registering is no switch in `legacy` and
//! `shadow`: it starts no epoch and ends no run. A sign-off never switches.
//!
//! The transitions are the pure [`apply_set_mode`], [`apply_register`], [`apply_signoff`],
//! [`apply_claim`], [`apply_tokens`], [`apply_run`] and [`apply_reversal`], tested
//! hermetically; the object only loads, journals the clock and persists. [`RolloutClient`]
//! reaches it over the Restate ingress, for the bridge and for the `rollout` tooling. Nothing
//! here touches the world model.

use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::promotion::{Criteria, Held, Verdict, Version};

/// The registered name of the rollout object.
pub const AGENT_ROLLOUT_OBJECT: &str = "AgentRollout";
/// The single key the rollout runs under.
pub const AGENT_ROLLOUT_KEY: &str = "reasoner";

/// Restate state key of the rollout state.
const STATE: &str = "state";

/// The rollout mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Mode {
    /// No agent run starts.
    #[default]
    Legacy,
    /// Shadow runs may start until `until`, for at most `max_slates` slates and `max_tokens`
    /// reported tokens.
    Shadow {
        /// The window end, in seconds since the Unix epoch.
        until: u64,
        /// The most shadow slates that may start.
        max_slates: u32,
        /// The most tokens the shadow runs may report.
        max_tokens: u64,
    },
    /// As `shadow`, and a completed run's slate is the one its point gets.
    Agent {
        /// The window end, in seconds since the Unix epoch.
        until: u64,
        /// The most agent slates that may start.
        max_slates: u32,
        /// The most tokens the agent runs may report.
        max_tokens: u64,
    },
}

impl Mode {
    /// The mode's name, as it is written.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Shadow { .. } => "shadow",
            Self::Agent { .. } => "agent",
        }
    }

    /// The window end and the two budgets of a mode that runs the agent.
    fn bounds(&self) -> Option<(u64, u32, u64)> {
        match *self {
            Self::Legacy => None,
            Self::Shadow {
                until,
                max_slates,
                max_tokens,
            }
            | Self::Agent {
                until,
                max_slates,
                max_tokens,
            } => Some((until, max_slates, max_tokens)),
        }
    }
}

/// Why shadow stopped by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stopped {
    /// The window end passed.
    WindowClosed,
    /// The slate or token budget is used up.
    BudgetExhausted,
}

/// One mode switch, as the history records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeChange {
    /// The mode set.
    #[serde(flatten)]
    pub mode: Mode,
    /// Who set it.
    pub by: String,
    /// Why.
    pub reason: String,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
    /// The digest of the criteria registration that was active at this switch, on a switch to
    /// `shadow` or `agent`. It never changes afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<String>,
    /// When the sign-off was taken that a switch to `agent` was entered under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signoff: Option<u64>,
}

/// An operator's acceptance of the evidence for the active criteria, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signoff {
    /// The digest of the criteria signed for.
    pub criteria: String,
    /// The digest of the paired report file the verdict was computed from.
    pub report: String,
    /// The verdict, as computed when signing.
    pub verdict: Verdict,
    /// What the evidence is of: the verdict's eval version.
    pub version: Version,
    /// Who signed.
    pub by: String,
    /// Why.
    pub reason: String,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
}

/// A sign-off, as requested. The object cannot read the report, so it takes the verdict the
/// command computed and checks what it can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignoffRequest {
    /// The verdict signed against.
    pub verdict: Verdict,
    /// The digest of the paired report file.
    pub report: String,
    /// Who signs.
    pub by: String,
    /// Why.
    pub reason: String,
}

/// Promotion criteria as registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// The criteria.
    pub criteria: Criteria,
    /// The digest of the document they were read from.
    pub digest: String,
    /// Who registered them.
    pub by: String,
    /// Why.
    pub reason: String,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
}

/// A registration, as requested: the criteria document's text, who and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// The document's text. The object parses it and takes its digest itself.
    pub document: String,
    /// Who registers it.
    pub by: String,
    /// Why.
    pub reason: String,
}

/// The rollout's durable state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutState {
    /// The current mode.
    #[serde(flatten)]
    pub mode: Mode,
    /// Shadow slates started since the mode was set.
    pub slates_started: u32,
    /// Tokens the shadow runs reported since the mode was set.
    pub tokens_used: u64,
    /// Why shadow stopped by itself, if it did.
    pub stopped: Option<Stopped>,
    /// Every switch, oldest first.
    pub history: Vec<ModeChange>,
    /// Every criteria registration, oldest first. The last is the active one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registrations: Vec<Registration>,
    /// Every sign-off, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signoffs: Vec<Signoff>,
    /// Agent runs that failed in a row since the mode was set, in `agent` mode.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub agent_failures: u32,
    /// Promotions of an agent slate reversed since the mode was set, in `agent` mode.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reversed_promotions: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl RolloutState {
    /// The active criteria registration: the latest.
    #[must_use]
    pub fn active(&self) -> Option<&Registration> {
        self.registrations.last()
    }

    /// The current epoch: the number of switches so far. A slate is claimed in an epoch, and
    /// any later switch ends it.
    #[must_use]
    pub fn epoch(&self) -> u32 {
        u32::try_from(self.history.len()).unwrap_or(u32::MAX)
    }

    /// Whether an agent slate claimed in `epoch` may start its next candidate: the mode still
    /// runs the agent, and it was not switched since (the kill switch).
    #[must_use]
    pub fn allows(&self, epoch: u32) -> bool {
        self.mode.bounds().is_some() && self.epoch() == epoch
    }

    /// Whether the current `agent` mode covers joint slates: the criteria of the sign-off it
    /// was entered under state joint bounds. No sign-off given for other criteria does.
    #[must_use]
    pub fn covers_joint(&self) -> bool {
        self.entered_under().is_some_and(|signoff| {
            self.registrations
                .iter()
                .rev()
                .find(|registration| registration.digest == signoff.criteria)
                .is_some_and(|registration| registration.criteria.covers_joint())
        })
    }

    /// The sign-off the current `agent` mode was entered under.
    #[must_use]
    pub fn entered_under(&self) -> Option<&Signoff> {
        let entry = self
            .history
            .last()
            .filter(|entry| matches!(entry.mode, Mode::Agent { .. }))?;
        self.signoffs
            .iter()
            .rev()
            .find(|signoff| Some(signoff.at) == entry.signoff)
    }
}

/// A mode switch, as requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetModeRequest {
    /// The mode to set.
    #[serde(flatten)]
    pub mode: Mode,
    /// Who sets it.
    pub by: String,
    /// Why.
    pub reason: String,
}

/// What a slate claim answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "claim", rename_all = "snake_case")]
pub enum Claim {
    /// One agent slate may run, in `epoch`; it is the `slate`-th of the mode.
    Granted {
        /// The epoch the slate runs in.
        epoch: u32,
        /// Which slate of the mode it is (1-based).
        slate: u32,
        /// Whether the slate is the one its point or portfolio gets: the mode is `agent`, and
        /// for a joint slate it was entered under criteria that cover joint slates.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        authoritative: bool,
    },
    /// No shadow slate may run now.
    Refused {
        /// `legacy`, `window_closed` or `budget_exhausted`.
        reason: String,
    },
}

/// Why a switch was refused, with the HTTP status the ingress answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// 400: a switch without who or why, or a shadow mode without a future window or budgets.
    pub code: u16,
    /// Why, for the operator.
    pub reason: String,
}

impl Refusal {
    fn bad(reason: impl Into<String>) -> Self {
        Self {
            code: 400,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.reason, self.code)
    }
}

impl From<Refusal> for HandlerError {
    fn from(refusal: Refusal) -> Self {
        TerminalError::new_with_code(refusal.code, refusal.reason).into()
    }
}

/// What an agent run reports when its slate is the one a point would get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// The epoch the slate was claimed in.
    pub epoch: u32,
    /// The tokens its runs used.
    pub tokens: u64,
    /// Whether the run completed with a slate.
    pub completed: bool,
    /// What it ran with.
    pub version: Version,
}

/// What the rollout answers a [`RunReport`]: whether the agent's slate may reach the point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum RunVerdict {
    /// The run is of the signed version: its slate is the point's.
    Proceed,
    /// The mode was switched since the slate was claimed: the point gets the existing path's.
    Stale,
    /// The report switched the mode back to `legacy`: the point gets the existing path's.
    RolledBack {
        /// What the trigger observed.
        reason: String,
    },
    /// The run failed and was counted: the point gets the existing path's.
    Fallback,
}

/// The actor a rollback trigger's switch is recorded with.
#[must_use]
pub fn trigger_actor(trigger: &str) -> String {
    format!("trigger:{trigger}")
}

/// Sets `mode` at `now` for `by`: the counters reset, `stopped` clears, and the switch is
/// appended to the history (a new epoch). Every switch goes through here, a trigger's too.
fn switch(state: &mut RolloutState, mode: Mode, by: &str, reason: &str, now: u64) {
    let agent = matches!(mode, Mode::Agent { .. });
    let entry = ModeChange {
        criteria: mode
            .bounds()
            .and_then(|_| state.active().map(|active| active.digest.clone())),
        signoff: agent
            .then(|| state.signoffs.last().map(|signoff| signoff.at))
            .flatten(),
        mode: mode.clone(),
        by: by.to_owned(),
        reason: reason.to_owned(),
        at: now,
    };
    state.mode = mode;
    state.slates_started = 0;
    state.tokens_used = 0;
    state.agent_failures = 0;
    state.reversed_promotions = 0;
    state.stopped = None;
    state.history.push(entry);
}

/// A rollback: `trigger` switches the mode to `legacy` at `now`, recording what it `observed`.
fn roll_back(state: &mut RolloutState, trigger: &str, observed: &str, now: u64) {
    switch(state, Mode::Legacy, &trigger_actor(trigger), observed, now);
}

/// Switches `state` to `request.mode` at `now`: the counters reset, `stopped` clears, and the
/// switch is appended to the history (a new epoch).
///
/// # Errors
/// A 400 [`Refusal`] for an empty `by` or `reason`, a shadow or agent mode whose window end is
/// not in the future or whose budgets are zero, or an agent mode whose latest sign-off is not
/// for the active criteria. A refused switch changes nothing.
pub fn apply_set_mode(
    state: &mut RolloutState,
    request: &SetModeRequest,
    now: u64,
) -> Result<(), Refusal> {
    let (by, reason) = (request.by.trim(), request.reason.trim());
    if by.is_empty() {
        return Err(Refusal::bad("a mode change needs who makes it (`by`)"));
    }
    if reason.is_empty() {
        return Err(Refusal::bad("a mode change needs a reason"));
    }
    let name = request.mode.name();
    if let Some((until, max_slates, max_tokens)) = request.mode.bounds() {
        if until <= now {
            return Err(Refusal::bad(format!(
                "a {name} window must end in the future (until {until}, now {now})"
            )));
        }
        if max_slates == 0 || max_tokens == 0 {
            return Err(Refusal::bad(format!(
                "a {name} mode needs a maximum number of slates and of tokens above zero"
            )));
        }
    }
    if matches!(request.mode, Mode::Agent { .. }) {
        let Some(active) = state.active() else {
            return Err(Refusal::bad(
                "the agent mode needs registered promotion criteria and a sign-off for them",
            ));
        };
        match state.signoffs.last() {
            Some(signoff) if signoff.criteria == active.digest => {}
            Some(signoff) => {
                return Err(Refusal::bad(format!(
                    "the agent mode needs a sign-off for the active criteria ({}); the latest \
                     sign-off is for {}",
                    active.digest, signoff.criteria
                )));
            }
            None => {
                return Err(Refusal::bad(format!(
                    "the agent mode needs a sign-off for the active criteria ({})",
                    active.digest
                )));
            }
        }
    }
    switch(state, request.mode.clone(), by, reason, now);
    Ok(())
}

/// Registers the criteria of `request.document` at `now` as the active ones. In `legacy` and
/// `shadow` the mode, its counters and the history are left alone, so no window is tied to them
/// and no run ends. In `agent` the mode is rolled back: the sign-off it was entered under does
/// not cover the new criteria. Registering the active document again changes nothing.
///
/// # Errors
/// A 400 [`Refusal`] for an empty `by` or `reason`, or a document that is not a criteria
/// document. A refused registration changes nothing.
pub fn apply_register(
    state: &mut RolloutState,
    request: &RegisterRequest,
    now: u64,
) -> Result<(), Refusal> {
    let (by, reason) = (request.by.trim(), request.reason.trim());
    if by.is_empty() {
        return Err(Refusal::bad("a registration needs who makes it (`by`)"));
    }
    if reason.is_empty() {
        return Err(Refusal::bad("a registration needs a reason"));
    }
    let criteria = Criteria::parse(&request.document).map_err(Refusal::bad)?;
    let digest = Criteria::digest_of(&request.document);
    if state.active().is_some_and(|active| active.digest == digest) {
        return Ok(());
    }
    let signed = state
        .entered_under()
        .map(|signoff| signoff.criteria.clone());
    state.registrations.push(Registration {
        criteria,
        digest: digest.clone(),
        by: by.to_owned(),
        reason: reason.to_owned(),
        at: now,
    });
    if matches!(state.mode, Mode::Agent { .. }) {
        let observed = format!(
            "criteria {digest} were registered; the mode was entered under a sign-off for {}",
            signed.as_deref().unwrap_or("none")
        );
        roll_back(state, "criteria_changed", &observed, now);
    }
    Ok(())
}

/// Records a sign-off of the active criteria at `now`. The mode, its counters and the history
/// are left alone: entering `agent` is a switch of its own.
///
/// # Errors
/// A 400 [`Refusal`] for an empty `by` or `reason`, a verdict for other criteria than the
/// active ones, a verdict that is not met or names no eval version, or a signer who registered
/// the active criteria. A refused sign-off changes nothing.
pub fn apply_signoff(
    state: &mut RolloutState,
    request: &SignoffRequest,
    now: u64,
) -> Result<(), Refusal> {
    let (by, reason) = (request.by.trim(), request.reason.trim());
    if by.is_empty() {
        return Err(Refusal::bad("a sign-off needs who signs (`by`)"));
    }
    if reason.is_empty() {
        return Err(Refusal::bad("a sign-off needs a reason"));
    }
    let Some(active) = state.active() else {
        return Err(Refusal::bad("no promotion criteria are registered"));
    };
    let verdict = &request.verdict;
    if verdict.criteria.as_deref() != Some(active.digest.as_str()) {
        return Err(Refusal::bad(format!(
            "the verdict is for other criteria ({}), the active ones are {}",
            verdict.criteria.as_deref().unwrap_or("none"),
            active.digest
        )));
    }
    let unmet: Vec<&str> = verdict
        .checks
        .iter()
        .filter(|check| check.held != Held::Held)
        .map(|check| check.name.as_str())
        .collect();
    if !verdict.met || !unmet.is_empty() {
        return Err(Refusal::bad(format!(
            "the criteria are not met: {}",
            if unmet.is_empty() {
                "the verdict says so".to_owned()
            } else {
                unmet.join("; ")
            }
        )));
    }
    let Some(version) = verdict.eval_version.clone() else {
        return Err(Refusal::bad(
            "the verdict names no eval version to sign for",
        ));
    };
    if by == active.by {
        return Err(Refusal::bad(format!(
            "{by} registered the active criteria; another operator has to sign"
        )));
    }
    let signoff = Signoff {
        criteria: active.digest.clone(),
        report: request.report.clone(),
        verdict: verdict.clone(),
        version,
        by: by.to_owned(),
        reason: reason.to_owned(),
        at: now,
    };
    state.signoffs.push(signoff);
    Ok(())
}

/// What an agent slate is claimed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlateKind {
    /// A point's slate.
    Point,
    /// A portfolio's joint slate.
    Joint,
}

/// Claims one agent slate of `kind` at `now`: granted, and counted, only in `shadow` or `agent`,
/// before the window end, with both budgets left. Otherwise refused; a closed window or a spent
/// budget also sets `stopped`, so reading the state says why the agent stopped. The mode stays
/// what it is. Both kinds share the window and the budgets.
///
/// A joint slate is granted as the one its portfolio gets only in `agent` mode entered under
/// criteria that cover joint slates. Otherwise it is granted as a recorded-only run.
pub fn apply_claim(state: &mut RolloutState, now: u64, kind: SlateKind) -> Claim {
    let Some((until, max_slates, max_tokens)) = state.mode.bounds() else {
        return Claim::Refused {
            reason: "legacy".to_owned(),
        };
    };
    let stopped = if now >= until {
        Some(Stopped::WindowClosed)
    } else if state.slates_started >= max_slates || state.tokens_used >= max_tokens {
        Some(Stopped::BudgetExhausted)
    } else {
        None
    };
    if let Some(stopped) = stopped {
        state.stopped = Some(stopped);
        return Claim::Refused {
            reason: match stopped {
                Stopped::WindowClosed => "window_closed",
                Stopped::BudgetExhausted => "budget_exhausted",
            }
            .to_owned(),
        };
    }
    state.slates_started += 1;
    Claim::Granted {
        epoch: state.epoch(),
        slate: state.slates_started,
        authoritative: matches!(state.mode, Mode::Agent { .. })
            && (kind == SlateKind::Point || state.covers_joint()),
    }
}

/// Adds `tokens` the shadow runs reported (informational, a cost control).
pub fn apply_tokens(state: &mut RolloutState, tokens: u64) {
    state.tokens_used = state.tokens_used.saturating_add(tokens);
}

/// Takes the report of an agent run whose slate would be its point's or its portfolio's, at
/// `now`, and says whether it may be. The tokens count whatever the answer. A joint run reports
/// here too when it is only recorded, so its version and its failures count as a point's do.
///
/// - A report of another epoch than the current one, or outside `agent`, is [`RunVerdict::Stale`].
/// - A completed run of another engine version, model or skill version than the sign-off the
///   mode was entered under rolls the mode back. A skill only one of the two names is no
///   difference. Otherwise it ends a run of failures.
/// - A failed run is counted; more failures in a row than the active criteria allow roll the
///   mode back. Criteria that state no bound trigger nothing.
pub fn apply_run(state: &mut RolloutState, report: &RunReport, now: u64) -> RunVerdict {
    apply_tokens(state, report.tokens);
    if !matches!(state.mode, Mode::Agent { .. }) || report.epoch != state.epoch() {
        return RunVerdict::Stale;
    }
    if !report.completed {
        state.agent_failures = state.agent_failures.saturating_add(1);
        let bound = state
            .active()
            .and_then(|active| active.criteria.rollback().max_consecutive_agent_failures);
        if let Some(most) = bound.filter(|most| state.agent_failures > *most) {
            let observed = format!(
                "{} agent runs failed in a row, the criteria allow {most}",
                state.agent_failures
            );
            roll_back(state, "agent_failures", &observed, now);
        }
        return RunVerdict::Fallback;
    }
    let observed = match state.entered_under() {
        Some(signoff) => {
            let differ = crate::promotion::differences(&[&signoff.version, &report.version].into());
            (!differ.is_empty()).then(|| {
                format!(
                    "an agent run is of another version than the signed one: {}",
                    differ.join("; ")
                )
            })
        }
        // Without the sign-off there is nothing to hold the run against.
        None => Some("the sign-off the mode was entered under is not recorded".to_owned()),
    };
    if let Some(observed) = observed {
        roll_back(state, "version_changed", &observed, now);
        return RunVerdict::RolledBack { reason: observed };
    }
    state.agent_failures = 0;
    RunVerdict::Proceed
}

/// Counts a reversed promotion of an agent slate claimed in `epoch`, at `now`. Only the current
/// epoch of `agent` counts; more reversals than the active criteria allow roll the mode back.
pub fn apply_reversal(state: &mut RolloutState, epoch: u32, now: u64) {
    if !matches!(state.mode, Mode::Agent { .. }) || epoch != state.epoch() {
        return;
    }
    state.reversed_promotions = state.reversed_promotions.saturating_add(1);
    let bound = state
        .active()
        .and_then(|active| active.criteria.rollback().max_reversed_promotions);
    if let Some(most) = bound.filter(|most| state.reversed_promotions > *most) {
        let observed = format!(
            "{} promotion(s) of an agent slate were reversed, the criteria allow {most}",
            state.reversed_promotions
        );
        roll_back(state, "promotion_reversed", &observed, now);
    }
}

/// The rollout object.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentRollout;

/// Seconds since the Unix epoch. A clock read, so call it only inside `ctx.run`.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[restate_sdk::object]
impl AgentRollout {
    /// Switch the mode, recording who, why and when. Setting `legacy` is the kill switch.
    #[handler]
    async fn set_mode(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<SetModeRequest>,
    ) -> Result<Json<RolloutState>, HandlerError> {
        let request = request.into_inner();
        let mut state = load(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("set_at")
            .await?;
        apply_set_mode(&mut state, &request, now)?;
        ctx.set(STATE, Json(state.clone()));
        Ok(Json(state))
    }

    /// Register promotion criteria, recording who, why and when. It switches nothing.
    #[handler]
    async fn register_criteria(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<RegisterRequest>,
    ) -> Result<Json<RolloutState>, HandlerError> {
        let request = request.into_inner();
        let mut state = load(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("registered_at")
            .await?;
        apply_register(&mut state, &request, now)?;
        ctx.set(STATE, Json(state.clone()));
        Ok(Json(state))
    }

    /// Record a sign-off of the active criteria against a verdict. It switches nothing.
    #[handler]
    async fn sign_off(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<SignoffRequest>,
    ) -> Result<Json<RolloutState>, HandlerError> {
        let request = request.into_inner();
        let mut state = load(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("signed_at")
            .await?;
        apply_signoff(&mut state, &request, now)?;
        ctx.set(STATE, Json(state.clone()));
        Ok(Json(state))
    }

    /// Take the report of an agent run in `agent` mode and say whether its slate may reach the
    /// point. May roll the mode back.
    #[handler]
    async fn report_run(
        &self,
        ctx: ObjectContext<'_>,
        report: Json<RunReport>,
    ) -> Result<Json<RunVerdict>, HandlerError> {
        let report = report.into_inner();
        let mut state = load(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("reported_at")
            .await?;
        let verdict = apply_run(&mut state, &report, now);
        ctx.set(STATE, Json(state));
        Ok(Json(verdict))
    }

    /// Count a reversed promotion of an agent slate claimed in `epoch`. May roll the mode back.
    #[handler]
    async fn report_reversal(
        &self,
        ctx: ObjectContext<'_>,
        epoch: Json<u32>,
    ) -> Result<(), HandlerError> {
        let mut state = load(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("reversed_at")
            .await?;
        apply_reversal(&mut state, epoch.into_inner(), now);
        ctx.set(STATE, Json(state));
        Ok(())
    }

    /// Claim one agent slate for a point (counted), or be refused.
    #[handler]
    async fn claim_slate(&self, ctx: ObjectContext<'_>) -> Result<Json<Claim>, HandlerError> {
        claim(&ctx, SlateKind::Point).await
    }

    /// Claim one agent slate for a portfolio's joint slate (counted against the same budgets),
    /// or be refused.
    #[handler]
    async fn claim_joint_slate(&self, ctx: ObjectContext<'_>) -> Result<Json<Claim>, HandlerError> {
        claim(&ctx, SlateKind::Joint).await
    }

    /// Add the tokens a shadow slate's runs reported.
    #[handler]
    async fn report_tokens(
        &self,
        ctx: ObjectContext<'_>,
        tokens: Json<u64>,
    ) -> Result<(), HandlerError> {
        let mut state = load(&ctx).await?;
        apply_tokens(&mut state, tokens.into_inner());
        ctx.set(STATE, Json(state));
        Ok(())
    }

    /// The rollout state. Shared: each shadow candidate reads it (the kill switch).
    #[handler]
    async fn get(&self, ctx: SharedObjectContext<'_>) -> Result<Json<RolloutState>, HandlerError> {
        Ok(Json(
            ctx.get::<Json<RolloutState>>(STATE)
                .await?
                .map(Json::into_inner)
                .unwrap_or_default(),
        ))
    }
}

async fn claim(ctx: &ObjectContext<'_>, kind: SlateKind) -> Result<Json<Claim>, HandlerError> {
    let mut state = load(ctx).await?;
    let now = ctx
        .run(|| async { Ok::<_, HandlerError>(unix_now()) })
        .name("claimed_at")
        .await?;
    let claim = apply_claim(&mut state, now, kind);
    ctx.set(STATE, Json(state));
    Ok(Json(claim))
}

async fn load(ctx: &ObjectContext<'_>) -> Result<RolloutState, HandlerError> {
    Ok(ctx
        .get::<Json<RolloutState>>(STATE)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default())
}

/// Why an object call over the ingress failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressError {
    /// The ingress could not be reached, or answered something unreadable.
    Unreachable(String),
    /// The object refused the request: the HTTP status and the ingress's message.
    Refused(u16, String),
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "unreachable: {why}"),
            Self::Refused(code, why) => write!(f, "refused ({code}): {why}"),
        }
    }
}

impl std::error::Error for IngressError {}

/// `POST {ingress}/{object}/{key}/{handler}` with an optional JSON `input`, reading a JSON
/// answer (`()` for an empty one).
///
/// # Errors
/// [`IngressError`].
pub(crate) async fn call_object<I: Serialize + ?Sized, O: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    ingress: &str,
    path: [&str; 3],
    input: Option<&I>,
) -> Result<O, IngressError> {
    let mut url =
        reqwest::Url::parse(ingress).map_err(|e| IngressError::Unreachable(e.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| IngressError::Unreachable("the ingress url cannot be a base".to_owned()))?
        .pop_if_empty()
        .extend(path);
    let mut request = http.post(url);
    if let Some(input) = input {
        let body =
            serde_json::to_vec(input).map_err(|e| IngressError::Unreachable(e.to_string()))?;
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| IngressError::Unreachable(e.to_string()))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| IngressError::Unreachable(e.to_string()))?;
    if !status.is_success() {
        // The ingress answers a terminal error as `{"message": …}`; keep the text otherwise.
        let message = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        return Err(IngressError::Refused(status.as_u16(), message));
    }
    let body: &[u8] = if body.is_empty() { b"null" } else { &body };
    serde_json::from_slice(body)
        .map_err(|e| IngressError::Unreachable(format!("unreadable {} answer: {e}", path[2])))
}

/// The rollout object over the Restate ingress (plain HTTP).
#[derive(Debug, Clone)]
pub struct RolloutClient {
    http: reqwest::Client,
    ingress: String,
    key: String,
}

impl RolloutClient {
    /// A client of the rollout behind the ingress at `ingress` (e.g. `http://localhost:8080`).
    #[must_use]
    pub fn new(http: reqwest::Client, ingress: impl Into<String>) -> Self {
        Self {
            http,
            ingress: ingress.into(),
            key: AGENT_ROLLOUT_KEY.to_owned(),
        }
    }

    /// The same client for the rollout under `key`. The engine runs one rollout, under
    /// [`AGENT_ROLLOUT_KEY`]; another key is a rollout of its own, for a suite that must not
    /// touch the engine's.
    #[must_use]
    pub fn under(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    /// The key this client's rollout runs under.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    async fn call<I: Serialize + ?Sized, O: serde::de::DeserializeOwned>(
        &self,
        handler: &str,
        input: Option<&I>,
    ) -> Result<O, IngressError> {
        call_object(
            &self.http,
            &self.ingress,
            [AGENT_ROLLOUT_OBJECT, &self.key, handler],
            input,
        )
        .await
    }

    /// Switch the mode.
    ///
    /// # Errors
    /// [`IngressError`]; a refused switch is [`IngressError::Refused`] with 400.
    pub async fn set_mode(&self, request: &SetModeRequest) -> Result<RolloutState, IngressError> {
        self.call("set_mode", Some(request)).await
    }

    /// Register promotion criteria.
    ///
    /// # Errors
    /// [`IngressError`]; a refused registration is [`IngressError::Refused`] with 400.
    pub async fn register_criteria(
        &self,
        request: &RegisterRequest,
    ) -> Result<RolloutState, IngressError> {
        self.call("register_criteria", Some(request)).await
    }

    /// Record a sign-off.
    ///
    /// # Errors
    /// [`IngressError`]; a refused sign-off is [`IngressError::Refused`] with 400.
    pub async fn sign_off(&self, request: &SignoffRequest) -> Result<RolloutState, IngressError> {
        self.call("sign_off", Some(request)).await
    }

    /// Report an agent run of `agent` mode.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn report_run(&self, report: &RunReport) -> Result<RunVerdict, IngressError> {
        self.call("report_run", Some(report)).await
    }

    /// Report a reversed promotion of an agent slate claimed in `epoch`.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn report_reversal(&self, epoch: u32) -> Result<(), IngressError> {
        self.call("report_reversal", Some(&epoch)).await
    }

    /// Claim one agent slate of `kind`.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn claim_slate(&self, kind: SlateKind) -> Result<Claim, IngressError> {
        let handler = match kind {
            SlateKind::Point => "claim_slate",
            SlateKind::Joint => "claim_joint_slate",
        };
        self.call::<(), _>(handler, None).await
    }

    /// Report tokens a shadow slate's runs used.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn report_tokens(&self, tokens: u64) -> Result<(), IngressError> {
        self.call("report_tokens", Some(&tokens)).await
    }

    /// The rollout state.
    ///
    /// # Errors
    /// [`IngressError`].
    pub async fn get(&self) -> Result<RolloutState, IngressError> {
        self.call::<(), _>("get", None).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn shadow(until: u64, max_slates: u32, max_tokens: u64) -> SetModeRequest {
        SetModeRequest {
            mode: Mode::Shadow {
                until,
                max_slates,
                max_tokens,
            },
            by: "ops".to_owned(),
            reason: "collect evidence".to_owned(),
        }
    }

    fn legacy(reason: &str) -> SetModeRequest {
        SetModeRequest {
            mode: Mode::Legacy,
            by: "ops".to_owned(),
            reason: reason.to_owned(),
        }
    }

    fn granted(claim: &Claim) -> bool {
        matches!(claim, Claim::Granted { .. })
    }

    fn refused(claim: &Claim, why: &str) -> bool {
        matches!(claim, Claim::Refused { reason } if reason == why)
    }

    #[test]
    fn the_default_is_legacy_and_claims_nothing() {
        let mut state = RolloutState::default();
        assert_eq!(state.mode, Mode::Legacy);
        assert!(state.history.is_empty());
        assert!(refused(
            &apply_claim(&mut state, NOW, SlateKind::Point),
            "legacy"
        ));
        assert_eq!(state.slates_started, 0);
        assert!(!state.allows(0));
    }

    #[test]
    fn a_switch_without_who_or_why_or_with_a_past_window_is_refused() {
        let mut state = RolloutState::default();
        let mut no_actor = shadow(NOW + 60, 1, 1);
        no_actor.by = "  ".to_owned();
        let mut no_reason = shadow(NOW + 60, 1, 1);
        no_reason.reason = String::new();
        for request in [
            no_actor,
            no_reason,
            shadow(NOW, 1, 1),
            shadow(NOW - 1, 1, 1),
            shadow(NOW + 60, 0, 1),
            shadow(NOW + 60, 1, 0),
        ] {
            let refusal = apply_set_mode(&mut state, &request, NOW).expect_err("refused");
            assert_eq!(refusal.code, 400, "{refusal}");
            assert_eq!(state, RolloutState::default(), "unchanged");
        }
    }

    #[test]
    fn a_claim_is_granted_only_in_shadow_inside_the_window_within_both_budgets() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 60, 2, 1_000), NOW).expect("shadow");
        let first = apply_claim(&mut state, NOW + 1, SlateKind::Point);
        assert_eq!(
            first,
            Claim::Granted {
                epoch: 1,
                slate: 1,
                authoritative: false
            }
        );
        assert!(state.allows(1));
        assert!(granted(&apply_claim(&mut state, NOW + 2, SlateKind::Point)));
        assert_eq!(state.slates_started, 2);
        // The slate budget is spent.
        assert!(refused(
            &apply_claim(&mut state, NOW + 3, SlateKind::Point),
            "budget_exhausted"
        ));
        assert_eq!(state.stopped, Some(Stopped::BudgetExhausted));
        assert_eq!(state.slates_started, 2, "a refused claim is not counted");
    }

    #[test]
    fn the_token_budget_ends_shadow() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 60, 10, 1_000), NOW).expect("shadow");
        assert!(granted(&apply_claim(&mut state, NOW, SlateKind::Point)));
        apply_tokens(&mut state, 600);
        assert!(
            granted(&apply_claim(&mut state, NOW, SlateKind::Point)),
            "600 of 1000 used"
        );
        apply_tokens(&mut state, 400);
        assert!(refused(
            &apply_claim(&mut state, NOW, SlateKind::Point),
            "budget_exhausted"
        ));
        assert_eq!(state.stopped, Some(Stopped::BudgetExhausted));
        assert_eq!(state.tokens_used, 1_000);
    }

    #[test]
    fn the_window_ends_shadow() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 60, 10, 1_000), NOW).expect("shadow");
        assert!(refused(
            &apply_claim(&mut state, NOW + 60, SlateKind::Point),
            "window_closed"
        ));
        assert_eq!(state.stopped, Some(Stopped::WindowClosed));
    }

    #[test]
    fn the_history_records_every_switch_in_order_and_a_switch_resets_the_counters() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 60, 1, 10), NOW).expect("shadow");
        assert!(granted(&apply_claim(&mut state, NOW, SlateKind::Point)));
        apply_tokens(&mut state, 10);
        assert!(refused(
            &apply_claim(&mut state, NOW, SlateKind::Point),
            "budget_exhausted"
        ));
        apply_set_mode(&mut state, &legacy("evidence gathered"), NOW + 5).expect("legacy");
        assert_eq!(
            (state.slates_started, state.tokens_used, state.stopped),
            (0, 0, None)
        );
        let modes: Vec<(&Mode, &str, u64)> = state
            .history
            .iter()
            .map(|change| (&change.mode, change.reason.as_str(), change.at))
            .collect();
        assert_eq!(
            modes,
            vec![
                (
                    &Mode::Shadow {
                        until: NOW + 60,
                        max_slates: 1,
                        max_tokens: 10
                    },
                    "collect evidence",
                    NOW
                ),
                (&Mode::Legacy, "evidence gathered", NOW + 5),
            ]
        );
        assert!(state.history.iter().all(|change| change.by == "ops"));
    }

    #[test]
    fn setting_legacy_refuses_every_later_claim_and_stops_runs_in_flight() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 600, 10, 1_000), NOW).expect("shadow");
        let Claim::Granted { epoch, .. } = apply_claim(&mut state, NOW, SlateKind::Point) else {
            panic!("granted");
        };
        assert!(state.allows(epoch), "the run may go on");
        apply_set_mode(&mut state, &legacy("kill switch"), NOW + 1).expect("legacy");
        assert!(!state.allows(epoch), "the run in flight stops");
        for t in 0..3 {
            assert!(refused(
                &apply_claim(&mut state, NOW + 2 + t, SlateKind::Point),
                "legacy"
            ));
        }
        // Back to shadow: a new epoch, so the old run still does not resume.
        apply_set_mode(&mut state, &shadow(NOW + 600, 10, 1_000), NOW + 9).expect("shadow");
        assert!(!state.allows(epoch));
        assert!(state.allows(state.epoch()));
    }

    fn registration(document: &str, reason: &str) -> RegisterRequest {
        RegisterRequest {
            document: document.to_owned(),
            by: "ops".to_owned(),
            reason: reason.to_owned(),
        }
    }

    const CRITERIA: &str = crate::promotion::tests::DOCUMENT;

    #[test]
    fn a_registration_is_appended_and_leaves_the_mode_and_its_budget_use_alone() {
        let mut state = RolloutState::default();
        assert!(state.active().is_none());
        apply_set_mode(&mut state, &shadow(NOW + 600, 5, 1_000), NOW).expect("shadow");
        assert!(granted(&apply_claim(&mut state, NOW + 1, SlateKind::Point)));
        apply_tokens(&mut state, 40);
        let before = state.clone();

        apply_register(&mut state, &registration(CRITERIA, "first"), NOW + 2).expect("first");
        let active = state.active().expect("active");
        assert_eq!(active.digest, Criteria::digest_of(CRITERIA));
        assert_eq!(
            (active.by.as_str(), active.reason.as_str(), active.at),
            ("ops", "first", NOW + 2)
        );
        assert_eq!(active.criteria.shadow.min_compared, 4);
        assert_eq!(state.mode, before.mode);
        assert_eq!(
            (state.slates_started, state.tokens_used, state.stopped),
            (1, 40, None)
        );
        assert_eq!(state.history, before.history);
        assert_eq!(state.epoch(), before.epoch(), "a run in flight goes on");

        // The same document again is no new registration; another one is, after the first.
        apply_register(&mut state, &registration(CRITERIA, "again"), NOW + 3).expect("again");
        assert_eq!(state.registrations.len(), 1);
        let other = CRITERIA.replace("min_compared: 4", "min_compared: 9");
        apply_register(&mut state, &registration(&other, "stricter"), NOW + 4).expect("other");
        let reasons: Vec<&str> = state
            .registrations
            .iter()
            .map(|r| r.reason.as_str())
            .collect();
        assert_eq!(reasons, ["first", "stricter"]);
        assert_eq!(
            state.active().expect("active").digest,
            Criteria::digest_of(&other)
        );
    }

    #[test]
    fn a_registration_without_who_or_why_or_with_a_broken_document_changes_nothing() {
        let mut state = RolloutState::default();
        apply_register(&mut state, &registration(CRITERIA, "first"), NOW).expect("first");
        let before = state.clone();
        let mut nobody = registration(CRITERIA, "x");
        nobody.by = " ".to_owned();
        let broken = format!("{CRITERIA}notes: x\n");
        for (request, why) in [
            (nobody, "who"),
            (registration(CRITERIA, ""), "reason"),
            (registration(&broken, "x"), "notes"),
        ] {
            let refusal = apply_register(&mut state, &request, NOW + 1).expect_err(why);
            assert_eq!(refusal.code, 400);
            assert!(refusal.reason.contains(why), "{refusal}");
            assert_eq!(state, before);
        }
    }

    #[test]
    fn a_shadow_switch_carries_the_criteria_active_at_that_moment_and_keeps_them() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 600, 5, 1_000), NOW).expect("shadow");
        assert_eq!(state.history[0].criteria, None, "nothing was registered");

        apply_register(&mut state, &registration(CRITERIA, "first"), NOW + 1).expect("first");
        assert_eq!(
            state.history[0].criteria, None,
            "the open window is not tied to criteria registered after it opened"
        );
        apply_set_mode(&mut state, &legacy("closed"), NOW + 2).expect("legacy");
        assert_eq!(state.history[1].criteria, None, "legacy is no window");
        apply_set_mode(&mut state, &shadow(NOW + 600, 5, 1_000), NOW + 3).expect("shadow");
        let first = Criteria::digest_of(CRITERIA);
        assert_eq!(state.history[2].criteria.as_deref(), Some(first.as_str()));

        let other = CRITERIA.replace("min_compared: 4", "min_compared: 9");
        apply_register(&mut state, &registration(&other, "stricter"), NOW + 4).expect("other");
        assert_eq!(state.history[2].criteria.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn stored_state_reads_and_writes_as_before_without_registrations() {
        let stored = serde_json::json!({
            "mode": "shadow", "until": NOW + 60, "max_slates": 1, "max_tokens": 10,
            "slates_started": 1, "tokens_used": 7, "stopped": null,
            "history": [{
                "mode": "shadow", "until": NOW + 60, "max_slates": 1, "max_tokens": 10,
                "by": "ops", "reason": "collect evidence", "at": NOW
            }]
        });
        let state: RolloutState = serde_json::from_value(stored.clone()).expect("an older state");
        assert!(state.registrations.is_empty() && state.active().is_none());
        assert_eq!(state.history[0].criteria, None);
        assert_eq!(serde_json::to_value(&state).expect("json"), stored);

        let mut with = state;
        apply_register(&mut with, &registration(CRITERIA, "first"), NOW).expect("first");
        let json = serde_json::to_value(&with).expect("json");
        assert_eq!(
            json["registrations"][0]["criteria"]["eval"]["min_trials"],
            3
        );
        assert_eq!(
            serde_json::from_value::<RolloutState>(json).expect("back"),
            with
        );
    }

    #[test]
    fn the_state_is_snake_case_json() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 60, 1, 10), NOW).expect("shadow");
        let json = serde_json::to_value(&state).expect("json");
        assert_eq!(json["mode"], "shadow");
        assert_eq!(json["until"], NOW + 60);
        assert_eq!(json["max_slates"], 1);
        assert_eq!(json["history"][0]["by"], "ops");
        let back: RolloutState = serde_json::from_value(json).expect("back");
        assert_eq!(back, state);
        assert_eq!(
            serde_json::to_value(Claim::Granted {
                epoch: 1,
                slate: 1,
                authoritative: false
            })
            .expect("json"),
            serde_json::json!({ "claim": "granted", "epoch": 1, "slate": 1 })
        );
    }

    // --- the sign-off ---

    const SIGNED_REPORT: &str = "report-digest";

    fn version(model: &str, skills: &[(&str, &str)]) -> Version {
        Version {
            engine: "0.1.0".to_owned(),
            model: model.to_owned(),
            skills: skills
                .iter()
                .map(|(name, version)| ((*name).to_owned(), (*version).to_owned()))
                .collect(),
        }
    }

    fn signed_version() -> Version {
        version(
            "m/1",
            &[("propose-plan", "v1"), ("process-work-centres", "v1")],
        )
    }

    /// A met verdict on `criteria`, of the signed version.
    fn met(criteria: &str) -> Verdict {
        Verdict {
            criteria: Some(Criteria::digest_of(criteria)),
            met: true,
            checks: vec![crate::promotion::Check {
                name: "eval: cases the agent passes at k".to_owned(),
                required: "at least 3".to_owned(),
                observed: "4 of 4".to_owned(),
                held: Held::Held,
            }],
            eval_missing: None,
            shadow: crate::promotion::ShadowEvidence::default(),
            eval_version: Some(signed_version()),
            shadow_versions: Vec::new(),
        }
    }

    fn signoff(verdict: Verdict, by: &str) -> SignoffRequest {
        SignoffRequest {
            verdict,
            report: SIGNED_REPORT.to_owned(),
            by: by.to_owned(),
            reason: "the evidence holds".to_owned(),
        }
    }

    fn agent(until: u64, max_slates: u32, max_tokens: u64) -> SetModeRequest {
        SetModeRequest {
            mode: Mode::Agent {
                until,
                max_slates,
                max_tokens,
            },
            by: "ops".to_owned(),
            reason: "the criteria are met and signed".to_owned(),
        }
    }

    /// The criteria with rollback bounds.
    fn bounded(failures: u32, reversals: u32) -> String {
        format!(
            "{CRITERIA}rollback:\n  max_consecutive_agent_failures: {failures}\n  \
             max_reversed_promotions: {reversals}\n"
        )
    }

    /// A state with `criteria` registered by `ops` and signed off by `qa`.
    fn signed(criteria: &str) -> RolloutState {
        let mut state = RolloutState::default();
        apply_register(&mut state, &registration(criteria, "first"), NOW).expect("registered");
        apply_signoff(&mut state, &signoff(met(criteria), "qa"), NOW + 1).expect("signed");
        state
    }

    /// A state in `agent` mode under `criteria`, and the epoch a slate is claimed in.
    fn in_agent(criteria: &str) -> (RolloutState, u32) {
        let mut state = signed(criteria);
        apply_set_mode(&mut state, &agent(NOW + 600, 10, 1_000), NOW + 2).expect("agent");
        let epoch = state.epoch();
        (state, epoch)
    }

    #[test]
    fn a_sign_off_is_recorded_and_leaves_the_mode_the_counters_and_the_history_alone() {
        let mut state = RolloutState::default();
        apply_register(&mut state, &registration(CRITERIA, "first"), NOW).expect("registered");
        apply_set_mode(&mut state, &shadow(NOW + 600, 5, 1_000), NOW + 1).expect("shadow");
        assert!(granted(&apply_claim(&mut state, NOW + 2, SlateKind::Point)));
        apply_tokens(&mut state, 40);
        let before = state.clone();

        apply_signoff(&mut state, &signoff(met(CRITERIA), " qa "), NOW + 3).expect("signed");
        assert_eq!(
            state.signoffs,
            vec![Signoff {
                criteria: Criteria::digest_of(CRITERIA),
                report: SIGNED_REPORT.to_owned(),
                verdict: met(CRITERIA),
                version: signed_version(),
                by: "qa".to_owned(),
                reason: "the evidence holds".to_owned(),
                at: NOW + 3,
            }]
        );
        assert_eq!(state.mode, before.mode);
        assert_eq!(state.history, before.history);
        assert_eq!(
            (state.slates_started, state.tokens_used, state.stopped),
            (1, 40, None)
        );
        assert_eq!(state.epoch(), before.epoch(), "a run in flight goes on");

        // Every sign-off is kept, in order.
        apply_signoff(&mut state, &signoff(met(CRITERIA), "audit"), NOW + 4).expect("again");
        let signers: Vec<&str> = state.signoffs.iter().map(|s| s.by.as_str()).collect();
        assert_eq!(signers, ["qa", "audit"]);
    }

    #[test]
    fn a_refused_sign_off_changes_nothing_and_says_why() {
        let mut state = RolloutState::default();
        assert!(
            apply_signoff(&mut state, &signoff(met(CRITERIA), "qa"), NOW)
                .expect_err("nothing registered")
                .reason
                .contains("no promotion criteria")
        );
        apply_register(&mut state, &registration(CRITERIA, "first"), NOW).expect("registered");
        let before = state.clone();

        let mut nobody = signoff(met(CRITERIA), " ");
        nobody.by = " ".to_owned();
        let mut no_reason = signoff(met(CRITERIA), "qa");
        no_reason.reason = String::new();
        let other = CRITERIA.replace("min_compared: 4", "min_compared: 9");
        let mut unmet = met(CRITERIA);
        unmet.met = false;
        unmet.checks.push(crate::promotion::Check {
            name: "shadow: compared records".to_owned(),
            required: "at least 4".to_owned(),
            observed: "1".to_owned(),
            held: Held::Failed,
        });
        unmet.checks.push(crate::promotion::Check {
            name: "one version: engine, model and skills".to_owned(),
            required: "the same".to_owned(),
            observed: "no evidence".to_owned(),
            held: Held::NoEvidence,
        });
        let mut no_version = met(CRITERIA);
        no_version.eval_version = None;
        let digests = [Criteria::digest_of(&other), Criteria::digest_of(CRITERIA)];
        for (request, why) in [
            (nobody, vec!["who signs"]),
            (no_reason, vec!["a reason"]),
            (
                signoff(met(&other), "qa"),
                digests.iter().map(String::as_str).collect(),
            ),
            (
                signoff(unmet, "qa"),
                vec![
                    "not met",
                    "shadow: compared records",
                    "one version: engine, model and skills",
                ],
            ),
            (signoff(no_version, "qa"), vec!["no eval version"]),
            (
                signoff(met(CRITERIA), "ops"),
                vec!["another operator has to sign"],
            ),
        ] {
            let refusal = apply_signoff(&mut state, &request, NOW + 1).expect_err(why[0]);
            assert_eq!(refusal.code, 400);
            for part in why {
                assert!(refusal.reason.contains(part), "{part}: {refusal}");
            }
            assert_eq!(state, before);
        }
    }

    #[test]
    fn stored_state_reads_and_writes_as_before_without_sign_offs() {
        let stored = serde_json::json!({
            "mode": "shadow", "until": NOW + 60, "max_slates": 1, "max_tokens": 10,
            "slates_started": 1, "tokens_used": 7, "stopped": null,
            "history": [{
                "mode": "shadow", "until": NOW + 60, "max_slates": 1, "max_tokens": 10,
                "by": "ops", "reason": "collect evidence", "at": NOW
            }]
        });
        let state: RolloutState = serde_json::from_value(stored.clone()).expect("an older state");
        assert!(state.signoffs.is_empty());
        assert_eq!((state.agent_failures, state.reversed_promotions), (0, 0));
        assert_eq!(state.history[0].signoff, None);
        assert_eq!(serde_json::to_value(&state).expect("json"), stored);

        let json = serde_json::to_value(signed(CRITERIA)).expect("json");
        assert_eq!(json["signoffs"][0]["by"], "qa");
        assert_eq!(json["signoffs"][0]["verdict"]["met"], true);
        assert_eq!(json["signoffs"][0]["version"]["model"], "m/1");
        assert_eq!(
            serde_json::from_value::<RolloutState>(json).expect("back"),
            signed(CRITERIA)
        );
    }

    // --- the agent mode ---

    #[test]
    fn the_agent_mode_is_refused_without_a_sign_off_for_the_active_criteria() {
        let mut state = RolloutState::default();
        let enter = agent(NOW + 600, 10, 1_000);
        // Nothing registered, then registered and unsigned.
        assert!(apply_set_mode(&mut state, &enter, NOW).is_err());
        apply_register(&mut state, &registration(CRITERIA, "first"), NOW).expect("registered");
        let before = state.clone();
        let refusal = apply_set_mode(&mut state, &enter, NOW + 1).expect_err("unsigned");
        assert_eq!(refusal.code, 400);
        assert!(refusal.reason.contains("sign-off"), "{refusal}");
        assert_eq!(state, before);
    }

    #[test]
    fn a_sign_off_for_earlier_criteria_does_not_open_the_agent_mode() {
        let mut state = signed(CRITERIA);
        let other = CRITERIA.replace("min_compared: 4", "min_compared: 9");
        apply_register(&mut state, &registration(&other, "stricter"), NOW + 2).expect("other");
        let before = state.clone();
        let refusal = apply_set_mode(&mut state, &agent(NOW + 600, 10, 1_000), NOW + 3)
            .expect_err("signed for earlier criteria");
        assert!(
            refusal.reason.contains(&Criteria::digest_of(&other))
                && refusal.reason.contains(&Criteria::digest_of(CRITERIA)),
            "{refusal}"
        );
        assert_eq!(state, before);
        assert_eq!(state.mode, Mode::Legacy);
    }

    #[test]
    fn a_valid_agent_entry_names_its_sign_off_and_its_criteria() {
        let mut state = signed(CRITERIA);
        apply_set_mode(&mut state, &agent(NOW + 600, 10, 1_000), NOW + 2).expect("agent");
        assert_eq!(
            state.mode,
            Mode::Agent {
                until: NOW + 600,
                max_slates: 10,
                max_tokens: 1_000
            }
        );
        let entry = state.history.last().expect("the switch");
        assert_eq!(entry.signoff, Some(NOW + 1));
        assert_eq!(
            entry.criteria.as_deref(),
            Some(Criteria::digest_of(CRITERIA).as_str())
        );
        assert_eq!((entry.by.as_str(), entry.at), ("ops", NOW + 2));
        assert_eq!(state.entered_under().map(|s| s.by.as_str()), Some("qa"));
        let json = serde_json::to_value(&state).expect("json");
        assert_eq!(json["mode"], "agent");
        assert_eq!(json["history"][0]["signoff"], NOW + 1);
        // A sign-off enters nothing by itself, and a shadow entry names none.
        let mut shadowed = signed(CRITERIA);
        assert_eq!(shadowed.mode, Mode::Legacy);
        apply_set_mode(&mut shadowed, &shadow(NOW + 600, 5, 1_000), NOW + 2).expect("shadow");
        assert_eq!(shadowed.history[0].signoff, None);
        assert!(shadowed.entered_under().is_none());
    }

    #[test]
    fn the_agent_modes_bounds_are_refused_as_for_shadow() {
        let mut state = signed(CRITERIA);
        let before = state.clone();
        let mut nobody = agent(NOW + 600, 1, 1);
        nobody.by = String::new();
        for request in [
            nobody,
            agent(NOW, 1, 1),
            agent(NOW - 1, 1, 1),
            agent(NOW + 600, 0, 1),
            agent(NOW + 600, 1, 0),
        ] {
            let refusal = apply_set_mode(&mut state, &request, NOW).expect_err("refused");
            assert_eq!(refusal.code, 400, "{refusal}");
            assert_eq!(state, before);
        }
    }

    #[test]
    fn a_claim_in_agent_mode_is_authoritative_and_bounded_as_in_shadow() {
        let (mut state, epoch) = in_agent(CRITERIA);
        assert_eq!(
            apply_claim(&mut state, NOW + 3, SlateKind::Point),
            Claim::Granted {
                epoch,
                slate: 1,
                authoritative: true
            }
        );
        assert!(state.allows(epoch));
        // In shadow the same claim is not authoritative.
        let mut shadowed = RolloutState::default();
        apply_set_mode(&mut shadowed, &shadow(NOW + 600, 5, 1_000), NOW).expect("shadow");
        assert!(matches!(
            apply_claim(&mut shadowed, NOW + 1, SlateKind::Point),
            Claim::Granted {
                authoritative: false,
                ..
            }
        ));

        // A spent token budget, a spent slate budget and a closed window refuse; the mode stays.
        apply_tokens(&mut state, 1_000);
        assert!(refused(
            &apply_claim(&mut state, NOW + 4, SlateKind::Point),
            "budget_exhausted"
        ));
        assert_eq!(state.stopped, Some(Stopped::BudgetExhausted));
        assert!(matches!(state.mode, Mode::Agent { .. }));

        let mut slates = signed(CRITERIA);
        apply_set_mode(&mut slates, &agent(NOW + 600, 1, 1_000), NOW + 2).expect("agent");
        assert!(granted(&apply_claim(
            &mut slates,
            NOW + 3,
            SlateKind::Point
        )));
        assert!(refused(
            &apply_claim(&mut slates, NOW + 4, SlateKind::Point),
            "budget_exhausted"
        ));

        let (mut window, _) = in_agent(CRITERIA);
        assert!(refused(
            &apply_claim(&mut window, NOW + 600, SlateKind::Point),
            "window_closed"
        ));
        assert_eq!(window.stopped, Some(Stopped::WindowClosed));
        assert!(matches!(window.mode, Mode::Agent { .. }));

        // Setting legacy is the kill switch for agent mode too.
        apply_set_mode(&mut state, &legacy("kill switch"), NOW + 9).expect("legacy");
        assert!(!state.allows(epoch));
        assert!(refused(
            &apply_claim(&mut state, NOW + 10, SlateKind::Point),
            "legacy"
        ));
    }

    #[test]
    fn a_stored_granted_claim_without_the_field_is_not_authoritative() {
        let stored = serde_json::json!({ "claim": "granted", "epoch": 1, "slate": 1 });
        assert_eq!(
            serde_json::from_value::<Claim>(stored).expect("an older claim"),
            Claim::Granted {
                epoch: 1,
                slate: 1,
                authoritative: false
            }
        );
        assert_eq!(
            serde_json::to_value(Claim::Granted {
                epoch: 1,
                slate: 1,
                authoritative: true
            })
            .expect("json")["authoritative"],
            true
        );
    }

    // --- run reports and triggers ---

    fn ran(epoch: u32, version: Version) -> RunReport {
        RunReport {
            epoch,
            tokens: 100,
            completed: true,
            version,
        }
    }

    fn failed(epoch: u32) -> RunReport {
        RunReport {
            epoch,
            tokens: 10,
            completed: false,
            version: version("", &[]),
        }
    }

    /// The last history entry, when a trigger made it: its name and what it observed.
    fn rolled_back(state: &RolloutState) -> Option<(&str, &str)> {
        let last = state.history.last()?;
        let trigger = last.by.strip_prefix("trigger:")?;
        assert_eq!(last.mode, Mode::Legacy);
        assert_eq!(state.mode, Mode::Legacy);
        Some((trigger, last.reason.as_str()))
    }

    #[test]
    fn a_run_of_the_signed_version_proceeds_and_its_tokens_count() {
        let (mut state, epoch) = in_agent(CRITERIA);
        // The run's profile loaded one of the two signed skills: no difference.
        let run = ran(epoch, version("m/1", &[("propose-plan", "v1")]));
        assert_eq!(apply_run(&mut state, &run, NOW + 3), RunVerdict::Proceed);
        assert_eq!(state.tokens_used, 100);
        assert!(matches!(state.mode, Mode::Agent { .. }));
        assert_eq!(state.epoch(), epoch);
        // Nor is a skill only the run names.
        let run = ran(epoch, version("m/1", &[("another-skill", "v7")]));
        assert_eq!(apply_run(&mut state, &run, NOW + 4), RunVerdict::Proceed);
        assert!(rolled_back(&state).is_none());
    }

    #[test]
    fn a_report_of_an_earlier_epoch_or_outside_agent_mode_is_stale() {
        let (mut state, epoch) = in_agent(CRITERIA);
        let before = state.history.clone();
        assert_eq!(
            apply_run(&mut state, &ran(epoch - 1, signed_version()), NOW + 3),
            RunVerdict::Stale
        );
        assert_eq!(state.history, before);
        assert_eq!(state.tokens_used, 100, "the tokens still count");
        // Even a report that would roll back changes nothing when it is stale.
        assert_eq!(
            apply_run(&mut state, &ran(epoch - 1, version("m/2", &[])), NOW + 3),
            RunVerdict::Stale
        );
        assert!(matches!(state.mode, Mode::Agent { .. }));

        let mut shadowed = RolloutState::default();
        apply_set_mode(&mut shadowed, &shadow(NOW + 600, 5, 1_000), NOW).expect("shadow");
        assert_eq!(
            apply_run(&mut shadowed, &ran(1, signed_version()), NOW + 1),
            RunVerdict::Stale
        );
    }

    #[test]
    fn a_changed_skill_model_or_engine_rolls_back_naming_the_version_change() {
        let mut engine = signed_version();
        engine.engine = "0.2.0".to_owned();
        for (changed, part) in [
            (
                version("m/1", &[("propose-plan", "v2")]),
                "skill propose-plan: v1, v2",
            ),
            (version("m/2", &[("propose-plan", "v1")]), "model: m/1, m/2"),
            (engine, "engine: 0.1.0, 0.2.0"),
        ] {
            let (mut state, epoch) = in_agent(CRITERIA);
            let verdict = apply_run(&mut state, &ran(epoch, changed), NOW + 3);
            let RunVerdict::RolledBack { reason } = verdict else {
                panic!("{part}: {verdict:?}");
            };
            assert!(reason.contains(part), "{reason}");
            let (trigger, observed) = rolled_back(&state).expect("a trigger's switch");
            assert_eq!(trigger, "version_changed");
            assert_eq!(observed, reason);
            let last = state.history.last().expect("the switch");
            assert_eq!(last.at, NOW + 3);
            assert_eq!(state.epoch(), epoch + 1, "a new epoch");
            assert!(!state.allows(epoch), "runs in flight stop");
        }
    }

    #[test]
    fn failures_in_a_row_roll_back_over_the_bound_and_a_completed_run_resets_the_count() {
        let criteria = bounded(2, 0);
        let (mut state, epoch) = in_agent(&criteria);
        // Two fail, one completes, two more fail: never three in a row.
        for _ in 0..2 {
            assert_eq!(
                apply_run(&mut state, &failed(epoch), NOW + 3),
                RunVerdict::Fallback
            );
        }
        assert_eq!(state.agent_failures, 2);
        assert_eq!(
            apply_run(&mut state, &ran(epoch, signed_version()), NOW + 4),
            RunVerdict::Proceed
        );
        assert_eq!(state.agent_failures, 0);
        for _ in 0..2 {
            assert_eq!(
                apply_run(&mut state, &failed(epoch), NOW + 5),
                RunVerdict::Fallback
            );
        }
        assert!(matches!(state.mode, Mode::Agent { .. }), "at the bound");
        assert!(rolled_back(&state).is_none());

        // The third in a row is over it.
        assert_eq!(
            apply_run(&mut state, &failed(epoch), NOW + 6),
            RunVerdict::Fallback
        );
        let (trigger, observed) = rolled_back(&state).expect("a trigger's switch");
        assert_eq!(trigger, "agent_failures");
        assert_eq!(
            observed,
            "3 agent runs failed in a row, the criteria allow 2"
        );
        assert_eq!(state.agent_failures, 0, "a switch resets the counters");
    }

    #[test]
    fn criteria_without_a_bound_trigger_neither_rollback() {
        let (mut state, epoch) = in_agent(CRITERIA);
        for _ in 0..50 {
            assert_eq!(
                apply_run(&mut state, &failed(epoch), NOW + 3),
                RunVerdict::Fallback
            );
            apply_reversal(&mut state, epoch, NOW + 3);
        }
        assert!(matches!(state.mode, Mode::Agent { .. }));
        assert_eq!((state.agent_failures, state.reversed_promotions), (50, 50));
        assert_eq!(state.epoch(), epoch);
    }

    #[test]
    fn reversed_promotions_roll_back_over_the_bound_and_only_the_current_epoch_counts() {
        // None allowed: the first reversal rolls back.
        let (mut state, epoch) = in_agent(&bounded(2, 0));
        apply_reversal(&mut state, epoch, NOW + 3);
        let (trigger, observed) = rolled_back(&state).expect("a trigger's switch");
        assert_eq!(trigger, "promotion_reversed");
        assert_eq!(
            observed,
            "1 promotion(s) of an agent slate were reversed, the criteria allow 0"
        );

        // One allowed: at the bound nothing happens, over it the mode rolls back.
        let (mut state, epoch) = in_agent(&bounded(2, 1));
        apply_reversal(&mut state, epoch - 1, NOW + 3);
        assert_eq!(state.reversed_promotions, 0, "an earlier epoch is ignored");
        apply_reversal(&mut state, epoch, NOW + 3);
        assert!(matches!(state.mode, Mode::Agent { .. }), "at the bound");
        apply_reversal(&mut state, epoch, NOW + 4);
        assert_eq!(
            rolled_back(&state).map(|(trigger, _)| trigger),
            Some("promotion_reversed")
        );
        // After the rollback a late reversal of that epoch is ignored too.
        let history = state.history.len();
        apply_reversal(&mut state, epoch, NOW + 5);
        assert_eq!(state.history.len(), history);

        // Outside agent mode nothing is counted.
        let mut shadowed = RolloutState::default();
        apply_set_mode(&mut shadowed, &shadow(NOW + 600, 5, 1_000), NOW).expect("shadow");
        apply_reversal(&mut shadowed, 1, NOW + 1);
        assert_eq!(shadowed.reversed_promotions, 0);
    }

    #[test]
    fn registering_other_criteria_rolls_back_from_agent_and_the_same_ones_do_not() {
        let (mut state, epoch) = in_agent(CRITERIA);
        apply_register(&mut state, &registration(CRITERIA, "again"), NOW + 3).expect("again");
        assert!(matches!(state.mode, Mode::Agent { .. }));
        assert_eq!(state.epoch(), epoch);

        let other = CRITERIA.replace("min_compared: 4", "min_compared: 9");
        apply_register(&mut state, &registration(&other, "stricter"), NOW + 4).expect("other");
        assert_eq!(state.registrations.len(), 2, "the registration is recorded");
        let (trigger, observed) = rolled_back(&state).expect("a trigger's switch");
        assert_eq!(trigger, "criteria_changed");
        assert!(
            observed.contains(&Criteria::digest_of(&other))
                && observed.contains(&Criteria::digest_of(CRITERIA)),
            "{observed}"
        );
        assert_eq!(state.history.last().map(|entry| entry.at), Some(NOW + 4));
    }

    #[test]
    fn a_rollback_does_not_undo_itself_and_a_late_report_of_the_old_epoch_is_stale() {
        let (mut state, epoch) = in_agent(&bounded(0, 0));
        assert_eq!(
            apply_run(&mut state, &failed(epoch), NOW + 3),
            RunVerdict::Fallback
        );
        assert_eq!(
            rolled_back(&state).map(|(trigger, _)| trigger),
            Some("agent_failures")
        );
        // The condition clears: a run of the old epoch completes with the signed version.
        assert_eq!(
            apply_run(&mut state, &ran(epoch, signed_version()), NOW + 4),
            RunVerdict::Stale
        );
        assert_eq!(state.mode, Mode::Legacy);
        assert!(refused(
            &apply_claim(&mut state, NOW + 5, SlateKind::Point),
            "legacy"
        ));
        // Only a new switch enters the mode again, under the same condition as the first.
        apply_set_mode(&mut state, &agent(NOW + 600, 10, 1_000), NOW + 6).expect("agent again");
        assert_eq!(state.epoch(), epoch + 2);
        let modes: Vec<&str> = state.history.iter().map(|e| e.mode.name()).collect();
        assert_eq!(modes, ["agent", "legacy", "agent"]);
    }

    // --- joint slates ---

    /// The criteria in the format that states joint bounds, with rollback bounds.
    fn joint_criteria(failures: u32, reversals: u32) -> String {
        format!(
            "{}joint:\n  eval: {{ min_passed: 2 }}\n  shadow: {{ min_compared: 2 }}\n\
             rollback:\n  max_consecutive_agent_failures: {failures}\n  \
             max_reversed_promotions: {reversals}\n",
            CRITERIA.replace("version: 1\n", "version: 2\n")
        )
    }

    fn authoritative(claim: &Claim) -> bool {
        matches!(
            claim,
            Claim::Granted {
                authoritative: true,
                ..
            }
        )
    }

    #[test]
    fn a_joint_claim_in_shadow_is_granted_as_a_recorded_only_run() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 600, 5, 1_000), NOW).expect("shadow");
        let claim = apply_claim(&mut state, NOW + 1, SlateKind::Joint);
        assert!(granted(&claim) && !authoritative(&claim), "{claim:?}");
        assert!(!state.covers_joint());
        // In legacy no joint slate is claimed either.
        let mut legacy = RolloutState::default();
        let claim = apply_claim(&mut legacy, NOW, SlateKind::Joint);
        assert!(refused(&claim, "legacy"));
    }

    #[test]
    fn a_joint_claim_in_agent_mode_is_authoritative_only_under_criteria_with_joint_bounds() {
        // Entered under criteria without joint bounds, as every sign-off given so far.
        let (mut state, _) = in_agent(&bounded(2, 0));
        assert!(!state.covers_joint());
        let joint = apply_claim(&mut state, NOW + 3, SlateKind::Joint);
        assert!(granted(&joint) && !authoritative(&joint), "{joint:?}");
        let point = apply_claim(&mut state, NOW + 3, SlateKind::Point);
        assert!(
            authoritative(&point),
            "a point's slate is the point's as before"
        );

        // Entered under criteria that state joint bounds.
        let (mut state, epoch) = in_agent(&joint_criteria(2, 0));
        assert!(state.covers_joint());
        let joint = apply_claim(&mut state, NOW + 3, SlateKind::Joint);
        assert_eq!(
            joint,
            Claim::Granted {
                epoch,
                slate: 1,
                authoritative: true
            }
        );
    }

    #[test]
    fn criteria_registered_after_the_sign_off_do_not_lend_their_joint_bounds() {
        // The mode is entered under the earlier criteria; joint criteria registered in shadow
        // afterwards have no sign-off, so nothing is covered.
        let mut state = signed(&bounded(2, 0));
        apply_register(
            &mut state,
            &registration(&joint_criteria(2, 0), "joint bounds"),
            NOW + 2,
        )
        .expect("registered");
        assert!(!state.covers_joint(), "not in agent mode");
        let refusal = apply_set_mode(&mut state, &agent(NOW + 600, 10, 1_000), NOW + 3)
            .expect_err("the sign-off is for the earlier criteria");
        assert_eq!(refusal.code, 400);
        assert!(!state.covers_joint());
    }

    #[test]
    fn joint_and_point_slates_share_the_slate_and_token_budgets() {
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 600, 2, 1_000), NOW).expect("shadow");
        assert!(granted(&apply_claim(&mut state, NOW + 1, SlateKind::Point)));
        assert!(granted(&apply_claim(&mut state, NOW + 1, SlateKind::Joint)));
        assert_eq!(state.slates_started, 2);
        // A second portfolio: the maximum of two has started.
        let third = apply_claim(&mut state, NOW + 2, SlateKind::Joint);
        assert!(refused(&third, "budget_exhausted"), "{third:?}");
        assert_eq!(state.stopped, Some(Stopped::BudgetExhausted));

        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 600, 9, 1_000), NOW).expect("shadow");
        assert!(granted(&apply_claim(&mut state, NOW + 1, SlateKind::Joint)));
        apply_tokens(&mut state, 1_000);
        for kind in [SlateKind::Point, SlateKind::Joint] {
            assert!(refused(
                &apply_claim(&mut state, NOW + 2, kind),
                "budget_exhausted"
            ));
        }
        // And the window.
        let mut state = RolloutState::default();
        apply_set_mode(&mut state, &shadow(NOW + 600, 9, 1_000), NOW).expect("shadow");
        let late = apply_claim(&mut state, NOW + 600, SlateKind::Joint);
        assert!(refused(&late, "window_closed"));
    }

    #[test]
    fn a_joint_run_counts_for_the_failures_and_the_version_like_a_points() {
        // A joint failure adds to the row: two point runs failed, the joint one is the third.
        let (mut state, epoch) = in_agent(&joint_criteria(2, 0));
        assert!(granted(&apply_claim(&mut state, NOW + 3, SlateKind::Joint)));
        for _ in 0..3 {
            assert_eq!(
                apply_run(&mut state, &failed(epoch), NOW + 4),
                RunVerdict::Fallback
            );
        }
        let (trigger, observed) = rolled_back(&state).expect("a trigger's switch");
        assert_eq!(trigger, "agent_failures");
        assert!(
            observed.starts_with("3 agent runs failed in a row"),
            "{observed}"
        );

        // A completed joint run ends the row.
        let (mut state, epoch) = in_agent(&joint_criteria(2, 0));
        apply_run(&mut state, &failed(epoch), NOW + 4);
        apply_run(&mut state, &failed(epoch), NOW + 4);
        assert_eq!(
            apply_run(&mut state, &ran(epoch, signed_version()), NOW + 5),
            RunVerdict::Proceed
        );
        assert_eq!(state.agent_failures, 0);

        // A joint run of another skill version rolls back before its slate is shown. This
        // holds for a recorded-only joint run too: it reports under criteria without joint
        // bounds as well.
        for criteria in [joint_criteria(2, 0), bounded(2, 0)] {
            let (mut state, epoch) = in_agent(&criteria);
            let other = version("m/1", &[("propose-plan", "v2")]);
            let verdict = apply_run(&mut state, &ran(epoch, other), NOW + 4);
            assert!(
                matches!(verdict, RunVerdict::RolledBack { .. }),
                "{verdict:?}"
            );
            assert_eq!(
                rolled_back(&state).map(|(trigger, _)| trigger),
                Some("version_changed")
            );
        }
    }

    #[test]
    fn an_undone_agent_joint_plan_rolls_back_and_one_claimed_before_the_mode_does_not() {
        let (mut state, epoch) = in_agent(&joint_criteria(2, 0));
        let claim = apply_claim(&mut state, NOW + 3, SlateKind::Joint);
        assert!(authoritative(&claim));
        apply_reversal(&mut state, epoch, NOW + 9);
        let (trigger, observed) = rolled_back(&state).expect("a trigger's switch");
        assert_eq!(trigger, "promotion_reversed");
        assert!(
            observed.starts_with("1 promotion(s) of an agent slate"),
            "{observed}"
        );

        // A joint record of an earlier epoch (a shadow window's) is not an agent slate's.
        let (mut state, epoch) = in_agent(&joint_criteria(2, 0));
        apply_reversal(&mut state, epoch - 1, NOW + 9);
        assert!(matches!(state.mode, Mode::Agent { .. }));
        assert_eq!(state.reversed_promotions, 0);
    }
}
