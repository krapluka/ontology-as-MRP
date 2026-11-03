//! Hermetic tests of the cognition boundary's resilience against a local fake
//! OpenAI-compatible endpoint. They pin the `async-openai` client semantics the provider
//! relies on (the retry layer, the header timeout next to a live stream, and the permit
//! held across a whole streamed call) and each failure scenario of the provider-neutral
//! cognition boundary.
//!
//! Every scenario runs on both routes: `direct`, against a fake provider, and `runtime`,
//! against a fake that speaks the served runtime's shapes (a stream that ends with a usage
//! chunk and no `[DONE]`, errors wrapped in FastAPI's `detail`, and embeddings on the
//! `embed` pipeline).

// `expect` is the assertion mechanism here; the prose names protocol terms.
#![allow(clippy::expect_used, clippy::doc_markdown)]

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ekg_cognition::{
    CognitionConfig, CognitionLimits, CognitionProvider, CognitionRoute, OpenAiCompatCognition,
};
use ekg_domain::EngineError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The head of a streamed (SSE) answer.
const SSE_HEAD: &str =
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";

/// Which route a scenario runs on, and so which wire shapes the fake speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Direct,
    Runtime,
}

/// One scripted reply of the fake endpoint.
#[derive(Clone)]
enum Reply {
    /// A complete response: status, extra headers, body. On the runtime route an error
    /// body is wrapped the way the runtime's FastAPI layer wraps it (`{"detail": …}`).
    Status(u16, Vec<(&'static str, &'static str)>, &'static str),
    /// A streamed (SSE) answer: its content parts, sent `gap` apart, the last one
    /// carrying the finish marker; then `[DONE]` (direct) or a usage chunk (runtime).
    Stream {
        parts: Vec<&'static str>,
        gap: Duration,
    },
    /// A stream that sends its parts and then closes without a finish marker.
    Truncated { parts: Vec<&'static str> },
    /// A stream that sends one part and then nothing for `stall`.
    Stall { stall: Duration },
    /// No response headers for `delay`, then a normal streamed answer.
    SlowHeaders { delay: Duration },
}

/// A fake endpoint that plays one reply per request (the last repeats), counting the
/// requests and the most answers it was sending at once, and recording each request.
struct Fake {
    route: Route,
    addr: String,
    hits: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    /// Each request's head (lower-cased) and body, in arrival order.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

async fn fake(route: Route, script: Vec<Reply>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let hits = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);
    {
        let (hits, max_in_flight, requests) = (
            Arc::clone(&hits),
            Arc::clone(&max_in_flight),
            Arc::clone(&requests),
        );
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let n = hits.fetch_add(1, Ordering::SeqCst);
                let reply = script[n.min(script.len() - 1)].clone();
                let counters = (Arc::clone(&in_flight), Arc::clone(&max_in_flight));
                tokio::spawn(serve(socket, route, reply, counters, Arc::clone(&requests)));
            }
        });
    }
    Fake {
        route,
        addr,
        hits,
        max_in_flight,
        requests,
    }
}

