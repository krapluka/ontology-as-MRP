/**
 * Fixtures for Storybook and tests: inbox data in the view-model shape. They carry the full
 * display framing the read surface serves. For the do-nothing baseline and each option that
 * is the delivery outcome, severity tier, projected completion and day-slip against the
 * original promise, and order value and cost each with its currency (the seeded data is
 * USD). They also carry each change's label and from/to, the auto-applied title/effect, and
 * the revenue-at-risk and late-orders (now and if approved) KPIs. The states covered:
 * recommended, selected, a retained rejected candidate, human-gated (`needs`), an at-risk
 * plan with no day count, and empty.
 */
import type {
  AppliedChange,
  ApprovedPlan,
  Bar,
  Change,
  Delivery,
  InboxItem,
  InboxKpis,
  Lane,
  Option,
  Plan,
  PlanEntry,
  PointState,
  Policy,
  ResolvedPoint,
  ScopeCount,
  Trace,
} from "../viewmodel/types.ts";
import { deliveryLabel } from "../viewmodel/format.ts";
import { verbsTitle } from "../viewmodel/wording.ts";

const ID = "https://ekg.example/id";

/** The seeded company's currency (every order, PO and KPI is served in it). */
const USD = "USD";

function change(
  verb: string,
  object: string,
  value: string,
  { inverse = null, from = null, label = null }: { inverse?: string | null; from?: string | null; label?: string | null } = {},
): Change {
  return { verb, object, params: [{ key: "new_value", value }], inverse, from, label };
}

/** An expedite: it targets the inbound purchase-order line and carries its `new_date`. */
function expedite(
  line: string,
  value: string,
  { inverse = null, from = null, label = null }: { inverse?: string | null; from?: string | null; label?: string | null } = {},
): Change {
  return { verb: "expedite_po", object: `${ID}/purchase.order.line/${line}`, params: [{ key: "new_date", value }], inverse, from, label };
}

/** A do-nothing baseline's served framing (the same keys an option serves, minus cost). */
function baselineOf(b: {
  delivery: Delivery;
  severity: string;
  projectedCompletion?: string;
  daySlip?: number;
  orderValue: string;
}): Plan["baseline"] {
  return {
    rankKey: "99999",
    metrics: [
      { key: "delivery", value: b.delivery },
      { key: "severity", value: b.severity },
      ...(b.projectedCompletion ? [{ key: "projected_completion", value: b.projectedCompletion }] : []),
      ...(b.daySlip !== undefined ? [{ key: "day_slip", value: String(b.daySlip) }] : []),
      { key: "order_value", value: b.orderValue },
      { key: "currency", value: USD },
      { key: "late_orders", value: b.delivery === "on_time" ? "0" : "1" },
    ],
  };
}

function bar(workOrder: string, start: string, end: string, row: number, changed = false): Bar {
  return { workOrder, start: `2027-${start}`, end: `2027-${end}`, row, changed };
}

const assemblyLane: Lane = {
  workCenter: `${ID}/mrp.workcenter/assembly`,
  capacity: 2,
  before: [bar("X", "01-25", "01-29", 0), bar("S1", "01-27", "01-30", 1), bar("T4", "01-28", "01-30", 2)],
  after: [
    bar("X", "01-25", "01-29", 0),
    bar("S1·30", "01-26", "01-28", 1, true),
    bar("S1·10", "01-30", "01-31", 0, true),
    bar("T4", "01-30", "02-01", 1, true),
  ],
};

const paintLane: Lane = {
  workCenter: `${ID}/mrp.workcenter/paint`,
  capacity: 1,
  before: [bar("X", "01-29", "01-30", 0), bar("S1", "01-30", "02-01", 0), bar("T4", "01-30", "01-31", 1)],
  after: [
    bar("S1", "01-28", "01-29", 0, true),
    bar("X", "01-29", "01-30", 0),
    bar("S1", "01-30", "01-31", 0, true),
    bar("T4", "02-01", "02-02", 0, true),
  ],
};

