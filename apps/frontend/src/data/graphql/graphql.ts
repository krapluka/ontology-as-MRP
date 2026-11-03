/* eslint-disable */
import type { TypedDocumentNode as DocumentNode } from '@graphql-typed-document-node/core';
export type Maybe<T> = T | null;
export type InputMaybe<T> = Maybe<T>;
export type Exact<T extends { [key: string]: unknown }> = { [K in keyof T]: T[K] };
export type MakeOptional<T, K extends keyof T> = Omit<T, K> & { [SubKey in K]?: Maybe<T[SubKey]> };
export type MakeMaybe<T, K extends keyof T> = Omit<T, K> & { [SubKey in K]: Maybe<T[SubKey]> };
export type MakeEmpty<T extends { [key: string]: unknown }, K extends keyof T> = { [_ in K]?: never };
export type Incremental<T> = T | { [P in keyof T]?: P extends ' $fragmentName' | '__typename' ? T[P] : never };
/** All built-in and custom scalars, mapped to their actual values */
export type Scalars = {
  ID: { input: string; output: string; }
  String: { input: string; output: string; }
  Boolean: { input: boolean; output: boolean; }
  Int: { input: number; output: number; }
  Float: { input: number; output: number; }
};

/**
 * An action point as the list card shows it: its id, the subject it is keyed by, its
 * current state, the deterministic reason the monitor opened it, and its short digest.
 */
export type ActionPoint = {
  /**
   * The point's short display code: four hex digits from [`display_code`] over its id, shown
   * as `AP-XXXX`.
   */
  code: Scalars['String']['output'];
  /**
   * The point's short digest: the digest that names a portfolio and a plan, over the
   * point's subject, so a client can show a short, reproducible point id without
   * re-implementing the function.
   */
  digest: Scalars['String']['output'];
  /** Stable action-point id. */
  id: Scalars['ID']['output'];
  /** The deterministic reason the monitor opened it (not model-generated). */
  reason: Scalars['String']['output'];
  /** Current lifecycle state. */
  state: ActionPointState;
  /** The subject entity this point is keyed by. */
  subject: Scalars['ID']['output'];
};

/** The lifecycle state of an action point, mirroring [`ActionPointState`]. */
export enum ActionPointState {
  /** A human aborted it via the stop control. */
  Aborted = 'ABORTED',
  /** It expired via an escalation timer without promotion. */
  Expired = 'EXPIRED',
  /** Raised by the monitor; awaiting a proposal. */
  Open = 'OPEN',
  /** A human (or the auto-apply policy) promoted the plan; the write-back ran. */
  Promoted = 'PROMOTED',
  /** The bounded agent attached a grounded proposal. */
  Proposed = 'PROPOSED',
  /** A promoted change was reversed through its inverse. */
  Reversed = 'REVERSED',
  /** A human is reviewing (oversight in progress). */
  UnderReview = 'UNDER_REVIEW'
}

/**
 * A change an agent auto-applied under policy, listed with the criteria it satisfied
 * and the window within which it may still be reversed. The changes carry their recorded
 * inverse (they are executed), so the governed undo can reverse them.
 */
export type AppliedChange = {
  /** The action point whose plan was auto-applied. */
  actionPoint: Scalars['ID']['output'];
  /**
   * The display code of the action point it closed, shown as `AP-XXXX`: the point's own
   * [`ActionPointView::code`].
   */
  actionPointCode: Scalars['String']['output'];
  /** When it was applied, as Unix epoch seconds (stringly). */
  appliedAt?: Maybe<Scalars['String']['output']>;
  /** The changes that were committed, each with its recorded inverse. */
  changes: Array<Change>;
  /**
   * The change's short display code, shown as `AU-XXXX`: [`display_code`] over
   * `applied/` plus the action point's id.
   */
  code: Scalars['String']['output'];
  /** The policy criteria this auto-apply satisfied. */
  criteria: Array<Criterion>;
  /**
   * The opaque, capability-filled display bag for this auto-applied change: a composed
   * `title` (its verbs) and `effect` sentence (its changes), so the view need not fabricate
   * prose. Rides the same neutral [`Metric`] container as a score's metrics; the contract
   * names none of the keys.
   */
  metrics: Array<Metric>;
  /** Whether every committed change is reversible (an undo can reverse the whole plan). */
  reversible: Scalars['Boolean']['output'];
  /** The subject the plan resolved. */
  subject: Scalars['ID']['output'];
  /**
   * The end of the reversal window, as Unix epoch seconds; an undo after this is
   * refused. `null` if the applied time is unknown.
   */
  undoUntil?: Maybe<Scalars['String']['output']>;
};

/**
 * A plan an operator approved: a promoted portfolio, or a point a human promoted on its own,
 * with what was approved and until when it can be undone.
 */
