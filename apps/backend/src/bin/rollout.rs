//! The internal rollout commands: switch the agent's governed rollout mode and read it, over
//! the Restate ingress, with no `/api` surface.
//!
//! ```text
//! cargo run -q -p ekg-backend --bin rollout -- mode shadow --until <when> --max-slates <n> --max-tokens <n> --reason <text>
//! cargo run -q -p ekg-backend --bin rollout -- mode agent --until <when> --max-slates <n> --max-tokens <n> --reason <text>
//! cargo run -q -p ekg-backend --bin rollout -- mode legacy --reason <text>
//! cargo run -q -p ekg-backend --bin rollout -- status
//! cargo run -q -p ekg-backend --bin rollout -- shadow-report [--json <path>]
//! cargo run -q -p ekg-backend --bin rollout -- criteria register [--file <path>] --reason <text>
//! cargo run -q -p ekg-backend --bin rollout -- criteria show
//! cargo run -q -p ekg-backend --bin rollout -- verdict --eval <paired report> [--json <path>]
//! cargo run -q -p ekg-backend --bin rollout -- signoff --eval <paired report> --reason <text>
//! ```
//!
//! `<when>` is a UTC date or date-time (`2027-10-14`, `2027-10-14T18:00:00`) or a span from now
//! (`+90m`, `+8h`, `+2d`). Setting `legacy` is the kill switch. Who switches is `EKG_OPERATOR`,
//! else `git config user.name`, else `$USER`; it is recorded with the reason in the mode's
//! history. The ingress is `RESTATE_INGRESS_URL` (default `http://localhost:8080`). Shadow
//! records are read with Restate's own state introspection (`just agent-shadow-list`). The
//! `just` recipes `agent-mode` and `agent-status` run these.
//!
//! `shadow-report` sets every shadow record's slate beside the slate its point got and the
//! decision on that point, and totals what it found. It lists the subjects through the Restate
//! admin endpoint (`RESTATE_ADMIN_URL`, default `http://localhost:9070`), reads over the
//! ingress, and writes nothing but its own output: the table, and the same as JSON to
//! `--json <path>`. `just agent-shadow-report` runs it.
//!
//! `criteria register` records the promotion criteria document (the eval set's `promotion.yaml`
//! unless `--file` names another) with the rollout, with who and why; a `shadow` window opened
//! afterwards is tied to it. `verdict` checks the registered criteria against a paired eval
//! report and the shadow records that count, prints one line per criterion, and exits non-zero
//! when they are not met. It reads only, and it decides nothing. `just agent-criteria-register`,
//! `just agent-criteria` and `just agent-promotion-verdict` run these.
//!
//! `signoff` computes the same verdict and records it with the rollout as the operator's
//! sign-off: the verdict, the digests of the criteria and of the report file, the version the
//! evidence is of, who signs and why. The rollout refuses it when the verdict is not met, when
//! it is for other criteria than the active ones, or when the signer registered those criteria.
//! It switches nothing: `mode agent` is a separate command, refused without a sign-off for the
//! active criteria. `just agent-signoff` runs it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ekg_backend::operator::operator;
use ekg_backend::promotion::{self, PROMOTION_FILE, SetFacts};
use ekg_backend::rollout::{
    Mode, RegisterRequest, Registration, RolloutClient, RolloutState, SetModeRequest, Signoff,
    SignoffRequest,
};
use ekg_backend::shadow_report;
use ekg_cognition::eval::{EvalSet, Lock, PairedReport, digest};
use ekg_kinetic::verbs::{epoch_to_odoo_datetime, odoo_datetime_to_epoch};

const USAGE: &str = "usage: rollout mode shadow|agent --until <when> --max-slates <n> \
                     --max-tokens <n> --reason <text> | mode legacy --reason <text> | status | \
                     shadow-report [--json <path>] | criteria register [--file <path>] \
                     --reason <text> | criteria show | verdict --eval <paired report> \
                     [--json <path>] | signoff --eval <paired report> --reason <text>";

