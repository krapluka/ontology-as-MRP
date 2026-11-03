//! The engine's typed error and result alias.
//!
//! `unwrap` is denied workspace-wide: seams fail loud with a typed error rather
//! than panic and risk corrupting truth.

use thiserror::Error;

/// Errors any engine seam may return.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The referenced entity or record does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// An event or materialization was not newer than the last one applied for the
    /// entity. This is the in-order guard that keeps stale writes from winning.
    #[error("stale revision for {entity}: incoming {incoming} <= applied {applied}")]
    StaleRevision {
        /// The entity URI.
        entity: String,
        /// The rejected revision.
        incoming: u64,
        /// The revision already applied.
        applied: u64,
    },

    /// Read-policy forbade the requested access.
    #[error("policy {policy} denied access")]
    PolicyDenied {
        /// The policy that denied.
        policy: String,
    },

    /// A proposal violated a governance invariant (e.g. undisclosed omissions).
    #[error("invalid proposal: {0}")]
    InvalidProposal(String),

    /// There is nothing a plan could reach, so no plan is asked for. An engine decision, not a
    /// failure of a run.
    #[error("nothing to do: {0}")]
    NothingToDo(String),

    /// An action cannot be reversed.
    #[error("action not reversible: {0}")]
    NotReversible(String),

    /// A conditional reverse was refused because the source advanced past the
    /// revision the action was executed against: reversing would clobber a
    /// legitimate later change.
    #[error(
        "reverse conflict for {external_ref}: source at {found} is newer than executed {executed}"
    )]
    ReverseConflict {
        /// The receipt's system-of-record reference.
        external_ref: String,
        /// The source revision the action was executed against.
        executed: u64,
        /// The (newer) source revision found at reverse time.
        found: u64,
    },

    /// A plan's mid-execution rollback could not complete: after an action failed,
    /// one or more already-committed actions could not be reversed (a concurrent edit
    /// advanced the source past a just-written watermark), so the plan is partially
    /// applied. The source boundary is not transactional.
    #[error(
        "plan partially applied: original failure [{failure}]; \
         {unreversed} already-committed action(s) could not be rolled back: {details}"
    )]
    PlanPartiallyApplied {
        /// The original action failure that triggered the rollback.
        failure: String,
        /// How many already-committed actions could not be reversed.
        unreversed: usize,
        /// Per-action rollback-refusal detail.
        details: String,
    },

    /// Reversing a plan left one or more actions un-reversed: their source records
    /// changed after the write, so their conditional reverse was refused rather than
    /// clobber the newer value. The other actions were reversed; this names what
    /// could not be.
    #[error("plan reverse incomplete: {unreversed} of {total} action(s) refused: {details}")]
    PlanReverseIncomplete {
        /// How many actions the plan had.
        total: usize,
        /// How many could not be reversed.
        unreversed: usize,
        /// Per-action refusal detail.
        details: String,
    },

    /// A governed invariant was violated before the store was touched (e.g. a
    /// materialization missing mandatory provenance). The write is refused so the
    /// world model is never left non-conforming.
    #[error("invariant violated: {0}")]
    Invariant(String),

    /// A backing store or transport failed.
    #[error("seam backend error: {0}")]
    Backend(String),

    /// A remote endpoint refused the call for load (e.g. HTTP 429) and kept refusing
    /// after the boundary's bounded retries. This is a transient failure a caller may
    /// retry later, unlike a refused answer.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// A remote endpoint could not answer: a timeout, a transport failure, a server
    /// error (HTTP 5xx), a permanent quota refusal or an empty answer. A failure of
    /// the call, not a refusal of its content.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// The engine's result alias.
pub type Result<T> = std::result::Result<T, EngineError>;