export type ApprovedPlan = {
  /** The committed changes, each with its recorded inverse. */
  changes: Array<Change>;
  /** The plan's short display code (the same rule as a listed plan's). */
  code: Scalars['String']['output'];
  /** The plan id (the same rule as a listed plan's). */
  id: Scalars['ID']['output'];
  /** The portfolio whose plan was approved, or `null` for a single point. */
  portfolio?: Maybe<PortfolioRef>;
  /** When it was approved, as Unix epoch seconds. */
  promotedAt?: Maybe<Scalars['String']['output']>;
  /** The approved option's `rank_key`, when recorded (opaque). */
  rankKey?: Maybe<Scalars['String']['output']>;
  /** Their rationale. */
  rationale: Scalars['String']['output'];
  /**
   * How many of its committed changes are still unreversed after a partial undo (0 when
   * no undo ran).
   */
  remaining: Scalars['Int']['output'];
  /** The points the plan resolved. */
  resolves: Array<ActionPoint>;
  /** Whether every committed change is reversible. */
  reversible: Scalars['Boolean']['output'];
  /** Who approved it. */
  reviewer: Scalars['String']['output'];
  /** When its reversal window closes, as Unix epoch seconds. An undo after it is refused. */
  undoUntil?: Maybe<Scalars['String']['output']>;
};

/**
 * One before/after bar on a work-center lane: a work order, its planned window, its row
 * (for stacking), and whether this option's changes moved it.
 */
export type Bar = {
  /** Whether this option's proposed changes moved this bar between before and after. */
  changed: Scalars['Boolean']['output'];
  /** Planned end (as materialized). */
  end: Scalars['String']['output'];
  /** The row for stacking overlapping bars on the lane. */
  row: Scalars['Int']['output'];
  /** Planned start (as materialized). */
  start: Scalars['String']['output'];
  /** The work order this bar represents. */
  workOrder: Scalars['ID']['output'];
};

/**
 * One governed change an option would commit: the verb, the target object, its
 * parameters (the intended new values) and the target's from value. An executed change
 * (an auto-applied one) also carries the recorded inverse that reverses it; for an option
 * not yet promoted the inverse is `null`, since it is only recorded once the write-back
 * runs. The `params` carry the to value, so `from` plus `params` give the change's from
 * and to.
 */
export type Change = {
  /**
   * The change's from value: the target's current value of the fact the verb moves, as
   * the capability reads it. An expedite's from is its line's current arrival, and a change
   * that moves no fact (a notification) has none. For an executed change this is the
   * recorded prior; for an option not yet promoted it is read live from the world model.
   * `null` when the fact is unknown or unread.
   */
  from?: Maybe<Scalars['String']['output']>;
  /** The recorded inverse that reverses this change, present only once executed. */
  inverse?: Maybe<Scalars['String']['output']>;
  /**
   * A display label for the change's object, composed by the capability from the
   * materialized names (for example an order line as its order reference, open quantity
   * and product). `null` when no name is materialized; the view then falls back to the
   * object id.
   */
  label?: Maybe<Scalars['String']['output']>;
  /** The entity the change targets. */
  object: Scalars['ID']['output'];
  /** The change parameters (the intended new value(s)), key-ordered. */
  params: Array<ChangeParam>;
  /** The governed verb (defined by the kinetic layer). */
  verb: Scalars['String']['output'];
};

/** One parameter of a change (e.g. the new date), stringly like the neutral core. */
export type ChangeParam = {
  /** The parameter name. */
  key: Scalars['String']['output'];
  /** Its value. */
  value: Scalars['String']['output'];
};

/**
 * One policy criterion an auto-applied change satisfied (or the active policy exposes):
 * a rule name and its threshold/value.
 */
export type Criterion = {
  /** The rule (e.g. `cost_within_limit`, `single_step_reversible`). */
  rule: Scalars['String']['output'];
  /** Its threshold / value (e.g. the cost limit). */
  value: Scalars['String']['output'];
};

/**
 * An entity id's display label: the name an operator reads for it, composed by the
 * capability from the world model. A display projection (texts keep their ids), served
 * only for an id the world can name.
 */
export type EntityLabel = {
  /** The entity id, exactly as asked. */
  id: Scalars['ID']['output'];
  /** The label to show for it. */
  label: Scalars['String']['output'];
};

/** One field of a projection row (a SPARQL solution binding of a variable to a term). */
export type Field = {
  /** The projection column / SPARQL variable name. */
  key: Scalars['String']['output'];
  /** Its term value. */
  value: Scalars['String']['output'];
};

/**
 * A work-center lane: its capacity band plus the "before" (current) and "after" (under
 * the option's changes) bars. A projection read, never a persisted artifact.
 */
export type Lane = {
  /** The schedule's bars under this option's proposed changes. */
  after: Array<Bar>;
  /** The current schedule's bars. */
  before: Array<Bar>;
  /** The agent's parallel capacity, if materialized. */
  capacity?: Maybe<Scalars['Float']['output']>;
  /** The work-center agent this lane belongs to. */
  workCenter: Scalars['ID']['output'];
};

/**
 * One entry of the opaque, capability-filled metrics bag. The schema enumerates no metric
 * name: `key`/`value` carry whatever the capability's scorer (and the derived display
 * metrics) put there. Delivery/cost ride this bag unnamed.
 */
export type Metric = {
  /** The capability-defined metric name (opaque to the contract). */
  key: Scalars['String']['output'];
  /** Its value, stringly like every neutral field. */
  value: Scalars['String']['output'];
};

/**
 * A structured reason a candidate cannot be auto-applied and needs a human: the policy
 * predicate it fails, carried as data so oversight sees why a plan waits without parsing
 * prose.
 */
export type Need = {
  /** A stable, capability-defined reason code. */
  code: Scalars['String']['output'];
  /** A human-readable detail for the reason. */
  detail: Scalars['String']['output'];
};