/// The repository's eval set, where the criteria document lives.
const DEFAULT_SET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../evals/agent");

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("rollout: {why}");
            ExitCode::FAILURE
        }
    }
}

/// A URL setting, or `default` when it is unset or blank.
fn url(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn ingress() -> String {
    url("RESTATE_INGRESS_URL", "http://localhost:8080")
}

fn client() -> RolloutClient {
    RolloutClient::new(reqwest::Client::new(), ingress())
}

/// Where `shadow-report` also writes its JSON, from the arguments after the command.
fn report_json(rest: &[String]) -> Result<Option<PathBuf>, String> {
    match rest {
        [] => Ok(None),
        [flag, path] if flag == "--json" => Ok(Some(PathBuf::from(path))),
        _ => Err(USAGE.to_owned()),
    }
}

async fn shadow_report(json: Option<PathBuf>) -> Result<(), String> {
    let admin = url("RESTATE_ADMIN_URL", "http://localhost:9070");
    let report = shadow_report::collect(&reqwest::Client::new(), &admin, &ingress()).await?;
    print!("{}", report.table());
    if let Some(path) = json {
        let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|why| format!("{}: {why}", path.display()))?;
        println!("json: {}", path.display());
    }
    Ok(())
}

/// Seconds since the Unix epoch.
fn now() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|e| format!("system clock: {e}"))
}

/// A window end: a UTC date or date-time, or `+<n>m|h|d` from `now`.
fn parse_until(value: &str, now: u64) -> Result<u64, String> {
    let bad = || format!("--until {value:?} is not a UTC date(-time) or +<n>m|h|d");
    if let Some(span) = value.strip_prefix('+') {
        let (count, unit) = span.split_at(span.len().saturating_sub(1));
        let count: u64 = count.parse().map_err(|_| bad())?;
        let secs = match unit {
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => return Err(bad()),
        };
        return Ok(now.saturating_add(count.saturating_mul(secs)));
    }
    let canonical = ekg_kinetic::canonical_datetime(value).ok_or_else(bad)?;
    Ok(odoo_datetime_to_epoch(&canonical))
}

/// The flags after a mode, as `--name value` pairs.
fn flags(rest: &[String]) -> Result<Vec<(&str, &str)>, String> {
    if !rest.len().is_multiple_of(2) {
        return Err(USAGE.to_owned());
    }
    rest.chunks(2)
        .map(|pair| match pair {
            [name, value] if name.starts_with("--") => Ok((name.as_str(), value.as_str())),
            _ => Err(USAGE.to_owned()),
        })
        .collect()
}

fn flag<'a>(flags: &[(&str, &'a str)], name: &str) -> Result<&'a str, String> {
    flags
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
        .ok_or_else(|| format!("missing {name}; {USAGE}"))
}

/// The mode request the arguments after `mode` name, at `now`, by `by`.
fn mode_request(args: &[String], now: u64, by: String) -> Result<SetModeRequest, String> {
    let (mode, rest) = args.split_first().ok_or(USAGE)?;
    let flags = flags(rest)?;
    let known: &[&str] = match mode.as_str() {
        "shadow" | "agent" => &["--until", "--max-slates", "--max-tokens", "--reason"],
        "legacy" => &["--reason"],
        _ => return Err(USAGE.to_owned()),
    };
    if let Some((unknown, _)) = flags.iter().find(|(name, _)| !known.contains(name)) {
        return Err(format!("{unknown} does not apply to `{mode}`; {USAGE}"));
    }
    let bounds = || -> Result<(u64, u32, u64), String> {
        Ok((
            parse_until(flag(&flags, "--until")?, now)?,
            flag(&flags, "--max-slates")?
                .parse()
                .map_err(|_| "--max-slates must be a whole number".to_owned())?,
            flag(&flags, "--max-tokens")?
                .parse()
                .map_err(|_| "--max-tokens must be a whole number".to_owned())?,
        ))
    };
    let mode = match mode.as_str() {
        "shadow" => {
            let (until, max_slates, max_tokens) = bounds()?;
            Mode::Shadow {
                until,
                max_slates,
                max_tokens,
            }
        }
        "agent" => {
            let (until, max_slates, max_tokens) = bounds()?;
            Mode::Agent {
                until,
                max_slates,
                max_tokens,
            }
        }
        _ => Mode::Legacy,
    };
    Ok(SetModeRequest {
        mode,
        by,
        reason: flag(&flags, "--reason")?.to_owned(),
    })
}

