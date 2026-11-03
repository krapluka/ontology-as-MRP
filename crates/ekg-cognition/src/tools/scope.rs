//! A task's scope: the entities its tools may ground.
//!
//! The scope is the task's subjects, the entities the profile's hook relates to them, and
//! the entity-IRI objects of the visible edges of those groundings (not literals). This is
//! the same closure the bounded agent admits as targets ([`crate::evidence`]). It is one
//! evidence hop, so tools cannot walk the graph. It is computed once per token and cached
//! until the token expires.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ekg_domain::{EngineError, EntityUri, Result};

use super::profile::ReadOnlyWorld;
use super::token::TaskClaims;
use crate::evidence::evidenced_members;

/// The default number of task scopes held at once.
pub const DEFAULT_SCOPE_CACHE_CAPACITY: usize = 1024;

/// The entities one task may ground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskScope {
    subjects: Vec<EntityUri>,
    entities: HashSet<EntityUri>,
}

impl TaskScope {
    /// Resolve the scope of `subjects` as `world` reads them: each subject, its related
    /// entities, and the entities their visible groundings evidence. A subject missing
    /// from the world model stays in scope (grounding it reports not-found) but evidences
    /// nothing.
    ///
    /// # Errors
    /// A world-model read failure other than not-found.
    pub async fn resolve(subjects: &[EntityUri], world: &ReadOnlyWorld) -> Result<Self> {
        let mut entities: HashSet<EntityUri> = subjects.iter().cloned().collect();
        for subject in subjects {
            match world.ground_extended(subject).await {
                Ok(grounded) => {
                    entities.extend(evidenced_members(grounded.extended, &grounded.grounding));
                }
                Err(EngineError::NotFound(_)) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(Self {
            subjects: subjects.to_vec(),
            entities,
        })
    }

    /// Whether `entity` is one of the task's subjects.
    #[must_use]
    pub fn is_subject(&self, entity: &EntityUri) -> bool {
        self.subjects.contains(entity)
    }

    /// Whether the task may ground `entity`.
    #[must_use]
    pub fn contains(&self, entity: &EntityUri) -> bool {
        self.entities.contains(entity)
    }

    /// The task's subjects.
    #[must_use]
    pub fn subjects(&self) -> &[EntityUri] {
        &self.subjects
    }

    /// Every entity the task may ground, in no particular order.
    pub fn entities(&self) -> impl Iterator<Item = &EntityUri> {
        self.entities.iter()
    }
}

/// Task scopes by token id, each held until its token expires. Bounded: an insert first
/// evicts expired entries, then (if still full) the entry expiring soonest.
pub struct ScopeCache {
    capacity: usize,
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    exp: u64,
    scope: Arc<TaskScope>,
}

impl Default for ScopeCache {
    fn default() -> Self {
        Self::new(DEFAULT_SCOPE_CACHE_CAPACITY)
    }
}

impl ScopeCache {
    /// A cache holding at most `capacity` scopes (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The scope of the task `claims` names, resolved through `world` on the token's first
    /// call and served from the cache after.
    ///
    /// # Errors
    /// A world-model read failure while resolving; a poisoned cache lock.
    pub async fn scope(
        &self,
        claims: &TaskClaims,
        world: &ReadOnlyWorld,
        now: u64,
    ) -> Result<Arc<TaskScope>> {
        if let Some(entry) = self.lock()?.get(&claims.jti) {
            return Ok(Arc::clone(&entry.scope));
        }
        let scope = Arc::new(TaskScope::resolve(&claims.subjects, world).await?);
        let mut entries = self.lock()?;
        // A concurrent first call may have resolved it meanwhile; keep the first.
        if let Some(entry) = entries.get(&claims.jti) {
            return Ok(Arc::clone(&entry.scope));
        }
        entries.retain(|_, entry| entry.exp > now);
        if entries.len() >= self.capacity {
            let soonest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.exp)
                .map(|(jti, _)| jti.clone());
            if let Some(soonest) = soonest {
                entries.remove(&soonest);
            }
        }
        entries.insert(
            claims.jti.clone(),
            Entry {
                exp: claims.exp,
                scope: Arc::clone(&scope),
            },
        );
        Ok(scope)
    }

    /// How many scopes are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |entries| entries.len())
    }

