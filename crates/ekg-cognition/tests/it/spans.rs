//! What the suites in this binary read exported spans from: one in-memory exporter behind the
//! process's tracer provider. Tests run at the same time and share it, so each reads the spans
//! of its own trace.

#![allow(clippy::expect_used)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{
    SpanContext, SpanId, Status, TraceContextExt as _, TraceFlags, TraceId, TraceState,
};
use opentelemetry::{Context, global};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

/// Install the exporter, once. Spans are exported as they end.
fn exporter() -> &'static InMemorySpanExporter {
    static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();
    EXPORTER.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_tracer_provider(provider);
        exporter
    })
}

/// Turn span export on for this process.
pub fn on() {
    let _ = exporter();
}

/// A trace id no other test uses.
pub fn fresh_trace() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!(
        "{:016x}{:016x}",
        u64::try_from(nanos & u128::from(u64::MAX)).expect("masked") | 1,
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// A span of `trace` with id `span`, as something to hang other spans under.
pub fn span_context(trace: &str, span: &str, sampled: bool) -> SpanContext {
    SpanContext::new(
        TraceId::from_hex(trace).expect("trace id"),
        SpanId::from_hex(span).expect("span id"),
        if sampled {
            TraceFlags::SAMPLED
        } else {
            TraceFlags::NOT_SAMPLED
        },
        true,
        TraceState::default(),
    )
}

/// A context whose spans are not exported: a caller that is traced but not sampled, which is
/// what the engine sees when telemetry is off.
pub fn unsampled() -> Context {
    Context::new().with_remote_span_context(span_context(&fresh_trace(), "00000000000000aa", false))
}

/// The exported spans of `trace`, in the order they ended.
pub fn of(trace: &str) -> Vec<SpanData> {
    exporter()
        .get_finished_spans()
        .expect("spans")
        .into_iter()
        .filter(|span| {
            span.span_context
                .trace_id()
                .to_string()
                .eq_ignore_ascii_case(trace)
        })
        .collect()
}

/// The one span of `spans` named `name`.
pub fn named<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let found: Vec<&SpanData> = spans.iter().filter(|span| span.name == name).collect();
    assert_eq!(
        found.len(),
        1,
        "one span {name:?} among {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );
    found[0]
}

/// The attribute `key` of `span`, as text.
pub fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// Whether `span` ended as an error.
pub fn is_error(span: &SpanData) -> bool {
    matches!(span.status, Status::Error { .. })
}

/// Everything `spans` carry as text: names and attribute keys and values.
pub fn text(spans: &[SpanData]) -> String {
    spans
        .iter()
        .flat_map(|span| {
            std::iter::once(span.name.to_string()).chain(
                span.attributes
                    .iter()
                    .map(|kv| format!("{}={}", kv.key, kv.value)),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
