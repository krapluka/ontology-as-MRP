import type { EntitySelection, GraphNode, GraphView } from "../../viewmodel/types.ts";
import { Blueprint } from "../primitives/Blueprint.tsx";
import { isServed, labelOf } from "../../viewmodel/format.ts";
import { graphEndpointRole, graphNodeKey, type GraphEdgeSide } from "../../data/tabMapping.ts";
import { EntityBox, Kicker, PATH_NOT_SERVED, SectionLabel } from "../tabs/shared.tsx";

/** The six supply columns, left to right, with the roles each one holds. A sub-assembly is
 *  served as `Product`, so it sits in the Product · BOM column. */
const COLUMNS: ReadonlyArray<{ title: string; roles: ReadonlyArray<string> }> = [
  { title: "Customer", roles: ["Customer"] },
  { title: "Sales order", roles: ["SalesOrder"] },
  { title: "Product · BOM", roles: ["Product", "BOM"] },
  { title: "Component", roles: ["Component"] },
  { title: "Supplier", roles: ["Supplier"] },
  { title: "Purchase order", roles: ["PurchaseOrder"] },
];

/** The role's display kind (the small line above a node's name). */
const KIND: Readonly<Record<string, string>> = {
  Customer: "Customer",
  SalesOrder: "Sales order",
  Product: "Product",
  BOM: "BOM",
  Component: "Component",
  Supplier: "Supplier",
  PurchaseOrder: "Purchase order",
};

// Geometry: 6 columns at a 17.2 % pitch, nodes 14 % wide and 44 px tall.
const COL_PITCH = 17.2;
const NODE_W = 14;
const NODE_H = 44;
const NODE_GAP = 14;
/** Opacity of dimmed context nodes. The text inside keeps the full text colour; blended at
 *  0.64 over the page ground its contrast is about 4.8 : 1, which is AA with a margin for
 *  compositing rounding (0.62 would be 4.49 : 1, just under 4.5). */
const DIMMED_OPACITY = 0.64;

function columnOf(role: string | null): number {
  return role === null ? -1 : COLUMNS.findIndex((c) => c.roles.includes(role));
}

const PRODUCES = "produces_product";

/** Orders the nodes of each column. Product · BOM keeps every product followed by the BOM(s)
 *  that produce it, so the produces edge is a short vertical link. */
function layoutColumns(view: GraphView): GraphNode[][] {
  const columns: GraphNode[][] = COLUMNS.map(() => []);
  for (const node of view.nodes) {
    const i = columnOf(node.role);
    if (i >= 0) columns[i]?.push(node);
  }
  const productColumn = columns[2] ?? [];
  const bomsOf = new Map<string, string[]>();
  for (const e of view.edges) {
    if (!e.predicate.endsWith(PRODUCES)) continue;
    bomsOf.set(e.target, [...(bomsOf.get(e.target) ?? []), e.source]);
  }
  const byIri = new Map(productColumn.map((n) => [n.node, n]));
  const ordered: GraphNode[] = [];
  const placed = new Set<string>();
  for (const n of productColumn) {
    if (n.role !== "Product" || placed.has(n.node)) continue;
    ordered.push(n);
    placed.add(n.node);
    for (const bom of bomsOf.get(n.node) ?? []) {
      const b = byIri.get(bom);
      if (b && !placed.has(b.node)) {
        ordered.push(b);
        placed.add(b.node);
      }
    }
  }
  for (const n of productColumn) if (!placed.has(n.node)) ordered.push(n);
  columns[2] = ordered;
  return columns;
}

type Placed = { x: number; y: number; col: number };

/** One edge's path between two placed nodes (x in % of the width, y in px). */
function edgePath(a: Placed, b: Placed): string {
  if (a.col === b.col) {
    const [top, bottom] = a.y <= b.y ? [a, b] : [b, a];
    const x = top.x + NODE_W / 2;
    return `M${x} ${top.y + NODE_H}L${x} ${bottom.y}`;
  }
  const [left, right] = a.col < b.col ? [a, b] : [b, a];
  const x1 = left.x + NODE_W;
  const y1 = left.y + NODE_H / 2;
  const x2 = right.x;
  const y2 = right.y + NODE_H / 2;
  const dx = (x2 - x1) / 2;
  return `M${x1} ${y1}C${x1 + dx} ${y1} ${x2 - dx} ${y2} ${x2} ${y2}`;
}

