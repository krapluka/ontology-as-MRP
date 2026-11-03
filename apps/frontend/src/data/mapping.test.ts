import { describe, expect, test } from "vitest";
import { ActionPointState, TraceTag } from "./graphql/graphql.ts";
import type { PlanFieldsFragment } from "./graphql/graphql.ts";
import { deriveLabel, deriveOutcome, mapAppliedChange, mapApproved, mapEntry, mapKpis, mapPlan, mapPolicy } from "./mapping.ts";

const seededPlan: PlanFieldsFragment = {
  subject: "https://ekg.example/id/sale.order/147",
  resolves: ["https://ekg.example/id/mrp.production/318", "https://ekg.example/id/sale.order/147"],
  lead: "https://ekg.example/id/sale.order/147",
  baseline: {
    rankKey: "99999",
    metrics: [
      { key: "delivery", value: "late" },
      { key: "severity", value: "critical" },
      { key: "projected_completion", value: "2027-02-24T00:00:00" },
      { key: "day_slip", value: "24" },
      { key: "order_value", value: "20700.00" },
      { key: "currency", value: "USD" },
      { key: "late_orders", value: "1" },
    ],
  },
  trace: {
    summary: { simulated: 5, rejected: 2, openAssumptions: 1 },
    steps: [
      { tag: TraceTag.Graph, text: "read", rejected: false },
      { tag: TraceTag.Simulated, text: "a cheaper candidate left shortfall", rejected: true },
    ],
  },
  options: [
    {
      recommended: true,
      rankKey: "00000",
      needs: [{ code: "cost_over_limit", detail: "cost 320.00 exceeds the limit 250.00" }],
      changes: [
        { verb: "split_production", object: "https://ekg.example/id/mrp.production/318", inverse: null, from: "40× City S1", label: null, params: [{ key: "new_value", value: "30+10" }] },
        {
          verb: "expedite_po",
          object: "https://ekg.example/id/purchase.order.line/91",
          inverse: null,
          from: null,
          label: "P00001 · 10 × Battery Pack 500Wh",
          params: [{ key: "new_date", value: "29 Jan" }],
        },
      ],
      metrics: [
        { key: "delivery", value: "on_time" },
        { key: "cost", value: "320.00" },
        { key: "cost_currency", value: "USD" },
        { key: "severity", value: "high" },
        { key: "projected_completion", value: "2027-01-31T00:00:00" },
        { key: "day_slip", value: "0" },
        { key: "order_value", value: "20700.00" },
        { key: "currency", value: "USD" },
      ],
      scope: [
        { kind: "order", count: 1 },
        { kind: "purchase order", count: 1 },
      ],
      lanes: [
        {
          workCenter: "https://ekg.example/id/mrp.workcenter/assembly",
          capacity: 2,
          before: [{ workOrder: "S1", start: "2027-01-27", end: "2027-01-30", row: 1, changed: false }],
          after: [{ workOrder: "S1", start: "2027-01-26", end: "2027-01-28", row: 1, changed: true }],
        },
      ],
      trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: [] },
    },
    {
      recommended: false,
      rankKey: "00001",
      needs: [],
      changes: [{ verb: "re_promise_date", object: "https://ekg.example/id/sale.order/147", inverse: null, params: [] }],
      metrics: [
        { key: "delivery", value: "late" },
        { key: "day_slip", value: "24" },
      ],
      scope: [],
      lanes: [],
      trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: [] },
    },
    {
      recommended: false,
      rankKey: "00002",
      needs: [],
      changes: [{ verb: "notify_owner", object: "https://ekg.example/id/sale.order/147", inverse: null, from: null, params: [] }],
      // Slip 0 with delivery at_risk: not "On time".
      metrics: [
        { key: "delivery", value: "at_risk" },
        { key: "day_slip", value: "0" },
      ],
      scope: [{ kind: "order", count: 1 }],
      lanes: [],
      trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: [] },
    },
  ],
};