/**
 * A plan: a ranked slate over the points it resolves. `resolves` is a list, so a plan that
 * resolves one point and a portfolio's joint plan that resolves several are the same
 * contract.
 */
export type Plan = {
  /** The do-nothing baseline score (the outcome if no action is taken). */
  baseline: Score;
  /**
   * The point whose baseline frames the plan: the one it resolves, or the member with the
   * worst baseline. The baseline and every option's display metrics are judged for it.
   */
  lead: Scalars['ID']['output'];
  /** The ranked options, best-first. */
  options: Array<PlanOption>;
  /** The action points this plan resolves: the one point, or every member of its portfolio. */
  resolves: Array<Scalars['ID']['output']>;
  /** The action-point subject this plan was read for (a listed plan: its lead point). */
  subject: Scalars['ID']['output'];
  /** The slate-level reasoning trace over the whole loop (incl. rejected candidates). */
  trace: Trace;
};

/**
 * One plan awaiting oversight, shown as one inbox row: the joint plan of an active portfolio,
 * or the plan of a single awaiting point in no active portfolio.
 */
export type PlanEntry = {
  /** The plan's short display code, shown as `PL-XXXX`: `display_code` over the plan id. */
  code: Scalars['String']['output'];
  /**
   * The plan id: `plan/` plus the digest of the sorted points it resolves. This is the digest
   * that names a portfolio, so a portfolio's plan id and its portfolio id share one digest.
   */
  id: Scalars['ID']['output'];
  /**
   * When the plan's current slate was attached, as Unix epoch seconds; `null` while it is
   * pending, or for a slate attached before the time was recorded.
   */
  openedAt?: Maybe<Scalars['String']['output']>;
  /**
   * The plan itself (its options, reasoning, lanes and framing, as a point's plan read
   * serves them), or `null` while its slate is pending. A failure to read one entry's plan
   * is reported on this field only; the list is still served.
   */
  plan?: Maybe<Plan>;
  /** The portfolio the plan resolves, or `null` for a single point. */
  portfolio?: Maybe<PortfolioRef>;
  /** The points the plan resolves, each with its state and reason. */
  resolves: Array<ActionPoint>;
  /**
   * Why the points need a plan: a portfolio's coupling reasons for its current generation,
   * or the point's reason.
   */
  situation: Scalars['String']['output'];
};

/**
 * One ranked option on a plan's slate: its changes, the policy `needs` that keep it
 * human-gated, whether it is recommended, its reasoning-trace slice, and (resolved on
 * demand) its opaque display metrics and before/after work-center lanes.
 *
 * The plan, subject, and the scorer's raw metrics are carried but not exposed as fields
 * (`skip`); they back the derived `metrics`/`lanes` resolvers.
 */
export type PlanOption = {
  /** The governed changes this option would commit, in listed order. */
  changes: Array<Change>;
  /**
   * The before/after per-work-center lanes for this option: the current world-model
   * schedule ("before") and the schedule under this option's proposed changes ("after"),
   * derived as a projection read that writes nothing. Computed only when the field is
   * selected.
   */
  lanes: Array<Lane>;
  /**
   * The opaque display-metrics bag: the scorer's stored metrics plus the delivery and cost
   * display metrics derived read-only from this option's plan. Display-only: they do not
   * change the ranking (`rank_key`). Computed only when the field is selected.
   */
  metrics: Array<Metric>;
  /** The policy reasons this option cannot be auto-applied (empty if none). */
  needs: Array<Need>;
  /** The capability-defined ordering value (best-first); ranking is authoritative. */
  rankKey: Scalars['String']['output'];
  /** Whether this option is the agent's recommended one (at most one per slate). */
  recommended: Scalars['Boolean']['output'];
  /**
   * The option's scope: how many distinct objects its changes touch, per kind, counted
   * by the capability's stated rule over the world model. The kinds are data, never schema
   * names; a kind with none is omitted, and a failed read omits the scope rather than
   * estimating it. Computed only when the field is selected.
   */
  scope: Array<ScopeCount>;
  /** This option's reasoning-trace slice. */
  trace: Trace;
};

/**
 * The active auto-apply policy as the operator sees it: the criteria a change must
 * meet, the reversal window, and the pre-authorized verbs.
 */
export type Policy = {
  /** The verbs the policy pre-authorizes for auto-apply. */
  autoApplyActions: Array<Scalars['String']['output']>;
  /** The currency the cost limit is expressed in (ISO 4217 code), when configured. */
  costLimitCurrency?: Maybe<Scalars['String']['output']>;
  /** The criteria a change must satisfy to be auto-applied. */
  criteria: Array<Criterion>;
  /** The policy version id. */
  policyId: Scalars['String']['output'];
  /** A human-readable statement of the reversal window (e.g. "reversible for 8h"). */
  undoWindow: Scalars['String']['output'];
};

/** A portfolio a plan resolves, as its commands need it: its id and current generation. */
export type PortfolioRef = {
  /** The generation the plan belongs to, which a portfolio command must carry. */
  generation: Scalars['Int']['output'];
  /** The portfolio id (`portfolio/…`). */
  id: Scalars['ID']['output'];
};

/**
 * One row of a read-model tab projection: a neutral set of key/value fields (a SPARQL
 * solution). The tabs differ by which projection query runs, not by the row shape.
 */
export type ProjectionRow = {
  /** The row's fields, key-ordered. */
  fields: Array<Field>;
};

