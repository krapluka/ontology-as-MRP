//! Entity display labels: the name an operator reads for an entity id, composed from the
//! materialized names by the entity's model. A label is a display projection read at request
//! time (texts keep their ids), and a part that is not materialized is left out, never
//! invented.

use std::collections::BTreeMap;

use ekg_domain::{EntityUri, Result, WorldModel};

use super::delivery::model_of;
use super::money::Money;
use super::queries;
use crate::ontology;

type Row = BTreeMap<String, String>;

fn bound<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

/// A quantity as an operator reads it: `10`, `2.5` (no trailing zeros).
fn quantity(value: Money) -> String {
    let rendered = value.render();
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// The label for an entity from its label row, by its model:
/// - a purchase-order line: `"{PO} · {open qty} × {product}"`;
/// - a sales order: `"{SO} · {customer}"`;
/// - a work order: `"{MO} · {operation}"`, else `"{MO} · {work center}"`;
/// - a capacity record: `"{work center} capacity · {product}"`;
/// - anything else: its own name.
///
/// A part that is not materialized is left out; with no name at all there is no label.
fn compose(model: &str, row: &Row) -> Option<String> {
    let join = |head: &str, tail: Option<&str>| match tail {
        Some(tail) => format!("{head} · {tail}"),
        None => head.to_owned(),
    };
    match model {
        "purchase.order.line" => {
            let po = bound(row, "poName")?;
            let open = bound(row, "qty").and_then(Money::parse).and_then(|qty| {
                let received =
                    bound(row, "received").map_or(Some(Money::default()), Money::parse)?;
                qty.checked_sub(received)
            });
            let tail = match (open, bound(row, "productName")) {
                (Some(open), Some(product)) => Some(format!("{} × {product}", quantity(open))),
                (None, Some(product)) => Some(product.to_owned()),
                (_, None) => None,
            };
            Some(join(po, tail.as_deref()))
        }
        "sale.order" => Some(join(bound(row, "name")?, bound(row, "customerName"))),
        "mrp.workorder" => Some(join(
            bound(row, "moName").or_else(|| bound(row, "name"))?,
            bound(row, "opName").or_else(|| bound(row, "wcName")),
        )),
        "mrp.workcenter.capacity" => match bound(row, "capWcName") {
            Some(wc) => Some(join(
                &format!("{wc} capacity"),
                bound(row, "capProductName"),
            )),
            None => bound(row, "name").map(str::to_owned),
        },
        _ => bound(row, "name").map(str::to_owned),
    }
}

/// The labels of the canonical `subjects` rows name, from one read of `entity-labels.rq`.
async fn read_labels(
    world: &dyn WorldModel,
    subjects: &[&str],
) -> Result<BTreeMap<String, String>> {
    let Some(sparql) = queries::entity_labels_query(subjects.iter().copied())? else {
        return Ok(BTreeMap::new());
    };
    let mut first: BTreeMap<String, Row> = BTreeMap::new();
    for row in world.select(&sparql).await? {
        if let Some(s) = bound(&row, "s") {
            first.entry(s.to_owned()).or_insert(row);
        }
    }
    Ok(first
        .into_iter()
        .filter_map(|(s, row)| {
            let label = compose(model_of(&EntityUri(s.clone()))?, &row)?;
            Some((s, label))
        })
        .collect())
}

/// The display labels of `ids`, in request order: each id asked once, and only the ids the
/// world can name. An id that is not a canonical entity id is never interpolated and gets no
/// label, like an unnamed one.
///
/// # Errors
/// A read error.
pub async fn entity_labels(
    world: &dyn WorldModel,
    ids: &[String],
) -> Result<Vec<(EntityUri, String)>> {
    let mut asked: Vec<&str> = Vec::new();
    for id in ids {
        if ontology::is_canonical_entity_iri(id) && !asked.contains(&id.as_str()) {
            asked.push(id);
        }
    }
    let mut labels = read_labels(world, &asked).await?;
    Ok(asked
        .into_iter()
        .filter_map(|id| {
            let label = labels.remove(id)?;
            Some((EntityUri(id.to_owned()), label))
        })
        .collect())
}

/// A change object's display label, by the same rule as [`entity_labels`]. `Ok(None)` when no
/// name is materialized.
///
/// # Errors
/// A read error, or a non-canonical IRI.
pub async fn change_label(world: &dyn WorldModel, object: &EntityUri) -> Result<Option<String>> {
    Ok(read_labels(world, &[object.0.as_str()])
        .await?
        .remove(&object.0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ekg_domain::{EngineError, Grounding, ReadContext, Revision, SourceRecord};

    use super::*;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A world answering the label query from canned rows, recording every query it runs.
    struct CannedWorld {
        rows: Vec<Row>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl WorldModel for CannedWorld {
        async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
            Err(EngineError::Backend("read-only".to_owned()))
        }
        async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
            Err(EngineError::Backend("read-only".to_owned()))
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
            Err(EngineError::Backend("read-only".to_owned()))
        }
        async fn select(&self, sparql: &str) -> Result<Vec<Row>> {
            self.asked
                .lock()
                .map_err(|_| EngineError::Backend("poisoned".to_owned()))?
                .push(sparql.to_owned());
            Ok(self.rows.clone())
        }
    }

    fn world(rows: Vec<Row>) -> CannedWorld {
        CannedWorld {
            rows,
            asked: Mutex::new(Vec::new()),
        }
    }

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }

    /// The label `world` composes for the one entity `s`, whose row is `pairs`.
    async fn label_of(s: &str, pairs: &[(&str, &str)]) -> Option<String> {
        let mut r = row(pairs);
        r.insert("s".to_owned(), s.to_owned());
        change_label(&world(vec![r]), &EntityUri(s.to_owned()))
            .await
            .expect("read")
    }

    #[tokio::test]
    async fn labels_read_per_model() {
        let line = iri("purchase.order.line", 3);
        assert_eq!(
            label_of(
                &line,
                &[
                    ("poName", "P00001"),
                    ("qty", "12.0"),
                    ("received", "2.0"),
                    ("productName", "Battery Pack 500Wh"),
                ]
            )
            .await
            .as_deref(),
            Some("P00001 · 10 × Battery Pack 500Wh"),
            "the OPEN quantity"
        );
        assert_eq!(
            label_of(
                &iri("sale.order", 1),
                &[("name", "S00001"), ("customerName", "VeloMax Retail")]
            )
            .await
            .as_deref(),
            Some("S00001 · VeloMax Retail")
        );
        assert_eq!(
            label_of(
                &iri("mrp.workorder", 5),
                &[
                    ("moName", "WH/MO/00001"),
                    ("opName", "Assemble drive + frame"),
                    ("wcName", "Assembly Line"),
                ]
            )
            .await
            .as_deref(),
            Some("WH/MO/00001 · Assemble drive + frame"),
            "the operation, the more specific name"
        );
        assert_eq!(
            label_of(
                &iri("mrp.workcenter.capacity", 1),
                &[
                    ("capWcName", "Assembly Line"),
                    ("capProductName", "E-Bike Model X"),
                ]
            )
            .await
            .as_deref(),
            Some("Assembly Line capacity · E-Bike Model X")
        );
        for (model, name) in [
            ("mrp.production", "WH/MO/00001"),
            ("mrp.workcenter", "Assembly Line"),
            ("res.partner", "CellTech"),
        ] {
            assert_eq!(
                label_of(&iri(model, 1), &[("name", name)]).await.as_deref(),
                Some(name)
            );
        }
    }

    #[tokio::test]
    async fn a_part_that_is_not_materialized_is_left_out() {
        assert_eq!(
            label_of(
                &iri("mrp.workorder", 5),
                &[("moName", "WH/MO/00001"), ("wcName", "Assembly Line")]
            )
            .await
            .as_deref(),
            Some("WH/MO/00001 · Assembly Line"),
            "no operation, so the work center"
        );
        assert_eq!(
            label_of(&iri("mrp.workorder", 5), &[("moName", "WH/MO/00001")])
                .await
                .as_deref(),
            Some("WH/MO/00001")
        );
        assert_eq!(
            label_of(
                &iri("mrp.workcenter.capacity", 1),
                &[("capWcName", "Assembly Line")]
            )
            .await
            .as_deref(),
            Some("Assembly Line capacity")
        );
        assert_eq!(
            label_of(
                &iri("purchase.order.line", 3),
                &[("poName", "P00001"), ("productName", "Battery Pack 500Wh")]
            )
            .await
            .as_deref(),
            Some("P00001 · Battery Pack 500Wh"),
            "no quantity, so the product alone"
        );
        assert_eq!(
            label_of(&iri("sale.order", 1), &[("name", "S00001")])
                .await
                .as_deref(),
            Some("S00001")
        );
        // Nothing materialized, so no label (the view falls back to the short id).
        assert_eq!(label_of(&iri("sale.order", 1), &[]).await, None);
        assert_eq!(
            label_of(
                &iri("mrp.workcenter.capacity", 1),
                &[("capProductName", "E-Bike Model X")]
            )
            .await,
            None,
            "a product alone does not name a capacity record"
        );
    }

    #[tokio::test]
    async fn a_batch_is_read_once_in_request_order_without_duplicates_or_foreign_ids() {
        let mo = iri("mrp.production", 1);
        let wc = iri("mrp.workcenter", 1);
        let unnamed = iri("mrp.production", 9);
        let w = world(vec![
            row(&[("s", &wc), ("name", "Assembly Line")]),
            row(&[("s", &mo), ("name", "WH/MO/00001")]),
            row(&[("s", &unnamed)]),
            // A second row for the same id (a cross-product of optionals) is ignored.
            row(&[("s", &mo), ("name", "WH/MO/00001")]),
        ]);
        let hostile = format!("{mo}> }} DROP ALL #");
        let ids = vec![
            mo.clone(),
            "https://evil.example/x".to_owned(),
            wc.clone(),
            hostile.clone(),
            mo.clone(),
            unnamed.clone(),
        ];
        let labels = entity_labels(&w, &ids).await.expect("read");
        assert_eq!(
            labels,
            vec![
                (EntityUri(mo.clone()), "WH/MO/00001".to_owned()),
                (EntityUri(wc.clone()), "Assembly Line".to_owned()),
            ],
            "request order; the unnamed and foreign ids get none"
        );
        let asked = w.asked.lock().expect("lock").clone();
        assert_eq!(asked.len(), 1, "one read for the batch");
        let query = &asked[0];
        assert_eq!(query.matches(&format!("<{mo}>")).count(), 1, "asked once");
        assert!(!query.contains("evil.example") && !query.contains("DROP ALL"));
        // An empty (or all-foreign) batch reads nothing.
        let none = world(Vec::new());
        assert!(
            entity_labels(&none, &[hostile])
                .await
                .expect("read")
                .is_empty()
        );
        assert!(none.asked.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn a_change_label_refuses_a_non_canonical_object() {
        let hostile = EntityUri("https://ekg.example/id/sale.order/1> } DROP ALL #".to_owned());
        assert!(change_label(&world(Vec::new()), &hostile).await.is_err());
    }
}
