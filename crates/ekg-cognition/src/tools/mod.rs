//! The agent tool gateway: the engine-owned MCP surface agents read the world model through.
//!
//! - [`token`]: the short-lived task token the engine mints per reasoning task and
//!   verifies on every request (HMAC-SHA256, fixed format, capped lifetime).
//! - [`profile`]: the [`AgentTool`] trait, the [`ReadOnlyWorld`] facade tools see the
//!   world through, and the [`ToolProfile`]s a capability registers under opaque ids.
//! - [`scope`]: a task's [`TaskScope`] (its subjects, their related entities and what
//!   their groundings evidence), resolved once per token.
//! - [`neutral`]: the tools every profile offers (`ground_entity`, `related_entities`).
//! - [`submit`]: `submit_plan`, the typed way a task hands in its plan, over the profile's
//!   injected [`PlanCatalog`].
//! - [`finding`]: `submit_finding`, the typed way an analyst hands in what it found, with no
//!   field for free text.
//! - [`ledger`]: the [`RunLedger`]: per task, every call's outcome, the edges it read and what
//!   it handed in, read once by the engine after the task and never served.
//! - [`gateway`]: the `rmcp` server handler and the axum router with its bearer check.
//! - [`skill_routes`]: the skill versions a task's token pins, served read-only behind the
//!   same bearer check from an injected [`SkillSource`].
//!
//! The gateway is read-only and names no capability: the read context, the
//! related-entities hook and the tools all come from the profile a token names, and nothing
//! in a request can change them. A profile composed of others reads each entity as the member
//! it belongs to does.

pub mod finding;
pub mod gateway;
pub mod ledger;
pub mod neutral;
pub mod profile;
pub mod scope;
pub mod skill_routes;
pub mod submit;
pub mod token;

pub use finding::{
    CauseKind, Derived, Finding, FindingCause, FindingOrigin, FindingSource, MAX_CAUSE_EVIDENCE,
    MAX_FINDING_CAUSES, MemberState, ReadEdge, SUBMIT_FINDING, SubmitFinding, render_finding,
};
pub use gateway::{
    CallLog, CallRecord, GatewayConfig, TaskContext, ToolGateway, WWW_AUTHENTICATE, gateway_router,
};
pub use ledger::{AcceptedPlan, DEFAULT_RUN_LEDGER_CAPACITY, RunLedger, TaskRun};
pub use neutral::{GroundEntity, RelatedEntities};
pub use profile::{
    AgentTool, DEFAULT_TEXT_BOUND, DuplicateProfile, EmptyComposition, EntityDirectory, MemberRule,
    PlanRule, ProfileRegistry, ReadOnlyWorld, RequiredRead, ToolError, ToolProfile,
};
pub use scope::{DEFAULT_SCOPE_CACHE_CAPACITY, ScopeCache, TaskScope};
pub use skill_routes::{SkillLog, SkillOp, SkillRecord, SkillSource};
pub use submit::{
    ActionSpec, DuplicateVerb, JoinedCatalog, ParamSpec, PlanCatalog, SUBMIT_PLAN, SubmitPlan,
};
pub use token::{
    MAX_TOKEN_TTL, MIN_SECRET_LEN, SkillPin, TaskClaims, TaskGrant, TaskTokens, TokenError,
    now_secs,
};

/// Shared fixtures for the module's unit tests.
#[cfg(test)]
pub(crate) mod testing {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ekg_domain::fakes::InMemoryWorldModel;
    use ekg_domain::{
        EntityUri, Grounding, ReadContext, Result, Revision, SourceRecord, SourceRef, WorldModel,
    };

    use super::profile::{ReadOnlyWorld, ToolProfile};
    use super::token::TaskClaims;
    use crate::agent::GroundingExtension;

    /// A world model that counts its reads, over the in-memory fake.
    pub(crate) struct CountingWorld {
        inner: InMemoryWorldModel,
        grounds: AtomicUsize,
    }

    impl CountingWorld {
        pub(crate) fn grounds(&self) -> usize {
            self.grounds.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl WorldModel for CountingWorld {
        async fn materialize(&self, record: &SourceRecord) -> Result<Revision> {
            self.inner.materialize(record).await
        }
        async fn retract(&self, entity: &EntityUri) -> Result<Revision> {
            self.inner.retract(entity).await
        }
        async fn ground(&self, entity: &EntityUri, ctx: &ReadContext) -> Result<Grounding> {
            self.grounds.fetch_add(1, Ordering::SeqCst);
            self.inner.ground(entity, ctx).await
        }
        async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            self.inner.select(sparql).await
        }
    }

    /// A hook relating one subject (the order) to one entity (its purchase-order line),
    /// counting its calls.
    struct LineHook {
        order: EntityUri,
        line: EntityUri,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl GroundingExtension for LineHook {
        async fn related(&self, subject: &EntityUri) -> Result<Vec<EntityUri>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if *subject == self.order {
                vec![self.line.clone()]
            } else {
                Vec::new()
            })
        }
    }

