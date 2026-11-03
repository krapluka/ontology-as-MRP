//! The provider-neutral [`CognitionProvider`] driver over an OpenAI-compatible
//! endpoint (OpenRouter by default).
//!
//! It takes a prompt or text and returns text or vectors, no domain types, so the
//! model stays out of the truth path and any provider is swappable by configuration.
//! The embedding width is pinned so vector projections stay consistent.
//!
//! It reaches the provider by one of two [`CognitionRoute`]s: through the served
//! cognition runtime (Hayhooks, the default: generation streams from its
//! OpenAI-compatible chat endpoint, embedding posts to its `embed` pipeline, and the
//! engine holds no provider credential), or directly. Both routes share every bound
//! below, so callers behave the same on either.
//!
//! The HTTP client is `async-openai` pointed at the configured API base. Requests and
//! replies stay plain JSON (its `byot` calls), so any OpenAI-compatible server works.
//! The bounds are configured in [`CognitionLimits`]:
//!
//! - a 429, a 5xx or a connection error at the start of a call is retried with
//!   exponential backoff, honouring `Retry-After`, by the client's `OpenAIRetryLayer`
//!   (a permanent `insufficient_quota` 429 is not retried). Retries happen here only:
//!   the runtime passes the provider's status through instead of retrying itself;
//! - a call that never starts answering fails at the header timeout;
//! - generation is streamed: a stream that stalls between chunks fails at the idle
//!   timeout, one that never ends at the total cap, while a slow but live answer is not
//!   cut off. A stream that ends without a finish marker was cut off mid-answer and
//!   fails instead of returning a partial answer;
//! - a process-wide permit pool, held across the whole streamed call (retries
//!   included), bounds the generations in flight, so a burst of callers queues instead
//!   of tripping the endpoint's rate limit. Clones of one provider share its client and
//!   its pool.
//!
//! Every call carries a W3C `traceparent` (the same on each of its attempts), so the
//! runtime's telemetry for the call joins the engine's trace. Every failure carries its
//! class on the neutral error and names the trace id: a retry-exhausted 429 is
//! [`EngineError::RateLimited`]; a quota refusal, 5xx, timeout, transport failure,
//! truncated or empty answer is [`EngineError::Unavailable`].

use std::fmt::Write as _;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::error::{ApiErrorResponse, OpenAIError, WrappedError};
use async_openai::middleware::HttpRequestFactory;
use async_openai::middleware::retry::OpenAIRetryLayer;
use async_trait::async_trait;
use ekg_domain::{CognitionProvider, EngineError, Result};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tower::ServiceExt as _;
use tower::util::BoxCloneSyncService;

/// Default bound on retries of a rate-limited / failed call start.
pub const DEFAULT_MAX_RETRIES: usize = 3;
/// Default bound on waiting for a call to start answering (its response headers).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound on the gap between two chunks of a streamed answer.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default cap on one whole call.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Default bound on generations in flight at once, process-wide.
pub const DEFAULT_MAX_CONCURRENT: NonZeroUsize = NonZeroUsize::MIN.saturating_add(1);

/// The resilience bounds of the boundary, all configurable. The defaults work against a
/// shared public endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CognitionLimits {
    /// Retries of a call start that met a 429, a 5xx or a connection error.
    pub max_retries: usize,
    /// How long a call may take to start answering (until its response headers).
    pub connect_timeout: Duration,
    /// How long a streamed answer may go without a chunk.
    pub idle_timeout: Duration,
    /// The cap on one whole call, once it holds its permit.
    pub total_timeout: Duration,
    /// How many generations may be in flight at once, process-wide.
    pub max_concurrent: NonZeroUsize,
}

impl Default for CognitionLimits {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            total_timeout: DEFAULT_TOTAL_TIMEOUT,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }
}

/// The served runtime's generation pipeline, which is the `model` its OpenAI-compatible
/// chat endpoint routes on.
pub const RUNTIME_GENERATE_PIPELINE: &str = "generate";