const splitTrace: Trace = {
  summary: { simulated: 5, rejected: 2, openAssumptions: 1 },
  steps: [
    { tag: "Graph", text: "Linked AP-0233 (controllers late) and AP-0239 (Assembly Line over capacity on 28 Jan). Both run through MO-0318.", rejected: false },
    { tag: "Graph", text: "30 controllers on hand cover 30 of 40 S1 bikes; P00091 brings 50 on 1 Feb.", rejected: false },
    { tag: "Simulated", text: "Run T4 before S1 — Paint Booth double-booked on 28 Jan.", rejected: true },
    { tag: "Simulated", text: "Split S1 30+10 without expediting — last 10 ship 3 Feb, 3 days late.", rejected: true },
    { tag: "Simulated", text: "Split S1 and expedite P00091 to 29 Jan — all 40 ship by 31 Jan.", rejected: false },
    { tag: "Assumption", text: "PartsHub can deliver by 29 Jan (express confirmed 3 of last 4).", rejected: false },
  ],
};

/** A capability severity tier from the residual-risk count, mirroring the backend threshold. */
function severityOf(risk: number): string {
  if (risk === 0) return "on_track";
  return risk === 1 ? "high" : "critical";
}

/** A scope from `[kind, count]` pairs, in the order served. */
function scope(...pairs: [string, number][]): ScopeCount[] {
  return pairs.map(([kind, count]) => ({ kind, count }));
}

function option(o: {
  key: string;
  recommended?: boolean;
  rankKey: string;
  delivery: Delivery;
  cost: string;
  risk: number;
  projectedCompletion?: string;
  daySlip?: number;
  orderValue?: string;
  needs?: [string, string][];
  changes: Change[];
  scope?: ScopeCount[];
  lanes?: Lane[];
  trace?: Trace;
}): Option {
  const severity = severityOf(o.risk);
  const projectedCompletion = o.projectedCompletion ?? null;
  return {
    key: o.key,
    // The label the mapping derives from the changes' verbs.
    label: verbsTitle(o.changes),
    recommended: o.recommended ?? false,
    rankKey: o.rankKey,
    scope: o.scope ?? [],
    needs: (o.needs ?? []).map(([code, detail]) => ({ code, detail })),
    changes: o.changes,
    metrics: [
      { key: "delivery", value: o.delivery },
      { key: "cost", value: o.cost },
      { key: "cost_currency", value: USD },
      { key: "risk_left", value: String(o.risk) },
      { key: "severity", value: severity },
      ...(projectedCompletion ? [{ key: "projected_completion", value: projectedCompletion }] : []),
      ...(o.daySlip !== undefined ? [{ key: "day_slip", value: String(o.daySlip) }] : []),
      ...(o.orderValue ? [{ key: "order_value", value: o.orderValue }, { key: "currency", value: USD }] : []),
      { key: "late_orders", value: o.delivery === "on_time" ? "0" : "1" },
    ],
    delivery: o.delivery,
    outcome: deliveryLabel(o.delivery, o.daySlip ?? null),
    severity,
    projectedCompletion,
    daySlip: o.daySlip ?? null,
    cost: o.cost,
    costCurrency: USD,
    orderValue: o.orderValue ?? null,
    currency: o.orderValue ? USD : null,
    lanes: o.lanes ?? [],
    trace: o.trace ?? splitTrace,
  };
}

/** PL-0412: a portfolio of the Stadtrad order's point and its manufacturing order's point,
 *  planned together. The order is the lead (4 days late). */
