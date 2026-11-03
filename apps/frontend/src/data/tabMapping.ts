/**
 * The per-tab view-model mapping: where the neutral `readModel(subject, tab)` rows become
 * typed props for each impact tab. A field the row carries is rendered; a field the row omits
 * maps to `null` or is dropped. Every row value is a string; IRIs come through full and the
 * presentational layer shortens them.
 *
 * The keys each mapper reads are declared below as typed constants per tab. They are the
 * frontend half of the row vocabulary the backend exports as `tab-rows.json`.
 * `tabVocabulary.test.ts` checks every constant (and every fixture row) against that
 * snapshot, so a renamed or dropped key fails the tests instead of rendering nothing.
 */
import type { ReadModelQuery } from "./graphql/graphql.ts";
import type {
  FlowEntity,
  FlowStage,
  FlowView,
  GraphEdge,
  GraphNode,
  GraphView,
  Marker,
  Sourcing,
  SupplierDetail,
  SuppliersView,
  SupplierSummary,
  TimelineInbound,
  TimelineOrder,
  TimelineView,
} from "../viewmodel/types.ts";

/** One opaque projection row as a key/value map (the contract enumerates no field by name). */
export type RowMap = Readonly<Record<string, string>>;

// --- the read vocabulary ---

/** The marker keys every entity row may carry (graph edges carry only `on_path`). */
export const MARKER_KEYS = ["at_risk", "on_path", "cause"] as const;

/** The keys the Timeline mapper reads. */
export const TIMELINE_KEYS = [
  "entity",
  "kind",
  "date",
  "label",
  "order_value",
  "product",
  "quantity",
  "supplier",
  ...MARKER_KEYS,
] as const;
/** The Timeline `kind`s the mapper understands. */
export const TIMELINE_KINDS = ["promise", "ready", "inbound"] as const;

/** The keys the Graph mapper reads. */
export const GRAPH_KEYS = ["kind", "node", "role", "label", "s", "p", "o", ...MARKER_KEYS] as const;
/** The Graph row `kind`s. */
export const GRAPH_KINDS = ["node", "edge"] as const;
/** The capability roles the Graph places in its six columns. */
export const GRAPH_ROLES = ["Customer", "SalesOrder", "Product", "BOM", "Component", "Supplier", "PurchaseOrder"] as const;

/** The keys the Flow mapper reads. */
export const FLOW_KEYS = [
  "entity",
  "stage",
  "label",
  "lead_time",
  "sourcing",
  "supplier",
  "product",
  "quantity",
  "arrival",
  "on_hand",
  "safety",
  "lifecycle_state",
  "part_of",
  "commitment_date",
  "order_value",
  ...MARKER_KEYS,
] as const;
/** The five Flow stages, in column order. */
export const FLOW_STAGES = ["suppliers", "on_order", "in_stock", "in_production", "promised"] as const satisfies ReadonlyArray<FlowStage>;

/** The keys the Suppliers mapper reads. */
export const SUPPLIERS_KEYS = [
  "kind",
  "supplier",
  "label",
  "lead_time",
  "otd",
  "exposure",
  "sourcing",
  "po",
  "product",
  "quantity",
  "arrival",
  "stock_of",
  "on_hand",
  "safety",
  "demand",
  "order",
  "order_value",
  "commitment_date",
  ...MARKER_KEYS,
] as const;
/** The Suppliers row `kind`s. */
export const SUPPLIERS_KINDS = ["supplier", "open_po", "stock", "dependent_order"] as const;

/** The sourcing roles (Flow suppliers, Suppliers summary and stock rows). */
export const SOURCING_ROLES = ["single", "primary", "backup"] as const satisfies ReadonlyArray<Sourcing>;

type TimelineKey = (typeof TIMELINE_KEYS)[number];
type GraphKey = (typeof GRAPH_KEYS)[number];
type FlowKey = (typeof FLOW_KEYS)[number];
type SuppliersKey = (typeof SUPPLIERS_KEYS)[number];
type MarkerKey = (typeof MARKER_KEYS)[number];

// --- row readers ---

/** Folds a `readModel` response's rows into plain key/value maps. */
export function toRowMaps(rows: ReadModelQuery["readModel"]["rows"]): RowMap[] {
  return rows.map((r) => {
    const map: Record<string, string> = {};
    for (const f of r.fields) map[f.key] = f.value;
    return map;
  });
}

