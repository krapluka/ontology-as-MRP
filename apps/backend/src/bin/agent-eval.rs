//! The offline eval of the tool-using agent: run the eval set against the real model and grade
//! every trial.
//!
//! ```text
//! cargo run -q -p ekg-backend --bin agent-eval -- run [--trials <k>] [--max-tokens <n>]
//!     [--retries <n>] [--concurrency <n>] [--arm agent|legacy|both]
//!     [--case <id>]... [--category <name>]... [--profile <id>]...
//!     [--report <path>]
//! cargo run -q -p ekg-backend --bin agent-eval -- lock
//! ```
//!
//! `run` needs a reachable GraphDB (`GRAPHDB_URL`), the cognition runtime at
//! `EKG_AGENT_EVAL_RUNTIME_URL` with `EKG_TOOL_GATEWAY_URL` pointing at this process's gateway
//! (port `EKG_AGENT_EVAL_GATEWAY_PORT`, default 8791), and a model key in the environment. The
//! `just agent-eval` recipe starts the runtime that way. It needs neither Restate nor Odoo, and
//! it reads and writes nothing of the running stack: every case gets a repository of its own.
//!
//! A run spends model tokens: `k` agent runs per case (`--trials`, `EKG_AGENT_EVAL_TRIALS`,
//! default 3), and it starts no further trial once `--max-tokens`
//! (`EKG_AGENT_EVAL_MAX_TOKENS`, default 2,000,000) is reached. It writes its report to
//! `.artifacts/agent-eval/` and exits non-zero when the run is incomplete. `lock` renews
//! `set.lock` after the set's version was raised.
//!
//! A trial that ends in an error (the model or the runtime did not answer, or the plan could
//! not be scored) is attempted again, up to `--retries` further times
//! (`EKG_AGENT_EVAL_RETRIES`, default 2); a graded trial never is. Up to `--concurrency`
//! trials of a case run at once (`EKG_AGENT_EVAL_CONCURRENCY`, default 3), fewer when the
//! GraphDB server has no room for their repositories (`EKG_GRAPHDB_MAX_REPOSITORIES`,
//! default 5); the cases run one after another. `--case`, `--category` and `--profile` narrow
//! a run, which then does not count as evidence for a verdict.
//!
//! `--arm legacy` (`EKG_AGENT_EVAL_ARM`) runs the bounded prompt agent, the path that produces
//! today's proposals, on the same cases: one model call per trial through the same runtime,
//! named by `CHAT_MODEL`. It starts no gateway and reports no token usage. `--arm both` runs
//! the legacy arm and then the agent, and writes one paired report. A paired report carries the
//! digest of `promotion.yaml` in the set's
//! directory as it was when the run started, which is what lets it count as evidence for those
//! criteria; a document that does not parse stops the run before any trial.

// Docs mention product names (GraphDB, Restate, Odoo).
#![allow(clippy::doc_markdown)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use ekg_backend::eval::{
    self, ArmKind, Arms, Command, DEFAULT_GATEWAY_PORT, DEFAULT_RUNTIME_URL, EvalEnv, RunArgs,
};
use ekg_backend::telemetry::Telemetry;
use ekg_backend::{IngestionConfig, promotion, propose};
use ekg_cognition::RuntimeReasoner;
use ekg_cognition::eval::{
    Case, CaseResult, EvalSet, Experiment, LOCK_FILE, Lock, Measures, PairedReport, Report,
    RunLimits, RunSettings, run_cases_as,
};
use ekg_cognition::tools::now_secs;
use ekg_kinetic::verbs::epoch_to_odoo_datetime;
use ekg_semantic::GraphDbClient;

/// The repository's eval set.
const DEFAULT_SET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../evals/agent");
/// The repository's skills, next to the runtime that loads them.
const DEFAULT_SKILLS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../hayhooks/skills");
/// Where reports go unless `--report` names a file.
const REPORT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.artifacts/agent-eval");
/// How a container on this machine names the host the gateway listens on.
const HOST_FROM_CONTAINER: &str = "host.containers.internal";

