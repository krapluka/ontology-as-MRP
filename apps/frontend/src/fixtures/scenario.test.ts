import { describe, expect, test } from "vitest";
import type { RowMap } from "../data/tabMapping.ts";
import { FLAGSHIP_SUBJECT, recordedPoints, rowsFor, rulesUnreadable } from "./scenario.ts";

const CAUSE_DEPENDENT = new Set(["Component", "Supplier", "PurchaseOrder"]);

/** The derived rows' node `on_path` per IRI (the recorded scenario has no dual-role partner). */
function nodePaths(rows: ReadonlyArray<RowMap>): Map<string, string | undefined> {
  return new Map(rows.filter((r) => r["kind"] === "node").map((r) => [r["node"] ?? "", r["on_path"]]));
}

describe("the recorded flagship point", () => {
  test("its inbox reason names the component its chain lights (one cause order for both)", () => {
    const lit = rowsFor(FLAGSHIP_SUBJECT, "graph").filter((r) => r["kind"] === "node" && r["role"] === "Component" && r["on_path"] === "true");
    expect(lit).toHaveLength(1);
    const reason = recordedPoints.find((p) => p.subject === FLAGSHIP_SUBJECT)?.reason ?? "";
    expect(reason).toMatch(new RegExp(`${(lit[0]?.["node"] ?? "").replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`));
  });

  test("its Flow lights the fulfilling MO (the recording was taken after graphdb-derive)", () => {
    const mo = rowsFor(FLAGSHIP_SUBJECT, "flow").filter((r) => r["stage"] === "in_production" && r["on_path"] === "true");
    expect(mo.map((r) => r["entity"])).toEqual([expect.stringMatching(/\/mrp\.production\/\d+$/)]);
    expect(rowsFor(FLAGSHIP_SUBJECT, "timeline").some((r) => r["kind"] === "ready" && r["entity"] === FLAGSHIP_SUBJECT)).toBe(true);
  });
});

describe("rulesUnreadable (graph) mirrors the backend's failure rule", () => {
  const recorded = rowsFor(FLAGSHIP_SUBJECT, "graph");
  const derived = rulesUnreadable("graph", recorded);

  test("strips every risk marker and the cause-dependent nodes' on_path, keeping the rest", () => {
    expect(derived).toHaveLength(recorded.length);
    for (const row of derived) {
      expect(row).not.toHaveProperty("at_risk");
      expect(row).not.toHaveProperty("cause");
      if (row["kind"] !== "node") continue;
      if (CAUSE_DEPENDENT.has(row["role"] ?? "")) expect(row).not.toHaveProperty("on_path");
      else expect(row["on_path"]).toMatch(/^(true|false)$/);
    }
  });

  test("omits an edge's on_path only when neither end is known off the path", () => {
    const paths = nodePaths(derived);
    const edges = derived.filter((r) => r["kind"] === "edge");
    let oneEndOffOtherUnknown = 0;
    for (const edge of edges) {
      const ends = [paths.get(edge["s"] ?? ""), paths.get(edge["o"] ?? "")];
      if (ends.includes("false")) {
        if (ends.includes(undefined)) oneEndOffOtherUnknown++;
        expect(edge["on_path"], `${edge["s"]} -> ${edge["o"]}`).toBe("false");
      } else if (ends.every((e) => e === "true")) {
        expect(edge["on_path"], `${edge["s"]} -> ${edge["o"]}`).toBe("true");
      } else {
        expect(edge, `${edge["s"]} -> ${edge["o"]}`).not.toHaveProperty("on_path");
      }
    }
    // The case the backend serves as "false" (e.g. an off-path BOM to an unknown component).
    expect(oneEndOffOtherUnknown).toBeGreaterThan(0);
  });
});