/// What `criteria register` was asked for: the document's file and the reason.
fn register_args(rest: &[String]) -> Result<(PathBuf, String), String> {
    let flags = flags(rest)?;
    if let Some((unknown, _)) = flags
        .iter()
        .find(|(name, _)| !["--file", "--reason"].contains(name))
    {
        return Err(format!(
            "{unknown} does not apply to `criteria register`; {USAGE}"
        ));
    }
    let file = flags
        .iter()
        .find(|(name, _)| *name == "--file")
        .map_or_else(
            || Path::new(DEFAULT_SET).join(PROMOTION_FILE),
            |(_, path)| PathBuf::from(path),
        );
    Ok((file, flag(&flags, "--reason")?.to_owned()))
}

/// What `verdict` was asked for: the paired report, and where the JSON also goes.
fn verdict_args(rest: &[String]) -> Result<(PathBuf, Option<PathBuf>), String> {
    let flags = flags(rest)?;
    if let Some((unknown, _)) = flags
        .iter()
        .find(|(name, _)| !["--eval", "--json"].contains(name))
    {
        return Err(format!("{unknown} does not apply to `verdict`; {USAGE}"));
    }
    let json = flags
        .iter()
        .find(|(name, _)| *name == "--json")
        .map(|(_, path)| PathBuf::from(path));
    Ok((PathBuf::from(flag(&flags, "--eval")?), json))
}

/// What `signoff` was asked for: the paired report and the reason.
fn signoff_args(rest: &[String]) -> Result<(PathBuf, String), String> {
    let flags = flags(rest)?;
    if let Some((unknown, _)) = flags
        .iter()
        .find(|(name, _)| !["--eval", "--reason"].contains(name))
    {
        return Err(format!("{unknown} does not apply to `signoff`; {USAGE}"));
    }
    Ok((
        PathBuf::from(flag(&flags, "--eval")?),
        flag(&flags, "--reason")?.to_owned(),
    ))
}

fn read_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|why| format!("{}: {why}", path.display()))
}

fn print_registration(registration: &Registration) {
    println!(
        "criteria: {}  registered {} UTC by {}: {}",
        registration.digest,
        epoch_to_odoo_datetime(registration.at),
        registration.by,
        registration.reason
    );
}

async fn register(file: &Path, reason: String) -> Result<(), String> {
    let request = RegisterRequest {
        document: read_file(file)?,
        by: operator()?,
        reason,
    };
    let state = client()
        .register_criteria(&request)
        .await
        .map_err(|e| e.to_string())?;
    match state.active() {
        Some(active) => print_registration(active),
        None => println!("criteria: (none registered)"),
    }
    Ok(())
}

async fn show_criteria() -> Result<(), String> {
    let state = client().get().await.map_err(|e| e.to_string())?;
    let Some(active) = state.active() else {
        println!("criteria: (none registered)");
        return Ok(());
    };
    print_registration(active);
    let document = serde_saphyr::to_string(&active.criteria).map_err(|e| e.to_string())?;
    print!("{document}");
    if state.registrations.len() > 1 {
        println!("earlier registrations:");
        for earlier in state.registrations.iter().rev().skip(1) {
            println!(
                "  {}  {} UTC by {}: {}",
                earlier.digest,
                epoch_to_odoo_datetime(earlier.at),
                earlier.by,
                earlier.reason
            );
        }
    }
    Ok(())
}

