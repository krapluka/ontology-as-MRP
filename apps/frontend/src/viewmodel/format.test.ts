import { afterEach, describe, expect, test } from "vitest";
import { asDelivery, deliveryLabel, formatLocalDay, formatMoney, isToday, slipLabel, undoOpen, undoStatus } from "./format.ts";

// Node re-reads process.env.TZ on assignment, so each case pins a zone and restores it.
const originalTz = process.env["TZ"];
afterEach(() => {
  if (originalTz === undefined) delete process.env["TZ"];
  else process.env["TZ"] = originalTz;
});

describe("formatLocalDay", () => {
  test.each([
    // East of UTC: local midnight is the previous day in UTC.
    ["Europe/Berlin", "2027-01-06T00:00:00", "6 Jan"],
    // West of UTC: a late local evening is the next day in UTC.
    ["Pacific/Honolulu", "2027-01-06T20:00:00", "6 Jan"],
    ["UTC", "2027-12-31T23:59:59", "31 Dec"],
  ])("names the local day in %s", (zone, naive, day) => {
    process.env["TZ"] = zone;
    // A naive served date parses as local time, the axis the Timeline ticks live on.
    expect(formatLocalDay(Date.parse(naive))).toBe(day);
  });

  test("is null for a non-finite instant", () => {
    expect(formatLocalDay(Number.NaN)).toBeNull();
  });
});

describe("formatMoney", () => {
  test("renders a value in its served ISO currency, whole units", () => {
    expect(formatMoney("20700.00", "USD")).toBe("$20,700");
    expect(formatMoney("1450.00", "EUR")).toBe("€1,450");
  });

  test("renders a value served without a currency as a plain number (never an assumed one)", () => {
    expect(formatMoney("20700.00", null)).toBe("20,700");
    // A code the runtime does not know is not a currency we can name.
    expect(formatMoney("20700.00", "not-a-code")).toBe("20,700");
  });

  test("renders the KPI shape in compact notation", () => {
    expect(formatMoney("39200.00", "USD", { compact: true })).toBe("$39.2K");
    expect(formatMoney("74000.00", "USD", { compact: true })).toBe("$74K");
    expect(formatMoney("39200.00", null, { compact: true })).toBe("39.2K");
  });

  test("is null for an unserved or non-numeric value", () => {
    expect(formatMoney(null, "USD")).toBeNull();
    expect(formatMoney("n/a", "USD")).toBeNull();
  });
});

describe("delivery labels", () => {
  test("asDelivery reads only the outcomes the contract emits", () => {
    expect(asDelivery("on_time")).toBe("on_time");
    expect(asDelivery("late")).toBe("late");
    expect(asDelivery("at_risk")).toBe("at_risk");
    expect(asDelivery("weird")).toBeNull();
    expect(asDelivery(undefined)).toBeNull();
  });

  test.each([
    ["on_time", 0, "On time"],
    ["on_time", null, "On time"],
    ["late", 29, "29 d late"],
    ["late", null, "Late"],
    ["at_risk", null, "At risk"],
    // A slip of 0 on an at-risk plan is not "On time".
    ["at_risk", 0, "At risk"],
    [null, 0, null],
    [null, 5, null],
  ] as const)("deliveryLabel(%s, %s) gives %s", (delivery, days, label) => {
    expect(deliveryLabel(delivery, days)).toBe(label);
  });

  test.each([
    [0, "on_time", "on time"],
    [null, "on_time", "on time"],
    [24, "late", "+24 days late"],
    [1, "late", "+1 day late"],
    [null, "late", "late"],
    [null, "at_risk", "at risk"],
    // Slip 0 with delivery at_risk must not read "on time".
    [0, "at_risk", "at risk"],
    // Without a served delivery a non-positive slip says nothing (not "on time").
    [0, null, null],
    [-3, null, null],
    [4, null, "+4 days late"],
    [null, null, null],
  ] as const)("slipLabel(%s, %s) gives %s", (days, delivery, label) => {
    expect(slipLabel(days, delivery)).toBe(label);
  });
});

// --- the undo window and "today", against a fixed now ---

describe("undoOpen", () => {
  const now = 1_798_899_200_000; // a fixed "now", epoch ms
  const at = (secs: number) => String(secs);

  test("open strictly before the window's end, closed at and after it", () => {
    expect(undoOpen({ reversible: true, undoUntil: at(now / 1000 + 1) }, now)).toBe(true);
    expect(undoOpen({ reversible: true, undoUntil: at(now / 1000) }, now)).toBe(false);
    expect(undoOpen({ reversible: true, undoUntil: at(now / 1000 - 1) }, now)).toBe(false);
  });

  test("never open for a non-reversible change, or with no (or an unparseable) window", () => {
    expect(undoOpen({ reversible: false, undoUntil: at(now / 1000 + 3600) }, now)).toBe(false);
    expect(undoOpen({ reversible: true, undoUntil: null }, now)).toBe(false);
    expect(undoOpen({ reversible: true, undoUntil: "soon" }, now)).toBe(false);
  });

  test("undoStatus names the state: open until, closed at, not reversible", () => {
    expect(undoStatus({ reversible: true, undoUntil: at(now / 1000 + 60) }, now)).toMatch(/^Undo available until /);
    expect(undoStatus({ reversible: true, undoUntil: at(now / 1000 - 60) }, now)).toMatch(/^reversal window closed at /);
    expect(undoStatus({ reversible: false, undoUntil: at(now / 1000 + 60) }, now)).toBe("not reversible");
  });
});

describe("isToday", () => {
  // Local calendar instants, so the boundaries hold in every time zone.
  const midnight = new Date(2027, 0, 2, 0, 0, 0).getTime();
  const nextMidnight = new Date(2027, 0, 3, 0, 0, 0).getTime();
  const now = new Date(2027, 0, 2, 15, 30, 0).getTime();
  const secs = (ms: number) => String(ms / 1000);

  test("from local midnight (inclusive) to the next local midnight (exclusive)", () => {
    expect(isToday(secs(midnight), now)).toBe(true);
    expect(isToday(secs(midnight - 1000), now)).toBe(false);
    expect(isToday(secs(nextMidnight - 1000), now)).toBe(true);
    expect(isToday(secs(nextMidnight), now)).toBe(false);
  });

  test.each(["UTC", "Pacific/Auckland", "America/Los_Angeles"])("uses the local day in %s, not the UTC day", (tz) => {
    process.env["TZ"] = tz;
    const localNow = new Date(2027, 0, 2, 0, 30, 0).getTime();
    const lateYesterday = new Date(2027, 0, 1, 23, 30, 0).getTime();
    expect(isToday(secs(localNow - 10 * 60 * 1000), localNow)).toBe(true);
    expect(isToday(secs(lateYesterday), localNow)).toBe(false);
  });

  test("an unknown instant is never today", () => {
    expect(isToday(null, now)).toBe(false);
    expect(isToday("not a time", now)).toBe(false);
  });
});
