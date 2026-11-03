//! The reasoner seam: one tool-using agent task in, one run report out.
//!
//! A [`Reasoner`] runs a task an engine-minted token scopes: it reads the world model and the
//! pinned skills only through the engine's gateway with that token, and hands its plan in with
//! `submit_plan` or its finding with `submit_finding`. Its [`RunReport`] is telemetry only: how
//! the run ended, its steps, its tokens and the model that served it. It never carries what was
//! handed in; the engine takes that from the gateway's own run ledger
//! ([`crate::tools::RunLedger`]), so a reasoner (or a prompt injection inside one) cannot claim a
//! plan it never submitted.
//!
//! [`RuntimeReasoner`] is the served runtime's implementation: it posts the task to the
//! runtime's `agent` pipeline under its own concurrency limit and a total timeout, and maps a
//! failed call to the neutral failure classes the slate loop traces (`RateLimited`,
//! `Unavailable`). Capability-neutral: the task text is the capability's, the seam names
//! none.

use std::fmt::{self, Write as _};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ekg_domain::{EngineError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::telemetry::trace_headers;

/// The served runtime's agent pipeline (`POST {runtime}/agent/run`).
pub const RUNTIME_AGENT_PIPELINE: &str = "agent";

/// The default step limit of one agent run.
pub const DEFAULT_MAX_AGENT_STEPS: u32 = 12;

/// The default step limit of a group's planner: it reads nothing, so it has only to load its
/// skill, draft and correct what `submit_plan` rejects.
pub const DEFAULT_MAX_JOINT_AGENT_STEPS: u32 = 8;

/// The default cap on a group's planner run.
pub const DEFAULT_JOINT_RUN_CAP: Duration = Duration::from_secs(120);

/// The default step limit of an analyst: it reads one member and hands in one finding.
pub const DEFAULT_MAX_ANALYST_STEPS: u32 = 10;

/// The default cap on an analyst run: it reads, so it takes longer than a planner.
pub const DEFAULT_ANALYST_RUN_CAP: Duration = Duration::from_secs(240);

/// One agent task: the capability's task text, the token scoping it, its trace id and its step
/// limit. The token is a credential: it is never printed.
#[derive(Clone)]
pub struct ReasoningTask {
    /// What the agent is asked to do (the subject, its drift, the plans already tried and the
    /// skills to load).
    pub task: String,
    /// The engine-minted task token the runtime carries to the gateway as its bearer.
    pub token: String,
    /// The task's W3C trace id (32 hex digits), the token's `trace_id`.
    pub trace_id: String,
    /// The id of the span the run is traced by (16 hex digits): the parent the runtime's spans
    /// continue.
    pub parent_span_id: String,
    /// The most agent steps the run may take.
    pub max_steps: u32,
    /// How long the run may take, once it holds its permit.
    pub run_cap: Duration,
    /// The permit the caller took with [`Reasoner::admit`], if it took one.
    pub permit: Option<Arc<OwnedSemaphorePermit>>,
}

impl fmt::Debug for ReasoningTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReasoningTask")
            .field("trace_id", &self.trace_id)
            .field("max_steps", &self.max_steps)
            .field("run_cap", &self.run_cap)
            .finish_non_exhaustive()
    }
}

/// How an agent run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    /// `submit_plan` accepted a plan, or `submit_finding` a finding, and the run ended on it.
    Submitted,
    /// The run reached its step limit.
    StepLimit,
    /// The model stopped without an accepted plan.
    NoPlan,
    /// The provider cut the model's answer off for length: not a decision to stop.
    Length,
    /// The run failed (a tool, the toolset or the model).
    Error,
}

impl fmt::Display for Termination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Submitted => "submitted",
            Self::StepLimit => "step_limit",
            Self::NoPlan => "no_plan",
            Self::Length => "length",
            Self::Error => "error",
        })
    }
}

/// What a reasoner reports about one run: telemetry, never the plan. The counts are the
/// runtime's own and informational.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// How the run ended.
    pub termination: Termination,
    /// The agent steps it took.
    pub steps: u32,
    /// The model's input tokens, summed over the run.
    pub input_tokens: u64,
    /// The model's output tokens, summed over the run.
    pub output_tokens: u64,
    /// The model that served the run.
    pub model: String,
    /// What the agent said last, when the run ended without a plan. Model text: for reading a
    /// decline afterwards, never for deciding anything.
    #[serde(default)]
    pub closing_answer: Option<String>,
}

/// The most characters of a closing answer the engine keeps, whatever the runtime sent.
const CLOSING_ANSWER_CHARS: usize = 2000;