/// The digest that identifies a paired report file's text.
fn report_digest(text: &str) -> String {
    digest(&std::collections::BTreeMap::from([(
        "paired-report".to_owned(),
        text.to_owned(),
    )]))
}

/// The verdict on the registered criteria against the paired report at `eval` and the shadow
/// records that count, with the digest of the report file.
async fn judge(
    eval: &Path,
    ingress: &str,
    admin: &str,
) -> Result<(promotion::Verdict, String), String> {
    let text = read_file(eval)?;
    let paired: PairedReport = serde_json::from_str(&text)
        .map_err(|why| format!("{}: not a paired report: {why}", eval.display()))?;
    let set = EvalSet::read_dir(Path::new(DEFAULT_SET)).map_err(|e| e.to_string())?;
    let set = SetFacts {
        digest: Lock::of(&set).digest,
        case_ids: set.cases.iter().map(|case| case.id.clone()).collect(),
    };
    let http = reqwest::Client::new();
    let state = RolloutClient::new(http.clone(), ingress)
        .get()
        .await
        .map_err(|why| format!("the rollout at {ingress}: {why}"))?;
    let subjects = shadow_report::subjects(&http, admin).await?;
    let (records, points) = shadow_report::read(&http, ingress, &subjects).await?;
    let verdict = promotion::verdict(&state, &set, Some(&paired), &records, &points, now()?);
    Ok((verdict, report_digest(&text)))
}

async fn verdict(
    eval: &Path,
    json: Option<PathBuf>,
    ingress: &str,
    admin: &str,
) -> Result<bool, String> {
    let (verdict, _) = judge(eval, ingress, admin).await?;
    print!("{}", verdict.table());
    if let Some(path) = json {
        let text = serde_json::to_string_pretty(&verdict).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|why| format!("{}: {why}", path.display()))?;
        println!("json: {}", path.display());
    }
    Ok(verdict.met)
}

fn print_signoff(signoff: &Signoff) {
    println!(
        "  {}  by {}: {}  [criteria {}, report {}, engine {}, model {}]",
        epoch_to_odoo_datetime(signoff.at),
        signoff.by,
        signoff.reason,
        signoff.criteria,
        signoff.report,
        signoff.version.engine,
        signoff.version.model
    );
}

/// Computes the verdict and records it as the operator's sign-off. The rollout refuses an
/// unmet verdict, so there is nothing to override here.
async fn signoff(eval: &Path, reason: String, ingress: &str, admin: &str) -> Result<(), String> {
    let (verdict, report) = judge(eval, ingress, admin).await?;
    print!("{}", verdict.table());
    let request = SignoffRequest {
        verdict,
        report,
        by: operator()?,
        reason,
    };
    let state = RolloutClient::new(reqwest::Client::new(), ingress)
        .sign_off(&request)
        .await
        .map_err(|why| format!("the sign-off: {why}"))?;
    println!("signed off:");
    if let Some(signoff) = state.signoffs.last() {
        print_signoff(signoff);
    }
    Ok(())
}

fn describe(mode: &Mode) -> String {
    match mode {
        Mode::Legacy => "legacy".to_owned(),
        Mode::Shadow {
            until,
            max_slates,
            max_tokens,
        }
        | Mode::Agent {
            until,
            max_slates,
            max_tokens,
        } => format!(
            "{} until {} UTC, at most {max_slates} slate(s) and {max_tokens} token(s)",
            mode.name(),
            epoch_to_odoo_datetime(*until)
        ),
    }
}

