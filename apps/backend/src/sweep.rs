//! The reconciliation sweep: a scheduled Restate virtual object.
//!
//! Two backstops for the thin-notify path, run on a self-rescheduling timer:
//!
//! - Missed changes. Read the `write_date` watermark from durable Restate state,
//!   ask the source for everything changed since, and re-submit each ref to the
//!   ingest object (which fat-pulls and materializes). The watermark advances only
//!   after the pass, so a crash re-runs instead of skipping.
//! - Deletions. Odoo archives instead of hard-deleting and fires no reliable
//!   `unlink` webhook, so per allowlisted model the sweep diffs the source's current
//!   ids against what is materialized and retracts the entities gone from the
//!   source. Each retract derives the structural edges that read its model, then
//!   publishes `RdfChanged`.
//!
//! Keyed by a single constant ([`SWEEP_KEY`]) so its state (the watermark) and its
//! timer are a durable singleton. It holds the concrete [`OdooIngestor`] because
//! `current_ids` is a source-side listing that the `SourceIngestor` seam does not
//! expose.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ekg_domain::{
    ChangeKind, EntityUri, NamedGraph, RdfChangePublisher, RdfChanged, Revision, SourceIngestor,
    SourceRef,
};
use ekg_semantic::{OdooIngestor, ontology};
use restate_sdk::prelude::*;

use crate::ingest::SOURCE_SYSTEM;
use crate::worker::{Derivations, IngestClient, classify, derive_then_publish};

/// The registered name of the sweep virtual object.
pub const SWEEP_OBJECT: &str = "Sweep";
/// The single key the sweep singleton runs under (its watermark and timer live here).
pub const SWEEP_KEY: &str = "default";
/// The sweep handler that runs one pass and reschedules the next.
pub const SWEEP_HANDLER: &str = "run";
/// The operator-invoked scoped re-sync handler (backfill of newly mapped fields).
pub const RESYNC_HANDLER: &str = "resync";

/// What to re-sync: the allowlisted source models whose every current record is re-pulled.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResyncRequest {
    /// Source model names (e.g. `stock.picking`); each must be on the ingest allowlist.
    pub models: Vec<String>,
}

/// What a re-sync resubmitted, per model (for the operator's confirmation).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResyncReport {
    /// `(model, records resubmitted to ingest)` in request order.
    pub resubmitted: Vec<(String, usize)>,
}

/// Restate state key holding the `write_date` cursor.
const WATERMARK_STATE: &str = "watermark";
/// The cursor's zero value: an Odoo datetime before any record, so the first pass
/// reconciles everything.
const WATERMARK_EPOCH: &str = "1970-01-01 00:00:00";

/// The reconciliation sweep. Cheap to clone: every field is an `Arc` or small.
#[derive(Clone)]
pub struct Sweep {
    odoo: Arc<OdooIngestor>,
    world: Arc<dyn ekg_domain::WorldModel>,
    publisher: Arc<dyn RdfChangePublisher>,
    allowlist: Vec<String>,
    interval_secs: u64,
    derivations: Option<Derivations>,
}

impl Sweep {
    /// Wire the sweep over its drivers, the model allowlist, and the timer interval.
    #[must_use]
    pub fn new(
        odoo: Arc<OdooIngestor>,
        world: Arc<dyn ekg_domain::WorldModel>,
        publisher: Arc<dyn RdfChangePublisher>,
        allowlist: Vec<String>,
        interval_secs: u64,
    ) -> Self {
        Self {
            odoo,
            world,
            publisher,
            allowlist,
            interval_secs,
            derivations: None,
        }
    }

    /// Run `derivations` after each retraction, before publishing.
    #[must_use]
    pub fn with_derivations(mut self, derivations: Derivations) -> Self {
        self.derivations = Some(derivations);
        self
    }
}

