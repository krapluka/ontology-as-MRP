import { describe, expect, test } from "vitest";
import { render } from "@testing-library/react";
import { GraphTab } from "./GraphTab.tsx";
import { mapGraph, type RowMap } from "../../data/tabMapping.ts";

const ID = "https://ekg.example/id";
const EKG = "https://ekg.example/ontology#";
const SO = `${ID}/sale.order/1`;
const OTHER_SO = `${ID}/sale.order/2`;
const CUSTOMER = `${ID}/res.partner/12`;
const PARTNER = `${ID}/res.partner/7`;
const PO = `${ID}/purchase.order/1`;

// The row shape the read surface serves for a vendor that is also another order's customer
// (the store's RDFS inference types it both): one node row per role, and each edge end
// resolved to the role its predicate names.
const rows: RowMap[] = [
  { kind: "node", node: CUSTOMER, role: "Customer", on_path: "true" },
  { kind: "node", node: PARTNER, role: "Customer", on_path: "false" },
  { kind: "node", node: SO, role: "SalesOrder", on_path: "true" },
  { kind: "node", node: OTHER_SO, role: "SalesOrder", on_path: "false" },
  { kind: "node", node: PARTNER, role: "Supplier", on_path: "true" },
  { kind: "node", node: PO, role: "PurchaseOrder", on_path: "true" },
  { kind: "edge", s: SO, p: `${EKG}for_customer`, o: CUSTOMER, on_path: "true" },
  { kind: "edge", s: OTHER_SO, p: `${EKG}for_customer`, o: PARTNER, on_path: "false" },
  { kind: "edge", s: PO, p: `${EKG}from_supplier`, o: PARTNER, on_path: "true" },
];

describe("GraphTab: a partner typed both Customer and Supplier", () => {
  test("draws one box per role and ends each edge at the role its predicate names", () => {
    const { container } = render(<GraphTab view={mapGraph(rows)} subject={SO} />);
    const boxes = [...container.querySelectorAll<HTMLElement>(`[data-entity="${PARTNER}"]`)];
    // Customer column (0 %) dimmed, Supplier column (4 times 17.2 %) lit.
    expect(boxes.map((b) => [b.style.left, b.dataset["path"]])).toEqual([
      ["0%", "off"],
      ["68.8%", "on"],
    ]);
    // The lit from_supplier edge leaves the Supplier box's right side (68.8 + 14 = 82.8),
    // not the Customer column.
    const lit = container.querySelector('[data-testid="graph-lit-edges"]')?.getAttribute("d") ?? "";
    expect(lit).toContain("M82.8 ");
    expect(container.querySelector('[data-testid="graph-lit-edges"]')?.getAttribute("data-count")).toBe("2");
  });
});
