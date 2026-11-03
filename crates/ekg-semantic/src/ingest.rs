//! The Odoo [`SourceIngestor`] driver: fat-pull and reconciliation.
//!
//! Thin-notify/fat-pull: a webhook is only a `{model, id}` ping; the authoritative
//! fact is always re-read here via JSON-RPC (`read`), so duplicate / out-of-order /
//! thin payloads are non-problems and provenance traces to a governed source read.
//! `changed_since` and `current_ids` back the reconciliation sweep, and both use
//! Odoo's default active-only search scope so archived records are excluded.
//!
//! The shared client ([`odoo_client::Odoo`]) is blocking (`ureq`); the async seam
//! bridges it with `spawn_blocking`. The driver is capability-neutral: the model
//! allowlist is configuration, not baked-in vocabulary.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_domain::{EngineError, Result, SourceIngestor, SourceRecord, SourceRef};
use odoo_client::Odoo;
use serde_json::{Value, json};

/// Connection + scope for the Odoo source system.
#[derive(Debug, Clone)]
pub struct OdooConfig {
    /// Base URL, e.g. `http://localhost:8069`.
    pub base_url: String,
    /// Database name.
    pub db: String,
    /// Login (user).
    pub login: String,
    /// Password / API key.
    pub password: String,
    /// The source-system tag stamped on every `SourceRef` (e.g. `"odoo"`).
    pub system: String,
    /// The models this ingestor is allowed to pull / sweep.
    pub allowlist: Vec<String>,
    /// Per-model source scope (source-query domain terms, from
    /// `MappingContract::source_scopes`): only a model's records matching every term are part of
    /// the world model. ANDed into the sweep listings; a pull of an out-of-scope record is
    /// refused as not found. Unscoped models are absent. A scope applies wherever its model is
    /// listed or pulled, also for a model outside [`Self::allowlist`], so a queued or retried
    /// ingest after the allowlist was narrowed still refuses an out-of-scope record. Each term
    /// must pass the contract loader's own rules (e.g. never the model's archive flag, never an
    /// empty `in` list). The scopes of allowlisted models are checked against the source once
    /// at [`OdooIngestor::connect`], so a domain the source rejects fails at startup instead of
    /// stalling the sweep. The others are not probed, so a narrowed allowlist never needs read
    /// access to unrelated source models.
    pub scopes: BTreeMap<String, Vec<Value>>,
}

/// A [`SourceIngestor`] backed by the shared Odoo JSON-RPC client.
pub struct OdooIngestor {
    odoo: Arc<Odoo>,
    system: String,
    allowlist: Vec<String>,
    scopes: BTreeMap<String, Vec<Value>>,
}

impl OdooIngestor {
    /// Authenticate against Odoo and build the ingestor. Runs the blocking connect
    /// on a blocking thread.
    ///
    /// # Errors
    /// - [`EngineError::Backend`] if authentication or the runtime bridge fails.
    /// - [`EngineError::Invariant`] if a source scope term breaks the contract's scope rules
    ///   (e.g. it names the model's archive flag), or an allowlisted model's scope cannot be
    ///   validated against the source (the source rejects the domain, for example a mistyped
    ///   field path or a wrong operand, or fails while probing it).
    pub async fn connect(config: OdooConfig) -> Result<Self> {
        let OdooConfig {
            base_url,
            db,
            login,
            password,
            system,
            allowlist,
            scopes,
        } = config;
        validate_scopes(&scopes)?;
        let odoo =
            tokio::task::spawn_blocking(move || Odoo::connect(&base_url, &db, &login, &password))
                .await
                .map_err(|e| EngineError::Backend(format!("odoo connect join: {e}")))?
                .map_err(|e| EngineError::Backend(format!("odoo connect: {e}")))?;
        let ingestor = Self {
            odoo: Arc::new(odoo),
            system,
            allowlist,
            scopes,
        };
        // Fail loud at startup on a scope the source rejects (a mistyped field path, a wrong
        // operand): at query time it would be a retryable backend error that stalls every
        // sweep pass for every model. A bounded probe: a count, never the listing itself. Only
        // the allowlisted models' scopes: the ingestor never lists the others.
        for (model, terms) in probed_scopes(&ingestor.scopes, &ingestor.allowlist) {
            ingestor
                .count(model, scoped(current_ids_domain(), Some(terms)))
                .await
                .map_err(|e| {
                    EngineError::Invariant(format!(
                        "source scope for {model} could not be validated against the source: {e}"
                    ))
                })?;
        }
        Ok(ingestor)
    }

