//! The neutral tools every profile offers: `ground_entity` and `related_entities`.
//!
//! Both check the task's scope before reading, so a refused call does not touch the
//! world model, and both return the domain types' own JSON (snake_case).

use async_trait::async_trait;
use ekg_domain::EntityUri;
use serde_json::{Value, json};

use super::profile::{AgentTool, ReadOnlyWorld, ToolError};
use super::scope::TaskScope;

/// `ground_entity({ entity })`: the read-policy-filtered grounding of an in-scope entity:
/// its visible evidence edges with their source records and revisions, and the omissions
/// the read policy imposed.
pub struct GroundEntity;

/// `related_entities({ subject })`: the entities the profile's hook relates to one of the
/// task's subjects.
pub struct RelatedEntities;

/// The string argument `key` of a call, or an invalid-arguments error naming it.
fn entity_arg(args: &Value, key: &str) -> Result<EntityUri, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| EntityUri(value.to_owned()))
        .ok_or_else(|| ToolError::InvalidArguments(format!("`{key}` must be an entity IRI string")))
}

/// An object schema with one required entity-IRI string property.
fn one_entity_schema(key: &str, description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            key: { "type": "string", "description": description }
        },
        "required": [key],
        "additionalProperties": false
    })
}

#[async_trait]
impl AgentTool for GroundEntity {
    fn name(&self) -> &'static str {
        "ground_entity"
    }

    fn description(&self) -> &'static str {
        "Return the evidence the read policy lets this task see about one entity: its edges \
         (subject, predicate, object), each with the source record and revision it was read \
         at, and the omissions the policy imposed. Only the task's subjects, their related \
         entities and the entities their evidence names can be grounded."
    }

    fn input_schema(&self) -> Value {
        one_entity_schema("entity", "The entity IRI to ground.")
    }

    fn entity_of(&self, args: &Value) -> Option<String> {
        entity_arg(args, "entity").ok().map(|entity| entity.0)
    }

    async fn call(
        &self,
        args: Value,
        world: &ReadOnlyWorld,
        scope: &TaskScope,
    ) -> Result<Value, ToolError> {
        let entity = entity_arg(&args, "entity")?;
        if !scope.contains(&entity) {
            return Err(ToolError::Refused(format!(
                "{} is outside this task's scope",
                entity.0
            )));
        }
        let grounding = world.ground(&entity).await?;
        serde_json::to_value(grounding).map_err(|e| ToolError::Failed(e.to_string()))
    }
}