export type Query = {
  /**
   * The action-point list, optionally filtered to one lifecycle `state`. Reads the
   * durable state via the admin introspection (as the REST list does); commits nothing.
   */
  actionPoints: Array<ActionPoint>;
  /**
   * The plans approved by a human: promoted portfolios, and points a human promoted on
   * their own, approved since `since` (Unix epoch seconds, the start of the operator's
   * local day; the server's UTC day when omitted) or with their reversal window still
   * open. Each carries when its undo window closes. Commits nothing.
   */
  approvedPlans: Array<ApprovedPlan>;
  /**
   * The changes an agent auto-applied under policy, each with the criteria it
   * satisfied and the window within which it may still be reversed. An
   * auto-applied change is a promoted plan whose promoter is the policy (reviewer
   * `policy:<id>`), still in the `promoted` state (a reversed one is done). The changes
   * carry their recorded inverse, so the governed undo can reverse them.
   */
  autoApplied: Array<AppliedChange>;
  /**
   * The inbox's headline metrics as an opaque, capability-filled KPI list: late orders
   * now and if approved (`late_orders_now` / `late_orders`), and `revenue_at_risk` with its
   * `currency`. Each is taken over the points awaiting oversight and judged by the
   * capability. A metric is true or absent: a count or sum that cannot be read is omitted,
   * never served as zero. The keys are opaque, the contract does not enumerate them, so
   * the base stays capability-neutral. A non-authoritative read: computing it commits
   * nothing.
   */
  headlineMetrics: Array<Metric>;
  /**
   * The display labels of the entity ids a client shows: for each id the world can name,
   * the label the capability composes from the materialized names, in request order and
   * each id once. An unnamed, foreign or non-canonical id gets no label, never a guessed
   * one. At most 200 ids per request. A display projection: served texts keep their ids,
   * and reading labels commits nothing.
   */
  labels: Array<EntityLabel>;
  /**
   * One action point's plan for `subject`: the do-nothing baseline, each ranked option
   * (its changes, `needs`, opaque display metrics, scope, recommended flag, reasoning-trace
   * slice), and the slate-level trace. While the point is a member of an active portfolio,
   * its plan is the portfolio's joint slate for the current generation, resolving every
   * member; otherwise its own slate. `None` if the subject has no point or its plan is
   * still pending, never a member's pre-join per-point slate.
   */
  plan?: Maybe<Plan>;
  /**
   * The plans awaiting oversight, one inbox row each: the joint plan of every active
   * portfolio, and the plan of every awaiting point in no active portfolio, newest slate
   * first. A plan whose slate is pending is listed with no plan; a plan that cannot be
   * read is reported on its own `plan` field. Commits nothing.
   */
  plans: Array<PlanEntry>;
  /**
   * The active auto-apply policy as the operator sees it: the criteria a change must
   * meet, the reversal window, and the pre-authorized verbs.
   */
  policy: Policy;
  /**
   * A read-model tab projection (Timeline / Graph / Flow / Suppliers): the plant-wide
   * picture, in which `subject` only selects which path, risk and cause markers are lit.
   * Non-authoritative: reading it mutates nothing. An ill-formed subject yields no rows;
   * a well-formed subject the world does not know yields the plant-wide rows with nothing
   * lit. Neither is an error.
   */
  readModel: ReadModelView;
};


export type QueryActionPointsArgs = {
  state?: InputMaybe<ActionPointState>;
};


export type QueryApprovedPlansArgs = {
  since?: InputMaybe<Scalars['String']['input']>;
};


export type QueryLabelsArgs = {
  ids: Array<Scalars['ID']['input']>;
};


export type QueryPlanArgs = {
  subject: Scalars['ID']['input'];
};


export type QueryReadModelArgs = {
  subject: Scalars['ID']['input'];
  tab: TabKind;
};

/**
 * A read-model tab projection for a subject: the tab kind and its rows, each row a neutral
 * projection of the world model. Non-authoritative: reading mutates nothing.
 */
export type ReadModelView = {
  /** The projected rows. */
  rows: Array<ProjectionRow>;
  /** The subject the tab was projected for. */
  subject: Scalars['ID']['output'];
  /** Which tab this is. */
  tab: TabKind;
};

/**
 * One kind's count in an option's scope: a neutral kind label from the capability's fixed
 * vocabulary (data, not a schema name) and how many distinct objects of it the option's
 * changes touch.
 */
export type ScopeCount = {
  /** How many distinct objects of that kind the changes touch (never 0). */
  count: Scalars['Int']['output'];
  /** The kind of object counted (opaque to the contract). */
  kind: Scalars['String']['output'];
};

/**
 * A capability-filled [`Score`]: the opaque metrics bag plus the capability-defined
 * ranking value (`rank_key`; lexically smaller sorts first, so best-first). Reused for a
 * plan's do-nothing baseline and for each option.
 */
export type Score = {
  /** The opaque metrics bag. */
  metrics: Array<Metric>;
  /**
   * The capability-defined ordering value (best-first). Ranking is authoritative;
   * the derived display metrics never change it.
   */
  rankKey: Scalars['String']['output'];
};

/**
 * Which read-model tab a [`ReadModelView`] projects. Neutral: the tab kind selects the
 * projection query; the returned shape is the same neutral rows for every tab. Every tab is
 * plant-wide; the subject only decides what is lit.
 */
