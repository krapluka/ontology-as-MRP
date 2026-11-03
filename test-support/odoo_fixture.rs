//! Self-contained Odoo fixtures shared by the gated source-scope tests of every crate. One
//! copy, included by `#[path]` from each `tests/it/common/mod.rs` (a sibling crate's test module
//! cannot be imported). Each fixture creates throwaway records. A setup that fails partway
//! undoes what it had already created before it panics, and `teardown` returns what it could
//! not undo instead of panicking, so a caller can finish its other cleanup first (report the
//! error at once, assert it last).

use std::fmt::Display;
use std::sync::Arc;

use odoo_client::Odoo;
use serde_json::{Value, json};

/// Run a blocking Odoo call off the async runtime; a failure is the call's error text, with
/// its whole cause chain (`{:#}`), so a wrapped transport error keeps its cause.
///
/// # Errors
/// The Odoo call's error (or the join error), as text.
pub async fn try_blocking<T: Send + 'static, E: Display + Send + 'static>(
    odoo: &Arc<Odoo>,
    f: impl FnOnce(&Odoo) -> Result<T, E> + Send + 'static,
) -> Result<T, String> {
    let odoo = Arc::clone(odoo);
    tokio::task::spawn_blocking(move || f(&odoo))
        .await
        .map_err(|e| format!("odoo call join: {e:#}"))?
        .map_err(|e| format!("odoo call: {e:#}"))
}

/// One fixture setup step: [`try_blocking`], with a failure naming the step.
async fn step<T: Send + 'static, E: Display + Send + 'static>(
    odoo: &Arc<Odoo>,
    what: &str,
    f: impl FnOnce(&Odoo) -> Result<T, E> + Send + 'static,
) -> Result<T, String> {
    try_blocking(odoo, f)
        .await
        .map_err(|why| format!("{what}: {why}"))
}

/// A unique-per-run stamp for throwaway record names.
#[must_use]
pub fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// What a fixture has created so far: what [`undo`] reverts, on a failed setup and at
/// teardown alike.
#[derive(Debug, Default, Clone, Copy)]
struct Created {
    vendor: Option<i64>,
    product: Option<i64>,
    purchase: Option<i64>,
    /// Whether the purchase's receipt is done (a received purchase cannot be cancelled).
    received: bool,
    vendor_return: Option<i64>,
}

