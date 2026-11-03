//! Gated end-to-end integration test: the transport ETA slip opens an action point through
//! the whole governed loop (Odoo, materialise, NATS fan-out, monitor, action point).
//!
//! Ignored by default. Needs the full local stack up (`just podman-local-up`),
//! the backend running under NATS (`EKG_EVENT_BUS=nats just backend`), and the
//! deployment registered (`just restate-register`). Skips when the backend or Odoo
//! is unreachable, unless `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e`
//! does), in which case it fails. Run with:
//!
//! ```text
//! cargo test -p ekg-e2e --test it transport_integration_it -- --ignored --nocapture
//! ```
//!
//! Scenario (seeded by `ekg-odoo-seed`): the CityLogix "Cargo Trike" order is
//! covered by an on-time inbound brake-module shipment. Slipping that shipment's ETA
//! past the promise must flip it to at risk. The flagship battery order is at risk
//! regardless and is used as a liveness signal that the loop has run. The trike point's
//! reason must report the seeded shortfall: short by 10.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use std::time::Duration;

use crate::common;

use ekg_semantic::ontology;
use ekg_semantic::tabs::Day;
use odoo_client::Odoo;
use serde_json::json;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Resolve a seeded external id (`ekg_seed.<name>`) to its Odoo record id.
fn seed_res_id(odoo: &Odoo, name: &str) -> Option<i64> {
    let rows = odoo
        .search_read(
            "ir.model.data",
            json!([["module", "=", "ekg_seed"], ["name", "=", name]]),
            json!(["res_id"]),
        )
        .ok()?;
    rows.as_array()?.first()?["res_id"].as_i64()
}

