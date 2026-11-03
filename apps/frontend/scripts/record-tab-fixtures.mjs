// Records the tab fixtures from a live projection of the seeded scenario, so the fixtures
// hold only reachable data. Reads every open action point from the running read surface,
// then `readModel(subject, tab)` for each point's subject and each of the four tabs, and
// writes the rows verbatim to `src/fixtures/recorded.ts`.
//
//   just podman-local-up && just seed && just transport-sim   # the seeded scenario
//   just graphdb-derive          # required: ekg:fulfills (the MO hop, Timeline `ready` dates)
//                                # and ekg:capacity (process points) exist only after it. Run
//                                # it once the world model has materialized
//   npm run record-fixtures      # EKG_API_ORIGIN overrides :8090
//
// The recorder refuses to write a projection taken without the derive step: it needs at least
// one Timeline `ready` row and a sales-order point whose Flow lights a fulfilling MO
// (`in_production` on the path). The output is generated: re-record it, never hand-edit it.
// `tabVocabulary.test.ts` proves every recorded key and enum value is in `tab-rows.json`.

import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const origin = process.env.EKG_API_ORIGIN ?? "http://localhost:8090";
const TABS = ["timeline", "graph", "flow", "suppliers"];

async function gql(query, variables = {}) {
  const res = await fetch(`${origin}/api/graphql`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  if (!res.ok) throw new Error(`read surface answered ${res.status}`);
  const body = await res.json();
  if (body.errors) throw new Error(JSON.stringify(body.errors));
  return body.data;
}

// The points still awaiting a decision (raised, or carrying a proposal the inbox lists).
const listed = await gql("query { actionPoints { subject state reason } }");
// Sorted by subject, so a re-recording diffs only where the projection changed.
const listedOpen = listed.actionPoints
  .filter((p) => p.state === "OPEN" || p.state === "PROPOSED")
  .sort((a, b) => a.subject.localeCompare(b.subject));

async function rowsOf(subject, tab) {
  const data = await gql(
    "query ($subject: ID!, $tab: TabKind!) { readModel(subject: $subject, tab: $tab) { rows { fields { key value } } } }",
    { subject, tab: tab.toUpperCase() },
  );
  return data.readModel.rows.map((row) =>
    Object.fromEntries(row.fields.map((f) => [f.key, f.value]).sort(([a], [b]) => a.localeCompare(b))),
  );
}

// Only the seeded scenario's points: a point whose subject the plant itself serves (an order on
// the Timeline or an entity in the Flow). A shared stack also lists points left by gated test
// runs whose subjects are not in the world model; recording them would only add noise.
const actionPoints = [];
const skipped = [];
const rowsBySubject = {};
for (const point of listedOpen) {
  const tabs = {};
  for (const tab of TABS) tabs[tab] = await rowsOf(point.subject, tab);
  const served = [...tabs.timeline, ...tabs.flow].some((r) => r.entity === point.subject);
  if (!served) {
    skipped.push(point.subject);
    continue;
  }
  actionPoints.push(point);
  rowsBySubject[point.subject] = tabs;
}
if (actionPoints.length === 0) throw new Error("no open action point on a served subject: seed the scenario first");
if (skipped.length > 0) console.log(`skipped ${skipped.length} point(s) whose subject the plant does not serve: ${skipped.join(", ")}`);

// Refuse a projection recorded without `just graphdb-derive`: without ekg:fulfills no order has
// a `ready` date and no MO fulfils a promised order, so the fixtures would silently lose the
// Timeline's ready bars and the Flow's In production hop.
const allRows = Object.values(rowsBySubject).flatMap((tabs) => Object.values(tabs).flat());
const readyServed = Object.values(rowsBySubject).some((tabs) => tabs.timeline.some((r) => r.kind === "ready"));
const moLit = actionPoints.some(
  ({ subject }) =>
    subject.includes("/sale.order/") &&
    rowsBySubject[subject].flow.some((r) => r.stage === "in_production" && r.on_path === "true"),
);
const missing = [
  readyServed ? null : "no Timeline `ready` row",
  moLit ? null : "no sales-order point lights a fulfilling MO (Flow `in_production` on the path)",
].filter(Boolean);
if (missing.length > 0) {
  throw new Error(
    `refusing to record ${allRows.length} rows: ${missing.join("; ")}. Run \`just graphdb-derive\` once the world model has materialized, then re-record`,
  );
}

const recordedOn = new Date().toISOString().slice(0, 10);
const rowsLiteral = (rows) => `[\n${rows.map((r) => `      ${JSON.stringify(r)},`).join("\n")}\n    ]`;
const subjectsLiteral = Object.entries(rowsBySubject)
  .map(
    ([subject, tabs]) =>
      `  ${JSON.stringify(subject)}: {\n${TABS.map((tab) => `    ${tab}: ${rowsLiteral(tabs[tab])},`).join("\n")}\n  },`,
  )
  .join("\n");

const out = `// Generated by scripts/record-tab-fixtures.mjs from a live projection of the seeded
// scenario on ${recordedOn}. Re-record it with \`npm run record-fixtures\`; do not edit by hand.
import type { RowMap } from "../data/tabMapping.ts";

export type RecordedTab = "timeline" | "graph" | "flow" | "suppliers";

/** The day the projection was recorded (the rows' dates are relative to it). */
export const RECORDED_ON = ${JSON.stringify(recordedOn)};

/** The open action points the read surface listed when recorded. */
export const recordedPoints: ReadonlyArray<{ subject: string; reason: string }> = ${JSON.stringify(
  actionPoints.map(({ subject, reason }) => ({ subject, reason })),
  null,
  2,
)};

/** Each point subject's served rows, per tab, verbatim. */
export const recordedRows: Readonly<Record<string, Readonly<Record<RecordedTab, ReadonlyArray<RowMap>>>>> = {
${subjectsLiteral}
};
`;

writeFileSync(join(root, "src/fixtures/recorded.ts"), out);
console.log(`recorded ${actionPoints.length} point(s) × ${TABS.length} tabs from ${origin}`);
