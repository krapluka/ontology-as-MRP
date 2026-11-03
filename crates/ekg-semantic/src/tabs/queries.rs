//! The tab queries: embedded from `ontology/tabs/*.rq` and composed per read.
//!
//! Every query is capability configuration authored as a `.rq` file next to the rule it
//! mirrors; this module only splices the shared open-inbound fragment into them, fills the
//! day-window bounds (computed here, never `NOW()`), and interpolates the subject / cause IRIs.
//! Each IRI is first rebuilt by [`ontology::canonical_subject`], so a crafted IRI never reaches
//! a query.

use ekg_domain::{EngineError, EntityUri, Result};

use super::Tab;
use crate::ontology;

/// The shared open-inbound definition, spliced into each query at [`OPEN_INBOUND`].
pub const OPEN_INBOUND_FRAGMENT: &str = include_str!("../../ontology/tabs/open-inbound.rq");
/// Timeline: promised orders (promise / ready) and open inbound arrivals.
pub const TIMELINE_RQ: &str = include_str!("../../ontology/tabs/timeline.rq");
/// Graph: the six-column supply graph as typed nodes + edges.
pub const GRAPH_RQ: &str = include_str!("../../ontology/tabs/graph.rq");
/// Flow: every entity placed in one of the five stages with its tile facts.
pub const FLOW_RQ: &str = include_str!("../../ontology/tabs/flow.rq");
/// Suppliers: the supplier table (lead time + on-time-delivery evidence).
pub const SUPPLIERS_RQ: &str = include_str!("../../ontology/tabs/suppliers.rq");
/// Suppliers: the per-supplier detail (open POs, stock/safety/demand, dependent orders).
pub const SUPPLIERS_DETAIL_RQ: &str = include_str!("../../ontology/tabs/suppliers-detail.rq");
/// Tab input: every sourcing link as (supplier, component, sequence).
pub const SOURCING_RQ: &str = include_str!("../../ontology/tabs/sourcing.rq");
/// Tab input: the selected point's rule-independent path.
pub const PATH_RQ: &str = include_str!("../../ontology/tabs/path.rq");
/// Tab input: the selected point's cause-dependent path candidates.
pub const PATH_CAUSES_RQ: &str = include_str!("../../ontology/tabs/path-causes.rq");
/// Proposer input: the entities of a supply-risk point's cause chain.
pub const CAUSE_CHAIN_RQ: &str = include_str!("../../ontology/tabs/cause-chain.rq");
/// Proposer input: the work centers and capacity records a process point may act on.
pub const PROCESS_CONTEXT_RQ: &str = include_str!("../../ontology/tabs/process-context.rq");
/// Proposer input: every work centre, identifiers only.
pub const WORK_CENTRES_RQ: &str = include_str!("../../ontology/tabs/work-centres.rq");
/// Proposer input: the work orders scheduled on one work centre, identifiers only.
pub const WORK_CENTRE_SCHEDULE_RQ: &str =
    include_str!("../../ontology/tabs/work-centre-schedule.rq");

/// Marker the open-inbound fragment is spliced in at.
const OPEN_INBOUND: &str = "{{OPEN_INBOUND}}";
/// Marker for the canonical entity-IRI regex (built from [`ontology::ID_NAMESPACE`]).
const CANONICAL_IRI: &str = "{{CANONICAL_IRI}}";
/// Marker for today's ISO day.
const TODAY: &str = "{{TODAY}}";
/// Marker for the first day of the trailing on-time-delivery window.
const OTD_FROM: &str = "{{OTD_FROM}}";
/// Marker for the last day of the forward demand window.
const DEMAND_TO: &str = "{{DEMAND_TO}}";
/// Placeholder IRI the subject replaces in the path queries.
const SUBJECT: &str = "<urn:ekg:subject>";
/// Marker the cause components' IRIs replace in `path-causes.rq`'s `VALUES` block.
const CAUSES: &str = "{{CAUSES}}";

/// Days in the trailing on-time-delivery window.
pub const OTD_WINDOW_DAYS: i64 = 90;
/// Days in the forward demand window.
pub const DEMAND_WINDOW_DAYS: i64 = 30;

/// A calendar day (UTC), as whole days since the Unix epoch. The read layer takes "today" from
/// the clock once per read; tests pin it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Day(i64);

