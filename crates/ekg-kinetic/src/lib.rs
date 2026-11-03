//! Kinetic layer: governed action types (verbs) that write back under policy.
//!
//! Owns the real [`ActionExecutor`] driver ([`OdooActionExecutor`]) and the
//! concrete governed verbs ([`Verb`]): re-promise, expedite PO, notify owner,
//! reschedule / reassign a work order, split a manufacturing order, raise a work
//! center's capacity. Each has an inverse, writes back to Odoo via json2 and is
//! reversible under a source-watermark conflict check. The neutral core carries
//! verbs opaquely as `ekg_domain::ActionType`; the concrete set lives here.
//!
//! `execute` takes a `PromotedAction`, never a bare proposal, so a governed
//! write-back is unrepresentable without a human promotion. The [`PolicyEngine`]
//! decides whether a human is needed (human-gated by default, or auto-promote for
//! an allowlisted, reversible verb under a human-authored policy). It never opens
//! a second write path.

// The docs name products and regulations (Odoo, EU AI Act) without backticks.
#![allow(clippy::doc_markdown)]

pub mod executor;
pub mod policy;
pub mod verbs;

pub use ekg_domain::ActionExecutor;
pub use executor::OdooActionExecutor;
pub use policy::{
    ATTR_COST, ATTR_PO_STATE, CostWithinLimit, NoPromisedDateMove, PolicyContext, PolicyEngine,
    PromotionDecision, PromotionPredicate, PromotionTier, StructuralChange, SupplierConfirmed,
};
pub use verbs::{Verb, canonical_datetime, validate_action};
