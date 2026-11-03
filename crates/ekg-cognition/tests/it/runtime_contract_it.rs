//! Gated contract suite for the served cognition runtime: the built image
//! (`localhost/ekg-hayhooks:local`, from `just podman-build`) runs in a container whose
//! provider is an in-test fake, so every runtime promise the engine relies on is checked
//! against the real pipelines:
//!
//! - a provider rate limit passes through with its `Retry-After`, after one provider
//!   request (the runtime does not retry);
//! - an exhausted quota keeps its error type; an unreachable provider is a 503;
//! - a failure mid-stream leaves no finish marker; a complete answer ends with one and a
//!   usage chunk;
//! - durable mode is off, and with no OTLP endpoint nothing is exported;
//! - an engine call that is dropped stops the provider stream;
//! - a configured OTLP endpoint receives the call's spans under the engine's trace id,
//!   with the token counts and no prompt text.
//!
//! Ignored by default. It reports a missing container runtime or image through the shared
//! loud-skip helper, so `just test-e2e` (which sets `EKG_E2E_REQUIRE_INFRA`) fails instead
//! of passing on a skip. Run with:
//!
//! ```text
//! just podman-build
//! cargo test -p ekg-cognition --test it runtime_contract -- --ignored --test-threads=1
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown)]

use crate::loud_skip;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The image under test.
const IMAGE: (&str, &str) = ("localhost/ekg-hayhooks", "local");
/// How a container on this machine reaches a listener on the host (Podman).
const HOST_FROM_CONTAINER: &str = "host.containers.internal";
/// The chat model the runtime is configured with. The name is distinctive, so a span
/// naming it is recognizable in the exported bytes.
const CHAT_MODEL: &str = "contract-chat-model";
/// A prompt marker that must never appear in exported telemetry.
const PROMPT_MARKER: &str = "PROMPT-MARKER-7d1f";

// --- the fake provider ---

/// How the fake provider answers every chat request.
#[derive(Clone, Copy)]
enum Mode {
    /// "hel" + "lo" (finish `stop`), a usage chunk, `[DONE]`.
    Answer,
    /// HTTP 429 with `Retry-After: 2`.
    RateLimited,
    /// HTTP 429 with an `insufficient_quota` error.
    Quota,
    /// One chunk, then the connection drops.
    MidStream,
    /// A chunk every 100 ms for ~10 s, noting whether the reader went away early.
    Slow,
}

/// A fake OpenAI-compatible provider on the host, counting requests.
struct Provider {
    port: u16,
    hits: Arc<AtomicUsize>,
    /// Set when a `Slow` stream could no longer be written (its reader closed it).
    closed_early: Arc<AtomicBool>,
}

async fn provider(mode: Mode) -> Provider {
    let listener = TcpListener::bind("0.0.0.0:0").await.expect("bind provider");
    let port = listener.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let closed_early = Arc::new(AtomicBool::new(false));
    {
        let (hits, closed_early) = (Arc::clone(&hits), Arc::clone(&closed_early));
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                hits.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(answer(socket, mode, Arc::clone(&closed_early)));
            }
        });
    }
    Provider {
        port,
        hits,
        closed_early,
    }
}

/// Read one HTTP request (head + content-length body).
async fn read_request(socket: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        let Ok(read) = socket.read(&mut buf).await else {
            return request;
        };
        if read == 0 {
            return request;
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
                return request;
            }
        }
    }
}

fn sse(content: &str, finish: Option<&str>) -> String {
    let finish = finish.map_or("null".to_owned(), |f| format!("\"{f}\""));
    format!(
        "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{CHAT_MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":{finish}}}]}}\n\n"
    )
}

/// The head of a streamed (SSE) answer.
const SSE_HEAD: &str =
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";