    /// The models this ingestor sweeps (the allowlist).
    #[must_use]
    pub fn allowlist(&self) -> &[String] {
        &self.allowlist
    }

    /// Run a blocking `search` on a blocking thread, returning the matching ids.
    async fn search(&self, model: &str, domain: Value) -> Result<Vec<i64>> {
        let odoo = Arc::clone(&self.odoo);
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || odoo.search(&model, domain))
            .await
            .map_err(|e| EngineError::Backend(format!("search join: {e}")))?
            .map_err(|e| EngineError::Backend(format!("odoo search: {e}")))
    }

    /// Run a blocking `search_count` on a blocking thread: how many records match `domain`,
    /// without listing them.
    async fn count(&self, model: &str, domain: Value) -> Result<i64> {
        let odoo = Arc::clone(&self.odoo);
        let model = model.to_owned();
        let counted = tokio::task::spawn_blocking(move || {
            odoo.call_method(&model, "search_count", json!({ "domain": domain }))
        })
        .await
        .map_err(|e| EngineError::Backend(format!("search_count join: {e}")))?
        .map_err(|e| EngineError::Backend(format!("odoo search_count: {e}")))?;
        counted.as_i64().ok_or_else(|| {
            EngineError::Backend(format!("odoo search_count returned a non-count: {counted}"))
        })
    }

    /// Run a blocking `search_read` on a blocking thread (all fields).
    async fn search_read(&self, model: &str, domain: Value) -> Result<Value> {
        let odoo = Arc::clone(&self.odoo);
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || odoo.search_read(&model, domain, Value::Null))
            .await
            .map_err(|e| EngineError::Backend(format!("search_read join: {e}")))?
            .map_err(|e| EngineError::Backend(format!("odoo search_read: {e}")))
    }

    /// Run a blocking `read` on a blocking thread (all fields).
    async fn read(&self, model: &str, id: i64) -> Result<Value> {
        let odoo = Arc::clone(&self.odoo);
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || odoo.read(&model, json!([id]), Value::Null))
            .await
            .map_err(|e| EngineError::Backend(format!("read join: {e}")))?
            .map_err(|e| EngineError::Backend(format!("odoo read: {e}")))
    }

    /// The active-record ids of `model`: a plain `search` with Odoo's default
    /// active-only scope, so archived records are excluded. The reconciliation
    /// sweep diffs this against materialized entities to retract those gone from the
    /// source; widening it to archived records would break that.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport/RPC failure.
    pub async fn current_ids(&self, model: &str) -> Result<Vec<i64>> {
        self.search(
            model,
            scoped(
                current_ids_domain(),
                self.scopes.get(model).map(Vec::as_slice),
            ),
        )
        .await
    }
}

#[async_trait]
impl SourceIngestor for OdooIngestor {
    async fn pull(&self, source_ref: &SourceRef) -> Result<SourceRecord> {
        // A scoped model's record outside its source scope is not part of the world model:
        // refuse it as not found (the worker's terminal outcome for a vanished record), so it
        // is never materialized.
        // One scoped read (not a check then a read): a record leaving the scope between two
        // calls can never slip through.
        let rows = match self.scopes.get(&source_ref.model) {
            Some(scope) => {
                let rows = self
                    .search_read(
                        &source_ref.model,
                        scoped(json!([["id", "=", source_ref.id]]), Some(scope)),
                    )
                    .await?;
                if rows.as_array().is_none_or(Vec::is_empty) {
                    return Err(EngineError::NotFound(format!(
                        "{}:{} is outside the model's source scope",
                        source_ref.model, source_ref.id
                    )));
                }
                rows
            }
            None => self.read(&source_ref.model, source_ref.id).await?,
        };
        let record = rows.as_array().and_then(|a| a.first()).ok_or_else(|| {
            EngineError::NotFound(format!("{}:{}", source_ref.model, source_ref.id))
        })?;
        Ok(record_from_read(source_ref.clone(), record))
    }

    async fn changed_since(&self, watermark: &str) -> Result<Vec<SourceRef>> {
        let mut refs = Vec::new();
        for model in &self.allowlist {
            let ids = self
                .search(
                    model,
                    scoped(
                        changed_domain(watermark),
                        self.scopes.get(model).map(Vec::as_slice),
                    ),
                )
                .await?;
            for id in ids {
                refs.push(SourceRef {
                    system: self.system.clone(),
                    model: model.clone(),
                    id,
                });
            }
        }
        Ok(refs)
    }
}

