import { beforeEach, describe, expect, test } from "vitest";
import { act, render, renderHook, screen, waitFor } from "@testing-library/react";
import { Provider, type Client } from "urql";
import type { TypedDocumentNode } from "urql";
import {
  ActionPointState,
  ActivePolicyDocument,
  ApprovedPlansDocument,
  AutoAppliedDocument,
  LabelsDocument,
  PlansDocument,
  TraceTag,
} from "./graphql/graphql.ts";
import { type TabId, useEntityLabels, useInboxData, useReadModel } from "./hooks.ts";
import { RiskInbox } from "../screens/RiskInbox.tsx";
import { fixedClock } from "../fixtures/clock.ts";

// A minimal fake urql client that routes each committed document to a canned response,
// enough to show the data hooks and mapping render a real plan with no backend. Only the
// `query(...).toPromise()` shape the hook uses is implemented.
const SUBJECT = "https://ekg.example/id/sale.order/147";
const MO = "https://ekg.example/id/mrp.production/318";

const pointOf = (subject: string, code: string, state = ActionPointState.Proposed) => ({
  id: `action-point/${subject}`,
  subject,
  state,
  reason: `reason of ${subject}`,
  code,
});

const option = {
  recommended: true,
  rankKey: "00000",
  needs: [{ code: "cost_over_limit", detail: "cost 320.00 exceeds the limit 250.00" }],
  changes: [{ verb: "split_production", object: MO, inverse: null, params: [{ key: "new_value", value: "30+10" }] }],
  metrics: [{ key: "delivery", value: "on_time" }],
  scope: [
    { kind: "order", count: 2 },
    { kind: "work center", count: 2 },
    { kind: "purchase order", count: 1 },
  ],
  lanes: [
    {
      workCenter: "https://ekg.example/id/mrp.workcenter/assembly",
      capacity: 2,
      before: [{ workOrder: "S1", start: "2027-01-27", end: "2027-01-30", row: 1, changed: false }],
      after: [{ workOrder: "S1", start: "2027-01-26", end: "2027-01-28", row: 1, changed: true }],
    },
  ],
  trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: [] },
};

const planOf = (lead: string, resolves: string[], options: unknown[]) => ({
  subject: lead,
  resolves,
  lead,
  baseline: { rankKey: "99999", metrics: [{ key: "delivery", value: "at_risk" }] },
  trace: { summary: { simulated: 3, rejected: 1, openAssumptions: 0 }, steps: [{ tag: TraceTag.Simulated, text: "chose the split", rejected: false }] },
  options,
});

/** Two rows: a portfolio plan (PL-0412) and a single point's plan (PL-0409). */
const portfolioEntry = {
  id: "plan/0412e7c1d9a3",
  code: "0412",
  resolves: [pointOf(MO, "0239"), pointOf(SUBJECT, "0233")],
  portfolio: { id: "portfolio/0412e7c1d9a3", generation: 2 },
  openedAt: "1798899360",
  situation: "Controllers arrive a day late; MO-0318 fulfils SO-0147.",
  plan: planOf(SUBJECT, [MO, SUBJECT], [option]),
};
const loneSubject = "https://ekg.example/id/sale.order/142";
const loneEntry = {
  id: "plan/0409d2b6f4e8",
  code: "0409",
  resolves: [pointOf(loneSubject, "0231")],
  portfolio: null,
  openedAt: "1798893000",
  situation: "Battery Pack short.",
  plan: planOf(loneSubject, [loneSubject], [option]),
};

const responses = new Map<unknown, unknown>([
  [PlansDocument, { data: { plans: [portfolioEntry, loneEntry] } }],
  [ApprovedPlansDocument, { data: { approvedPlans: [] } }],
  [AutoAppliedDocument, { data: { autoApplied: [] } }],
  [ActivePolicyDocument, { data: { policy: { policyId: "p/v1", criteria: [], undoWindow: "8h", autoApplyActions: [] } } }],
]);

const fakeClient = {
  query: (doc: TypedDocumentNode<unknown, object>) => ({
    toPromise: async () => responses.get(doc) ?? { data: undefined, error: undefined },
  }),
  // The Provider only needs a Client-shaped value; the hook uses `query` alone.
} as unknown as Client;