/// Read one request (headers + a content-length body), record it, then play `reply`.
async fn serve(
    mut socket: TcpStream,
    route: Route,
    reply: Reply,
    (in_flight, max_in_flight): (Arc<AtomicUsize>, Arc<AtomicUsize>),
    requests: Arc<Mutex<Vec<String>>>,
) {
    let mut request = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        let Ok(read) = socket.read(&mut buf).await else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buf[..read]);
        let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
    requests
        .lock()
        .expect("requests")
        .push(String::from_utf8_lossy(&request).to_ascii_lowercase());
    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    max_in_flight.fetch_max(now, Ordering::SeqCst);
    let chunk = |content: &str, finish: bool| {
        let reason = if finish { "\"stop\"" } else { "null" };
        format!(
            "data: {{\"id\":\"x\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":{reason}}}]}}\n\n"
        )
    };
    // How a complete stream ends: `[DONE]` from a provider; a usage chunk and a plain
    // close (no `[DONE]`) from the runtime.
    let end = match route {
        Route::Direct => "data: [DONE]\n\n".to_owned(),
        Route::Runtime => "data: {\"id\":\"x\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n".to_owned(),
    };
    match reply {
        Reply::Status(status, headers, body) => {
            let body = if route == Route::Runtime && status >= 400 && body.starts_with('{') {
                format!("{{\"detail\":{body}}}")
            } else {
                body.to_owned()
            };
            let mut head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (key, value) in headers {
                let _ = write!(head, "{key}: {value}\r\n");
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }
        Reply::Stream { parts, gap } => {
            let _ = socket.write_all(SSE_HEAD.as_bytes()).await;
            let last = parts.len().saturating_sub(1);
            for (i, part) in parts.iter().enumerate() {
                tokio::time::sleep(gap).await;
                let _ = socket.write_all(chunk(part, i == last).as_bytes()).await;
            }
            // Leave the in-flight count before the end, so the client cannot have
            // finished (and started another call) while this one still counts.
            in_flight.fetch_sub(1, Ordering::SeqCst);
            let _ = socket.write_all(end.as_bytes()).await;
        }
        Reply::Truncated { parts } => {
            let _ = socket.write_all(SSE_HEAD.as_bytes()).await;
            for part in parts {
                let _ = socket.write_all(chunk(part, false).as_bytes()).await;
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }
        Reply::Stall { stall } => {
            let _ = socket.write_all(SSE_HEAD.as_bytes()).await;
            let _ = socket.write_all(chunk("partial", false).as_bytes()).await;
            tokio::time::sleep(stall).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }
        Reply::SlowHeaders { delay } => {
            tokio::time::sleep(delay).await;
            let _ = socket.write_all(SSE_HEAD.as_bytes()).await;
            let _ = socket.write_all(chunk("late", true).as_bytes()).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            let _ = socket.write_all(end.as_bytes()).await;
        }
    }
    let _ = socket.shutdown().await;
}

fn provider_with_key(
    fake: &Fake,
    limits: CognitionLimits,
    key: Option<&str>,
) -> OpenAiCompatCognition {
    let route = match fake.route {
        Route::Direct => CognitionRoute::Direct,
        Route::Runtime => CognitionRoute::Runtime {
            url: format!("http://{}", fake.addr),
        },
    };
    OpenAiCompatCognition::new(CognitionConfig {
        route,
        base_url: format!("http://{}/v1", fake.addr),
        api_key: key.map(str::to_owned),
        chat_model: "m".to_owned(),
        embed_model: "e".to_owned(),
        embed_dim: 3,
        limits,
    })
    .expect("provider")
}

fn provider(fake: &Fake, limits: CognitionLimits) -> OpenAiCompatCognition {
    provider_with_key(fake, limits, None)
}

/// Short bounds so the scenarios run in about a second each.
fn quick() -> CognitionLimits {
    CognitionLimits {
        max_retries: 2,
        connect_timeout: Duration::from_millis(500),
        idle_timeout: Duration::from_millis(400),
        total_timeout: Duration::from_secs(5),
        max_concurrent: NonZeroUsize::new(4).expect("non-zero"),
    }
}

fn answer(parts: Vec<&'static str>) -> Reply {
    Reply::Stream {
        parts,
        gap: Duration::from_millis(10),
    }
}

/// The value of header `name` in a recorded (lower-cased) request.
fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}:")))
        .map(str::trim)
}

/// Generate one test per route for a scenario `async fn(Route)`.
macro_rules! on_both_routes {
    ($($scenario:ident),* $(,)?) => {$(
        mod $scenario {
            #[tokio::test(flavor = "multi_thread")]
            async fn direct() {
                super::$scenario(super::Route::Direct).await;
            }
            #[tokio::test(flavor = "multi_thread")]
            async fn runtime() {
                super::$scenario(super::Route::Runtime).await;
            }
        }
    )*};
}