// --- pure helpers ---

/// The `write_date > watermark` search domain used by the reconciliation sweep.
/// Carries no `active` clause: Odoo's default `search` scope is active-only, so an
/// archived record is absent from `changed_since` and retracted via the `current_ids`
/// diff, never re-materialized. The json2 `search` takes only a `domain` (no
/// `context`/kwargs), so `active_test` cannot be widened here.
fn changed_domain(watermark: &str) -> Value {
    json!([["write_date", ">", watermark]])
}

/// The empty search domain listing all active records of a model.
fn current_ids_domain() -> Value {
    json!([])
}

/// Check every scope term handed to the ingestor against the contract loader's own term rules
/// ([`crate::mapping::check_scope_term`]), so the two cannot drift. A term must be a
/// `[field, op, value]` triple.
fn validate_scopes(scopes: &BTreeMap<String, Vec<Value>>) -> Result<()> {
    for (model, terms) in scopes {
        for term in terms {
            let Some([Value::String(field), Value::String(op), value]) =
                term.as_array().map(Vec::as_slice)
            else {
                return Err(EngineError::Invariant(format!(
                    "source scope for {model}: a term must be [field, op, value], got {term}"
                )));
            };
            crate::mapping::check_scope_term(model, field, op, value)
                .map_err(|why| EngineError::Invariant(format!("source scope: {why}")))?;
        }
    }
    Ok(())
}

/// The scopes probed at connect: those of allowlisted models only (the ingestor never lists
/// the others, so a narrowed allowlist must not need read access to them).
fn probed_scopes<'a>(
    scopes: &'a BTreeMap<String, Vec<Value>>,
    allowlist: &'a [String],
) -> impl Iterator<Item = (&'a String, &'a Vec<Value>)> {
    scopes
        .iter()
        .filter(move |(model, _)| allowlist.contains(model))
}

/// AND a model's source `scope` terms into `base` (Odoo's implicit AND of listed terms). An
/// unscoped model's domain is returned unchanged.
fn scoped(base: Value, scope: Option<&[Value]>) -> Value {
    let mut terms = match base {
        Value::Array(terms) => terms,
        _ => Vec::new(),
    };
    if let Some(extra) = scope {
        terms.extend(extra.iter().cloned());
    }
    Value::Array(terms)
}

/// Build a [`SourceRecord`] from an Odoo `read` result object, flattening each
/// field to the string wire form the mapping contract consumes.
fn record_from_read(source_ref: SourceRef, obj: &Value) -> SourceRecord {
    let mut fields = std::collections::BTreeMap::new();
    if let Some(map) = obj.as_object() {
        for (key, value) in map {
            if key == "id" {
                continue; // the id lives on the SourceRef, not as a field
            }
            if let Some(flat) = flatten_field(value) {
                fields.insert(key.clone(), flat);
            }
        }
    }
    SourceRecord { source_ref, fields }
}

/// Flatten one Odoo field value to the mapping contract's string wire form:
/// - `false` / null: dropped (Odoo's empty value for a field/relation);
/// - scalars: their string form;
/// - a many2one `[id, "name"]`: the referenced id (`"7"`);
/// - a one2many/many2many `[1, 2, 3]`: `"[1, 2, 3]"`.
fn flatten_field(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(false) | Value::Object(_) => None,
        Value::Bool(true) => Some("true".to_owned()),
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => flatten_relation(items),
    }
}

