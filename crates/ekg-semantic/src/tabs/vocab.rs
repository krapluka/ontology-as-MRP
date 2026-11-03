//! The tab row vocabulary: per tab, every row key its projection can serve and the literal
//! values of its enum-valued keys, taken from the tab queries themselves plus the keys and
//! values the Rust row builder adds. Exported as `apps/frontend/tab-rows.json`, so no frontend
//! fixture can carry a key or value the live API cannot produce.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::Tab;
use super::queries;
use super::rows::{MARKER_KEYS, OTD_EVIDENCE_KEYS, SOURCING_ROLES};

/// Timeline row kinds (bound in `timeline.rq`).
pub const TIMELINE_KINDS: [&str; 3] = ["promise", "ready", "inbound"];
/// Graph row kinds (bound in `graph.rq`).
pub const GRAPH_KINDS: [&str; 2] = ["node", "edge"];
/// Graph node roles: the six columns, Product and BOM sharing one (bound in `graph.rq`).
pub const GRAPH_ROLES: [&str; 7] = [
    "Customer",
    "SalesOrder",
    "Product",
    "BOM",
    "Component",
    "Supplier",
    "PurchaseOrder",
];
/// Flow stages (bound in `flow.rq`).
pub const FLOW_STAGES: [&str; 5] = [
    "suppliers",
    "on_order",
    "in_stock",
    "in_production",
    "promised",
];
/// Suppliers row kinds (bound in `suppliers.rq` / `suppliers-detail.rq`).
pub const SUPPLIERS_KINDS: [&str; 4] = ["supplier", "open_po", "stock", "dependent_order"];

/// The enum-valued keys whose literals the queries bind.
#[cfg(test)]
const QUERY_ENUM_KEYS: [&str; 3] = ["kind", "stage", "role"];

/// One tab's served row vocabulary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TabVocabulary {
    /// Every key a row of this tab can carry.
    pub keys: BTreeSet<String>,
    /// Each enum-valued key's declared values.
    pub enums: BTreeMap<String, BTreeSet<String>>,
}

/// The keys the Rust row builder adds to a tab's rows (markers + derived fields).
fn added_keys(tab: Tab) -> Vec<&'static str> {
    let mut keys = MARKER_KEYS.to_vec();
    match tab {
        Tab::Flow => keys.push("sourcing"),
        Tab::Suppliers => keys.extend(["sourcing", "otd", "exposure"]),
        Tab::Timeline | Tab::Graph => {}
    }
    keys
}

/// The keys the Rust row builder consumes and never serves.
fn dropped_keys(tab: Tab) -> &'static [&'static str] {
    match tab {
        Tab::Suppliers => &OTD_EVIDENCE_KEYS,
        _ => &[],
    }
}

/// The declared enum values of a tab, keyed by enum key.
fn declared_enums(tab: Tab) -> BTreeMap<&'static str, Vec<&'static str>> {
    match tab {
        Tab::Timeline => BTreeMap::from([("kind", TIMELINE_KINDS.to_vec())]),
        Tab::Graph => BTreeMap::from([
            ("kind", GRAPH_KINDS.to_vec()),
            ("role", GRAPH_ROLES.to_vec()),
        ]),
        Tab::Flow => BTreeMap::from([
            ("stage", FLOW_STAGES.to_vec()),
            ("sourcing", SOURCING_ROLES.to_vec()),
        ]),
        Tab::Suppliers => BTreeMap::from([
            ("kind", SUPPLIERS_KINDS.to_vec()),
            ("sourcing", SOURCING_ROLES.to_vec()),
        ]),
    }
}

/// The enum values the Rust layer adds rather than a query binding them.
#[cfg(test)]
fn rust_added_enums(tab: Tab) -> BTreeMap<&'static str, Vec<&'static str>> {
    match tab {
        Tab::Flow | Tab::Suppliers => BTreeMap::from([("sourcing", SOURCING_ROLES.to_vec())]),
        Tab::Timeline | Tab::Graph => BTreeMap::new(),
    }
}

