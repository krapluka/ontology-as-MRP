import { beforeEach, describe, expect, test, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { Provider, type Client } from "urql";
import { ActionPointState, ActivePolicyDocument, ApprovedPlansDocument, AutoAppliedDocument, PlansDocument } from "./data/graphql/graphql.ts";
import { Inbox, UNCERTAIN_OUTCOME } from "./App.tsx";
import type { CommandResult, Commands } from "./data/commands.ts";
import { fixedClock } from "./fixtures/clock.ts";

// The live container over a fake read surface and fake command client. Covers the command
// rules: a refetch after every result (not only success), the uncertain-outcome notice for a
// 502, the stale banner that keeps the data after a failed refresh, and one command at a time
// per plan. Also covers that a portfolio's plan is commanded through the portfolio endpoints,
// one command carrying its generation.
const SUBJECT = "https://ekg.example/id/sale.order/147";
const MO = "https://ekg.example/id/mrp.production/318";
const NOW = 1798900200 * 1000;

const point = (subject: string, code: string, state = ActionPointState.Proposed) => ({
  id: `action-point/${subject}`,
  subject,
  state,
  reason: "Controllers arrive a day late.",
  code,
});

function planOf(lead: string, resolves: string[]) {
  return {
    subject: lead,
    resolves,
    lead,
    baseline: { rankKey: "99999", metrics: [] },
    trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: [] },
    options: [
      {
        recommended: true,
        rankKey: "00000~aaaaaaaaaaaa",
        needs: [],
        changes: [{ verb: "expedite_po", object: "https://ekg.example/id/purchase.order.line/1", inverse: null, params: [] }],
        metrics: [],
        scope: [{ kind: "purchase order", count: 1 }],
        lanes: [],
        trace: { summary: { simulated: 1, rejected: 0, openAssumptions: 0 }, steps: [] },
      },
    ],
  };
}

const loneEntry = {
  id: "plan/0233a1f0c9e2",
  code: "0233",
  resolves: [point(SUBJECT, "0233")],
  portfolio: null,
  openedAt: null,
  situation: "Controllers arrive a day late.",
  plan: planOf(SUBJECT, [SUBJECT]),
};

const portfolioEntry = {
  id: "plan/0412e7c1d9a3",
  code: "0412",
  resolves: [point(MO, "0239"), point(SUBJECT, "0233a1f0c9e2")],
  portfolio: { id: "portfolio/0412e7c1d9a3", generation: 2 },
  openedAt: null,
  situation: "MO-0318 fulfils SO-0147.",
  plan: planOf(SUBJECT, [MO, SUBJECT]),
};

const approvedPortfolio = {
  id: "plan/0398f1e2d3c4",
  code: "0398",
  resolves: [point("https://ekg.example/id/sale.order/139", "0228", ActionPointState.Promoted)],
  portfolio: { id: "portfolio/0398f1e2d3c4", generation: 1 },
  rankKey: "00000",
  changes: [{ verb: "expedite_po", object: "https://ekg.example/id/purchase.order.line/2", inverse: "restore", params: [] }],
  reviewer: "Dana Ops",
  rationale: "why",
  promotedAt: String(NOW / 1000 - 600),
  undoUntil: String(NOW / 1000 + 3600),
  reversible: true,
  remaining: 1,
};

function readSurface(entries: unknown[] = [loneEntry], approved: unknown[] = []) {
  const state = { planReads: 0, failPlans: false };
  const client = {
    query: (doc: unknown) => ({
      toPromise: async () => {
        if (doc === PlansDocument) {
          state.planReads += 1;
          if (state.failPlans) return { error: new Error("network down") };
          return { data: { plans: entries } };
        }
        if (doc === ApprovedPlansDocument) return { data: { approvedPlans: approved } };
        if (doc === AutoAppliedDocument) return { data: { autoApplied: [] } };
        if (doc === ActivePolicyDocument) return { data: { policy: { policyId: "p/v1", criteria: [], undoWindow: "8h", autoApplyActions: [] } } };
        return { data: { headlineMetrics: [] } };
      },
    }),
  } as unknown as Client;
  return { client, state };
}