export const planWithLanes: Plan = {
  subject: `${ID}/sale.order/147`,
  resolves: [`${ID}/mrp.production/318`, `${ID}/sale.order/147`],
  lead: `${ID}/sale.order/147`,
  situation:
    "Two signals meet at MO-0318. Controllers for the 40 City S1 bikes arrive 1 Feb, a day after the promise. The Assembly Line is also booked for three orders on 28 Jan against a capacity of two, and the Paint Booth is double-booked on 30 Jan.",
  severity: "critical",
  delivery: "late",
  daySlip: 4,
  projectedCompletion: "2027-02-04T00:00:00",
  orderValue: "18500.00",
  currency: USD,
  baseline: baselineOf({ delivery: "late", severity: "critical", projectedCompletion: "2027-02-04T00:00:00", daySlip: 4, orderValue: "18500.00" }),
  trace: splitTrace,
  options: [
    option({
      key: "A",
      recommended: true,
      rankKey: "00000",
      delivery: "on_time",
      cost: "320.00",
      risk: 1,
      projectedCompletion: "2027-01-31T00:00:00",
      daySlip: 0,
      orderValue: "18500.00",
      needs: [
        ["cost_over_limit", "cost 320.00 exceeds the limit 250.00"],
        ["structural_change", "splits a manufacturing order"],
      ],
      changes: [
        change("split_production", `${ID}/mrp.production/318`, "30 now + 10 on 29 Jan", { from: "40× City S1", label: "WH/MO/00318 · 40× City S1" }),
        change("reschedule_work_order", `${ID}/mrp.workorder/3181`, "26 Jan", { from: "28 Jan", label: "WH/MO/00318 · Assembly" }),
        change("reschedule_work_order", `${ID}/mrp.workorder/3191`, "Assembly 30 Jan · Paint 1 Feb", { label: "WH/MO/00319 · Assembly" }),
        expedite("91", "29 Jan · +$320", { from: "1 Feb", label: "P00091 · 50 × Motor Controller" }),
      ],
      scope: scope(["order", 2], ["work center", 2], ["purchase order", 1]),
      lanes: [assemblyLane, paintLane],
    }),
    option({
      key: "B",
      rankKey: "00001",
      delivery: "on_time",
      cost: "2170.00",
      risk: 2,
      projectedCompletion: "2027-01-31T00:00:00",
      daySlip: 0,
      orderValue: "18500.00",
      needs: [["cost_over_limit", "cost 2170.00 exceeds the limit 250.00"]],
      changes: [
        expedite("91", "27 Jan · +$320", { from: "1 Feb", label: "P00091 · 50 × Motor Controller" }),
        change("raise_work_center_capacity", `${ID}/mrp.workcenter.capacity/1`, "capacity 3 · +$1,850", { from: "capacity 2", label: "Assembly Line" }),
        change("reschedule_work_order", `${ID}/mrp.workorder/3192`, "31 Jan", { label: "WH/MO/00319 · Paint Booth" }),
      ],
      scope: scope(["order", 1], ["work center", 2], ["purchase order", 1]),
    }),
    option({
      key: "C",
      rankKey: "00002",
      delivery: "late",
      cost: "0.00",
      risk: 2,
      projectedCompletion: "2027-02-04T00:00:00",
      daySlip: 4,
      orderValue: "18500.00",
      needs: [["promised_date_move", "moves a customer promised date (sale.order.commitment_date)"]],
      changes: [
        change("re_promise_date", `${ID}/sale.order/147`, "4 Feb", { from: "31 Jan", label: "S00147 · Stadtrad Leasing" }),
        change("reschedule_work_order", `${ID}/mrp.workorder/3181`, "1 Feb", { from: "28 Jan", label: "WH/MO/00318 · Assembly" }),
      ],
      scope: scope(["order", 2], ["work center", 1]),
    }),
  ],
};

const batteryTrace: Trace = {
  summary: { simulated: 4, rejected: 2, openAssumptions: 1 },
  steps: [
    { tag: "Graph", text: "Battery Pack 500Wh: 2 on hand against 10 needed; P00087 lands 17 Feb.", rejected: false },
    { tag: "Graph", text: "Substitute the 400Wh pack — not an approved BOM alternative.", rejected: true },
    { tag: "Simulated", text: "Expedite P00087 by sea to 31 Jan — bikes ready 7 Feb, 14 days late.", rejected: true },
    { tag: "Simulated", text: "Air-freight P00087 for 17 Jan and book Assembly 18–20 Jan — ready 22 Jan.", rejected: false },
    { tag: "Assumption", text: "CellTech can ship by 17 Jan — not confirmed yet.", rejected: false },
  ],
};

/** The reference scenario: the do-nothing baseline slips 24 days and the recommended option is
 *  on time, so the plan row reads "+24 / DAYS LATE", option A "On time". */