impl Day {
    /// 1970-01-01, the day the count starts from.
    pub const EPOCH: Self = Self(0);

    /// Today (UTC) from the system clock.
    #[must_use]
    pub fn today() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Self(i64::try_from(secs / 86_400).unwrap_or(0))
    }

    /// The day `days` after (negative: before) this one.
    #[must_use]
    pub fn plus(self, days: i64) -> Self {
        Self(self.0 + days)
    }

    /// Parse a `YYYY-MM-DD` (optionally followed by a `T…` time) into its day, or `None` for
    /// anything that is not a real calendar day.
    #[must_use]
    pub fn from_iso(value: &str) -> Option<Self> {
        let date = value.split('T').next()?;
        let mut parts = date.split('-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || !(1..=12).contains(&month) {
            return None;
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let max_day = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ if leap => 29,
            _ => 28,
        };
        if !(1..=max_day).contains(&day) {
            return None;
        }
        // Howard Hinnant's days_from_civil (proleptic Gregorian).
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = if month > 2 { month - 3 } else { month + 9 };
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(Self(era * 146_097 + doe - 719_468))
    }

    /// Whole days from `earlier` to this day (negative when this day is before it).
    #[must_use]
    pub fn days_since(self, earlier: Self) -> i64 {
        self.0 - earlier.0
    }

    /// The day as an ISO `YYYY-MM-DD` literal, the only form interpolated into a query.
    #[must_use]
    pub fn iso(self) -> String {
        // Howard Hinnant's civil_from_days (proleptic Gregorian).
        let z = self.0 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// The day-window bounds a read interpolates: `today`, the first day of the trailing
/// on-time-delivery window, and the last day of the forward demand window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Today.
    pub today: Day,
    /// `today - 90 days`.
    pub otd_from: Day,
    /// `today + 30 days`.
    pub demand_to: Day,
}

impl Window {
    /// The windows around `today`.
    #[must_use]
    pub fn around(today: Day) -> Self {
        Self {
            today,
            otd_from: today.plus(-OTD_WINDOW_DAYS),
            demand_to: today.plus(DEMAND_WINDOW_DAYS),
        }
    }
}

/// The regex a canonical entity IRI matches (`{ID_NAMESPACE}<model>/<digits>`), as a SPARQL
/// string-literal body: the namespace's dots are bracketed rather than backslash-escaped so
/// the literal needs no escaping.
#[must_use]
pub fn canonical_iri_regex() -> String {
    format!(
        "^{}[a-z0-9._]+/[0-9]+$",
        ontology::ID_NAMESPACE.replace('.', "[.]")
    )
}

/// Splice the shared open-inbound fragment and the canonical-IRI regex into a template.
#[must_use]
pub fn splice(template: &str) -> String {
    template
        .replace(OPEN_INBOUND, OPEN_INBOUND_FRAGMENT)
        .replace(CANONICAL_IRI, &canonical_iri_regex())
}

/// [`splice`] a tab query template and fill its window bounds.
#[must_use]
pub fn compose(template: &str, window: &Window) -> String {
    splice(template)
        .replace(TODAY, &window.today.iso())
        .replace(OTD_FROM, &window.otd_from.iso())
        .replace(DEMAND_TO, &window.demand_to.iso())
}

/// The structural (subject-free) query templates whose rows a tab serves. The read
/// ([`tab_queries`]) and the row vocabulary ([`super::row_vocabulary`]) both use this list, so
/// the vocabulary cannot describe a different set of queries than a read runs.
#[must_use]
pub(crate) fn templates(tab: Tab) -> &'static [&'static str] {
    match tab {
        Tab::Timeline => &[TIMELINE_RQ],
        Tab::Graph => &[GRAPH_RQ],
        Tab::Flow => &[FLOW_RQ],
        Tab::Suppliers => &[SUPPLIERS_RQ, SUPPLIERS_DETAIL_RQ],
    }
}

/// The structural (subject-free) queries whose rows a tab serves, composed for `window`.
#[must_use]
pub fn tab_queries(tab: Tab, window: &Window) -> Vec<String> {
    templates(tab).iter().map(|t| compose(t, window)).collect()
}