function Harness() {
  const data = useInboxData();
  if (data.status !== "ready") return <div>loading</div>;
  return (
    <RiskInbox
      items={data.items}
      autoApplied={data.autoApplied}
      approved={data.approved}
      policy={data.policy}
      kpis={data.kpis}
      onApprove={async () => ({ ok: true })}
      onDismiss={async () => ({ ok: true })}
      onUndo={async () => ({ ok: true })}
    />
  );
}

describe("useInboxData", () => {
  beforeEach(() => localStorage.clear());

  test("renders the plans read as rows, the first plan's options and lanes", async () => {
    render(
      <Provider value={fakeClient}>
        <Harness />
      </Provider>,
    );

    // The recommended option's derived label becomes the detail heading; the situation, the
    // option's scope and its lanes render, all mapped from the contract response.
    const heading = await screen.findByRole("heading", { level: 2 });
    await waitFor(() => expect(heading.textContent ?? "").toBe("Split"));
    expect(screen.getByText(/Controllers arrive a day late; MO-0318 fulfils SO-0147/)).toBeTruthy();
    expect(screen.getAllByText("PL-0412").length).toBeGreaterThan(0);
    expect(screen.getByText("PL-0409")).toBeTruthy();
    expect(screen.getByText(/Moved by plan/i)).toBeTruthy();
  });
});

// --- every plan awaiting oversight is listed and counted ---

type PlansResponse = { data?: { plans: unknown[] }; error?: unknown };

/** A fake client over a `plans` response; `failPlans` makes it fail outright (a refresh
 *  failure). It records the variables of each read. */
function inboxClient(plans: PlansResponse) {
  const calls: { doc: unknown; vars: unknown }[] = [];
  const state = { failPlans: false };
  const client = {
    query: (doc: unknown, vars: unknown) => ({
      toPromise: async () => {
        calls.push({ doc, vars });
        if (doc === PlansDocument) return state.failPlans ? { error: new Error("network down") } : plans;
        return responses.get(doc) ?? { data: undefined, error: undefined };
      },
    }),
  } as unknown as Client;
  return { client, calls, state };
}

function useProbe() {
  return useInboxData(fixedClock(new Date(2027, 0, 3, 14, 32).getTime()));
}

