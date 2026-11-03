//! Serving skills to a task: the gateway serves only the skill versions a task's token
//! pins, read-only, behind the same bearer check as MCP.
//!
//! The routes are plain JSON (Haystack's `SkillToolset` reads skills through a `SkillStore`,
//! not MCP):
//!
//! - `GET /skills`: `[{name, description}]` for the token's pins;
//! - `GET /skills/{name}`: `{name, version, body, files}`, the instructions and the manifest
//!   of bundled files;
//! - `GET /skills/{name}/files/{*path}`: one bundled file, as text.
//!
//! A skill the token does not pin, or a file the pinned version does not bundle, is a 404
//! (the body says which: `unknown-skill` or `unknown-file`), and so is every route when the
//! gateway has no [`SkillSource`]. Each request is logged with the task's trace and token ids.
//! Versions come from the injected [`SkillSource`]; the gateway never asks which version is
//! active, so a version switched mid-run never changes a running task.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use opentelemetry::trace::FutureExt;
use opentelemetry::{Context, KeyValue};
use serde_json::{Value, json};

use super::gateway::TaskContext;
use super::ledger::RunLedger;
use crate::skills::SkillBundle;
use crate::telemetry;

/// Where the gateway reads skill versions from. The backend implements it over the durable
/// skill registry.
#[async_trait]
pub trait SkillSource: Send + Sync {
    /// The content of skill version `version`, or `None` when no such version is registered.
    ///
    /// # Errors
    /// A message when the source cannot be read.
    async fn version(&self, version: &str) -> Result<Option<Arc<SkillBundle>>, String>;
}

/// What a skill request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOp {
    /// `GET /skills`.
    List,
    /// `GET /skills/{name}`.
    Load,
    /// `GET /skills/{name}/files/{*path}`.
    File,
}

impl SkillOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Load => "load",
            Self::File => "file",
        }
    }
}

/// One skill request, as the skill log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRecord {
    /// The task's trace id.
    pub trace_id: String,
    /// The token id.
    pub jti: String,
    /// The profile id.
    pub profile: String,
    /// The skill and its pinned version (`None` for a list, or a skill the token does not pin).
    pub skill: Option<(String, String)>,
    /// The skill named by a request for a skill the token does not pin.
    pub unpinned: Option<String>,
    /// What was requested.
    pub op: SkillOp,
    /// `ok`, `not-found` or `error`.
    pub outcome: &'static str,
}

impl fmt::Display for SkillRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let skill = match (&self.skill, &self.unpinned) {
            (Some((name, version)), _) => {
                format!("{name}@{}", version.get(..12).unwrap_or(version))
            }
            (None, Some(name)) => format!("{name}@-"),
            (None, None) => "*".to_owned(),
        };
        write!(
            f,
            "tool-gateway trace={} jti={} profile={} skill={skill} op={} outcome={}",
            self.trace_id,
            self.jti,
            self.profile,
            self.op.as_str(),
            self.outcome
        )
    }
}

/// Where skill records go. The default writes each as one line to the engine's log.
pub type SkillLog = Arc<dyn Fn(&SkillRecord) + Send + Sync>;

#[derive(Clone)]
struct SkillState {
    source: Option<Arc<dyn SkillSource>>,
    log: SkillLog,
    ledger: Option<Arc<RunLedger>>,
}

/// `value` as a JSON response with `status`.
fn json_response(status: StatusCode, value: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

/// Why a skill request failed, as the response says it.
enum Failure {
    /// No source, or a skill the token does not pin.
    UnknownSkill(String),
    /// A file the pinned version does not bundle.
    UnknownFile(String),
    /// The source failed, or does not hold a pinned version.
    Unavailable(String),
}

impl Failure {
    fn outcome(&self) -> &'static str {
        match self {
            Self::UnknownSkill(_) | Self::UnknownFile(_) => "not-found",
            Self::Unavailable(_) => "error",
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, error, message) = match self {
            Self::UnknownSkill(message) => (StatusCode::NOT_FOUND, "unknown-skill", message),
            Self::UnknownFile(message) => (StatusCode::NOT_FOUND, "unknown-file", message),
            Self::Unavailable(message) => (StatusCode::BAD_GATEWAY, "unavailable", message),
        };
        json_response(status, &json!({ "error": error, "message": message }))
    }
}

impl SkillState {
    /// The span of one skill request of `task`, parented as a tool call's is. It names the
    /// skill only when the token pins it: any other name is the caller's text.
    fn span(&self, task: &TaskContext, headers: &HeaderMap, skill: Option<&str>) -> Context {
        let run_span = self
            .ledger
            .as_ref()
            .and_then(|ledger| ledger.run_span(&task.claims.jti));
        let parent = telemetry::call_parent(Some(headers), &task.claims.trace_id, run_span);
        let name = match skill {
            None => "list_skills".to_owned(),
            Some(name) if task.claims.skills.contains_key(name) => format!("read_skill {name}"),
            Some(_) => "read_skill unknown".to_owned(),
        };
        telemetry::call_span(parent, name)
    }