function commandsReturning(result: () => Promise<CommandResult>) {
  return {
    promote: vi.fn(result),
    abort: vi.fn(result),
    undo: vi.fn(result),
    promotePortfolio: vi.fn(result),
    abortPortfolio: vi.fn(result),
    undoPortfolio: vi.fn(result),
  } satisfies Commands;
}

async function renderInbox(commands: Commands, entries?: unknown[], approved?: unknown[]) {
  const surface = readSurface(entries, approved);
  render(
    <Provider value={surface.client}>
      <Inbox commands={commands} clock={fixedClock(NOW)} />
    </Provider>,
  );
  await screen.findByRole("heading", { level: 2 });
  fireEvent.change(screen.getByLabelText(/^Reviewer$/), { target: { value: "Dana Ops" } });
  fireEvent.change(screen.getByLabelText(/rationale/i), { target: { value: "Worth the freight." } });
  return surface;
}

describe("the inbox container", () => {
  beforeEach(() => localStorage.clear());

  test("a 409 refusal refreshes the inbox, surfaces the reason and keeps the rationale", async () => {
    const commands = commandsReturning(async () => ({ ok: false, status: 409, reason: "another command is in progress" }));
    const surface = await renderInbox(commands);
    const before = surface.state.planReads;

    fireEvent.click(screen.getByRole("button", { name: /Approve plan A/i }));
    expect((await screen.findByRole("alert")).textContent).toBe("another command is in progress");
    await waitFor(() => expect(surface.state.planReads).toBe(before + 1));
    expect(commands.promote).toHaveBeenCalledWith(expect.objectContaining({ reviewer: "Dana Ops", rationale: "Worth the freight.", option_rank_key: "00000~aaaaaaaaaaaa" }));
    expect((screen.getByLabelText(/rationale/i) as HTMLTextAreaElement).value).toBe("Worth the freight.");
  });

  test("a 502 refreshes the inbox and reports the outcome as uncertain", async () => {
    const commands = commandsReturning(async () => ({ ok: false, status: 502, reason: "write-back partly applied" }));
    const surface = await renderInbox(commands);
    const before = surface.state.planReads;

    fireEvent.click(screen.getByRole("button", { name: /Approve plan A/i }));
    expect((await screen.findByRole("alert")).textContent).toBe(UNCERTAIN_OUTCOME);
    expect(UNCERTAIN_OUTCOME).toBe("Outcome uncertain: the write-back partly failed; refreshed. Check the source record.");
    await waitFor(() => expect(surface.state.planReads).toBe(before + 1));
  });

  test("a failed refresh keeps the data under a stale banner (no error page)", async () => {
    const commands = commandsReturning(async () => ({ ok: true, status: 200 }));
    const surface = await renderInbox(commands);
    surface.state.failPlans = true;

    fireEvent.click(screen.getByRole("button", { name: /Approve plan A/i }));
    expect((await screen.findByRole("status")).textContent).toMatch(/Could not refresh the inbox \(network down\).*may be stale/);
    // The last data read is still shown.
    expect(screen.getByText("Controllers arrive a day late.", { selector: "p" })).toBeTruthy();
    expect(screen.queryByText(/Could not reach the plant-pulse-api read surface/)).toBeNull();
  });

  test("a double click issues one command; the controls are busy while it is in flight", async () => {
    let settle!: (result: CommandResult) => void;
    const commands = commandsReturning(() => new Promise<CommandResult>((resolve) => (settle = resolve)));
    await renderInbox(commands);

    const approve = screen.getByRole("button", { name: /Approve plan A/i });
    fireEvent.click(approve);
    fireEvent.click(approve);
    expect(commands.promote).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(approve.getAttribute("aria-busy")).toBe("true"));
    expect((approve as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: /^Dismiss$/ }) as HTMLButtonElement).disabled).toBe(true);

    await act(async () => settle({ ok: true, status: 200 }));
    await waitFor(() => expect(approve.hasAttribute("aria-busy")).toBe(false));
    expect(commands.promote).toHaveBeenCalledTimes(1);
  });

  test("a single point's plan is approved and dismissed through the point commands", async () => {
    const commands = commandsReturning(async () => ({ ok: true, status: 200 }));
    await renderInbox(commands);
    fireEvent.click(screen.getByRole("button", { name: /Approve plan A/i }));
    await waitFor(() => expect(commands.promote).toHaveBeenCalledTimes(1));
    expect(commands.promote).toHaveBeenCalledWith(expect.objectContaining({ subject: SUBJECT, option_rank_key: "00000~aaaaaaaaaaaa" }));
    expect(commands.promotePortfolio).not.toHaveBeenCalled();
  });

  test("a portfolio plan is approved as ONE portfolio command carrying the generation shown", async () => {
    const commands = commandsReturning(async () => ({ ok: true, status: 200 }));
    await renderInbox(commands, [portfolioEntry]);
    fireEvent.click(screen.getByRole("button", { name: /Approve plan A/i }));
    await waitFor(() => expect(commands.promotePortfolio).toHaveBeenCalledTimes(1));
    expect(commands.promotePortfolio).toHaveBeenCalledWith(
      expect.objectContaining({
        portfolio: "portfolio/0412e7c1d9a3",
        generation: 2,
        option_rank_key: "00000~aaaaaaaaaaaa",
        reviewer: "Dana Ops",
        rationale: "Worth the freight.",
      }),
    );
    expect(commands.promote).not.toHaveBeenCalled();
  });

  test("a portfolio plan is dismissed as one portfolio command with the confirmed reason", async () => {
    const commands = commandsReturning(async () => ({ ok: true, status: 200 }));
    await renderInbox(commands, [portfolioEntry]);
    fireEvent.click(screen.getByRole("button", { name: /^Dismiss$/ }));
    const dialog = screen.getByRole("dialog", { name: /Dismiss plan PL-0412/i });
    expect(dialog.textContent).toMatch(/Its 2 points are aborted; nothing is written to Odoo/);
    fireEvent.change(within(dialog).getByLabelText(/Your reason/i), { target: { value: "the customer moved the order" } });
    fireEvent.click(within(dialog).getByRole("button", { name: /Dismiss plan/i }));
    await waitFor(() => expect(commands.abortPortfolio).toHaveBeenCalledTimes(1));
    expect(commands.abortPortfolio).toHaveBeenCalledWith({
      portfolio: "portfolio/0412e7c1d9a3",
      generation: 2,
      reviewer: "Dana Ops",
      reason: "the customer moved the order",
    });
    expect(commands.abort).not.toHaveBeenCalled();
  });

  test("an approved portfolio plan is undone through the portfolio undo", async () => {
    const commands = commandsReturning(async () => ({ ok: true, status: 200 }));
    await renderInbox(commands, [loneEntry], [approvedPortfolio]);
    fireEvent.click(screen.getByRole("button", { name: /^Undo$/ }));
    const dialog = screen.getByRole("dialog", { name: /Undo plan PL-0398/i });
    fireEvent.change(within(dialog).getByLabelText(/Your reason/i), { target: { value: "supplier fell through" } });
    fireEvent.click(within(dialog).getByRole("button", { name: /Undo plan/i }));
    await waitFor(() => expect(commands.undoPortfolio).toHaveBeenCalledTimes(1));
    expect(commands.undoPortfolio).toHaveBeenCalledWith({ portfolio: "portfolio/0398f1e2d3c4", reviewer: "Dana Ops", reason: "supplier fell through" });
    expect(commands.undo).not.toHaveBeenCalled();
  });
});