describe("useInboxData lists every plan awaiting oversight", () => {
  const pendingPortfolio = {
    id: "plan/0415c0ffee00",
    code: "0415",
    resolves: [pointOf("https://ekg.example/id/mrp.production/320", "0240"), pointOf("https://ekg.example/id/sale.order/148", "0241")],
    portfolio: { id: "portfolio/0415c0ffee00", generation: 1 },
    openedAt: null,
    situation: "linked",
    plan: null,
  };
  const failing = { ...loneEntry, id: "plan/0172bb1c2d3e", code: "0172", resolves: [pointOf("https://ekg.example/id/sale.order/172", "0172", ActionPointState.Open)], plan: null };
  const noOption = { ...loneEntry, id: "plan/0160aa000000", code: "0160", plan: planOf(loneSubject, [loneSubject], []) };
  // The read reports the failing entry's plan as a field error at its path; the list itself
  // is served.
  const plans: PlansResponse = {
    data: { plans: [portfolioEntry, pendingPortfolio, failing, noOption, loneEntry] },
    error: { graphQLErrors: [{ message: "plan unavailable: the plan read timed out", path: ["plans", 2, "plan"] }] },
  };

  test("a portfolio, a pending portfolio, an unavailable and a no-option plan are each listed and counted", async () => {
    const { client, calls } = inboxClient(plans);
    const { result } = renderHook(useProbe, { wrapper: ({ children }) => <Provider value={client}>{children}</Provider> });
    await waitFor(() => expect(result.current.status).toBe("ready"));

    // One plans read, with no per-point plan fan-out, in the served (newest-first) order.
    expect(calls.filter((c) => c.doc === PlansDocument)).toHaveLength(1);
    const rows = result.current.items.map((i) => [i.kind, i.entry.id]);
    expect(rows).toEqual([
      ["plan", "plan/0412e7c1d9a3"],
      ["pending", "plan/0415c0ffee00"],
      ["unavailable", "plan/0172bb1c2d3e"],
      ["no_option", "plan/0160aa000000"],
      ["plan", "plan/0409d2b6f4e8"],
    ]);
    const portfolio = result.current.items[0];
    expect(portfolio?.entry.portfolio).toEqual({ id: "portfolio/0412e7c1d9a3", generation: 2 });
    expect(portfolio?.kind === "plan" ? portfolio.plan.lead : null).toBe(SUBJECT);
    expect(result.current.items[1]?.entry.resolves).toHaveLength(2);
    const unavailable = result.current.items[2];
    expect(unavailable?.kind === "unavailable" ? unavailable.reason : null).toMatch(/timed out/);
    expect(unavailable?.entry.resolves[0]?.state).toBe("open");
    // "waiting" counts every listed plan (row), not points.
    expect(result.current.kpis.waiting).toBe(5);
    expect(result.current.initialError).toBeNull();
    expect(result.current.refreshError).toBeNull();
  });

  test("the approved plans are read since the start of the operator's local day", async () => {
    const { client, calls } = inboxClient(plans);
    const { result } = renderHook(useProbe, { wrapper: ({ children }) => <Provider value={client}>{children}</Provider> });
    await waitFor(() => expect(result.current.status).toBe("ready"));
    const approvedRead = calls.find((c) => c.doc === ApprovedPlansDocument);
    expect(approvedRead?.vars).toEqual({ since: String(new Date(2027, 0, 3).getTime() / 1000) });
  });

  test("a refresh failure keeps the data and sets refreshError, never the initial error", async () => {
    const { client, state } = inboxClient(plans);
    const { result } = renderHook(useProbe, { wrapper: ({ children }) => <Provider value={client}>{children}</Provider> });
    await waitFor(() => expect(result.current.status).toBe("ready"));

    state.failPlans = true;
    await act(async () => {
      await result.current.refetch();
    });
    expect(result.current.status).toBe("ready");
    expect(result.current.items).toHaveLength(5);
    expect(result.current.refreshError).toMatch(/network down/);
    expect(result.current.initialError).toBeNull();

    // The next good load clears the stale flag.
    state.failPlans = false;
    await act(async () => {
      await result.current.refetch();
    });
    expect(result.current.refreshError).toBeNull();
  });

  test("a failed first load is the initial error (nothing to keep)", async () => {
    const { client, state } = inboxClient(plans);
    state.failPlans = true;
    const { result } = renderHook(useProbe, { wrapper: ({ children }) => <Provider value={client}>{children}</Provider> });
    await waitFor(() => expect(result.current.status).toBe("error"));
    expect(result.current.initialError).toMatch(/network down/);
    expect(result.current.refreshError).toBeNull();
    expect(result.current.items).toEqual([]);
  });

  test("an error that is not a per-entry plan error fails the read", async () => {
    const { client } = inboxClient({ data: { plans: [loneEntry] }, error: { graphQLErrors: [{ message: "boom", path: ["plans"] }] } });
    const { result } = renderHook(useProbe, { wrapper: ({ children }) => <Provider value={client}>{children}</Provider> });
    await waitFor(() => expect(result.current.status).toBe("error"));
  });
});

