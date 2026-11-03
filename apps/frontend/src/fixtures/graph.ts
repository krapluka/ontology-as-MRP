/**
 * Graph-tab fixtures: the recorded live projection of the seeded scenario, mapped through the
 * Graph view-model. Covers the flagship point's lit chain, an open point whose rule has
 * cleared, and the shape served when the rules cannot be read.
 */
import type { GraphView } from "../viewmodel/types.ts";
import { mapGraph } from "../data/tabMapping.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, rowsFor, rulesUnreadable } from "./scenario.ts";

export const graphRows = rowsFor(FLAGSHIP_SUBJECT, "graph");
export const graphView: GraphView = mapGraph(graphRows);
export const graphClearedView: GraphView = mapGraph(rowsFor(CLEARED_SUBJECT, "graph"));
export const graphUnservedView: GraphView = mapGraph(rulesUnreadable("graph", graphRows));

/** The flagship rows as served when no purchase order is open inbound (every PO received):
 *  the Graph draws a PO only while it is open, so the Purchase-order column is empty. Its
 *  node rows and every edge touching them are removed. */
const poNodes = new Set(graphRows.filter((r) => r["kind"] === "node" && r["role"] === "PurchaseOrder").map((r) => r["node"]));
export const graphNoOpenPoView: GraphView = mapGraph(
  graphRows.filter((r) => (r["kind"] === "node" ? r["role"] !== "PurchaseOrder" : !poNodes.has(r["s"]) && !poNodes.has(r["o"]))),
);

/** The empty Graph (a subject the projection serves no rows for). */
export const emptyGraphView: GraphView = { nodes: [], edges: [] };