/** A reader bound to one tab's key vocabulary, so a mapper can only read a declared key. */
function reader<TKey extends string>(row: RowMap) {
  return {
    /** The value, or null when the row does not serve it. */
    str: (key: TKey): string | null => row[key] ?? null,
    /** The value as a finite number, or null when unserved/unparseable. */
    num: (key: TKey): number | null => {
      const raw = row[key];
      if (raw === undefined) return null;
      const n = Number(raw);
      return Number.isFinite(n) ? n : null;
    },
    /** A tri-state marker: "true" is true, "false" is false, absent (or anything else) is
     *  null, meaning not served. It is not coerced into a negative. */
    marker: (key: TKey & MarkerKey): Marker => {
      const raw = row[key];
      return raw === "true" ? true : raw === "false" ? false : null;
    },
    sourcing: (key: TKey & "sourcing"): Sourcing | null => {
      const raw = row[key];
      return (SOURCING_ROLES as ReadonlyArray<string>).includes(raw ?? "") ? (raw as Sourcing) : null;
    },
  };
}

/** Combine the same marker served on two rows about one entity: a served `true` wins, then a
 *  served `false`; unserved only when neither row serves it. */
function mergeMarker(a: Marker, b: Marker): Marker {
  if (a === true || b === true) return true;
  if (a === false || b === false) return false;
  return null;
}

// --- Timeline ---

/** Maps the Timeline rows: every promised order (its `promise` tick and, when served, its
 *  `ready` date) plus the plant's open inbound lines (one `inbound` row per PO and product). */
export function mapTimeline(rows: RowMap[]): TimelineView {
  const byEntity = new Map<string, TimelineOrder>();
  const inbound: TimelineInbound[] = [];

  for (const row of rows) {
    const r = reader<TimelineKey>(row);
    const entity = r.str("entity");
    const kind = r.str("kind");
    const date = r.str("date");
    if (entity === null || kind === null) continue;

    if (kind === "inbound") {
      if (date === null) continue;
      inbound.push({
        entity,
        label: r.str("label"),
        date,
        product: r.str("product"),
        quantity: r.num("quantity"),
        supplier: r.str("supplier"),
        onPath: r.marker("on_path"),
        cause: r.marker("cause"),
      });
      continue;
    }
    if (kind !== "promise" && kind !== "ready") continue;

    const existing = byEntity.get(entity);
    const order: TimelineOrder = existing ?? {
      entity,
      label: null,
      promise: null,
      ready: null,
      orderValue: null,
      atRisk: null,
      onPath: null,
    };
    if (kind === "promise") order.promise = date;
    else order.ready = date;
    order.label = order.label ?? r.str("label");
    order.orderValue = order.orderValue ?? r.str("order_value");
    order.atRisk = mergeMarker(order.atRisk, r.marker("at_risk"));
    order.onPath = mergeMarker(order.onPath, r.marker("on_path"));
    if (!existing) byEntity.set(entity, order);
  }

  return { orders: [...byEntity.values()], inbound };
}

// --- Graph ---

/** A Graph node's identity: its IRI and its role. A partner typed both Customer and
 *  Supplier is served as two node rows with the same IRI, one per role, in two columns. */
export function graphNodeKey(node: string, role: string | null): string {
  return `${node}|${role ?? ""}`;
}

/** Which end of a Graph edge (its stored RDF subject or object). */
export type GraphEdgeSide = "subject" | "object";

/**
 * The role a Graph edge's predicate gives one of its ends, where that end can be a partner
 * typed both Customer and Supplier: `for_customer` points at the Customer, `supplies_product`
 * starts from the Supplier, `from_supplier` points at the Supplier. Every other end has one
 * role (null). The backend resolves an edge end's `on_path` by the same rule.
 */
