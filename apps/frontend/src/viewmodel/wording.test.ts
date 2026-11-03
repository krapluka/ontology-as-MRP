import { describe, expect, test } from "vitest";
import {
  VERB_LABEL,
  appliedLabel,
  changeValue,
  collectIds,
  labelText,
  needChips,
  splitIds,
  clockTime,
  headerClock,
  localDayStart,
  needText,
  planLabel,
  planOptionTitle,
  pointLabel,
  proposedAgo,
  riskWord,
  scopeLine,
  traceSummaryLine,
  verbLabel,
  verbsTitle,
} from "./wording.ts";
import type { Policy } from "./types.ts";

// The wording rules: every word derived from a served code or value by a fixed rule; an
// unknown code shown as served; an unserved value omitted.

const policy: Policy = {
  policyId: "p",
  criteria: [{ rule: "cost_within_limit", value: "250.00" }],
  costLimitCurrency: "USD",
  undoWindow: "",
  autoApplyActions: [],
};

describe("ids", () => {
  test("a plan, a point and an applied change read as PL-, AP- and AU- plus their served code", () => {
    expect(planLabel("0412")).toBe("PL-0412");
    expect(pointLabel("0233")).toBe("AP-0233");
    expect(appliedLabel("0117")).toBe("AU-0117");
  });
});

describe("scope", () => {
  test("the PL-0412 scope reads in the fixed kind order", () => {
    expect(
      scopeLine([
        { kind: "order", count: 2 },
        { kind: "work center", count: 2 },
        { kind: "purchase order", count: 1 },
      ]),
    ).toBe("2 orders · 2 work centers · 1 PO");
  });

  test("a scope served in another order is normalised; singulars read singular", () => {
    expect(
      scopeLine([
        { kind: "order", count: 1 },
        { kind: "purchase order", count: 1 },
        { kind: "work center", count: 1 },
      ]),
    ).toBe("1 order · 1 work center · 1 PO");
    expect(scopeLine([{ kind: "purchase order", count: 3 }])).toBe("3 PO");
  });

  test("an unknown kind follows as served; an empty scope reads nothing", () => {
    expect(scopeLine([{ kind: "shipment", count: 2 }, { kind: "order", count: 1 }])).toBe("1 order · 2 shipment");
    expect(scopeLine([])).toBe("");
  });
});

describe("verbs", () => {
  test("every verb code of the command surface has a label; an unknown code is shown as served", () => {
    expect(verbLabel("split_production")).toBe("Split");
    expect(verbLabel("expedite_po")).toBe("Expedite");
    expect(verbLabel("re_promise_date")).toBe("Re-promise");
    expect(verbLabel("reschedule_work_order")).toBe("Move");
    expect(verbLabel("raise_work_center_capacity")).toBe("Raise capacity");
    expect(verbLabel("notify_owner")).toBe("Notify");
    expect(verbLabel("reassign_work_center")).toBe("Reassign");
    expect(Object.keys(VERB_LABEL)).toHaveLength(7);
    expect(verbLabel("teleport")).toBe("teleport");
    expect(verbsTitle([{ verb: "split_production" }, { verb: "expedite_po" }, { verb: "split_production" }])).toBe("Split · Expedite");
  });
});

describe("needs", () => {
  const option = { cost: "320.00", costCurrency: "USD" };

  test("a cost over the limit reads in words, both amounts in the served currency", () => {
    expect(needText({ code: "cost_over_limit", detail: "cost 320.00 exceeds the limit 250.00" }, option, policy)).toBe(
      "$320 is above the $250 limit",
    );
    expect(needText({ code: "cost_over_limit", detail: "x" }, { cost: "320.00", costCurrency: "EUR" }, { ...policy, costLimitCurrency: "EUR" })).toBe(
      "€320 is above the €250 limit",
    );
  });

  test("a cost need without a served limit, cost or common currency shows its served detail", () => {
    const need = { code: "cost_over_limit", detail: "cost 320.00 exceeds the limit 250.00" };
    expect(needText(need, option, null)).toBe("Cost 320.00 exceeds the limit 250.00");
    expect(needText(need, { cost: null, costCurrency: "USD" }, policy)).toBe("Cost 320.00 exceeds the limit 250.00");
    expect(needText(need, { cost: "320.00", costCurrency: "EUR" }, policy)).toBe("Cost 320.00 exceeds the limit 250.00");
  });

  test("the other known codes read in words; an unknown code shows its detail", () => {
    expect(needText({ code: "promised_date_move", detail: "moves a customer promised date" }, option, policy)).toBe("Moves a promised date");
    expect(needText({ code: "structural_change", detail: "splits a manufacturing order" }, option, policy)).toBe("Splits a manufacturing order");
    expect(needText({ code: "structural_change", detail: "raises a work center's capacity" }, option, policy)).toBe("Raises capacity");
    expect(needText({ code: "supplier_unconfirmed", detail: "state sent" }, option, policy)).toBe("The supplier has not confirmed");
    expect(needText({ code: "brand_new", detail: "a brand new reason" }, option, policy)).toBe("A brand new reason");
  });
});