/// How the boundary reaches the provider. Switching needs no code change and every
/// caller behaves the same on either route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CognitionRoute {
    /// Through the served cognition runtime at `url` (e.g. `http://localhost:1416`):
    /// generation streams from its OpenAI-compatible chat endpoint with the
    /// [`RUNTIME_GENERATE_PIPELINE`] as the model, embedding posts to its `embed`
    /// pipeline. The runtime holds the provider credential and chooses the models, so
    /// [`CognitionConfig::api_key`] and the model names are not used.
    Runtime {
        /// Base URL of the runtime.
        url: String,
    },
    /// Straight to the OpenAI-compatible endpoint at [`CognitionConfig::base_url`].
    Direct,
}

/// Configuration for the OpenAI-compatible boundary. No vendor is hardcoded; the
/// route, endpoint and model names come from config.
#[derive(Debug, Clone)]
pub struct CognitionConfig {
    /// How the provider is reached.
    pub route: CognitionRoute,
    /// Base URL of the OpenAI-compatible API on the direct route (e.g.
    /// `https://openrouter.ai/api/v1`).
    pub base_url: String,
    /// Optional bearer token on the direct route; omitted for a local unauthenticated
    /// endpoint. Never sent on the runtime route.
    pub api_key: Option<String>,
    /// Chat/completions model name on the direct route.
    pub chat_model: String,
    /// Embeddings model name on the direct route.
    pub embed_model: String,
    /// Pinned embedding width; a response of a different width is rejected.
    pub embed_dim: usize,
    /// The timeout, retry and concurrency bounds.
    pub limits: CognitionLimits,
}

/// The HTTP stack every call runs through: retry, header timeout, error-body
/// normalization, then the transport.
type HttpStack = BoxCloneSyncService<HttpRequestFactory, reqwest::Response, OpenAIError>;

/// A [`CognitionProvider`] over the OpenAI-compatible endpoint. Cheap to clone: every
/// clone shares one client and one generation permit pool, so build it once per
/// process and hand clones out.
#[derive(Clone)]
pub struct OpenAiCompatCognition {
    client: Client<OpenAIConfig>,
    /// The same stack the client runs on, for the runtime's (non-OpenAI) embed call.
    stack: HttpStack,
    http: reqwest::Client,
    config: Arc<CognitionConfig>,
    permits: Arc<Semaphore>,
}

impl OpenAiCompatCognition {
    /// Build the provider (its client, retry and timeout stack, and permit pool) from
    /// its configuration.
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the HTTP transport cannot be built.
    pub fn new(config: CognitionConfig) -> Result<Self> {
        // The transport's rustls has no bundled crypto provider: install the ring one
        // as the process default (one another component installed first is kept).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(config.limits.connect_timeout)
            .build()
            .map_err(|e| EngineError::Backend(format!("cognition http client: {e}")))?;
        // On the runtime route the engine holds no provider credential; on the direct
        // route an unset key sends no bearer at all.
        let (api_base, api_key) = match &config.route {
            CognitionRoute::Runtime { url } => (format!("{}/v1", url.trim_end_matches('/')), None),
            CognitionRoute::Direct => (
                config.base_url.trim_end_matches('/').to_owned(),
                config.api_key.clone().filter(|key| !key.is_empty()),
            ),
        };
        // Retry outermost, so each attempt gets its own header timeout; an error body is
        // normalized below the retry layer so it can classify a 429.
        let stack: HttpStack = BoxCloneSyncService::new(
            tower::ServiceBuilder::new()
                .layer(OpenAIRetryLayer::new(config.limits.max_retries))
                .map_err(unbox)
                .timeout(config.limits.connect_timeout)
                .and_then(normalize_error)
                .service(Transport {
                    http: http.clone(),
                    bearer: api_key.is_some(),
                }),
        );
        // Every setting explicit, so no ambient OPENAI_* variable leaks in.
        let endpoint = OpenAIConfig::new()
            .with_api_base(api_base)
            .with_api_key(api_key.unwrap_or_default())
            .with_org_id("")
            .with_project_id("");
        Ok(Self {
            client: Client::with_config(endpoint).with_http_service(stack.clone()),
            stack,
            http,
            permits: Arc::new(Semaphore::new(config.limits.max_concurrent.get())),
            config: Arc::new(config),
        })
    }

