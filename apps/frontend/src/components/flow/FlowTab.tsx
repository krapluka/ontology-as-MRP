import type { EntitySelection, FlowEntity, FlowStage, FlowView } from "../../viewmodel/types.ts";
import { formatDate, formatMoney, isServed, labelOf } from "../../viewmodel/format.ts";
import { EntityBox, Kicker, PATH_NOT_SERVED, RiskDot } from "../tabs/shared.tsx";

/** The five pipeline columns, left to right. */
const STAGES: ReadonlyArray<{ stage: FlowStage; title: string }> = [
  { stage: "suppliers", title: "Suppliers" },
  { stage: "on_order", title: "On order" },
  { stage: "in_stock", title: "In stock" },
  { stage: "in_production", title: "In production" },
  { stage: "promised", title: "Promised" },
];

// Geometry: 5 columns 17.6 % wide with a 3 % gap, a 44 px head, 60 px tiles.
const COL_W = 17.6;
const COL_GAP = 3;
const HEAD_H = 44;
const TILE_H = 60;
const TILE_GAP = 8;

/** One drawn tile: the entity plus the work orders folded into it (an MO's own work orders). */
type Tile = { entity: FlowEntity; workOrders: FlowEntity[] };

/** Buckets a stage's entities into tiles. In production, a work order is folded into its MO's
 *  tile (as a count) unless it carries a served `cause` / `on_path` or its MO is not drawn,
 *  in which case it is a tile of its own. Every served row is represented. */
function tilesOf(stage: FlowStage, entities: FlowEntity[]): Tile[] {
  if (stage !== "in_production") return entities.map((entity) => ({ entity, workOrders: [] }));
  const mos = new Map<string, Tile>();
  for (const e of entities) if (e.partOf === null) mos.set(e.entity, { entity: e, workOrders: [] });
  const own: Tile[] = [];
  for (const e of entities) {
    if (e.partOf === null) continue;
    const mo = mos.get(e.partOf);
    if (mo && e.cause !== true && e.onPath !== true) mo.workOrders.push(e);
    else own.push({ entity: e, workOrders: [] });
  }
  return [...mos.values(), ...own];
}

function num(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1);
}

/** The tile's title and sub line, from the facts its row serves. */
function tileText(tile: Tile, labels: FlowView["labels"]): { title: string; sub: string } {
  const e = tile.entity;
  const name = labelOf(e.entity, e.label);
  const parts = (...xs: Array<string | null>) => xs.filter((x): x is string => x !== null && x.length > 0).join(" · ");
  switch (e.stage) {
    case "suppliers":
      return { title: name, sub: parts(e.leadTimeDays === null ? null : `${e.leadTimeDays}d lead`, e.sourcing) };
    case "on_order":
      return {
        title: parts(name, e.quantity === null ? null : `${num(e.quantity)}× ${e.product ? labelOf(e.product, labels[e.product]) : ""}`.trim()),
        sub: parts(formatDate(e.arrival) ? `arrives ${formatDate(e.arrival)}` : null, e.supplier ? labelOf(e.supplier, labels[e.supplier]) : null),
      };
    case "in_stock":
      return { title: name, sub: parts(e.onHand === null ? null : `${num(e.onHand)} on hand`, e.safety === null ? null : `safety ${num(e.safety)}`) };
    case "in_production": {
      const wos = tile.workOrders.length;
      return {
        title: parts(name, e.product ? labelOf(e.product, labels[e.product]) : null),
        sub: parts(
          e.lifecycleState,
          e.quantity === null ? null : `${num(e.quantity)}×`,
          e.partOf ? `part of ${labelOf(e.partOf, labels[e.partOf])}` : null,
          wos ? `${wos} work ${wos === 1 ? "order" : "orders"}` : null,
        ),
      };
    }
    case "promised":
      return { title: name, sub: parts(formatDate(e.commitmentDate) ? `due ${formatDate(e.commitmentDate)}` : null, formatMoney(e.orderValue, null)) };
  }
}