/// What a history entry was entered under: its criteria, and the sign-off of an `agent` entry.
fn entered_under(criteria: Option<&String>, signoff: Option<u64>) -> String {
    let mut under = Vec::new();
    if let Some(digest) = criteria {
        under.push(format!("criteria {digest}"));
    }
    if let Some(at) = signoff {
        under.push(format!("sign-off of {} UTC", epoch_to_odoo_datetime(at)));
    }
    if under.is_empty() {
        String::new()
    } else {
        format!("  [{}]", under.join(", "))
    }
}

fn print_state(state: &RolloutState) {
    println!("mode: {}", describe(&state.mode));
    if !matches!(state.mode, Mode::Legacy) {
        println!(
            "used: {} slate(s), {} token(s)",
            state.slates_started, state.tokens_used
        );
    }
    if matches!(state.mode, Mode::Agent { .. }) {
        println!(
            "since entered: {} agent run(s) failed in a row, {} promotion(s) reversed",
            state.agent_failures, state.reversed_promotions
        );
    }
    if let Some(stopped) = state.stopped {
        println!(
            "stopped: {}",
            serde_json::to_value(stopped)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        );
    }
    match state.active() {
        Some(active) => print_registration(active),
        None => println!("criteria: (none registered)"),
    }
    if state.signoffs.is_empty() {
        println!("sign-offs: (none)");
    } else {
        println!("sign-offs:");
        state.signoffs.iter().for_each(print_signoff);
    }
    if state.history.is_empty() {
        println!("history: (none, never set; the default is legacy)");
    } else {
        println!("history:");
        for change in &state.history {
            println!(
                "  {}  {}  by {}: {}{}",
                epoch_to_odoo_datetime(change.at),
                describe(&change.mode),
                change.by,
                change.reason,
                entered_under(change.criteria.as_ref(), change.signoff)
            );
        }
    }
}

