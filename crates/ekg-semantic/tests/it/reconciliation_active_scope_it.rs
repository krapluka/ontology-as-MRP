//! Gated live guard for the active-only scope: reconciliation source listings use Odoo's
//! default active-only scope, so an archived record is absent from both `changed_since`
//! and `current_ids` (and is therefore retracted, not re-materialized, by the sweep).
//!
//! Ignored by default. Needs a reachable Odoo. Run with:
//!
//! ```text
//! cargo test -p ekg-semantic --test it reconciliation_active_scope_it -- --ignored
//! ```
//!
//! Creates a throwaway `res.partner`, proves it is visible while active, archives it,
//! then asserts it drops out of both listings. Also proves the contract-declared source
//! scope: only internal stock is on hand, and only receipts (never a return to the vendor)
//! are delivering shipments. Skips if Odoo is unreachable, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), when it fails loudly.

#![allow(clippy::expect_used, clippy::doc_markdown, clippy::too_many_lines)]

use std::sync::Arc;

use crate::common;

use common::odoo_fixture::{QuantFixture, VendorReturnFixture, read_field};
use ekg_domain::{EngineError, SourceRef};
use ekg_semantic::{OdooConfig, OdooIngestor, SourceIngestor, ontology};
use odoo_client::Odoo;
use serde_json::json;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

async fn changed_ids(ingestor: &OdooIngestor, watermark: &str) -> Vec<i64> {
    ingestor
        .changed_since(watermark)
        .await
        .expect("changed_since")
        .into_iter()
        .filter(|r| r.model == "res.partner")
        .map(|r| r.id)
        .collect()
}

#[tokio::test]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn archived_record_drops_out_of_reconciliation_listings() {
    let url = env_or("ODOO_URL", "http://localhost:8069");
    let db = env_or("ODOO_DB", "ekg");
    let user = env_or("ODOO_USER", "admin");
    let key = env_or("ODOO_API_KEY", "admin");

    // Raw client for the create/archive/unlink mutations (blocking, off the runtime).
    let Some(odoo) = raw_odoo(&url, &db, &user, &key).await else {
        return;
    };

    let ingestor = OdooIngestor::connect(OdooConfig {
        base_url: url,
        db,
        login: user,
        password: key,
        system: "odoo".to_owned(),
        allowlist: vec!["res.partner".to_owned()],
        scopes: std::collections::BTreeMap::new(),
    })
    .await
    .expect("connect ingestor");

    // A throwaway active partner; watermark well before "now" so it qualifies.
    let watermark = "2000-01-01 00:00:00";
    let id = {
        let odoo = Arc::clone(&odoo);
        tokio::task::spawn_blocking(move || {
            odoo.create(
                "res.partner",
                json!({ "name": "EKG active-scope IT (throwaway)" }),
            )
        })
        .await
        .expect("join")
        .expect("create partner")
    };

    // While active: visible in both listings.
    assert!(
        changed_ids(&ingestor, watermark).await.contains(&id),
        "active partner should appear in changed_since"
    );
    assert!(
        ingestor
            .current_ids("res.partner")
            .await
            .expect("current_ids")
            .contains(&id),
        "active partner should appear in current_ids"
    );

    // Archive it (active = false).
    {
        let odoo = Arc::clone(&odoo);
        tokio::task::spawn_blocking(move || {
            odoo.write("res.partner", json!([id]), json!({ "active": false }))
        })
        .await
        .expect("join")
        .expect("archive partner");
    }

    let changed_after = changed_ids(&ingestor, watermark).await;
    let current_after = ingestor
        .current_ids("res.partner")
        .await
        .expect("current_ids");

    // Cleanup before asserting so a failure doesn't leak the record.
    {
        let odoo = Arc::clone(&odoo);
        let _ = tokio::task::spawn_blocking(move || odoo.unlink("res.partner", json!([id]))).await;
    }

    assert!(
        !changed_after.contains(&id),
        "archived record must be absent from changed_since (active-only scope)"
    );
    assert!(
        !current_after.contains(&id),
        "archived record must be absent from current_ids (active-only scope)"
    );
}

/// Connect the raw Odoo client for fixture mutations, or report the infrastructure unavailable.
async fn raw_odoo(url: &str, db: &str, user: &str, key: &str) -> Option<Arc<Odoo>> {
    let (c_url, c_db, c_user, c_key) = (
        url.to_owned(),
        db.to_owned(),
        user.to_owned(),
        key.to_owned(),
    );
    match tokio::task::spawn_blocking(move || Odoo::connect(&c_url, &c_db, &c_user, &c_key))
        .await
        .expect("join")
    {
        Ok(o) => Some(Arc::new(o)),
        Err(e) => {
            common::infra_unavailable(&format!("Odoo unreachable: {e:#}"));
            None
        }
    }
}