export const planNoLanes: Plan = {
  subject: `${ID}/sale.order/142`,
  resolves: [`${ID}/sale.order/142`],
  lead: `${ID}/sale.order/142`,
  situation:
    "Battery Pack 500Wh: 2 on hand (safety stock 5) against a demand of 10. The only inbound PO, P00087 from CellTech GmbH, lands 17 Feb, 24 days after the promise.",
  severity: "high",
  delivery: "late",
  daySlip: 24,
  projectedCompletion: "2027-02-22T00:00:00",
  orderValue: "20700.00",
  currency: USD,
  baseline: baselineOf({ delivery: "late", severity: "high", projectedCompletion: "2027-02-22T00:00:00", daySlip: 24, orderValue: "20700.00" }),
  trace: batteryTrace,
  options: [
    option({
      key: "A",
      recommended: true,
      rankKey: "00000",
      delivery: "on_time",
      cost: "1450.00",
      risk: 2,
      projectedCompletion: "2027-01-22T00:00:00",
      daySlip: 0,
      orderValue: "20700.00",
      needs: [
        ["cost_over_limit", "cost 1450.00 exceeds the limit 250.00"],
        ["supplier_unconfirmed", "purchase order is not in a confirmed state (state: \"sent\")"],
      ],
      changes: [
        expedite("87", "17 Jan · air freight", { from: "17 Feb", label: "P00087 · 10 × Battery Pack 500Wh" }),
        change("reschedule_work_order", `${ID}/mrp.workorder/3161`, "18–20 Jan", { label: "WH/MO/00316 · Assembly" }),
        change("notify_owner", `${ID}/sale.order/142`, "task for account owner", { label: "S00142 · VeloMax Retail" }),
      ],
      // "1 order · 1 PO · 1 work center", served in another kind order.
      scope: scope(["order", 1], ["purchase order", 1], ["work center", 1]),
      trace: batteryTrace,
    }),
    option({
      key: "B",
      rankKey: "00001",
      delivery: "late",
      cost: "0.00",
      risk: 1,
      projectedCompletion: "2027-02-22T00:00:00",
      daySlip: 24,
      orderValue: "20700.00",
      needs: [["promised_date_move", "moves a customer promised date (sale.order.commitment_date)"]],
      changes: [change("re_promise_date", `${ID}/sale.order/142`, "22 Feb", { from: "29 Jan", label: "S00142 · VeloMax Retail" })],
      scope: scope(["order", 1]),
      trace: batteryTrace,
    }),
  ],
};

const flagshipTrace: Trace = {
  summary: { simulated: 2, rejected: 1, openAssumptions: 0 },
  steps: [
    { tag: "Graph", text: "Battery Pack 500Wh: 2 on hand against 10 needed; P00001 plans 10 for 20 Feb.", rejected: false },
    { tag: "Graph", text: "Drive Subassembly is made in-house and has no inbound to cover it.", rejected: false },
    { tag: "Simulated", text: "Re-promise to 20 Feb — the Drive Subassembly is still uncovered.", rejected: true },
  ],
};

/** An at-risk plan with no day count (the seeded flagship's shape): the baseline and every
 *  option serve delivery `at_risk` with a slip of 0 and no projected completion, so nothing
 *  on it may read "on time". */
export const planAtRisk: Plan = {
  subject: `${ID}/sale.order/1`,
  resolves: [`${ID}/sale.order/1`],
  lead: `${ID}/sale.order/1`,
  situation: "Battery Pack 500Wh and Drive Subassembly short before the commitment date.",
  severity: "critical",
  delivery: "at_risk",
  daySlip: 0,
  projectedCompletion: null,
  orderValue: "20700.00",
  currency: USD,
  baseline: baselineOf({ delivery: "at_risk", severity: "critical", daySlip: 0, orderValue: "20700.00" }),
  trace: flagshipTrace,
  options: [
    option({
      key: "A",
      recommended: true,
      rankKey: "00000",
      delivery: "at_risk",
      cost: "180.00",
      risk: 1,
      daySlip: 0,
      orderValue: "20700.00",
      changes: [expedite("1", "31 Jan", { from: "20 Feb", label: "P00001 · 10 × Battery Pack 500Wh" })],
      scope: scope(["purchase order", 1]),
      trace: flagshipTrace,
    }),
    option({
      key: "B",
      rankKey: "00001",
      delivery: "at_risk",
      cost: "0.00",
      risk: 2,
      orderValue: "20700.00",
      changes: [change("notify_owner", `${ID}/sale.order/1`, "task for account owner", { label: "S00001 · VeloMax Retail" })],
      scope: scope(["order", 1]),
      trace: flagshipTrace,
    }),
  ],
};

