//! Trace export: off unless an OTLP endpoint is configured.
//!
//! With `OTEL_EXPORTER_OTLP_ENDPOINT` set, the process exports the engine's spans over OTLP
//! (HTTP with protobuf) through a batch processor. The exporter reads the endpoint and
//! `OTEL_EXPORTER_OTLP_HEADERS` itself; `OTEL_SERVICE_NAME` names the service (default
//! `ekg-engine`). With no endpoint nothing is installed: every span is a no-op and no
//! connection is opened.
//!
//! Export is best effort. Spans are queued and sent from a thread of their own, so no engine
//! path waits for the trace backend; a full queue drops spans. A failed export is printed
//! once per process, with the endpoint's host and never its headers, and counted after that.
//! [`Telemetry::shutdown`] flushes what is queued, within [`FLUSH_BOUND`].

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::http::Uri;
use opentelemetry::global;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData};

/// The setting that turns trace export on.
pub const ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// The setting that names the service on exported spans.
pub const SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
/// The service name when none is set.
pub const DEFAULT_SERVICE_NAME: &str = "ekg-engine";
/// How long one export may take.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the flush on exit may take.
pub const FLUSH_BOUND: Duration = Duration::from_secs(5);

/// Where a line about telemetry goes. The default is the process's standard error.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

fn stderr() -> Sink {
    Arc::new(|line: &str| eprintln!("{line}"))
}

/// The host of `endpoint`, with its port: what a line may say about where spans go. Never
/// the path, a query or user info, which can hold a key.
fn host_of(endpoint: &str) -> String {
    endpoint
        .trim()
        .parse::<Uri>()
        .ok()
        .and_then(|uri| {
            uri.authority()
                .map(|authority| authority.as_str().to_owned())
        })
        .map_or_else(
            || "the configured endpoint".to_owned(),
            |authority| {
                // An authority may carry `user:password@`.
                authority
                    .rsplit_once('@')
                    .map_or(authority.clone(), |(_, host)| host.to_owned())
            },
        )
}

/// The line printed when export starts.
fn startup_line(endpoint: &str, service: &str) -> String {
    format!(
        "telemetry: exporting traces to {} as {service}",
        host_of(endpoint)
    )
}

/// An exporter that says once when an export fails and counts the failures after that.
struct Reporting<E> {
    inner: E,
    host: String,
    failures: Arc<AtomicU64>,
    sink: Sink,
}

