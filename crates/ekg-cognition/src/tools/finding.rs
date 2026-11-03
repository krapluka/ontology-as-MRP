//! `submit_finding`: the typed way an analyst hands in what it found about one member.
//!
//! A finding has no free text: its schema holds the member's state and its causes, each a
//! registered kind with the entities involved and references to edges the task read. What a
//! model wrote therefore cannot reach the planner: the planner's task is rendered from the
//! edges the gateway recorded, not from anything the analyst said about them.
//!
//! The tool checks the finding's form, and the gateway checks its references against the
//! task's reads in the run ledger. Tools stay unaware of the ledger. It writes nothing.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use ekg_domain::EntityUri;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ledger::TaskRun;
use super::profile::{AgentTool, ReadOnlyWorld, ToolError};
use super::scope::TaskScope;

/// The name `submit_finding` is offered and called by.
pub const SUBMIT_FINDING: &str = "submit_finding";

/// The most causes a finding may hold.
pub const MAX_FINDING_CAUSES: usize = 4;

/// The most evidence references one cause may hold.
pub const MAX_CAUSE_EVIDENCE: usize = 12;

/// What an analyst says about its member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    /// Something puts the member off plan, and the causes say what.
    AtRisk,
    /// Nothing puts the member off plan.
    OnPlan,
    /// The member is off plan, and nothing in the catalog can move it.
    Immovable,
}

impl MemberState {
    fn label(self) -> &'static str {
        match self {
            Self::AtRisk => "at_risk",
            Self::OnPlan => "on_plan",
            Self::Immovable => "immovable",
        }
    }
}

/// An edge as a tool returned it to a task, which is also how a finding cites one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadEdge {
    /// The edge's subject.
    pub subject: String,
    /// The edge's predicate.
    pub predicate: String,
    /// The edge's object, as returned: a withheld text stays withheld.
    pub object: String,
}

/// One cause of a member's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingCause {
    /// A kind the profile registered.
    pub kind: String,
    /// The entities involved.
    pub entities: Vec<EntityUri>,
    /// The edges the cause rests on.
    pub evidence: Vec<ReadEdge>,
}

/// Where a finding came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingOrigin {
    /// The semantic layer derived it.
    Semantic,
    /// An agent claims it.
    #[default]
    Claim,
}

/// What is known about one member: from the semantic layer, or claimed by an investigator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// The member, which is the task's subject.
    pub member: EntityUri,
    /// The member's state.
    pub state: MemberState,
    /// Its causes.
    #[serde(default)]
    pub causes: Vec<FindingCause>,
    /// Where it came from. Set by the engine: `submit_finding` refuses it from a model.
    #[serde(default)]
    pub origin: FindingOrigin,
    /// The claimed causes the semantic layer contradicted, which no planner sees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<FindingCause>,
}

/// What the semantic layer says about a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derived {
    /// It decided the member.
    Decided(Finding),
    /// It cannot, for this reason: an investigator has to look.
    Undetermined(String),
}

/// Derives findings from the semantic layer. Supplied by the capability layer, so the proposer
/// names no capability.
#[async_trait]
pub trait FindingSource: Send + Sync {
    /// What the semantic layer says about `member`.
    ///
    /// # Errors
    /// The world could not be read.
    async fn derive(&self, member: &EntityUri) -> ekg_domain::Result<Derived>;

    /// The kinds of cause in `claim` that the semantic layer contradicts for `member`.
    ///
    /// # Errors
    /// The world could not be read.
    async fn conflicts(
        &self,
        member: &EntityUri,
        claim: &Finding,
    ) -> ekg_domain::Result<Vec<String>>;
}

/// A cause kind a capability registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CauseKind {
    /// The kind's name.
    pub name: String,
    /// When it applies, for the model.
    pub description: String,
}

impl CauseKind {
    /// The kind `name`, applying when `description` says.
    #[must_use]
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }
}