export enum TabKind {
  /**
   * The plant's flow stages, from sources to promised orders, with the subject's chain
   * lit across them.
   */
  Flow = 'FLOW',
  /**
   * The plant-wide dependency graph from orders to their inbound sources, with the
   * subject's path lit and its cause nodes marked.
   */
  Graph = 'GRAPH',
  /**
   * The whole supplier base with lead-time, delivery and exposure signals; the subject's
   * supplier is marked.
   */
  Suppliers = 'SUPPLIERS',
  /**
   * Every order against its promise date, plus the inbound purchase orders; the subject's
   * order is marked.
   */
  Timeline = 'TIMELINE'
}

/**
 * A structured reasoning trace: the ordered steps plus the derived summary. The
 * comprehension surface for meaningful human oversight (EU AI Act Art. 14/12).
 */
export type Trace = {
  /** The reasoning steps, in the order the loop performed them. */
  steps: Array<TraceStep>;
  /** Counts derived from the steps (cannot disagree with them). */
  summary: TraceSummary;
};

/**
 * One reasoning-trace step: a tag, a human line, and whether the candidate it describes
 * was tried and set aside, which keeps rejected candidates visible to oversight.
 */
export type TraceStep = {
  /** Whether this step describes a candidate that was tried and set aside. */
  rejected: Scalars['Boolean']['output'];
  /** Which kind of reasoning this step records. */
  tag: TraceTag;
  /** The human-readable line surfaced for oversight. */
  text: Scalars['String']['output'];
};

/** Summary counts over a trace, so oversight sees the reasoning shape at a glance. */
export type TraceSummary = {
  /** How many open-assumption steps the trace carries. */
  openAssumptions: Scalars['Int']['output'];
  /** How many steps describe a candidate that was tried and set aside. */
  rejected: Scalars['Int']['output'];
  /** How many candidate plans were simulated (incl. the do-nothing baseline). */
  simulated: Scalars['Int']['output'];
};

/** The kind of a reasoning-trace step, mirroring [`TraceTag`] (Art. 14 comprehension). */
export enum TraceTag {
  /** An open assumption forced by a grounding omission. */
  Assumption = 'ASSUMPTION',
  /**
   * A model generation attempt that yielded no candidate (its failure class and
   * reason), or the time budget ending generation. Set aside, never simulated.
   */
  Generation = 'GENERATION',
  /** A traversal of the governed evidence subgraph the reasoning rested on. */
  Graph = 'GRAPH',
  /** A candidate plan was projected onto a what-if and scored. */
  Simulated = 'SIMULATED'
}

export type PointFieldsFragment = { id: string, subject: string, state: ActionPointState, reason: string, code: string };

export type ChangeFieldsFragment = { verb: string, object: string, inverse?: string | null, from?: string | null, label?: string | null, params: Array<{ key: string, value: string }> };

export type TraceFieldsFragment = { summary: { simulated: number, rejected: number, openAssumptions: number }, steps: Array<{ tag: TraceTag, text: string, rejected: boolean }> };

export type PlanFieldsFragment = { subject: string, resolves: Array<string>, lead: string, baseline: { rankKey: string, metrics: Array<{ key: string, value: string }> }, trace: { summary: { simulated: number, rejected: number, openAssumptions: number }, steps: Array<{ tag: TraceTag, text: string, rejected: boolean }> }, options: Array<{ recommended: boolean, rankKey: string, needs: Array<{ code: string, detail: string }>, changes: Array<{ verb: string, object: string, inverse?: string | null, from?: string | null, label?: string | null, params: Array<{ key: string, value: string }> }>, metrics: Array<{ key: string, value: string }>, scope: Array<{ kind: string, count: number }>, lanes: Array<{ workCenter: string, capacity?: number | null, before: Array<{ workOrder: string, start: string, end: string, row: number, changed: boolean }>, after: Array<{ workOrder: string, start: string, end: string, row: number, changed: boolean }> }>, trace: { summary: { simulated: number, rejected: number, openAssumptions: number }, steps: Array<{ tag: TraceTag, text: string, rejected: boolean }> } }> };

export type PlansQueryVariables = Exact<{ [key: string]: never; }>;


export type PlansQuery = { plans: Array<{ id: string, code: string, openedAt?: string | null, situation: string, resolves: Array<{ id: string, subject: string, state: ActionPointState, reason: string, code: string }>, portfolio?: { id: string, generation: number } | null, plan?: { subject: string, resolves: Array<string>, lead: string, baseline: { rankKey: string, metrics: Array<{ key: string, value: string }> }, trace: { summary: { simulated: number, rejected: number, openAssumptions: number }, steps: Array<{ tag: TraceTag, text: string, rejected: boolean }> }, options: Array<{ recommended: boolean, rankKey: string, needs: Array<{ code: string, detail: string }>, changes: Array<{ verb: string, object: string, inverse?: string | null, from?: string | null, label?: string | null, params: Array<{ key: string, value: string }> }>, metrics: Array<{ key: string, value: string }>, scope: Array<{ kind: string, count: number }>, lanes: Array<{ workCenter: string, capacity?: number | null, before: Array<{ workOrder: string, start: string, end: string, row: number, changed: boolean }>, after: Array<{ workOrder: string, start: string, end: string, row: number, changed: boolean }> }>, trace: { summary: { simulated: number, rejected: number, openAssumptions: number }, steps: Array<{ tag: TraceTag, text: string, rejected: boolean }> } }> } | null }> };