#[restate_sdk::object]
impl Sweep {
    /// Run one reconciliation pass, then reschedule the next.
    #[handler]
    async fn run(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        let watermark = ctx
            .get::<String>(WATERMARK_STATE)
            .await?
            .unwrap_or_else(|| WATERMARK_EPOCH.to_owned());

        // Journal the pass-start time. It becomes the next watermark, so any record
        // changed during the pass is picked up next time instead of skipped.
        let pass_started = ctx
            .run(|| async { Ok(Json::from(odoo_datetime(unix_now()))) })
            .name("pass_started")
            .await?
            .into_inner();

        // --- missed changes: re-submit everything changed since the watermark ---
        let changed = ctx
            .run(|| async {
                self.odoo
                    .changed_since(&watermark)
                    .await
                    .map(Json::from)
                    .map_err(classify)
            })
            .name("changed_since")
            .await?
            .into_inner();
        for source_ref in &changed {
            // Call the keyed ingest object, which fat-pulls and materializes. Keying by
            // entity keeps it to one materialization per entity at a time.
            let key = format!("{}:{}", source_ref.model, source_ref.id);
            ctx.object_client::<IngestClient>(key)
                .ingest(Json(source_ref.clone()))
                .send();
        }

        // --- deletions: retract entities archived or removed at the source ---
        for model in &self.allowlist {
            let source_ids = ctx
                .run(|| async {
                    self.odoo
                        .current_ids(model)
                        .await
                        .map(Json::from)
                        .map_err(classify)
                })
                .name(format!("current_ids:{model}"))
                .await?
                .into_inner();
            let rows = ctx
                .run(|| async {
                    self.world
                        .select(&materialized_query(model))
                        .await
                        .map(Json::from)
                        .map_err(classify)
                })
                .name(format!("materialized:{model}"))
                .await?
                .into_inner();
            let materialized: Vec<i64> = rows
                .iter()
                .filter_map(|row| {
                    row.get("entity")
                        .and_then(|uri| entity_id_for_model(uri, model))
                })
                .collect();

            for id in absent_from_source(&materialized, &source_ids) {
                self.retract(&ctx, model, id).await?;
            }
        }

        // Advance the cursor only after the pass has succeeded, and never backwards.
        ctx.set(
            WATERMARK_STATE,
            advance_watermark(&watermark, &pass_started),
        );

        // Reschedule the next pass on the durable timer under this object's own key
        // (`ctx.key()`), not SWEEP_KEY, so tests can run an isolated sweep under their
        // own key. Production runs under SWEEP_KEY.
        ctx.object_client::<SweepClient>(ctx.key())
            .run()
            .send_after(Duration::from_secs(self.interval_secs));
        Ok(())
    }

    /// Scoped re-sync: re-pull and re-materialize every current (active) record of the
    /// named models, regardless of the watermark, so a field newly added to the mapping
    /// contract is backfilled onto records already materialized (a record that has not
    /// changed at the source is otherwise never re-pulled by [`Self::run`]).
    ///
    /// Same governed path as any ingest (each ref goes to the keyed ingest object: fat-pull,
    /// provenance, revision, validation). A shared handler: it runs concurrently with a
    /// sweep pass instead of queueing behind it, and its context has no state-write access,
    /// so it cannot touch the watermark. It does not reschedule the timer. A model outside
    /// the allowlist (or an empty request) is refused terminally (400) before anything is
    /// pulled.
    #[handler]
    async fn resync(
        &self,
        ctx: SharedObjectContext<'_>,
        request: Json<ResyncRequest>,
    ) -> Result<Json<ResyncReport>, HandlerError> {
        // A refused request is the caller's error: a terminal 400 carrying the reason.
        let models = resync_models(&request.into_inner().models, &self.allowlist)
            .map_err(|reason| HandlerError::from(TerminalError::new_with_code(400, reason)))?;

        let mut resubmitted = Vec::with_capacity(models.len());
        for model in &models {
            let ids = ctx
                .run(|| async {
                    self.odoo
                        .current_ids(model)
                        .await
                        .map(Json::from)
                        .map_err(classify)
                })
                .name(format!("resync_ids:{model}"))
                .await?
                .into_inner();
            for id in &ids {
                let source_ref = SourceRef {
                    system: SOURCE_SYSTEM.to_owned(),
                    model: model.clone(),
                    id: *id,
                };
                ctx.object_client::<IngestClient>(format!("{model}:{id}"))
                    .ingest(Json(source_ref))
                    .send();
            }
            resubmitted.push((model.clone(), ids.len()));
        }
        Ok(Json(ResyncReport { resubmitted }))
    }
}