/// Runs one agent task. Read-only: a reasoner proposes through `submit_plan` and commits
/// nothing.
#[async_trait]
pub trait Reasoner: Send + Sync {
    /// Run `task` and report how it went.
    ///
    /// # Errors
    /// [`EngineError::RateLimited`] when the model refused for load; [`EngineError::Unavailable`]
    /// when the run could not be carried out (a timeout, a transport or server failure, an
    /// unreadable report).
    async fn reason(&self, task: &ReasoningTask) -> Result<RunReport>;

    /// Wait for room to run, so that a task token is minted only once its run can start: a
    /// token minted before the wait would spend its lifetime queueing. Hand the permit to the
    /// run in [`ReasoningTask::permit`]. A reasoner without a limit has none.
    ///
    /// # Errors
    /// [`EngineError::Unavailable`] when no permit can be had.
    async fn admit(&self) -> Result<Option<Arc<OwnedSemaphorePermit>>> {
        Ok(None)
    }
}

/// The served runtime's [`Reasoner`]: `POST {runtime}/agent/run` with `{task, max_steps}`, the
/// task token as `Authorization: Bearer` and the task's `traceparent`, under its own permit pool
/// and the task's run cap. The token is never in the body: the runtime logs request bodies.
/// Cheap to clone: clones share the client and the pool.
#[derive(Clone)]
pub struct RuntimeReasoner {
    http: reqwest::Client,
    endpoint: String,
    permits: Arc<Semaphore>,
}

impl fmt::Debug for RuntimeReasoner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeReasoner")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl RuntimeReasoner {
    /// A reasoner over the runtime at `runtime_url` (e.g. `http://hayhooks:1416`), running at
    /// most `max_concurrent` tasks at once, each bounded by its own run cap once it holds its
    /// permit.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the HTTP transport cannot be built.
    pub fn new(runtime_url: &str, max_concurrent: NonZeroUsize) -> Result<Self> {
        // As the provider: the transport's rustls has no bundled crypto provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| EngineError::Backend(format!("reasoner http client: {e}")))?;
        Ok(Self {
            http,
            endpoint: format!(
                "{}/{RUNTIME_AGENT_PIPELINE}/run",
                runtime_url.trim_end_matches('/')
            ),
            permits: Arc::new(Semaphore::new(max_concurrent.get())),
        })
    }

    /// One call, without the permit or the cap.
    async fn call(&self, task: &ReasoningTask) -> Result<RunReport> {
        let body = serde_json::to_vec(&json!({
            "task": task.task,
            "max_steps": task.max_steps,
        }))
        .map_err(|e| EngineError::Backend(format!("reasoner: request: {e}")))?;
        let response = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", task.token),
            )
            .headers(trace_headers(&task.trace_id, &task.parent_span_id)?)
            .body(body)
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("reasoner: {}", e.without_url())))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| EngineError::Unavailable(format!("reasoner: reply: {e}")))?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(EngineError::RateLimited(format!(
                "reasoner: the runtime answered {status}"
            )));
        }
        if !status.is_success() {
            return Err(EngineError::Unavailable(format!(
                "reasoner: the runtime answered {status}"
            )));
        }
        parse_report(&bytes)
    }
}

#[async_trait]
impl Reasoner for RuntimeReasoner {
    async fn reason(&self, task: &ReasoningTask) -> Result<RunReport> {
        let _permit = match &task.permit {
            Some(_) => None,
            None => self.admit().await?,
        };
        let total = task.run_cap;
        tokio::time::timeout(total, self.call(task))
            .await
            .map_err(|_| {
                EngineError::Unavailable(format!(
                    "reasoner: no run report within {total:?} (trace {})",
                    task.trace_id
                ))
            })?
            .map_err(|err| name_trace(err, &task.trace_id))
    }

    async fn admit(&self) -> Result<Option<Arc<OwnedSemaphorePermit>>> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|e| EngineError::Unavailable(format!("reasoner: permits closed: {e}")))?;
        Ok(Some(Arc::new(permit)))
    }
}

/// Read the runtime's reply: Hayhooks wraps the pipeline's answer as `{"result": …}`. Only the
/// telemetry fields are read; anything else the reply carries is ignored.
fn parse_report(bytes: &[u8]) -> Result<RunReport> {
    let reply: Value = serde_json::from_slice(bytes)
        .map_err(|e| EngineError::Unavailable(format!("reasoner: unreadable reply: {e}")))?;
    let result = reply.get("result").unwrap_or(&reply).clone();
    let mut report: RunReport = serde_json::from_value(result)
        .map_err(|e| EngineError::Unavailable(format!("reasoner: unreadable run report: {e}")))?;
    report.closing_answer = report
        .closing_answer
        .filter(|_| report.termination == Termination::NoPlan)
        .map(|answer| {
            answer
                .trim()
                .chars()
                .take(CLOSING_ANSWER_CHARS)
                .collect::<String>()
        })
        .filter(|answer| !answer.is_empty());
    Ok(report)
}