on_both_routes!(
    a_rate_limited_call_is_retried_after_retry_after_and_succeeds,
    a_rate_limit_reset_without_retry_after_is_waited_for,
    a_call_that_stays_rate_limited_fails_as_rate_limited,
    a_quota_refusal_is_not_retried_and_fails_as_unavailable,
    a_server_error_is_retried,
    a_slow_but_live_answer_is_not_cut_off_by_the_header_or_idle_timeout,
    a_stalled_stream_fails_as_unavailable_at_the_idle_bound,
    a_call_that_never_starts_answering_fails_at_the_header_timeout,
    a_stream_that_never_ends_fails_at_the_total_cap,
    an_empty_answer_fails_as_unavailable,
    a_truncated_answer_is_not_accepted,
    a_burst_queues_within_the_bound_and_the_permit_spans_the_stream,
    every_call_carries_one_traceparent_across_its_retries,
    a_failed_call_names_its_trace_id,
    embeddings_keep_the_pinned_width,
    a_rate_limited_embedding_is_retried,
    a_failing_embedding_fails_as_unavailable,
);

/// A 429 in the shape OpenRouter documents: a numeric `code` and a `metadata` object.
const RATE_LIMITED: &str = r#"{"error":{"code":429,"message":"Rate limit exceeded","metadata":{"error_type":"rate_limit_exceeded"}}}"#;

async fn a_rate_limited_call_is_retried_after_retry_after_and_succeeds(route: Route) {
    let fake = fake(
        route,
        vec![
            Reply::Status(429, vec![("retry-after", "1")], RATE_LIMITED),
            answer(vec!["expedite ", "the PO"]),
        ],
    )
    .await;
    let started = Instant::now();
    let reply = provider(&fake, quick())
        .generate("p")
        .await
        .expect("the retry returns the answer");
    assert_eq!(reply, "expedite the PO");
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2, "one retry");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After honoured"
    );
}

async fn a_rate_limit_reset_without_retry_after_is_waited_for(route: Route) {
    // A platform limit that names only its reset (epoch ms, as OpenRouter sends it). The
    // library alone would back off ~100 ms; the reset is honoured instead.
    let reset_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        + 1_500;
    let reset: &'static str = Box::leak(reset_ms.to_string().into_boxed_str());
    let fake = fake(
        route,
        vec![
            Reply::Status(429, vec![("x-ratelimit-reset", reset)], RATE_LIMITED),
            answer(vec!["ok"]),
        ],
    )
    .await;
    let started = Instant::now();
    let reply = provider(&fake, quick())
        .generate("p")
        .await
        .expect("retried after the reset");
    assert_eq!(reply, "ok");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the reset is waited for: {:?}",
        started.elapsed()
    );
}

async fn a_call_that_stays_rate_limited_fails_as_rate_limited(route: Route) {
    let fake = fake(
        route,
        vec![Reply::Status(429, vec![("retry-after", "0")], RATE_LIMITED)],
    )
    .await;
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("rate limited throughout");
    assert!(matches!(err, EngineError::RateLimited(_)), "{err:?}");
    assert_eq!(
        fake.hits.load(Ordering::SeqCst),
        3,
        "the first try + 2 retries"
    );
}

async fn a_quota_refusal_is_not_retried_and_fails_as_unavailable(route: Route) {
    // On the runtime route the body arrives wrapped in `detail`; the provider's own
    // `insufficient_quota` type must survive the hop, or the call would be retried.
    let fake = fake(
        route,
        vec![Reply::Status(
            429,
            vec![],
            r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        )],
    )
    .await;
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("quota");
    assert!(matches!(err, EngineError::Unavailable(_)), "{err:?}");
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1, "not retried");
}

async fn a_server_error_is_retried(route: Route) {
    let fake = fake(
        route,
        vec![
            Reply::Status(503, vec![("retry-after", "0")], "upstream down"),
            answer(vec!["ok"]),
        ],
    )
    .await;
    let reply = provider(&fake, quick())
        .generate("p")
        .await
        .expect("retried");
    assert_eq!(reply, "ok");
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
}

async fn a_slow_but_live_answer_is_not_cut_off_by_the_header_or_idle_timeout(route: Route) {
    // Six parts 250 ms apart: 1.5 s in total, over both the 500 ms header timeout and
    // the 400 ms idle bound, but never a gap over the idle bound.
    let fake = fake(
        route,
        vec![Reply::Stream {
            parts: vec!["a", "b", "c", "d", "e", "f"],
            gap: Duration::from_millis(250),
        }],
    )
    .await;
    let reply = provider(&fake, quick())
        .generate("p")
        .await
        .expect("the whole answer");
    assert_eq!(reply, "abcdef");
}