async fn run(args: Vec<String>) -> Result<(), String> {
    match args.split_first() {
        Some((command, rest)) if command == "mode" => {
            let request = mode_request(rest, now()?, operator()?)?;
            let state = client()
                .set_mode(&request)
                .await
                .map_err(|e| e.to_string())?;
            print_state(&state);
            Ok(())
        }
        Some((command, rest)) if command == "status" && rest.is_empty() => {
            print_state(&client().get().await.map_err(|e| e.to_string())?);
            Ok(())
        }
        Some((command, rest)) if command == "shadow-report" => {
            shadow_report(report_json(rest)?).await
        }
        Some((command, rest)) if command == "criteria" => match rest.split_first() {
            Some((action, rest)) if action == "register" => {
                let (file, reason) = register_args(rest)?;
                register(&file, reason).await
            }
            Some((action, [])) if action == "show" => show_criteria().await,
            _ => Err(USAGE.to_owned()),
        },
        Some((command, rest)) if command == "verdict" => {
            let (eval, json) = verdict_args(rest)?;
            let admin = url("RESTATE_ADMIN_URL", "http://localhost:9070");
            if verdict(&eval, json, &ingress(), &admin).await? {
                Ok(())
            } else {
                Err("the registered criteria are not met".to_owned())
            }
        }
        Some((command, rest)) if command == "signoff" => {
            let (eval, reason) = signoff_args(rest)?;
            let admin = url("RESTATE_ADMIN_URL", "http://localhost:9070");
            signoff(&eval, reason, &ingress(), &admin).await
        }
        _ => Err(USAGE.to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn a_window_end_is_a_utc_date_time_or_a_span_from_now() {
        assert_eq!(parse_until("+90m", NOW), Ok(NOW + 5_400));
        assert_eq!(parse_until("+8h", NOW), Ok(NOW + 28_800));
        assert_eq!(parse_until("+2d", NOW), Ok(NOW + 172_800));
        assert_eq!(
            parse_until("2027-10-14T18:00:00", NOW),
            Ok(odoo_datetime_to_epoch("2027-10-14 18:00:00"))
        );
        assert_eq!(
            parse_until("2027-10-14", NOW),
            Ok(odoo_datetime_to_epoch("2027-10-14 00:00:00"))
        );
        for bad in ["+8w", "+h", "tomorrow", "2027-02-30"] {
            assert!(parse_until(bad, NOW).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_shadow_report_takes_an_optional_json_path_and_nothing_else() {
        assert_eq!(report_json(&[]), Ok(None));
        assert_eq!(
            report_json(&args("--json out/report.json")),
            Ok(Some(PathBuf::from("out/report.json")))
        );
        for bad in ["--json", "--csv x", "--json a b", "x"] {
            assert_eq!(report_json(&args(bad)), Err(USAGE.to_owned()), "{bad}");
        }
    }

    #[tokio::test]
    async fn the_shadow_report_fails_on_an_unreachable_admin_endpoint_naming_it() {
        let why = shadow_report::collect(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
        )
        .await
        .expect_err("nothing listens there");
        assert!(why.contains("http://127.0.0.1:1/query"), "{why}");
    }

    #[test]
    fn a_registration_names_its_reason_and_defaults_to_the_sets_document() {
        let (file, reason) = register_args(&args("--reason before-the-window")).expect("parsed");
        assert!(file.ends_with("evals/agent/promotion.yaml"), "{file:?}");
        assert_eq!(reason, "before-the-window");
        let (file, _) = register_args(&args("--file other.yaml --reason x")).expect("parsed");
        assert_eq!(file, PathBuf::from("other.yaml"));
        let why = register_args(&args("--file other.yaml")).expect_err("no reason");
        assert!(why.contains("missing --reason"), "{why}");
        let why = register_args(&args("--reason x --force yes")).expect_err("unknown flag");
        assert!(why.contains("--force does not apply"), "{why}");
    }

    #[test]
    fn a_verdict_needs_a_paired_report_and_takes_an_optional_json_path() {
        assert_eq!(
            verdict_args(&args("--eval report.json")),
            Ok((PathBuf::from("report.json"), None))
        );
        assert_eq!(
            verdict_args(&args("--json out.json --eval report.json")),
            Ok((
                PathBuf::from("report.json"),
                Some(PathBuf::from("out.json"))
            ))
        );
        let why = verdict_args(&args("--json out.json")).expect_err("no report");
        assert!(why.contains("missing --eval"), "{why}");
        let why = verdict_args(&args("--eval r.json --gate yes")).expect_err("unknown flag");
        assert!(why.contains("--gate does not apply"), "{why}");
    }

    #[tokio::test]
    async fn a_verdict_refuses_another_file_and_names_an_unreachable_ingress() {
        use ekg_cognition::eval::{Report, VersionTuple};

        let dir = std::env::temp_dir().join(format!("ekg-verdict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let nowhere = "http://127.0.0.1:1";
        let other = dir.join("other.json");
        std::fs::write(&other, "{}").expect("write");
        let why = verdict(&other, None, nowhere, nowhere)
            .await
            .expect_err("not a paired report");
        assert!(why.contains("not a paired report"), "{why}");

        let arm = |name: &str| {
            let version = VersionTuple {
                engine: String::new(),
                model: String::new(),
                skills: std::collections::BTreeMap::new(),
                tools: std::collections::BTreeMap::new(),
                set_version: 1,
                set_digest: String::new(),
                trials: 1,
                max_steps: 0,
                joint_max_steps: 0,
                analyst_max_steps: 0,
                planners: 0,
                planner_reads: false,
            };
            Report::new(name, version, Vec::new(), 0)
        };
        let paired = PairedReport::new(arm("legacy"), arm("agent")).expect("paired");
        let report = dir.join("paired.json");
        std::fs::write(&report, serde_json::to_string(&paired).expect("json")).expect("write");
        let why = verdict(&report, None, nowhere, nowhere)
            .await
            .expect_err("nothing listens there");
        assert!(why.contains("the rollout at http://127.0.0.1:1"), "{why}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_mode_arguments_parse_into_a_request() {
        let shadow = mode_request(
            &args("shadow --until +1h --max-slates 3 --max-tokens 50000 --reason evidence"),
            NOW,
            "ops".to_owned(),
        )
        .expect("shadow");
        assert_eq!(
            shadow,
            SetModeRequest {
                mode: Mode::Shadow {
                    until: NOW + 3_600,
                    max_slates: 3,
                    max_tokens: 50_000
                },
                by: "ops".to_owned(),
                reason: "evidence".to_owned(),
            }
        );
        let legacy =
            mode_request(&args("legacy --reason stop"), NOW, "ops".to_owned()).expect("legacy");
        assert_eq!(legacy.mode, Mode::Legacy);
        let agent = mode_request(
            &args("agent --until +2d --max-slates 40 --max-tokens 900000 --reason signed"),
            NOW,
            "ops".to_owned(),
        )
        .expect("agent");
        assert_eq!(
            agent,
            SetModeRequest {
                mode: Mode::Agent {
                    until: NOW + 172_800,
                    max_slates: 40,
                    max_tokens: 900_000
                },
                by: "ops".to_owned(),
                reason: "signed".to_owned(),
            }
        );
        for bad in [
            "shadow --until +1h --max-slates 3 --reason x",
            "legacy --reason stop --max-slates 3",
            "legacy",
            "agent --reason x",
            "agent --until +1h --max-slates 3 --reason x",
            "agent --until +1h --max-tokens 3 --reason x",
            "agent --max-slates 3 --max-tokens 3 --reason x",
            "agent --until +1h --max-slates 3 --max-tokens 3",
            "agent --until +1h --max-slates 3 --max-tokens 3 --reason x --force yes",
            "authoritative --reason x",
        ] {
            assert!(
                mode_request(&args(bad), NOW, "ops".to_owned()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_sign_off_needs_a_paired_report_and_a_reason_and_takes_nothing_else() {
        assert_eq!(
            signoff_args(&args("--eval report.json --reason holds")),
            Ok((PathBuf::from("report.json"), "holds".to_owned()))
        );
        let why = signoff_args(&args("--reason holds")).expect_err("no report");
        assert!(why.contains("missing --eval"), "{why}");
        let why = signoff_args(&args("--eval report.json")).expect_err("no reason");
        assert!(why.contains("missing --reason"), "{why}");
        // There is no way to sign off an unmet verdict.
        for flag in ["--force yes", "--override yes", "--json out.json"] {
            let line = format!("--eval report.json --reason holds {flag}");
            let why = signoff_args(&args(&line)).expect_err("unknown flag");
            assert!(why.contains("does not apply to `signoff`"), "{why}");
        }
    }

    #[test]
    fn a_report_files_digest_is_stable_and_changes_with_its_text() {
        let digest = report_digest("{\"cases\": []}");
        assert_eq!(digest, report_digest("{\"cases\": []}"));
        assert_eq!(digest.len(), 64);
        assert_ne!(digest, report_digest("{\"cases\": [1]}"));
    }

    #[test]
    fn a_history_entry_names_the_criteria_and_the_sign_off_it_was_entered_under() {
        assert_eq!(entered_under(None, None), "");
        let digest = "abc".to_owned();
        assert_eq!(entered_under(Some(&digest), None), "  [criteria abc]");
        let both = entered_under(
            Some(&digest),
            Some(odoo_datetime_to_epoch("2027-10-14 18:00:00")),
        );
        assert_eq!(
            both,
            "  [criteria abc, sign-off of 2027-10-14 18:00:00 UTC]"
        );
        let agent = describe(&Mode::Agent {
            until: odoo_datetime_to_epoch("2027-10-15 00:00:00"),
            max_slates: 4,
            max_tokens: 9,
        });
        assert_eq!(
            agent,
            "agent until 2027-10-15 00:00:00 UTC, at most 4 slate(s) and 9 token(s)"
        );
    }
}