/// Remove `#` comments outside string literals and IRIs (a `<` followed by a space or `=` is
/// an operator, not an IRI). The query tests and the row vocabulary share it.
#[must_use]
pub(crate) fn strip_comments(sparql: &str) -> String {
    let mut out = String::new();
    for line in sparql.lines() {
        let chars: Vec<char> = line.chars().collect();
        let (mut in_iri, mut in_string) = (false, false);
        for (i, &c) in chars.iter().enumerate() {
            match c {
                '"' if !in_iri => in_string = !in_string,
                '<' if !in_iri && !in_string => {
                    in_iri = !matches!(chars.get(i + 1), None | Some('=' | ' ' | '\t'));
                }
                '>' if in_iri => in_iri = false,
                '#' if !in_iri && !in_string => break,
                _ => {}
            }
            out.push(c);
        }
        out.push('\n');
    }
    out
}

/// The sourcing-link input query.
#[must_use]
pub fn sourcing_query() -> String {
    splice(SOURCING_RQ)
}

/// Rebuild `iri` canonically, or refuse it. This is the only way an IRI reaches a path query.
fn canonical(iri: &str) -> Result<String> {
    ontology::canonical_subject(iri)
        .map(|c| c.0)
        .ok_or_else(|| {
            EngineError::Invariant(format!(
                "refusing to interpolate a non-canonical entity IRI into a tab query: {iri:?}"
            ))
        })
}

/// The rule-independent path query for `subject` (re-canonicalized before substitution).
///
/// # Errors
/// [`EngineError::Invariant`] if `subject` is not a canonical entity IRI.
pub fn path_query(subject: &EntityUri) -> Result<String> {
    let subject = canonical(&subject.0)?;
    Ok(splice(PATH_RQ).replace(SUBJECT, &format!("<{subject}>")))
}

/// The cause-dependent path query for `subject` and its cause components. `Ok(None)` for an
/// empty cause set, so an empty `VALUES` block is never issued (it would erase the chain).
///
/// # Errors
/// [`EngineError::Invariant`] if the subject or any cause is not a canonical entity IRI
/// (checked before anything is substituted).
pub fn path_causes_query<'a>(
    subject: &EntityUri,
    causes: impl IntoIterator<Item = &'a str>,
) -> Result<Option<String>> {
    let subject = canonical(&subject.0)?;
    let causes = causes
        .into_iter()
        .map(|iri| canonical(iri).map(|c| format!("<{c}>")))
        .collect::<Result<Vec<_>>>()?;
    if causes.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        splice(PATH_CAUSES_RQ)
            .replace(SUBJECT, &format!("<{subject}>"))
            .replace(CAUSES, &causes.join(" ")),
    ))
}

/// The cause-chain query for a point's cause components. `Ok(None)` for an empty cause set,
/// so an empty `VALUES` block is never issued.
///
/// # Errors
/// [`EngineError::Invariant`] if any cause is not a canonical entity IRI (checked before
/// anything is substituted).
pub fn cause_chain_query<'a>(causes: impl IntoIterator<Item = &'a str>) -> Result<Option<String>> {
    let causes = causes
        .into_iter()
        .map(|iri| canonical(iri).map(|c| format!("<{c}>")))
        .collect::<Result<Vec<_>>>()?;
    if causes.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        splice(CAUSE_CHAIN_RQ).replace(CAUSES, &causes.join(" ")),
    ))
}

/// The process-context query for a manufacturing-order `subject` (re-canonicalized before
/// substitution).
///
/// # Errors
/// [`EngineError::Invariant`] if `subject` is not a canonical entity IRI.
pub fn process_context_query(subject: &EntityUri) -> Result<String> {
    let subject = canonical(&subject.0)?;
    Ok(splice(PROCESS_CONTEXT_RQ).replace(SUBJECT, &format!("<{subject}>")))
}

/// The work-centres query (subject-free, spliced only).
#[must_use]
pub fn work_centres_query() -> String {
    splice(WORK_CENTRES_RQ)
}