/// Every tab's row vocabulary, keyed by [`Tab::name`]. The source of `tab-rows.json`.
#[must_use]
pub fn row_vocabulary() -> BTreeMap<&'static str, TabVocabulary> {
    Tab::ALL
        .into_iter()
        .map(|tab| {
            let mut keys: BTreeSet<String> = queries::templates(tab)
                .iter()
                .flat_map(|q| select_variables(q))
                .collect();
            for key in dropped_keys(tab) {
                keys.remove(*key);
            }
            keys.extend(added_keys(tab).into_iter().map(str::to_owned));
            let enums = declared_enums(tab)
                .into_iter()
                .map(|(key, values)| {
                    (
                        key.to_owned(),
                        values.into_iter().map(str::to_owned).collect(),
                    )
                })
                .collect();
            (tab.name(), TabVocabulary { keys, enums })
        })
        .collect()
}

/// The variables a query's top-level `SELECT` projects, in order: a plain `?var` at paren
/// depth 0, or the `?var` after `AS` in a `(expression AS ?var)`.
#[must_use]
pub fn select_variables(sparql: &str) -> Vec<String> {
    let text = queries::strip_comments(sparql);
    let Some(start) = text.find("SELECT") else {
        return Vec::new();
    };
    let clause = &text[start + "SELECT".len()..];
    let clause = clause.find("WHERE").map_or(clause, |end| &clause[..end]);
    let spaced = clause.replace('(', " ( ").replace(')', " ) ");
    let (mut depth, mut previous, mut out) = (0_usize, "", Vec::new());
    for token in spaced.split_whitespace() {
        match token {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            var if var.starts_with('?') && (depth == 0 || previous.eq_ignore_ascii_case("AS")) => {
                out.push(var.trim_start_matches('?').to_owned());
            }
            _ => {}
        }
        previous = token;
    }
    out
}

/// Every enum literal a `BIND(… AS ?key)` in `sparql` can bind, per enum `key`: the roles,
/// stages and kinds the query can serve (the empty `""` sentinel excluded). `BIND` and `AS`
/// match in any case and spacing. Only result literals count: a bare literal, or an
/// `IF(condition, then, else)`'s two branches, recursively. A condition's literals never do.
///
/// # Errors
/// Any other expression carrying a string literal is refused (named in the error), so a new
/// enum shape cannot slip past the vocabulary test unread.
#[cfg(test)]
fn bound_enum_literals(sparql: &str) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let chars: Vec<char> = queries::strip_comments(sparql).chars().collect();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut at = 0;
    while let Some(open) = find_keyword_call(&chars, at, "BIND") {
        at = open + 1;
        let close = matching_paren(&chars, open)
            .ok_or_else(|| format!("unbalanced BIND at char {open}"))?;
        let span = &chars[open + 1..close];
        let Some(as_at) = last_top_level_as(span) else {
            continue;
        };
        let key: String = span[as_at + 2..].iter().collect();
        let key = key.trim().trim_start_matches('?').to_owned();
        if !QUERY_ENUM_KEYS.contains(&key.as_str()) {
            continue;
        }
        let mut literals = BTreeSet::new();
        branch_literals(&span[..as_at], &mut literals)?;
        literals.remove("");
        out.entry(key).or_default().extend(literals);
    }
    Ok(out)
}

/// The index of the `(` opening the next `keyword(` call at or after `from`: the keyword
/// matched in any case at a word boundary, any whitespace before the parenthesis.
#[cfg(test)]
fn find_keyword_call(chars: &[char], from: usize, keyword: &str) -> Option<usize> {
    let word: Vec<char> = keyword.chars().collect();
    let mut in_string = false;
    for i in from..chars.len() {
        if chars[i] == '"' {
            in_string = !in_string;
        }
        if in_string || i + word.len() > chars.len() {
            continue;
        }
        let matches = chars[i..i + word.len()]
            .iter()
            .zip(&word)
            .all(|(c, w)| c.eq_ignore_ascii_case(w));
        let boundary = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        if !matches || !boundary {
            continue;
        }
        let mut j = i + word.len();
        while chars.get(j).is_some_and(|c| c.is_whitespace()) {
            j += 1;
        }
        if chars.get(j) == Some(&'(') {
            return Some(j);
        }
    }
    None
}