    /// The model the chat request names: the runtime's pipeline, or the direct model.
    fn chat_model(&self) -> &str {
        match self.config.route {
            CognitionRoute::Runtime { .. } => RUNTIME_GENERATE_PIPELINE,
            CognitionRoute::Direct => &self.config.chat_model,
        }
    }

    /// Stream one chat answer and assemble its content, failing a stall at the idle
    /// timeout and a stream that ends without a finish marker.
    async fn stream_answer(&self, prompt: &str) -> Result<String> {
        let idle = self.config.limits.idle_timeout;
        let mut stream = self
            .client
            .chat()
            .create_stream_byot(chat_body(self.chat_model(), prompt))
            .await
            .map_err(|e| classify(&e))?;
        let mut answer = String::new();
        let mut finished = false;
        loop {
            let next = tokio::time::timeout(idle, stream.next())
                .await
                .map_err(|_| {
                    EngineError::Unavailable(format!("cognition: the answer stalled for {idle:?}"))
                })?;
            match next {
                None => break,
                Some(Ok(chunk)) => finished |= absorb_chunk(&chunk, &mut answer)?,
                Some(Err(err)) => return Err(classify(&err)),
            }
        }
        // A stream cut off mid-answer is a failure of the call, not a partial answer.
        if !finished {
            return Err(EngineError::Unavailable(
                "cognition: the answer ended without a finish marker".to_owned(),
            ));
        }
        // An empty answer is a failure of the call, not a refusal of its content.
        if answer.trim().is_empty() {
            return Err(EngineError::Unavailable(
                "cognition: the answer was empty".to_owned(),
            ));
        }
        Ok(answer)
    }

    /// One generation, under a permit and the total cap.
    async fn bounded_generate(&self, prompt: &str) -> Result<String> {
        // The permit spans the whole streamed call, retries included; the total cap
        // starts once it is held (queueing is the caller's budget, not the call's).
        let _permit = self.permits.acquire().await.map_err(|e| {
            EngineError::Unavailable(format!("cognition: generation permits closed: {e}"))
        })?;
        let total = self.config.limits.total_timeout;
        tokio::time::timeout(total, self.stream_answer(prompt))
            .await
            .map_err(|_| {
                EngineError::Unavailable(format!("cognition: no whole answer within {total:?}"))
            })?
    }

    /// One embedding, under the total cap, on the configured route.
    async fn bounded_embed(&self, text: &str) -> Result<Vec<f32>> {
        let total = self.config.limits.total_timeout;
        let call = async {
            match &self.config.route {
                CognitionRoute::Runtime { url } => {
                    let response = self.embed_via_runtime(url, text).await?;
                    parse_runtime_embedding(&response, self.config.embed_dim)
                }
                CognitionRoute::Direct => {
                    let response: Value = self
                        .client
                        .embeddings()
                        .create_byot(embed_body(&self.config.embed_model, text))
                        .await
                        .map_err(|e| classify(&e))?;
                    parse_embedding(&response, self.config.embed_dim)
                }
            }
        };
        tokio::time::timeout(total, call).await.map_err(|_| {
            EngineError::Unavailable(format!("cognition: no embedding within {total:?}"))
        })?
    }

    /// Post one text to the runtime's `embed` pipeline over the shared stack (so the
    /// same retry, timeout and classification apply) and return its JSON reply.
    async fn embed_via_runtime(&self, url: &str, text: &str) -> Result<Value> {
        let endpoint = format!("{}/embed/run", url.trim_end_matches('/'));
        let body = serde_json::to_vec(&json!({ "texts": [text] }))
            .map_err(|e| EngineError::Backend(format!("cognition: embed request: {e}")))?;
        let http = self.http.clone();
        let factory = HttpRequestFactory::new(move || {
            let request = http
                .post(&endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .build()
                .map_err(OpenAIError::Reqwest);
            async move { request }
        });
        let response = self
            .stack
            .clone()
            .oneshot(factory)
            .await
            .map_err(|e| classify(&e))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| classify(&OpenAIError::Reqwest(e)))?;
        if !status.is_success() {
            let api_error = serde_json::from_slice::<WrappedError>(&openai_error_body(&bytes))
                .map_err(|e| EngineError::Unavailable(format!("cognition: embed {status}: {e}")))?
                .error;
            return Err(classify(&OpenAIError::ApiError(ApiErrorResponse {
                status_code: status,
                api_error,
            })));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| EngineError::Backend(format!("cognition: embed reply: {e}")))
    }
}

