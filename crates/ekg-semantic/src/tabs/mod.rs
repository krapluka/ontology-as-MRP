//! The operator impact tabs as read-model projections.
//!
//! Each tab (Timeline / Graph / Flow / Suppliers) is plant-wide structure (subject-free SELECTs
//! authored as `ontology/tabs/*.rq`) plus markers the pure Rust row builder adds for the
//! selected point: `at_risk` (the monitor's own rule verdict, handed in as a plain IRI set so
//! this crate never depends on the dynamic layer), `on_path` (the point's deterministic
//! one-per-column chain) and `cause`. The selected point decides what is lit, never what is
//! shown. Every projection is a read: nothing here writes.
//!
//! Markers whose inputs cannot be read are omitted, never served as `"false"`.

pub mod causes;
pub mod chain;
pub mod member;
pub mod queries;
pub mod rows;
pub mod vocab;

use std::collections::{BTreeMap, BTreeSet};

/// One projection row: variable/field name to value (the neutral opaque row the contract
/// carries).
pub type Row = BTreeMap<String, String>;

/// A read-model tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tab {
    /// Orders against their promise, with inbound arrivals.
    Timeline,
    /// The six-column supply graph.
    Graph,
    /// The five-stage pipeline.
    Flow,
    /// The supplier table and its detail.
    Suppliers,
}

impl Tab {
    /// Every tab, in display order.
    pub const ALL: [Self; 4] = [Self::Timeline, Self::Graph, Self::Flow, Self::Suppliers];

    /// The tab's stable snake_case name (the tab-row vocabulary's key).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Timeline => "timeline",
            Self::Graph => "graph",
            Self::Flow => "flow",
            Self::Suppliers => "suppliers",
        }
    }
}

pub use causes::{
    CauseMap, Causes, cause_chain, cause_map, process_context, work_centre_schedule, work_centres,
};
pub use chain::{Chain, PathHops, choose_chain};
pub use member::{ContendedWork, MemberRead, ShortComponent, member_read};
pub use queries::{Day, Window};
pub use rows::{Markers, SourcingLink, build_rows, sourcing_roles};
pub use vocab::{TabVocabulary, row_vocabulary};

use ekg_domain::{EngineError, EntityUri, Result, WorldModel};

use crate::ontology;

/// A served tab: its rows, plus the degradations the read had to make (a marker input that
/// could not be read, so its markers were omitted). The caller logs them at warn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabProjection {
    /// The served rows (structure + markers + derived fields).
    pub rows: Vec<Row>,
    /// Why some markers were omitted, if any were.
    pub warnings: Vec<String>,
}

/// Project `tab` for the selected point `subject`, today. `at_risk` is the monitor's own
/// current verdict set (`None` when the rules could not be evaluated: every `at_risk`/`cause`
/// marker and the cause-dependent `on_path` are then omitted). A read: it mutates nothing.
///
/// # Errors
/// [`EngineError::Invariant`] if `subject` is not a canonical entity IRI. Otherwise any failed
/// read the tab cannot be served without:
///
/// - the structural queries (the tab itself);
/// - the sourcing links (Flow and Suppliers): without them every sourcing role and every
///   exposure would be served as if no supplier sourced anything;
/// - the rule-independent path (`path.rq`): a default path would serve every customer,
///   order, product, BOM and MO row `on_path="false"`, a denial.
///
/// A failed marker read (the cause map, the cause path) degrades into
/// [`TabProjection::warnings`] instead: those markers are omitted, never denied.
pub async fn project(
    world: &dyn WorldModel,
    subject: &EntityUri,
    tab: Tab,
    at_risk: Option<&BTreeSet<String>>,
) -> Result<TabProjection> {
    project_on(world, subject, tab, at_risk, Day::today()).await
}