describe("words and times", () => {
  test("risk bars read Low / Medium / High", () => {
    expect([1, 2, 3].map(riskWord)).toEqual(["Low", "Medium", "High"]);
    expect(riskWord(0)).toBeNull();
    expect(riskWord(null)).toBeNull();
  });

  test("the opened time reads relative to now, and nothing when unserved", () => {
    const now = 1_798_900_200_000;
    expect(proposedAgo(String(now / 1000 - 14 * 60), now)).toBe("proposed 14 min ago");
    expect(proposedAgo(String(now / 1000 - 2 * 3600), now)).toBe("proposed 2 h ago");
    expect(proposedAgo(String(now / 1000 - 3 * 86_400), now)).toBe("proposed 3 d ago");
    expect(proposedAgo(String(now / 1000 - 10), now)).toBe("proposed just now");
    expect(proposedAgo(null, now)).toBeNull();
    expect(proposedAgo("soon", now)).toBeNull();
  });

  test("the header clock and a local wall-clock time", () => {
    const at = new Date(2027, 0, 3, 14, 32);
    expect(headerClock(at.getTime())).toBe("Sun 3 Jan · 14:32");
    expect(clockTime(String(at.getTime() / 1000))).toBe("14:32");
    expect(clockTime(null)).toBeNull();
    expect(localDayStart(at.getTime())).toBe(new Date(2027, 0, 3).getTime() / 1000);
  });

  test("plan copy: the option title and the trace summary", () => {
    expect(planOptionTitle("A", 4)).toBe("Plan A · 4 changes");
    expect(planOptionTitle("B", 1)).toBe("Plan B · 1 change");
    expect(traceSummaryLine({ simulated: 5, rejected: 2, openAssumptions: 1 })).toBe("5 plans simulated · 2 rejected · 1 open assumption");
    expect(traceSummaryLine({ simulated: 4, rejected: 2, openAssumptions: 0 })).toBe("4 plans simulated · 2 rejected · no open assumptions");
  });
});

// --- entity ids, values and needs ---

const MO = "https://ekg.example/id/mrp.production/1";
const WC = "https://ekg.example/id/mrp.workcenter/1";

describe("ids in a served text", () => {
  test("are found with the generic pattern, a trailing . or : left to the sentence", () => {
    expect(collectIds(`linked: ${MO} and ${WC}.`)).toEqual([MO, WC]);
    expect(collectIds(`Linked: ${MO}: late`)).toEqual([MO]);
    expect(collectIds(`read ${MO} ekg:produced ${WC}`)).toEqual([MO, WC]);
    expect(collectIds("no ids here")).toEqual([]);
    expect(splitIds(`for ${WC}.`)).toEqual([
      { kind: "text", text: "for " },
      { kind: "id", id: WC },
      { kind: "text", text: "." },
    ]);
  });

  test("read as their labels, else an entity's short form, and a non-entity URL stays as served", () => {
    const labels = new Map([[MO, "WH/MO/00001"]]);
    expect(labelText(`linked: ${MO} and ${WC}.`, labels)).toBe("linked: WH/MO/00001 and mrp.workcenter/1.");
    expect(labelText("see https://docs.example/page for more", labels)).toBe("see https://docs.example/page for more");
  });
});

describe("a change value", () => {
  test("a date-time reads as day and month, with the time when both sides fall on the same day", () => {
    expect(changeValue("2027-02-24T12:00:00", "2027-01-11 12:00:00")).toBe("24 Feb");
    expect(changeValue("2027-01-11 12:00:00", "2027-02-24T12:00:00")).toBe("11 Jan");
    expect(changeValue("2027-01-17T08:00:00", "2027-01-17 14:30:00")).toBe("17 Jan 08:00");
    expect(changeValue("2027-01-17 14:30:00", "2027-01-17T08:00:00")).toBe("17 Jan 14:30");
    expect(changeValue("2027-01-17T08:00:00", null)).toBe("17 Jan");
  });

  test("a whole number drops its trailing .0, and anything else is shown as served", () => {
    expect(`${changeValue("1.0", "3.0")} → ${changeValue("3.0", "1.0")}`).toBe("1 → 3");
    expect(changeValue("-2.00", null)).toBe("-2");
    expect(changeValue("2.5", null)).toBe("2.5");
    expect(changeValue("30 now + 10 on 29 Jan", null)).toBe("30 now + 10 on 29 Jan");
    expect(changeValue(WC, MO)).toBe(WC);
  });
});

describe("need chips", () => {
  const gated = (verb: string) => ({ code: "not_allowlisted", detail: `Action type "${verb}" is not in the auto-promotion allowlist` });
  const changes = [
    { verb: "reassign_work_center", object: MO, params: [], inverse: null, from: null, label: null },
    { verb: "reassign_work_center", object: WC, params: [], inverse: null, from: null, label: null },
    { verb: "raise_work_center_capacity", object: WC, params: [], inverse: null, from: null, label: null },
    { verb: "expedite_po", object: WC, params: [], inverse: null, from: null, label: null },
  ];
  const option = { cost: null, costCurrency: null, changes };
  const needs = [gated("reassign_work_center"), gated("reassign_work_center"), gated("raise_work_center_capacity")];

  test("two reassigns and a capacity raise read as ONE not-pre-approved chip naming the verbs", () => {
    const chips = needChips(needs, option, { ...policy, autoApplyActions: ["expedite_po"] });
    expect(chips.map((c) => c.text)).toEqual(["Not pre-approved: Reassign · Raise capacity"]);
    expect(chips[0]?.detail.split("\n")).toHaveLength(2);
  });

  test("without a served policy the details are shown once each; no chip is ever shown twice", () => {
    expect(needChips(needs, option, null).map((c) => c.text)).toEqual([
      'Action type "reassign_work_center" is not in the auto-promotion allowlist',
      'Action type "raise_work_center_capacity" is not in the auto-promotion allowlist',
    ]);
    const twice = [
      { code: "promised_date_move", detail: "moves a promised date (a)" },
      { code: "promised_date_move", detail: "moves a promised date (b)" },
    ];
    expect(needChips(twice, option, policy).map((c) => c.text)).toEqual(["Moves a promised date"]);
  });
});
