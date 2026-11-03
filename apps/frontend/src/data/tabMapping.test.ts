import { describe, expect, test } from "vitest";
import type { ReadModelQuery } from "./graphql/graphql.ts";
import { graphEndpointRole, mapFlow, mapGraph, mapSuppliers, mapTimeline, toRowMaps } from "./tabMapping.ts";

const ID = "https://ekg.example/id";
const EKG = "https://ekg.example/ontology#";

/** Builds a `readModel` rows payload from plain key/value maps (the neutral row shape). */
function rows(...maps: Array<Record<string, string>>): ReadModelQuery["readModel"]["rows"] {
  return maps.map((m) => ({ fields: Object.entries(m).map(([key, value]) => ({ key, value })) }));
}

describe("mapTimeline", () => {
  test("maps every promised order and the plant's inbound lines with their markers", () => {
    const view = mapTimeline(
      toRowMaps(
        rows(
          { entity: `${ID}/sale.order/1`, kind: "promise", date: "2027-01-27T12:00:00", label: "S00001", order_value: "20700.0", at_risk: "true", on_path: "true", cause: "false" },
          { entity: `${ID}/sale.order/2`, kind: "promise", date: "2027-01-27T12:00:00", label: "S00002", at_risk: "false", on_path: "false" },
          { entity: `${ID}/sale.order/1`, kind: "ready", date: "2027-01-31T12:00:00" },
          { entity: `${ID}/purchase.order/1`, kind: "inbound", date: "2027-02-20T12:00:00", label: "P00001", product: `${ID}/product.product/4`, quantity: "10.0", supplier: `${ID}/res.partner/6`, on_path: "true", cause: "true" },
        ),
      ),
    );
    expect(view.orders).toHaveLength(2);
    const order = view.orders[0];
    expect(order).toMatchObject({ label: "S00001", promise: "2027-01-27T12:00:00", ready: "2027-01-31T12:00:00", orderValue: "20700.0", atRisk: true, onPath: true });
    expect(view.orders[1]).toMatchObject({ atRisk: false, onPath: false, orderValue: null, ready: null });
    expect(view.inbound).toEqual([
      { entity: `${ID}/purchase.order/1`, label: "P00001", date: "2027-02-20T12:00:00", product: `${ID}/product.product/4`, quantity: 10, supplier: `${ID}/res.partner/6`, onPath: true, cause: true },
    ]);
  });

  test("keeps at_risk and on_path unserved (null) when the rows omit them, never false", () => {
    const view = mapTimeline(toRowMaps(rows({ entity: `${ID}/sale.order/1`, kind: "promise", date: "2027-01-27T12:00:00" })));
    expect(view.orders[0]?.atRisk).toBeNull();
    expect(view.orders[0]?.onPath).toBeNull();
  });

  test("ignores a row missing its entity or kind, or of an unknown kind", () => {
    const view = mapTimeline(toRowMaps(rows({ kind: "promise", date: "2027-01-31T00:00:00" }, { entity: `${ID}/x/1`, kind: "other", date: "2027-01-31T00:00:00" })));
    expect(view.orders).toHaveLength(0);
    expect(view.inbound).toHaveLength(0);
  });
});