/// Flatten a relation array: all-integers is an x2many id list; `[int, string]` is
/// a many2one pair whose id we keep.
fn flatten_relation(items: &[Value]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    if items.iter().all(Value::is_i64) {
        let ids: Vec<String> = items
            .iter()
            .filter_map(Value::as_i64)
            .map(|i| i.to_string())
            .collect();
        return Some(format!("[{}]", ids.join(", ")));
    }
    if items.len() == 2
        && let (Some(id), true) = (items[0].as_i64(), items[1].is_string())
    {
        return Some(id.to_string());
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_source_scope_is_anded_into_the_listing_domains() {
        let scope = vec![json!(["location_id.usage", "=", "internal"])];
        assert_eq!(
            scoped(
                changed_domain("2027-01-07 00:00:00"),
                Some(scope.as_slice())
            ),
            json!([
                ["write_date", ">", "2027-01-07 00:00:00"],
                ["location_id.usage", "=", "internal"]
            ])
        );
        assert_eq!(
            scoped(current_ids_domain(), Some(scope.as_slice())),
            json!([["location_id.usage", "=", "internal"]])
        );
        assert_eq!(
            scoped(json!([["id", "=", 7]]), Some(scope.as_slice())),
            json!([["id", "=", 7], ["location_id.usage", "=", "internal"]])
        );
    }

    fn allow(models: &[&str]) -> Vec<String> {
        models.iter().map(|m| (*m).to_owned()).collect()
    }

    fn scopes(model: &str, terms: Vec<Value>) -> BTreeMap<String, Vec<Value>> {
        BTreeMap::from([(model.to_owned(), terms)])
    }

    #[test]
    fn only_allowlisted_scopes_are_probed_but_every_scope_is_kept() {
        let all = BTreeMap::from([
            (
                "stock.quant".to_owned(),
                vec![json!(["location_id.usage", "=", "internal"])],
            ),
            (
                "stock.picking".to_owned(),
                vec![json!(["picking_type_code", "=", "incoming"])],
            ),
        ]);
        assert!(validate_scopes(&all).is_ok(), "well-formed scopes");
        let allowlist = allow(&["stock.quant", "res.partner"]);
        assert_eq!(
            probed_scopes(&all, &allowlist)
                .map(|(model, _)| model.as_str())
                .collect::<Vec<_>>(),
            vec!["stock.quant"],
            "a scope of a model outside the allowlist is not probed"
        );
    }

    #[test]
    fn a_scope_handed_to_the_ingestor_passes_the_contracts_term_rules() {
        for field in crate::mapping::ACTIVE_FIELDS {
            let refused = validate_scopes(&scopes(
                "res.partner",
                vec![json!([field, "in", [true, false]])],
            ));
            assert!(
                matches!(&refused, Err(EngineError::Invariant(why)) if why.contains(&format!("`{field}`"))),
                "a scope on `{field}` is refused, naming it: {refused:?}"
            );
        }
        for (term, needle) in [
            // `in []` would empty the listing, and the sweep would retract the whole model.
            (json!(["location_id", "in", []]), "non-empty list"),
            // `not in []` would silently do nothing.
            (json!(["location_id", "not in", []]), "non-empty list"),
            (json!(["location_id", "like", "WH"]), "unknown operator"),
            (json!(["location_id", "="]), "[field, op, value]"),
            (json!(["", "=", 5]), "empty field"),
        ] {
            let refused = validate_scopes(&scopes("stock.quant", vec![term.clone()]));
            assert!(
                matches!(&refused, Err(EngineError::Invariant(why)) if why.contains(needle)),
                "{term} is refused ({needle}): {refused:?}"
            );
        }
        // The rules apply to every scope, not only the allowlisted ones: an out-of-allowlist
        // scope is still applied to pulls, so it must be sound too.
        assert!(
            validate_scopes(&scopes("res.partner", vec![json!(["active", "=", false])])).is_err()
        );
    }

    /// A config whose source is unreachable: `connect` must refuse a bad scope before it
    /// reaches the network (an `Invariant`, not the `Backend` a connect failure gives).
    fn unreachable(scopes: BTreeMap<String, Vec<Value>>, allowlist: &[&str]) -> OdooConfig {
        OdooConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            db: "none".to_owned(),
            login: "none".to_owned(),
            password: "none".to_owned(),
            system: "odoo".to_owned(),
            allowlist: allow(allowlist),
            scopes,
        }
    }

    #[tokio::test]
    async fn connect_refuses_a_bad_scope_before_touching_the_source() {
        for (model, allowlist) in [
            ("res.partner", &["res.partner"][..]),
            // also a model off the allowlist: its scope still applies to pulls.
            ("res.partner", &["stock.quant"][..]),
        ] {
            let refused = OdooIngestor::connect(unreachable(
                scopes(model, vec![json!(["active", "=", false])]),
                allowlist,
            ))
            .await;
            assert!(
                matches!(&refused, Err(EngineError::Invariant(why)) if why.contains("`active`")),
                "refused as an invariant, before any network call ({allowlist:?}): {:?}",
                refused.err()
            );
        }
        // A sound scope passes the check and only then meets the unreachable source.
        let unreached = OdooIngestor::connect(unreachable(
            scopes(
                "stock.quant",
                vec![json!(["location_id.usage", "=", "internal"])],
            ),
            &["stock.quant"],
        ))
        .await;
        assert!(
            matches!(&unreached, Err(EngineError::Backend(_))),
            "a sound scope reaches the (unreachable) source: {:?}",
            unreached.err()
        );
    }

    #[test]
    fn an_unscoped_model_keeps_its_domain() {
        assert_eq!(scoped(current_ids_domain(), None), json!([]));
        assert_eq!(
            scoped(changed_domain("w"), None),
            json!([["write_date", ">", "w"]])
        );
    }

    #[test]
    fn flattens_scalars_and_drops_odoo_empties() {
        assert_eq!(flatten_field(&json!("VeloMax")), Some("VeloMax".to_owned()));
        assert_eq!(flatten_field(&json!(10)), Some("10".to_owned()));
        assert_eq!(flatten_field(&json!(2.5)), Some("2.5".to_owned()));
        // Odoo returns `false` for an unset field / empty relation.
        assert_eq!(flatten_field(&json!(false)), None);
        assert_eq!(flatten_field(&Value::Null), None);
    }

    #[test]
    fn flattens_many2one_to_the_referenced_id() {
        // many2one reads back as [id, "display name"].
        assert_eq!(
            flatten_field(&json!([7, "Battery 48V"])),
            Some("7".to_owned())
        );
    }

    #[test]
    fn flattens_x2many_to_an_id_list() {
        assert_eq!(flatten_field(&json!([41, 42])), Some("[41, 42]".to_owned()));
        assert_eq!(flatten_field(&json!([10])), Some("[10]".to_owned()));
        // An empty relation is dropped.
        assert_eq!(flatten_field(&json!([])), None);
    }

    #[test]
    fn record_from_read_shapes_a_source_record() {
        let sr = SourceRef {
            system: "odoo".to_owned(),
            model: "sale.order".to_owned(),
            id: 288,
        };
        let obj = json!({
            "id": 288,
            "commitment_date": "2027-10-19 12:00:00",
            "partner_id": [8, "VeloMax"],
            "order_line": [41, 42],
            "note": false
        });
        let rec = record_from_read(sr.clone(), &obj);
        assert_eq!(rec.source_ref, sr);
        assert_eq!(
            rec.fields.get("commitment_date").map(String::as_str),
            Some("2027-10-19 12:00:00")
        );
        assert_eq!(rec.fields.get("partner_id").map(String::as_str), Some("8"));
        assert_eq!(
            rec.fields.get("order_line").map(String::as_str),
            Some("[41, 42]")
        );
        assert!(
            !rec.fields.contains_key("id"),
            "id lives on the ref, not a field"
        );
        assert!(
            !rec.fields.contains_key("note"),
            "an Odoo false field is dropped"
        );
    }

    #[test]
    fn changed_domain_filters_on_write_date() {
        let d = changed_domain("2027-09-28 00:00:00");
        assert_eq!(d, json!([["write_date", ">", "2027-09-28 00:00:00"]]));
    }

    #[test]
    fn reconciliation_queries_stay_active_scoped() {
        // Reconciliation searches must use Odoo's default active-only scope so archived
        // records fall out of both `changed_since` and `current_ids`.
        for domain in [changed_domain("2000-01-01 00:00:00"), current_ids_domain()] {
            assert!(
                !domain.to_string().contains("active"),
                "reconciliation domain must not reference `active`: {domain}"
            );
        }
        // The json2 `search` passes only the domain, with no `context`/kwargs channel, so
        // `active_test: false` (or any active-widening) cannot be passed here without a new
        // call shape.

        // A contract-declared source scope, ANDed into both listings, must not name the
        // active field either. The loader already refuses such a term, so what this pins is the
        // composition: ANDing a shipped scope into a listing (`scoped`) never injects one.
        let scopes = crate::ontology::supply_risk_contract()
            .expect("the supply-risk contract loads")
            .source_scopes();
        for scope in scopes.values() {
            for domain in [
                scoped(
                    changed_domain("2000-01-01 00:00:00"),
                    Some(scope.as_slice()),
                ),
                scoped(current_ids_domain(), Some(scope.as_slice())),
            ] {
                let names_active = domain.as_array().into_iter().flatten().any(|term| {
                    term.get(0)
                        .and_then(Value::as_str)
                        .is_some_and(|f| crate::mapping::ACTIVE_FIELDS.contains(&f))
                });
                assert!(
                    !names_active,
                    "a scoped reconciliation domain must not name the active field: {domain}"
                );
            }
        }
    }
}