export const plans: Plan[] = [planWithLanes, planNoLanes];

/** The fixtures' "now" (epoch ms): just after the later auto-applied change, inside both
 *  reversal windows, so both are listed with Undo open. */
export const FIXTURE_NOW = 1798900200 * 1000;

/** A fixture hash of `text`: 8 hex digits from a simple string hash, for fixture ids and
 *  codes only. The real ids and display codes are served by the backend; the frontend does
 *  not compute one. */
function fixtureHash(text: string): string {
  let h = 5381;
  for (const ch of text) h = (Math.imul(h, 33) ^ ch.charCodeAt(0)) >>> 0;
  return h.toString(16).padStart(8, "0");
}

/** A fixture display code (four upper-case hex digits) for `text`. */
function fixtureCode(text: string): string {
  return fixtureHash(text).slice(-4).toUpperCase();
}

/** A point a plan resolves. */
export function pointOf(subject: string, { code, state = "proposed", reason = "at risk" }: { code?: string; state?: PointState; reason?: string } = {}): ResolvedPoint {
  return { id: `action-point/${subject}`, subject, state, reason, code: code ?? fixtureCode(subject) };
}

const MINUTE = 60;

/** The point codes of the fixture plans (AP-0233, AP-0239, AP-0231). */
const CODE: Readonly<Record<string, string>> = {
  [`${ID}/sale.order/147`]: "0233",
  [`${ID}/mrp.production/318`]: "0239",
  [`${ID}/sale.order/142`]: "0231",
};

/** The row of a plan: PL-0412 (a portfolio, proposed 14 min ago) for `planWithLanes` and
 *  PL-0409 (a single point, proposed 2 h ago) for `planNoLanes`. Any other plan is a
 *  single-point plan over its lead with a fixture id. */
export function entryOf(plan: Plan, over: Partial<PlanEntry> = {}): PlanEntry {
  const nowSecs = FIXTURE_NOW / 1000;
  const points = plan.resolves.map((subject) => pointOf(subject, CODE[subject] ? { code: CODE[subject] } : {}));
  const base: PlanEntry =
    plan.subject === planWithLanes.subject
      ? { id: "plan/0412e7c1d9a3", code: "0412", resolves: points, portfolio: { id: "portfolio/0412e7c1d9a3", generation: 2 }, openedAt: String(nowSecs - 14 * MINUTE), situation: plan.situation }
      : plan.subject === planNoLanes.subject
        ? { id: "plan/0409d2b6f4e8", code: "0409", resolves: points, portfolio: null, openedAt: String(nowSecs - 120 * MINUTE), situation: plan.situation }
        : {
            id: `plan/${fixtureHash(plan.resolves.join("\n"))}`,
            code: fixtureCode(plan.resolves.join("\n")),
            resolves: points,
            portfolio: plan.resolves.length > 1 ? { id: `portfolio/${fixtureHash(plan.resolves.join("\n"))}`, generation: 1 } : null,
            openedAt: null,
            situation: plan.situation,
          };
  return { ...base, ...over };
}

/** Plans as inbox rows: a `plan` row when it has options, else `no_option`. */
export function planItems(list: Plan[]): InboxItem[] {
  return list.map((plan) => itemOf(plan));
}

/** One plan as an inbox row, its entry overridable. */
export function itemOf(plan: Plan, over: Partial<PlanEntry> = {}): InboxItem {
  const entry = entryOf(plan, over);
  return plan.options.length > 0 ? { kind: "plan", entry, plan } : { kind: "no_option", entry, plan };
}

/** A plan whose slate carries no option: every model attempt was set aside, and the trace
 *  (with its Generation steps) says why. */