describe("mapGraph", () => {
  test("maps typed nodes (role, label, markers) and directed edges with on_path", () => {
    const view = mapGraph(
      toRowMaps(
        rows(
          { kind: "node", node: `${ID}/sale.order/1`, role: "SalesOrder", label: "S00001", at_risk: "true", on_path: "true", cause: "false" },
          { kind: "node", node: `${ID}/product.product/4`, role: "Component", label: "Battery", at_risk: "false", on_path: "true", cause: "true" },
          { kind: "node", node: `${ID}/res.partner/12`, role: "Customer", at_risk: "false", on_path: "false", cause: "false" },
          { kind: "edge", s: `${ID}/sale.order/1`, p: `${EKG}for_customer`, o: `${ID}/res.partner/12`, on_path: "false" },
        ),
      ),
    );
    expect(view.nodes).toHaveLength(3);
    expect(view.nodes[0]).toEqual({ node: `${ID}/sale.order/1`, role: "SalesOrder", label: "S00001", atRisk: true, onPath: true, cause: false });
    expect(view.nodes[1]?.cause).toBe(true);
    expect(view.nodes[2]?.label).toBeNull();
    expect(view.edges).toEqual([{ source: `${ID}/sale.order/1`, predicate: `${EKG}for_customer`, target: `${ID}/res.partner/12`, onPath: false }]);
  });

  test("an omitted at_risk / on_path / cause maps to unserved (null), not false", () => {
    const view = mapGraph(
      toRowMaps(
        rows(
          { kind: "node", node: `${ID}/product.product/4`, role: "Component" },
          { kind: "edge", s: `${ID}/res.partner/6`, p: `${EKG}supplies_product`, o: `${ID}/product.product/4` },
        ),
      ),
    );
    expect(view.nodes[0]).toMatchObject({ atRisk: null, onPath: null, cause: null });
    expect(view.edges[0]?.onPath).toBeNull();
  });

  test("de-duplicates a node by IRI and role, keeping the first label and a served true marker", () => {
    const view = mapGraph(
      toRowMaps(
        rows(
          { kind: "node", node: `${ID}/sale.order/1`, role: "SalesOrder", label: "S00001", on_path: "false" },
          { kind: "node", node: `${ID}/sale.order/1`, role: "SalesOrder", on_path: "true" },
        ),
      ),
    );
    expect(view.nodes).toHaveLength(1);
    expect(view.nodes[0]?.label).toBe("S00001");
    expect(view.nodes[0]?.onPath).toBe(true);
    expect(view.nodes[0]?.atRisk).toBeNull();
  });

  test("keeps a partner typed both Customer and Supplier as two nodes, each with its own markers", () => {
    const partner = `${ID}/res.partner/7`;
    const view = mapGraph(
      toRowMaps(
        rows(
          { kind: "node", node: partner, role: "Customer", label: "Both GmbH", on_path: "true" },
          { kind: "node", node: partner, role: "Supplier", label: "Both GmbH", on_path: "false" },
        ),
      ),
    );
    expect(view.nodes.map((n) => [n.role, n.onPath])).toEqual([
      ["Customer", true],
      ["Supplier", false],
    ]);
  });
});

describe("graphEndpointRole", () => {
  test("names the partner role a predicate gives an edge end, else none", () => {
    expect(graphEndpointRole(`${EKG}for_customer`, "object")).toBe("Customer");
    expect(graphEndpointRole(`${EKG}supplies_product`, "subject")).toBe("Supplier");
    expect(graphEndpointRole(`${EKG}from_supplier`, "object")).toBe("Supplier");
    expect(graphEndpointRole(`${EKG}for_customer`, "subject")).toBeNull();
    expect(graphEndpointRole(`${EKG}supplies_product`, "object")).toBeNull();
    expect(graphEndpointRole(`${EKG}component_product`, "object")).toBeNull();
  });
});

describe("mapFlow", () => {
  test("maps each stage's tile facts, with in_stock as the component product", () => {
    const view = mapFlow(
      toRowMaps(
        rows(
          { entity: `${ID}/res.partner/6`, stage: "suppliers", label: "CellTech GmbH", lead_time: "45", sourcing: "single", on_path: "true", at_risk: "false", cause: "false" },
          { entity: `${ID}/purchase.order/1`, stage: "on_order", label: "P00001", supplier: `${ID}/res.partner/6`, product: `${ID}/product.product/4`, quantity: "10.0", arrival: "2027-02-20T12:00:00", on_path: "true", cause: "true" },
          { entity: `${ID}/product.product/4`, stage: "in_stock", label: "Battery", on_hand: "2.0", safety: "5.0", on_path: "true" },
          { entity: `${ID}/mrp.workorder/1`, stage: "in_production", lifecycle_state: "ready", part_of: `${ID}/mrp.production/1` },
          { entity: `${ID}/sale.order/1`, stage: "promised", label: "S00001", commitment_date: "2027-01-27T12:00:00", order_value: "20700.0", at_risk: "true", on_path: "true" },
        ),
      ),
    );
    const [sup, po, stock, wo, order] = view.entities;
    expect(sup).toMatchObject({ stage: "suppliers", leadTimeDays: 45, sourcing: "single", onPath: true });
    expect(po).toMatchObject({ supplier: `${ID}/res.partner/6`, product: `${ID}/product.product/4`, quantity: 10, arrival: "2027-02-20T12:00:00", cause: true });
    expect(stock).toMatchObject({ stage: "in_stock", onHand: 2, safety: 5 });
    expect(wo).toMatchObject({ lifecycleState: "ready", partOf: `${ID}/mrp.production/1`, label: null });
    expect(order).toMatchObject({ commitmentDate: "2027-01-27T12:00:00", orderValue: "20700.0", atRisk: true });
    expect(view.labels[`${ID}/product.product/4`]).toBe("Battery");
  });

  test("an omitted on_path / at_risk maps to unserved (null), not false", () => {
    const view = mapFlow(toRowMaps(rows({ entity: `${ID}/res.partner/6`, stage: "suppliers" })));
    expect(view.entities[0]).toMatchObject({ onPath: null, atRisk: null, cause: null, sourcing: null });
  });

  test("drops a row whose stage is not one of the five known columns", () => {
    const view = mapFlow(toRowMaps(rows({ entity: `${ID}/x/1`, stage: "unknown" })));
    expect(view.entities).toHaveLength(0);
  });
});

