//! Spans of an agent run, through the process's global tracer.
//!
//! Nothing here exports: a binary installs a tracer provider when it is configured to, and
//! until then every span is a no-op and every id is drawn at random. A span carries names,
//! versions, outcomes and counts only: never a task's text, a tool's arguments or result, an
//! entity, or a token.

use axum::http::HeaderMap;
use ekg_domain::{EngineError, Result};
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId, TraceState, Tracer,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_sdk::propagation::TraceContextPropagator;

use crate::reasoner::random_hex;

/// The instrumentation scope of the engine's spans.
const SCOPE: &str = "ekg-engine";

pub(crate) fn tracer() -> BoxedTracer {
    global::tracer(SCOPE)
}

/// `parent` with a new span `name` of `kind` as its active span.
pub(crate) fn child(parent: &Context, name: String, kind: SpanKind) -> Context {
    let tracer = tracer();
    let span = tracer
        .span_builder(name)
        .with_kind(kind)
        .start_with_context(&tracer, parent);
    parent.with_span(span)
}

/// A context holding a new root span `name`: a trace of its own, whatever is current.
pub(crate) fn root(name: String, kind: SpanKind) -> Context {
    child(&Context::new(), name, kind)
}

/// Whether the active span of `cx` is exported.
pub(crate) fn recording(cx: &Context) -> bool {
    cx.span().is_recording() && cx.span().span_context().is_valid()
}

/// The trace id and span id a task started under `cx` is traced by: the active span's when it
/// is exported, random ones otherwise, so a run record and a failure always name a trace.
pub(crate) fn task_ids(cx: &Context) -> Result<(String, String)> {
    if recording(cx) {
        let span = cx.span();
        let context = span.span_context();
        return Ok((
            context.trace_id().to_string(),
            context.span_id().to_string(),
        ));
    }
    Ok((random_hex(16)?, random_hex(8)?))
}

/// The id of the active span of `cx`, when it is exported.
pub(crate) fn span_id(cx: &Context) -> Option<String> {
    recording(cx).then(|| cx.span().span_context().span_id().to_string())
}

/// The active span of `cx`, as a parent another request can be hung under.
pub(crate) fn span_context(cx: &Context) -> Option<SpanContext> {
    recording(cx).then(|| cx.span().span_context().clone())
}

/// The W3C trace-context headers of a request made for the task traced as `trace_id`, under
/// the span `span_id`.
///
/// # Errors
/// [`EngineError::Backend`] when either id is not a W3C id.
pub(crate) fn trace_headers(trace_id: &str, span_id: &str) -> Result<HeaderMap> {
    let invalid = || EngineError::Backend(format!("trace context: not a W3C id: {trace_id}"));
    let context = SpanContext::new(
        TraceId::from_hex(trace_id).map_err(|_| invalid())?,
        SpanId::from_hex(span_id).map_err(|_| invalid())?,
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    );
    if !context.is_valid() {
        return Err(invalid());
    }
    let mut headers = HeaderMap::new();
    TraceContextPropagator::new().inject_context(
        &Context::new().with_remote_span_context(context),
        &mut HeaderInjector(&mut headers),
    );
    Ok(headers)
}

/// The trace context `headers` carry, when it names the trace `trace_id`. A context of
/// another trace is ignored: the task's token decides which trace a call belongs to.
fn traceparent_of(headers: &HeaderMap, trace_id: &str) -> Option<SpanContext> {
    let cx = TraceContextPropagator::new().extract(&HeaderExtractor(headers));
    let span = cx.span();
    let context = span.span_context();
    (context.is_valid()
        && context
            .trace_id()
            .to_string()
            .eq_ignore_ascii_case(trace_id))
    .then(|| context.clone())
}

/// The parent of a gateway call of the task traced as `trace_id`: the caller's span when the
/// request continues the task's trace, the task's run span otherwise, and none when neither is
/// known (the call then gets no span).
pub(crate) fn call_parent(
    headers: Option<&HeaderMap>,
    trace_id: &str,
    run_span: Option<SpanContext>,
) -> Option<Context> {
    headers
        .and_then(|headers| traceparent_of(headers, trace_id))
        .or(run_span)
        .map(|parent| Context::new().with_remote_span_context(parent))
}