export const planNoOption: Plan = {
  subject: `${ID}/sale.order/160`,
  resolves: [`${ID}/sale.order/160`],
  lead: `${ID}/sale.order/160`,
  situation: "Frame Kit short for the promise; no inbound purchase order covers it.",
  severity: "high",
  delivery: "late",
  daySlip: 6,
  projectedCompletion: "2027-02-10T00:00:00",
  orderValue: "9400.00",
  currency: USD,
  baseline: baselineOf({ delivery: "late", severity: "high", projectedCompletion: "2027-02-10T00:00:00", daySlip: 6, orderValue: "9400.00" }),
  trace: {
    summary: { simulated: 1, rejected: 2, openAssumptions: 0 },
    steps: [
      { tag: "Simulated", text: "do-nothing baseline: delivery=late", rejected: false },
      { tag: "Generation", text: "attempt 1: rate_limited — cognition endpoint 429 Too Many Requests", rejected: true },
      { tag: "Generation", text: "attempt 2: refused — action ExpeditePo is missing required param \"new_date\"", rejected: true },
    ],
  },
  options: [],
};

/** A proposed point whose slate has not been attached yet. */
export const pendingItem: InboxItem = {
  kind: "pending",
  entry: {
    id: "plan/0171aa9b8c7d",
    code: "0171",
    resolves: [pointOf(`${ID}/sale.order/171`, { code: "0171", reason: "Gear Set inbound slips past the promise." })],
    portfolio: null,
    openedAt: null,
    situation: "Gear Set inbound slips past the promise.",
  },
};

/** An open point whose plan read failed. */
export const unavailableItem: InboxItem = {
  kind: "unavailable",
  entry: {
    id: "plan/0172bb1c2d3e",
    code: "0172",
    resolves: [pointOf(`${ID}/sale.order/172`, { code: "0172", state: "open", reason: "Paint Booth over capacity on 2 Feb." })],
    portfolio: null,
    openedAt: null,
    situation: "Paint Booth over capacity on 2 Feb.",
  },
  reason: "plan read timed out",
};

/** A portfolio plan whose manufacturing order's point is still `open`: Approve waits for it. */
export const waitingItem: InboxItem = itemOf(planWithLanes, {
  resolves: [
    pointOf(`${ID}/mrp.production/318`, { code: "0239", state: "open" }),
    pointOf(`${ID}/sale.order/147`, { code: "0233" }),
  ],
});

/** A portfolio whose joint slate is still pending (listed, resolving both points). */
export const pendingPortfolioItem: InboxItem = {
  kind: "pending",
  entry: {
    id: "plan/0415c0ffee00",
    code: "0415",
    resolves: [pointOf(`${ID}/mrp.production/320`, { code: "0240" }), pointOf(`${ID}/sale.order/148`, { code: "0241" })],
    portfolio: { id: "portfolio/0415c0ffee00", generation: 1 },
    openedAt: null,
    situation: "MO-0320 fulfils SO-0148; the Paint Booth is double-booked on 2 Feb.",
  },
};

export const autoApplied: AppliedChange[] = [
  {
    actionPoint: `${ID}/action.point/238`,
    subject: `${ID}/sale.order/151`,
    code: "0117",
    actionPointCode: "0238",
    changes: [expedite("95", "6 Feb", { inverse: "restore 10 Feb", from: "10 Feb", label: "P00095 · 80 × Gear Set" })],
    criteria: [
      { rule: "cost_within_limit", value: "150.00" },
      { rule: "no_promised_date_move", value: "true" },
      { rule: "single_step_reversible", value: "true" },
      { rule: "supplier_confirmed", value: "PartsHub · 10:58" },
    ],
    appliedAt: "1798899840",
    undoUntil: "1798928640",
    reversible: true,
    title: "Expedite",
    effect: "Expedite P00095 · 80 × Gear Set → 6 Feb",
  },
  {
    actionPoint: `${ID}/action.point/236`,
    subject: `${ID}/sale.order/150`,
    code: "0115",
    actionPointCode: "0236",
    changes: [change("notify_owner", `${ID}/purchase.order/94`, "task for account owner", { inverse: "cancel task", label: "P00094 · FrameCo Metals" })],
    criteria: [
      { rule: "notify_only", value: "mail.activity" },
      { rule: "cost_within_limit", value: "0.00" },
      { rule: "no_promised_date_move", value: "true" },
      { rule: "single_step_reversible", value: "true" },
    ],
    appliedAt: "1798894200",
    undoUntil: "1798923000",
    reversible: true,
    title: "Notify",
    effect: "Notify P00094 · FrameCo Metals → task for account owner",
  },
];