export function graphEndpointRole(predicate: string, side: GraphEdgeSide): "Customer" | "Supplier" | null {
  const local = predicate.split(/[#/]/).at(-1) ?? predicate;
  if (local === "for_customer" && side === "object") return "Customer";
  if ((local === "supplies_product" && side === "subject") || (local === "from_supplier" && side === "object")) return "Supplier";
  return null;
}

/** Maps the Graph rows into typed nodes (`kind=node`: node, role, label, markers) and
 *  directed edges (`kind=edge`: s, p, o in stored RDF direction, `on_path`). Nodes
 *  de-duplicate by IRI and role (a dual-role partner stays two nodes); the first served label
 *  is kept. */
export function mapGraph(rows: RowMap[]): GraphView {
  const nodes = new Map<string, GraphNode>();
  const edges: GraphEdge[] = [];

  for (const row of rows) {
    const r = reader<GraphKey>(row);
    const kind = r.str("kind");
    if (kind === "node") {
      const node = r.str("node");
      if (node === null) continue;
      const role = r.str("role");
      const key = graphNodeKey(node, role);
      const existing = nodes.get(key);
      nodes.set(key, {
        node,
        role,
        label: existing?.label ?? r.str("label"),
        atRisk: mergeMarker(existing?.atRisk ?? null, r.marker("at_risk")),
        onPath: mergeMarker(existing?.onPath ?? null, r.marker("on_path")),
        cause: mergeMarker(existing?.cause ?? null, r.marker("cause")),
      });
    } else if (kind === "edge") {
      const source = r.str("s");
      const predicate = r.str("p");
      const target = r.str("o");
      if (source === null || predicate === null || target === null) continue;
      edges.push({ source, predicate, target, onPath: r.marker("on_path") });
    }
  }

  return { nodes: [...nodes.values()], edges };
}

// --- Flow ---

function isFlowStage(stage: string): stage is FlowStage {
  return (FLOW_STAGES as ReadonlyArray<string>).includes(stage);
}

/** Maps the Flow rows into placed entities with their tile facts. A row whose stage is not
 *  one of the five known columns is dropped. `in_stock` rows are component products carrying
 *  `on_hand` / `safety`. */
export function mapFlow(rows: RowMap[]): FlowView {
  const entities: FlowEntity[] = [];
  const labels: Record<string, string> = {};
  for (const row of rows) {
    const r = reader<FlowKey>(row);
    const entity = r.str("entity");
    const stage = r.str("stage");
    if (entity === null || stage === null || !isFlowStage(stage)) continue;
    const label = r.str("label");
    if (label !== null) labels[entity] = label;
    entities.push({
      entity,
      stage,
      label,
      atRisk: r.marker("at_risk"),
      onPath: r.marker("on_path"),
      cause: r.marker("cause"),
      leadTimeDays: r.num("lead_time"),
      sourcing: r.sourcing("sourcing"),
      supplier: r.str("supplier"),
      product: r.str("product"),
      quantity: r.num("quantity"),
      arrival: r.str("arrival"),
      onHand: r.num("on_hand"),
      safety: r.num("safety"),
      lifecycleState: r.str("lifecycle_state"),
      partOf: r.str("part_of"),
      commitmentDate: r.str("commitment_date"),
      orderValue: r.str("order_value"),
    });
  }
  return { entities, labels };
}

// --- Suppliers ---

/** Gets (creating if needed) a supplier's detail bundle. */
function detailFor(map: Record<string, SupplierDetail>, supplier: string): SupplierDetail {
  const existing = map[supplier];
  if (existing) return existing;
  const created: SupplierDetail = { openPos: [], stock: [], dependentOrders: [] };
  map[supplier] = created;
  return created;
}

/** Maps the Suppliers rows into the lead / sourcing / on-time / exposure summary table
 *  plus the per-supplier detail (open PO lines, stock vs safety vs demand, dependent orders),
 *  grouped by supplier IRI, in served row order. An unserved field stays null (`otd` without
 *  receipts, `exposure` without an at-risk decision, `demand` with no order in the window). */
export function mapSuppliers(rows: RowMap[]): SuppliersView {
  const summaries: SupplierSummary[] = [];
  const detailBySupplier: Record<string, SupplierDetail> = {};
  const labels: Record<string, string> = {};

  for (const row of rows) {
    const r = reader<SuppliersKey>(row);
    const kind = r.str("kind");
    const supplier = r.str("supplier");
    if (supplier === null) continue;
    const label = r.str("label");

    if (kind === "supplier") {
      if (label !== null) labels[supplier] = label;
      summaries.push({
        supplier,
        label,
        leadTimeDays: r.num("lead_time"),
        otd: r.num("otd"),
        exposure: r.str("exposure"),
        sourcing: r.sourcing("sourcing"),
        onPath: r.marker("on_path"),
      });
    } else if (kind === "open_po") {
      const po = r.str("po");
      if (po !== null && label !== null) labels[po] = label;
      detailFor(detailBySupplier, supplier).openPos.push({
        supplier,
        po,
        label,
        product: r.str("product"),
        quantity: r.num("quantity"),
        arrival: r.str("arrival"),
        onPath: r.marker("on_path"),
        cause: r.marker("cause"),
      });
    } else if (kind === "stock") {
      const stockOf = r.str("stock_of");
      if (stockOf !== null && label !== null) labels[stockOf] = label;
      detailFor(detailBySupplier, supplier).stock.push({
        supplier,
        stockOf,
        label,
        onHand: r.num("on_hand"),
        safety: r.num("safety"),
        demand: r.num("demand"),
        sourcing: r.sourcing("sourcing"),
        onPath: r.marker("on_path"),
        cause: r.marker("cause"),
      });
    } else if (kind === "dependent_order") {
      const order = r.str("order");
      if (order !== null && label !== null) labels[order] = label;
      detailFor(detailBySupplier, supplier).dependentOrders.push({
        supplier,
        order,
        label,
        orderValue: r.str("order_value"),
        commitmentDate: r.str("commitment_date"),
        atRisk: r.marker("at_risk"),
        onPath: r.marker("on_path"),
      });
    }
  }

  return { summaries, detailBySupplier, labels };
}
