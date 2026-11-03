import { describe, expect, test } from "vitest";
import type { Marker, SupplierSummary } from "../../viewmodel/types.ts";
import { defaultSupplier } from "./defaultSupplier.ts";

function s(supplier: string, onPath: Marker, exposure: string | null): SupplierSummary {
  return { supplier, label: supplier, leadTimeDays: null, otd: null, exposure, sourcing: null, onPath };
}

describe("defaultSupplier (design D8)", () => {
  test("opens on the first on-path supplier", () => {
    expect(defaultSupplier([s("a", false, "9.00"), s("b", true, "0.00"), s("c", true, "1.00")])).toMatchObject({ supplier: "b", basis: "on_path" });
  });

  test("with on_path served but none on it, opens on the top exposure (ties keep served order)", () => {
    expect(defaultSupplier([s("a", false, "0.00"), s("b", false, "5.00"), s("c", false, "5.00")])).toEqual({
      supplier: "b",
      basis: "top_exposure",
      pathServed: true,
      exposureComplete: true,
    });
  });

  test("with on_path unserved, opens on the top exposure when exposure is served", () => {
    expect(defaultSupplier([s("a", null, "0.00"), s("b", null, "7.00")])).toEqual({
      supplier: "b",
      basis: "top_exposure",
      pathServed: false,
      exposureComplete: true,
    });
  });

  test("with neither on_path nor exposure served, opens on the first supplier", () => {
    expect(defaultSupplier([s("a", null, null), s("b", null, null)])).toMatchObject({ supplier: "a", basis: "first", pathServed: false });
    expect(defaultSupplier([s("a", false, null), s("b", false, null)])).toMatchObject({ supplier: "a", basis: "first", pathServed: true });
  });

  test("an omitted exposure is unknown: it outranks a known 0, never the lowest", () => {
    expect(defaultSupplier([s("a", false, "0.00"), s("b", false, null), s("c", false, "0.00")])).toEqual({
      supplier: "b",
      basis: "top_exposure",
      pathServed: true,
      exposureComplete: false,
    });
  });

  test("an omitted exposure cannot be ranked against a known positive: served order decides", () => {
    expect(defaultSupplier([s("a", false, "0.00"), s("b", false, "3.00"), s("c", false, null)])).toMatchObject({ supplier: "b", exposureComplete: false });
    expect(defaultSupplier([s("a", false, "0.00"), s("c", false, null), s("b", false, "3.00")])).toMatchObject({ supplier: "c", exposureComplete: false });
  });

  test("no rows, no pick", () => {
    expect(defaultSupplier([])).toBeNull();
  });
});