/// The index of the `)` closing the `(` at `open` (string literals may hold parentheses).
#[cfg(test)]
fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let (mut depth, mut in_string) = (0_i64, false);
    for (i, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The index of the last top-level `AS` keyword (any case, whitespace or `)`/`"` before it,
/// whitespace or `?` after it) in a `BIND`'s span.
#[cfg(test)]
fn last_top_level_as(span: &[char]) -> Option<usize> {
    let (mut depth, mut in_string, mut found) = (0_i64, false, None);
    for (i, &c) in span.iter().enumerate() {
        match c {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            _ if !in_string && depth == 0 && c.eq_ignore_ascii_case(&'a') => {
                let s_next = span
                    .get(i + 1)
                    .is_some_and(|c| c.eq_ignore_ascii_case(&'s'));
                let before = i
                    .checked_sub(1)
                    .and_then(|j| span.get(j))
                    .is_some_and(|c| c.is_whitespace() || matches!(c, ')' | '"'));
                let after = span
                    .get(i + 2)
                    .is_some_and(|c| c.is_whitespace() || *c == '?');
                if s_next && before && after {
                    found = Some(i);
                }
            }
            _ => {}
        }
    }
    found
}

/// Collect the result literals of an enum expression: a bare literal, or an `IF`'s two
/// branches (recursively). Refuse any other expression that carries a literal.
#[cfg(test)]
fn branch_literals(expression: &[char], out: &mut BTreeSet<String>) -> Result<(), String> {
    let text: String = expression.iter().collect();
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if !trimmed.contains('"') {
        return Ok(()); // a variable or IRI: no literal to declare
    }
    if chars.first() == Some(&'"') && chars.len() >= 2 && chars.last() == Some(&'"') {
        let inner: String = chars[1..chars.len() - 1].iter().collect();
        if !inner.contains('"') {
            out.insert(inner);
            return Ok(());
        }
    }
    if let Some(open) = find_keyword_call(&chars, 0, "IF").filter(|open| {
        chars[..2]
            .iter()
            .collect::<String>()
            .eq_ignore_ascii_case("IF")
            && matching_paren(&chars, *open) == Some(chars.len() - 1)
    }) {
        let args = split_top_level_commas(&chars[open + 1..chars.len() - 1]);
        if let [_condition, then, otherwise] = args.as_slice() {
            branch_literals(then, out)?;
            return branch_literals(otherwise, out);
        }
    }
    Err(format!(
        "an enum BIND must bind a literal or an IF chain of literals, got: {trimmed}"
    ))
}

/// Split a call's argument list at its top-level commas.
#[cfg(test)]
fn split_top_level_commas(args: &[char]) -> Vec<&[char]> {
    let (mut depth, mut in_string, mut start, mut out) = (0_i64, false, 0, Vec::new());
    for (i, &c) in args.iter().enumerate() {
        match c {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            ',' if !in_string && depth == 0 => {
                out.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&args[start..]);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::too_many_lines)]
    use super::*;

    #[test]
    fn the_select_parser_sees_every_projected_variable() {
        assert_eq!(
            select_variables(
                "# SELECT ?commented WHERE\nSELECT DISTINCT ?a ?b\n  (MAX(?c) AS ?d) ?e WHERE { ?a ?b ?c }"
            ),
            ["a", "b", "d", "e"]
        );
        // Every key parsed from a served query is a variable its WHERE really binds.
        for tab in Tab::ALL {
            for query in queries::templates(tab) {
                let vars = select_variables(query);
                assert!(!vars.is_empty(), "{}: no variables parsed", tab.name());
                let body = queries::strip_comments(query);
                let where_at = body.find("WHERE").expect("a WHERE");
                for var in vars {
                    assert!(
                        body[where_at..].contains(&format!("?{var}")),
                        "{}: ?{var} is projected but never bound",
                        tab.name()
                    );
                }
            }
        }
        assert_eq!(
            select_variables(queries::TIMELINE_RQ),
            [
                "kind",
                "entity",
                "date",
                "label",
                "order_value",
                "product",
                "quantity",
                "supplier"
            ]
        );
    }

    #[test]
    fn every_bound_enum_literal_is_declared_and_every_declared_value_is_served() {
        for tab in Tab::ALL {
            let mut bound: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for query in queries::templates(tab) {
                let literals = bound_enum_literals(query)
                    .unwrap_or_else(|err| panic!("{}: {err}", tab.name()));
                for (key, values) in literals {
                    bound.entry(key).or_default().extend(values);
                }
            }
            let declared = declared_enums(tab);
            let rust_added = rust_added_enums(tab);
            for (key, values) in &bound {
                let known = declared.get(key.as_str()).unwrap_or_else(|| {
                    panic!(
                        "{}: `{key}` is bound in a query but not declared",
                        tab.name()
                    )
                });
                for value in values {
                    assert!(
                        known.contains(&value.as_str()),
                        "{}: {key}={value:?} is bound in a query but not declared",
                        tab.name()
                    );
                }
            }
            for (key, values) in &declared {
                for value in values {
                    let from_rust = rust_added
                        .get(key)
                        .is_some_and(|added| added.contains(value));
                    let from_query = bound.get(*key).is_some_and(|b| b.contains(*value));
                    assert!(
                        from_rust || from_query,
                        "{}: {key}={value:?} is declared but neither a query nor Rust serves it",
                        tab.name()
                    );
                }
            }
        }
    }

    #[test]
    fn the_vocabulary_names_the_markers_and_derived_keys_but_not_the_evidence() {
        let vocab = row_vocabulary();
        assert_eq!(
            vocab.keys().copied().collect::<Vec<_>>(),
            ["flow", "graph", "suppliers", "timeline"]
        );
        for tab in vocab.values() {
            for marker in MARKER_KEYS {
                assert!(tab.keys.contains(marker));
            }
        }
        let suppliers = &vocab["suppliers"];
        for key in [
            "otd",
            "exposure",
            "sourcing",
            "demand",
            "lead_time",
            "on_hand",
        ] {
            assert!(suppliers.keys.contains(key), "suppliers serves {key}");
        }
        for evidence in OTD_EVIDENCE_KEYS {
            assert!(
                !suppliers.keys.contains(evidence),
                "{evidence} is never served"
            );
        }
        assert_eq!(
            vocab["flow"].enums["stage"],
            FLOW_STAGES.iter().map(|s| (*s).to_owned()).collect()
        );
        assert!(vocab["graph"].keys.contains("role") && vocab["graph"].keys.contains("label"));
    }

    #[test]
    fn enum_literals_are_read_from_the_bind_that_binds_the_key() {
        let literals = bound_enum_literals(
            "SELECT ?stage WHERE { FILTER(?x != \"done\") \
             BIND(IF(?c = ekg:A, \"a (x)\", IF(?c = ekg:B, \"b\", \"\")) AS ?stage) \
             BIND(\"ignored\" AS ?other) }",
        )
        .expect("an IF chain of literals");
        assert_eq!(
            literals,
            BTreeMap::from([(
                "stage".to_owned(),
                BTreeSet::from(["a (x)".to_owned(), "b".to_owned()])
            )])
        );
        // Any case and spacing; a condition's literal is not a served value.
        let literals = bound_enum_literals(
            "SELECT ?kind WHERE { bind ( \"x\"  as\t?kind ) \
             Bind(If(?s = \"done\", \"y\", \"z\")AS ?kind) }",
        )
        .expect("literals");
        assert_eq!(
            literals["kind"],
            BTreeSet::from(["x".to_owned(), "y".to_owned(), "z".to_owned()]),
            "`done` is compared against, never bound"
        );
        // Any other literal-carrying expression is refused rather than half-read.
        assert!(bound_enum_literals("BIND(CONCAT(\"a\", ?x) AS ?kind)").is_err());
        assert!(bound_enum_literals("BIND(CONCAT(\"a\", ?x) AS ?other)").is_ok());
    }

    /// Every declared kind/stage/role, run through the real row builder with every marker
    /// input known and one entity standing in for every chain hop: each served key and enum
    /// value is in the vocabulary, every key the builder adds is served at least once, and
    /// every declared value is classified onto a chain hop (served `on_path="true"`), so a
    /// value the builder does not know (served as an off-path `Other`) fails here.
    #[test]
    fn the_row_builder_serves_only_the_vocabulary_and_places_every_declared_value() {
        use super::super::causes::CauseMap;
        use super::super::chain::{Chain, PathHops};
        use super::super::rows::{Markers, SourcingLink, build_rows};
        use crate::ontology;

        let x = ontology::entity_iri("product.product", 1);
        let path = PathHops {
            customer: Some(x.clone()),
            order: Some(x.clone()),
            product: Some(x.clone()),
            bom: Some(x.clone()),
            mo: Some(x.clone()),
        };
        let chain = Chain {
            component: Some(x.clone()),
            supplier: Some(x.clone()),
            po: Some(x.clone()),
            po_late: true,
            ..Chain::default()
        };
        let at_risk = BTreeSet::from([x.clone()]);
        let causes = CauseMap::from_rows(
            &[BTreeMap::from([
                ("so".to_owned(), x.clone()),
                ("comp".to_owned(), x.clone()),
                ("shortfall".to_owned(), "1".to_owned()),
            ])],
            &[],
        );
        let markers = Markers {
            subject: &x,
            at_risk: Some(&at_risk),
            causes: Some(&causes),
            path: &path,
            chain: Some(&chain),
        };
        let sourcing = vec![SourcingLink {
            supplier: x.clone(),
            component: x.clone(),
            sequence: Some(1),
        }];
        let vocab = row_vocabulary();
        for tab in Tab::ALL {
            let name = tab.name();
            // Per query: its own projected variables, and one row per enum value it binds.
            let mut structural = Vec::new();
            for query in queries::templates(tab) {
                let base: super::super::Row = select_variables(query)
                    .into_iter()
                    .filter(|v| !QUERY_ENUM_KEYS.contains(&v.as_str()))
                    .map(|v| {
                        let value = match v.as_str() {
                            "completed" => "2",
                            "on_time" => "1",
                            "order_value" => "10",
                            _ => x.as_str(),
                        };
                        (v, value.to_owned())
                    })
                    .collect();
                let literals = bound_enum_literals(query).expect("readable enum binds");
                for (key, values) in literals {
                    for value in values {
                        let mut row = base.clone();
                        if key == "role" {
                            row.insert("kind".to_owned(), "node".to_owned());
                        }
                        if key == "kind" && value == "node" {
                            row.insert("role".to_owned(), "Customer".to_owned());
                        }
                        row.insert(key.clone(), value);
                        structural.push(row);
                    }
                }
            }
            assert!(!structural.is_empty(), "{name}: declares values");
            let served = build_rows(tab, structural, &sourcing, &markers);
            let tab_vocab = &vocab[name];
            let mut seen_keys = BTreeSet::new();
            for row in &served {
                for (key, value) in row {
                    assert!(
                        tab_vocab.keys.contains(key),
                        "{name}: `{key}` is served but not in the vocabulary: {row:?}"
                    );
                    if let Some(allowed) = tab_vocab.enums.get(key) {
                        assert!(
                            allowed.contains(value),
                            "{name}: {key}={value:?} undeclared"
                        );
                    }
                    seen_keys.insert(key.as_str());
                }
                assert_eq!(
                    row.get("on_path").map(String::as_str),
                    Some("true"),
                    "{name}: this declared value is not placed on a chain hop: {row:?}"
                );
            }
            for key in added_keys(tab) {
                assert!(
                    seen_keys.contains(key),
                    "{name}: the vocabulary declares `{key}` but the builder never serves it"
                );
            }
        }
    }
}