describe("mapPlan", () => {
  const plan = mapPlan(seededPlan, "Controllers arrive a day late; Assembly over capacity.");

  test("carries sourced fields straight through", () => {
    expect(plan.subject).toBe(seededPlan.subject);
    expect(plan.resolves).toHaveLength(2);
    expect(plan.lead).toBe("https://ekg.example/id/sale.order/147");
    expect(plan.situation).toMatch(/Controllers arrive/);
    expect(plan.options).toHaveLength(3);
    expect(plan.options[0]!.recommended).toBe(true);
    expect(plan.options[0]!.changes[0]!.verb).toBe("split_production");
    expect(plan.options[0]!.scope).toEqual([
      { kind: "order", count: 1 },
      { kind: "purchase order", count: 1 },
    ]);
  });

  test("derives the slate key A/B/C from rank order", () => {
    expect(plan.options.map((o) => o.key)).toEqual(["A", "B", "C"]);
  });

  test("derives the option label from distinct change verbs, worded by the verb map", () => {
    expect(plan.options[0]!.label).toBe("Split · Expedite");
    expect(plan.options[1]!.label).toBe("Re-promise");
    expect(plan.options[2]!.label).toBe("Notify");
    // A code the map does not know is shown as served.
    expect(deriveLabel([{ verb: "teleport" }, { verb: "expedite_po" }])).toBe("teleport · Expedite");
  });

  test("derives the Delivery label from the served delivery and slip", () => {
    expect(plan.options.map((o) => o.delivery)).toEqual(["on_time", "late", "at_risk"]);
    expect(plan.options[0]!.outcome).toBe("On time");
    // Late: the positive slip against the original promise.
    expect(plan.options[1]!.outcome).toBe("24 d late");
    // Slip 0 with delivery at_risk reads "At risk", not "On time".
    expect(plan.options[2]!.daySlip).toBe(0);
    expect(plan.options[2]!.outcome).toBe("At risk");
  });

  test("maps a generation step (a failed model attempt) as set aside", () => {
    const failed = mapPlan(
      {
        ...seededPlan,
        trace: {
          summary: { simulated: 1, rejected: 1, openAssumptions: 0 },
          steps: [{ tag: TraceTag.Generation, text: "attempt 1: rate_limited — 429", rejected: true }],
        },
      },
      "",
    );
    expect(failed.trace.steps).toEqual([{ tag: "Generation", text: "attempt 1: rate_limited — 429", rejected: true }]);
  });

  test("maps trace tags and retains rejected candidates", () => {
    expect(plan.trace.steps.map((s) => s.tag)).toEqual(["Graph", "Simulated"]);
    expect(plan.trace.steps.some((s) => s.rejected)).toBe(true);
    expect(plan.trace.summary.rejected).toBe(2);
  });

  test("maps before/after lanes and the moved flag", () => {
    const lane = plan.options[0]!.lanes[0]!;
    expect(lane.capacity).toBe(2);
    expect(lane.after[0]!.changed).toBe(true);
  });

  test("sources each change's from→to (from + params), omitting an unserved from", () => {
    expect(plan.options[0]!.changes[0]!.from).toBe("40× City S1");
    expect(plan.options[0]!.changes[0]!.params[0]!.value).toBe("30+10");
    // A change the contract serves no from for maps to null.
    expect(plan.options[0]!.changes[1]!.from).toBeNull();
  });

  test("sources each change's label, null when unserved (the view then shows the id)", () => {
    expect(plan.options[0]!.changes[1]!.label).toBe("P00001 · 10 × Battery Pack 500Wh");
    expect(plan.options[0]!.changes[0]!.label).toBeNull();
    // An older response without the field maps to null too.
    expect(plan.options[1]!.changes[0]!.label).toBeNull();
  });

  test("sources the option display framing from the opaque bag", () => {
    const option = plan.options[0]!;
    expect(option.severity).toBe("high");
    expect(option.projectedCompletion).toBe("2027-01-31T00:00:00");
    expect(option.daySlip).toBe(0);
    expect(option.cost).toBe("320.00");
    expect(option.costCurrency).toBe("USD");
    expect(option.orderValue).toBe("20700.00");
    expect(option.currency).toBe("USD");
    // A capability that serves no such key maps to null (option B has no severity metric).
    expect(plan.options[1]!.severity).toBeNull();
    expect(plan.options[1]!.orderValue).toBeNull();
    expect(plan.options[1]!.currency).toBeNull();
    expect(plan.options[1]!.costCurrency).toBeNull();
  });

  test("sources the plan framing from the do-nothing baseline, not the recommended option", () => {
    expect(plan.severity).toBe("critical");
    expect(plan.delivery).toBe("late");
    // The baseline's slip (the row's DAYS LATE), although the recommended option is on time.
    expect(plan.daySlip).toBe(24);
    expect(plan.options[0]!.daySlip).toBe(0);
    expect(plan.projectedCompletion).toBe("2027-02-24T00:00:00");
    expect(plan.orderValue).toBe("20700.00");
    expect(plan.currency).toBe("USD");
  });

  test("an at-risk baseline with slip 0 and no completion keeps its at_risk delivery", () => {
    const atRisk = mapPlan(
      {
        ...seededPlan,
        baseline: {
          rankKey: "99999",
          metrics: [
            { key: "delivery", value: "at_risk" },
            { key: "day_slip", value: "0" },
            { key: "order_value", value: "20700.00" },
          ],
        },
      },
      "reason",
    );
    expect(atRisk.delivery).toBe("at_risk");
    expect(atRisk.daySlip).toBe(0);
    expect(atRisk.projectedCompletion).toBeNull();
    // An order value served without a currency keeps a null currency (rendered plain).
    expect(atRisk.currency).toBeNull();
  });

  test("never fabricates a value the contract does not serve", () => {
    // With a bag that carries none of the framing keys, every derived display field is null.
    const bare = mapPlan(
      {
        ...seededPlan,
        baseline: { rankKey: "99999", metrics: [] },
        options: [{ ...seededPlan.options[0]!, metrics: [] }],
      },
      "reason",
    );
    expect(bare.severity).toBeNull();
    expect(bare.delivery).toBeNull();
    expect(bare.daySlip).toBeNull();
    expect(bare.projectedCompletion).toBeNull();
    expect(bare.orderValue).toBeNull();
    expect(bare.currency).toBeNull();
    expect(bare.options[0]!.severity).toBeNull();
    expect(bare.options[0]!.projectedCompletion).toBeNull();
    expect(bare.options[0]!.daySlip).toBeNull();
    expect(bare.options[0]!.delivery).toBeNull();
    expect(bare.options[0]!.outcome).toBeNull();
  });
});