#[async_trait]
impl CognitionProvider for OpenAiCompatCognition {
    async fn generate(&self, prompt: &str) -> Result<String> {
        let trace = TraceContext::mint()?;
        TRACEPARENT
            .scope(trace.header(), self.bounded_generate(prompt))
            .await
            .map_err(|err| trace.name_in(err))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let trace = TraceContext::mint()?;
        TRACEPARENT
            .scope(trace.header(), self.bounded_embed(text))
            .await
            .map_err(|err| trace.name_in(err))
    }
}

// --- trace context ---

tokio::task_local! {
    /// The `traceparent` of the call in progress; the transport stamps it on every
    /// attempt, so a retried call keeps one trace.
    static TRACEPARENT: String;
}

/// A W3C trace context minted for one call (`00-<trace id>-<span id>-01`, sampled).
struct TraceContext {
    trace_id: String,
    span_id: String,
}

impl TraceContext {
    /// Mint a random, valid (non-zero) trace and span id.
    fn mint() -> Result<Self> {
        let mut bytes = [0_u8; 24];
        getrandom::fill(&mut bytes)
            .map_err(|e| EngineError::Backend(format!("cognition: trace id: {e}")))?;
        // An all-zero id is invalid; force a bit rather than retry.
        bytes[15] |= 1;
        bytes[23] |= 1;
        let hex = |part: &[u8]| {
            part.iter().fold(String::new(), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
        };
        Ok(Self {
            trace_id: hex(&bytes[..16]),
            span_id: hex(&bytes[16..]),
        })
    }

    fn header(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }

    /// Name the trace id in a failure, keeping its class.
    fn name_in(&self, err: EngineError) -> EngineError {
        let id = &self.trace_id;
        match err {
            EngineError::RateLimited(why) => {
                EngineError::RateLimited(format!("{why} (trace {id})"))
            }
            EngineError::Unavailable(why) => {
                EngineError::Unavailable(format!("{why} (trace {id})"))
            }
            EngineError::Backend(why) => EngineError::Backend(format!("{why} (trace {id})")),
            other => other,
        }
    }
}

// --- the HTTP stack's adapters ---

/// The transport at the bottom of the stack: rebuilds each attempt's request, stamps the
/// call's `traceparent` on it, drops an empty bearer, and executes it.
#[derive(Clone)]
struct Transport {
    http: reqwest::Client,
    /// Whether a bearer is configured; without one no `Authorization` header is sent.
    bearer: bool,
}

impl tower::Service<HttpRequestFactory> for Transport {
    type Response = reqwest::Response;
    type Error = OpenAIError;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<reqwest::Response, OpenAIError>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), OpenAIError>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, factory: HttpRequestFactory) -> Self::Future {
        let (http, bearer) = (self.http.clone(), self.bearer);
        // Read here, while the caller's task (and its trace scope) is polling the call.
        let traceparent = TRACEPARENT
            .try_with(|value| reqwest::header::HeaderValue::from_str(value).ok())
            .ok()
            .flatten();
        Box::pin(async move {
            let mut request = factory.build().await?;
            if !bearer {
                request.headers_mut().remove(reqwest::header::AUTHORIZATION);
            }
            if let Some(value) = traceparent {
                request.headers_mut().insert("traceparent", value);
            }
            http.execute(request).await.map_err(OpenAIError::Reqwest)
        })
    }
}

/// Recover the client's own error from the timeout layer's boxed one, so the retry
/// layer still recognizes a connection error; the timeout itself stays boxed.
fn unbox(err: tower::BoxError) -> OpenAIError {
    match err.downcast::<OpenAIError>() {
        Ok(inner) => *inner,
        Err(other) => OpenAIError::Boxed(other),
    }
}