#[tokio::main]
async fn main() -> ExitCode {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = eval::parse_args(
        &args,
        &env,
        PathBuf::from(DEFAULT_SET),
        PathBuf::from(DEFAULT_SKILLS),
    );
    // Trace export, when an endpoint is configured; nothing otherwise.
    let telemetry = Telemetry::from_settings(|key| env.get(key).cloned());
    let outcome = match command {
        Ok(Command::Lock { set }) => lock(&set),
        Ok(Command::Run(args)) => run(&args, &env, telemetry.is_on()).await,
        Err(why) => Err(why),
    };
    // The exit code is decided here, before the flush: a trace backend that does not answer
    // costs the flush bound and nothing else.
    let code = match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("agent-eval: {why}");
            ExitCode::FAILURE
        }
    };
    telemetry.shutdown();
    code
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|why| format!("{}: {why}", path.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|why| format!("{}: {why}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|why| format!("{}: {why}", path.display()))
}

fn lock(set_dir: &Path) -> Result<(), String> {
    let set = EvalSet::read_dir(set_dir).map_err(|e| e.to_string())?;
    let lock = Lock::of(&set);
    write(&set_dir.join(LOCK_FILE), &lock.to_text())?;
    println!(
        "locked eval set v{} ({} cases) at {}",
        lock.version,
        set.cases.len(),
        lock.digest
    );
    Ok(())
}

/// Whether the set is the one its lock was written for, as a message when it is not.
fn lock_state(set_dir: &Path, set: &EvalSet) -> Result<(), String> {
    Lock::parse(&read(&set_dir.join(LOCK_FILE))?)?
        .check(set)
        .map_err(|why| format!("{why} (`just agent-eval-lock`)"))
}

/// When the run started, as its report's file and its experiment runs are named.
fn started_stamp() -> Result<String, String> {
    let now = now_secs().map_err(|e| e.to_string())?;
    Ok(format!(
        "{}Z",
        epoch_to_odoo_datetime(now)
            .replace(' ', "T")
            .replace([':', '-'], "")
    ))
}

fn report_path(args: &RunArgs, started: &str) -> PathBuf {
    args.report
        .clone()
        .unwrap_or_else(|| Path::new(REPORT_DIR).join(format!("{started}.json")))
}

/// The model the runs reported: the first that named one.
fn served_model(cases: &[CaseResult]) -> String {
    cases
        .iter()
        .flat_map(|case| &case.trials)
        .filter_map(|trial| trial.run.as_ref())
        .map(|run| run.model.clone())
        .find(|model| !model.is_empty())
        .unwrap_or_default()
}

/// What every pass of a run shares.
struct Setup<'a> {
    args: &'a RunArgs,
    env: &'a BTreeMap<String, String>,
    set: &'a EvalSet,
    cases: &'a [&'a Case],
    config: &'a IngestionConfig,
    runtime: &'a str,
    /// The repositories the GraphDB server holds.
    max_repositories: usize,
    /// When the run started.
    started: &'a str,
    /// Whether the run's traces are exported.
    exporting: bool,
}

/// The environment of one pass, for `concurrency` trials in flight. The agent's also serves
/// the gateway the runtime reads through; the legacy arm calls the runtime's chat pipeline
/// and needs none.
async fn pass_env(setup: &Setup<'_>, arm: ArmKind, concurrency: u32) -> Result<EvalEnv, String> {
    let Setup {
        args,
        env,
        config,
        runtime,
        ..
    } = setup;
    let server = GraphDbClient::new(config.graphdb_url.clone(), config.graphdb_repo.clone())
        .map_err(|e| e.to_string())?;
    let reasoner =
        RuntimeReasoner::new(runtime, eval::permits(concurrency)).map_err(|e| e.to_string())?;
    let eval_env = EvalEnv::new(server, Arc::new(reasoner), &args.skills, config)?
        .scoring_at_most(concurrency);
    if arm == ArmKind::Legacy {
        let routed = eval::legacy_config(config, runtime, concurrency);
        let provider = propose::provider(&routed).map_err(|e| e.to_string())?;
        return Ok(eval_env.with_legacy_arm(Arc::new(provider)));
    }
    let setting = |key: &str| env.get(key).filter(|value| !value.trim().is_empty());
    let port = match setting("EKG_AGENT_EVAL_GATEWAY_PORT") {
        Some(value) => value
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("EKG_AGENT_EVAL_GATEWAY_PORT is not a port: {value:?}"))?,
        None => DEFAULT_GATEWAY_PORT,
    };
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|why| format!("the gateway cannot listen on port {port}: {why}"))?;
    let router = eval_env.gateway(HOST_FROM_CONTAINER);
    tokio::spawn(async move {
        if let Err(why) = axum::serve(listener, router).await {
            eprintln!("agent-eval: the gateway stopped: {why}");
        }
    });
    Ok(eval_env)
}