function nodeLook(node: GraphNode) {
  if (node.cause === true) {
    return { border: "var(--color-accent-900)", background: "var(--color-accent-900)", color: "var(--color-bg)", opacity: 1 };
  }
  if (node.onPath === true) {
    return { border: "var(--color-accent-900)", background: "var(--color-accent-100)", color: "var(--color-text)", opacity: 1 };
  }
  if (node.onPath === false) {
    return { border: "var(--color-neutral-400)", background: "var(--color-bg)", color: "var(--color-text)", opacity: DIMMED_OPACITY };
  }
  // on_path not served: neutral, neither lit nor dimmed.
  return { border: "var(--color-neutral-500)", background: "var(--color-bg)", color: "var(--color-text)", opacity: 1 };
}

/**
 * The Graph tab: the plant-wide supply graph in six columns (Customer, Sales order,
 * Product · BOM, Component, Supplier, Purchase order). The selected point's path is lit
 * (accent-900 border, accent-100 fill) with its edges, the cause nodes are filled, and every
 * other node is dimmed context. A column whose rows carry no `on_path` says "path not served".
 * The side panel lists the on-path chain in column order.
 */
export function GraphTab({ view, subject, selection }: { view: GraphView; subject: string; selection?: EntitySelection }) {
  if (view.nodes.length === 0 && view.edges.length === 0) {
    return <p className="text-muted" style={{ fontSize: 13 }}>No graph rows for this subject.</p>;
  }

  const columns = layoutColumns(view);
  const tallest = Math.max(1, ...columns.map((c) => c.length));
  const height = tallest * NODE_H + (tallest - 1) * NODE_GAP;
  // Placed by IRI and role: a partner typed both Customer and Supplier is two boxes.
  const placed = new Map<string, Placed>();
  const placedByIri = new Map<string, Placed[]>();
  columns.forEach((nodes, col) => {
    const total = nodes.length * NODE_H + Math.max(0, nodes.length - 1) * NODE_GAP;
    const y0 = (height - total) / 2;
    nodes.forEach((n, j) => {
      const at = { x: col * COL_PITCH, y: y0 + j * (NODE_H + NODE_GAP), col };
      placed.set(graphNodeKey(n.node, n.role), at);
      placedByIri.set(n.node, [...(placedByIri.get(n.node) ?? []), at]);
    });
  });
  // An edge end is the box of the role its predicate names (a dual-role partner), else the
  // IRI's only box.
  const endOf = (iri: string, predicate: string, side: GraphEdgeSide): Placed | undefined => {
    const role = graphEndpointRole(predicate, side);
    return (role === null ? undefined : placed.get(graphNodeKey(iri, role))) ?? placedByIri.get(iri)?.[0];
  };
  const drawn = view.edges.flatMap((e) => {
    const a = endOf(e.source, e.predicate, "subject");
    const b = endOf(e.target, e.predicate, "object");
    return a && b ? [{ edge: e, a, b }] : [];
  });
  const pathOf = (edges: typeof drawn) => edges.map(({ a, b }) => edgePath(a, b)).join(" ");
  const litEdges = drawn.filter(({ edge }) => edge.onPath === true);
  const unplaced = view.nodes.filter((n) => !placed.has(graphNodeKey(n.node, n.role))).length;

  return (
    <div style={{ display: "grid", gridTemplateColumns: "minmax(0,1fr) minmax(280px,360px)", gap: "clamp(20px,2.6vw,40px)", alignItems: "start" }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 12, minWidth: 0 }}>
        <Kicker>
          Knowledge graph · {view.nodes.length} entities · {view.edges.length} relations
        </Kicker>
        <div style={{ position: "relative", height: 30 }}>
          {COLUMNS.map((c, i) => {
            const nodes = columns[i] ?? [];
            const unserved = nodes.length > 0 && !isServed(nodes.map((n) => n.onPath));
            return (
              <div
                key={c.title}
                style={{ position: "absolute", left: `${i * COL_PITCH}%`, width: `${NODE_W}%`, fontSize: 10.5, letterSpacing: ".07em", textTransform: "uppercase", color: "var(--color-neutral-700)", lineHeight: 1.3 }}
              >
                <div style={{ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{c.title}</div>
                {unserved ? <div style={{ textTransform: "none", letterSpacing: 0, fontSize: 11.5 }}>{PATH_NOT_SERVED}</div> : null}
              </div>
            );
          })}
        </div>
        <div style={{ position: "relative", width: "100%", height }}>
          <svg
            aria-hidden="true"
            viewBox={`0 0 100 ${height}`}
            preserveAspectRatio="none"
            style={{ position: "absolute", inset: 0, width: "100%", height: "100%", fill: "none", overflow: "visible" }}
          >
            <path d={pathOf(drawn.filter(({ edge }) => edge.onPath !== true))} style={{ stroke: "var(--color-neutral-400)", strokeWidth: 1, vectorEffect: "non-scaling-stroke" }} />
            <path data-testid="graph-lit-edges" data-count={litEdges.length} d={pathOf(litEdges)} style={{ stroke: "var(--color-accent-900)", strokeWidth: 2.5, vectorEffect: "non-scaling-stroke" }} />
          </svg>
          {columns.flatMap((nodes) =>
            nodes.map((node) => {
              const key = graphNodeKey(node.node, node.role);
              const at = placed.get(key);
              if (!at) return null;
              const look = nodeLook(node);
              return (
                <EntityBox
                  key={key}
                  iri={node.node}
                  path={node.onPath}
                  subject={subject}
                  selection={selection}
                  style={{
                    position: "absolute",
                    left: `${at.x}%`,
                    top: at.y,
                    width: `${NODE_W}%`,
                    height: NODE_H,
                    border: `${node.onPath === true ? 2 : 1}px solid ${look.border}`,
                    background: look.background,
                    color: look.color,
                    opacity: look.opacity,
                    padding: "3px 8px",
                    display: "flex",
                    flexDirection: "column",
                    justifyContent: "center",
                    minWidth: 0,
                  }}
                >
                  <span data-cause={node.cause === true ? "true" : undefined} style={{ display: "flex", alignItems: "center", gap: 5, fontSize: 9.5, letterSpacing: ".05em", textTransform: "uppercase", whiteSpace: "nowrap", overflow: "hidden" }}>
                    {node.atRisk === true ? (
                      <span role="img" aria-label="At risk" style={{ width: 7, height: 7, flex: "none", background: node.cause === true ? "var(--color-bg)" : "var(--color-accent-900)" }} />
                    ) : null}
                    {KIND[node.role ?? ""] ?? node.role}
                    {node.cause === true ? " · cause" : ""}
                  </span>
                  <span style={{ display: "block", fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 14, lineHeight: 1.1, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
                    {labelOf(node.node, node.label)}
                  </span>
                </EntityBox>
              );
            }),
          )}
        </div>
        {unplaced > 0 ? (
          <div style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>
            {unplaced} {unplaced === 1 ? "node has" : "nodes have"} a role outside the six columns and {unplaced === 1 ? "is" : "are"} not drawn.
          </div>
        ) : null}
      </div>

      <PathPanel view={view} columns={columns} subject={subject} isOpenPoint={selection?.selectable.has(subject) === true} />
    </div>
  );
}

/** The side panel: the selected point's status and its on-path chain in column order.
 *  `isOpenPoint` says the subject is a point the inbox lists as awaiting a decision. Only then
 *  may a cleared rule be called "still open for your decision" (an auto-applied, restored or
 *  already-decided subject is not). */
function PathPanel({ view, columns, subject, isOpenPoint }: { view: GraphView; columns: GraphNode[][]; subject: string; isOpenPoint: boolean }) {
  const riskServed = isServed(view.nodes.map((n) => n.atRisk));
  const subjectNode = view.nodes.find((n) => n.node === subject);
  const causes = view.nodes.filter((n) => n.cause === true);

  let status: string | null = null;
  if (!riskServed) status = "Risk status not served";
  else if (subjectNode?.atRisk === false) {
    status = isOpenPoint ? "The rule no longer flags this order; the point is still open for your decision." : "The rules do not currently flag this order.";
  }

  return (
    <Blueprint padding="14px 16px" style={{ display: "flex", flexDirection: "column", gap: 14, minWidth: 0 }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
        <SectionLabel>Selected point</SectionLabel>
        <div style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 22, lineHeight: 1.1 }}>
          {labelOf(subject, subjectNode?.label)}
        </div>
        {subjectNode?.atRisk === true ? (
          <span style={{ alignSelf: "flex-start", padding: "2px 9px", fontSize: 11.5, letterSpacing: ".06em", textTransform: "uppercase", background: "var(--color-accent-900)", color: "var(--color-bg)" }}>
            At risk
          </span>
        ) : null}
        {status !== null ? (
          <div role="status" style={{ fontSize: 13, lineHeight: 1.4, color: "var(--color-neutral-800)" }}>
            {status}
          </div>
        ) : null}
      </div>

      <div style={{ display: "flex", flexDirection: "column" }}>
        <SectionLabel>Path</SectionLabel>
        <ol style={{ listStyle: "none", margin: "6px 0 0", padding: 0 }}>
          {COLUMNS.map((c, i) => {
            const nodes = columns[i] ?? [];
            const onPath = nodes.filter((n) => n.onPath === true);
            // An empty column (e.g. no PO open inbound) is a known fact, not an unserved path.
            const empty = nodes.length === 0;
            const served = empty || isServed(nodes.map((n) => n.onPath));
            const last = i === COLUMNS.length - 1;
            return (
              <li key={c.title} style={{ display: "grid", gridTemplateColumns: "18px minmax(0,1fr)", gap: 10 }}>
                <span style={{ display: "flex", flexDirection: "column", alignItems: "center" }}>
                  <span
                    style={{
                      width: 11,
                      height: 11,
                      marginTop: 4,
                      flex: "none",
                      background: onPath.some((n) => n.cause === true) ? "var(--color-accent-900)" : onPath.length ? "var(--color-accent-100)" : "transparent",
                      border: `1px ${served ? "solid" : "dashed"} ${onPath.length ? "var(--color-accent-900)" : "var(--color-neutral-600)"}`,
                    }}
                  />
                  {last ? null : <span style={{ flex: 1, width: 1, background: "var(--color-divider)" }} />}
                </span>
                <span style={{ paddingBottom: 9, minWidth: 0 }}>
                  <span style={{ display: "block", fontSize: 10.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>{c.title}</span>
                  {onPath.length ? (
                    onPath.map((n) => (
                      <span key={graphNodeKey(n.node, n.role)} style={{ display: "block", fontSize: 13.5 }}>
                        <b style={{ fontWeight: 500 }}>{labelOf(n.node, n.label)}</b>
                        {n.cause === true ? <span style={{ color: "var(--color-accent-800)" }}> — cause</span> : null}
                      </span>
                    ))
                  ) : (
                    <span style={{ display: "block", fontSize: 13, color: "var(--color-neutral-700)" }}>
                      {served ? (empty ? "none in the plant" : "none on this point's path") : PATH_NOT_SERVED}
                    </span>
                  )}
                </span>
              </li>
            );
          })}
        </ol>
      </div>

      {causes.length > 0 ? (
        <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
          <SectionLabel>Cause nodes</SectionLabel>
          <div style={{ fontSize: 13 }}>{causes.map((n) => labelOf(n.node, n.label)).join(" · ")}</div>
        </div>
      ) : null}

      <div style={{ fontSize: 11.5, color: "var(--color-neutral-700)", borderTop: "1px solid var(--color-divider)", paddingTop: 8 }}>
        {view.edges.length} governed {view.edges.length === 1 ? "edge" : "edges"} · {view.nodes.length} {view.nodes.length === 1 ? "node" : "nodes"}
      </div>
    </Blueprint>
  );
}