/** A resolvable promise so a test controls when a query settles. */
function deferred<TValue>(): { promise: Promise<TValue>; resolve: (value: TValue) => void } {
  let resolve!: (value: TValue) => void;
  const promise = new Promise<TValue>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

function Probe({ subject, tab }: { subject: string; tab: TabId }) {
  const data = useReadModel(subject, tab);
  return <div data-testid="probe">{data.status === "ready" ? data.view?.tab : data.status}</div>;
}

describe("useReadModel", () => {
  test("a superseded tab load never overwrites the current view", async () => {
    // The first (Timeline) load hangs; switching to Graph supersedes it. The Graph response
    // lands, then the stale Timeline response resolves late. The staleness guard must drop
    // it, so the view stays Graph rather than flipping back to the superseded tab.
    const timeline = deferred<unknown>();
    const graph = deferred<unknown>();
    const client = {
      query: (_doc: unknown, vars: { tab: string }) => ({
        toPromise: async () => (vars.tab === "TIMELINE" ? timeline.promise : graph.promise),
      }),
    } as unknown as Client;

    const { rerender } = render(
      <Provider value={client}>
        <Probe subject={SUBJECT} tab="timeline" />
      </Provider>,
    );
    // Switch tabs before the Timeline load resolves, which supersedes it.
    rerender(
      <Provider value={client}>
        <Probe subject={SUBJECT} tab="graph" />
      </Provider>,
    );

    // The current (Graph) load lands first.
    graph.resolve({ data: { readModel: { subject: SUBJECT, tab: "GRAPH", rows: [] } } });
    await waitFor(() => expect(screen.getByTestId("probe").textContent).toBe("graph"));

    // The superseded Timeline load resolves late. Drain its continuation and any resulting
    // re-render (a macrotask flush inside `act`) so that, were the guard removed, the stale
    // `setData({tab:"timeline"})` would have landed before the synchronous check below.
    await act(async () => {
      timeline.resolve({ data: { readModel: { subject: SUBJECT, tab: "TIMELINE", rows: [] } } });
      await new Promise((resolve) => setTimeout(resolve));
    });
    expect(screen.getByTestId("probe").textContent).toBe("graph");
  });
});

// --- entity labels: one read per id set, cached per id ---

describe("useEntityLabels", () => {
  const A = "https://ekg.example/id/mrp.production/1";
  const B = "https://ekg.example/id/mrp.workcenter/1";
  const UNNAMED = "https://ekg.example/id/mrp.production/9";

  test("reads an id set once, sorted and de-duplicated, and later asks only about new ids", async () => {
    const asked: string[][] = [];
    const client = {
      query: (doc: unknown, vars: { ids: string[] }) => ({
        toPromise: async () => {
          if (doc !== LabelsDocument) return { data: undefined };
          asked.push(vars.ids);
          return { data: { labels: vars.ids.filter((id) => id !== UNNAMED).map((id) => ({ id, label: `name of ${id}` })) } };
        },
      }),
    } as unknown as Client;
    const wrapper = ({ children }: { children: React.ReactNode }) => <Provider value={client}>{children}</Provider>;
    const { result, rerender } = renderHook(({ ids }: { ids: string[] }) => useEntityLabels(ids), {
      initialProps: { ids: [B, A, B] },
      wrapper,
    });
    await waitFor(() => expect(result.current.get(A)).toBe(`name of ${A}`));
    expect(result.current.get(B)).toBe(`name of ${B}`);
    expect(asked).toEqual([[A, B]]);

    // The same set in another order is not read again.
    rerender({ ids: [A, B] });
    // A new id is read on its own. An id the read cannot name gets no label and is not asked
    // again.
    rerender({ ids: [A, B, UNNAMED] });
    await waitFor(() => expect(asked).toEqual([[A, B], [UNNAMED]]));
    rerender({ ids: [UNNAMED, A] });
    await act(async () => {});
    expect(asked).toEqual([[A, B], [UNNAMED]]);
    expect(result.current.has(UNNAMED)).toBe(false);
  });

  test("a failed read caches nothing, so the short forms stay and the ids are asked again", async () => {
    let calls = 0;
    const client = {
      query: () => ({
        toPromise: async () => {
          calls += 1;
          return { data: undefined, error: new Error("labels unavailable") };
        },
      }),
    } as unknown as Client;
    const wrapper = ({ children }: { children: React.ReactNode }) => <Provider value={client}>{children}</Provider>;
    const { result, rerender } = renderHook(({ ids }: { ids: string[] }) => useEntityLabels(ids), { initialProps: { ids: [A] }, wrapper });
    await waitFor(() => expect(calls).toBe(1));
    expect(result.current.size).toBe(0);
    rerender({ ids: [A, B] });
    await waitFor(() => expect(calls).toBe(2));
  });
});