/// The contract's source scope bounds reconciliation and pulls. Self-contained: a throwaway
/// product given a non-zero inventory adjustment at `WH/Stock` yields one internal quant and
/// one counterpart quant at the virtual `Inventory adjustment` location. The scoped listing
/// names the internal quant and not the virtual one; pulling the virtual one is refused as not
/// found while the internal one pulls in full, also through an ingestor whose allowlist does
/// not name quants; and a scope the source rejects fails `connect` loudly.
#[tokio::test]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn the_source_scope_keeps_virtual_location_stock_out() {
    let url = env_or("ODOO_URL", "http://localhost:8069");
    let db = env_or("ODOO_DB", "ekg");
    let user = env_or("ODOO_USER", "admin");
    let key = env_or("ODOO_API_KEY", "admin");
    let Some(odoo) = raw_odoo(&url, &db, &user, &key).await else {
        return;
    };
    let config = |scopes| OdooConfig {
        base_url: url.clone(),
        db: db.clone(),
        login: user.clone(),
        password: key.clone(),
        system: "odoo".to_owned(),
        allowlist: vec!["stock.quant".to_owned()],
        scopes,
    };

    // --- a scope the source rejects fails connect (not a later, retrying sweep) ---
    let bad = std::collections::BTreeMap::from([(
        "stock.quant".to_owned(),
        vec![json!(["location_id.usag", "=", "internal"])],
    )]);
    let refused = OdooIngestor::connect(config(bad.clone())).await;
    assert!(
        matches!(&refused, Err(EngineError::Invariant(why)) if why.contains("stock.quant")),
        "a rejected scope fails connect with an invariant error"
    );
    // But only an allowlisted model's scope is probed: off the allowlist, the same rejected
    // scope never reaches the source at connect (a narrowed allowlist needs no read access).
    let unprobed = OdooIngestor::connect(OdooConfig {
        allowlist: vec!["res.partner".to_owned()],
        ..config(bad)
    })
    .await;
    assert!(
        unprobed.is_ok(),
        "a scope off the allowlist is not probed at connect: {:?}",
        unprobed.err()
    );

    // Everything that can fail without the fixture is done before it exists.
    let contract_scopes = ontology::supply_risk_contract()
        .expect("contract")
        .source_scopes();

    // --- self-contained fixture: an internal quant and its virtual counterpart ---
    let fixture = QuantFixture::create(&odoo, "EKG scope IT").await;
    let (internal, virtual_quant) = (fixture.internal, fixture.virtual_quant);

    // --- the scoped ingestor ---
    let ingestor = OdooIngestor::connect(config(contract_scopes.clone())).await;
    let (listed, virtual_pull, internal_pull) = match &ingestor {
        Ok(ingestor) => (
            ingestor.current_ids("stock.quant").await,
            Some(pull(ingestor, "stock.quant", virtual_quant).await),
            Some(pull(ingestor, "stock.quant", internal).await),
        ),
        Err(e) => (Err(EngineError::Backend(e.to_string())), None, None),
    };
    // --- a narrowed allowlist (quants not on it) still applies the quant scope to a pull
    // that reaches the ingestor anyway (a queued ingest, a direct call) ---
    let narrowed = OdooIngestor::connect(OdooConfig {
        allowlist: vec!["res.partner".to_owned()],
        ..config(contract_scopes)
    })
    .await;
    let narrowed_virtual_pull = match &narrowed {
        Ok(ingestor) => Some(pull(ingestor, "stock.quant", virtual_quant).await),
        Err(_) => None,
    };

    // --- cleanup before asserting: zero the stock, archive the product ---
    let teardown = fixture.teardown(&odoo).await;
    // Reported at once: a later failed assertion must not swallow it.
    if let Err(why) = &teardown {
        eprintln!("fixture teardown failed: {why}");
    }

    ingestor.expect("connect scoped ingestor");
    let listed = listed.expect("current_ids");
    assert!(listed.contains(&internal), "the internal quant is listed");
    assert!(
        !listed.contains(&virtual_quant),
        "the virtual-location quant is not listed (reconciliation retracts it if materialized)"
    );
    assert!(
        matches!(virtual_pull, Some(Err(EngineError::NotFound(_)))),
        "pulling the virtual-location quant is refused as not found: {virtual_pull:?}"
    );
    let record = internal_pull
        .expect("pulled")
        .expect("the internal quant pulls");
    assert!(
        record.fields.contains_key("quantity"),
        "the scoped pull reads the whole record: {:?}",
        record.fields.keys().collect::<Vec<_>>()
    );
    narrowed.expect("connect the narrowed-allowlist ingestor");
    assert!(
        matches!(narrowed_virtual_pull, Some(Err(EngineError::NotFound(_)))),
        "a model off the allowlist keeps its scope for pulls: {narrowed_virtual_pull:?}"
    );
    teardown.expect("the quant fixture is cleaned up");
}