    /// Log one request and end its `span`.
    fn record(
        &self,
        span: &Context,
        task: &TaskContext,
        op: SkillOp,
        skill: Option<(String, String)>,
        unpinned: Option<String>,
        outcome: &'static str,
    ) {
        let mut attributes = vec![
            KeyValue::new("ekg.skill.op", op.as_str()),
            KeyValue::new("ekg.skill.outcome", outcome),
        ];
        if let Some((name, version)) = &skill {
            attributes.push(KeyValue::new("ekg.skill.name", name.clone()));
            attributes.push(KeyValue::new("ekg.skill.version", version.clone()));
        }
        telemetry::finish(span, attributes, outcome == "ok", Some(outcome));
        (self.log)(&SkillRecord {
            trace_id: task.claims.trace_id.clone(),
            jti: task.claims.jti.clone(),
            profile: task.claims.profile.clone(),
            skill,
            unpinned,
            op,
            outcome,
        });
    }

    /// The pinned version of the skill `name`, read from the source.
    async fn pinned(&self, task: &TaskContext, name: &str) -> Result<Arc<SkillBundle>, Failure> {
        let unknown = || {
            Failure::UnknownSkill(format!(
                "no skill {name:?} is available to this task (available: {})",
                pinned_names(task)
            ))
        };
        let source = self.source.as_ref().ok_or_else(unknown)?;
        let version = task.claims.skills.get(name).ok_or_else(unknown)?;
        let bundle = source
            .version(version)
            .await
            .map_err(|why| Failure::Unavailable(format!("skill {name:?}: {why}")))?
            .ok_or_else(|| {
                Failure::Unavailable(format!(
                    "skill {name:?}: version {version} is not registered"
                ))
            })?;
        if bundle.name() != name {
            return Err(Failure::Unavailable(format!(
                "skill {name:?}: version {version} is the skill {:?}",
                bundle.name()
            )));
        }
        Ok(bundle)
    }
}

/// The names of the task's pinned skills, for an error message.
fn pinned_names(task: &TaskContext) -> String {
    if task.claims.skills.is_empty() {
        "none".to_owned()
    } else {
        task.claims
            .skills
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The pinned version of `name`, for the log (`None` when the token does not pin it).
fn pin_of(task: &TaskContext, name: &str) -> (Option<(String, String)>, Option<String>) {
    match task.claims.skills.get(name) {
        Some(version) => (Some((name.to_owned(), version.clone())), None),
        None => (None, Some(name.to_owned())),
    }
}

async fn list(
    State(state): State<SkillState>,
    Extension(task): Extension<TaskContext>,
    headers: HeaderMap,
) -> Response {
    let span = state.span(&task, &headers, None);
    let mut skills = Vec::with_capacity(task.claims.skills.len());
    let mut failure = None;
    if state.source.is_none() {
        failure = Some(Failure::UnknownSkill(
            "this gateway serves no skills".to_owned(),
        ));
    } else {
        for name in task.claims.skills.keys() {
            match state.pinned(&task, name).with_context(span.clone()).await {
                Ok(bundle) => skills
                    .push(json!({ "name": bundle.name(), "description": bundle.description() })),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
    }
    if let Some(failure) = failure {
        state.record(&span, &task, SkillOp::List, None, None, failure.outcome());
        return failure.into_response();
    }
    state.record(&span, &task, SkillOp::List, None, None, "ok");
    json_response(StatusCode::OK, &Value::Array(skills))
}

async fn load(
    State(state): State<SkillState>,
    Extension(task): Extension<TaskContext>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let span = state.span(&task, &headers, Some(&name));
    let (skill, unpinned) = pin_of(&task, &name);
    match state.pinned(&task, &name).with_context(span.clone()).await {
        Ok(bundle) => {
            state.record(&span, &task, SkillOp::Load, skill, unpinned, "ok");
            json_response(
                StatusCode::OK,
                &json!({
                    "name": bundle.name(),
                    "version": bundle.digest(),
                    "body": bundle.body(),
                    "files": bundle.bundled(),
                }),
            )
        }
        Err(failure) => {
            state.record(
                &span,
                &task,
                SkillOp::Load,
                skill,
                unpinned,
                failure.outcome(),
            );
            failure.into_response()
        }
    }
}

async fn file(
    State(state): State<SkillState>,
    Extension(task): Extension<TaskContext>,
    Path((name, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let span = state.span(&task, &headers, Some(&name));
    let (skill, unpinned) = pin_of(&task, &name);
    let pinned = state.pinned(&task, &name).with_context(span.clone()).await;
    let served = pinned.and_then(|bundle| {
        bundle
            .bundled_file(&path)
            .map(str::to_owned)
            .ok_or_else(|| {
                Failure::UnknownFile(format!(
                    "skill {name:?} bundles no file {path:?} (files: {})",
                    bundle.bundled().join(", ")
                ))
            })
    });
    match served {
        Ok(text) => {
            state.record(&span, &task, SkillOp::File, skill, unpinned, "ok");
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
        }
        Err(failure) => {
            state.record(
                &span,
                &task,
                SkillOp::File,
                skill,
                unpinned,
                failure.outcome(),
            );
            failure.into_response()
        }
    }
}

/// The skill routes, reading versions from `source`. The caller layers the bearer check on
/// top, which puts the verified [`TaskContext`] in every request. `ledger` is where a task's
/// run span is looked up.
pub(crate) fn router(
    source: Option<Arc<dyn SkillSource>>,
    log: SkillLog,
    ledger: Option<Arc<RunLedger>>,
) -> Router {
    Router::new()
        .route("/skills", get(list))
        .route("/skills/{name}", get(load))
        .route("/skills/{name}/files/{*path}", get(file))
        .with_state(SkillState {
            source,
            log,
            ledger,
        })
}