impl Sweep {
    /// Retract the entity `model`/`id` removed at the source: a journaled retract, then the
    /// derivations that read the model re-derive without it, then `RdfChanged` is published.
    /// In that order, so the monitor does not hear of the retraction before the derived edges
    /// reflect it.
    async fn retract(
        &self,
        ctx: &ObjectContext<'_>,
        model: &str,
        id: i64,
    ) -> Result<(), HandlerError> {
        let entity = EntityUri(ontology::entity_iri(model, id));
        let revision = ctx
            .run(|| async {
                self.world
                    .retract(&entity)
                    .await
                    .map(Json::from)
                    .map_err(classify)
            })
            .name(format!("retract:{model}:{id}"))
            .await?
            .into_inner();
        let source_ref = SourceRef {
            system: SOURCE_SYSTEM.to_owned(),
            model: model.to_owned(),
            id,
        };
        let event = retracted_event(&source_ref, revision);
        let derivations = self.derivations.as_ref().filter(|d| d.read(model));
        derive_then_publish(
            async {
                if let Some(derivations) = derivations {
                    ctx.run(|| async {
                        derivations
                            .run_for(model)
                            .await
                            .map(Json::from)
                            .map_err(classify)
                    })
                    .name(format!("derive:{model}:{id}"))
                    .await?;
                }
                Ok::<(), HandlerError>(())
            },
            self.publisher.as_ref(),
            &event,
            "retract publish failed",
        )
        .await
    }
}

/// Validate a re-sync request against the ingest allowlist. Returns the requested models,
/// de-duplicated in request order, or the refusal reason: an empty request, or any model not
/// allowlisted (all offenders named, nothing partially accepted).
fn resync_models(requested: &[String], allowlist: &[String]) -> Result<Vec<String>, String> {
    if requested.is_empty() {
        return Err("scoped re-sync refused: name at least one model".to_owned());
    }
    let refused: Vec<&str> = requested
        .iter()
        .filter(|m| !allowlist.contains(m))
        .map(String::as_str)
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "scoped re-sync refused: {} not on the ingest allowlist",
            refused.join(", ")
        ));
    }
    let mut models: Vec<String> = Vec::with_capacity(requested.len());
    for model in requested {
        if !models.contains(model) {
            models.push(model.clone());
        }
    }
    Ok(models)
}

// --- startup bootstrap trigger ---

/// Build the ingress URL that triggers one sweep pass without waiting for it:
/// `{ingress}/Sweep/default/run/send`, the same call `just sweep-trigger` makes.
///
/// # Errors
/// Returns a message if `ingress_base` is not a valid base URL.
pub fn sweep_run_url(ingress_base: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(ingress_base).map_err(|e| format!("bad ingress url: {e}"))?;
    url.path_segments_mut()
        .map_err(|()| "ingress url cannot be a base".to_owned())?
        .pop_if_empty()
        .extend([SWEEP_OBJECT, SWEEP_KEY, SWEEP_HANDLER, "send"]);
    Ok(url.into())
}

