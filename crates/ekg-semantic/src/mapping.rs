//! The declarative source-to-ontology mapping contract.
//!
//! A source record is a bag of string fields (`ekg_domain::SourceRecord`); the
//! world model needs typed entities (`rdf:type`, typed datatype literals, and
//! object relations to other entities) for the SHACL shapes and the SPARQL risk
//! rule to traverse. The contract is a versioned document that says, per source
//! model, which `ekg:` class it becomes and how each field projects.
//!
//! Nothing here names a supply-chain concept. The supply-risk contract itself is
//! configuration (`ontology::CONTRACT_YAML`): a second source is a new contract,
//! not a new projector.
//!
//! The contract is validated against the ontology at load (every class and
//! property it references must be declared there). The loader fails rather than
//! serve a mapping that has drifted from the ontology.

use std::collections::{BTreeMap, BTreeSet};

use ekg_domain::{EngineError, Result, SourceRecord};
use serde::Deserialize;

use crate::ontology;

/// The XSD namespace. Datatype literals are emitted with a full datatype IRI so
/// the projection needs no prefix prolog beyond `ekg:`.
const XSD_NAMESPACE: &str = "http://www.w3.org/2001/XMLSchema#";

/// The datatypes a contract field may declare (the `xsd:` local names the
/// projection knows how to emit). `string` is projected as a plain literal (whose
/// datatype is `xsd:string` in RDF 1.1); the rest carry an explicit datatype IRI.
const ALLOWED_DATATYPES: &[&str] = &[
    "string", "integer", "decimal", "dateTime", "date", "boolean", "long",
];

/// The comparison operators a model's source `scope` term may use (source-query domain
/// operators). `in` / `not in` take a list value; the others a scalar.
const ALLOWED_SCOPE_OPS: &[&str] = &["=", "!=", "in", "not in"];

/// A model's own archive flag. A scope term on it would disable the source's implicit
/// active-only filter, so [`check_scope_term`] refuses it, for the contract loader and for a
/// scope handed to the ingestor directly (`OdooConfig.scopes`) alike.
pub(crate) const ACTIVE_FIELDS: &[&str] = &["active", "x_active"];

// --- parsed contract (wire) ---

/// The YAML wire shape, deserialized before validation.
#[derive(Debug, Deserialize)]
struct RawContract {
    version: u32,
    source: String,
    models: BTreeMap<String, RawModel>,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    class: String,
    #[serde(default)]
    fields: BTreeMap<String, RawField>,
    #[serde(default)]
    variants: Vec<RawVariant>,
    /// Optional source scope: only the source's records matching every term are part of the
    /// world model (e.g. stock at internal locations only). Neutral: a field, an operator and
    /// a value over the source's own fields; the loader names no model, and the only fields it
    /// knows are the source's archive flags ([`ACTIVE_FIELDS`]), which it refuses.
    #[serde(default)]
    scope: Vec<RawScopeTerm>,
}