/// The action-points list body (raw JSON text) or `None` if the backend is down.
async fn action_points(client: &reqwest::Client, backend: &str) -> Option<String> {
    let resp = client
        .get(format!("{backend}/api/action-points"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

#[tokio::test]
#[ignore = "requires the live substrate + backend under NATS (run with --ignored)"]
async fn a_transport_eta_slip_opens_an_action_point_end_to_end() {
    let odoo_url = env_or("ODOO_URL", "http://localhost:8069");
    let odoo_db = env_or("ODOO_DB", "ekg");
    let odoo_user = env_or("ODOO_USER", "admin");
    let odoo_key = env_or("ODOO_API_KEY", "admin");
    let backend = env_or("EKG_BACKEND_URL", "http://localhost:8090");
    let secret = env_or("EKG_WEBHOOK_SECRET", "dev-secret");

    let client = reqwest::Client::new();

    // --- skip unless the backend is up ---
    if action_points(&client, &backend).await.is_none() {
        common::infra_unavailable(&format!(
            "backend not reachable at {backend} (run `just backend` under NATS)"
        ));
        return;
    }
    let Ok(odoo) = Odoo::connect(&odoo_url, &odoo_db, &odoo_user, &odoo_key) else {
        common::infra_unavailable(&format!("Odoo unreachable at {odoo_url}"));
        return;
    };

    // --- resolve the seeded records ---
    let po = seed_res_id(&odoo, "po_brake_inbound").expect("brake PO seeded (run `just seed`)");
    let trike_so = seed_res_id(&odoo, "so_citylogix").expect("trike sales order seeded");
    let battery_so = seed_res_id(&odoo, "so_velomax").expect("battery sales order seeded");
    let picking = *odoo
        .search("stock.picking", json!([["purchase_id", "=", po]]))
        .expect("search picking")
        .first()
        .expect("an inbound picking exists for the brake PO");

    let trike_subject = ontology::entity_iri("sale.order", trike_so);
    let battery_subject = ontology::entity_iri("sale.order", battery_so);

    // --- baseline: the loop has run (battery is at risk) but the trike is covered ---
    // Poll until the battery point is present, which shows materialise and monitor ran.
    let mut battery_seen = false;
    for _ in 0..40 {
        if let Some(body) = action_points(&client, &backend).await
            && live_state(&body, &battery_subject).is_some()
        {
            battery_seen = true;
            // The on-time trike must not be at risk yet. Subjects match on the full IRI (a
            // substring would also match the e2e fixtures' `sale.order/2x`), and only a live
            // point counts: an earlier run's retired point is not a flag.
            assert!(
                live_state(&body, &trike_subject).is_none(),
                "the trike order is covered before the slip, but was already flagged"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        battery_seen,
        "the governed loop did not open the known at-risk battery point. Is the deployment \
         registered (`just restate-register`) and the sweep run?"
    );

    // --- slip the shipment ETA well past the promise (as `just transport-sim` does: today+60),
    // then notify. Remember the seeded ETA so the restore below returns to it. ---
    let seeded_eta = odoo
        .read("stock.picking", json!([picking]), json!(["scheduled_date"]))
        .expect("read the seeded ETA")
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row["scheduled_date"].as_str())
        .map(str::to_owned)
        .expect("the seeded shipment carries a scheduled_date");
    let commitment = odoo
        .read("sale.order", json!([trike_so]), json!(["commitment_date"]))
        .expect("read the trike promise")
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row["commitment_date"].as_str())
        .map(str::to_owned)
        .expect("the seeded trike order carries a commitment_date");
    // A dirty start (a slip a previous run could not undo) must fail here, not be
    // "restored" to the slipped value. Both are Odoo `YYYY-MM-DD HH:MM:SS`, so the string
    // order is the time order.
    assert!(
        seeded_eta < commitment,
        "dirty start: the brake shipment's ETA {seeded_eta} is not before the trike promise \
         {commitment}; re-seed (`just seed`) before running this test"
    );
    let late_eta = format!("{} 12:00:00", Day::today().plus(60).iso());
    odoo.write(
        "stock.picking",
        json!([picking]),
        json!({ "scheduled_date": late_eta }),
    )
    .expect("slip the shipment ETA");
    let slip_notify = notify_picking(&client, &backend, &secret, picking).await;

    // --- the trike order flips to at risk (only worth polling if the notify landed) ---
    let mut trike_reason = None;
    if matches!(&slip_notify, Ok(code) if code.is_success()) {
        for _ in 0..60 {
            if let Some(body) = action_points(&client, &backend).await {
                trike_reason = reason_of(&body, &trike_subject);
                if trike_reason.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    // --- always restore the seeded ETA, before any assertion, so a re-run starts from a
    // covered baseline (notified, so the world model returns to covered immediately instead
    // of at the next sweep pass). A failed restore is reported, not dropped. ---
    let restored = odoo
        .write(
            "stock.picking",
            json!([picking]),
            json!({ "scheduled_date": seeded_eta }),
        )
        .map_err(|e| format!("write the seeded ETA back: {e}"));
    let restore = match restored {
        Ok(()) => match notify_picking(&client, &backend, &secret, picking).await {
            Ok(code) if code.is_success() => Ok(()),
            Ok(code) => Err(format!("the restore notify was rejected (HTTP {code})")),
            Err(e) => Err(format!("the restore notify failed: {e}")),
        },
        Err(why) => Err(why),
    };
    if let Err(why) = &restore {
        eprintln!(
            "warning: restore of picking {picking} to ETA {seeded_eta} failed ({why}); run `just seed` \
             before the next run"
        );
    }
    // --- always retire the trike point the slip opened, so a re-run on the same stack starts
    // without it: through the governed commands, not a state write.
    let retired = retire_point(&client, &backend, &trike_subject).await;
    if let Err(why) = &retired {
        eprintln!("warning: {why}");
    }

    match &slip_notify {
        Ok(code) => assert!(
            code.is_success(),
            "ingest webhook rejected the shipment notify (HTTP {code})"
        ),
        Err(e) => panic!("the shipment notify could not be sent: {e}"),
    }

    let reason = trike_reason.unwrap_or_else(|| {
        panic!(
            "the transport ETA slip did not open an action point for the trike order ({trike_subject})"
        )
    });
    // The seeded numbers: 10 trikes need 10 brake modules, none on hand, and the only inbound
    // slipped, so the order is short by 10. The fully received historical receipt is consumed
    // history and counts as neither on hand nor inbound.
    let shortfall = reason
        .split("short by ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| c != '.' && !c.is_ascii_digit()).next())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("the reason names a shortfall: {reason}"));
    assert!(
        (shortfall - 10.0).abs() < f64::EPSILON,
        "the brake/trike point reports short by 10, got {shortfall}: {reason}"
    );
    restore.expect("the seeded shipment ETA was restored for the next run");
    retired.expect("the trike point was retired for the next run");
}

/// The reviewer the cleanup acts as (recorded in the audit trail).
const CLEANUP_REVIEWER: &str = "e2e-transport-milestone";

/// Retire `subject`'s live point through the governed commands: abort it (the stop control)
/// while it awaits oversight, or undo it if the policy promoted it (which also restores what
/// an applied expedite wrote). A claim held by an in-flight command (e.g. the policy promoting
/// right now) is retried within a bound. `Ok` once no live point remains (or none opened).
async fn retire_point(
    client: &reqwest::Client,
    backend: &str,
    subject: &str,
) -> Result<(), String> {
    let mut last = String::from("no attempt");
    for _ in 0..60 {
        let Some(body) = action_points(client, backend).await else {
            "the action-point list is unreachable".clone_into(&mut last);
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let command = match live_state(&body, subject).as_deref() {
            None => return Ok(()),
            Some("promoted") => "undo",
            Some(_) => "abort",
        };
        let resp = client
            .post(format!("{backend}/api/action-points/{command}"))
            .header("content-type", "application/json")
            .body(
                json!({
                    "subject": subject,
                    "reviewer": CLEANUP_REVIEWER,
                    "reason": "transport milestone cleanup: the slipped ETA was restored",
                })
                .to_string(),
            )
            .send()
            .await;
        last = match resp {
            Ok(resp) => format!(
                "{command} returned {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ),
            Err(e) => format!("{command} failed: {e}"),
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "the trike point {subject} could not be retired ({last}); abort or undo it in Plant \
         Pulse before the next run"
    ))
}

/// The state of `subject`'s point in the list body when it is live (not aborted, reversed
/// or expired), matched on the full subject.
fn live_state(body: &str, subject: &str) -> Option<String> {
    let points: serde_json::Value = serde_json::from_str(body).ok()?;
    points
        .as_array()?
        .iter()
        .find(|p| p["subject"].as_str() == Some(subject))?["state"]
        .as_str()
        .filter(|state| !matches!(*state, "aborted" | "reversed" | "expired"))
        .map(str::to_owned)
}

/// Fire the ingest webhook for a changed shipment; returns the HTTP status, or the transport
/// error (not a panic, so the caller can still restore the shipment first).
async fn notify_picking(
    client: &reqwest::Client,
    backend: &str,
    secret: &str,
    picking: i64,
) -> reqwest::Result<reqwest::StatusCode> {
    let notify = json!({ "_model": "stock.picking", "_id": picking }).to_string();
    client
        .post(format!("{backend}/api/ingest/odoo"))
        .header("x-ekg-webhook-secret", secret)
        .header("content-type", "application/json")
        .body(notify)
        .send()
        .await
        .map(|resp| resp.status())
}

/// The reason of the live action point keyed by `subject` in the list body, if listed. An
/// earlier run's retired point carries the previous reason, so it is skipped.
fn reason_of(body: &str, subject: &str) -> Option<String> {
    live_state(body, subject)?;
    let points: serde_json::Value = serde_json::from_str(body).ok()?;
    points
        .as_array()?
        .iter()
        .find(|p| p["subject"].as_str() == Some(subject))?["reason"]
        .as_str()
        .map(str::to_owned)
}