/// Best-effort trigger of the first sweep pass on a fresh boot.
///
/// The sweep only reschedules itself after it has run once, so nothing starts it on a cold
/// start. This fires it once via the ingress. It is not idempotent: the keyed singleton
/// serializes the passes, but every pass schedules its successor, so a trigger while a
/// timer is already running adds a second timer chain. Trigger it only when no chain is
/// running. A failure is not fatal: the caller logs the returned reason and keeps `/api`
/// up, because the sweep can be started later (next boot or `just sweep-trigger`).
///
/// # Errors
/// Returns the reason the trigger did not land (ingress unreachable, or a non-success
/// status such as a 404 before the deployment is registered).
pub async fn trigger_first_sweep(ingress_base: &str) -> Result<(), String> {
    let url = sweep_run_url(ingress_base)?;
    let resp = reqwest::Client::new()
        .post(url)
        .send()
        .await
        .map_err(|e| format!("restate ingress unreachable: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "restate ingress rejected sweep trigger: HTTP {}",
            resp.status()
        ))
    }
}

// --- pure helpers ---

/// Seconds since the Unix epoch (UTC). A clock read: call only inside `ctx.run`.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Format Unix seconds as Odoo's `YYYY-MM-DD HH:MM:SS` (UTC) datetime string, the
/// wire form its `write_date` domain filters compare against. Uses Howard Hinnant's
/// civil-from-days algorithm.
#[must_use]
fn odoo_datetime(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02}")
}

/// The watermark after a successful pass. It never moves backwards. Odoo datetimes are
/// fixed-width `YYYY-MM-DD HH:MM:SS`, so lexicographic order is chronological order and
/// a clock blip or out-of-order pass cannot rewind the cursor.
#[must_use]
fn advance_watermark(current: &str, pass_started: &str) -> String {
    if pass_started > current {
        pass_started.to_owned()
    } else {
        current.to_owned()
    }
}

/// The SPARQL that lists materialized (non-retracted) entity IRIs of `model`.
///
/// Uses only the public IRI scheme and the provenance predicate: retract drops the data
/// graph (where `ekg:wasDerivedFrom` lives), so a tombstoned entity drops out. The
/// in-memory fake ignores the SPARQL and returns every stored entity's `entity`
/// binding; [`entity_id_for_model`] filters those down to this model.
#[must_use]
fn materialized_query(model: &str) -> String {
    format!(
        "PREFIX ekg: <{vocab}>\n\
         SELECT ?entity WHERE {{ GRAPH ?g {{ ?entity ekg:wasDerivedFrom ?s }} \
         FILTER(STRSTARTS(STR(?entity), \"{ns}{model}/\")) }}",
        vocab = ontology::VOCAB_NAMESPACE,
        ns = ontology::ID_NAMESPACE,
    )
}

/// Parse the source id out of an entity IRI if it belongs to `model` (the `<id>` of
/// `.../id/<model>/<id>`); `None` for any other model or a non-numeric tail.
#[must_use]
fn entity_id_for_model(entity_uri: &str, model: &str) -> Option<i64> {
    let prefix = format!("{}{}/", ontology::ID_NAMESPACE, model);
    entity_uri.strip_prefix(&prefix)?.parse().ok()
}

/// The materialized ids absent from the source's current ids: the entities to retract.
#[must_use]
fn absent_from_source(materialized: &[i64], source: &[i64]) -> Vec<i64> {
    let present: HashSet<i64> = source.iter().copied().collect();
    materialized
        .iter()
        .copied()
        .filter(|id| !present.contains(id))
        .collect()
}

