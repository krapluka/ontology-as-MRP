//! `ekg-transport-sim`: the transport/ETA shipment simulator.
//!
//! An on-demand operator tool, never run by default. It slips the `scheduled_date` of
//! the seeded brake-module inbound shipment in Odoo past the customer promise, then
//! fires the ingest webhook so the durable worker fat-pulls and re-materialises it. The
//! risk rule then uses the late shipment ETA in place of the PO's planned date, and the
//! CityLogix "Cargo Trike" order flips to at risk.
//!
//! Requires the local substrate up (`just podman-local-up`) and a backend running
//! (containerized via `just podman-local-up`, or host `just backend`). Config comes
//! from the repo-root `.env` with local defaults.
//!
//! Usage:
//!   cargo run -p ekg-transport-sim -- [--slip <days>] [--on-time]
//!   just transport-sim [--slip <days>] [--on-time]
//!     --slip <days>   how far past today to push the ETA (default 60, well after the
//!                     3-week promise, so at risk).
//!     --on-time       restore an on-time ETA (today+7) instead of slipping; undoes a
//!                     previous slip so the order returns to covered.

// The docs name products and Odoo models (Odoo, Cargo Trike, stock.picking).
#![allow(clippy::doc_markdown)]

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use odoo_client::Odoo;
use serde_json::json;

/// The seeded external id of the brake-module inbound purchase order (from
/// `ekg-odoo-seed`); its generated `stock.picking` is the shipment we slip.
const SEED_PO_XMLID: (&str, &str) = ("ekg_seed", "po_brake_inbound");
/// On-time restore offset in days past today. Matches the seed's `transport_eta_ok`.
const ON_TIME_DAYS: i64 = 7;
/// Default slip offset in days past today, well beyond the 3-week promise.
const DEFAULT_SLIP_DAYS: i64 = 60;

/// Which ETA to write.
enum Mode {
    /// Slip the ETA `days` past today (order flips to at risk).
    Slip(i64),
    /// Restore an on-time ETA (order returns to covered).
    OnTime,
}

fn main() -> Result<()> {
    // `parse_args` returns None when --help was handled; exit 0 in that case.
    let Some(mode) = parse_args()? else {
        return Ok(());
    };

    let dotenv = load_dotenv();
    let odoo_url = cfg(&dotenv, "ODOO_URL", "http://localhost:8069");
    let odoo_db = cfg(&dotenv, "ODOO_DB", "ekg");
    let odoo_user = cfg(&dotenv, "ODOO_USER", "admin");
    let odoo_api_key = cfg(&dotenv, "ODOO_API_KEY", "");
    let backend_url = cfg(&dotenv, "EKG_BACKEND_URL", "http://localhost:8090");
    let webhook_secret = cfg(&dotenv, "EKG_WEBHOOK_SECRET", "dev-secret");

    if odoo_api_key.is_empty() {
        bail!("ODOO_API_KEY is empty (run 'just podman-local-up' to mint one)");
    }

    let offset_days = match mode {
        Mode::OnTime => ON_TIME_DAYS,
        Mode::Slip(days) => days,
    };
    let new_eta = ymd_plus(offset_days)?;

    let odoo = Odoo::connect(&odoo_url, &odoo_db, &odoo_user, &odoo_api_key)
        .with_context(|| format!("connecting to Odoo at {odoo_url} (db={odoo_db})"))?;

    let (module, name) = SEED_PO_XMLID;
    println!("resolving the seeded brake inbound PO ({module}.{name})...");
    let po_id = odoo.xmlid(module, name).with_context(|| {
        format!("brake inbound PO not found ({module}.{name}): run 'just seed' first")
    })?;

    println!("finding the inbound shipment (stock.picking) Odoo generated for PO {po_id}...");
    let picking_id = *odoo
        .search("stock.picking", json!([["purchase_id", "=", po_id]]))
        .context("searching for the inbound picking")?
        .first()
        .with_context(|| format!("no inbound picking for PO {po_id}: re-run 'just seed'"))?;

    println!("setting shipment #{picking_id} ETA (scheduled_date) to {new_eta}...");
    odoo.write(
        "stock.picking",
        json!([picking_id]),
        json!({ "scheduled_date": new_eta }),
    )
    .context("writing the shipment scheduled_date")?;
    println!("  ✓ ETA updated in Odoo");

    println!("firing the ingest webhook so the world model re-materialises the shipment...");
    fire_webhook(&backend_url, &webhook_secret, picking_id)?;
    println!("  ✓ webhook accepted (202); the monitor will re-evaluate the promise");

    match mode {
        Mode::OnTime => println!(
            "✓ restored an on-time ETA ({new_eta}); the Cargo Trike order returns to covered."
        ),
        Mode::Slip(_) => println!(
            "✓ slipped the shipment ETA to {new_eta} (past the promise); the Cargo Trike order \
             should open an action point. Inspect: GET {}/api/action-points",
            backend_url.trim_end_matches('/')
        ),
    }
    Ok(())
}