describe("mapSuppliers", () => {
  const supplier = `${ID}/res.partner/6`;

  test("maps the summary (sourcing, otd, exposure) and groups detail rows by supplier", () => {
    const view = mapSuppliers(
      toRowMaps(
        rows(
          { kind: "supplier", supplier, label: "CellTech GmbH", lead_time: "45", otd: "1.0000", exposure: "20700.00", sourcing: "single", on_path: "true" },
          { kind: "open_po", supplier, po: `${ID}/purchase.order/1`, label: "P00001", product: `${ID}/product.product/4`, quantity: "10.0", arrival: "2027-02-20T12:00:00", on_path: "true", cause: "true" },
          { kind: "stock", supplier, stock_of: `${ID}/product.product/4`, label: "Battery", on_hand: "2.0", safety: "5.0", demand: "10.00", sourcing: "single", on_path: "true", cause: "true" },
          { kind: "dependent_order", supplier, order: `${ID}/sale.order/1`, label: "S00001", order_value: "20700.0", commitment_date: "2027-01-27T12:00:00", at_risk: "true", on_path: "true" },
        ),
      ),
    );
    expect(view.summaries[0]).toEqual({ supplier, label: "CellTech GmbH", leadTimeDays: 45, otd: 1, exposure: "20700.00", sourcing: "single", onPath: true });
    const detail = view.detailBySupplier[supplier];
    expect(detail?.openPos[0]).toMatchObject({ po: `${ID}/purchase.order/1`, quantity: 10, arrival: "2027-02-20T12:00:00", cause: true });
    expect(detail?.stock[0]).toMatchObject({ onHand: 2, safety: 5, demand: 10, sourcing: "single" });
    expect(detail?.dependentOrders[0]).toMatchObject({ label: "S00001", commitmentDate: "2027-01-27T12:00:00", atRisk: true });
    expect(view.labels[`${ID}/product.product/4`]).toBe("Battery");
  });

  test("omits otd, exposure and demand when unserved (never a fabricated 0)", () => {
    const view = mapSuppliers(
      toRowMaps(
        rows(
          { kind: "supplier", supplier, lead_time: "21" },
          { kind: "stock", supplier, stock_of: `${ID}/product.product/4`, on_hand: "2.0" },
        ),
      ),
    );
    expect(view.summaries[0]).toMatchObject({ otd: null, exposure: null, sourcing: null, onPath: null, leadTimeDays: 21 });
    expect(view.detailBySupplier[supplier]?.stock[0]).toMatchObject({ demand: null, safety: null });
  });

  test("an omitted at_risk / on_path on a dependent order maps to unserved (null)", () => {
    const view = mapSuppliers(toRowMaps(rows({ kind: "dependent_order", supplier, order: `${ID}/sale.order/1` })));
    expect(view.detailBySupplier[supplier]?.dependentOrders[0]).toMatchObject({ atRisk: null, onPath: null });
  });
});
