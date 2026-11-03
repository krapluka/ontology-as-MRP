//! The evidence closure shared by the bounded agent and the tool gateway's scope.
//!
//! One definition of what a subject's grounding evidences, so the entities an agent may
//! target ([`crate::agent`]) and the entities a task may ground through the gateway
//! ([`crate::tools`]) cannot drift apart.

use ekg_domain::{
    EngineError, EntityUri, Grounding, ReadContext, Result, WorldModel, entity_model,
};

use crate::agent::{GroundedSubject, GroundingExtension};

/// Ground `subject` under `read_context`: its read-policy-filtered grounding (hidden facts
/// surface as omissions), extended by the capability's hook with each related entity
/// grounded under the same read context and merged in. A related entity that is missing
/// from the world model is skipped.
///
/// # Errors
/// [`EngineError::NotFound`] if the subject is unknown; a world-model read failure.
pub(crate) async fn ground_extended(
    world: &dyn WorldModel,
    read_context: &ReadContext,
    extension: Option<&dyn GroundingExtension>,
    subject: &EntityUri,
) -> Result<GroundedSubject> {
    let mut grounding = world.ground(subject, read_context).await?;
    // Related entities go through the same read context, so the same filter and provenance
    // apply to them.
    let mut extended = Vec::new();
    if let Some(extension) = extension {
        for entity in extension.related(subject).await? {
            if entity == *subject || extended.contains(&entity) {
                continue;
            }
            let related = match world.ground(&entity, read_context).await {
                Ok(related) => related,
                Err(EngineError::NotFound(_)) => continue,
                Err(other) => return Err(other),
            };
            merge_grounding(&mut grounding, related);
            extended.push(entity);
        }
    }
    Ok(GroundedSubject {
        grounding,
        extended,
    })
}

/// The entities a grounding evidences: the ones the hook grounded (the agent saw their
/// evidence), then the entity-IRI objects of its edges. Literal objects (dates,
/// quantities) are excluded, so a literal can never become an admissible target.
pub(crate) fn evidenced_members(extended: Vec<EntityUri>, grounding: &Grounding) -> Vec<EntityUri> {
    let mut members: Vec<EntityUri> = Vec::new();
    let entities = extended.into_iter().chain(
        grounding
            .edges
            .iter()
            .filter(|edge| entity_model(&edge.object).is_some())
            .map(|edge| EntityUri(edge.object.clone())),
    );
    for entity in entities {
        if !members.contains(&entity) {
            members.push(entity);
        }
    }
    members
}

/// Merge a related entity's grounding into the subject's: its edges are appended (an
/// identical edge once), and its omissions are summed per hidden type and policy, so the
/// disclosure stays one entry per type.
pub(crate) fn merge_grounding(into: &mut Grounding, related: Grounding) {
    for edge in related.edges {
        if !into.edges.contains(&edge) {
            into.edges.push(edge);
        }
    }
    for omission in related.omissions {
        if let Some(existing) = into.omissions.iter_mut().find(|o| {
            o.kind == omission.kind
                && o.type_iri == omission.type_iri
                && o.policy_id == omission.policy_id
        }) {
            existing.count += omission.count;
        } else {
            into.omissions.push(omission);
        }
    }
}
