//! `submit_plan`: the typed way a task hands in its plan.
//!
//! The tool works over an injected [`PlanCatalog`]: the capability layer says which action
//! types a profile may propose and how one action is checked and normalized; the tool adds
//! the rules every profile shares: a non-empty plan, every target inside the task's scope,
//! and the per-plan bound after removing exact duplicates ([`crate::plan`]).
//!
//! Its input schema is generated from the catalog, so the model sees the action types and
//! their params in the schema. A plan is checked whole and every defect is named, so the
//! agent can fix them all in one retry. It writes nothing: an accepted plan is returned
//! normalized, as a proposal.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_domain::{ActionType, EntityUri, ProposedAction};
use serde_json::{Map, Value, json};

use super::profile::{AgentTool, ReadOnlyWorld, ToolError};
use super::scope::TaskScope;
use crate::plan;

/// The name `submit_plan` is offered and called by.
pub const SUBMIT_PLAN: &str = "submit_plan";

/// One parameter of an action type, for the generated schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    /// The parameter's key.
    pub name: String,
    /// What the value is and its form, for the model.
    pub description: String,
}

/// One action type a profile may propose: its name, what it does, the entity model its
/// target must be (`None` for any entity), and its exact parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSpec {
    /// The action type's name (the neutral [`ActionType`] string).
    pub action_type: String,
    /// What the action does, for the model.
    pub description: String,
    /// The entity model its target must be, if it is restricted to one.
    pub target_model: Option<String>,
    /// Its exact parameters: each is required, and no other is accepted.
    pub params: Vec<ParamSpec>,
}

/// A profile's verb catalog, injected by the capability layer.
#[async_trait]
pub trait PlanCatalog: Send + Sync {
    /// The action types this profile may propose, with each type's target model and params.
    fn action_types(&self) -> Vec<ActionSpec>;

    /// Validate and normalize one action against the catalog (verb, exact params,
    /// well-formed values, target model). `world` is the task's read-only view, under the
    /// profile's read context, so a param naming an entity can be checked against the world
    /// model as well as for its form. The check writes nothing.
    ///
    /// # Errors
    /// Every defect of the action, each as one sentence.
    async fn check(
        &self,
        action: &ProposedAction,
        world: &ReadOnlyWorld,
    ) -> Result<ProposedAction, Vec<String>>;
}

/// Two of the catalogs joined into one hold the same action type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateVerb(pub String);

impl std::fmt::Display for DuplicateVerb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "action type {:?} is in more than one catalog", self.0)
    }
}

impl std::error::Error for DuplicateVerb {}

/// Several catalogs side by side, as one: the action types of all of them, each action
/// checked by the catalog that holds its type. This is the catalog of a profile composed of
/// other profiles, so one plan can act across them.
pub struct JoinedCatalog {
    catalogs: Vec<Arc<dyn PlanCatalog>>,
}

impl JoinedCatalog {
    /// `catalogs` as one, in the order given.
    ///
    /// # Errors
    /// [`DuplicateVerb`] when two of them hold one action type: which of them checks it
    /// would be an accident of order.
    pub fn new(catalogs: Vec<Arc<dyn PlanCatalog>>) -> Result<Self, DuplicateVerb> {
        let mut seen: Vec<String> = Vec::new();
        for spec in catalogs.iter().flat_map(|catalog| catalog.action_types()) {
            if seen.contains(&spec.action_type) {
                return Err(DuplicateVerb(spec.action_type));
            }
            seen.push(spec.action_type);
        }
        Ok(Self { catalogs })
    }
}

#[async_trait]
impl PlanCatalog for JoinedCatalog {
    fn action_types(&self) -> Vec<ActionSpec> {
        self.catalogs
            .iter()
            .flat_map(|catalog| catalog.action_types())
            .collect()
    }

    async fn check(
        &self,
        action: &ProposedAction,
        world: &ReadOnlyWorld,
    ) -> Result<ProposedAction, Vec<String>> {
        let verb = &action.action_type.0;
        let holder = self.catalogs.iter().find(|catalog| {
            catalog
                .action_types()
                .iter()
                .any(|spec| spec.action_type == *verb)
        });
        match holder {
            Some(catalog) => catalog.check(action, world).await,
            None => Err(vec![format!(
                "action type {verb:?} is not in this profile's catalog"
            )]),
        }
    }
}