/// The longest rate-limit reset a call waits for. A reset further out (e.g. a daily
/// request cap) is not waited for: the call backs off briefly and fails as `rate_limited`.
const MAX_RESET_WAIT_SECS: u64 = 60;

/// Rewrite an error reply into the shape the retry layer and [`classify`] read:
///
/// - its body into the OpenAI error shape: the layer tells a transient 429 from a
///   permanent `insufficient_quota` one by parsing the body, and gives up without
///   retrying when it cannot (e.g. a numeric `code`, as some OpenAI-compatible servers
///   send). The runtime's FastAPI-wrapped `{"detail": {"error": …}}` is unwrapped, so the
///   provider's own `type` survives the extra hop;
/// - on a 429, a rate-limit reset (`X-RateLimit-Reset`) into the `Retry-After` the layer
///   honours, when the endpoint sent no `Retry-After` and the reset is within
///   [`MAX_RESET_WAIT_SECS`].
///
/// Successful responses pass through untouched.
async fn normalize_error(
    response: reqwest::Response,
) -> std::result::Result<reqwest::Response, OpenAIError> {
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return Ok(response);
    }
    let mut headers = response.headers().clone();
    headers.remove(reqwest::header::CONTENT_LENGTH);
    headers.remove(reqwest::header::TRANSFER_ENCODING);
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        && !headers.contains_key(reqwest::header::RETRY_AFTER)
    {
        let wait = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|reset| retry_after_from_reset(reset, std::time::SystemTime::now()));
        if let Some(secs) = wait {
            headers.insert(reqwest::header::RETRY_AFTER, secs.into());
        }
    }
    let body = openai_error_body(&response.bytes().await?);
    let mut rebuilt = http::Response::new(reqwest::Body::from(body));
    *rebuilt.status_mut() = status;
    *rebuilt.headers_mut() = headers;
    Ok(reqwest::Response::from(rebuilt))
}

/// Seconds to wait until a rate-limit reset: an epoch timestamp in milliseconds or
/// seconds, or a delay in seconds. `None` when unreadable or past
/// [`MAX_RESET_WAIT_SECS`]; at least one second otherwise.
fn retry_after_from_reset(reset: &str, now: std::time::SystemTime) -> Option<u64> {
    let value: u64 = reset.trim().parse().ok()?;
    let now_ms = u64::try_from(now.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()).ok()?;
    let wait_ms = if value >= 1_000_000_000_000 {
        value.saturating_sub(now_ms)
    } else if value >= 1_000_000_000 {
        value.saturating_mul(1000).saturating_sub(now_ms)
    } else {
        value.saturating_mul(1000)
    };
    let secs = wait_ms.div_ceil(1000).max(1);
    (secs <= MAX_RESET_WAIT_SECS).then_some(secs)
}

/// An error body in the OpenAI shape (`{"error":{"message":…}}` with string `type`,
/// `param` and `code`), built from whatever the endpoint sent, including the runtime's
/// FastAPI-wrapped `{"detail": {"error": …}}` or `{"detail": "…"}`.
fn openai_error_body(raw: &[u8]) -> Vec<u8> {
    let raw_text = || String::from_utf8_lossy(raw).into_owned();
    let parsed = serde_json::from_slice::<Value>(raw).ok();
    let mut error = parsed
        .as_ref()
        .and_then(|value| {
            value
                .get("error")
                .or_else(|| value.pointer("/detail/error"))
                .filter(|e| e.is_object())
                .cloned()
                .or_else(|| {
                    let detail = value.get("detail").and_then(Value::as_str)?;
                    Some(json!({ "message": detail }))
                })
        })
        .unwrap_or_else(|| json!({ "message": raw_text() }));
    if let Some(fields) = error.as_object_mut() {
        let message = match fields.get("message") {
            Some(Value::String(message)) => message.clone(),
            Some(other) => other.to_string(),
            None => raw_text(),
        };
        fields.insert("message".to_owned(), Value::String(message));
        for key in ["type", "param", "code"] {
            if let Some(value) = fields.get(key).filter(|v| !v.is_string() && !v.is_null()) {
                let text = value.to_string();
                fields.insert(key.to_owned(), Value::String(text));
            }
        }
    }
    serde_json::to_vec(&json!({ "error": error })).unwrap_or_default()
}