function FlowTile({ tile, labels, subject, selection }: { tile: Tile; labels: FlowView["labels"]; subject: string; selection: EntitySelection | undefined }) {
  const e = tile.entity;
  const on = e.onPath === true;
  const { title, sub } = tileText(tile, labels);
  const flagged: FlowEntity["atRisk"] = e.atRisk === true || e.cause === true ? true : e.atRisk === null && e.cause === null ? null : false;
  const gauge = e.stage === "in_stock" && e.onHand !== null;
  const max = Math.max(e.onHand ?? 0, e.safety ?? 0, 1) * 1.25;
  return (
    <EntityBox
      iri={e.entity}
      path={e.onPath}
      subject={subject}
      selection={selection}
      style={{
        height: TILE_H,
        display: "flex",
        flexDirection: "column",
        justifyContent: "center",
        gap: 3,
        padding: "0 10px",
        border: `1px solid ${on ? "var(--color-accent-900)" : "var(--color-divider)"}`,
        background: on ? "var(--color-accent-100)" : "var(--color-bg)",
        minWidth: 0,
        width: "100%",
      }}
    >
      <span style={{ display: "flex", alignItems: "center", gap: 7, minWidth: 0 }}>
        <RiskDot atRisk={flagged} size={8} name={e.cause === true ? "Cause" : undefined} />
        <span style={{ fontWeight: 500, fontSize: 13, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{title}</span>
      </span>
      <span style={{ display: "block", fontSize: 11.5, color: "var(--color-neutral-700)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{sub || " "}</span>
      {gauge ? (
        <span style={{ display: "block", position: "relative", height: 4, background: "var(--color-neutral-200)" }}>
          <span
            style={{
              position: "absolute",
              left: 0,
              top: 0,
              bottom: 0,
              width: `${((e.onHand ?? 0) / max) * 100}%`,
              background: e.safety !== null && (e.onHand ?? 0) < e.safety ? "var(--color-accent-900)" : "var(--color-accent-400)",
            }}
          />
          {e.safety !== null ? (
            <span style={{ position: "absolute", left: `${(e.safety / max) * 100}%`, top: -3, bottom: -3, width: 2, background: "var(--color-text)" }} />
          ) : null}
        </span>
      ) : null}
    </EntityBox>
  );
}

/**
 * The Flow tab: the five pipeline stages (Suppliers, On order, In stock, In production,
 * Promised) with their tile facts, the on-path tile of each stage highlighted, and the
 * point's chain drawn across them. A stage whose rows carry no `on_path` says "path not served"
 * and the chain skips it. Presentational: an entity with no known stage was already dropped
 * upstream.
 */
export function FlowTab({ view, subject, selection }: { view: FlowView; subject: string; selection?: EntitySelection }) {
  if (view.entities.length === 0) {
    return <p className="text-muted" style={{ fontSize: 13 }}>No flow rows for this subject.</p>;
  }

  const columns = STAGES.map(({ stage, title }) => {
    const entities = view.entities.filter((e) => e.stage === stage);
    const tiles = tilesOf(stage, entities);
    const served = isServed(entities.map((e) => e.onPath));
    // The chain attaches to the stage's lit tile. A multi-line PO is one tile per line (keyed
    // by PO + product), and only the chain component's line is lit.
    const litIndex = tiles.findIndex((t) => t.entity.onPath === true);
    return { stage, title, tiles, served, litIndex, count: tiles.length };
  });
  const tallest = Math.max(1, ...columns.map((c) => c.tiles.length));
  const height = HEAD_H + tallest * (TILE_H + TILE_GAP);

  // The chain: consecutive lit tiles, left to right, over the stages where the path is lit.
  const lit = columns.map((c, i) => ({ i, index: c.litIndex })).filter((c) => c.index >= 0);
  let chain = "";
  let skipped = "";
  for (let k = 0; k + 1 < lit.length; k++) {
    const a = lit[k];
    const b = lit[k + 1];
    if (!a || !b) continue;
    const x1 = a.i * (COL_W + COL_GAP) + COL_W;
    const x2 = b.i * (COL_W + COL_GAP);
    const y1 = HEAD_H + a.index * (TILE_H + TILE_GAP) + TILE_H / 2;
    const y2 = HEAD_H + b.index * (TILE_H + TILE_GAP) + TILE_H / 2;
    const link = `M${x1} ${y1}C${x1 + 1.8} ${y1} ${x2 - 1.8} ${y2} ${x2} ${y2} `;
    // A link over a stage with no lit tile is dashed: the chain has no served hop there.
    if (b.i === a.i + 1) chain += link;
    else skipped += link;
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      <Kicker>Pipeline · the point's chain across stages</Kicker>
      <div style={{ position: "relative", minWidth: 0, minHeight: height }}>
        <svg
          aria-hidden="true"
          data-testid="flow-chain"
          data-links={Math.max(0, lit.length - 1)}
          viewBox={`0 0 100 ${height}`}
          preserveAspectRatio="none"
          style={{ position: "absolute", left: 0, top: 0, width: "100%", height, fill: "none", pointerEvents: "none", overflow: "visible" }}
        >
          <path d={chain} style={{ stroke: "var(--color-accent-900)", strokeWidth: 2.5, vectorEffect: "non-scaling-stroke" }} />
          <path d={skipped} style={{ stroke: "var(--color-accent-900)", strokeWidth: 2, strokeDasharray: "5 4", vectorEffect: "non-scaling-stroke" }} />
        </svg>
        <div style={{ position: "relative", display: "grid", gridTemplateColumns: "repeat(5,minmax(0,1fr))", columnGap: `${COL_GAP}%` }}>
          {columns.map((c) => (
            <div key={c.stage} data-stage={c.stage} style={{ display: "flex", flexDirection: "column", gap: TILE_GAP, minWidth: 0 }}>
              <div style={{ height: HEAD_H - TILE_GAP, display: "flex", alignItems: "baseline", gap: 6, borderBottom: "1px solid var(--color-text)", paddingTop: 8, minWidth: 0, overflow: "hidden" }}>
                <span style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 17, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", minWidth: 0 }}>{c.title}</span>
                <span style={{ marginLeft: "auto", fontSize: 12, color: "var(--color-neutral-700)", whiteSpace: "nowrap", flex: "none" }}>
                  {c.count > 0 && !c.served ? PATH_NOT_SERVED : c.count}
                </span>
              </div>
              {c.tiles.length === 0 ? (
                <div style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>None served</div>
              ) : (
                c.tiles.map((t) => <FlowTile key={`${t.entity.entity}|${t.entity.product ?? ""}`} tile={t} labels={view.labels} subject={subject} selection={selection} />)
              )}
            </div>
          ))}
        </div>
      </div>
      <div style={{ display: "flex", flexWrap: "wrap", gap: "6px 18px", fontSize: 12, color: "var(--color-neutral-700)" }}>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 22, height: 12, background: "var(--color-accent-100)", border: "1px solid var(--color-accent-900)" }} /> On the point's path
        </span>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 22, height: 0, borderTop: "2.5px solid var(--color-accent-900)" }} /> Chain
        </span>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 22, height: 0, borderTop: "2px dashed var(--color-accent-900)" }} /> Chain over a stage with no lit hop
        </span>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 8, height: 8, background: "var(--color-accent-900)" }} /> At risk or cause
        </span>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 22, height: 4, background: "var(--color-accent-400)", position: "relative" }} /> On hand vs safety (line)
        </span>
      </div>
    </div>
  );
}