    /// A sales order (hidden supplier, a product link), its related purchase-order line
    /// (a date literal, a vendor link), the product, the vendor, and an unrelated order. For
    /// a second member profile: a manufacturing order and the work centre its hook relates.
    pub(crate) struct Fixture {
        pub(crate) mo: EntityUri,
        pub(crate) centre: EntityUri,
        pub(crate) counting: Arc<CountingWorld>,
        pub(crate) order: EntityUri,
        pub(crate) line: EntityUri,
        pub(crate) product: EntityUri,
        pub(crate) vendor: EntityUri,
        pub(crate) stranger: EntityUri,
        hook_calls: Arc<AtomicUsize>,
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

    impl Fixture {
        pub(crate) async fn new() -> Self {
            let counting = Arc::new(CountingWorld {
                inner: InMemoryWorldModel::default(),
                grounds: AtomicUsize::new(0),
            });
            let uri = |model: &str, id: i64| {
                InMemoryWorldModel::uri_of(&record(model, id, &[]).source_ref)
            };
            let (order, line, product, vendor, stranger) = (
                uri("sale.order", 1),
                uri("purchase.order.line", 9),
                uri("product.product", 3),
                uri("res.partner", 5),
                uri("sale.order", 99),
            );
            let (mo, centre) = (uri("mrp.production", 7), uri("mrp.workcenter", 2));
            for rec in [
                record(
                    "sale.order",
                    1,
                    &[
                        ("state", "late"),
                        ("supplier", "CellTech"),
                        ("product", &product.0),
                    ],
                ),
                record(
                    "purchase.order.line",
                    9,
                    &[
                        ("date_planned", "2027-11-15 08:00:00"),
                        ("partner", &vendor.0),
                    ],
                ),
                record("product.product", 3, &[("name", "Battery")]),
                record("res.partner", 5, &[("name", "CellTech")]),
                record("sale.order", 99, &[("state", "on-time")]),
                record("mrp.production", 7, &[("state", "confirmed")]),
                record("mrp.workcenter", 2, &[("name", "Line 2")]),
            ] {
                counting.materialize(&rec).await.expect("materialize");
            }
            Self {
                counting,
                mo,
                centre,
                order,
                line,
                product,
                vendor,
                stranger,
                hook_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// The planner profile with the line hook.
        pub(crate) fn profile(&self) -> ToolProfile {
            ToolProfile::new(
                "test-planner",
                ReadContext {
                    role: "planner".to_owned(),
                    purpose: "delivery-promise".to_owned(),
                },
                Some(Arc::new(LineHook {
                    order: self.order.clone(),
                    line: self.line.clone(),
                    calls: Arc::clone(&self.hook_calls),
                })),
            )
        }

        pub(crate) fn reader(&self) -> ReadOnlyWorld {
            self.profile()
                .reader(Arc::clone(&self.counting) as Arc<dyn WorldModel>)
        }

        /// The plant profile: a scheduler whose hook relates the manufacturing order to its
        /// work centre.
        pub(crate) fn plant_profile(&self) -> ToolProfile {
            ToolProfile::new(
                "test-plant",
                ReadContext {
                    role: "scheduler".to_owned(),
                    purpose: "on-plan-schedule".to_owned(),
                },
                Some(Arc::new(LineHook {
                    order: self.mo.clone(),
                    line: self.centre.clone(),
                    calls: Arc::new(AtomicUsize::new(0)),
                })),
            )
        }

        pub(crate) fn plant_reader(&self) -> ReadOnlyWorld {
            self.plant_profile()
                .reader(Arc::clone(&self.counting) as Arc<dyn WorldModel>)
        }

        /// The planner and plant profiles composed: a manufacturing entity is the plant's.
        pub(crate) fn joint_reader(&self) -> ReadOnlyWorld {
            let joint = ToolProfile::composed(
                "test-joint",
                &[&self.profile(), &self.plant_profile()],
                Arc::new(|entity: &EntityUri| {
                    let plant = entity
                        .model()
                        .is_some_and(|model| model.starts_with("mrp."));
                    if plant { "test-plant" } else { "test-planner" }.to_owned()
                }),
            )
            .expect("two members");
            joint.reader(Arc::clone(&self.counting) as Arc<dyn WorldModel>)
        }

        pub(crate) fn hook_calls(&self) -> usize {
            self.hook_calls.load(Ordering::SeqCst)
        }
    }

    /// Claims for `subjects` expiring at `exp`, with a fresh token id.
    pub(crate) fn claims_for(subjects: &[EntityUri], exp: u64) -> TaskClaims {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        TaskClaims {
            jti: format!("jti-{}", NEXT.fetch_add(1, Ordering::SeqCst)),
            profile: "test-planner".to_owned(),
            subjects: subjects.to_vec(),
            tools: vec!["ground_entity".to_owned(), "related_entities".to_owned()],
            exp,
            trace_id: "trace".to_owned(),
            skills: BTreeMap::new(),
        }
    }
}
