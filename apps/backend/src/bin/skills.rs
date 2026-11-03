//! The internal skill commands: validate the repository's agent skills, publish them into the
//! durable skill registry, activate (or roll back) a version, and list the registry. They go
//! over the Restate ingress and have no `/api` surface.
//!
//! ```text
//! cargo run -q -p ekg-backend --bin skills -- validate [dir]
//! cargo run -q -p ekg-backend --bin skills -- publish [dir]
//! cargo run -q -p ekg-backend --bin skills -- activate <skill> <version> --reason <text>
//! cargo run -q -p ekg-backend --bin skills -- list
//! ```
//!
//! `[dir]` defaults to the repository's `apps/hayhooks/skills`. Skills are validated against
//! the engine's tool profiles ([`propose::tool_profiles`]), built here over a world that is
//! never read: tool names do not depend on the world. A version may be named by a unique
//! prefix of at least 8 characters. Who activates is `EKG_OPERATOR`, else
//! `git config user.name`, else `$USER`. The ingress is `RESTATE_INGRESS_URL` (default
//! `http://localhost:8080`). The `just` recipes `skills-validate`, `skills-publish`,
//! `skill-activate` and `skills-list` run these.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_backend::propose;
use ekg_backend::skills::{ActivateRequest, RegistryClient, SkillIndex};
use ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS;
use ekg_cognition::skills::{self, ProfileTools};
use ekg_domain::{
    EngineError, EntityUri, Grounding, ReadContext, Result, Revision, SourceRecord, WorldModel,
};

const USAGE: &str = "usage: skills validate [dir] | publish [dir] | \
                     activate <skill> <version> --reason <text> | list";

/// The repository's skills, next to the runtime that loads them.
const DEFAULT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../hayhooks/skills");

/// A world model that is never read: the profiles are built only for their tool names.
struct NoWorld;

#[async_trait]
impl WorldModel for NoWorld {
    async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
        Err(never())
    }
    async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
        Err(never())
    }
    async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
        Err(never())
    }
    async fn select(&self, _sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
        Err(never())
    }
}

fn never() -> EngineError {
    EngineError::Backend("the skills tool reads no world model".to_owned())
}

/// The tools each of the engine's profiles offers.
fn profiles() -> std::result::Result<ProfileTools, String> {
    let world: Arc<dyn WorldModel> = Arc::new(NoWorld);
    propose::tool_profiles(&world, DEFAULT_MAX_PLAN_ACTIONS)
        .map(|registry| registry.offered_tools())
        .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("skills: {why}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Vec<String>) -> std::result::Result<(), String> {
    let mut args = args.into_iter();
    let command = args.next().ok_or(USAGE)?;
    let rest: Vec<String> = args.collect();
    let dir = |rest: &[String]| match rest {
        [] => {
            let default = PathBuf::from(DEFAULT_DIR);
            Ok(default.canonicalize().unwrap_or(default))
        }
        [dir] => Ok(PathBuf::from(dir)),
        _ => Err(USAGE.to_owned()),
    };
    match command.as_str() {
        "validate" => validate(&dir(&rest)?).map(|_| ()),
        "publish" => publish(&dir(&rest)?).await,
        "activate" => match rest.as_slice() {
            [skill, version, flag, reason] if flag == "--reason" => {
                activate(skill, version, reason).await
            }
            _ => Err(USAGE.to_owned()),
        },
        "list" if rest.is_empty() => list().await,
        _ => Err(USAGE.to_owned()),
    }
}

fn client() -> RegistryClient {
    let ingress = std::env::var("RESTATE_INGRESS_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:8080".to_owned());
    RegistryClient::new(reqwest::Client::new(), ingress)
}

/// The first 12 characters of a version, for display.
fn short(version: &str) -> &str {
    version.get(..12).unwrap_or(version)
}

/// Read and validate every skill under `dir` against the engine's profiles.
fn validate(dir: &Path) -> std::result::Result<Vec<skills::SkillBundle>, String> {
    let profiles = profiles()?;
    match skills::read_and_validate_all(dir, &profiles) {
        Ok(bundles) => {
            for bundle in &bundles {
                println!(
                    "✓ {}  {}  profiles [{}]  tools [{}]",
                    bundle.name(),
                    short(&bundle.digest()),
                    bundle.profiles().join(", "),
                    bundle.tools().join(", ")
                );
            }
            println!("{} skill(s) valid in {}", bundles.len(), dir.display());
            Ok(bundles)
        }
        Err(errors) => {
            for error in &errors {
                eprintln!("✗ {error}");
            }
            Err(format!(
                "{} invalid skill(s) in {}",
                errors.len(),
                dir.display()
            ))
        }
    }
}

/// Validate, then publish every skill; a new version of a known skill stays inactive.
async fn publish(dir: &Path) -> std::result::Result<(), String> {
    let bundles = validate(dir)?;
    let client = client();
    for bundle in &bundles {
        let outcome = client
            .publish(bundle)
            .await
            .map_err(|e| format!("{}: {e}", bundle.name()))?;
        let state = if outcome.created { "new" } else { "unchanged" };
        let active = if outcome.active == outcome.version {
            "active".to_owned()
        } else {
            format!("inactive (active: {})", short(&outcome.active))
        };
        println!(
            "- {}  {}  {state}, {active}",
            outcome.skill,
            short(&outcome.version)
        );
    }
    Ok(())
}

/// Activate a registered version (an earlier one is the rollback).
async fn activate(skill: &str, version: &str, reason: &str) -> std::result::Result<(), String> {
    let by = ekg_backend::operator::operator()?;
    let client = client();
    let index = client.index().await.map_err(|e| e.to_string())?;
    let entry = index
        .get(skill)
        .ok_or_else(|| format!("unknown skill {skill:?}"))?;
    let version = skills::resolve_version(
        entry.versions.iter().map(|info| info.version.as_str()),
        version,
    )
    .map_err(|why| format!("skill {skill:?}: {why}"))?;
    let outcome = client
        .activate(&ActivateRequest {
            skill: skill.to_owned(),
            version: version.to_owned(),
            by: by.clone(),
            reason: reason.to_owned(),
        })
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "✓ {} {} active (was {}), by {by}: {}",
        outcome.skill,
        short(&outcome.version),
        short(&outcome.previous),
        reason.trim()
    );
    Ok(())
}

/// Print every skill's versions, active version and history.
async fn list() -> std::result::Result<(), String> {
    let index: SkillIndex = client().index().await.map_err(|e| e.to_string())?;
    if index.is_empty() {
        println!("no skills published");
    }
    for (name, entry) in &index {
        println!("{name}  (active {})", short(&entry.active));
        println!("  versions:");
        for info in &entry.versions {
            let mark = if info.version == entry.active {
                "*"
            } else {
                " "
            };
            println!(
                "   {mark} {}  {}  profiles [{}]  tools [{}]",
                short(&info.version),
                ekg_kinetic::verbs::epoch_to_odoo_datetime(info.published_at),
                info.profiles.join(", "),
                info.tools.join(", ")
            );
        }
        println!("  history:");
        for activation in &entry.history {
            println!(
                "    {}  {} (was {})  by {}: {}",
                ekg_kinetic::verbs::epoch_to_odoo_datetime(activation.at),
                short(&activation.version),
                activation.previous.as_deref().map_or("-", short),
                activation.by,
                activation.reason
            );
        }
    }
    Ok(())
}