describe("derive helpers", () => {
  test("deriveOutcome omits (null) an unserved delivery metric", () => {
    expect(deriveOutcome([{ key: "cost", value: "10" }])).toBeNull();
    expect(deriveOutcome([{ key: "delivery", value: "weird" }])).toBeNull();
    // A slip alone, with no served delivery, is not labelled.
    expect(deriveOutcome([{ key: "day_slip", value: "0" }])).toBeNull();
  });

  test("deriveOutcome labels each served outcome", () => {
    expect(deriveOutcome([{ key: "delivery", value: "on_time" }, { key: "day_slip", value: "-2" }])).toBe("On time");
    expect(deriveOutcome([{ key: "delivery", value: "late" }, { key: "day_slip", value: "29" }])).toBe("29 d late");
    expect(deriveOutcome([{ key: "delivery", value: "late" }])).toBe("Late");
    expect(deriveOutcome([{ key: "delivery", value: "at_risk" }])).toBe("At risk");
    expect(deriveOutcome([{ key: "delivery", value: "at_risk" }, { key: "day_slip", value: "0" }])).toBe("At risk");
  });

  test("deriveLabel falls back when there are no changes", () => {
    expect(deriveLabel([])).toBe("No changes");
  });
});

describe("mapAppliedChange / mapPolicy / mapKpis", () => {
  test("maps an auto-applied change, its window, and the sourced title/effect", () => {
    const applied = mapAppliedChange({
      actionPoint: "https://ekg.example/id/action.point/238",
      subject: "https://ekg.example/id/sale.order/151",
      code: "0117",
      actionPointCode: "0238",
      changes: [
        {
          verb: "expedite_po",
          object: "https://ekg.example/id/purchase.order.line/95",
          inverse: "restore 10 Feb",
          from: "10 Feb",
          label: "P00095 · 80 × Gear Set",
          params: [],
        },
      ],
      criteria: [{ rule: "cost_within_limit", value: "150.00" }],
      appliedAt: "1798899840",
      undoUntil: "1798928640",
      reversible: true,
      metrics: [
        { key: "title", value: "Expedite" },
        { key: "effect", value: "Expedite purchase.order.line/95 → 6 Feb" },
      ],
    });
    expect(applied.reversible).toBe(true);
    expect(applied.code).toBe("0117");
    expect(applied.actionPointCode).toBe("0238");
    expect(applied.changes[0]!.inverse).toBe("restore 10 Feb");
    expect(applied.changes[0]!.from).toBe("10 Feb");
    expect(applied.changes[0]!.label).toBe("P00095 · 80 × Gear Set");
    expect(applied.undoUntil).toBe("1798928640");
    // The composed title/effect come from the opaque bag.
    expect(applied.title).toBe("Expedite");
    expect(applied.effect).toBe("Expedite purchase.order.line/95 → 6 Feb");
  });

  test("mapKpis folds the sourced revenue/late-order KPIs in with the derived count", () => {
    const kpis = mapKpis(2, [
      { key: "late_orders_now", value: "2" },
      { key: "late_orders", value: "0" },
      { key: "revenue_at_risk", value: "39200.00" },
      { key: "currency", value: "USD" },
    ]);
    expect(kpis.waiting).toBe(2);
    expect(kpis.revenueAtRisk).toBe("39200.00");
    expect(kpis.currency).toBe("USD");
    expect(kpis.lateOrdersNow).toBe("2");
    expect(kpis.lateOrders).toBe("0");
    // An empty headline list serves no revenue/late value: null, not 0.
    const none = mapKpis(0, []);
    expect(none.revenueAtRisk).toBeNull();
    expect(none.currency).toBeNull();
    expect(none.lateOrdersNow).toBeNull();
    expect(none.lateOrders).toBeNull();
  });

  test("maps the policy verbatim, with its limit currency", () => {
    const policy = mapPolicy({
      policyId: "local-autopromote/v1",
      criteria: [{ rule: "cost_within_limit", value: "250.00" }],
      costLimitCurrency: "EUR",
      undoWindow: "reversible for 8h after apply",
      autoApplyActions: ["expedite_po"],
    });
    expect(policy.policyId).toBe("local-autopromote/v1");
    expect(policy.autoApplyActions).toEqual(["expedite_po"]);
    expect(policy.costLimitCurrency).toBe("EUR");
    // Unserved: null, not a default currency.
    expect(mapPolicy({ policyId: "p", criteria: [], undoWindow: "", autoApplyActions: [] }).costLimitCurrency).toBeNull();
  });
});