/// [`project`] with a pinned "today" (the day windows are computed from it).
///
/// # Errors
/// As [`project`].
pub async fn project_on(
    world: &dyn WorldModel,
    subject: &EntityUri,
    tab: Tab,
    at_risk: Option<&BTreeSet<String>>,
    today: Day,
) -> Result<TabProjection> {
    let subject = ontology::canonical_subject(&subject.0).ok_or_else(|| {
        EngineError::Invariant(format!(
            "a tab is projected only for a canonical entity IRI, got {:?}",
            subject.0
        ))
    })?;
    let window = Window::around(today);
    let mut warnings = Vec::new();

    // The structure: plant-wide, subject-free.
    let mut structural = Vec::new();
    for query in queries::tab_queries(tab, &window) {
        structural.extend(world.select(&query).await?);
    }
    let sourcing = if matches!(tab, Tab::Flow | Tab::Suppliers) {
        SourcingLink::from_rows(&world.select(&queries::sourcing_query()).await?)
    } else {
        Vec::new()
    };

    // The rule-independent path: always read.
    let path = PathHops::from_rows(&world.select(&queries::path_query(&subject)?).await?);

    // The causes: only meaningful beside a readable at-risk set.
    let causes = match at_risk {
        Some(_) => match cause_map(world).await {
            Ok(map) => Some(map),
            Err(err) => {
                warnings.push(format!("cause map unreadable, causes omitted: {err}"));
                None
            }
        },
        None => None,
    };

    // The cause-dependent chain: only with the at-risk set and the cause map, and the cause
    // query is issued only for a non-empty cause set.
    let chain = match (at_risk, &causes) {
        (Some(_), Some(map)) => {
            let point = map.of(&subject.0).cloned().unwrap_or_default();
            match queries::path_causes_query(&subject, point.ranked.iter().map(String::as_str)) {
                Ok(None) => Some(Chain::default()),
                Ok(Some(query)) => match world.select(&query).await {
                    Ok(rows) => Some(choose_chain(&point, &path, &rows)),
                    Err(err) => {
                        warnings.push(format!("cause path unreadable, path omitted: {err}"));
                        None
                    }
                },
                Err(err) => {
                    warnings.push(format!("cause path refused, path omitted: {err}"));
                    None
                }
            }
        }
        _ => None,
    };

    let markers = Markers {
        subject: &subject.0,
        at_risk,
        causes: causes.as_ref(),
        path: &path,
        chain: chain.as_ref(),
    };
    Ok(TabProjection {
        rows: build_rows(tab, structural, &sourcing, &markers),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ekg_domain::{Grounding, ReadContext, Revision, SourceRecord};

    use super::*;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn iri(model: &str, id: i64) -> String {
        ontology::entity_iri(model, id)
    }

    /// A world that answers each query by its shape, records every query it was asked, can
    /// fail the rule SELECT or the cause-path query, can report a contended work order, and
    /// panics on any write (a tab read must mutate nothing).
    #[allow(clippy::struct_excessive_bools)] // independent failure switches of a test double
    struct ScriptedWorld {
        short: bool,
        fail_rules: bool,
        fail_path_causes: bool,
        contended: bool,
        asked: Mutex<Vec<String>>,
    }

    impl ScriptedWorld {
        fn new(short: bool, fail_rules: bool) -> Self {
            Self {
                short,
                fail_rules,
                fail_path_causes: false,
                contended: false,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn failing_path_causes(mut self) -> Self {
            self.fail_path_causes = true;
            self
        }

        fn contended(mut self) -> Self {
            self.contended = true;
            self
        }

        fn asked(&self, sparql: &str) -> bool {
            self.asked.lock().expect("lock").iter().any(|q| q == sparql)
        }

        fn asked_path_causes(&self) -> bool {
            self.asked
                .lock()
                .expect("lock")
                .iter()
                .any(|q| q.contains("VALUES ?cause"))
        }
    }

    #[async_trait]
    impl WorldModel for ScriptedWorld {
        async fn materialize(&self, _record: &SourceRecord) -> Result<Revision> {
            panic!("a tab read must never materialize");
        }
        async fn retract(&self, _entity: &EntityUri) -> Result<Revision> {
            panic!("a tab read must never retract");
        }
        async fn ground(&self, _entity: &EntityUri, _ctx: &ReadContext) -> Result<Grounding> {
            panic!("a tab read must never ground");
        }
        async fn select(&self, sparql: &str) -> Result<Vec<Row>> {
            self.asked.lock().expect("lock").push(sparql.to_owned());
            let so = iri("sale.order", 1);
            let comp = iri("product.product", 4);
            let po = iri("purchase.order", 1);
            let supplier = iri("res.partner", 6);
            let (mo, wo) = (iri("mrp.production", 3), iri("mrp.workorder", 1));
            if sparql == ontology::RISK_RULE_SELECT_SPARQL {
                if self.fail_rules {
                    return Err(EngineError::Backend("rule store down".to_owned()));
                }
                return Ok(if self.short {
                    vec![row(&[("so", &so), ("comp", &comp), ("shortfall", "10")])]
                } else {
                    Vec::new()
                });
            }
            if sparql == ontology::PROCESS_CONTENTION_SELECT_SPARQL {
                // One over-capacity work order (capacity 1, one overlapping peer), in the
                // bindings the shared predicate reads.
                return Ok(if self.contended {
                    vec![row(&[
                        ("mo", &mo),
                        ("wo", &wo),
                        ("capacity", "1"),
                        ("overlapPeers", "1"),
                    ])]
                } else {
                    Vec::new()
                });
            }
            if sparql.contains("VALUES ?cause") {
                if self.fail_path_causes {
                    return Err(EngineError::Backend("cause path down".to_owned()));
                }
                return Ok(vec![row(&[
                    ("hop", "line"),
                    ("cause", &comp),
                    ("po", &po),
                    ("vendor", &supplier),
                    ("arrival", "2027-11-15T00:00:00"),
                    ("late", "true"),
                ])]);
            }
            if sparql.contains("\"self_mo\"") {
                let mut hops = vec![row(&[("hop", "order"), ("entity", &so)])];
                if sparql.contains(&format!("<{mo}> a ekg:ManufacturingOrder")) {
                    hops.push(row(&[("hop", "self_mo"), ("entity", &mo)]));
                }
                return Ok(hops);
            }
            if sparql.contains("\"on_order\"") {
                return Ok(vec![
                    row(&[("stage", "promised"), ("entity", &so)]),
                    row(&[("stage", "in_stock"), ("entity", &comp)]),
                    row(&[("stage", "suppliers"), ("entity", &supplier)]),
                    row(&[("stage", "on_order"), ("entity", &po), ("product", &comp)]),
                    row(&[("stage", "in_production"), ("entity", &mo)]),
                    row(&[
                        ("stage", "in_production"),
                        ("entity", &wo),
                        ("part_of", &mo),
                    ]),
                ]);
            }
            if sparql.contains("\"dependent_order\"") {
                return Ok(vec![row(&[
                    ("kind", "dependent_order"),
                    ("supplier", &supplier),
                    ("order", &so),
                    ("order_value", "18000.0"),
                ])]);
            }
            if sparql.contains("BIND(\"supplier\" AS ?kind)") {
                return Ok(vec![row(&[("kind", "supplier"), ("supplier", &supplier)])]);
            }
            if sparql.contains("SELECT ?supplier ?component") {
                return Ok(vec![row(&[
                    ("supplier", &supplier),
                    ("component", &comp),
                    ("sequence", "1"),
                ])]);
            }
            Ok(Vec::new())
        }
    }

    fn marker<'r>(rows: &'r [Row], entity: &str, key: &str) -> Option<&'r str> {
        rows.iter()
            .find(|r| r.get("entity").is_some_and(|e| e == entity))
            .and_then(|r| r.get(key))
            .map(String::as_str)
    }

    fn today() -> Day {
        Day::from_iso("2027-10-01").expect("day")
    }

    #[tokio::test]
    async fn a_flagged_point_lights_its_chain_across_the_flow() {
        let world = ScriptedWorld::new(true, false);
        let subject = EntityUri(iri("sale.order", 1));
        let at_risk = BTreeSet::from([subject.0.clone()]);
        let projection = project_on(&world, &subject, Tab::Flow, Some(&at_risk), today())
            .await
            .expect("flow projected");
        assert!(projection.warnings.is_empty(), "{:?}", projection.warnings);
        let rows = &projection.rows;
        for entity in [
            iri("sale.order", 1),
            iri("product.product", 4),
            iri("res.partner", 6),
            iri("purchase.order", 1),
        ] {
            assert_eq!(marker(rows, &entity, "on_path"), Some("true"), "{entity}");
        }
        assert_eq!(
            marker(rows, &iri("purchase.order", 1), "cause"),
            Some("true")
        );
        assert_eq!(
            marker(rows, &iri("res.partner", 6), "sourcing"),
            Some("single")
        );
        assert!(world.asked_path_causes());
    }

    #[tokio::test]
    async fn an_empty_cause_set_never_issues_the_cause_query() {
        let world = ScriptedWorld::new(false, false);
        let subject = EntityUri(iri("sale.order", 1));
        let projection = project_on(&world, &subject, Tab::Flow, Some(&BTreeSet::new()), today())
            .await
            .expect("flow projected");
        assert!(!world.asked_path_causes(), "no causes, so no cause query");
        let rows = &projection.rows;
        assert_eq!(
            marker(rows, &iri("purchase.order", 1), "on_path"),
            Some("false")
        );
        assert_eq!(marker(rows, &iri("sale.order", 1), "on_path"), Some("true"));
        assert_eq!(
            marker(rows, &iri("sale.order", 1), "at_risk"),
            Some("false")
        );
    }

    #[tokio::test]
    async fn an_unreadable_cause_map_degrades_with_a_warning() {
        let world = ScriptedWorld::new(true, true);
        let subject = EntityUri(iri("sale.order", 1));
        let at_risk = BTreeSet::from([subject.0.clone()]);
        let projection = project_on(&world, &subject, Tab::Flow, Some(&at_risk), today())
            .await
            .expect("the tab is still served");
        assert_eq!(projection.warnings.len(), 1, "{:?}", projection.warnings);
        let rows = &projection.rows;
        assert_eq!(marker(rows, &iri("purchase.order", 1), "on_path"), None);
        assert_eq!(marker(rows, &iri("product.product", 4), "cause"), None);
        assert_eq!(marker(rows, &iri("sale.order", 1), "on_path"), Some("true"));
        assert_eq!(marker(rows, &iri("sale.order", 1), "at_risk"), Some("true"));
    }

    #[tokio::test]
    async fn without_the_at_risk_set_no_rule_is_read_and_no_marker_is_denied() {
        let world = ScriptedWorld::new(true, false);
        let subject = EntityUri(iri("sale.order", 1));
        let projection = project_on(&world, &subject, Tab::Flow, None, today())
            .await
            .expect("served");
        assert!(projection.rows.iter().all(|r| !r.contains_key("at_risk")));
        assert!(projection.rows.iter().all(|r| !r.contains_key("cause")));
        assert!(!world.asked_path_causes());
        for rule in [
            ontology::RISK_RULE_SELECT_SPARQL,
            ontology::PROCESS_CONTENTION_SELECT_SPARQL,
        ] {
            assert!(
                !world.asked(rule),
                "no rule is read without the at-risk set"
            );
        }
        let rows = &projection.rows;
        for cause_dependent in [
            iri("purchase.order", 1),
            iri("product.product", 4),
            iri("res.partner", 6),
        ] {
            assert_eq!(
                marker(rows, &cause_dependent, "on_path"),
                None,
                "{cause_dependent}: path status unknown, so omitted, never denied"
            );
        }
        assert_eq!(marker(rows, &iri("sale.order", 1), "on_path"), Some("true"));
    }

    #[tokio::test]
    async fn an_unreadable_cause_path_omits_the_supply_hops_with_a_warning() {
        let world = ScriptedWorld::new(true, false).failing_path_causes();
        let subject = EntityUri(iri("sale.order", 1));
        let at_risk = BTreeSet::from([subject.0.clone()]);
        let projection = project_on(&world, &subject, Tab::Flow, Some(&at_risk), today())
            .await
            .expect("the tab is still served");
        assert_eq!(projection.warnings.len(), 1, "{:?}", projection.warnings);
        assert!(projection.warnings[0].contains("cause path"));
        let rows = &projection.rows;
        let po = iri("purchase.order", 1);
        assert_eq!(marker(rows, &po, "on_path"), None, "never a denied chain");
        assert_eq!(marker(rows, &po, "cause"), None, "never a denied PO cause");
        assert_eq!(marker(rows, &iri("product.product", 4), "on_path"), None);
        assert_eq!(
            marker(rows, &iri("product.product", 4), "cause"),
            Some("true"),
            "the cause map was read: the short component is still a known cause"
        );
        assert_eq!(marker(rows, &iri("sale.order", 1), "on_path"), Some("true"));
    }

    #[tokio::test]
    async fn a_process_points_contended_work_order_is_its_cause_on_the_flow() {
        let world = ScriptedWorld::new(false, false).contended();
        let mo = iri("mrp.production", 3);
        let subject = EntityUri(mo.clone());
        let at_risk = BTreeSet::from([mo.clone()]);
        let projection = project_on(&world, &subject, Tab::Flow, Some(&at_risk), today())
            .await
            .expect("flow projected");
        assert!(projection.warnings.is_empty(), "{:?}", projection.warnings);
        let rows = &projection.rows;
        let wo = iri("mrp.workorder", 1);
        assert_eq!(
            marker(rows, &wo, "cause"),
            Some("true"),
            "read via the cause map"
        );
        assert_eq!(
            marker(rows, &mo, "on_path"),
            Some("true"),
            "an MO point lights itself"
        );
        assert_eq!(marker(rows, &mo, "at_risk"), Some("true"));
        assert_eq!(
            marker(rows, &iri("purchase.order", 1), "on_path"),
            Some("false"),
            "a process point has no supply hops"
        );
        assert!(
            !world.asked_path_causes(),
            "no short component, so no cause query"
        );
    }

    #[tokio::test]
    async fn the_suppliers_tab_serves_sourcing_exposure_and_the_chain_supplier() {
        let world = ScriptedWorld::new(true, false);
        let subject = EntityUri(iri("sale.order", 1));
        let at_risk = BTreeSet::from([subject.0.clone()]);
        let projection = project_on(&world, &subject, Tab::Suppliers, Some(&at_risk), today())
            .await
            .expect("suppliers projected");
        assert!(projection.warnings.is_empty(), "{:?}", projection.warnings);
        let supplier = projection
            .rows
            .iter()
            .find(|r| r.get("kind").is_some_and(|k| k == "supplier"))
            .expect("the supplier row");
        let get = |key: &str| supplier.get(key).map(String::as_str);
        assert_eq!(
            get("sourcing"),
            Some("single"),
            "the sourcing links were read"
        );
        assert_eq!(
            get("exposure"),
            Some("18000.00"),
            "sources the at-risk order's cause"
        );
        assert_eq!(get("on_path"), Some("true"), "the late PO's vendor");
        let dependent = projection
            .rows
            .iter()
            .find(|r| r.get("kind").is_some_and(|k| k == "dependent_order"))
            .expect("the dependent order row");
        assert_eq!(dependent.get("at_risk").map(String::as_str), Some("true"));
    }

    #[tokio::test]
    async fn a_crafted_subject_is_refused_before_any_read() {
        let world = ScriptedWorld::new(true, false);
        let crafted = EntityUri(format!("{}sale.order/1> }}", ontology::ID_NAMESPACE));
        let err = project_on(&world, &crafted, Tab::Graph, None, today()).await;
        assert!(matches!(err, Err(EngineError::Invariant(_))));
        assert!(world.asked.lock().expect("lock").is_empty());
    }
}