export type ApprovedPlansQueryVariables = Exact<{
  since?: InputMaybe<Scalars['String']['input']>;
}>;


export type ApprovedPlansQuery = { approvedPlans: Array<{ id: string, code: string, rankKey?: string | null, reviewer: string, rationale: string, promotedAt?: string | null, undoUntil?: string | null, reversible: boolean, remaining: number, resolves: Array<{ id: string, subject: string, state: ActionPointState, reason: string, code: string }>, portfolio?: { id: string, generation: number } | null, changes: Array<{ verb: string, object: string, inverse?: string | null, from?: string | null, label?: string | null, params: Array<{ key: string, value: string }> }> }> };

export type AutoAppliedQueryVariables = Exact<{ [key: string]: never; }>;


export type AutoAppliedQuery = { autoApplied: Array<{ actionPoint: string, subject: string, code: string, actionPointCode: string, appliedAt?: string | null, undoUntil?: string | null, reversible: boolean, changes: Array<{ verb: string, object: string, inverse?: string | null, from?: string | null, label?: string | null, params: Array<{ key: string, value: string }> }>, criteria: Array<{ rule: string, value: string }>, metrics: Array<{ key: string, value: string }> }> };

export type ReadModelQueryVariables = Exact<{
  subject: Scalars['ID']['input'];
  tab: TabKind;
}>;


export type ReadModelQuery = { readModel: { subject: string, tab: TabKind, rows: Array<{ fields: Array<{ key: string, value: string }> }> } };

export type HeadlineMetricsQueryVariables = Exact<{ [key: string]: never; }>;


export type HeadlineMetricsQuery = { headlineMetrics: Array<{ key: string, value: string }> };

export type ActivePolicyQueryVariables = Exact<{ [key: string]: never; }>;


export type ActivePolicyQuery = { policy: { policyId: string, costLimitCurrency?: string | null, undoWindow: string, autoApplyActions: Array<string>, criteria: Array<{ rule: string, value: string }> } };

export type LabelsQueryVariables = Exact<{
  ids: Array<Scalars['ID']['input']> | Scalars['ID']['input'];
}>;


export type LabelsQuery = { labels: Array<{ id: string, label: string }> };

