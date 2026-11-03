//! What the semantic layer says about one member of a portfolio: its short components with
//! their inbound lines, its contended work orders, and whether the monitor's own rules name a
//! cause at all. Read from the rule SELECTs the monitor evaluates, so it cannot drift from why a
//! member was flagged. A read of the world model; it commits nothing.

use ekg_domain::{EntityUri, Result, WorldModel};

use super::{Row, queries};
use crate::ontology;

/// One component a sales order is short of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortComponent {
    /// The component.
    pub component: String,
    /// Its open inbound lines.
    pub lines: Vec<String>,
    /// What else the chain names: its purchase orders and receipts.
    pub chain: Vec<String>,
    /// Whether some bill of materials makes it, so that no inbound line is needed.
    pub made: bool,
}

/// One work order over its work centre's capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContendedWork {
    /// The work order.
    pub work_order: String,
    /// Its work centre.
    pub work_centre: String,
    /// The work orders it overlaps.
    pub peers: Vec<String>,
}

/// What the supply-risk and contention rules hold about one member.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberRead {
    /// Its short components, in the rule's cause order.
    pub short: Vec<ShortComponent>,
    /// Its contended work orders.
    pub contended: Vec<ContendedWork>,
}

fn contended_of(member: &EntityUri, rows: &[Row]) -> Vec<ContendedWork> {
    rows.iter()
        .filter(|row| row.get("mo") == Some(&member.0))
        .filter(|row| {
            let capacity = row.get("capacity").and_then(|c| c.parse::<f64>().ok());
            let peers = row.get("overlapPeers").and_then(|p| p.parse::<i64>().ok());
            capacity
                .zip(peers)
                .and_then(|(capacity, peers)| ontology::over_capacity(capacity, peers))
                == Some(true)
        })
        .filter_map(|row| {
            Some(ContendedWork {
                work_order: row.get("wo")?.clone(),
                work_centre: row.get("wc")?.clone(),
                peers: row
                    .get("peers")
                    .map(|peers| peers.split(", ").map(str::to_owned).collect())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Whether a bill of materials produces `component`.
async fn is_made(world: &dyn WorldModel, component: &str) -> Result<bool> {
    let Some(component) = ontology::canonical_subject(component) else {
        return Ok(false);
    };
    let rows = world
        .select(&format!(
            "PREFIX ekg: <https://ekg.example/ontology#>\n\
             SELECT ?bom WHERE {{ ?bom ekg:produces_product <{component}> }} LIMIT 1"
        ))
        .await?;
    Ok(!rows.is_empty())
}

/// Read what the rules hold about `member`.
///
/// # Errors
/// A world-model read error, or a non-canonical component IRI in a rule row.
pub async fn member_read(world: &dyn WorldModel, member: &EntityUri) -> Result<MemberRead> {
    let risk = world.select(ontology::RISK_RULE_SELECT_SPARQL).await?;
    let mut components: Vec<String> = Vec::new();
    for row in risk.iter().filter(|row| row.get("so") == Some(&member.0)) {
        if let Some(component) = row.get("comp").filter(|c| !components.contains(c)) {
            components.push(component.clone());
        }
    }
    let chain_rows = match queries::cause_chain_query(components.iter().map(String::as_str))? {
        Some(query) => world.select(&query).await?,
        None => Vec::new(),
    };
    let mut short = Vec::with_capacity(components.len());
    for component in components {
        let of_component = || {
            chain_rows
                .iter()
                .filter(|row| row.get("cause") == Some(&component))
        };
        let mut lines: Vec<String> = of_component()
            .filter_map(|row| row.get("line").cloned())
            .collect();
        lines.sort();
        lines.dedup();
        let mut chain: Vec<String> = of_component()
            .flat_map(|row| ["po", "ship"].map(|key| row.get(key).cloned()))
            .flatten()
            .collect();
        chain.sort();
        chain.dedup();
        let made = is_made(world, &component).await?;
        short.push(ShortComponent {
            component,
            lines,
            chain,
            made,
        });
    }
    let contention = world
        .select(ontology::PROCESS_CONTENTION_SELECT_SPARQL)
        .await?;
    Ok(MemberRead {
        short,
        contended: contended_of(member, &contention),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn only_a_member_over_capacity_counts_as_contended() {
        let row = |mo: &str, wo: &str, peers: &str| -> Row {
            BTreeMap::from([
                ("mo".to_owned(), mo.to_owned()),
                ("wo".to_owned(), wo.to_owned()),
                ("wc".to_owned(), "urn:wc".to_owned()),
                ("capacity".to_owned(), "1".to_owned()),
                ("overlapPeers".to_owned(), peers.to_owned()),
                ("peers".to_owned(), "urn:a, urn:b".to_owned()),
            ])
        };
        let member = EntityUri("urn:mo".to_owned());
        let found = contended_of(
            &member,
            &[
                row("urn:mo", "urn:over", "1"),
                row("urn:mo", "urn:clear", "0"),
                row("urn:other", "urn:theirs", "2"),
            ],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].work_order, "urn:over");
        assert_eq!(found[0].peers, ["urn:a", "urn:b"]);
    }
}