/// The schedule query for `work_centre` (re-canonicalized before substitution).
///
/// # Errors
/// [`EngineError::Invariant`] if `work_centre` is not a canonical entity IRI.
pub fn work_centre_schedule_query(work_centre: &EntityUri) -> Result<String> {
    let work_centre = canonical(&work_centre.0)?;
    Ok(splice(WORK_CENTRE_SCHEDULE_RQ).replace(SUBJECT, &format!("<{work_centre}>")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    /// Every template the tabs embed, with its file name for failure messages.
    const TEMPLATES: [(&str, &str); 12] = [
        ("timeline.rq", TIMELINE_RQ),
        ("graph.rq", GRAPH_RQ),
        ("flow.rq", FLOW_RQ),
        ("suppliers.rq", SUPPLIERS_RQ),
        ("suppliers-detail.rq", SUPPLIERS_DETAIL_RQ),
        ("sourcing.rq", SOURCING_RQ),
        ("path.rq", PATH_RQ),
        ("path-causes.rq", PATH_CAUSES_RQ),
        ("cause-chain.rq", CAUSE_CHAIN_RQ),
        ("process-context.rq", PROCESS_CONTEXT_RQ),
        ("work-centres.rq", WORK_CENTRES_RQ),
        ("work-centre-schedule.rq", WORK_CENTRE_SCHEDULE_RQ),
    ];

    /// No marker survived: every `{{…}}` and the subject placeholder were filled.
    fn assert_filled(name: &str, query: &str) {
        assert!(
            !query.contains("{{") && !query.contains("urn:ekg:"),
            "{name}: a placeholder survived composition"
        );
    }

    fn day(iso: &str) -> Day {
        Day::from_iso(iso).expect("a real day")
    }

    /// A structural parse smoke test (no SPARQL engine is in the workspace):
    /// braces, parentheses and quotes balance outside comments and literals, the query is a
    /// SELECT … WHERE, and it binds the vocabulary prefix to the canonical namespace.
    fn assert_well_formed(name: &str, query: &str) {
        let body = strip_comments(query);
        let (mut braces, mut parens, mut quote) = (0_i64, 0_i64, false);
        for c in body.chars() {
            match (quote, c) {
                (_, '"') => quote = !quote,
                (false, '{') => braces += 1,
                (false, '}') => braces -= 1,
                (false, '(') => parens += 1,
                (false, ')') => parens -= 1,
                _ => {}
            }
            assert!(braces >= 0 && parens >= 0, "{name}: closes before it opens");
        }
        assert_eq!((braces, parens), (0, 0), "{name}: unbalanced braces/parens");
        assert!(!quote, "{name}: unterminated string literal");
        assert!(
            body.contains("SELECT") && body.contains("WHERE"),
            "{name}: not a SELECT"
        );
        assert!(
            body.contains(&format!("PREFIX ekg: <{}>", ontology::VOCAB_NAMESPACE)),
            "{name}: must bind ekg: to the canonical vocabulary namespace"
        );
        assert!(
            !body.contains("NOW()"),
            "{name}: the window comes from Rust, never NOW()"
        );
    }

    #[test]
    fn every_tab_query_is_embedded_and_well_formed_once_composed() {
        let window = Window::around(day("2027-10-01"));
        assert!(OPEN_INBOUND_FRAGMENT.contains("SELECT ?ib_po ?ib_pol ?ib_product"));
        for (name, template) in TEMPLATES {
            assert!(!template.trim().is_empty(), "{name} is embedded");
        }
        // The served tab queries, as a read composes them.
        for tab in Tab::ALL {
            for query in tab_queries(tab, &window) {
                assert_filled(tab.name(), &query);
                assert_well_formed(tab.name(), &query);
            }
        }
        // The input queries, as a read builds them: these are only spliced (no window
        // bounds), so a window marker added to one must fail here, not reach the store.
        let subject = EntityUri(ontology::entity_iri("sale.order", 1));
        let cause = ontology::entity_iri("product.product", 2);
        let inputs = [
            ("sourcing.rq", sourcing_query()),
            (
                "path.rq",
                path_query(&subject).expect("a canonical subject"),
            ),
            (
                "path-causes.rq",
                path_causes_query(&subject, [cause.as_str()])
                    .expect("canonical causes")
                    .expect("a non-empty cause set"),
            ),
            (
                "cause-chain.rq",
                cause_chain_query([cause.as_str()])
                    .expect("canonical causes")
                    .expect("a non-empty cause set"),
            ),
            (
                "process-context.rq",
                process_context_query(&EntityUri(ontology::entity_iri("mrp.production", 3)))
                    .expect("a canonical subject"),
            ),
            ("work-centres.rq", work_centres_query()),
            (
                "work-centre-schedule.rq",
                work_centre_schedule_query(&EntityUri(ontology::entity_iri("mrp.workcenter", 4)))
                    .expect("a canonical work centre"),
            ),
        ];
        for (name, query) in inputs {
            assert_filled(name, &query);
            assert_well_formed(name, &query);
        }
        // The capability's display queries, as a read builds them.
        for (name, query) in crate::display::queries::composed_for_test() {
            assert_filled(name, &query);
            assert_well_formed(name, &query);
        }
    }

    #[test]
    fn the_open_inbound_definition_is_spliced_in_everywhere_it_is_used() {
        // One definition, many users: every query that lights, lists or draws inbound carries
        // the fragment verbatim, and none restates it by hand.
        let window = Window::around(day("2027-10-01"));
        for (name, template) in [
            ("timeline.rq", TIMELINE_RQ),
            ("graph.rq", GRAPH_RQ),
            ("flow.rq", FLOW_RQ),
            ("suppliers-detail.rq", SUPPLIERS_DETAIL_RQ),
            ("path-causes.rq", PATH_CAUSES_RQ),
            ("cause-chain.rq", CAUSE_CHAIN_RQ),
        ] {
            assert!(
                template.matches(OPEN_INBOUND).count() >= 1,
                "{name} splices it"
            );
            assert!(
                !template.contains("ekg:qty_received"),
                "{name} must not restate it"
            );
            assert!(compose(template, &window).contains(OPEN_INBOUND_FRAGMENT));
        }
        // The Graph draws both the open PO nodes and their open lines' product edges from it.
        assert_eq!(
            GRAPH_RQ.matches(OPEN_INBOUND).count(),
            2,
            "graph.rq: nodes + line edges"
        );
        // The fragment carries the rule's receipt truth.
        let fragment = strip_comments(OPEN_INBOUND_FRAGMENT);
        for term in [
            "ekg:qty_received",
            "FILTER(?ib_open > 0)",
            "ekg:lifecycle_state \"cancel\"",
            "ekg:completed_at",
            "ekg:transport_status \"cancel\"",
            "COALESCE(?ib_ship_eta, ?ib_planned)",
        ] {
            assert!(
                fragment.contains(term),
                "open-inbound fragment missing {term}"
            );
        }
    }

    #[test]
    fn window_bounds_are_substituted_as_iso_days() {
        let window = Window::around(day("2027-10-01"));
        assert_eq!(window.otd_from.iso(), "2027-07-03", "90 days back");
        assert_eq!(window.demand_to.iso(), "2027-10-31", "30 days ahead");
        let suppliers = compose(SUPPLIERS_RQ, &window);
        assert!(suppliers.contains("?doneDay >= \"2027-07-03\" && ?doneDay <= \"2027-10-01\""));
        let detail = compose(SUPPLIERS_DETAIL_RQ, &window);
        assert!(detail.contains("?demandDay >= \"2027-10-01\" && ?demandDay <= \"2027-10-31\""));
        // The canonical-IRI regex is built from the namespace, dots bracketed.
        assert_eq!(
            canonical_iri_regex(),
            "^https://ekg[.]example/id/[a-z0-9._]+/[0-9]+$"
        );
        assert!(compose(GRAPH_RQ, &window).contains(&canonical_iri_regex()));
    }

    #[test]
    fn days_round_trip_through_iso_across_leap_years_and_the_epoch() {
        for iso in [
            "1970-01-01",
            "2024-02-29",
            "2027-12-31",
            "2027-01-01",
            "2000-03-01",
        ] {
            assert_eq!(day(iso).iso(), iso);
        }
        assert_eq!(day("2024-02-28").plus(1).iso(), "2024-02-29");
        assert_eq!(day("1999-12-31").plus(1).iso(), "2000-01-01");
        assert_eq!(
            day("2027-10-19T08:30:00").iso(),
            "2027-10-19",
            "time is ignored"
        );
        assert!(Day::from_iso("2027-02-30").is_none());
        assert!(Day::from_iso("2027-13-01").is_none());
        assert!(Day::from_iso("not-a-day").is_none());
        assert!(
            Day::today() > day("2000-01-01"),
            "the clock reads a plausible today"
        );
    }

    #[test]
    fn tab_queries_cover_each_tab() {
        let window = Window::around(day("2027-10-01"));
        assert_eq!(tab_queries(Tab::Timeline, &window).len(), 1);
        assert_eq!(tab_queries(Tab::Graph, &window).len(), 1);
        assert_eq!(tab_queries(Tab::Flow, &window).len(), 1);
        assert_eq!(
            tab_queries(Tab::Suppliers, &window).len(),
            2,
            "table + detail"
        );
    }

    #[test]
    fn a_crafted_iri_is_rejected_before_substitution() {
        let crafted = EntityUri(format!(
            "{}sale.order/1> }} UNION {{ ?s ?p ?o",
            ontology::ID_NAMESPACE
        ));
        assert!(matches!(
            path_query(&crafted),
            Err(EngineError::Invariant(_))
        ));
        let subject = EntityUri(ontology::entity_iri("sale.order", 1));
        let q = path_query(&subject).expect("a canonical subject");
        assert!(q.contains("<https://ekg.example/id/sale.order/1> a ekg:SalesOrder"));
        assert!(!q.contains(SUBJECT), "every subject marker is filled");
        // A crafted cause (or a provenance node) is refused, even beside good ones.
        for bad in [
            format!("{}product.product/2#source", ontology::ID_NAMESPACE),
            format!("{}product.product/2> }} ", ontology::ID_NAMESPACE),
        ] {
            let good = ontology::entity_iri("product.product", 3);
            assert!(matches!(
                path_causes_query(&subject, [good.as_str(), bad.as_str()]),
                Err(EngineError::Invariant(_))
            ));
        }
        assert!(matches!(
            path_causes_query(
                &crafted,
                [ontology::entity_iri("product.product", 3).as_str()]
            ),
            Err(EngineError::Invariant(_))
        ));
    }

    #[test]
    fn the_cause_query_carries_every_cause_and_an_empty_set_issues_none() {
        let subject = EntityUri(ontology::entity_iri("sale.order", 1));
        let (a, b) = (
            ontology::entity_iri("product.product", 4),
            ontology::entity_iri("product.product", 9),
        );
        let q = path_causes_query(&subject, [a.as_str(), b.as_str()])
            .expect("canonical causes")
            .expect("a non-empty cause set issues the query");
        assert!(q.contains(&format!("VALUES ?cause {{ <{a}> <{b}> }}")));
        assert!(q.contains("<https://ekg.example/id/sale.order/1> ekg:commitment_date"));
        // An empty cause set never produces the query (an empty VALUES would erase the chain).
        assert!(
            path_causes_query(&subject, std::iter::empty())
                .expect("no causes is not an error")
                .is_none()
        );
    }

    #[test]
    fn the_cause_chain_query_names_every_cause_and_refuses_a_crafted_iri() {
        let (battery, drive) = (
            ontology::entity_iri("product.product", 4),
            ontology::entity_iri("product.product", 7),
        );
        let query = cause_chain_query([battery.as_str(), drive.as_str()])
            .expect("canonical")
            .expect("non-empty");
        assert!(query.contains(&format!("VALUES ?cause {{ <{battery}> <{drive}> }}")));
        assert!(query.contains("SELECT ?cause ?line ?po ?vendor ?ship ?arrival"));
        assert_eq!(cause_chain_query([]).expect("empty is fine"), None);
        assert!(cause_chain_query(["https://ekg.example/id/product.product/4> } ; DROP"]).is_err());
    }

    #[test]
    fn the_schedule_query_names_its_work_centre_and_refuses_a_crafted_iri() {
        let centre = EntityUri(ontology::entity_iri("mrp.workcenter", 4));
        let query = work_centre_schedule_query(&centre).expect("canonical");
        assert!(query.contains(&format!("ekg:performedBy <{}>", centre.0)));
        assert!(query.contains("?entity a ekg:WorkOrder"));
        assert!(query.contains("SELECT DISTINCT ?entity"));
        let crafted = EntityUri("https://ekg.example/id/mrp.workcenter/4> } ; DROP".to_owned());
        assert!(work_centre_schedule_query(&crafted).is_err());
    }
}