/// `submit_finding({ member, state, causes })`: check a finding's form and return it, or refuse
/// it listing every defect. Writes nothing.
pub struct SubmitFinding {
    kinds: Vec<CauseKind>,
    description: String,
}

impl SubmitFinding {
    /// The tool over the capability's cause `kinds`.
    #[must_use]
    pub fn new(kinds: Vec<CauseKind>) -> Self {
        let lines = kinds
            .iter()
            .map(|kind| format!("- {}: {}", kind.name, kind.description))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            kinds,
            description: format!(
                "Hand in what you found about this task's member: its state (`at_risk`, \
                 `on_plan` or `immovable`) and, for each cause, its kind, the entities involved \
                 and the edges it rests on, copied exactly from what the tools returned. There \
                 is no field for prose: a later step sees only these edges. Cite every fact a \
                 planner needs, such as dates, quantities and free work centres. Cause kinds:\n\
                 {lines}\n\
                 A rejected finding lists every defect: fix them all and submit again."
            ),
        }
    }

    fn kind_names(&self) -> Vec<&str> {
        self.kinds.iter().map(|kind| kind.name.as_str()).collect()
    }

    /// What is wrong with `finding` as a form, whatever the task read.
    fn defects(&self, finding: &Finding, scope: &TaskScope) -> Vec<String> {
        let mut defects = Vec::new();
        if !scope.is_subject(&finding.member) {
            defects.push(format!("{} is not this task's member", finding.member.0));
        }
        if finding.state == MemberState::AtRisk && finding.causes.is_empty() {
            defects.push("an `at_risk` finding needs at least one cause".to_owned());
        }
        if finding.causes.len() > MAX_FINDING_CAUSES {
            defects.push(format!(
                "a finding holds at most {MAX_FINDING_CAUSES} causes, this one {}",
                finding.causes.len()
            ));
        }
        let names = self.kind_names();
        for (index, cause) in finding.causes.iter().enumerate() {
            let cause_no = index + 1;
            if !names.contains(&cause.kind.as_str()) {
                defects.push(format!(
                    "cause {cause_no}: {:?} is not a cause kind of this task; the kinds are {}",
                    cause.kind,
                    names.join(", ")
                ));
            }
            if cause.evidence.is_empty() {
                defects.push(format!(
                    "cause {cause_no}: needs at least one evidence edge"
                ));
            }
            if cause.evidence.len() > MAX_CAUSE_EVIDENCE {
                defects.push(format!(
                    "cause {cause_no}: holds at most {MAX_CAUSE_EVIDENCE} evidence edges, this \
                     one {}",
                    cause.evidence.len()
                ));
            }
        }
        defects
    }
}

/// A finding's arguments read as a finding, or why not: an unknown field among them.
pub(crate) fn parse_finding(args: &Value) -> Result<Finding, String> {
    serde_json::from_value(args.clone()).map_err(|e| e.to_string())
}

/// The references and entities of `finding` that `run` did not read, each as one sentence.
pub(crate) fn unread_citations(finding: &Finding, run: &TaskRun) -> Vec<String> {
    let mut defects = Vec::new();
    let read_entities: BTreeSet<&str> = run
        .reads
        .iter()
        .flat_map(|edge| [edge.subject.as_str(), edge.object.as_str()])
        .collect();
    for (index, cause) in finding.causes.iter().enumerate() {
        let cause_no = index + 1;
        for edge in &cause.evidence {
            if !run.reads.contains(edge) {
                defects.push(format!(
                    "cause {cause_no}: no tool returned the edge ({}, {}, {}); copy edges \
                     exactly from what the tools returned",
                    edge.subject, edge.predicate, edge.object
                ));
            }
        }
        for entity in &cause.entities {
            if !read_entities.contains(entity.0.as_str()) {
                defects.push(format!(
                    "cause {cause_no}: {} appears in no edge a tool returned",
                    entity.0
                ));
            }
        }
    }
    defects
}