/// Best-effort revert of `created`: cancel an open return, cancel a purchase whose receipt is
/// not done, book the product's `WH/Stock` quants back to zero, archive the product and the
/// vendor. Every step runs even if an earlier one failed.
///
/// # Errors
/// Every step that failed.
async fn undo(odoo: &Arc<Odoo>, created: Created) -> Result<(), String> {
    let mut failed = Vec::new();
    if let Some(vendor_return) = created.vendor_return
        && let Err(why) = try_blocking(odoo, move |o| {
            o.call_method(
                "stock.picking",
                "action_cancel",
                json!({ "ids": [vendor_return] }),
            )
        })
        .await
    {
        failed.push(format!("cancel return {vendor_return}: {why}"));
    }
    if let (Some(purchase), false) = (created.purchase, created.received)
        && let Err(why) = try_blocking(odoo, move |o| {
            o.call_method(
                "purchase.order",
                "button_cancel",
                json!({ "ids": [purchase] }),
            )
        })
        .await
    {
        failed.push(format!("cancel purchase {purchase}: {why}"));
    }
    if let Some(product) = created.product
        && let Err(why) = try_blocking(odoo, move |o| {
            let stock_loc = o.xmlid("stock", "stock_location_stock")?;
            let quants = o.search(
                "stock.quant",
                json!([
                    ["product_id", "=", product],
                    ["location_id", "=", stock_loc]
                ]),
            )?;
            if !quants.is_empty() {
                o.write(
                    "stock.quant",
                    json!(quants),
                    json!({ "inventory_quantity": 0 }),
                )?;
                o.call_method(
                    "stock.quant",
                    "action_apply_inventory",
                    json!({ "ids": quants }),
                )?;
            }
            o.write(
                "product.product",
                json!([product]),
                json!({ "active": false }),
            )
        })
        .await
    {
        failed.push(format!("zero and archive product {product}: {why}"));
    }
    if let Some(vendor) = created.vendor
        && let Err(why) = try_blocking(odoo, move |o| {
            o.write("res.partner", json!([vendor]), json!({ "active": false }))
        })
        .await
    {
        failed.push(format!("archive vendor {vendor}: {why}"));
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// Finish a fixture's setup: its value, or, after undoing what `created` holds, a panic
/// naming the failed step and whether the rollback was complete.
async fn settle<T>(
    odoo: &Arc<Odoo>,
    fixture: &str,
    built: Result<T, String>,
    created: Created,
) -> T {
    match built {
        Ok(value) => value,
        Err(why) => {
            let rollback = match undo(odoo, created).await {
                Ok(()) => "rolled back".to_owned(),
                Err(left) => format!("rollback INCOMPLETE ({left}) — clean up by hand"),
            };
            panic!("{fixture} setup failed: {why}; {rollback}")
        }
    }
}

/// A throwaway storable product with stock booked by an inventory adjustment at `WH/Stock`: one
/// internal quant and its counterpart quant at the virtual `Inventory adjustment` location.
pub struct QuantFixture {
    /// The throwaway `product.product`.
    pub product: i64,
    /// Its quant at the internal `WH/Stock` location.
    pub internal: i64,
    /// Its counterpart quant at a virtual location.
    pub virtual_quant: i64,
}

impl QuantFixture {
    /// Create the product, adjust 7 units into `WH/Stock`, and find the counterpart quant.
    ///
    /// # Panics
    /// When a setup call fails or no counterpart quant was booked, after undoing what was
    /// already created.
    pub async fn create(odoo: &Arc<Odoo>, label: &str) -> Self {
        let mut created = Created::default();
        let built = Self::build(odoo, label, &mut created).await;
        settle(odoo, "quant fixture", built, created).await
    }

    async fn build(odoo: &Arc<Odoo>, label: &str, created: &mut Created) -> Result<Self, String> {
        let name = format!("{label} {} (throwaway)", stamp());
        let product = step(odoo, "create the throwaway product", move |o| {
            o.create(
                "product.product",
                json!({ "name": name, "type": "consu", "is_storable": true }),
            )
        })
        .await?;
        created.product = Some(product);
        let stock_loc = step(odoo, "resolve WH/Stock", |o| {
            o.xmlid("stock", "stock_location_stock")
        })
        .await?;
        let internal = step(odoo, "create the internal quant", move |o| {
            o.create(
                "stock.quant",
                json!({ "product_id": product, "location_id": stock_loc, "inventory_quantity": 7 }),
            )
        })
        .await?;
        step(odoo, "apply the inventory adjustment", move |o| {
            o.call_method(
                "stock.quant",
                "action_apply_inventory",
                json!({ "ids": [internal] }),
            )
        })
        .await?;
        let quants = step(odoo, "list the product's quants", move |o| {
            o.search_read(
                "stock.quant",
                json!([["product_id", "=", product]]),
                json!(["id"]),
            )
        })
        .await?;
        let virtual_quant = quants
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|q| q["id"].as_i64())
            .find(|id| *id != internal)
            .ok_or_else(|| {
                format!(
                    "the adjustment booked no counterpart quant at a virtual location: {quants}"
                )
            })?;
        Ok(Self {
            product,
            internal,
            virtual_quant,
        })
    }

    /// Zero the stock and archive the product.
    ///
    /// # Errors
    /// What could not be undone.
    pub async fn teardown(&self, odoo: &Arc<Odoo>) -> Result<(), String> {
        let created = Created {
            product: Some(self.product),
            ..Created::default()
        };
        undo(odoo, created)
            .await
            .map_err(|why| format!("quant fixture teardown: {why}"))
    }
}

/// A throwaway purchase from a throwaway vendor whose receipt is validated (done, 5 units
/// into `WH/Stock`), plus an open return of that receipt to the vendor made through Odoo's own
/// return wizard. The return keeps the purchase order, like any vendor return.
pub struct VendorReturnFixture {
    /// The throwaway vendor.
    pub vendor: i64,
    /// The throwaway storable product.
    pub product: i64,
    /// The confirmed purchase order.
    pub purchase: i64,
    /// Its validated (done) incoming receipt.
    pub receipt: i64,
    /// The open return of that receipt to the vendor.
    pub vendor_return: i64,
}

impl VendorReturnFixture {
    /// Create the vendor, product and purchase order; confirm it; receive the full quantity;
    /// then return all of it through `stock.return.picking` and leave the return open.
    ///
    /// # Panics
    /// When a setup call fails, the receipt does not validate, or the wizard makes no return,
    /// after undoing what was already created.
    pub async fn create(odoo: &Arc<Odoo>, label: &str) -> Self {
        let mut created = Created::default();
        let built = Self::build(odoo, label, &mut created).await;
        settle(odoo, "vendor-return fixture", built, created).await
    }

    async fn build(odoo: &Arc<Odoo>, label: &str, created: &mut Created) -> Result<Self, String> {
        let tag = stamp();
        let vendor_name = format!("{label} vendor {tag} (throwaway)");
        let vendor = step(odoo, "create the throwaway vendor", move |o| {
            o.create(
                "res.partner",
                json!({ "name": vendor_name, "supplier_rank": 1 }),
            )
        })
        .await?;
        created.vendor = Some(vendor);
        let product_name = format!("{label} product {tag} (throwaway)");
        let product = step(odoo, "create the throwaway product", move |o| {
            o.create(
                "product.product",
                json!({ "name": product_name, "type": "consu", "is_storable": true }),
            )
        })
        .await?;
        created.product = Some(product);
        let uom = step(odoo, "resolve the unit UoM", |o| {
            o.xmlid("uom", "product_uom_unit")
        })
        .await?;
        let purchase = step(odoo, "create the purchase order", move |o| {
            o.create("purchase.order", json!({ "partner_id": vendor }))
        })
        .await?;
        created.purchase = Some(purchase);
        step(odoo, "create the purchase order line", move |o| {
            o.create(
                "purchase.order.line",
                json!({
                    "order_id": purchase,
                    "product_id": product,
                    "name": "Vendor-return scope IT line",
                    "product_qty": 5,
                    "product_uom_id": uom,
                    "price_unit": 1.0,
                }),
            )
        })
        .await?;
        step(odoo, "confirm the purchase order", move |o| {
            o.call_method(
                "purchase.order",
                "button_confirm",
                json!({ "ids": [purchase] }),
            )
        })
        .await?;
        let receipts = step(odoo, "find the generated receipt", move |o| {
            o.search("stock.picking", json!([["purchase_id", "=", purchase]]))
        })
        .await?;
        let receipt = *receipts
            .first()
            .ok_or_else(|| format!("confirming purchase order {purchase} generated no receipt"))?;
        validate_in_full(odoo, receipt).await?;
        created.received = true;
        // Odoo's own return wizard, returning everything received; the return stays open.
        let wizard = step(odoo, "open the return wizard", move |o| {
            o.create("stock.return.picking", json!({ "picking_id": receipt }))
        })
        .await?;
        let action = step(odoo, "create the return", move |o| {
            o.call_method(
                "stock.return.picking",
                "action_create_returns_all",
                json!({ "ids": [wizard] }),
            )
        })
        .await?;
        let vendor_return = action["res_id"]
            .as_i64()
            .ok_or_else(|| format!("the return wizard made no return picking: {action}"))?;
        created.vendor_return = Some(vendor_return);
        Ok(Self {
            vendor,
            product,
            purchase,
            receipt,
            vendor_return,
        })
    }

    /// Cancel the open return, book the received stock back out, and archive the product and
    /// the vendor. (A received purchase order cannot be cancelled; it stays, fully received.)
    ///
    /// # Errors
    /// What could not be undone.
    pub async fn teardown(&self, odoo: &Arc<Odoo>) -> Result<(), String> {
        let created = Created {
            vendor: Some(self.vendor),
            product: Some(self.product),
            purchase: Some(self.purchase),
            received: true,
            vendor_return: Some(self.vendor_return),
        };
        undo(odoo, created)
            .await
            .map_err(|why| format!("vendor-return fixture teardown: {why}"))
    }
}

/// Receive a receipt's full demand and validate it, so no backorder wizard is needed (as the
/// seed does).
///
/// # Errors
/// A failed call, or the receipt not `done` afterwards.
async fn validate_in_full(odoo: &Arc<Odoo>, receipt: i64) -> Result<(), String> {
    let moves = step(odoo, "read the receipt's moves", move |o| {
        o.search_read(
            "stock.move",
            json!([["picking_id", "=", receipt]]),
            json!(["product_uom_qty"]),
        )
    })
    .await?;
    for mv in moves.as_array().into_iter().flatten() {
        let (Some(id), Some(qty)) = (mv["id"].as_i64(), mv["product_uom_qty"].as_f64()) else {
            return Err(format!("a receipt move without an id or demand: {mv}"));
        };
        step(odoo, "set the received quantity", move |o| {
            o.write(
                "stock.move",
                json!([id]),
                json!({ "quantity": qty, "picked": true }),
            )
        })
        .await?;
    }
    let outcome = step(odoo, "validate the receipt", move |o| {
        o.call_method(
            "stock.picking",
            "button_validate",
            json!({ "ids": [receipt] }),
        )
    })
    .await?;
    let state = read_field(odoo, "stock.picking", receipt, "state").await?;
    if state.as_str() == Some("done") {
        Ok(())
    } else {
        Err(format!(
            "the receipt #{receipt} did not validate (state {state}); button_validate returned {outcome}"
        ))
    }
}

/// One field of one record, as JSON (Odoo's `false` for an empty field).
///
/// # Errors
/// The failed read (with its cause chain), or a record / field the answer does not carry.
/// Neither is folded into a value, so a read failure is never mistaken for a record's state.
pub async fn read_field(
    odoo: &Arc<Odoo>,
    model: &str,
    id: i64,
    field: &str,
) -> Result<Value, String> {
    let (model_name, field_name) = (model.to_owned(), field.to_owned());
    let rows = try_blocking(odoo, move |o| {
        o.read(&model_name, json!([id]), json!([field_name]))
    })
    .await
    .map_err(|why| format!("read {model}#{id}.{field}: {why}"))?;
    rows.as_array()
        .and_then(|a| a.first())
        .and_then(|r| r.get(field))
        .cloned()
        .ok_or_else(|| format!("read {model}#{id}.{field}: no such record or field in {rows}"))
}