export const PointFieldsFragmentDoc = {"kind":"Document","definitions":[{"kind":"FragmentDefinition","name":{"kind":"Name","value":"PointFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"ActionPoint"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"state"}},{"kind":"Field","name":{"kind":"Name","value":"reason"}},{"kind":"Field","name":{"kind":"Name","value":"code"}}]}}]} as unknown as DocumentNode<PointFieldsFragment, unknown>;
export const TraceFieldsFragmentDoc = {"kind":"Document","definitions":[{"kind":"FragmentDefinition","name":{"kind":"Name","value":"TraceFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Trace"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"summary"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"simulated"}},{"kind":"Field","name":{"kind":"Name","value":"rejected"}},{"kind":"Field","name":{"kind":"Name","value":"openAssumptions"}}]}},{"kind":"Field","name":{"kind":"Name","value":"steps"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"tag"}},{"kind":"Field","name":{"kind":"Name","value":"text"}},{"kind":"Field","name":{"kind":"Name","value":"rejected"}}]}}]}}]} as unknown as DocumentNode<TraceFieldsFragment, unknown>;
export const ChangeFieldsFragmentDoc = {"kind":"Document","definitions":[{"kind":"FragmentDefinition","name":{"kind":"Name","value":"ChangeFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Change"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"verb"}},{"kind":"Field","name":{"kind":"Name","value":"object"}},{"kind":"Field","name":{"kind":"Name","value":"params"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"inverse"}},{"kind":"Field","name":{"kind":"Name","value":"from"}},{"kind":"Field","name":{"kind":"Name","value":"label"}}]}}]} as unknown as DocumentNode<ChangeFieldsFragment, unknown>;
export const PlanFieldsFragmentDoc = {"kind":"Document","definitions":[{"kind":"FragmentDefinition","name":{"kind":"Name","value":"PlanFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Plan"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"resolves"}},{"kind":"Field","name":{"kind":"Name","value":"lead"}},{"kind":"Field","name":{"kind":"Name","value":"baseline"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"rankKey"}},{"kind":"Field","name":{"kind":"Name","value":"metrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}}]}},{"kind":"Field","name":{"kind":"Name","value":"trace"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"TraceFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"options"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"recommended"}},{"kind":"Field","name":{"kind":"Name","value":"rankKey"}},{"kind":"Field","name":{"kind":"Name","value":"needs"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"code"}},{"kind":"Field","name":{"kind":"Name","value":"detail"}}]}},{"kind":"Field","name":{"kind":"Name","value":"changes"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"ChangeFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"metrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"scope"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"kind"}},{"kind":"Field","name":{"kind":"Name","value":"count"}}]}},{"kind":"Field","name":{"kind":"Name","value":"lanes"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"workCenter"}},{"kind":"Field","name":{"kind":"Name","value":"capacity"}},{"kind":"Field","name":{"kind":"Name","value":"before"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"workOrder"}},{"kind":"Field","name":{"kind":"Name","value":"start"}},{"kind":"Field","name":{"kind":"Name","value":"end"}},{"kind":"Field","name":{"kind":"Name","value":"row"}},{"kind":"Field","name":{"kind":"Name","value":"changed"}}]}},{"kind":"Field","name":{"kind":"Name","value":"after"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"workOrder"}},{"kind":"Field","name":{"kind":"Name","value":"start"}},{"kind":"Field","name":{"kind":"Name","value":"end"}},{"kind":"Field","name":{"kind":"Name","value":"row"}},{"kind":"Field","name":{"kind":"Name","value":"changed"}}]}}]}},{"kind":"Field","name":{"kind":"Name","value":"trace"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"TraceFields"}}]}}]}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"TraceFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Trace"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"summary"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"simulated"}},{"kind":"Field","name":{"kind":"Name","value":"rejected"}},{"kind":"Field","name":{"kind":"Name","value":"openAssumptions"}}]}},{"kind":"Field","name":{"kind":"Name","value":"steps"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"tag"}},{"kind":"Field","name":{"kind":"Name","value":"text"}},{"kind":"Field","name":{"kind":"Name","value":"rejected"}}]}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"ChangeFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Change"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"verb"}},{"kind":"Field","name":{"kind":"Name","value":"object"}},{"kind":"Field","name":{"kind":"Name","value":"params"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"inverse"}},{"kind":"Field","name":{"kind":"Name","value":"from"}},{"kind":"Field","name":{"kind":"Name","value":"label"}}]}}]} as unknown as DocumentNode<PlanFieldsFragment, unknown>;
export const PlansDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"Plans"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"plans"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"code"}},{"kind":"Field","name":{"kind":"Name","value":"resolves"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"PointFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"portfolio"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"generation"}}]}},{"kind":"Field","name":{"kind":"Name","value":"openedAt"}},{"kind":"Field","name":{"kind":"Name","value":"situation"}},{"kind":"Field","name":{"kind":"Name","value":"plan"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"PlanFields"}}]}}]}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"TraceFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Trace"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"summary"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"simulated"}},{"kind":"Field","name":{"kind":"Name","value":"rejected"}},{"kind":"Field","name":{"kind":"Name","value":"openAssumptions"}}]}},{"kind":"Field","name":{"kind":"Name","value":"steps"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"tag"}},{"kind":"Field","name":{"kind":"Name","value":"text"}},{"kind":"Field","name":{"kind":"Name","value":"rejected"}}]}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"ChangeFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Change"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"verb"}},{"kind":"Field","name":{"kind":"Name","value":"object"}},{"kind":"Field","name":{"kind":"Name","value":"params"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"inverse"}},{"kind":"Field","name":{"kind":"Name","value":"from"}},{"kind":"Field","name":{"kind":"Name","value":"label"}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"PointFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"ActionPoint"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"state"}},{"kind":"Field","name":{"kind":"Name","value":"reason"}},{"kind":"Field","name":{"kind":"Name","value":"code"}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"PlanFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Plan"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"resolves"}},{"kind":"Field","name":{"kind":"Name","value":"lead"}},{"kind":"Field","name":{"kind":"Name","value":"baseline"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"rankKey"}},{"kind":"Field","name":{"kind":"Name","value":"metrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}}]}},{"kind":"Field","name":{"kind":"Name","value":"trace"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"TraceFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"options"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"recommended"}},{"kind":"Field","name":{"kind":"Name","value":"rankKey"}},{"kind":"Field","name":{"kind":"Name","value":"needs"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"code"}},{"kind":"Field","name":{"kind":"Name","value":"detail"}}]}},{"kind":"Field","name":{"kind":"Name","value":"changes"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"ChangeFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"metrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"scope"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"kind"}},{"kind":"Field","name":{"kind":"Name","value":"count"}}]}},{"kind":"Field","name":{"kind":"Name","value":"lanes"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"workCenter"}},{"kind":"Field","name":{"kind":"Name","value":"capacity"}},{"kind":"Field","name":{"kind":"Name","value":"before"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"workOrder"}},{"kind":"Field","name":{"kind":"Name","value":"start"}},{"kind":"Field","name":{"kind":"Name","value":"end"}},{"kind":"Field","name":{"kind":"Name","value":"row"}},{"kind":"Field","name":{"kind":"Name","value":"changed"}}]}},{"kind":"Field","name":{"kind":"Name","value":"after"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"workOrder"}},{"kind":"Field","name":{"kind":"Name","value":"start"}},{"kind":"Field","name":{"kind":"Name","value":"end"}},{"kind":"Field","name":{"kind":"Name","value":"row"}},{"kind":"Field","name":{"kind":"Name","value":"changed"}}]}}]}},{"kind":"Field","name":{"kind":"Name","value":"trace"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"TraceFields"}}]}}]}}]}}]} as unknown as DocumentNode<PlansQuery, PlansQueryVariables>;
export const ApprovedPlansDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"ApprovedPlans"},"variableDefinitions":[{"kind":"VariableDefinition","variable":{"kind":"Variable","name":{"kind":"Name","value":"since"}},"type":{"kind":"NamedType","name":{"kind":"Name","value":"String"}}}],"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"approvedPlans"},"arguments":[{"kind":"Argument","name":{"kind":"Name","value":"since"},"value":{"kind":"Variable","name":{"kind":"Name","value":"since"}}}],"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"code"}},{"kind":"Field","name":{"kind":"Name","value":"resolves"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"PointFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"portfolio"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"generation"}}]}},{"kind":"Field","name":{"kind":"Name","value":"rankKey"}},{"kind":"Field","name":{"kind":"Name","value":"changes"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"ChangeFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"reviewer"}},{"kind":"Field","name":{"kind":"Name","value":"rationale"}},{"kind":"Field","name":{"kind":"Name","value":"promotedAt"}},{"kind":"Field","name":{"kind":"Name","value":"undoUntil"}},{"kind":"Field","name":{"kind":"Name","value":"reversible"}},{"kind":"Field","name":{"kind":"Name","value":"remaining"}}]}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"PointFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"ActionPoint"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"state"}},{"kind":"Field","name":{"kind":"Name","value":"reason"}},{"kind":"Field","name":{"kind":"Name","value":"code"}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"ChangeFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Change"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"verb"}},{"kind":"Field","name":{"kind":"Name","value":"object"}},{"kind":"Field","name":{"kind":"Name","value":"params"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"inverse"}},{"kind":"Field","name":{"kind":"Name","value":"from"}},{"kind":"Field","name":{"kind":"Name","value":"label"}}]}}]} as unknown as DocumentNode<ApprovedPlansQuery, ApprovedPlansQueryVariables>;
export const AutoAppliedDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"AutoApplied"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"autoApplied"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"actionPoint"}},{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"code"}},{"kind":"Field","name":{"kind":"Name","value":"actionPointCode"}},{"kind":"Field","name":{"kind":"Name","value":"changes"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"FragmentSpread","name":{"kind":"Name","value":"ChangeFields"}}]}},{"kind":"Field","name":{"kind":"Name","value":"criteria"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"rule"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"appliedAt"}},{"kind":"Field","name":{"kind":"Name","value":"undoUntil"}},{"kind":"Field","name":{"kind":"Name","value":"reversible"}},{"kind":"Field","name":{"kind":"Name","value":"metrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}}]}}]}},{"kind":"FragmentDefinition","name":{"kind":"Name","value":"ChangeFields"},"typeCondition":{"kind":"NamedType","name":{"kind":"Name","value":"Change"}},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"verb"}},{"kind":"Field","name":{"kind":"Name","value":"object"}},{"kind":"Field","name":{"kind":"Name","value":"params"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"inverse"}},{"kind":"Field","name":{"kind":"Name","value":"from"}},{"kind":"Field","name":{"kind":"Name","value":"label"}}]}}]} as unknown as DocumentNode<AutoAppliedQuery, AutoAppliedQueryVariables>;
export const ReadModelDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"ReadModel"},"variableDefinitions":[{"kind":"VariableDefinition","variable":{"kind":"Variable","name":{"kind":"Name","value":"subject"}},"type":{"kind":"NonNullType","type":{"kind":"NamedType","name":{"kind":"Name","value":"ID"}}}},{"kind":"VariableDefinition","variable":{"kind":"Variable","name":{"kind":"Name","value":"tab"}},"type":{"kind":"NonNullType","type":{"kind":"NamedType","name":{"kind":"Name","value":"TabKind"}}}}],"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"readModel"},"arguments":[{"kind":"Argument","name":{"kind":"Name","value":"subject"},"value":{"kind":"Variable","name":{"kind":"Name","value":"subject"}}},{"kind":"Argument","name":{"kind":"Name","value":"tab"},"value":{"kind":"Variable","name":{"kind":"Name","value":"tab"}}}],"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"subject"}},{"kind":"Field","name":{"kind":"Name","value":"tab"}},{"kind":"Field","name":{"kind":"Name","value":"rows"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"fields"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}}]}}]}}]}}]} as unknown as DocumentNode<ReadModelQuery, ReadModelQueryVariables>;
export const HeadlineMetricsDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"HeadlineMetrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"headlineMetrics"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"key"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}}]}}]} as unknown as DocumentNode<HeadlineMetricsQuery, HeadlineMetricsQueryVariables>;
export const ActivePolicyDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"ActivePolicy"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"policy"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"policyId"}},{"kind":"Field","name":{"kind":"Name","value":"criteria"},"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"rule"}},{"kind":"Field","name":{"kind":"Name","value":"value"}}]}},{"kind":"Field","name":{"kind":"Name","value":"costLimitCurrency"}},{"kind":"Field","name":{"kind":"Name","value":"undoWindow"}},{"kind":"Field","name":{"kind":"Name","value":"autoApplyActions"}}]}}]}}]} as unknown as DocumentNode<ActivePolicyQuery, ActivePolicyQueryVariables>;
export const LabelsDocument = {"kind":"Document","definitions":[{"kind":"OperationDefinition","operation":"query","name":{"kind":"Name","value":"Labels"},"variableDefinitions":[{"kind":"VariableDefinition","variable":{"kind":"Variable","name":{"kind":"Name","value":"ids"}},"type":{"kind":"NonNullType","type":{"kind":"ListType","type":{"kind":"NonNullType","type":{"kind":"NamedType","name":{"kind":"Name","value":"ID"}}}}}}],"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"labels"},"arguments":[{"kind":"Argument","name":{"kind":"Name","value":"ids"},"value":{"kind":"Variable","name":{"kind":"Name","value":"ids"}}}],"selectionSet":{"kind":"SelectionSet","selections":[{"kind":"Field","name":{"kind":"Name","value":"id"}},{"kind":"Field","name":{"kind":"Name","value":"label"}}]}}]}}]} as unknown as DocumentNode<LabelsQuery, LabelsQueryVariables>;