export const policy: Policy = {
  policyId: "local-autopromote/v1",
  costLimitCurrency: USD,
  criteria: [
    { rule: "cost_within_limit", value: "250.00" },
    { rule: "no_promised_date_move", value: "true" },
    { rule: "single_step_reversible", value: "true" },
    { rule: "supplier_confirmed", value: "purchase" },
  ],
  undoWindow: "reversible for 8h after apply",
  autoApplyActions: ["expedite_po", "notify_owner"],
};

/** The empty state: no plans awaiting oversight. */
export const emptyPlans: Plan[] = [];


/** The header KPIs for the two-plan fixture: the derived count plus the sourced
 *  revenue-at-risk (in its currency) and late orders, now and if approved. */
export const kpis: InboxKpis = {
  waiting: plans.length,
  revenueAtRisk: "39200.00",
  currency: USD,
  lateOrdersNow: "2",
  lateOrders: "0",
};

/** The plans the operator approved today: PL-0412 approved 10 min before "now" (window
 *  open), and a single-point plan whose undo reversed one of its two changes. */
export const approved: ApprovedPlan[] = [
  {
    id: "plan/0398f1e2d3c4",
    code: "0398",
    resolves: [pointOf(`${ID}/mrp.production/310`, { code: "0229", state: "promoted" }), pointOf(`${ID}/sale.order/139`, { code: "0228", state: "promoted" })],
    portfolio: { id: "portfolio/0398f1e2d3c4", generation: 1 },
    rankKey: "00000",
    changes: planWithLanes.options[0]?.changes.map((c) => ({ ...c, inverse: "restore" })) ?? [],
    reviewer: "Dana Ops",
    rationale: "Split and expedite",
    promotedAt: String(FIXTURE_NOW / 1000 - 10 * MINUTE),
    undoUntil: String(FIXTURE_NOW / 1000 + 470 * MINUTE),
    reversible: true,
    remaining: 4,
  },
  {
    id: "plan/0406c8d9e0f1",
    code: "0406",
    resolves: [pointOf(`${ID}/sale.order/140`, { code: "0226", state: "promoted" })],
    portfolio: null,
    rankKey: "00001",
    changes: [
      change("re_promise_date", `${ID}/sale.order/140`, "22 Feb", { inverse: "restore 29 Jan", from: "29 Jan", label: "S00140 · VeloMax Retail" }),
      change("notify_owner", `${ID}/sale.order/140`, "task for account owner", { inverse: "cancel task", label: "S00140 · VeloMax Retail" }),
    ],
    reviewer: "Dana Ops",
    rationale: "Re-promise",
    promotedAt: String(FIXTURE_NOW / 1000 - 30 * MINUTE),
    undoUntil: String(FIXTURE_NOW / 1000 + 450 * MINUTE),
    reversible: true,
    remaining: 1,
  },
];

// --- the PL-0412 plan as the live read serves it: ids, ISO dates, `1.0` numbers ---

const MO_318 = `${ID}/mrp.production/318`;
const MO_319 = `${ID}/mrp.production/319`;
const SO_147 = `${ID}/sale.order/147`;
const WC_ASSEMBLY = `${ID}/mrp.workcenter/1`;
const WC_PAINT = `${ID}/mrp.workcenter/2`;
const WO_3181 = `${ID}/mrp.workorder/3181`;
const WO_3191 = `${ID}/mrp.workorder/3191`;
const WO_3192 = `${ID}/mrp.workorder/3192`;
const CAP_1 = `${ID}/mrp.workcenter.capacity/1`;
const LINE_91 = `${ID}/purchase.order.line/91`;
const CITY_S1 = `${ID}/product.product/12`;