#[derive(Debug, Deserialize)]
struct RawScopeTerm {
    field: String,
    op: String,
    value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct RawField {
    property: String,
    #[serde(default)]
    datatype: Option<String>,
    #[serde(default, rename = "ref")]
    ref_model: Option<String>,
}

/// A role a record takes on when a numeric field exceeds a threshold, e.g. a
/// party with `supplier_rank > 0` is also an `ekg:Supplier`. Kept generic (a field
/// and a threshold) so the loader carries no role vocabulary.
#[derive(Debug, Deserialize)]
struct RawVariant {
    when: RawWhen,
    class: String,
}

#[derive(Debug, Deserialize)]
struct RawWhen {
    field: String,
    gt: i64,
}

// --- validated contract ---

/// How one source field projects: a typed datatype literal or an object relation
/// to another entity.
#[derive(Debug, Clone)]
enum FieldMapping {
    /// A datatype property carrying a typed literal (`datatype` is an `xsd:` local
    /// name from [`ALLOWED_DATATYPES`]).
    Datatype { property: String, datatype: String },
    /// An object property whose value(s) are references to `target_model` records,
    /// resolved to their entity IRIs.
    Reference {
        property: String,
        target_model: String,
    },
}

/// A role class added when `field`'s value is greater than `threshold`.
#[derive(Debug, Clone)]
struct Variant {
    field: String,
    threshold: i64,
    class: String,
}

/// The mapping for one source model.
#[derive(Debug, Clone)]
struct ModelMapping {
    class: String,
    fields: BTreeMap<String, FieldMapping>,
    variants: Vec<Variant>,
    /// The validated source-scope terms (`field`, `op`, `value`); empty = unscoped.
    scope: Vec<(String, String, serde_json::Value)>,
}

/// A parsed, ontology-validated mapping contract, ready to project source records
/// into typed entity graphs.
#[derive(Debug, Clone)]
pub struct MappingContract {
    version: u32,
    source: String,
    models: BTreeMap<String, ModelMapping>,
}

impl MappingContract {
    /// Parse `yaml` and validate every class and property it references against
    /// the declared terms of `ontology_ttl`.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] if the YAML is malformed, a field declares
    /// neither or both of `datatype`/`ref`, a datatype is unknown, or a referenced
    /// `ekg:` class/property is absent from the ontology. The error names the
    /// offending term.
    pub fn load(yaml: &str, ontology_ttl: &str) -> Result<Self> {
        let raw: RawContract = serde_saphyr::from_str(yaml).map_err(|e| {
            EngineError::Invariant(format!("mapping contract is not valid YAML: {e}"))
        })?;
        let terms = declared_terms(ontology_ttl);

        let mut models = BTreeMap::new();
        for (model, raw_model) in raw.models {
            require_class(&terms, &raw_model.class, &model)?;

            let mut fields = BTreeMap::new();
            for (field, raw_field) in raw_model.fields {
                fields.insert(
                    field.clone(),
                    validate_field(&terms, &model, &field, raw_field)?,
                );
            }

            let mut variants = Vec::new();
            for raw_variant in raw_model.variants {
                require_class(&terms, &raw_variant.class, &model)?;
                variants.push(Variant {
                    field: raw_variant.when.field,
                    threshold: raw_variant.when.gt,
                    class: raw_variant.class,
                });
            }

            let scope = raw_model
                .scope
                .into_iter()
                .map(|term| validate_scope_term(&model, term))
                .collect::<Result<Vec<_>>>()?;

            models.insert(
                model,
                ModelMapping {
                    class: raw_model.class,
                    fields,
                    variants,
                    scope,
                },
            );
        }

        Ok(Self {
            version: raw.version,
            source: raw.source,
            models,
        })
    }

    /// The contract version (for provenance/audit; not otherwise interpreted).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The source system this contract maps (e.g. `"odoo"`).
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Each scoped model's source scope as source-query domain terms (`[field, op, value]`
    /// each), all of which a record must match to be part of the world model. Unscoped models
    /// are absent. The ingestor ANDs these into its listings and pull.
    #[must_use]
    pub fn source_scopes(&self) -> BTreeMap<String, Vec<serde_json::Value>> {
        self.models
            .iter()
            .filter(|(_, mapping)| !mapping.scope.is_empty())
            .map(|(model, mapping)| {
                let terms = mapping
                    .scope
                    .iter()
                    .map(|(field, op, value)| serde_json::json!([field, op, value]))
                    .collect();
                (model.clone(), terms)
            })
            .collect()
    }