async fn answer(mut socket: TcpStream, mode: Mode, closed_early: Arc<AtomicBool>) {
    read_request(&mut socket).await;
    let json_reply = |status: &str, headers: &str, body: &str| {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{body}",
            body.len()
        )
    };
    match mode {
        Mode::RateLimited => {
            let body =
                r#"{"error":{"message":"slow down","type":"rate_limit","code":"rate_limited"}}"#;
            let reply = json_reply("429 Too Many Requests", "retry-after: 2\r\n", body);
            let _ = socket.write_all(reply.as_bytes()).await;
        }
        Mode::Quota => {
            let body = r#"{"error":{"message":"quota","type":"insufficient_quota","code":"insufficient_quota"}}"#;
            let reply = json_reply("429 Too Many Requests", "", body);
            let _ = socket.write_all(reply.as_bytes()).await;
        }
        Mode::Answer => {
            let usage = format!(
                "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{CHAT_MODEL}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}}}\n\n"
            );
            let body = format!(
                "{SSE_HEAD}{}{}{usage}data: [DONE]\n\n",
                sse("hel", None),
                sse("lo", Some("stop"))
            );
            let _ = socket.write_all(body.as_bytes()).await;
        }
        Mode::MidStream => {
            let _ = socket
                .write_all(format!("{SSE_HEAD}{}", sse("hel", None)).as_bytes())
                .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Drop the connection without finishing the answer.
            return;
        }
        Mode::Slow => {
            if socket.write_all(SSE_HEAD.as_bytes()).await.is_err() {
                return;
            }
            for _ in 0..100 {
                if socket.write_all(sse("x", None).as_bytes()).await.is_err() {
                    closed_early.store(true, Ordering::SeqCst);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    let _ = socket.shutdown().await;
}

// --- the fake OTLP collector ---

/// A fake OTLP/HTTP collector on the host, keeping every export body.
pub(crate) struct Collector {
    pub(crate) port: u16,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Collector {
    /// Every byte exported so far.
    pub(crate) fn exported(&self) -> Vec<u8> {
        self.bodies
            .lock()
            .expect("bodies")
            .iter()
            .flatten()
            .copied()
            .collect()
    }
}

pub(crate) async fn collector() -> Collector {
    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind collector");
    let port = listener.local_addr().expect("addr").port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let bodies = Arc::clone(&bodies);
                tokio::spawn(async move {
                    // Keep-alive: serve every export on this connection.
                    loop {
                        let request = read_request(&mut socket).await;
                        if request.is_empty() {
                            return;
                        }
                        bodies.lock().expect("bodies").push(request);
                        let ok = "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n";
                        if socket.write_all(ok.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
    }
    Collector { port, bodies }
}

// --- the runtime under test ---

/// Start the runtime with its provider at `provider_url` (plus `extra` env), or report
/// the missing runtime/image through the loud-skip helper and return `None`.
async fn runtime(
    provider_url: &str,
    extra: &[(&str, String)],
) -> Option<(ContainerAsync<GenericImage>, String)> {
    let mut request = GenericImage::new(IMAGE.0, IMAGE.1)
        .with_exposed_port(1416.tcp())
        .with_wait_for(WaitFor::message_on_either_std(
            "Application startup complete",
        ))
        .with_env_var("MODEL_API_KEY", "contract-dummy-key")
        .with_env_var("MODEL_BASE_URL", provider_url)
        .with_env_var("CHAT_MODEL", CHAT_MODEL)
        .with_env_var("EMBED_MODEL", "contract-embed-model")
        .with_env_var("HAYHOOKS_DURABLE_MODE", "false")
        .with_env_var("HAYSTACK_CONTENT_TRACING_ENABLED", "false")
        .with_startup_timeout(Duration::from_secs(90));
    for (name, value) in extra {
        request = request.with_env_var(*name, value.clone());
    }
    let container = match request.start().await {
        Ok(container) => container,
        Err(why) => {
            loud_skip::infra_unavailable(&format!(
                "no container runtime or no {}:{} image (run `just podman-build`): {why}",
                IMAGE.0, IMAGE.1
            ));
            return None;
        }
    };
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(1416.tcp())
        .await
        .expect("mapped 1416");
    Some((container, format!("http://{host}:{port}")))
}

fn provider_url(provider: &Provider) -> String {
    format!("http://{HOST_FROM_CONTAINER}:{}/v1", provider.port)
}

fn http() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client")
}

/// A W3C traceparent with a fixed, recognizable trace id.
const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// POST a streamed chat request naming the `generate` pipeline.
async fn chat(base: &str) -> reqwest::Response {
    let body = format!(
        r#"{{"model":"generate","stream":true,"messages":[{{"role":"user","content":"{PROMPT_MARKER} say hello"}}]}}"#
    );
    http()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("traceparent", format!("00-{TRACE_ID}-00f067aa0ba902b7-01"))
        .body(body)
        .send()
        .await
        .expect("chat request")
}

/// Read a response body to its end (or to the transfer error an aborted stream gives).
async fn read_all(mut response: reqwest::Response) -> (String, bool) {
    let mut body = Vec::new();
    let mut aborted = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(_) => {
                aborted = true;
                break;
            }
        }
    }
    (String::from_utf8_lossy(&body).into_owned(), aborted)
}

async fn logs(container: &ContainerAsync<GenericImage>) -> String {
    let out = container.stdout_to_vec().await.unwrap_or_default();
    let err = container.stderr_to_vec().await.unwrap_or_default();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    )
}

// --- the contract ---

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_provider_rate_limit_passes_through_unchanged() {
    let provider = provider(Mode::RateLimited).await;
    let Some((_container, base)) = runtime(&provider_url(&provider), &[]).await else {
        return;
    };
    let response = chat(&base).await;
    assert_eq!(response.status(), 429);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );
    let (body, _) = read_all(response).await;
    assert!(body.contains("slow down"), "{body}");
    assert_eq!(
        provider.hits.load(Ordering::SeqCst),
        1,
        "the runtime never retries the provider"
    );
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn an_exhausted_quota_keeps_its_error_type() {
    let provider = provider(Mode::Quota).await;
    let Some((_container, base)) = runtime(&provider_url(&provider), &[]).await else {
        return;
    };
    let response = chat(&base).await;
    assert_eq!(response.status(), 429);
    let (body, _) = read_all(response).await;
    assert!(body.contains(r#""type":"insufficient_quota""#), "{body}");
    assert_eq!(provider.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn an_unreachable_provider_is_a_503() {
    // Port 1 on the host: nothing listens there.
    let unreachable = format!("http://{HOST_FROM_CONTAINER}:1/v1");
    let Some((_container, base)) = runtime(&unreachable, &[]).await else {
        return;
    };
    assert_eq!(chat(&base).await.status(), 503);
    let embed = http()
        .post(format!("{base}/embed/run"))
        .header("content-type", "application/json")
        .body(r#"{"texts":["a"]}"#)
        .send()
        .await
        .expect("embed request");
    assert_eq!(embed.status(), 503);
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_failure_mid_stream_leaves_no_finish_marker() {
    let provider = provider(Mode::MidStream).await;
    let Some((_container, base)) = runtime(&provider_url(&provider), &[]).await else {
        return;
    };
    let response = chat(&base).await;
    assert_eq!(response.status(), 200, "the stream had begun");
    let (body, _) = read_all(response).await;
    assert!(body.contains(r#""content":"hel""#), "{body}");
    assert!(!body.contains(r#""finish_reason":"stop""#), "{body}");
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_complete_answer_ends_with_its_finish_marker_and_usage_and_nothing_is_exported() {
    let provider = provider(Mode::Answer).await;
    let Some((container, base)) = runtime(&provider_url(&provider), &[]).await else {
        return;
    };
    let status = http()
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("status");
    let (status, _) = read_all(status).await;
    assert!(status.contains(r#""durable_mode":false"#), "{status}");
    assert!(
        status.contains("generate") && status.contains("embed"),
        "{status}"
    );

    let (body, aborted) = read_all(chat(&base).await).await;
    assert!(!aborted, "a complete answer is a complete transfer");
    let stop = body
        .find(r#""finish_reason":"stop""#)
        .expect("finish marker");
    let usage = body
        .find(r#""usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}"#)
        .expect("usage chunk");
    assert!(stop < usage, "the usage chunk comes last: {body}");
    assert!(
        !logs(&container).await.contains("tracing enabled with OTLP"),
        "no OTLP endpoint configured, so no exporter"
    );
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_dropped_engine_call_stops_the_provider_stream() {
    let provider = provider(Mode::Slow).await;
    let Some((_container, base)) = runtime(&provider_url(&provider), &[]).await else {
        return;
    };
    let mut response = chat(&base).await;
    assert_eq!(response.status(), 200);
    response
        .chunk()
        .await
        .expect("first chunk")
        .expect("some content");
    // The engine gives up (its time bound, or its caller went away).
    drop(response);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !provider.closed_early.load(Ordering::SeqCst) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        provider.closed_early.load(Ordering::SeqCst),
        "the runtime closed its provider stream after the engine dropped the call"
    );
}

/// Whether `haystack` contains `needle` as a contiguous byte run.
pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

pub(crate) fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}

#[tokio::test]
#[ignore = "requires a container runtime and the built runtime image (run with --ignored)"]
async fn a_configured_otlp_endpoint_receives_the_engine_trace_without_content() {
    let provider = provider(Mode::Answer).await;
    let collector = collector().await;
    let endpoint = format!("http://{HOST_FROM_CONTAINER}:{}", collector.port);
    let extra = [
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf".to_owned()),
        ("OTEL_SERVICE_NAME", "ekg-cognition-runtime".to_owned()),
        // Export promptly instead of the 5 s default batch delay.
        ("OTEL_BSP_SCHEDULE_DELAY", "200".to_owned()),
    ];
    let Some((_container, base)) = runtime(&provider_url(&provider), &extra).await else {
        return;
    };
    let (body, _) = read_all(chat(&base).await).await;
    assert!(body.contains(r#""finish_reason":"stop""#), "{body}");

    let trace_id = hex_bytes(TRACE_ID);
    let deadline = Instant::now() + Duration::from_secs(20);
    let exported = loop {
        let all = collector.exported();
        if contains(&all, &trace_id) && contains(&all, b"gen_ai.usage.input_tokens") {
            break all;
        }
        assert!(
            Instant::now() < deadline,
            "no export carried trace {TRACE_ID} with token counts ({} bytes exported)",
            all.len()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(
        contains(&exported, CHAT_MODEL.as_bytes()),
        "the model is named"
    );
    assert!(
        !contains(&exported, PROMPT_MARKER.as_bytes()),
        "no prompt text leaves the runtime"
    );
}