    /// Whether no scope is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Entry>>> {
        self.entries
            .lock()
            .map_err(|_| EngineError::Backend("tool gateway: scope cache poisoned".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::tools::testing::{Fixture, claims_for};

    #[tokio::test]
    async fn subjects_related_and_evidenced_entities_are_in_scope() {
        let fx = Fixture::new().await;
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &fx.reader())
            .await
            .expect("scope");
        assert!(scope.contains(&fx.order), "the subject");
        assert!(scope.is_subject(&fx.order));
        assert!(scope.contains(&fx.line), "the hook's related entity");
        assert!(!scope.is_subject(&fx.line));
        assert!(
            scope.contains(&fx.product),
            "evidenced by the order's grounding"
        );
        assert!(
            scope.contains(&fx.vendor),
            "evidenced by the related line's grounding"
        );
    }

    #[tokio::test]
    async fn literals_and_strangers_are_out_of_scope() {
        let fx = Fixture::new().await;
        let scope = TaskScope::resolve(std::slice::from_ref(&fx.order), &fx.reader())
            .await
            .expect("scope");
        assert!(
            !scope.contains(&EntityUri("late".to_owned())),
            "a literal object"
        );
        assert!(
            !scope.contains(&EntityUri("2027-11-15 08:00:00".to_owned())),
            "a date literal"
        );
        assert!(!scope.contains(&fx.stranger), "an unrelated entity");
    }

    #[tokio::test]
    async fn a_missing_subject_stays_in_scope_but_evidences_nothing() {
        let fx = Fixture::new().await;
        let ghost = EntityUri("https://ekg.example/id/sale.order/404".to_owned());
        let scope = TaskScope::resolve(std::slice::from_ref(&ghost), &fx.reader())
            .await
            .expect("scope");
        assert!(scope.contains(&ghost));
        assert!(!scope.contains(&fx.product));
    }

    #[tokio::test]
    async fn subjects_of_different_members_each_bring_their_own_members_scope() {
        let fx = Fixture::new().await;
        let both = [fx.order.clone(), fx.mo.clone()];
        let joint = TaskScope::resolve(&both, &fx.joint_reader())
            .await
            .expect("scope");
        // What each subject evidences under its own member profile, and no more.
        let order = TaskScope::resolve(std::slice::from_ref(&fx.order), &fx.reader())
            .await
            .expect("order");
        let mo = TaskScope::resolve(std::slice::from_ref(&fx.mo), &fx.plant_reader())
            .await
            .expect("mo");
        let union: HashSet<EntityUri> = order.entities.union(&mo.entities).cloned().collect();
        assert_eq!(joint.entities, union);
        assert!(joint.contains(&fx.line), "only the order evidences it");
        assert!(
            joint.contains(&fx.centre),
            "only the plant's hook relates it"
        );
        assert!(!order.contains(&fx.centre) && !mo.contains(&fx.line));
        assert!(joint.is_subject(&fx.order) && joint.is_subject(&fx.mo));
        assert!(!joint.contains(&fx.stranger), "neither evidences it");
    }

    #[tokio::test]
    async fn the_scope_is_resolved_once_per_token() {
        let fx = Fixture::new().await;
        let cache = ScopeCache::default();
        let claims = claims_for(std::slice::from_ref(&fx.order), 1_000);
        let reader = fx.reader();
        let first = cache.scope(&claims, &reader, 10).await.expect("first");
        let grounded = fx.counting.grounds();
        assert!(grounded > 0);
        let second = cache.scope(&claims, &reader, 20).await.expect("second");
        assert_eq!(first, second);
        assert_eq!(
            fx.counting.grounds(),
            grounded,
            "the second call is served from the cache"
        );
    }

    #[tokio::test]
    async fn the_cache_is_bounded_and_evicts_expired_tokens_first() {
        let fx = Fixture::new().await;
        let cache = ScopeCache::new(2);
        let reader = fx.reader();
        let expired = claims_for(std::slice::from_ref(&fx.order), 50);
        let soon = claims_for(std::slice::from_ref(&fx.order), 500);
        let late = claims_for(std::slice::from_ref(&fx.order), 900);
        cache.scope(&expired, &reader, 10).await.expect("expired");
        cache.scope(&soon, &reader, 10).await.expect("soon");
        assert_eq!(cache.len(), 2);
        // At t=100 the first token has expired: it is evicted, the other two are held.
        cache.scope(&late, &reader, 100).await.expect("late");
        assert_eq!(cache.len(), 2);
        // Full with live tokens: the one expiring soonest makes room.
        let newest = claims_for(std::slice::from_ref(&fx.order), 950);
        cache.scope(&newest, &reader, 100).await.expect("newest");
        assert_eq!(cache.len(), 2);
        let before = fx.counting.grounds();
        cache.scope(&late, &reader, 100).await.expect("late again");
        assert_eq!(
            fx.counting.grounds(),
            before,
            "the later token is still cached"
        );
        cache.scope(&soon, &reader, 100).await.expect("soon again");
        assert!(fx.counting.grounds() > before, "the soonest was evicted");
    }
}