/// Map a client error to its failure class on the neutral error.
fn classify(err: &OpenAIError) -> EngineError {
    match err {
        OpenAIError::ApiError(api) => {
            let status = api.status_code;
            let why = format!("cognition endpoint {status}: {}", api.api_error);
            if api.api_error.r#type.as_deref() == Some("insufficient_quota") {
                EngineError::Unavailable(why)
            } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                EngineError::RateLimited(why)
            } else if status.is_server_error() {
                EngineError::Unavailable(why)
            } else {
                EngineError::Backend(why)
            }
        }
        OpenAIError::InvalidArgument(_) => EngineError::Backend(err.to_string()),
        // A timeout, a transport or stream failure, an undecodable reply.
        _ => EngineError::Unavailable(format!("cognition: {err}")),
    }
}

// --- request and response shaping ---

/// The streamed chat/completions request body: one user turn carrying the prompt.
fn chat_body(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "stream": true,
    })
}

/// Append a streamed chunk's content to the answer, returning whether the chunk carries
/// the finish marker (a non-null `finish_reason`). A chunk carrying an `error` (an
/// endpoint failing mid-stream) fails the call with its class.
fn absorb_chunk(chunk: &Value, answer: &mut String) -> Result<bool> {
    if let Some(error) = chunk.get("error") {
        let why = format!("cognition: the answer failed mid-stream: {error}");
        let code = error.get("code");
        let rate_limited = code.and_then(Value::as_u64) == Some(429)
            || code.and_then(Value::as_str) == Some("429");
        return Err(if rate_limited {
            EngineError::RateLimited(why)
        } else {
            EngineError::Unavailable(why)
        });
    }
    if let Some(content) = chunk
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
    {
        answer.push_str(content);
    }
    Ok(chunk
        .pointer("/choices/0/finish_reason")
        .is_some_and(|reason| !reason.is_null()))
}

fn embed_body(model: &str, input: &str) -> Value {
    json!({ "model": model, "input": input })
}

/// Extract the first embedding vector and enforce the pinned width.
// Embeddings are f32; narrowing the JSON f64 is expected.
#[allow(clippy::cast_possible_truncation)]
fn parse_embedding(value: &Value, expected_dim: usize) -> Result<Vec<f32>> {
    let array = value
        .pointer("/data/0/embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| EngineError::Backend("cognition: no data[0].embedding".to_owned()))?;
    if array.len() != expected_dim {
        return Err(EngineError::Backend(format!(
            "cognition: embedding width {} != pinned {expected_dim}",
            array.len()
        )));
    }
    array
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| EngineError::Backend("cognition: non-numeric embedding".to_owned()))
        })
        .collect()
}