/// Name the trace id in a failure, keeping its class.
fn name_trace(err: EngineError, trace_id: &str) -> EngineError {
    match err {
        EngineError::RateLimited(why) => {
            EngineError::RateLimited(format!("{why} (trace {trace_id})"))
        }
        EngineError::Unavailable(why) => {
            EngineError::Unavailable(format!("{why} (trace {trace_id})"))
        }
        other => other,
    }
}

/// `bytes` random bytes as lower-case hex, never all zero (a valid W3C id).
///
/// # Errors
/// [`EngineError::Backend`] when the OS random source fails.
pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0_u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| EngineError::Backend(format!("random id: {e}")))?;
    if let Some(last) = buf.last_mut() {
        *last |= 1;
    }
    Ok(buf.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    use super::*;

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT: &str = "00f067aa0ba902b7";

    /// What the fake runtime saw and how it answers.
    #[derive(Clone)]
    struct Fake {
        status: StatusCode,
        reply: Value,
        delay: Duration,
        seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Fake {
        fn answering(status: StatusCode, reply: Value) -> Self {
            Self {
                status,
                reply,
                delay: Duration::ZERO,
                seen: Arc::default(),
                in_flight: Arc::default(),
                peak: Arc::default(),
            }
        }
    }

    async fn run(
        State(fake): State<Fake>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, String) {
        let now = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        fake.peak.fetch_max(now, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        fake.seen.lock().expect("seen").push((headers, body));
        tokio::time::sleep(fake.delay).await;
        fake.in_flight.fetch_sub(1, Ordering::SeqCst);
        (fake.status, fake.reply.to_string())
    }

    /// Serve `fake` at `/agent/run` on an ephemeral port; the runtime's base URL.
    async fn serve(fake: Fake) -> String {
        let router = Router::new()
            .route("/agent/run", post(run))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        format!("http://127.0.0.1:{port}/")
    }

    fn task() -> ReasoningTask {
        capped(Duration::from_secs(5))
    }

    fn capped(run_cap: Duration) -> ReasoningTask {
        ReasoningTask {
            task: "Plan for the order.".to_owned(),
            token: "v1.claims.mac".to_owned(),
            trace_id: TRACE.to_owned(),
            parent_span_id: PARENT.to_owned(),
            max_steps: 7,
            run_cap,
            permit: None,
        }
    }

    fn reasoner(url: &str, max: usize) -> RuntimeReasoner {
        RuntimeReasoner::new(url, NonZeroUsize::new(max).expect("max")).expect("reasoner")
    }

    fn report() -> Value {
        json!({ "result": {
            "termination": "submitted", "steps": 5, "input_tokens": 1200,
            "output_tokens": 300, "model": "openai/gpt-oss-120b",
            // Anything else the reply carries is ignored: the plan comes from the ledger.
            "actions": [{ "action_type": "cancel_order" }]
        }})
    }

    #[tokio::test]
    async fn the_request_carries_the_task_and_steps_with_the_token_as_its_bearer() {
        let fake = Fake::answering(StatusCode::OK, report());
        let seen = Arc::clone(&fake.seen);
        let url = serve(fake).await;
        let report = reasoner(&url, 2).reason(&task()).await.expect("report");
        assert_eq!(
            report,
            RunReport {
                termination: Termination::Submitted,
                steps: 5,
                input_tokens: 1200,
                output_tokens: 300,
                model: "openai/gpt-oss-120b".to_owned(),
                closing_answer: None,
            }
        );
        let seen = seen.lock().expect("seen");
        let (headers, body) = &seen[0];
        assert_eq!(
            body,
            &json!({ "task": "Plan for the order.", "max_steps": 7 }),
            "the token is never in the body"
        );
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer v1.claims.mac")
        );
        let traceparent = headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .expect("traceparent");
        let parts: Vec<&str> = traceparent.split('-').collect();
        assert_eq!(parts.len(), 4, "{traceparent}");
        assert_eq!(
            (parts[0], parts[1], parts[2], parts[3]),
            ("00", TRACE, PARENT, "01"),
            "the task's trace under the span its run is traced by"
        );
    }

    #[test]
    fn each_termination_parses_and_a_bare_report_is_read_too() {
        for (wire, termination) in [
            ("submitted", Termination::Submitted),
            ("step_limit", Termination::StepLimit),
            ("no_plan", Termination::NoPlan),
            ("length", Termination::Length),
            ("error", Termination::Error),
        ] {
            let bare = json!({
                "termination": wire, "steps": 1, "input_tokens": 0,
                "output_tokens": 0, "model": "m"
            });
            let report = parse_report(bare.to_string().as_bytes()).expect("report");
            assert_eq!(report.termination, termination);
            assert_eq!(termination.to_string(), wire);
        }
        let err = parse_report(br#"{"result": {"termination": "won"}}"#).expect_err("bad");
        assert!(matches!(err, EngineError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn a_closing_answer_is_kept_for_a_run_without_a_plan_only_and_cut_to_its_bound() {
        let answer = |termination: &str, text: &str| {
            let reply = json!({
                "termination": termination, "steps": 1, "input_tokens": 0,
                "output_tokens": 0, "model": "m", "closing_answer": text
            });
            parse_report(reply.to_string().as_bytes())
                .expect("report")
                .closing_answer
        };
        assert_eq!(
            answer("no_plan", "  Nothing on order helps.\n").as_deref(),
            Some("Nothing on order helps.")
        );
        let long = "é".repeat(CLOSING_ANSWER_CHARS + 50);
        assert_eq!(
            answer("no_plan", &long).map(|kept| kept.chars().count()),
            Some(CLOSING_ANSWER_CHARS),
            "cut by characters, not bytes"
        );
        assert_eq!(answer("no_plan", "   "), None, "an empty answer is none");
        for termination in ["submitted", "step_limit", "error"] {
            assert_eq!(answer(termination, "a stray text"), None, "{termination}");
        }
    }

    #[tokio::test]
    async fn a_429_is_rate_limited_and_a_5xx_is_unavailable() {
        let limited = serve(Fake::answering(
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "detail": "slow down" }),
        ))
        .await;
        let err = reasoner(&limited, 1)
            .reason(&task())
            .await
            .expect_err("429");
        assert!(
            matches!(&err, EngineError::RateLimited(why) if why.contains(TRACE)),
            "{err:?}"
        );
        let down = serve(Fake::answering(
            StatusCode::BAD_GATEWAY,
            json!({ "detail": "upstream" }),
        ))
        .await;
        let err = reasoner(&down, 1).reason(&task()).await.expect_err("502");
        assert!(matches!(err, EngineError::Unavailable(_)), "{err:?}");
        // Nothing listens on port 1: a transport failure is unavailable too.
        let err = reasoner("http://127.0.0.1:1", 1)
            .reason(&task())
            .await
            .expect_err("unreachable");
        assert!(matches!(err, EngineError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_run_past_its_cap_is_unavailable() {
        let mut fake = Fake::answering(StatusCode::OK, report());
        fake.delay = Duration::from_secs(5);
        let url = serve(fake).await;
        let err = reasoner(&url, 1)
            .reason(&capped(Duration::from_millis(200)))
            .await
            .expect_err("timeout");
        assert!(
            matches!(&err, EngineError::Unavailable(why) if why.contains("no run report within 200ms")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn each_run_is_bounded_by_its_own_cap() {
        let mut fake = Fake::answering(StatusCode::OK, report());
        fake.delay = Duration::from_millis(600);
        let url = serve(fake).await;
        let reasoner = reasoner(&url, 2);
        let (short, long) = (
            capped(Duration::from_millis(200)),
            capped(Duration::from_secs(5)),
        );
        let (short, long) = tokio::join!(reasoner.reason(&short), reasoner.reason(&long));
        let err = short.expect_err("the short cap");
        assert!(
            matches!(&err, EngineError::Unavailable(why) if why.contains("within 200ms")),
            "{err:?}"
        );
        assert_eq!(long.expect("inside the long cap").steps, 5);
    }

    #[tokio::test]
    async fn runs_beyond_the_concurrency_limit_queue() {
        let mut fake = Fake::answering(StatusCode::OK, report());
        fake.delay = Duration::from_millis(100);
        let peak = Arc::clone(&fake.peak);
        let url = serve(fake).await;
        let reasoner = reasoner(&url, 1);
        let task = task();
        let runs = (0..3).map(|_| reasoner.reason(&task));
        for result in futures::future::join_all(runs).await {
            result.expect("report");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "one run at a time");
    }

    #[tokio::test]
    async fn a_run_that_was_admitted_takes_no_second_permit() {
        let url = serve(Fake::answering(StatusCode::OK, report())).await;
        let reasoner = reasoner(&url, 1);
        let mut admitted = task();
        admitted.permit = reasoner.admit().await.expect("a permit");
        assert!(admitted.permit.is_some());
        tokio::time::timeout(Duration::from_secs(5), reasoner.reason(&admitted))
            .await
            .expect("not waiting on its own permit")
            .expect("report");
    }

    #[test]
    fn a_task_never_prints_its_token() {
        let printed = format!("{:?}", task());
        assert!(!printed.contains("v1.claims.mac"), "{printed}");
        assert!(printed.contains(TRACE));
    }
}