impl<E> fmt::Debug for Reporting<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reporting")
            .field("host", &self.host)
            .field("failures", &self.failures.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for Reporting<E>
{
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        if let Err(why) = &result
            && self.failures.fetch_add(1, Ordering::Relaxed) == 0
        {
            (self.sink)(&format!(
                "warning: telemetry: spans could not be exported to {}: {why}. Further failures are \
                 counted, not printed; nothing waits for the export.",
                self.host
            ));
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// The process's trace export, or none. Dropping it without [`Self::shutdown`] loses what is
/// still queued.
#[must_use = "shut telemetry down before the process exits, or queued spans are lost"]
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
    failures: Arc<AtomicU64>,
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telemetry")
            .field("on", &self.is_on())
            .field("failed_exports", &self.failed_exports())
            .finish()
    }
}

impl Telemetry {
    /// No export: nothing is installed.
    pub fn off() -> Self {
        Self {
            provider: None,
            failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Install trace export as the process's settings say, read with `setting`: on when
    /// [`ENDPOINT`] is set and not empty, off otherwise. A setting that cannot be used turns
    /// export off with a line saying why; it never stops the process.
    pub fn from_settings(setting: impl Fn(&str) -> Option<String>) -> Self {
        let value = |key: &str| setting(key).filter(|value| !value.trim().is_empty());
        let Some(endpoint) = value(ENDPOINT) else {
            return Self::off();
        };
        let service = value(SERVICE_NAME).unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());
        Self::start(&endpoint, None, &service, &stderr())
    }

    /// Install trace export to `endpoint`, the OTLP base URL. `traces_url` overrides the URL
    /// spans are posted to, which is otherwise the exporter's reading of the environment.
    fn start(endpoint: &str, traces_url: Option<String>, service: &str, sink: &Sink) -> Self {
        // The exporter's HTTP client has no TLS provider of its own; an already installed
        // one is kept.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut builder = SpanExporter::builder()
            .with_http()
            .with_timeout(EXPORT_TIMEOUT);
        if let Some(url) = traces_url {
            builder = builder.with_endpoint(url);
        }
        let exporter = match builder.build() {
            Ok(exporter) => exporter,
            Err(why) => {
                sink(&format!(
                    "warning: telemetry: the exporter for {} could not be built: {why}; no trace is \
                     exported",
                    host_of(endpoint)
                ));
                return Self::off();
            }
        };
        let failures = Arc::new(AtomicU64::new(0));
        let provider = SdkTracerProvider::builder()
            .with_resource(
                Resource::builder()
                    .with_service_name(service.to_owned())
                    .build(),
            )
            .with_batch_exporter(Reporting {
                inner: exporter,
                host: host_of(endpoint),
                failures: Arc::clone(&failures),
                sink: Arc::clone(sink),
            })
            .build();
        global::set_tracer_provider(provider.clone());
        sink(&startup_line(endpoint, service));
        Self {
            provider: Some(provider),
            failures,
        }
    }

    /// Whether spans are exported.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.provider.is_some()
    }

    /// How many exports failed so far.
    #[must_use]
    pub fn failed_exports(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Send what is queued now, waiting for the export to end or fail.
    pub fn flush(&self) {
        if let Some(provider) = &self.provider {
            // A failed export is reported by the exporter; there is nothing to add here.
            let _ = provider.force_flush();
        }
    }

    /// Flush what is queued and stop exporting, within [`FLUSH_BOUND`]. A backend that does
    /// not answer in time loses the spans still queued; the caller's outcome is its own.
    pub fn shutdown(self) {
        if let Some(provider) = self.provider {
            let _ = provider.shutdown_with_timeout(FLUSH_BOUND);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::Mutex;
    use std::time::Instant;

    use opentelemetry::trace::{Span as _, Tracer as _};

    use super::*;

    fn collecting() -> (Sink, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&lines);
        let sink: Sink = Arc::new(move |line: &str| {
            kept.lock().expect("lines").push(line.to_owned());
        });
        (sink, lines)
    }

    /// A local port nothing listens on.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }

    fn span(name: &'static str) {
        global::tracer("test").start(name).end();
    }

    #[test]
    fn an_unset_or_empty_endpoint_installs_nothing() {
        assert!(!Telemetry::from_settings(|_| None).is_on());
        assert!(!Telemetry::from_settings(|key| (key == ENDPOINT).then(String::new)).is_on());
        assert!(
            !Telemetry::from_settings(|key| (key == ENDPOINT).then(|| "   ".to_owned())).is_on()
        );
    }

    #[test]
    fn the_startup_line_names_the_host_and_nothing_else() {
        let line = startup_line(
            "http://pk-user:sk-secret@langfuse:3000/api/public/otel?key=sk-query",
            "ekg-engine",
        );
        assert_eq!(
            line,
            "telemetry: exporting traces to langfuse:3000 as ekg-engine"
        );
        assert!(!line.contains("sk-"), "{line}");
        assert_eq!(host_of("not a url"), "the configured endpoint");
    }

    /// Tests that install the process's tracer provider run one at a time.
    static PROVIDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn a_refusing_endpoint_is_reported_once_and_holds_nothing_up() {
        let _one_at_a_time = PROVIDER.blocking_lock();
        let endpoint = format!("http://127.0.0.1:{}", closed_port());
        let (sink, lines) = collecting();
        let telemetry = Telemetry::start(
            &endpoint,
            Some(format!("{endpoint}/v1/traces")),
            "svc",
            &sink,
        );
        assert!(telemetry.is_on());
        for _ in 0..3 {
            span("work");
            telemetry.flush();
        }
        assert!(telemetry.failed_exports() >= 3, "{telemetry:?}");
        let started = Instant::now();
        span("last");
        telemetry.shutdown();
        assert!(started.elapsed() < FLUSH_BOUND + Duration::from_secs(1));
        let lines = lines.lock().expect("lines");
        let failures: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("could not be exported"))
            .collect();
        assert_eq!(failures.len(), 1, "{lines:?}");
        assert!(failures[0].contains("127.0.0.1"));
    }

    mod agent_run {
        use std::collections::BTreeMap;

        use async_trait::async_trait;
        use ekg_cognition::tools::{RunLedger, SkillPin, TaskTokens};
        use ekg_cognition::{
            AgentLimits, AgentProposer, Reasoner, ReasoningTask, RunRecord, RunRecorder, RunReport,
            SkillPins, TaskBuilder, Termination,
        };
        use ekg_domain::fakes::InMemoryWorldModel;
        use ekg_domain::{CandidateProposer, EntityUri, SourceRecord, SourceRef, WorldModel};
        use opentelemetry::trace::{
            FutureExt as _, SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId,
            TraceState,
        };

        use super::*;
        use crate::propose::supply_risk_profile;

        /// Answers at once, as a runtime whose agent ended without a plan.
        struct Scripted;

        #[async_trait]
        impl Reasoner for Scripted {
            async fn reason(&self, _task: &ReasoningTask) -> ekg_domain::Result<RunReport> {
                Ok(RunReport {
                    termination: Termination::NoPlan,
                    steps: 2,
                    input_tokens: 10,
                    output_tokens: 5,
                    model: "scripted".to_owned(),
                    closing_answer: None,
                })
            }
        }

        struct NoPins;

        #[async_trait]
        impl SkillPins for NoPins {
            async fn pins(&self, _profile: &str) -> ekg_domain::Result<Vec<SkillPin>> {
                Ok(Vec::new())
            }
        }

        #[derive(Default)]
        struct Records(Mutex<Vec<RunRecord>>);

        impl RunRecorder for Records {
            fn record(&self, record: RunRecord) {
                self.0.lock().expect("records").push(record);
            }
        }

        /// One agent run over an in-memory order under `cx`: its record, without what differs
        /// from run to run, and how long it took.
        async fn run(cx: opentelemetry::Context) -> (RunRecord, Duration) {
            let world = Arc::new(InMemoryWorldModel::default());
            let source_ref = SourceRef {
                system: "odoo".to_owned(),
                model: "sale.order".to_owned(),
                id: 1,
            };
            let order: EntityUri = InMemoryWorldModel::uri_of(&source_ref);
            world
                .materialize(&SourceRecord {
                    source_ref,
                    fields: BTreeMap::from([("state".to_owned(), "late".to_owned())]),
                })
                .await
                .expect("materialize");
            let world: Arc<dyn WorldModel> = world;
            let records = Arc::new(Records::default());
            let task: TaskBuilder = Arc::new(|_, _| "Plan for the order.".to_owned());
            let proposer = AgentProposer::new(
                Arc::clone(&world),
                Arc::new(supply_risk_profile(Arc::clone(&world), 4)),
                Arc::new(TaskTokens::new(b"telemetry-suite-secret-32-bytes!!").expect("secret")),
                Arc::new(Scripted),
                Arc::new(RunLedger::default()),
                Arc::new(NoPins),
                task,
                AgentLimits {
                    max_steps: 4,
                    run_cap: Duration::from_secs(60),
                },
            )
            .with_recorder(Arc::clone(&records) as Arc<dyn RunRecorder>);
            let started = Instant::now();
            let _ = proposer
                .propose_candidate(&order, &[])
                .with_context(cx)
                .await;
            let took = started.elapsed();
            let mut record = records.0.lock().expect("records").remove(0);
            record.trace_id.clear();
            record.elapsed_ms = 0;
            (record, took)
        }

        /// A caller that is traced but not exported: what a run sees with telemetry off.
        fn unsampled() -> opentelemetry::Context {
            opentelemetry::Context::new().with_remote_span_context(SpanContext::new(
                TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace"),
                SpanId::from_hex("00f067aa0ba902b7").expect("span"),
                TraceFlags::NOT_SAMPLED,
                true,
                TraceState::default(),
            ))
        }

        /// A local endpoint that accepts every connection and never answers.
        fn silent_endpoint() -> String {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for connection in listener.incoming().flatten() {
                    held.push(connection);
                }
            });
            format!("http://127.0.0.1:{port}")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_run_does_not_wait_for_a_trace_backend_that_refuses_or_never_answers() {
            let _one_at_a_time = PROVIDER.lock().await;
            let (without, _) = run(unsampled()).await;
            assert_eq!(without.termination, Termination::NoPlan);

            for (endpoint, refuses) in [
                (format!("http://127.0.0.1:{}", closed_port()), true),
                (silent_endpoint(), false),
            ] {
                let (sink, lines) = collecting();
                let telemetry = Telemetry::start(
                    &endpoint,
                    Some(format!("{endpoint}/v1/traces")),
                    "svc",
                    &sink,
                );
                assert!(telemetry.is_on());
                let (record, took) = run(opentelemetry::Context::new()).await;
                assert_eq!(record, without, "the run is the one without telemetry");
                assert!(took < Duration::from_secs(2), "the run took {took:?}");

                let started = Instant::now();
                tokio::task::spawn_blocking(move || {
                    if refuses {
                        telemetry.flush();
                        assert!(telemetry.failed_exports() >= 1);
                    }
                    telemetry.shutdown();
                })
                .await
                .expect("shutdown");
                assert!(
                    started.elapsed() < FLUSH_BOUND + Duration::from_secs(1),
                    "the flush is bounded"
                );
                let reported = lines
                    .lock()
                    .expect("lines")
                    .iter()
                    .filter(|line| line.contains("could not be exported"))
                    .count();
                assert_eq!(reported, usize::from(refuses), "said once, when it refused");
            }
        }
    }
}