/// What the traces of `arm`'s pass are marked as. The agent's model is the runtime's to report,
/// so it is not known yet; the legacy arm's is the one it was configured with.
fn experiment_of(
    setup: &Setup<'_>,
    arm: ArmKind,
    eval_env: &EvalEnv,
    settings: RunSettings,
) -> Experiment {
    let configured = match arm {
        ArmKind::Agent => "",
        ArmKind::Legacy => setup.config.cognition_chat_model.as_str(),
    };
    let experiment = Experiment::of(
        arm.name(),
        setup.started,
        &eval_env.version(setup.set, setup.args.trials, configured),
        settings,
        &setup.args.narrowing(),
    );
    if setup.exporting {
        println!(
            "traces are marked as the experiment run {:?}",
            experiment.name
        );
    }
    experiment
}

/// Run every selected case under `arm` and report it.
async fn pass(setup: &Setup<'_>, arm: ArmKind) -> Result<Report, String> {
    let Setup {
        args,
        set,
        cases,
        config,
        runtime,
        max_repositories,
        exporting,
        ..
    } = setup;
    let server = GraphDbClient::new(config.graphdb_url.clone(), config.graphdb_repo.clone())
        .map_err(|e| e.to_string())?;
    for repo in eval::sweep_stale(&server).await? {
        println!("reclaimed the stale eval repository {repo}");
    }
    // What a killed run left is gone; what is left now stays for the whole pass.
    let existing = server.repository_ids().await.map_err(|e| e.to_string())?;
    let concurrency = eval::trial_bound(*max_repositories, &existing, args.concurrency)?;
    if concurrency < args.concurrency {
        println!(
            "{concurrency} trial(s) at a time instead of {}: the server holds at most \
             {max_repositories} repositories and has {} ({})",
            args.concurrency,
            existing.len(),
            existing.join(", ")
        );
    }
    let eval_env = pass_env(setup, arm, concurrency).await?;
    let bound = match arm {
        ArmKind::Agent => format!("at most {} tokens", args.max_tokens),
        ArmKind::Legacy => format!("one call of {} per trial", config.cognition_chat_model),
    };
    println!(
        "{} arm: eval set v{}, {} case(s), k={}, {concurrency} trial(s) at a time, {} further \
         attempt(s), {bound}, runtime {runtime}",
        arm.name(),
        set.manifest.version,
        cases.len(),
        args.trials,
        args.retries
    );
    let settings = RunSettings {
        concurrency,
        retries: args.retries,
    };
    let experiment = experiment_of(setup, arm, &eval_env, settings);
    let (mut results, mut tokens) = (Vec::with_capacity(cases.len()), 0_u64);
    for case in *cases {
        // One case at a time, so its line prints as soon as it is graded; the bound carries
        // over as what is left of it.
        let limits = RunLimits {
            trials: args.trials,
            max_tokens: args.max_tokens.saturating_sub(tokens),
            retries: args.retries,
            concurrency,
            set_version: set.manifest.version,
        };
        let (mut graded, used) = run_cases_as(
            &eval_env,
            std::slice::from_ref(case),
            limits,
            Some(&experiment),
        )
        .await;
        tokens = tokens.saturating_add(used);
        for result in &graded {
            println!(
                "{:<40} {}/{} passed, {} failed, {} violating, {} errored, {} not run",
                result.id,
                result.passed(),
                args.trials,
                result.failed(),
                result.violating(),
                result.errored(),
                result.not_run
            );
        }
        results.append(&mut graded);
    }
    let report = match arm {
        ArmKind::Agent => {
            let version = eval_env.version(set, args.trials, &served_model(&results));
            Report::new(arm.name(), version, results, tokens)
        }
        // The prompt path reports neither a served model nor its usage: the model is the one
        // the run was configured with.
        ArmKind::Legacy => {
            let version = eval_env.version(set, args.trials, &config.cognition_chat_model);
            Report::new(arm.name(), version, results, 0).measuring(Measures::CALLS_ONLY)
        }
    };
    Ok(report
        .run_with(settings)
        .narrowed_to(args.narrowing())
        .exported_as(exporting.then(|| experiment.reference())))
}

