/**
 * The view-model mapping layer: the one place where contract responses become presentational
 * props. Sourced fields are copied straight through, derived fields follow a rule the
 * operator could reconstruct, and fields the contract does not serve stay null.
 */
import type {
  AppliedChange as GqlAppliedChange,
  ActivePolicyQuery,
  ApprovedPlansQuery,
  AutoAppliedQuery,
  Change as GqlChange,
  PlanFieldsFragment,
  PlansQuery,
  PointFieldsFragment,
  Trace as GqlTrace,
} from "./graphql/graphql.ts";
import { ActionPointState, TraceTag as GqlTraceTag } from "./graphql/graphql.ts";
import type {
  AppliedChange,
  ApprovedPlan,
  Change,
  InboxKpis,
  Option,
  Plan,
  PlanEntry,
  PointState,
  Policy,
  ResolvedPoint,
  Trace,
  TraceStep,
  TraceTag,
} from "../viewmodel/types.ts";
import { asDelivery, deliveryLabel } from "../viewmodel/format.ts";
import { verbsTitle } from "../viewmodel/wording.ts";

type GqlPlan = PlanFieldsFragment;
type GqlOption = GqlPlan["options"][number];
type GqlEntry = PlansQuery["plans"][number];

const POINT_STATE: Record<ActionPointState, PointState> = {
  [ActionPointState.Open]: "open",
  [ActionPointState.Proposed]: "proposed",
  [ActionPointState.UnderReview]: "under_review",
  [ActionPointState.Promoted]: "promoted",
  [ActionPointState.Aborted]: "aborted",
  [ActionPointState.Reversed]: "reversed",
  [ActionPointState.Expired]: "expired",
};

const TRACE_TAG: Record<GqlTraceTag, TraceTag> = {
  [GqlTraceTag.Simulated]: "Simulated",
  [GqlTraceTag.Graph]: "Graph",
  [GqlTraceTag.Assumption]: "Assumption",
  [GqlTraceTag.Generation]: "Generation",
};

function mapTrace(trace: GqlTrace): Trace {
  return {
    steps: trace.steps.map<TraceStep>((s) => ({ tag: TRACE_TAG[s.tag], text: s.text, rejected: s.rejected })),
    summary: {
      simulated: trace.summary.simulated,
      rejected: trace.summary.rejected,
      openAssumptions: trace.summary.openAssumptions,
    },
  };
}

function mapChange(c: GqlChange): Change {
  return {
    verb: c.verb,
    object: c.object,
    params: c.params.map((p) => ({ key: p.key, value: p.value })),
    inverse: c.inverse ?? null,
    from: c.from ?? null,
    label: c.label ?? null,
  };
}

/** Reads one key's value from an opaque metrics bag, or null when it serves none. */
function metricValue(metrics: ReadonlyArray<{ key: string; value: string }>, key: string): string | null {
  return metrics.find((m) => m.key === key)?.value ?? null;
}

/** Derived: parses a signed whole-day slip from the bag, or null when unserved/unparseable. */
function daySlipOf(metrics: ReadonlyArray<{ key: string; value: string }>): number | null {
  const raw = metricValue(metrics, "day_slip");
  if (raw === null) return null;
  const n = Number(raw);
  return Number.isInteger(n) ? n : null;
}

/** Derived: the Delivery label read from the `delivery` display metric and its `day_slip`
 *  ("On time" / "N d late" / "At risk"). Only the outcomes the contract emits are labelled;
 *  anything else returns null. Nothing reads "On time" unless the served delivery is
 *  `on_time`. */
export function deriveOutcome(metrics: ReadonlyArray<{ key: string; value: string }>): string | null {
  return deliveryLabel(asDelivery(metricValue(metrics, "delivery")), daySlipOf(metrics));
}

/** Derived: the option's headline from its distinct change verbs, each worded by the fixed
 *  verb-label map (`split_production` reads "Split"; an unknown code is shown as served).
 *  The contract serves no option-level verb. */
export function deriveLabel(changes: ReadonlyArray<{ verb: string }>): string {
  return verbsTitle(changes);
}

/** Derived: the slate key A/B/C… from the option's rank position (options arrive best-first). */
function keyForIndex(index: number): string {
  return String.fromCharCode(65 + index);
}