/// Extract the first vector of the runtime's `embed` reply (`{"result": {"vectors": …}}`)
/// and enforce the pinned width, as on the direct route.
fn parse_runtime_embedding(value: &Value, expected_dim: usize) -> Result<Vec<f32>> {
    let vector = value
        .pointer("/result/vectors/0")
        .ok_or_else(|| EngineError::Backend("cognition: no result.vectors[0]".to_owned()))?;
    parse_embedding(&json!({ "data": [{ "embedding": vector }] }), expected_dim)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn parsed(body: &str) -> Value {
        serde_json::from_str(body).expect("json")
    }

    #[test]
    fn streamed_chunks_assemble_the_answer() {
        let mut answer = String::new();
        for chunk in [
            r#"{"choices":[{"delta":{"role":"assistant","content":"expedite "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"thinking"}}]}"#,
            r#"{"choices":[{"delta":{"content":"the PO"}}]}"#,
        ] {
            assert!(!absorb_chunk(&parsed(chunk), &mut answer).expect("chunk"));
        }
        assert_eq!(answer, "expedite the PO");
        // The chunk carrying a finish reason marks the answer complete.
        let last = parsed(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);
        assert!(absorb_chunk(&last, &mut answer).expect("finish"));
        // A usage-only chunk (no choices) neither adds text nor finishes.
        let usage = parsed(r#"{"choices":[],"usage":{"total_tokens":5}}"#);
        assert!(!absorb_chunk(&usage, &mut answer).expect("usage"));
        assert_eq!(answer, "expedite the PO");
    }

    #[test]
    fn a_mid_stream_error_fails_with_its_class() {
        let mut answer = String::new();
        let limited = absorb_chunk(
            &parsed(r#"{"error":{"message":"slow down","code":429}}"#),
            &mut answer,
        );
        assert!(matches!(limited, Err(EngineError::RateLimited(_))));
        let failed = absorb_chunk(
            &parsed(r#"{"error":{"message":"provider down","code":502}}"#),
            &mut answer,
        );
        assert!(matches!(failed, Err(EngineError::Unavailable(_))));
    }

    #[test]
    fn a_rate_limit_body_is_normalized_to_the_openai_shape() {
        let read = |raw: &str| -> Value {
            serde_json::from_slice(&openai_error_body(raw.as_bytes())).expect("json")
        };
        // A numeric code (as some OpenAI-compatible servers send) becomes a string.
        let numeric = read(r#"{"error":{"message":"rate-limited upstream","code":429}}"#);
        assert_eq!(numeric["error"]["code"], "429");
        assert_eq!(numeric["error"]["message"], "rate-limited upstream");
        // A quota type is carried unchanged, so the retry layer still refuses to retry.
        let quota = read(r#"{"error":{"message":"q","type":"insufficient_quota"}}"#);
        assert_eq!(quota["error"]["type"], "insufficient_quota");
        // A plain-text body becomes the message.
        let text = read("Too Many Requests");
        assert_eq!(text["error"]["message"], "Too Many Requests");
        // Every result parses as the client's own error type.
        for raw in [r#"{"error":{"code":429}}"#, "Too Many Requests", "{}"] {
            let body = openai_error_body(raw.as_bytes());
            serde_json::from_slice::<async_openai::error::WrappedError>(&body)
                .expect("the retry layer can read it");
        }
    }

    #[test]
    fn a_rate_limit_reset_becomes_a_bounded_wait() {
        use std::time::{Duration, UNIX_EPOCH};
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        // An epoch-milliseconds reset 2.5 s ahead rounds up to 3 s.
        assert_eq!(retry_after_from_reset("1800000002500", now), Some(3));
        // An epoch-seconds reset 10 s ahead.
        assert_eq!(retry_after_from_reset("1800000010", now), Some(10));
        // A delay in seconds.
        assert_eq!(retry_after_from_reset("5", now), Some(5));
        // A reset already past waits one second, not zero.
        assert_eq!(retry_after_from_reset("1799999999000", now), Some(1));
        // A reset hours away (a daily cap) is not waited for.
        assert_eq!(retry_after_from_reset("1800007200000", now), None);
        assert_eq!(retry_after_from_reset("soon", now), None);
    }

    #[test]
    fn an_embedding_of_the_pinned_width_parses() {
        let body = parsed(r#"{"data":[{"embedding":[0.1,0.2,0.3,0.4]}]}"#);
        let v = parse_embedding(&body, 4).expect("embedding");
        assert_eq!(v.len(), 4);
        assert!((v[0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn an_embedding_of_the_wrong_width_is_rejected() {
        let body = parsed(r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#);
        // Pinned 4, got 3: an error, which keeps vector projections consistent.
        assert!(parse_embedding(&body, 4).is_err());
    }

    #[test]
    fn request_bodies_carry_the_model_input_and_stream_flag() {
        assert_eq!(chat_body("m", "hi")["model"], "m");
        assert_eq!(chat_body("m", "hi")["messages"][0]["content"], "hi");
        assert_eq!(chat_body("m", "hi")["stream"], true);
        assert_eq!(embed_body("e", "text")["input"], "text");
    }

    #[test]
    fn the_default_limits_match_the_design() {
        let limits = CognitionLimits::default();
        assert_eq!(limits.max_retries, 3);
        assert_eq!(limits.connect_timeout, Duration::from_secs(30));
        assert_eq!(limits.idle_timeout, Duration::from_secs(30));
        assert_eq!(limits.total_timeout, Duration::from_secs(120));
        assert_eq!(limits.max_concurrent.get(), 2);
    }
}