/// A return to the vendor is never a delivering shipment. Self-contained: a throwaway
/// purchase whose receipt is validated, then returned in full through Odoo's own return wizard
/// and left open. The return keeps the purchase order, so were it in scope it would read as an
/// inbound shipment that can still deliver. The contract's scoped listing names the receipt
/// and not the return, and pulling the return is refused as not found. The direction term
/// alone (`location_dest_id.usage = internal`) already keeps the return out, so the exclusion
/// holds whether or not the receipt type names a return type (when it does not, Odoo's wizard
/// reuses the incoming receipt type for the return).
#[tokio::test]
#[ignore = "requires a reachable Odoo (run with --ignored)"]
async fn a_return_to_the_vendor_is_never_a_delivering_shipment() {
    const MODEL: &str = "stock.picking";
    let url = env_or("ODOO_URL", "http://localhost:8069");
    let db = env_or("ODOO_DB", "ekg");
    let user = env_or("ODOO_USER", "admin");
    let key = env_or("ODOO_API_KEY", "admin");
    let Some(odoo) = raw_odoo(&url, &db, &user, &key).await else {
        return;
    };
    let config = |scopes| OdooConfig {
        base_url: url.clone(),
        db: db.clone(),
        login: user.clone(),
        password: key.clone(),
        system: "odoo".to_owned(),
        allowlist: vec![MODEL.to_owned()],
        scopes,
    };

    // Everything that can fail without the fixture is done before it exists.
    let contract_scopes = ontology::supply_risk_contract()
        .expect("contract")
        .source_scopes();
    let fixture = VendorReturnFixture::create(&odoo, "EKG vendor-return scope IT").await;
    let (receipt, vendor_return, purchase) =
        (fixture.receipt, fixture.vendor_return, fixture.purchase);
    let return_type = read_field(&odoo, MODEL, vendor_return, "picking_type_code").await;
    let return_state = read_field(&odoo, MODEL, vendor_return, "state").await;
    let return_order = read_field(&odoo, MODEL, vendor_return, "purchase_id").await;

    // --- the contract's scope ---
    let scoped = OdooIngestor::connect(config(contract_scopes)).await;
    // --- the direction term alone (the config-independent guard) ---
    let direction_only = OdooIngestor::connect(config(std::collections::BTreeMap::from([(
        MODEL.to_owned(),
        vec![json!(["location_dest_id.usage", "=", "internal"])],
    )])))
    .await;
    let (listed, return_pull, receipt_pull) = match &scoped {
        Ok(ingestor) => (
            ingestor.current_ids(MODEL).await,
            Some(pull(ingestor, MODEL, vendor_return).await),
            Some(pull(ingestor, MODEL, receipt).await),
        ),
        Err(e) => (Err(EngineError::Backend(e.to_string())), None, None),
    };
    let listed_by_direction = match &direction_only {
        Ok(ingestor) => ingestor.current_ids(MODEL).await,
        Err(e) => Err(EngineError::Backend(e.to_string())),
    };

    // --- cleanup before asserting ---
    let teardown = fixture.teardown(&odoo).await;
    // Reported at once: a later failed assertion must not swallow it.
    if let Err(why) = &teardown {
        eprintln!("fixture teardown failed: {why}");
    }

    scoped.expect("connect the contract-scoped ingestor");
    direction_only.expect("connect the direction-scoped ingestor");
    let return_type = return_type.expect("read the return's type");
    let return_state = return_state.expect("read the return's state");
    let return_order = return_order.expect("read the return's purchase order");
    // The premise the test exists for: the open return still carries the purchase order, so
    // were it in scope it would read as a shipment that can still deliver that order.
    assert_eq!(
        return_order
            .as_array()
            .and_then(|m2o| m2o.first())
            .and_then(serde_json::Value::as_i64),
        Some(purchase),
        "precondition: the return #{vendor_return} keeps purchase {purchase}: {return_order}"
    );
    let context = format!(
        "the return #{vendor_return} is {return_type} / {return_state}, of purchase {return_order}"
    );
    assert!(
        return_state
            .as_str()
            .is_some_and(|s| s != "done" && s != "cancel"),
        "precondition: the return is open ({context})"
    );
    let listed = listed.expect("current_ids");
    assert!(
        listed.contains(&receipt),
        "the validated receipt is in scope ({context})"
    );
    assert!(
        !listed.contains(&vendor_return),
        "a return to the vendor is out of the shipment scope ({context})"
    );
    assert!(
        matches!(return_pull, Some(Err(EngineError::NotFound(_)))),
        "pulling the return is refused as not found ({context}): {return_pull:?}"
    );
    let record = receipt_pull.expect("pulled").expect("the receipt pulls");
    assert!(
        record.fields.contains_key("purchase_id"),
        "the receipt pulls in full: {:?}",
        record.fields.keys().collect::<Vec<_>>()
    );
    let listed_by_direction = listed_by_direction.expect("direction-scoped current_ids");
    assert!(
        listed_by_direction.contains(&receipt) && !listed_by_direction.contains(&vendor_return),
        "the direction term alone keeps the return out and the receipt in, whatever the return's type ({context})"
    );
    teardown.expect("the vendor-return fixture is cleaned up");
}

/// Pull one record of `model` through the ingestor.
async fn pull(
    ingestor: &OdooIngestor,
    model: &str,
    id: i64,
) -> ekg_domain::Result<ekg_domain::SourceRecord> {
    ingestor
        .pull(&SourceRef {
            system: "odoo".to_owned(),
            model: model.to_owned(),
            id,
        })
        .await
}