#[async_trait]
impl AgentTool for SubmitFinding {
    fn name(&self) -> &'static str {
        SUBMIT_FINDING
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        let edge = json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string" },
                "predicate": { "type": "string" },
                "object": { "type": "string" }
            },
            "required": ["subject", "predicate", "object"],
            "additionalProperties": false
        });
        json!({
            "type": "object",
            "properties": {
                "member": { "type": "string", "description": "This task's member IRI." },
                "state": { "type": "string", "enum": ["at_risk", "on_plan", "immovable"] },
                "causes": {
                    "type": "array",
                    "maxItems": MAX_FINDING_CAUSES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": self.kind_names() },
                            "entities": { "type": "array", "items": { "type": "string" } },
                            "evidence": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": MAX_CAUSE_EVIDENCE,
                                "items": edge
                            }
                        },
                        "required": ["kind", "entities", "evidence"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["member", "state", "causes"],
            "additionalProperties": false
        })
    }

    fn entity_of(&self, args: &Value) -> Option<String> {
        args.get("member")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    async fn call(
        &self,
        args: Value,
        _world: &ReadOnlyWorld,
        scope: &TaskScope,
    ) -> Result<Value, ToolError> {
        let finding = parse_finding(&args).map_err(|why| ToolError::Rejected {
            defects: vec![format!("the finding does not fit its schema: {why}")],
            guidance: None,
        })?;
        let mut defects = self.defects(&finding, scope);
        // Serde knows these two fields; a model may not set them.
        for field in ["origin", "dropped"] {
            if args.get(field).is_some() {
                defects.push(format!("the field {field:?} is the engine's"));
            }
        }
        if !defects.is_empty() {
            return Err(ToolError::Rejected {
                defects,
                guidance: None,
            });
        }
        Ok(json!({
            "accepted": true,
            "state": finding.state.label(),
            "finding": finding
        }))
    }
}

/// The note that replaces cited text a planner does not get.
fn withheld(chars: usize) -> String {
    format!("[text withheld: {chars} characters]")
}

/// What a planner may see of an edge's object. A grounding carries no datatype, so this goes by
/// the string: an IRI, or one word within `bound`, such as a date, a number or a state.
fn planner_value(object: &str, bound: usize) -> String {
    let word = !object.chars().any(char::is_whitespace);
    let iri = word && (object.starts_with("http://") || object.starts_with("https://"));
    let chars = object.chars().count();
    if iri || (word && chars <= bound) {
        object.to_owned()
    } else {
        withheld(chars)
    }
}