/// The `RdfChanged` for a retraction (a source record archived or removed).
#[must_use]
fn retracted_event(source_ref: &SourceRef, revision: Revision) -> RdfChanged {
    RdfChanged {
        entity_uri: EntityUri(ontology::entity_iri(&source_ref.model, source_ref.id)),
        named_graph: NamedGraph(ontology::named_graph(&source_ref.model, source_ref.id)),
        change_kind: ChangeKind::Retracted,
        source: source_ref.clone(),
        revision,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::worker::tests::{FAKE_DERIVATIONS, Recorder};

    #[tokio::test]
    async fn a_retract_of_an_in_scope_model_derives_before_it_publishes() {
        let recorder = Arc::new(Recorder::default());
        let derivations = Derivations::new(
            Arc::clone(&recorder) as Arc<dyn crate::worker::DerivationStore>,
            FAKE_DERIVATIONS,
        );
        let source_ref = SourceRef {
            system: SOURCE_SYSTEM.to_owned(),
            model: "mrp.production".to_owned(),
            id: 9,
        };
        let event = retracted_event(&source_ref, Revision(4));
        derive_then_publish(
            async { derivations.run_for(&source_ref.model).await.map(|_| ()) },
            recorder.as_ref(),
            &event,
            "retract publish failed",
        )
        .await
        .expect("settles");
        assert_eq!(
            recorder.log(),
            [
                "update:B",
                "publish:https://ekg.example/id/mrp.production/9"
            ]
        );
        assert_eq!(event.change_kind, ChangeKind::Retracted);
    }

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn resync_accepts_allowlisted_models_once_each_in_order() {
        let allow = models(&["stock.picking", "purchase.order", "mrp.production"]);
        assert_eq!(
            resync_models(
                &models(&["purchase.order", "stock.picking", "purchase.order"]),
                &allow
            ),
            Ok(models(&["purchase.order", "stock.picking"]))
        );
    }

    #[test]
    fn resync_refuses_a_model_outside_the_allowlist_naming_every_offender() {
        let allow = models(&["stock.picking"]);
        let err = resync_models(
            &models(&["stock.picking", "res.users", "account.move"]),
            &allow,
        )
        .expect_err("a non-allowlisted model is refused");
        assert!(
            err.contains("res.users") && err.contains("account.move"),
            "{err}"
        );
        // nothing is partially accepted: the allowlisted one is not returned on refusal
    }

    #[test]
    fn resync_refuses_an_empty_request() {
        assert!(resync_models(&[], &models(&["stock.picking"])).is_err());
    }

    #[test]
    fn watermark_never_moves_backwards() {
        assert_eq!(
            advance_watermark("2027-01-07 00:00:00", "2027-02-07 12:00:00"),
            "2027-02-07 12:00:00",
            "a later pass advances the cursor",
        );
        assert_eq!(
            advance_watermark("2027-02-07 12:00:00", "2027-01-07 00:00:00"),
            "2027-02-07 12:00:00",
            "an earlier pass-start never rewinds the cursor",
        );
    }

    #[test]
    fn odoo_datetime_formats_utc_seconds() {
        assert_eq!(odoo_datetime(0), "1970-01-01 00:00:00");
        // 1_700_000_000 = 2023-11-14 22:13:20 UTC.
        assert_eq!(odoo_datetime(1_700_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn entity_id_is_parsed_only_for_the_matching_model() {
        assert_eq!(
            entity_id_for_model("https://ekg.example/id/sale.order/42", "sale.order"),
            Some(42),
        );
        assert_eq!(
            entity_id_for_model("https://ekg.example/id/res.partner/7", "sale.order"),
            None,
            "a different model does not match",
        );
        assert_eq!(
            entity_id_for_model("https://ekg.example/id/sale.order/x", "sale.order"),
            None,
            "a non-numeric id is ignored",
        );
    }

    #[test]
    fn sweep_run_url_targets_the_singleton_send_endpoint() {
        assert_eq!(
            sweep_run_url("http://localhost:8080").expect("url"),
            "http://localhost:8080/Sweep/default/run/send",
        );
        assert_eq!(
            sweep_run_url("http://localhost:8080/").expect("url"),
            "http://localhost:8080/Sweep/default/run/send",
            "a trailing slash on the base is tolerated",
        );
    }

    #[test]
    fn absent_diff_is_materialized_minus_source() {
        // 2 and 4 are gone from the source, so they are retracted, in order.
        assert_eq!(absent_from_source(&[1, 2, 3, 4], &[1, 3]), vec![2, 4],);
        assert_eq!(
            absent_from_source(&[1, 2], &[1, 2, 3]),
            Vec::<i64>::new(),
            "nothing to retract when the source has everything",
        );
    }
}
