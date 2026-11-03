/**
 * The tab fixtures' scenario: the recorded live projection of the seeded scenario
 * (`recorded.ts`, generated) plus the one shape the read surface serves when the monitor's
 * rules cannot be read, derived from those rows by the backend's failure rule. Only
 * reachable data: nothing here invents an entity or a value.
 */
import { graphEndpointRole, type GraphEdgeSide, type RowMap } from "../data/tabMapping.ts";
import { recordedPoints, recordedRows, type RecordedTab } from "./recorded.ts";

export { RECORDED_ON, recordedPoints, recordedRows } from "./recorded.ts";
export type { RecordedTab } from "./recorded.ts";

/** Whether `subject` is served `at_risk="true"` on its own promise row. */
function servedAtRisk(subject: string): boolean {
  const timeline = recordedRows[subject]?.timeline ?? [];
  return timeline.some((r) => r["entity"] === subject && r["at_risk"] === "true");
}

function pick(predicate: (subject: string) => boolean, what: string): string {
  const found = recordedPoints.find((p) => predicate(p.subject));
  if (!found) throw new Error(`the recorded scenario has no ${what} point; re-record it`);
  return found.subject;
}

/** The flagship point: its subject is served at risk (the lit supply chain). */
export const FLAGSHIP_SUBJECT = pick(servedAtRisk, "at-risk");
/** An open point whose rule has cleared: its subject is served `at_risk="false"`. */
export const CLEARED_SUBJECT = pick((s) => !servedAtRisk(s), "cleared-rule");

/** The subjects of every listed point (what a tab entity may select). */
export const SELECTABLE_SUBJECTS: ReadonlySet<string> = new Set(recordedPoints.map((p) => p.subject));

/** The recorded rows of one tab for one subject. */
export function rowsFor(subject: string, tab: RecordedTab): RowMap[] {
  const rows = recordedRows[subject]?.[tab];
  if (!rows) throw new Error(`no recorded ${tab} rows for ${subject}`);
  return [...rows];
}

// --- the "rules unreadable" shape ---

/** The Graph roles whose path status needs the rules (the cause-dependent columns). */
const CAUSE_DEPENDENT_ROLES: ReadonlySet<string> = new Set(["Component", "Supplier", "PurchaseOrder"]);
/** The Flow stages whose path status needs the rules. */
const CAUSE_DEPENDENT_STAGES: ReadonlySet<string> = new Set(["suppliers", "on_order", "in_stock"]);
/** The Suppliers row kinds whose path status needs the rules. */
const CAUSE_DEPENDENT_KINDS: ReadonlySet<string> = new Set(["supplier", "open_po", "stock"]);

function without(row: RowMap, keys: ReadonlyArray<string>): RowMap {
  const copy: Record<string, string> = { ...row };
  for (const key of keys) delete copy[key];
  return copy;
}

/** A served `on_path` value as a tri-state (absent is unknown). */
type PathMark = boolean | null;

function pathMark(value: string | undefined): PathMark {
  return value === "true" ? true : value === "false" ? false : null;
}

/**
 * The Graph edge's `on_path` from its two ends, by the backend's rule (`build_rows`): known
 * off when either end is known off, lit when both ends are lit, and omitted otherwise (only
 * when neither end is known to be off the path). Each end is the node of the role its
 * predicate names (a dual-role partner), else its roles merged: a lit role wins, any unknown
 * keeps it unknown.
 */
function edgeOnPath(edge: RowMap, nodes: ReadonlyMap<string, ReadonlyMap<string, PathMark>>): PathMark {
  const end = (iri: string | undefined, side: GraphEdgeSide): PathMark => {
    const roles = nodes.get(iri ?? "");
    if (!roles) return null;
    const role = graphEndpointRole(edge["p"] ?? "", side);
    if (role !== null && roles.has(role)) return roles.get(role) ?? null;
    const marks = [...roles.values()];
    if (marks.includes(true)) return true;
    return marks.length > 0 && marks.every((m) => m === false) ? false : null;
  };
  const s = end(edge["s"], "subject");
  const o = end(edge["o"], "object");
  if (s === false || o === false) return false;
  return s === true && o === true ? true : null;
}

/**
 * The rows the read surface serves for `tab` when the rule evaluation fails: no `at_risk`,
 * `cause` or `exposure` on any row, and no `on_path` on the cause-dependent columns / stages
 * (components, suppliers, POs), while the rule-independent hops keep it. A Graph edge's
 * `on_path` is re-derived from its ends by the backend's rule, so an edge with one end known
 * off the path stays `"false"` even when its other end is unknown.
 *
 * Without the rules there is no chain, so only the rule-independent Product · BOM hops stay
 * lit. The rows must light no sub-assembly route (one lit Product, one lit BOM at most);
 * otherwise this derivation cannot tell the route from the hops and throws.
 */
export function rulesUnreadable(tab: RecordedTab, rows: ReadonlyArray<RowMap>): RowMap[] {
  const causeDependent = (row: RowMap): boolean => {
    switch (tab) {
      case "graph":
        return CAUSE_DEPENDENT_ROLES.has(row["role"] ?? "");
      case "flow":
        return CAUSE_DEPENDENT_STAGES.has(row["stage"] ?? "");
      case "suppliers":
        return CAUSE_DEPENDENT_KINDS.has(row["kind"] ?? "");
      case "timeline":
        return row["kind"] === "inbound";
    }
  };
  const entities = rows
    .filter((row) => tab !== "graph" || row["kind"] === "node")
    .map((row) => without(row, causeDependent(row) ? ["at_risk", "cause", "exposure", "on_path"] : ["at_risk", "cause", "exposure"]));
  if (tab !== "graph") return entities;

  for (const role of ["Product", "BOM"]) {
    const lit = entities.filter((n) => n["role"] === role && n["on_path"] === "true").length;
    if (lit > 1) throw new Error(`the recorded chain lights ${lit} ${role} nodes (a sub-assembly route), so rulesUnreadable cannot derive the rule-independent hops`);
  }
  const nodes = new Map<string, Map<string, PathMark>>();
  for (const n of entities) {
    const roles = nodes.get(n["node"] ?? "") ?? new Map<string, PathMark>();
    roles.set(n["role"] ?? "", pathMark(n["on_path"]));
    nodes.set(n["node"] ?? "", roles);
  }
  let next = 0;
  return rows.map((row) => {
    if (row["kind"] === "node") {
      const node = entities[next++];
      if (!node) throw new Error("rulesUnreadable: node rows out of step");
      return node;
    }
    const edge = without(row, ["at_risk", "cause", "exposure", "on_path"]);
    const lit = edgeOnPath(edge, nodes);
    return lit === null ? edge : { ...edge, on_path: String(lit) };
  });
}