async fn run(
    args: &RunArgs,
    env: &BTreeMap<String, String>,
    exporting: bool,
) -> Result<(), String> {
    // Before anything is built or called: an eval without a model key cannot run.
    let credential = eval::model_credential(env)?;
    let set = EvalSet::read_dir(&args.set).map_err(|e| e.to_string())?;
    let cases = eval::select_cases(&set, args)?;
    if let Err(why) = lock_state(&args.set, &set) {
        eprintln!("warning: {why}");
    }
    // A paired run is evidence for the criteria that exist now, so they are read before any
    // trial: a report cannot name criteria written after it.
    let criteria = match args.arms {
        Arms::Both => promotion::digest_in(&args.set)?,
        Arms::Agent | Arms::Legacy => None,
    };

    let config = eval::engine_defaults(
        env,
        &[
            "GRAPHDB_URL",
            "EKG_AGENT_MAX_STEPS",
            "EKG_AGENT_JOINT_MAX_STEPS",
            "EKG_AGENT_JOINT_RUN_SECS",
            "EKG_AGENT_ANALYST_MAX_STEPS",
            "EKG_AGENT_ANALYST_RUN_SECS",
            "EKG_AGENT_PLANNERS",
            "EKG_AGENT_PLANNER_READS",
            "EKG_AGENT_TEXT_MAX_CHARS",
            "EKG_COGNITION_TOTAL_TIMEOUT_SECS",
            "CHAT_MODEL",
        ],
    )
    .map_err(|key| format!("{key} is not valid"))?;
    GraphDbClient::new(config.graphdb_url.clone(), config.graphdb_repo.clone())
        .map_err(|e| e.to_string())?
        .repository_ids()
        .await
        .map_err(|why| format!("no GraphDB at {}: {why}", config.graphdb_url))?;
    let runtime = env
        .get("EKG_AGENT_EVAL_RUNTIME_URL")
        .filter(|value| !value.trim().is_empty())
        .map_or(DEFAULT_RUNTIME_URL, String::as_str);
    println!("the runtime's model key comes from {credential}");
    let max_repositories = eval::max_repositories(env)?;

    let started = started_stamp()?;
    let setup = Setup {
        args,
        env,
        set: &set,
        cases: &cases,
        config: &config,
        runtime,
        max_repositories,
        started: &started,
        exporting,
    };
    let mut reports = Vec::new();
    for arm in args.arms.passes() {
        reports.push(pass(&setup, *arm).await?);
    }

    let path = report_path(args, &started);
    let (json, table) = if args.arms == Arms::Both {
        let agent = reports.pop().ok_or("the agent pass did not run")?;
        let legacy = reports.pop().ok_or("the legacy pass did not run")?;
        let paired = PairedReport::new(legacy, agent)?.under(criteria);
        let out = (
            serde_json::to_string_pretty(&paired).map_err(|e| e.to_string())?,
            paired.table(),
        );
        reports = vec![paired.legacy, paired.agent];
        out
    } else {
        let report = reports.first().ok_or("no pass ran")?;
        (
            serde_json::to_string_pretty(report).map_err(|e| e.to_string())?,
            report.table(),
        )
    };
    write(&path, &format!("{json}\n"))?;
    println!("\n{table}");
    println!("report: {}", path.display());
    for line in eval::retried_summary(&reports) {
        println!("{line}");
    }
    if let Some(line) = eval::left_out(&set, &cases) {
        println!("{line}");
    }
    for report in &reports {
        for line in &report.incomplete {
            eprintln!("incomplete: {}: {line}", report.arm);
        }
    }

    if reports.iter().all(Report::is_complete) {
        Ok(())
    } else {
        Err("the run is incomplete".to_owned())
    }
}