#[async_trait]
impl AgentTool for RelatedEntities {
    fn name(&self) -> &'static str {
        "related_entities"
    }

    fn description(&self) -> &'static str {
        "Return the entities this task's capability relates to one of its subjects (for \
         example what causes its drift, or its process context). Ground them with \
         ground_entity."
    }

    fn input_schema(&self) -> Value {
        one_entity_schema("subject", "One of the task's subject IRIs.")
    }

    fn entity_of(&self, args: &Value) -> Option<String> {
        entity_arg(args, "subject").ok().map(|entity| entity.0)
    }

    async fn call(
        &self,
        args: Value,
        world: &ReadOnlyWorld,
        scope: &TaskScope,
    ) -> Result<Value, ToolError> {
        let subject = entity_arg(&args, "subject")?;
        if !scope.is_subject(&subject) {
            return Err(ToolError::Refused(format!(
                "{} is not a subject of this task",
                subject.0
            )));
        }
        let related = world.related(&subject).await?;
        Ok(json!({ "subject": subject, "related": related }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::tools::testing::Fixture;

    #[tokio::test]
    async fn ground_entity_returns_the_grounding_json_with_provenance_and_omissions() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let out = GroundEntity
            .call(json!({ "entity": fx.order.0 }), &reader, &scope)
            .await
            .expect("grounding");
        assert_eq!(out["entity"], json!(fx.order.0));
        let edge = &out["edges"][0];
        assert_eq!(edge["source"]["system"], json!("odoo"));
        assert!(edge["revision"].is_u64(), "{edge}");
        assert_eq!(out["omissions"][0]["type_iri"], json!("ekg:Supplier"));
        assert_eq!(out["omissions"][0]["count"], json!(1));
    }

    #[tokio::test]
    async fn ground_entity_hands_on_no_literal_longer_than_the_readers_bound() {
        let fx = Fixture::new().await;
        let plain = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &plain)
            .await
            .expect("scope");
        let args = json!({ "entity": fx.line.0 });
        let full = GroundEntity
            .call(args.clone(), &plain, &scope)
            .await
            .expect("grounding");
        let bounded = GroundEntity
            .call(args, &fx.reader().with_text_bound(5), &scope)
            .await
            .expect("grounding");
        let objects = |out: &Value| -> Vec<String> {
            out["edges"]
                .as_array()
                .expect("edges")
                .iter()
                .map(|edge| edge["object"].as_str().expect("object").to_owned())
                .collect()
        };
        let (full, bounded) = (objects(&full), objects(&bounded));
        assert_eq!(full.len(), bounded.len(), "every edge stays");
        let mut withheld = 0;
        for (was, seen) in full.iter().zip(&bounded) {
            if was.starts_with("https://") || was.chars().count() <= 5 {
                assert_eq!(seen, was, "an IRI and a short literal pass");
            } else {
                assert_eq!(
                    seen,
                    &format!("[text withheld: {} characters]", was.chars().count())
                );
                withheld += 1;
            }
        }
        assert!(
            withheld > 0,
            "the line's date is longer than five: {full:?}"
        );
    }

    #[tokio::test]
    async fn an_evidenced_entity_can_be_grounded() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let out = GroundEntity
            .call(json!({ "entity": fx.product.0 }), &reader, &scope)
            .await
            .expect("grounding");
        assert_eq!(out["entity"], json!(fx.product.0));
    }

    #[tokio::test]
    async fn an_out_of_scope_entity_is_refused_without_reading_the_world() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let before = fx.counting.grounds();
        let err = GroundEntity
            .call(json!({ "entity": fx.stranger.0 }), &reader, &scope)
            .await
            .expect_err("refused");
        assert!(
            matches!(&err, ToolError::Refused(why) if why.contains(&fx.stranger.0)),
            "{err}"
        );
        assert_eq!(
            fx.counting.grounds(),
            before,
            "the refused call grounded nothing"
        );
    }

    #[tokio::test]
    async fn related_entities_returns_the_hooks_entities_for_a_subject() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let out = RelatedEntities
            .call(json!({ "subject": fx.order.0 }), &reader, &scope)
            .await
            .expect("related");
        assert_eq!(
            out,
            json!({ "subject": fx.order.0, "related": [fx.line.0] })
        );
    }

    #[tokio::test]
    async fn related_entities_of_a_non_subject_is_refused_without_reading() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        let (grounds, relates) = (fx.counting.grounds(), fx.hook_calls());
        // In scope (related), but not a subject.
        let err = RelatedEntities
            .call(json!({ "subject": fx.line.0 }), &reader, &scope)
            .await
            .expect_err("refused");
        assert!(
            matches!(&err, ToolError::Refused(why) if why.contains(&fx.line.0)),
            "{err}"
        );
        assert_eq!(fx.counting.grounds(), grounds);
        assert_eq!(fx.hook_calls(), relates, "the hook was not asked");
    }

    #[tokio::test]
    async fn missing_or_mistyped_arguments_are_invalid() {
        let fx = Fixture::new().await;
        let reader = fx.reader();
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &reader)
            .await
            .expect("scope");
        for args in [json!({}), json!({ "entity": 7 }), json!({ "entity": "" })] {
            let err = GroundEntity
                .call(args, &reader, &scope)
                .await
                .expect_err("invalid");
            assert!(matches!(err, ToolError::InvalidArguments(_)), "{err}");
        }
    }

    #[test]
    fn schemas_are_object_schemas_requiring_their_argument() {
        for (tool, key) in [
            (&GroundEntity as &dyn AgentTool, "entity"),
            (&RelatedEntities as &dyn AgentTool, "subject"),
        ] {
            let schema = tool.input_schema();
            assert_eq!(schema["type"], json!("object"));
            assert_eq!(schema["required"], json!([key]));
            assert_eq!(schema["properties"][key]["type"], json!("string"));
        }
    }
}