/// Parse CLI args. Returns `Ok(None)` when `--help` was handled (caller exits 0).
fn parse_args() -> Result<Option<Mode>> {
    let mut slip_days = DEFAULT_SLIP_DAYS;
    let mut on_time = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--slip" => {
                let raw = args.next().context("--slip needs a number of days")?;
                slip_days = raw
                    .parse::<i64>()
                    .with_context(|| format!("--slip expects an integer, got {raw:?}"))?;
            }
            "--on-time" => on_time = true,
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(Some(if on_time {
        Mode::OnTime
    } else {
        Mode::Slip(slip_days)
    }))
}

fn print_help() {
    print!(
        "ekg-transport-sim: slip an inbound shipment's ETA and fire the ingest webhook.\n\n\
         Usage: just transport-sim [--slip <days>] [--on-time]\n\
             --slip <days>   push the ETA <days> past today (default {DEFAULT_SLIP_DAYS} => at risk)\n\
             --on-time       restore an on-time ETA (today+{ON_TIME_DAYS}) => covered\n\
             -h, --help      show this help\n\n\
         Needs the substrate up ('just podman-local-up'); config from repo-root .env.\n"
    );
}

/// POST the thin-notify body to the ingest webhook; require HTTP 202.
fn fire_webhook(backend_url: &str, secret: &str, picking_id: i64) -> Result<()> {
    let url = format!("{}/api/ingest/odoo", backend_url.trim_end_matches('/'));
    let body = json!({ "_model": "stock.picking", "_id": picking_id });
    let resp = reqwest::blocking::Client::new()
        .post(&url)
        .header("x-ekg-webhook-secret", secret)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).context("encoding the webhook body")?)
        .send()
        .with_context(|| format!("posting the ingest webhook to {url} (is a backend running?)"))?;
    let code = resp.status();
    if code.as_u16() != 202 {
        bail!("webhook returned HTTP {code} (is a backend running at {backend_url}?)");
    }
    Ok(())
}

/// Resolve a config value: an already-exported env var wins (covers `just`'s
/// dotenv-load), else the repo-root `.env`, else the supplied default.
fn cfg(dotenv: &BTreeMap<String, String>, key: &str, default: &str) -> String {
    if let Ok(val) = std::env::var(key)
        && !val.is_empty()
    {
        return val;
    }
    if let Some(val) = dotenv.get(key)
        && !val.is_empty()
    {
        return val.clone();
    }
    default.to_owned()
}

/// Parse the repo-root `.env` (CWD) into key=value pairs. Best-effort: a missing
/// file yields an empty map (env vars / defaults still apply). Comments and blank
/// lines are skipped; only the first `=` splits each line.
fn load_dotenv() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return map;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            map.insert(key.trim().to_owned(), val.trim().to_owned());
        }
    }
    map
}

/// `today + days` as an Odoo datetime string (`YYYY-MM-DD 12:00:00`, UTC). Same
/// civil-date math as the seed, so the workspace needs no chrono.
fn ymd_plus(days: i64) -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before Unix epoch")?
        .as_secs();
    let epoch_day = i64::try_from(secs / 86_400).context("date overflow")? + days;
    let (y, m, d) = civil_from_days(epoch_day);
    Ok(format!("{y:04}-{m:02}-{d:02} 12:00:00"))
}

/// Convert days-since-Unix-epoch to a civil `(year, month, day)` (Howard Hinnant's
/// algorithm; proleptic Gregorian, UTC). Chrono-free, matching `ekg-odoo-seed`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}