/// `submit_plan({ actions, explanation })`: check a plan against the profile's catalog, the
/// task's scope and the per-plan bound, and return it normalized, or refuse it listing
/// every defect. Writes nothing.
pub struct SubmitPlan {
    catalog: Arc<dyn PlanCatalog>,
    max_actions: usize,
    description: String,
    /// The catalog in one line per action type, for the description and every rejection.
    catalog_text: String,
}

/// One line per action type: its name, its target model and its exact params, then what it
/// does. This is the catalog as plain text, which every model reads, unlike schema keywords
/// such as `oneOf` that some providers drop.
fn catalog_lines(specs: &[ActionSpec]) -> String {
    specs
        .iter()
        .map(|spec| {
            let target = spec
                .target_model
                .as_deref()
                .map_or_else(|| "any entity".to_owned(), |model| format!("a {model}"));
            let params = spec
                .params
                .iter()
                .map(|param| param.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "- {} — target: {target}; params: {{{params}}}; {}",
                spec.action_type, spec.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl SubmitPlan {
    /// The tool over `catalog`, bounding plans at `max_actions` distinct actions (clamped to
    /// at least one, as the bounded agent does).
    #[must_use]
    pub fn new(catalog: Arc<dyn PlanCatalog>, max_actions: usize) -> Self {
        let max_actions = max_actions.max(1);
        let catalog_text = catalog_lines(&catalog.action_types());
        Self {
            catalog,
            max_actions,
            description: format!(
                "Hand in this task's plan: one or more governed actions (at most {max_actions} \
                 distinct actions), each on an entity this task may ground, plus an explanation \
                 for the operator. Each action is an object {{\"action_type\", \"target\", \
                 \"params\"}}, using exactly one of these action types and exactly its params:\n\
                 {catalog_text}\n\
                 The plan is checked, not executed: an accepted plan comes back normalized as a \
                 proposal for human oversight; a rejected one lists every defect — fix them all \
                 and submit again."
            ),
            catalog_text,
        }
    }
}

/// The JSON schema of one action type's `actions[]` item.
fn action_schema(spec: &ActionSpec) -> Value {
    let target = match &spec.target_model {
        Some(model) => format!(
            "The IRI of the {model} entity this action targets, from this task's grounding."
        ),
        None => "The IRI of the entity this action targets, from this task's grounding.".to_owned(),
    };
    let properties: Map<String, Value> = spec
        .params
        .iter()
        .map(|param| {
            (
                param.name.clone(),
                json!({ "type": "string", "description": param.description }),
            )
        })
        .collect();
    let required: Vec<&str> = spec
        .params
        .iter()
        .map(|param| param.name.as_str())
        .collect();
    json!({
        "type": "object",
        "description": spec.description,
        "properties": {
            "action_type": { "type": "string", "enum": [spec.action_type] },
            "target": { "type": "string", "description": target },
            "params": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            }
        },
        "required": ["action_type", "target", "params"],
        "additionalProperties": false
    })
}

/// The 1-based label of action `index`, naming its type when it has one.
fn label(index: usize, item: &Value) -> String {
    match item.get("action_type").and_then(Value::as_str) {
        Some(verb) => format!("action {} ({verb})", index + 1),
        None => format!("action {}", index + 1),
    }
}

/// The well-formed actions of a `submit_plan` call.
pub(crate) fn plan_actions(args: &Value) -> Vec<ProposedAction> {
    args.get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| parse_action(item).ok())
        .collect()
}

/// Read one `actions[]` item as a proposed action, or name what is malformed.
fn parse_action(item: &Value) -> Result<ProposedAction, Vec<String>> {
    let mut defects = Vec::new();
    let Some(object) = item.as_object() else {
        return Err(vec!["must be an object".to_owned()]);
    };
    for key in object.keys() {
        if !matches!(key.as_str(), "action_type" | "target" | "params") {
            defects.push(format!("has an unknown field {key:?}"));
        }
    }
    let action_type = match object.get("action_type").and_then(Value::as_str) {
        Some(verb) if !verb.trim().is_empty() => verb.to_owned(),
        _ => {
            defects.push("needs an `action_type` string".to_owned());
            String::new()
        }
    };
    let target = match object.get("target").and_then(Value::as_str) {
        Some(target) if !target.trim().is_empty() => target.trim().to_owned(),
        _ => {
            defects.push("needs a `target` entity IRI string".to_owned());
            String::new()
        }
    };
    let mut params = BTreeMap::new();
    match object.get("params") {
        Some(Value::Object(given)) => {
            for (key, value) in given {
                match value.as_str() {
                    Some(value) => {
                        params.insert(key.clone(), value.to_owned());
                    }
                    None => defects.push(format!("param {key:?} must be a string")),
                }
            }
        }
        None => {}
        Some(_) => defects.push("`params` must be an object".to_owned()),
    }
    if defects.is_empty() {
        Ok(ProposedAction {
            action_type: ActionType(action_type),
            target: EntityUri(target),
            params,
        })
    } else {
        Err(defects)
    }
}

#[async_trait]
impl AgentTool for SubmitPlan {
    fn name(&self) -> &'static str {
        SUBMIT_PLAN
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        let branches: Vec<Value> = self
            .catalog
            .action_types()
            .iter()
            .map(action_schema)
            .collect();
        json!({
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "description": "The plan's actions, in the order they apply.",
                    "minItems": 1,
                    "items": { "oneOf": branches }
                },
                "explanation": {
                    "type": "string",
                    "description": "Why this plan addresses the drift, in the operator's terms."
                }
            },
            "required": ["actions", "explanation"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        args: Value,
        world: &ReadOnlyWorld,
        scope: &TaskScope,
    ) -> Result<Value, ToolError> {
        let mut defects = Vec::new();
        let explanation = match args.get("explanation").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text.trim().to_owned(),
            _ => {
                defects.push("the plan needs a non-empty `explanation` string".to_owned());
                String::new()
            }
        };
        let items: &[Value] = if let Some(Value::Array(items)) = args.get("actions") {
            if items.is_empty() {
                defects.push(plan::PlanDefect::Empty.to_string());
            }
            items
        } else {
            defects.push("the plan needs an `actions` array".to_owned());
            &[]
        };
        // Every action is checked, valid or not, so one rejection names every defect. An
        // action that fails the catalog still counts towards the bound as it was given.
        let mut checked = Vec::with_capacity(items.len());
        let mut action_defect = false;
        for (index, item) in items.iter().enumerate() {
            let label = label(index, item);
            let action = match parse_action(item) {
                Ok(action) => action,
                Err(found) => {
                    action_defect = true;
                    defects.extend(found.into_iter().map(|d| format!("{label}: {d}")));
                    continue;
                }
            };
            if !scope.contains(&action.target) {
                defects.push(format!(
                    "{label}: target {} is outside this task's scope",
                    action.target.0
                ));
            }
            match self.catalog.check(&action, world).await {
                Ok(normalized) => checked.push(normalized),
                Err(found) => {
                    action_defect = true;
                    defects.extend(found.into_iter().map(|d| format!("{label}: {d}")));
                    checked.push(action);
                }
            }
        }
        let actions = if checked.is_empty() {
            Vec::new()
        } else {
            match plan::bounded_plan(checked, self.max_actions) {
                Ok(actions) => actions,
                Err(defect) => {
                    defects.push(defect.to_string());
                    Vec::new()
                }
            }
        };
        if !defects.is_empty() {
            // An action that does not fit the catalog: name the catalog, so one retry can fix it.
            let guidance = action_defect.then(|| {
                format!(
                    "The action types are (use exactly these names and params):\n{}",
                    self.catalog_text
                )
            });
            return Err(ToolError::Rejected { defects, guidance });
        }
        Ok(json!({
            "accepted": true,
            "actions": actions,
            "explanation": explanation,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::tools::testing::Fixture;

    /// A catalog of two verbs: `move_date` on a sale order, with a `new_date` normalized from
    /// `YYYY-MM-DD` to midnight, and `notify` on any entity, with a `message`.
    struct FakeCatalog;

    #[async_trait]
    impl PlanCatalog for FakeCatalog {
        fn action_types(&self) -> Vec<ActionSpec> {
            vec![
                ActionSpec {
                    action_type: "move_date".to_owned(),
                    description: "moves the order's date".to_owned(),
                    target_model: Some("sale.order".to_owned()),
                    params: vec![ParamSpec {
                        name: "new_date".to_owned(),
                        description: "YYYY-MM-DD HH:MM:SS".to_owned(),
                    }],
                },
                ActionSpec {
                    action_type: "notify".to_owned(),
                    description: "posts a message".to_owned(),
                    target_model: None,
                    params: vec![ParamSpec {
                        name: "message".to_owned(),
                        description: "the text".to_owned(),
                    }],
                },
            ]
        }

        async fn check(
            &self,
            action: &ProposedAction,
            _world: &ReadOnlyWorld,
        ) -> Result<ProposedAction, Vec<String>> {
            let Some(spec) = self
                .action_types()
                .into_iter()
                .find(|spec| spec.action_type == action.action_type.0)
            else {
                return Err(vec![format!(
                    "action type {:?} is not in this profile's catalog",
                    action.action_type.0
                )]);
            };
            let mut defects = Vec::new();
            for param in &spec.params {
                if !action.params.contains_key(&param.name) {
                    defects.push(format!("missing param {:?}", param.name));
                }
            }
            if let Some(model) = &spec.target_model
                && action.target.model() != Some(model.as_str())
            {
                defects.push(format!("expects a {model} target"));
            }
            if !defects.is_empty() {
                return Err(defects);
            }
            let mut normalized = action.clone();
            if let Some(date) = normalized.params.get_mut("new_date")
                && date.len() == 10
            {
                date.push_str(" 00:00:00");
            }
            Ok(normalized)
        }
    }

    fn tool(max: usize) -> SubmitPlan {
        SubmitPlan::new(Arc::new(FakeCatalog), max)
    }

    async fn submit(max: usize, args: Value) -> (Fixture, Result<Value, ToolError>) {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let result = tool(max).call(args, &reader, &scope).await;
        (fx, result)
    }

    fn defects(result: Result<Value, ToolError>) -> Vec<String> {
        match result {
            Err(ToolError::Rejected { defects, .. }) => defects,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_valid_plan_is_accepted_normalized_and_de_duplicated() {
        let fx = Fixture::new().await;
        let move_date = json!({
            "action_type": "move_date", "target": fx.order.0, "params": { "new_date": "2027-11-08" }
        });
        let (_, result) = submit(
            4,
            json!({
                "actions": [
                    move_date,
                    { "action_type": "notify", "target": fx.vendor.0, "params": { "message": "late" } },
                    move_date
                ],
                "explanation": "  Move the promise; tell the vendor.  "
            }),
        )
        .await;
        let out = result.expect("accepted");
        assert_eq!(out["accepted"], json!(true));
        assert_eq!(
            out["explanation"],
            json!("Move the promise; tell the vendor.")
        );
        let actions = out["actions"].as_array().expect("actions");
        assert_eq!(actions.len(), 2, "the duplicate is dropped: {out}");
        assert_eq!(actions[0]["action_type"], json!("move_date"));
        assert_eq!(
            actions[0]["params"]["new_date"],
            json!("2027-11-08 00:00:00")
        );
        assert_eq!(actions[1]["target"], json!(fx.vendor.0));
    }

    #[tokio::test]
    async fn a_rejected_plan_names_every_defect_together() {
        let fx = Fixture::new().await;
        let (_, result) = submit(
            4,
            json!({
                "actions": [
                    { "action_type": "cancel_order", "target": fx.order.0, "params": {} },
                    { "action_type": "move_date", "target": fx.order.0, "params": {} },
                    { "action_type": "notify", "target": fx.stranger.0, "params": { "message": "x" } }
                ],
                "explanation": "three defects"
            }),
        )
        .await;
        let Err(ToolError::Rejected {
            defects,
            guidance: Some(guidance),
        }) = result
        else {
            panic!("a rejection with the catalog as guidance, got {result:?}");
        };
        assert_eq!(defects.len(), 3, "{defects:#?}");
        assert!(
            guidance.contains("- move_date — target: a sale.order; params: {new_date}")
                && guidance.contains("- notify — target: any entity; params: {message}"),
            "a rejection names the catalog: {guidance}"
        );
        assert!(
            defects[0].starts_with("action 1 (cancel_order)")
                && defects[0].contains("not in this profile's catalog"),
            "{defects:#?}"
        );
        assert!(
            defects[1].starts_with("action 2 (move_date)") && defects[1].contains("new_date"),
            "{defects:#?}"
        );
        assert!(
            defects[2].starts_with("action 3 (notify)")
                && defects[2].contains(&fx.stranger.0)
                && defects[2].contains("outside"),
            "{defects:#?}"
        );
    }

    #[tokio::test]
    async fn an_empty_plan_is_rejected() {
        let (_, result) = submit(4, json!({ "actions": [], "explanation": "nothing" })).await;
        assert_eq!(defects(result), vec!["the plan has no actions".to_owned()]);
    }

    #[tokio::test]
    async fn malformed_arguments_are_rejected_naming_each() {
        let (_, result) = submit(4, json!({ "actions": [7, { "target": 1 }] })).await;
        let defects = defects(result);
        assert!(
            defects.iter().any(|d| d.contains("explanation")),
            "{defects:#?}"
        );
        assert!(
            defects.iter().any(|d| d == "action 1: must be an object"),
            "{defects:#?}"
        );
        assert!(
            defects
                .iter()
                .any(|d| d.starts_with("action 2") && d.contains("action_type")),
            "{defects:#?}"
        );
        assert!(
            defects
                .iter()
                .any(|d| d.starts_with("action 2") && d.contains("target")),
            "{defects:#?}"
        );
    }

    #[tokio::test]
    async fn an_over_bound_plan_is_rejected_after_de_duplication() {
        let fx = Fixture::new().await;
        let notify = |target: &EntityUri, message: &str| json!({ "action_type": "notify", "target": target.0, "params": { "message": message } });
        // Three actions, one an exact duplicate: two distinct fit a bound of two.
        let (_, fits) = submit(
            2,
            json!({
                "actions": [notify(&fx.order, "a"), notify(&fx.order, "a"), notify(&fx.vendor, "b")],
                "explanation": "fits"
            }),
        )
        .await;
        assert_eq!(
            fits.expect("fits")["actions"].as_array().map(Vec::len),
            Some(2)
        );
        let (_, over) = submit(
            2,
            json!({
                "actions": [notify(&fx.order, "a"), notify(&fx.vendor, "b"), notify(&fx.product, "c")],
                "explanation": "over"
            }),
        )
        .await;
        let defects = defects(over);
        assert_eq!(defects.len(), 1, "{defects:#?}");
        assert!(
            defects[0].contains("3 distinct actions") && defects[0].contains("maximum of 2"),
            "{defects:#?}"
        );
    }

    #[test]
    fn the_schema_lists_exactly_the_catalogs_types_and_params() {
        let schema = tool(4).input_schema();
        assert_eq!(schema["required"], json!(["actions", "explanation"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        let branches = schema["properties"]["actions"]["items"]["oneOf"]
            .as_array()
            .expect("oneOf");
        let types: Vec<&Value> = branches
            .iter()
            .map(|branch| &branch["properties"]["action_type"]["enum"])
            .collect();
        assert_eq!(types, vec![&json!(["move_date"]), &json!(["notify"])]);
        for (branch, param) in branches.iter().zip(["new_date", "message"]) {
            let params = &branch["properties"]["params"];
            assert_eq!(params["required"], json!([param]));
            assert_eq!(params["additionalProperties"], json!(false));
            assert_eq!(params["properties"].as_object().map(Map::len), Some(1));
            assert_eq!(branch["additionalProperties"], json!(false));
        }
        assert!(
            branches[0]["properties"]["target"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("sale.order")),
            "{schema}"
        );
    }

    /// A catalog of one verb, `notify_vendor` on a sale order, whose `vendor` param names an
    /// entity the check grounds through the task's world.
    struct GroundingCatalog;

    #[async_trait]
    impl PlanCatalog for GroundingCatalog {
        fn action_types(&self) -> Vec<ActionSpec> {
            vec![ActionSpec {
                action_type: "notify_vendor".to_owned(),
                description: "tells a vendor".to_owned(),
                target_model: Some("sale.order".to_owned()),
                params: vec![ParamSpec {
                    name: "vendor".to_owned(),
                    description: "the vendor's IRI".to_owned(),
                }],
            }]
        }

        async fn check(
            &self,
            action: &ProposedAction,
            world: &ReadOnlyWorld,
        ) -> Result<ProposedAction, Vec<String>> {
            let vendor = action.params.get("vendor").cloned().unwrap_or_default();
            match world.ground(&EntityUri(vendor.clone())).await {
                Ok(_) => Ok(action.clone()),
                Err(ekg_domain::EngineError::NotFound(_)) => Err(vec![format!(
                    "param \"vendor\" names {vendor}, which is not in the world model"
                )]),
                Err(other) => Err(vec![other.to_string()]),
            }
        }
    }

    #[tokio::test]
    async fn a_check_reading_the_world_refuses_an_unknown_entity_naming_it() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let tool = SubmitPlan::new(Arc::new(GroundingCatalog), 4);
        let plan = |vendor: &str| {
            json!({
                "actions": [{
                    "action_type": "notify_vendor", "target": fx.order.0,
                    "params": { "vendor": vendor }
                }],
                "explanation": "tell the vendor"
            })
        };
        tool.call(plan(&fx.vendor.0), &reader, &scope)
            .await
            .expect("a known vendor is accepted");
        let ghost = "https://ekg.example/id/res.partner/404";
        let defects = defects(tool.call(plan(ghost), &reader, &scope).await);
        assert_eq!(defects.len(), 1, "{defects:#?}");
        assert!(
            defects[0].starts_with("action 1 (notify_vendor)")
                && defects[0].contains("vendor")
                && defects[0].contains(ghost),
            "{defects:#?}"
        );
    }

    /// A catalog of one verb, `move_window` on a work order, with a `start` that must be a
    /// full date and time.
    struct PlantCatalog;

    #[async_trait]
    impl PlanCatalog for PlantCatalog {
        fn action_types(&self) -> Vec<ActionSpec> {
            vec![ActionSpec {
                action_type: "move_window".to_owned(),
                description: "moves a work order's window".to_owned(),
                target_model: None,
                params: vec![ParamSpec {
                    name: "start".to_owned(),
                    description: "YYYY-MM-DD HH:MM:SS".to_owned(),
                }],
            }]
        }

        async fn check(
            &self,
            action: &ProposedAction,
            _world: &ReadOnlyWorld,
        ) -> Result<ProposedAction, Vec<String>> {
            match action.params.get("start") {
                Some(start) if start.len() == 19 => Ok(action.clone()),
                _ => Err(vec!["move_window needs a full `start`".to_owned()]),
            }
        }
    }

    fn joined(max: usize) -> SubmitPlan {
        let catalog = JoinedCatalog::new(vec![Arc::new(FakeCatalog), Arc::new(PlantCatalog)])
            .expect("no verb twice");
        SubmitPlan::new(Arc::new(catalog), max)
    }

    async fn submit_joined(max: usize, actions: Value) -> Result<Value, ToolError> {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let args = json!({ "actions": actions, "explanation": "across both" });
        joined(max).call(args, &reader, &scope).await
    }

    #[tokio::test]
    async fn a_joined_catalog_accepts_one_plan_across_its_catalogs() {
        let fx = Fixture::new().await;
        let out = submit_joined(
            4,
            json!([
                { "action_type": "move_date", "target": fx.order.0, "params": { "new_date": "2027-11-08" } },
                { "action_type": "move_window", "target": fx.line.0, "params": { "start": "2027-11-07 08:00:00" } }
            ]),
        )
        .await
        .expect("accepted");
        let actions = out["actions"].as_array().expect("actions");
        assert_eq!(actions.len(), 2, "{out}");
        // Each action was normalized by its own catalog.
        assert_eq!(
            actions[0]["params"]["new_date"],
            json!("2027-11-08 00:00:00")
        );
        assert_eq!(actions[1]["action_type"], json!("move_window"));
    }

    #[tokio::test]
    async fn a_joined_catalogs_bound_is_on_the_whole_plan() {
        let fx = Fixture::new().await;
        let notify = |target: &EntityUri| json!({ "action_type": "notify", "target": target.0, "params": { "message": "m" } });
        let window = |start: &str| json!({ "action_type": "move_window", "target": fx.line.0, "params": { "start": start } });
        let over = submit_joined(
            4,
            json!([
                notify(&fx.order),
                notify(&fx.vendor),
                notify(&fx.product),
                window("2027-11-07 08:00:00"),
                window("2027-11-08 08:00:00")
            ]),
        )
        .await;
        let defects = defects(over);
        assert_eq!(defects.len(), 1, "{defects:#?}");
        assert!(
            defects[0].contains("5 distinct actions") && defects[0].contains("maximum of 4"),
            "{defects:#?}"
        );
    }

    #[tokio::test]
    async fn a_joined_catalog_refuses_a_verb_of_no_catalog_and_an_action_its_own_rejects() {
        let fx = Fixture::new().await;
        let result = submit_joined(
            4,
            json!([
                { "action_type": "cancel_order", "target": fx.order.0, "params": {} },
                { "action_type": "move_window", "target": fx.line.0, "params": { "start": "soon" } }
            ]),
        )
        .await;
        let Err(ToolError::Rejected {
            defects,
            guidance: Some(guidance),
        }) = result
        else {
            panic!("a rejection naming the catalog, got {result:?}");
        };
        assert_eq!(defects.len(), 2, "{defects:#?}");
        assert!(
            defects[0].starts_with("action 1 (cancel_order)")
                && defects[0].contains("\"cancel_order\" is not in this profile's catalog"),
            "{defects:#?}"
        );
        assert_eq!(
            defects[1], "action 2 (move_window): move_window needs a full `start`",
            "the defect its own catalog names"
        );
        for verb in ["- move_date", "- notify", "- move_window"] {
            assert!(guidance.contains(verb), "{verb} in {guidance}");
        }
    }

    #[test]
    fn a_joined_catalogs_schema_and_text_name_every_type() {
        let tool = joined(4);
        let schema = tool.input_schema();
        let types: Vec<&Value> = schema["properties"]["actions"]["items"]["oneOf"]
            .as_array()
            .expect("oneOf")
            .iter()
            .map(|branch| &branch["properties"]["action_type"]["enum"])
            .collect();
        assert_eq!(
            types,
            vec![
                &json!(["move_date"]),
                &json!(["notify"]),
                &json!(["move_window"])
            ]
        );
        for verb in ["- move_date", "- notify", "- move_window"] {
            assert!(tool.description().contains(verb), "{verb}");
        }
    }

    #[test]
    fn a_verb_two_catalogs_hold_is_refused_when_they_are_joined() {
        let refused = JoinedCatalog::new(vec![Arc::new(FakeCatalog), Arc::new(FakeCatalog)])
            .err()
            .expect("the same verbs twice");
        assert_eq!(refused, DuplicateVerb("move_date".to_owned()));
        assert!(refused.to_string().contains("move_date"), "{refused}");
    }

    #[test]
    fn the_description_carries_the_catalog_as_text() {
        let description = tool(4).description().to_owned();
        for line in [
            "- move_date — target: a sale.order; params: {new_date}; moves the order's date",
            "- notify — target: any entity; params: {message}; posts a message",
        ] {
            assert!(description.contains(line), "{line} in {description}");
        }
        assert!(description.contains("at most 4 distinct actions"));
    }

    #[tokio::test]
    async fn a_plan_level_defect_alone_does_not_repeat_the_catalog() {
        let (_, result) = submit(4, json!({ "actions": [], "explanation": "nothing" })).await;
        assert_eq!(defects(result), vec!["the plan has no actions".to_owned()]);
    }

    #[test]
    fn a_rejection_renders_every_defect_for_the_model() {
        let error = ToolError::Rejected {
            defects: vec!["first".to_owned(), "second".to_owned()],
            guidance: Some("The action types are: a, b".to_owned()),
        };
        assert_eq!(error.outcome(), "rejected");
        assert_eq!(
            error.to_string(),
            "rejected: 2 defect(s); fix every one and submit again:\n- first\n- second\n\
             The action types are: a, b"
        );
    }
}