function mapOption(o: GqlOption, index: number): Option {
  return {
    key: keyForIndex(index),
    label: deriveLabel(o.changes),
    recommended: o.recommended,
    rankKey: o.rankKey,
    scope: o.scope.map((c) => ({ kind: c.kind, count: c.count })),
    needs: o.needs.map((n) => ({ code: n.code, detail: n.detail })),
    changes: o.changes.map(mapChange),
    metrics: o.metrics.map((m) => ({ key: m.key, value: m.value })),
    delivery: asDelivery(metricValue(o.metrics, "delivery")),
    outcome: deriveOutcome(o.metrics),
    severity: metricValue(o.metrics, "severity"),
    projectedCompletion: metricValue(o.metrics, "projected_completion"),
    daySlip: daySlipOf(o.metrics),
    cost: metricValue(o.metrics, "cost"),
    costCurrency: metricValue(o.metrics, "cost_currency"),
    orderValue: metricValue(o.metrics, "order_value"),
    currency: metricValue(o.metrics, "currency"),
    lanes: o.lanes.map((l) => ({
      workCenter: l.workCenter,
      capacity: l.capacity ?? null,
      before: l.before.map((b) => ({ ...b })),
      after: l.after.map((b) => ({ ...b })),
    })),
    trace: mapTrace(o.trace),
  };
}

/** Maps a served plan plus its `situation` into a Plan. The plan-level framing comes from
 *  the do-nothing baseline of its lead point: its severity, delivery, projected completion,
 *  day-slip and order value with its currency. They feed the row's severity fill ("DAYS
 *  LATE") and the "If we do nothing" line. Each option carries its own "if approved" framing. */
export function mapPlan(plan: GqlPlan, situation: string): Plan {
  const options = plan.options.map(mapOption);
  const baseline = plan.baseline.metrics;
  return {
    subject: plan.subject,
    resolves: [...plan.resolves],
    lead: plan.lead,
    situation,
    severity: metricValue(baseline, "severity"),
    delivery: asDelivery(metricValue(baseline, "delivery")),
    daySlip: daySlipOf(baseline),
    projectedCompletion: metricValue(baseline, "projected_completion"),
    orderValue: metricValue(baseline, "order_value"),
    currency: metricValue(baseline, "currency"),
    baseline: {
      rankKey: plan.baseline.rankKey,
      metrics: plan.baseline.metrics.map((m) => ({ key: m.key, value: m.value })),
    },
    options,
    trace: mapTrace(plan.trace),
  };
}

export function mapAppliedChange(a: AutoAppliedQuery["autoApplied"][number] | GqlAppliedChange): AppliedChange {
  return {
    actionPoint: a.actionPoint,
    subject: a.subject,
    code: a.code,
    actionPointCode: a.actionPointCode,
    changes: a.changes.map(mapChange),
    criteria: a.criteria.map((c) => ({ rule: c.rule, value: c.value })),
    appliedAt: a.appliedAt ?? null,
    undoUntil: a.undoUntil ?? null,
    reversible: a.reversible,
    title: metricValue(a.metrics, "title"),
    effect: metricValue(a.metrics, "effect"),
  };
}

/** Maps a served action point a plan resolves. */
export function mapPoint(p: PointFieldsFragment): ResolvedPoint {
  return { id: p.id, subject: p.subject, state: POINT_STATE[p.state], reason: p.reason, code: p.code };
}

/** Maps a listed plan's row fields (everything but its plan). */
export function mapEntry(e: Pick<GqlEntry, "id" | "code" | "resolves" | "portfolio" | "openedAt" | "situation">): PlanEntry {
  return {
    id: e.id,
    code: e.code,
    resolves: e.resolves.map(mapPoint),
    portfolio: e.portfolio ? { id: e.portfolio.id, generation: e.portfolio.generation } : null,
    openedAt: e.openedAt ?? null,
    situation: e.situation,
  };
}

/** Maps an approved plan. */
export function mapApproved(a: ApprovedPlansQuery["approvedPlans"][number]): ApprovedPlan {
  return {
    id: a.id,
    code: a.code,
    resolves: a.resolves.map(mapPoint),
    portfolio: a.portfolio ? { id: a.portfolio.id, generation: a.portfolio.generation } : null,
    rankKey: a.rankKey ?? null,
    changes: a.changes.map(mapChange),
    reviewer: a.reviewer,
    rationale: a.rationale,
    promotedAt: a.promotedAt ?? null,
    undoUntil: a.undoUntil ?? null,
    reversible: a.reversible,
    remaining: a.remaining,
  };
}

export function mapPolicy(p: ActivePolicyQuery["policy"]): Policy {
  return {
    policyId: p.policyId,
    criteria: p.criteria.map((c) => ({ rule: c.rule, value: c.value })),
    costLimitCurrency: p.costLimitCurrency ?? null,
    undoWindow: p.undoWindow,
    autoApplyActions: [...p.autoApplyActions],
  };
}

/** Header KPIs: the derived `waiting` count plus the sourced revenue-at-risk (with its
 *  currency) and late-orders now / if approved, read from the read surface's opaque
 *  headline-metrics list (null when it serves none). */
export function mapKpis(waiting: number, headline: ReadonlyArray<{ key: string; value: string }>): InboxKpis {
  return {
    waiting,
    revenueAtRisk: metricValue(headline, "revenue_at_risk"),
    currency: metricValue(headline, "currency"),
    lateOrdersNow: metricValue(headline, "late_orders_now"),
    lateOrders: metricValue(headline, "late_orders"),
  };
}