/// A span for a gateway call under `parent`, or a context without one when there is no parent.
pub(crate) fn call_span(parent: Option<Context>, name: String) -> Context {
    match parent {
        Some(parent) => child(&parent, name, SpanKind::Server),
        None => Context::new(),
    }
}

/// End the active span of `cx` with `attributes`, as an error unless `ok`.
pub(crate) fn finish(cx: &Context, attributes: Vec<KeyValue>, ok: bool, error_type: Option<&str>) {
    let span = cx.span();
    span.set_attributes(attributes);
    if !ok {
        if let Some(class) = error_type {
            span.set_attribute(KeyValue::new("error.type", class.to_owned()));
        }
        span.set_status(Status::error(error_type.unwrap_or("error").to_owned()));
    }
    span.end();
}

/// Exported spans, for the unit tests of this crate: one in-memory exporter behind the
/// process's tracer provider. Tests share it, so each reads its own spans by an attribute.
#[cfg(test)]
pub(crate) mod testing {
    #![allow(clippy::expect_used)]

    use std::sync::OnceLock;

    use opentelemetry::global;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

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
    pub(crate) fn on() {
        let _ = exporter();
    }

    /// The attribute `key` of `span`, as text.
    pub(crate) fn attr(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    }

    /// The exported spans whose attribute `key` is `value`, in the order they ended.
    pub(crate) fn with(key: &str, value: &str) -> Vec<SpanData> {
        exporter()
            .get_finished_spans()
            .expect("spans")
            .into_iter()
            .filter(|span| attr(span, key).as_deref() == Some(value))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", value.parse().expect("header"));
        headers
    }

    #[test]
    fn a_task_under_no_exported_span_still_gets_ids() {
        let cx = Context::new();
        assert!(!recording(&cx));
        assert!(span_context(&cx).is_none());
        let (trace, span) = task_ids(&cx).expect("ids");
        assert_eq!((trace.len(), span.len()), (32, 16));
        assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_traceparent_of_the_tasks_trace_is_the_parent() {
        let parent = call_parent(
            Some(&headers(&format!("00-{TRACE}-00f067aa0ba902b7-01"))),
            TRACE,
            None,
        )
        .expect("parent");
        let span = parent.span();
        assert_eq!(span.span_context().trace_id().to_string(), TRACE);
        assert_eq!(
            span.span_context().span_id().to_string(),
            "00f067aa0ba902b7"
        );
    }

    #[test]
    fn the_headers_of_a_task_name_its_trace_and_span() {
        let headers = trace_headers(TRACE, "00f067aa0ba902b7").expect("headers");
        assert_eq!(
            headers.get("traceparent").and_then(|v| v.to_str().ok()),
            Some(format!("00-{TRACE}-00f067aa0ba902b7-01").as_str())
        );
        assert!(trace_headers("not-hex", "00f067aa0ba902b7").is_err());
        assert!(trace_headers(TRACE, "0000000000000000").is_err());
    }

    #[test]
    fn a_traceparent_of_another_trace_is_ignored() {
        let other = "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-00f067aa0ba902b7-01";
        assert!(call_parent(Some(&headers(other)), TRACE, None).is_none());
        assert!(call_parent(Some(&headers("garbage")), TRACE, None).is_none());
        assert!(call_parent(None, TRACE, None).is_none());
    }

    #[test]
    fn without_a_traceparent_the_run_span_is_the_parent() {
        let run = SpanContext::new(
            TraceId::from_hex(TRACE).expect("trace"),
            SpanId::from_hex("1111111111111111").expect("span"),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        );
        let parent = call_parent(None, TRACE, Some(run)).expect("parent");
        assert_eq!(
            parent.span().span_context().span_id().to_string(),
            "1111111111111111"
        );
    }
}