    /// Project `record` into its typed entity graph body (Turtle triples, no
    /// prefix prolog): the neutral `ekg:GovernedEntity` type plus the mapped
    /// domain class (and any role variant), the mapped fields as typed literals or
    /// object relations, and the mandatory `ekg:wasDerivedFrom` provenance to an
    /// `ekg:SourceRecord`. Shared by the Turtle body and the SPARQL `INSERT`.
    ///
    /// # Errors
    /// [`EngineError::Invariant`] if the record's model is not in the contract, or
    /// a reference field's value is not a well-formed list of source ids. An
    /// unmapped model or a malformed edge is an error, not dropped data.
    pub fn project(&self, record: &SourceRecord, entity: &str) -> Result<String> {
        let sr = &record.source_ref;
        let model = self.models.get(&sr.model).ok_or_else(|| {
            EngineError::Invariant(format!(
                "mapping contract has no mapping for source model {:?}",
                sr.model
            ))
        })?;

        // rdf:type list: the neutral governance base class, the domain class, and
        // any role variant whose threshold the record clears.
        let mut types = vec![
            "ekg:GovernedEntity".to_owned(),
            format!("ekg:{}", model.class),
        ];
        for variant in &model.variants {
            if field_as_i64(record, &variant.field) > variant.threshold {
                types.push(format!("ekg:{}", variant.class));
            }
        }

        // One predicate-object line per mapped field present on the record.
        let mut lines: Vec<String> = Vec::new();
        for (field, mapping) in &model.fields {
            let Some(value) = record.fields.get(field) else {
                continue;
            };
            match mapping {
                FieldMapping::Datatype { property, datatype } => {
                    lines.push(format!("ekg:{property} {}", typed_literal(value, datatype)));
                }
                FieldMapping::Reference {
                    property,
                    target_model,
                } => {
                    for id in parse_ref_ids(value).map_err(|e| {
                        EngineError::Invariant(format!(
                            "field {field:?} of {:?} is not a valid reference: {e}",
                            sr.model
                        ))
                    })? {
                        let target = ontology::entity_iri(target_model, id);
                        lines.push(format!("ekg:{property} <{target}>"));
                    }
                }
            }
        }

        // Provenance: the entity traces to its source record.
        let source_node = format!("{entity}#source");
        lines.push(format!("ekg:wasDerivedFrom <{source_node}>"));

        let body = lines.join(" ;\n    ");
        Ok(format!(
            "<{entity}> a {types} ;\n    {body} .\n\
             <{source_node}> a ekg:SourceRecord ;\n\
             \x20   ekg:source_system \"{system}\" ;\n\
             \x20   ekg:source_model \"{model_name}\" ;\n\
             \x20   ekg:source_id {id} .\n",
            types = types.join(", "),
            system = escape_literal(&sr.system),
            model_name = escape_literal(&sr.model),
            id = sr.id,
        ))
    }
}

// --- validation helpers ---

/// The set of `ekg:` local names declared in the ontology. A declaration
/// subject sits alone on its line as `ekg:LocalName` (the authored Turtle style);
/// a usage always shares its line with a predicate, so this captures the declared
/// classes and properties, not terms merely mentioned in an object position.
fn declared_terms(ontology_ttl: &str) -> BTreeSet<String> {
    ontology_ttl
        .lines()
        .filter_map(|line| {
            let name = line.trim().strip_prefix("ekg:")?;
            (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then(|| name.to_owned())
        })
        .collect()
}

/// Require that `class` (an `ekg:` local name) is declared in the ontology.
fn require_class(terms: &BTreeSet<String>, class: &str, model: &str) -> Result<()> {
    if terms.contains(class) {
        Ok(())
    } else {
        Err(EngineError::Invariant(format!(
            "mapping contract for model {model:?} references ekg:{class}, which is not \
             declared in the ontology"
        )))
    }
}

/// Validate one source-scope term: a non-empty field that is not the model's own archive flag
/// ([`ACTIVE_FIELDS`]; naming it would widen reconciliation to archived records), an operator
/// from [`ALLOWED_SCOPE_OPS`], and a value of the operator's arity (a list for `in` /
/// `not in`, a scalar otherwise). Fails loud, naming the model: a malformed scope must never
/// silently widen or empty what the world model ingests.
fn validate_scope_term(
    model: &str,
    term: RawScopeTerm,
) -> Result<(String, String, serde_json::Value)> {
    let RawScopeTerm { field, op, value } = term;
    check_scope_term(model, &field, &op, &value)
        .map_err(|why| EngineError::Invariant(format!("mapping contract: {why}")))?;
    Ok((field, op, value))
}

/// The rule set for a source-scope term `[field, op, value]`, shared by the contract loader
/// and by the ingestor for a scope handed to it directly, so the two can never drift: a
/// non-empty field that is not the model's own archive flag ([`ACTIVE_FIELDS`]), an operator
/// from [`ALLOWED_SCOPE_OPS`], and a value of the operator's arity. `in []` would silently
/// empty the model's listing (and reconciliation would retract all of it), and `not in []`
/// would silently do nothing, so a list must be non-empty.
///
/// # Errors
/// Why the term is refused, naming the model.
pub(crate) fn check_scope_term(
    model: &str,
    field: &str,
    op: &str,
    value: &serde_json::Value,
) -> std::result::Result<(), String> {
    if field.trim().is_empty() {
        return Err(format!("{model} scope term has an empty field"));
    }
    // Reconciliation lists only active records, and Odoo drops its implicit `active = True`
    // filter as soon as a domain names the model's own active field, so a scope on it would
    // silently widen reconciliation to archived records.
    if ACTIVE_FIELDS.contains(&field) {
        return Err(format!(
            "{model} scope term on `{field}` is refused: scoping the active field would widen \
             reconciliation to archived records"
        ));
    }
    if !ALLOWED_SCOPE_OPS.contains(&op) {
        return Err(format!(
            "{model} scope term on `{field}` uses unknown operator `{op}` (allowed: {})",
            ALLOWED_SCOPE_OPS.join(", ")
        ));
    }
    let scalar = |v: &serde_json::Value| !(v.is_array() || v.is_object() || v.is_null());
    let list_op = op == "in" || op == "not in";
    let well_shaped = if list_op {
        value
            .as_array()
            .is_some_and(|items| !items.is_empty() && items.iter().all(scalar))
    } else {
        scalar(value)
    };
    if !well_shaped {
        return Err(format!(
            "{model} scope term `{field} {op}` needs a {} value, got {value}",
            if list_op {
                "non-empty list of scalars"
            } else {
                "scalar"
            }
        ));
    }
    Ok(())
}

/// Validate one field mapping: exactly one of `datatype`/`ref`, a known datatype,
/// and a declared property term.
fn validate_field(
    terms: &BTreeSet<String>,
    model: &str,
    field: &str,
    raw: RawField,
) -> Result<FieldMapping> {
    if !terms.contains(&raw.property) {
        return Err(EngineError::Invariant(format!(
            "mapping contract field {model}.{field} maps to ekg:{}, which is not declared \
             in the ontology",
            raw.property
        )));
    }
    match (raw.datatype, raw.ref_model) {
        (Some(_), Some(_)) => Err(EngineError::Invariant(format!(
            "mapping contract field {model}.{field} declares both datatype and ref"
        ))),
        (None, None) => Err(EngineError::Invariant(format!(
            "mapping contract field {model}.{field} declares neither datatype nor ref"
        ))),
        (Some(datatype), None) => {
            let local = datatype.strip_prefix("xsd:").unwrap_or(&datatype);
            if !ALLOWED_DATATYPES.contains(&local) {
                return Err(EngineError::Invariant(format!(
                    "mapping contract field {model}.{field} declares unknown datatype {datatype:?}"
                )));
            }
            Ok(FieldMapping::Datatype {
                property: raw.property,
                datatype: local.to_owned(),
            })
        }
        (None, Some(target_model)) => Ok(FieldMapping::Reference {
            property: raw.property,
            target_model,
        }),
    }
}

// --- projection helpers ---

/// A typed literal: a plain literal for `string` (its datatype is `xsd:string`),
/// otherwise an explicit datatype IRI so no `xsd:` prefix prolog is needed.
fn typed_literal(value: &str, datatype: &str) -> String {
    // Odoo returns datetimes space-separated ("YYYY-MM-DD HH:MM:SS"), which is not a
    // valid xsd:dateTime: ISO 8601 requires the 'T' separator, and the store's SHACL
    // datatype check rejects the space form (failing the whole materialization commit).
    // Normalise dateTime literals to ISO; other datatypes pass through unchanged.
    let normalized = if datatype == "dateTime" {
        normalize_datetime(value)
    } else {
        value.to_owned()
    };
    let escaped = escape_literal(&normalized);
    if datatype == "string" {
        format!("\"{escaped}\"")
    } else {
        format!("\"{escaped}\"^^<{XSD_NAMESPACE}{datatype}>")
    }
}

/// Normalise an Odoo datetime (`YYYY-MM-DD HH:MM:SS`) to ISO 8601
/// (`YYYY-MM-DDTHH:MM:SS`) by replacing the first space with `T`. A value already in
/// ISO form (or carrying no space) is returned unchanged.
fn normalize_datetime(value: &str) -> String {
    match value.split_once(' ') {
        Some((date, time)) => format!("{date}T{time}"),
        None => value.to_owned(),
    }
}

/// Parse a reference field value into source ids. Accepts a bare id (`"7"`) or a
/// bracketed/plain list (`"[12, 13]"`, `"12,13"`), the normalized wire form the
/// ingestion layer produces from Odoo many2one/one2many fields. Every token must
/// be a clean integer id; a malformed token is an error (an untrusted source
/// value must not silently drop an edge).
fn parse_ref_ids(value: &str) -> std::result::Result<Vec<i64>, String> {
    let inner = value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|tok| {
            let tok = tok.trim();
            tok.parse::<i64>()
                .map_err(|_| format!("{tok:?} is not an integer source id"))
        })
        .collect()
}

/// Read a record field as an `i64`, defaulting to `0` when the field is absent or
/// not an integer (used to evaluate role-variant thresholds).
fn field_as_i64(record: &SourceRecord, field: &str) -> i64 {
    record
        .fields
        .get(field)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Escape a string for use inside a Turtle/SPARQL double-quoted literal.
pub(crate) fn escape_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
// In unit tests `expect`/`unwrap` are the assertion mechanism: a failed load or
// projection should fail the test.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ekg_domain::SourceRef;

    fn contract() -> MappingContract {
        // The embedded contract validates against the union of the neutral core and
        // capability ontology (neutral verbs like ekg:performedBy live in the core).
        ontology::supply_risk_contract()
            .expect("the embedded supply-risk contract loads against core + capability ontology")
    }

    fn record(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
        SourceRecord {
            source_ref: SourceRef {
                system: "odoo".to_owned(),
                model: model.to_owned(),
                id,
            },
            fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn embedded_contract_loads_and_validates() {
        let c = contract();
        assert_eq!(c.source(), "odoo");
        assert!(c.version() >= 1);
    }

    #[test]
    fn projects_a_sales_order_as_a_typed_entity() {
        let entity = ontology::entity_iri("sale.order", 288);
        let rec = record(
            "sale.order",
            288,
            &[
                ("commitment_date", "2027-10-18T00:00:00"),
                ("order_line", "[41, 42]"),
                ("partner_id", "8"),
            ],
        );
        let body = contract().project(&rec, &entity).expect("project");
        // Neutral base type + the mapped domain class.
        assert!(body.contains("a ekg:GovernedEntity, ekg:SalesOrder ;"));
        // A typed datatype literal (explicit datatype IRI, no prefix needed).
        assert!(body.contains(
            "ekg:commitment_date \"2027-10-18T00:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
        ));
        // Object relations resolved to referenced entity IRIs (one per id).
        assert!(body.contains("ekg:has_line_of <https://ekg.example/id/sale.order.line/41>"));
        assert!(body.contains("ekg:has_line_of <https://ekg.example/id/sale.order.line/42>"));
        assert!(body.contains("ekg:for_customer <https://ekg.example/id/res.partner/8>"));
        // Provenance.
        assert!(body.contains("ekg:wasDerivedFrom <https://ekg.example/id/sale.order/288#source>"));
        assert!(body.contains("a ekg:SourceRecord"));
    }

    #[test]
    fn materializes_the_order_total_as_ekg_order_value() {
        // The supply-risk contract maps sale.order.amount_total onto ekg:order_value
        // (xsd:decimal) so revenue-at-risk / order value can be computed downstream.
        // An ingested order that carries the total exposes it.
        let entity = ontology::entity_iri("sale.order", 288);
        let rec = record(
            "sale.order",
            288,
            &[
                ("commitment_date", "2027-10-18T00:00:00"),
                ("amount_total", "12500.00"),
            ],
        );
        let body = contract().project(&rec, &entity).expect("project");
        assert!(
            body.contains(
                "ekg:order_value \"12500.00\"^^<http://www.w3.org/2001/XMLSchema#decimal>"
            ),
            "the order total must project as a typed ekg:order_value decimal: {body}"
        );
    }

    #[test]
    fn an_orders_currency_is_a_reference_to_its_currency_record() {
        // Sale and purchase orders reference the currency their money is recorded in, and the
        // currency carries its ISO code, so a value is always served with the currency the
        // source recorded.
        for (model, id) in [("sale.order", 288), ("purchase.order", 12)] {
            let entity = ontology::entity_iri(model, id);
            let rec = record(model, id, &[("currency_id", "1")]);
            let body = contract().project(&rec, &entity).expect("project");
            assert!(
                body.contains("ekg:in_currency <https://ekg.example/id/res.currency/1>"),
                "{model} must reference its currency: {body}"
            );
        }
        let entity = ontology::entity_iri("res.currency", 1);
        let body = contract()
            .project(&record("res.currency", 1, &[("name", "USD")]), &entity)
            .expect("project");
        assert!(
            body.contains("a ekg:GovernedEntity, ekg:Currency ;"),
            "{body}"
        );
        assert!(body.contains("ekg:currency_code \"USD\""), "{body}");
    }

    #[test]
    fn materializes_a_pickings_completion_as_ekg_completed_at_distinct_from_eta() {
        // The supply-risk contract maps stock.picking.date_done onto ekg:completed_at
        // (xsd:dateTime), the delivered-on date supplier OTD is computed from, kept
        // distinct from the scheduled ekg:eta (scheduled_date). A completed picking
        // carrying both exposes each as its own typed literal.
        let entity = ontology::entity_iri("stock.picking", 51);
        let rec = record(
            "stock.picking",
            51,
            &[
                ("scheduled_date", "2027-10-16 00:00:00"),
                ("date_done", "2027-10-15 14:30:00"),
            ],
        );
        let body = contract().project(&rec, &entity).expect("project");
        assert!(
            body.contains(
                "ekg:completed_at \"2027-10-15T14:30:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
            ),
            "the receipt completion must project as a typed ekg:completed_at dateTime: {body}"
        );
        assert!(
            body.contains(
                "ekg:eta \"2027-10-16T00:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
            ),
            "the scheduled ETA stays a distinct fact from the completion: {body}"
        );
    }

    #[test]
    fn an_odoo_space_datetime_projects_as_a_valid_iso_xsd_datetime() {
        // Odoo returns "YYYY-MM-DD HH:MM:SS"; the projection must emit ISO 8601 so the
        // store's SHACL xsd:dateTime check accepts it (a space form is rejected).
        let entity = ontology::entity_iri("sale.order", 289);
        let rec = record(
            "sale.order",
            289,
            &[("commitment_date", "2027-10-19 12:00:00")],
        );
        let body = contract().project(&rec, &entity).expect("project");
        assert!(
            body.contains(
                "ekg:commitment_date \"2027-10-19T12:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
            ),
            "space-separated Odoo datetime must normalise to ISO: {body}"
        );
        assert!(
            !body.contains("2027-10-19 12:00:00"),
            "the invalid space form must not survive into the literal"
        );
    }

    #[test]
    fn types_a_supplier_ranked_party_as_supplier() {
        let entity = ontology::entity_iri("res.partner", 8);
        let supplier = record(
            "res.partner",
            8,
            &[
                ("name", "CellTech"),
                ("supplier_rank", "1"),
                ("customer_rank", "0"),
            ],
        );
        let body = contract().project(&supplier, &entity).expect("project");
        assert!(
            body.contains("ekg:Supplier"),
            "a supplier-ranked party is typed ekg:Supplier"
        );
        assert!(
            !body.contains("ekg:Customer"),
            "customer_rank 0 does not add the Customer role"
        );
    }

    #[test]
    fn does_not_type_a_plain_party_as_supplier() {
        let entity = ontology::entity_iri("res.partner", 9);
        let customer = record(
            "res.partner",
            9,
            &[
                ("name", "VeloMax"),
                ("supplier_rank", "0"),
                ("customer_rank", "3"),
            ],
        );
        let body = contract().project(&customer, &entity).expect("project");
        assert!(body.contains("ekg:Customer"));
        assert!(!body.contains("ekg:Supplier"));
    }

    #[test]
    fn projects_a_work_order_onto_the_neutral_metamodel() {
        let entity = ontology::entity_iri("mrp.workorder", 5);
        let rec = record(
            "mrp.workorder",
            5,
            &[
                ("production_id", "3"),
                ("workcenter_id", "2"),
                ("operation_id", "7"),
                ("state", "progress"),
                ("date_start", "2027-10-11 08:00:00"),
                ("duration_expected", "90.0"),
            ],
        );
        let body = contract().project(&rec, &entity).expect("project");
        // Typed as the manufacturing class (neutrally an ekg:Activity via the subclass).
        assert!(body.contains("a ekg:GovernedEntity, ekg:WorkOrder ;"));
        // Performed by the work-center agent via the neutral verb.
        assert!(body.contains("ekg:performedBy <https://ekg.example/id/mrp.workcenter/2>"));
        assert!(body.contains("ekg:part_of <https://ekg.example/id/mrp.production/3>"));
        assert!(body.contains("ekg:realizes <https://ekg.example/id/mrp.routing.workcenter/7>"));
        // Lifecycle state literal + normalised ISO start time (neutral verbs).
        assert!(body.contains("ekg:lifecycle_state \"progress\""));
        assert!(body.contains(
            "ekg:startedAt \"2027-10-11T08:00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
        ));
        // Provenance.
        assert!(
            body.contains("ekg:wasDerivedFrom <https://ekg.example/id/mrp.workorder/5#source>")
        );
    }

    #[test]
    fn purchase_and_manufacturing_orders_carry_their_reference_as_a_name() {
        // The read-model tabs label POs and MOs by their source reference (P00087 / MO-0316).
        for (model, id, reference) in [
            ("purchase.order", 9, "P00087"),
            ("mrp.production", 4, "WH/MO/00016"),
        ] {
            let entity = ontology::entity_iri(model, id);
            let rec = record(model, id, &[("name", reference)]);
            let body = contract().project(&rec, &entity).expect("project");
            assert!(
                body.contains(&format!("ekg:name \"{reference}\"")),
                "{model} projects its reference as ekg:name: {body}"
            );
        }
    }

    fn scoped_contract(scope_yaml: &str) -> Result<MappingContract> {
        let yaml = format!(
            "version: 1\nsource: odoo\nmodels:\n  stock.quant:\n    class: StockLevel\n{scope_yaml}"
        );
        MappingContract::load(&yaml, ontology::ONTOLOGY_TTL)
    }

    #[test]
    fn a_source_scope_parses_into_its_query_domain() {
        let contract = scoped_contract(
            "    scope:\n      - { field: location_id.usage, op: \"=\", value: internal }\n      - { field: company_id, op: in, value: [1, 2] }\n",
        )
        .expect("a well-formed scope loads");
        let scopes = contract.source_scopes();
        assert_eq!(
            scopes
                .get("stock.quant")
                .cloned()
                .map(serde_json::Value::Array),
            Some(serde_json::json!([
                ["location_id.usage", "=", "internal"],
                ["company_id", "in", [1, 2]]
            ]))
        );
    }

    #[test]
    fn an_unscoped_model_has_no_domain() {
        let contract = scoped_contract("").expect("an unscoped model loads");
        assert!(contract.source_scopes().is_empty());
    }

    #[test]
    fn a_malformed_scope_fails_loud() {
        for (scope, needle) in [
            (
                "    scope:\n      - { field: location_id.usage, op: like, value: internal }\n",
                "unknown operator",
            ),
            (
                "    scope:\n      - { field: location_id, op: in, value: 5 }\n",
                "needs a non-empty list",
            ),
            (
                "    scope:\n      - { field: location_id, op: in, value: [] }\n",
                "needs a non-empty list",
            ),
            (
                "    scope:\n      - { field: location_id, op: \"not in\", value: [[5]] }\n",
                "needs a non-empty list",
            ),
            (
                "    scope:\n      - { field: location_id, op: \"=\", value: [5] }\n",
                "needs a scalar",
            ),
            (
                "    scope:\n      - { field: \"\", op: \"=\", value: 5 }\n",
                "empty field",
            ),
            (
                "    scope:\n      - { field: active, op: in, value: [true, false] }\n",
                "archived records",
            ),
            (
                "    scope:\n      - { field: x_active, op: \"=\", value: false }\n",
                "archived records",
            ),
        ] {
            let err = scoped_contract(scope).expect_err("a malformed scope is refused");
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn unmapped_model_fails_loud() {
        let entity = ontology::entity_iri("account.move", 1);
        let rec = record("account.move", 1, &[("name", "INV/1")]);
        assert!(matches!(
            contract().project(&rec, &entity),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn malformed_reference_fails_loud() {
        let entity = ontology::entity_iri("sale.order.line", 41);
        let rec = record(
            "sale.order.line",
            41,
            &[("product_id", "not-an-id"), ("product_uom_qty", "10")],
        );
        assert!(matches!(
            contract().project(&rec, &entity),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn rejects_a_contract_referencing_an_unknown_class() {
        let yaml = "version: 1\nsource: odoo\nmodels:\n  sale.order:\n    class: NotAThing\n";
        let err = MappingContract::load(yaml, ontology::ONTOLOGY_TTL).unwrap_err();
        assert!(matches!(&err, EngineError::Invariant(m) if m.contains("NotAThing")));
    }

    #[test]
    fn rejects_a_contract_referencing_an_unknown_property() {
        let yaml = "version: 1\nsource: odoo\nmodels:\n  sale.order:\n    class: SalesOrder\n    \
                    fields:\n      x: { property: not_a_property, datatype: xsd:string }\n";
        let err = MappingContract::load(yaml, ontology::ONTOLOGY_TTL).unwrap_err();
        assert!(matches!(&err, EngineError::Invariant(m) if m.contains("not_a_property")));
    }

    #[test]
    fn rejects_a_field_with_both_datatype_and_ref() {
        let yaml = "version: 1\nsource: odoo\nmodels:\n  sale.order:\n    class: SalesOrder\n    \
                    fields:\n      x: { property: commitment_date, datatype: xsd:dateTime, ref: res.partner }\n";
        assert!(MappingContract::load(yaml, ontology::ONTOLOGY_TTL).is_err());
    }

    #[test]
    fn declared_terms_capture_classes_and_properties_not_usages() {
        let terms = declared_terms(ontology::ONTOLOGY_TTL);
        assert!(terms.contains("SalesOrder"));
        assert!(terms.contains("commitment_date"));
        // Neutral governance/provenance terms live in the core artifact.
        let core_terms = declared_terms(ontology::CORE_TTL);
        assert!(core_terms.contains("wasDerivedFrom"));
        assert!(core_terms.contains("Activity"));
        // A term that only ever appears in object position is still declared
        // elsewhere; but a made-up token must not be present.
        assert!(!terms.contains("DefinitelyNotDeclared"));
    }

    #[test]
    fn parse_ref_ids_accepts_scalar_and_list_forms() {
        assert_eq!(parse_ref_ids("7"), Ok(vec![7]));
        assert_eq!(parse_ref_ids("[12, 13]"), Ok(vec![12, 13]));
        assert_eq!(parse_ref_ids("12,13"), Ok(vec![12, 13]));
        assert_eq!(parse_ref_ids("[]"), Ok(vec![]));
        assert!(parse_ref_ids("[7, name]").is_err());
    }
}