const point = (n: number, state: ActionPointState, code: string) => ({
  id: `action-point/https://ekg.example/id/sale.order/${n}`,
  subject: `https://ekg.example/id/sale.order/${n}`,
  state,
  reason: `reason ${n}`,
  code,
});

describe("mapEntry / mapApproved", () => {
  test("maps a listed plan's row fields: id, code, points with states and codes, portfolio, opened time", () => {
    const entry = mapEntry({
      id: "plan/0412e7c1d9a3",
      code: "0412",
      resolves: [point(1, ActionPointState.Proposed, "0233"), point(2, ActionPointState.Open, "0239")],
      portfolio: { id: "portfolio/0412e7c1d9a3", generation: 2 },
      openedAt: "1798899360",
      situation: "linked",
    });
    expect(entry.id).toBe("plan/0412e7c1d9a3");
    expect(entry.code).toBe("0412");
    expect(entry.resolves.map((p) => [p.state, p.code])).toEqual([
      ["proposed", "0233"],
      ["open", "0239"],
    ]);
    expect(entry.portfolio).toEqual({ id: "portfolio/0412e7c1d9a3", generation: 2 });
    expect(entry.openedAt).toBe("1798899360");
    // A single point's plan has no portfolio, and an unserved time maps to null.
    const lone = mapEntry({ id: "plan/0409d2b6f4e8", code: "0409", resolves: [point(3, ActionPointState.Proposed, "0231")], situation: "short" });
    expect(lone.portfolio).toBeNull();
    expect(lone.openedAt).toBeNull();
  });

  test("maps an approved plan with its window and what remains", () => {
    const approved = mapApproved({
      id: "plan/0398f1e2d3c4",
      code: "0398",
      resolves: [point(4, ActionPointState.Promoted, "0228")],
      portfolio: null,
      rankKey: "00001",
      changes: [{ verb: "re_promise_date", object: "https://ekg.example/id/sale.order/4", inverse: "restore", params: [] }],
      reviewer: "Dana Ops",
      rationale: "why",
      promotedAt: "1798899200",
      undoUntil: "1798928000",
      reversible: true,
      remaining: 1,
    });
    expect(approved.code).toBe("0398");
    expect(approved.resolves[0]!.state).toBe("promoted");
    expect(approved.undoUntil).toBe("1798928000");
    expect(approved.remaining).toBe(1);
    expect(approved.changes[0]!.from).toBeNull();
  });
});