/** The labels the read serves for the live-shaped plan's ids. */
export const liveLabels: ReadonlyMap<string, string> = new Map([
  [MO_318, "WH/MO/00318"],
  [MO_319, "WH/MO/00319"],
  [SO_147, "S00147 · Stadtrad Leasing"],
  [WC_ASSEMBLY, "Assembly Line"],
  [WC_PAINT, "Paint Booth"],
  [WO_3181, "WH/MO/00318 · Assemble drive + frame"],
  [WO_3191, "WH/MO/00319 · Assemble drive + frame"],
  [WO_3192, "WH/MO/00319 · Paint frame"],
  [CAP_1, "Assembly Line capacity · City S1"],
  [LINE_91, "P00091 · 50 × Motor Controller"],
  [CITY_S1, "City S1"],
]);

const liveTrace: Trace = {
  summary: { simulated: 3, rejected: 1, openAssumptions: 0 },
  steps: [
    { tag: "Graph", text: `Linked: ${MO_318}, ${MO_319}, ${SO_147} — contend for work center ${WC_ASSEMBLY}.`, rejected: false },
    { tag: "Graph", text: `read ${MO_318} ekg:produced ${CITY_S1}`, rejected: false },
    { tag: "Simulated", text: "Do nothing: 3 of 3 points still at risk · 2 orders late (15 units) · 2 over-capacity overlaps", rejected: false },
    { tag: "Simulated", text: "Candidate: 3 of 3 points still at risk · 2 orders late (15 units) · 2 over-capacity overlaps", rejected: true },
    { tag: "Simulated", text: "Candidate: 0 of 3 points still at risk · no order late · no over-capacity overlap", rejected: false },
  ],
};

/** A change as the live read serves it: no served label, raw from and to values. */
function liveChange(verb: string, object: string, key: string, value: string, from: string): Change {
  return { verb, object, params: [{ key, value }], inverse: null, from, label: null };
}

/** The PL-0412 plan as the live read serves it: its situation, trace, changes and lanes carry
 *  entity ids, its dates are ISO and its numbers `1.0`, and two of its actions carry the same
 *  not-pre-approved need. The view shows every id by its label. */
export const planWithIds: Plan = {
  ...planWithLanes,
  situation: `linked: ${MO_318} and ${MO_319} contend for work center ${WC_ASSEMBLY} beyond its capacity 1; ${SO_147} ships late.`,
  trace: liveTrace,
  options: [
    option({
      key: "A",
      recommended: true,
      rankKey: "00000",
      delivery: "on_time",
      cost: "0.00",
      risk: 0,
      projectedCompletion: "2027-01-31T00:00:00",
      daySlip: 0,
      orderValue: "18500.00",
      needs: [
        ["not_allowlisted", 'Action type "reassign_work_center" is not in the auto-promotion allowlist'],
        ["not_allowlisted", 'Action type "reassign_work_center" is not in the auto-promotion allowlist'],
        ["not_allowlisted", 'Action type "raise_work_center_capacity" is not in the auto-promotion allowlist'],
      ],
      changes: [
        liveChange("raise_work_center_capacity", CAP_1, "capacity", "3.0", "1.0"),
        liveChange("reassign_work_center", WO_3181, "work_center", WC_PAINT, WC_ASSEMBLY),
        liveChange("reassign_work_center", WO_3191, "work_center", WC_PAINT, WC_ASSEMBLY),
        liveChange("expedite_po", LINE_91, "new_date", "2027-01-11 12:00:00", "2027-02-24T12:00:00"),
      ],
      scope: scope(["order", 2], ["work center", 2], ["purchase order", 1]),
      lanes: [
        {
          workCenter: WC_ASSEMBLY,
          capacity: 1,
          before: [bar(WO_3181, "01-25", "01-29", 0), bar(WO_3191, "01-27", "01-30", 1)],
          after: [bar(WO_3191, "01-27", "01-30", 0)],
        },
        {
          workCenter: WC_PAINT,
          capacity: 1,
          before: [bar(WO_3192, "01-29", "01-30", 0)],
          after: [bar(WO_3181, "01-25", "01-29", 0, true), bar(WO_3192, "01-29", "01-30", 0)],
        },
      ],
      trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: liveTrace.steps.slice(-1) },
    }),
  ],
};

/** A policy that auto-applies expedites only, so a capacity raise and reassigns need you. */
export const expediteOnlyPolicy: Policy = { ...policy, autoApplyActions: ["expedite_po"] };