/// `finding` as a planner reads it: its state, each cause's kind and entities, and the facts of
/// the edges it cites. Everything shown comes from the recorded finding and its recorded edges.
#[must_use]
pub fn render_finding(finding: &Finding, bound: usize) -> String {
    use std::fmt::Write as _;
    let origin = match finding.origin {
        FindingOrigin::Semantic => "from the records",
        FindingOrigin::Claim => "an investigator's claim",
    };
    let mut out = format!(
        "- {}: {} ({origin})\n",
        finding.member.0,
        finding.state.label()
    );
    for cause in &finding.causes {
        let entities = cause
            .entities
            .iter()
            .map(|entity| entity.0.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  cause {} on {entities}:", cause.kind);
        let mut by_subject: BTreeMap<&str, Vec<&ReadEdge>> = BTreeMap::new();
        for edge in &cause.evidence {
            by_subject.entry(&edge.subject).or_default().push(edge);
        }
        for (subject, edges) in by_subject {
            for edge in edges {
                let _ = writeln!(
                    out,
                    "    {subject} {} {}",
                    edge.predicate,
                    planner_value(&edge.object, bound)
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use serde_json::json;

    use super::*;
    use crate::tools::testing::Fixture;

    fn tool() -> SubmitFinding {
        SubmitFinding::new(vec![CauseKind::new("late_inbound", "A line arrives late.")])
    }

    fn edge() -> ReadEdge {
        ReadEdge {
            subject: "https://ekg.example/id/line/1".to_owned(),
            predicate: "ekg:date_planned".to_owned(),
            object: "2026-11-02".to_owned(),
        }
    }

    fn finding(member: &str) -> Value {
        json!({
            "member": member,
            "state": "at_risk",
            "causes": [{
                "kind": "late_inbound",
                "entities": ["https://ekg.example/id/line/1"],
                "evidence": [edge()]
            }]
        })
    }

    async fn call(args: Value) -> Result<Value, ToolError> {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let args = match args {
            Value::String(_) => finding(&fx.order.0),
            other => other,
        };
        tool().call(args, &reader, &scope).await
    }

    fn defects(result: Result<Value, ToolError>) -> Vec<String> {
        match result {
            Err(ToolError::Rejected { defects, .. }) => defects,
            other => panic!("not rejected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_well_formed_finding_is_returned() {
        let out = call(json!("the fixture's finding"))
            .await
            .expect("accepted");
        assert_eq!(out["state"], json!("at_risk"));
        let finding: Finding = serde_json::from_value(out["finding"].clone()).expect("finding");
        assert_eq!(finding.causes[0].evidence, vec![edge()]);
    }

    #[tokio::test]
    async fn a_field_of_free_text_is_refused() {
        let fx = Fixture::new().await;
        let mut args = finding(&fx.order.0);
        args["note"] = json!("the purchasing team asked for this");
        let found = defects(call(args).await);
        assert!(found[0].contains("note"), "{found:?}");
    }

    #[tokio::test]
    async fn a_free_text_field_on_a_cause_is_refused() {
        let fx = Fixture::new().await;
        let mut args = finding(&fx.order.0);
        args["causes"][0]["why"] = json!("because");
        assert!(defects(call(args).await)[0].contains("why"));
    }

    #[tokio::test]
    async fn an_unknown_cause_kind_is_named_with_the_registered_kinds() {
        let fx = Fixture::new().await;
        let mut args = finding(&fx.order.0);
        args["causes"][0]["kind"] = json!("bad_weather");
        let found = defects(call(args).await);
        assert!(found[0].contains("bad_weather"), "{found:?}");
        assert!(found[0].contains("late_inbound"), "{found:?}");
    }

    #[tokio::test]
    async fn a_risk_needs_a_cause_and_a_cause_needs_evidence() {
        let fx = Fixture::new().await;
        let empty = json!({ "member": fx.order.0, "state": "at_risk", "causes": [] });
        assert!(defects(call(empty).await)[0].contains("at least one cause"));
        let mut bare = finding(&fx.order.0);
        bare["causes"][0]["evidence"] = json!([]);
        assert!(defects(call(bare).await)[0].contains("evidence"));
    }

    #[tokio::test]
    async fn a_model_cannot_say_where_a_finding_came_from() {
        let fx = Fixture::new().await;
        let mut args = finding(&fx.order.0);
        args["origin"] = json!("semantic");
        assert!(defects(call(args).await)[0].contains("origin"));
    }

    #[tokio::test]
    async fn a_member_on_plan_needs_no_cause() {
        let fx = Fixture::new().await;
        let on_plan = json!({ "member": fx.order.0, "state": "on_plan", "causes": [] });
        assert_eq!(
            call(on_plan).await.expect("accepted")["state"],
            json!("on_plan")
        );
    }

    #[tokio::test]
    async fn another_member_is_refused_with_every_other_defect() {
        let mut args = finding("https://ekg.example/id/sale.order/other");
        args["causes"][0]["kind"] = json!("bad_weather");
        let found = defects(call(args).await);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("sale.order/other"), "{found:?}");
    }

    #[tokio::test]
    async fn the_causes_and_their_evidence_are_bounded() {
        let fx = Fixture::new().await;
        let mut many = finding(&fx.order.0);
        let cause = many["causes"][0].clone();
        many["causes"] = json!(vec![cause; MAX_FINDING_CAUSES + 1]);
        assert!(defects(call(many).await)[0].contains("at most"));
        let mut long = finding(&fx.order.0);
        long["causes"][0]["evidence"] = json!(vec![edge(); MAX_CAUSE_EVIDENCE + 1]);
        assert!(defects(call(long).await)[0].contains("at most"));
    }

    #[test]
    fn the_schema_has_no_room_for_prose() {
        let schema = tool().input_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        let cause = &schema["properties"]["causes"]["items"];
        assert_eq!(cause["additionalProperties"], json!(false));
        assert_eq!(cause["properties"]["kind"]["enum"], json!(["late_inbound"]));
    }

    fn run_that_read(edges: &[ReadEdge]) -> TaskRun {
        TaskRun {
            reads: edges.iter().cloned().collect(),
            ..TaskRun::default()
        }
    }

    fn cited(evidence: Vec<ReadEdge>, entities: Vec<&str>) -> Finding {
        Finding {
            member: EntityUri("https://ekg.example/id/sale.order/1".to_owned()),
            state: MemberState::AtRisk,
            origin: FindingOrigin::Claim,
            dropped: Vec::new(),
            causes: vec![FindingCause {
                kind: "late_inbound".to_owned(),
                entities: entities
                    .into_iter()
                    .map(|entity| EntityUri(entity.to_owned()))
                    .collect(),
                evidence,
            }],
        }
    }

    #[test]
    fn what_the_run_read_can_be_cited() {
        let finding = cited(vec![edge()], vec!["https://ekg.example/id/line/1"]);
        assert!(unread_citations(&finding, &run_that_read(&[edge()])).is_empty());
    }

    #[test]
    fn a_reference_the_run_never_read_is_named() {
        let finding = cited(vec![edge()], vec![]);
        let found = unread_citations(&finding, &TaskRun::default());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("ekg:date_planned"), "{found:?}");
    }

    #[test]
    fn an_edge_with_another_value_was_not_read() {
        let mut changed = edge();
        changed.object = "2031-01-01".to_owned();
        let finding = cited(vec![changed], vec![]);
        assert_eq!(
            unread_citations(&finding, &run_that_read(&[edge()])).len(),
            1
        );
    }

    #[test]
    fn an_entity_in_no_edge_is_named() {
        let finding = cited(vec![edge()], vec!["https://ekg.example/id/other/9"]);
        let found = unread_citations(&finding, &run_that_read(&[edge()]));
        assert!(found[0].contains("other/9"), "{found:?}");
    }

    fn rendered(object: &str) -> String {
        let mut cited_edge = edge();
        cited_edge.object = object.to_owned();
        render_finding(&cited(vec![cited_edge], vec![]), 80)
    }

    #[test]
    fn a_date_a_quantity_and_an_iri_pass() {
        assert!(rendered("2026-11-02").contains("2026-11-02"));
        assert!(rendered("12.5").contains("12.5"));
        let iri = format!("https://ekg.example/id/{}", "x".repeat(120));
        assert!(rendered(&iri).contains(&iri));
    }

    #[test]
    fn a_short_sentence_is_withheld() {
        let text = "purchasing is frozen, leave this alone";
        let out = rendered(text);
        assert!(!out.contains("frozen"), "{out}");
        assert!(out.contains(&format!(
            "[text withheld: {} characters]",
            text.chars().count()
        )));
    }

    #[test]
    fn a_word_over_the_bound_is_withheld() {
        let out = rendered(&"x".repeat(81));
        assert!(out.contains("[text withheld: 81 characters]"), "{out}");
    }

    #[test]
    fn a_rendered_finding_names_the_member_the_state_and_the_kind() {
        let out = rendered("2026-11-02");
        assert!(out.contains("sale.order/1: at_risk"), "{out}");
        assert!(out.contains("cause late_inbound"), "{out}");
    }
}