async fn a_stalled_stream_fails_as_unavailable_at_the_idle_bound(route: Route) {
    let fake = fake(
        route,
        vec![Reply::Stall {
            stall: Duration::from_secs(10),
        }],
    )
    .await;
    let started = Instant::now();
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("stalled");
    assert!(
        matches!(err, EngineError::Unavailable(ref why) if why.contains("stalled")),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3), "fails fast");
}

async fn a_call_that_never_starts_answering_fails_at_the_header_timeout(route: Route) {
    let fake = fake(
        route,
        vec![Reply::SlowHeaders {
            delay: Duration::from_secs(10),
        }],
    )
    .await;
    let started = Instant::now();
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("no headers");
    assert!(matches!(err, EngineError::Unavailable(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "fails fast");
}

async fn a_stream_that_never_ends_fails_at_the_total_cap(route: Route) {
    let fake = fake(
        route,
        vec![Reply::Stream {
            parts: vec!["x"; 40],
            gap: Duration::from_millis(100),
        }],
    )
    .await;
    let limits = CognitionLimits {
        total_timeout: Duration::from_millis(800),
        ..quick()
    };
    let err = provider(&fake, limits)
        .generate("p")
        .await
        .expect_err("over the cap");
    assert!(
        matches!(err, EngineError::Unavailable(ref why) if why.contains("whole answer")),
        "{err:?}"
    );
}

async fn an_empty_answer_fails_as_unavailable(route: Route) {
    let fake = fake(route, vec![answer(vec![""])]).await;
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("empty");
    assert!(
        matches!(err, EngineError::Unavailable(ref why) if why.contains("empty")),
        "{err:?}"
    );
}

async fn a_truncated_answer_is_not_accepted(route: Route) {
    let fake = fake(
        route,
        vec![Reply::Truncated {
            parts: vec!["expedite ", "the"],
        }],
    )
    .await;
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("cut off mid-answer");
    assert!(
        matches!(err, EngineError::Unavailable(ref why) if why.contains("finish marker")),
        "{err:?}"
    );
}

async fn a_burst_queues_within_the_bound_and_the_permit_spans_the_stream(route: Route) {
    // Five callers, a bound of two, every answer streamed over ~300 ms. Were the permit
    // released at the response headers, more than two answers would stream at once.
    let fake = fake(
        route,
        vec![Reply::Stream {
            parts: vec!["a", "b", "c"],
            gap: Duration::from_millis(100),
        }],
    )
    .await;
    let limits = CognitionLimits {
        max_concurrent: NonZeroUsize::new(2).expect("non-zero"),
        ..quick()
    };
    let shared = provider(&fake, limits);
    let calls: Vec<_> = (0..5)
        .map(|_| {
            // Clones share one client and one permit pool.
            let clone = shared.clone();
            tokio::spawn(async move { clone.generate("p").await })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.expect("join").expect("answer"), "abc");
    }
    assert_eq!(fake.hits.load(Ordering::SeqCst), 5, "every call completes");
    assert_eq!(
        fake.max_in_flight.load(Ordering::SeqCst),
        2,
        "at most the bound streamed at once"
    );
}

/// Whether `value` is a well-formed, sampled W3C `traceparent` with non-zero ids.
fn is_traceparent(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
    parts.len() == 4
        && parts[0] == "00"
        && hex(parts[1], 32)
        && hex(parts[2], 16)
        && parts[3] == "01"
        && parts[1].bytes().any(|b| b != b'0')
        && parts[2].bytes().any(|b| b != b'0')
}

async fn every_call_carries_one_traceparent_across_its_retries(route: Route) {
    let fake = fake(
        route,
        vec![
            Reply::Status(429, vec![("retry-after", "0")], RATE_LIMITED),
            answer(vec!["ok"]),
            answer(vec!["again"]),
        ],
    )
    .await;
    let cognition = provider(&fake, quick());
    cognition.generate("p").await.expect("retried call");
    cognition.generate("p").await.expect("second call");
    let requests = fake.requests();
    let traces: Vec<&str> = requests
        .iter()
        .map(|r| header(r, "traceparent").expect("every attempt carries a traceparent"))
        .collect();
    assert!(traces.iter().all(|t| is_traceparent(t)), "{traces:?}");
    assert_eq!(traces[0], traces[1], "a retry keeps the call's trace");
    assert_ne!(traces[1], traces[2], "a new call mints a new trace");
}

async fn a_failed_call_names_its_trace_id(route: Route) {
    let fake = fake(
        route,
        vec![Reply::Status(429, vec![("retry-after", "0")], RATE_LIMITED)],
    )
    .await;
    let err = provider(&fake, quick())
        .generate("p")
        .await
        .expect_err("rate limited throughout");
    let sent = fake.requests();
    let trace_id = header(&sent[0], "traceparent")
        .and_then(|t| t.split('-').nth(1))
        .expect("trace id");
    assert!(
        matches!(err, EngineError::RateLimited(ref why) if why.contains(&format!("(trace {trace_id})"))),
        "{err:?} should name trace {trace_id}"
    );
}

/// An embedding reply of `width` floats in the route's shape.
fn embedding(route: Route, width: usize) -> &'static str {
    let vector = vec!["0.1"; width].join(",");
    let body = match route {
        Route::Direct => format!(r#"{{"data":[{{"embedding":[{vector}]}}]}}"#),
        Route::Runtime => {
            format!(r#"{{"result":{{"vectors":[[{vector}]],"dimension":{width}}}}}"#)
        }
    };
    Box::leak(body.into_boxed_str())
}

async fn embeddings_keep_the_pinned_width(route: Route) {
    let fake = fake(
        route,
        vec![
            Reply::Status(200, vec![], embedding(route, 3)),
            Reply::Status(200, vec![], embedding(route, 2)),
        ],
    )
    .await;
    let cognition = provider(&fake, quick());
    assert_eq!(cognition.embed("t").await.expect("embedding").len(), 3);
    assert!(
        cognition.embed("t").await.is_err(),
        "a width other than the pinned 3"
    );
    let path = |r: &str| r.lines().next().unwrap_or_default().to_owned();
    let expected = match route {
        Route::Direct => "post /v1/embeddings http/1.1",
        Route::Runtime => "post /embed/run http/1.1",
    };
    assert_eq!(path(&fake.requests()[0]), expected);
}

async fn a_rate_limited_embedding_is_retried(route: Route) {
    let fake = fake(
        route,
        vec![
            Reply::Status(429, vec![("retry-after", "0")], RATE_LIMITED),
            Reply::Status(200, vec![], embedding(route, 3)),
        ],
    )
    .await;
    assert_eq!(
        provider(&fake, quick())
            .embed("t")
            .await
            .expect("retried")
            .len(),
        3
    );
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
}

async fn a_failing_embedding_fails_as_unavailable(route: Route) {
    let fake = fake(
        route,
        vec![Reply::Status(
            503,
            vec![("retry-after", "0")],
            r#"{"error":{"message":"provider unavailable","type":"unavailable"}}"#,
        )],
    )
    .await;
    let err = provider(&fake, quick())
        .embed("t")
        .await
        .expect_err("down throughout");
    assert!(matches!(err, EngineError::Unavailable(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_route_names_the_pipeline_and_sends_no_bearer() {
    let fake = fake(Route::Runtime, vec![answer(vec!["ok"])]).await;
    // A key that is configured anyway is never sent to the runtime.
    provider_with_key(&fake, quick(), Some("provider-secret"))
        .generate("p")
        .await
        .expect("answer");
    let request = &fake.requests()[0];
    assert!(
        request.starts_with("post /v1/chat/completions "),
        "{request}"
    );
    assert!(request.contains(r#""model":"generate""#), "{request}");
    assert_eq!(header(request, "authorization"), None, "{request}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_direct_route_sends_its_bearer_only_when_one_is_configured() {
    let fake = fake(Route::Direct, vec![answer(vec!["ok"])]).await;
    provider_with_key(&fake, quick(), Some("provider-secret"))
        .generate("p")
        .await
        .expect("answer");
    provider(&fake, quick())
        .generate("p")
        .await
        .expect("answer");
    let requests = fake.requests();
    assert_eq!(
        header(&requests[0], "authorization"),
        Some("bearer provider-secret")
    );
    assert!(requests[0].contains(r#""model":"m""#));
    assert_eq!(header(&requests[1], "authorization"), None);
}
