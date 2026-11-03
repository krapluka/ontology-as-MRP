//! A change row's "from" value, read from the materialized facts of the current world. A
//! value that is not materialized is omitted. The row's label is an entity label
//! ([`super::labels`]).

use std::collections::BTreeMap;

use ekg_domain::{EntityUri, Result, WorldModel};

use super::queries;
use crate::ontology;

type Row = BTreeMap<String, String>;

fn bound<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

/// A change's "from": the current value of the fact the verb moves on its target. An
/// expedite's from is the line's current arrival (its open receipt's ETA, else its planned
/// date), the date the risk rule reads. A verb that moves no fact (a notification), or an
/// unknown one, has none.
///
/// # Errors
/// A read error, or a non-canonical IRI.
pub async fn change_from(
    world: &dyn WorldModel,
    verb: &str,
    target: &EntityUri,
) -> Result<Option<String>> {
    if verb == "expedite_po" {
        let rows = world.select(&queries::line_arrival_query(target)?).await?;
        return Ok(rows.first().and_then(|row| {
            bound(row, "arrival")
                .or_else(|| bound(row, "planned"))
                .map(str::to_owned)
        }));
    }
    let Some(write) = crate::simulator::verb_write(verb) else {
        return Ok(None);
    };
    let Some(target) = ontology::canonical_subject(&target.0) else {
        return Err(ekg_domain::EngineError::Invariant(format!(
            "refusing to interpolate a non-canonical entity IRI into a display query: {:?}",
            target.0
        )));
    };
    let sparql = format!(
        "PREFIX ekg: <{vocab}>\nSELECT ?v WHERE {{ <{s}> ekg:{predicate} ?v }}",
        vocab = ontology::VOCAB_NAMESPACE,
        s = target.0,
        predicate = write.predicate(),
    );
    let rows = world.select(&sparql).await?;
    Ok(rows
        .first()
        .and_then(|row| bound(row, "v"))
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use async_trait::async_trait;
    use ekg_domain::{EngineError, Grounding, ReadContext, Revision, SourceRecord};

    use super::*;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A world answering the arrival and current-value queries from canned rows.
    struct CannedWorld {
        arrival: Row,
        current: Row,
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
            Ok(vec![if sparql.contains("SELECT ?arrival ?planned") {
                self.arrival.clone()
            } else {
                self.current.clone()
            }])
        }
    }

    fn world() -> CannedWorld {
        CannedWorld {
            arrival: Row::new(),
            current: Row::new(),
        }
    }

    fn iri(model: &str, id: i64) -> EntityUri {
        EntityUri(ontology::entity_iri(model, id))
    }

    #[tokio::test]
    async fn an_expedite_reads_from_the_lines_arrival_and_a_notify_has_no_from() {
        let line = iri("purchase.order.line", 3);
        let mut w = world();
        w.arrival = row(&[
            ("arrival", "2027-11-18T08:00:00"),
            ("planned", "2027-11-15T08:00:00"),
        ]);
        assert_eq!(
            change_from(&w, "expedite_po", &line)
                .await
                .expect("read")
                .as_deref(),
            Some("2027-11-18T08:00:00"),
            "the open receipt's ETA, not the planned date"
        );
        w.arrival = row(&[("planned", "2027-11-15T08:00:00")]);
        assert_eq!(
            change_from(&w, "expedite_po", &line)
                .await
                .expect("read")
                .as_deref(),
            Some("2027-11-15T08:00:00"),
            "nothing open, so the planned date"
        );
        assert_eq!(
            change_from(&w, "notify_owner", &iri("sale.order", 1))
                .await
                .expect("read"),
            None
        );
        w.current = row(&[("v", "2027-10-22T00:00:00")]);
        assert_eq!(
            change_from(&w, "re_promise_date", &iri("sale.order", 1))
                .await
                .expect("read")
                .as_deref(),
            Some("2027-10-22T00:00:00")
        );
    }
}
