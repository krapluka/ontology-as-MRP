import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, waitFor, within } from "storybook/test";
import { RiskInbox, type RiskInboxProps } from "./RiskInbox.tsx";
import type { TabId } from "../data/hooks.ts";
import type { AppliedChange, EntitySelection, InboxItem, Option, Plan } from "../viewmodel/types.ts";
import { itemId } from "../viewmodel/types.ts";
import {
  approved,
  autoApplied,
  emptyPlans,
  entryOf,
  expediteOnlyPolicy,
  FIXTURE_NOW,
  itemOf,
  kpis,
  liveLabels,
  pendingItem,
  pendingPortfolioItem,
  planAtRisk,
  planItems,
  planNoLanes,
  planNoOption,
  plans,
  planWithIds,
  planWithLanes,
  pointOf,
  policy,
  unavailableItem,
  waitingItem,
} from "../fixtures/inbox.ts";
import { fakeClock, fixedClock } from "../fixtures/clock.ts";
import { TimelineTab } from "../components/timeline/TimelineTab.tsx";
import { GraphTab } from "../components/graph/GraphTab.tsx";
import { FlowTab } from "../components/flow/FlowTab.tsx";
import { SuppliersTab } from "../components/suppliers/SuppliersTab.tsx";
import { mapFlow, mapGraph, mapSuppliers, mapTimeline } from "../data/tabMapping.ts";
import { shortId } from "../viewmodel/format.ts";
import { safeStorage } from "../viewmodel/safeStorage.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, RECORDED_ON, recordedPoints, recordedRows, rowsFor } from "../fixtures/scenario.ts";

/** Wires each impact tab to the recorded live projection so the screen's tab nav can be driven
 *  in Storybook without a live read surface (the app injects the `readModel`-backed container
 *  instead). A recorded subject gets its own projection; any other subject is shown the
 *  flagship's, so the inbox fixtures' plans still drive the tabs. */
function renderImpactTabFromFixtures(tab: TabId, subject: string, selection: EntitySelection) {
  const recorded = recordedRows[subject] ? subject : FLAGSHIP_SUBJECT;
  switch (tab) {
    case "timeline":
      return <TimelineTab view={mapTimeline(rowsFor(recorded, "timeline"))} subject={subject} today={`${RECORDED_ON}T00:00:00`} selection={selection} />;
    case "graph":
      return <GraphTab view={mapGraph(rowsFor(recorded, "graph"))} subject={subject} selection={selection} />;
    case "flow":
      return <FlowTab view={mapFlow(rowsFor(recorded, "flow"))} subject={subject} selection={selection} />;
    case "suppliers":
      return (
        <>
          <span data-testid="impact-subject" style={{ display: "none" }}>
            {subject}
          </span>
          <SuppliersTab view={mapSuppliers(rowsFor(recorded, "suppliers"))} subject={subject} selection={selection} />
        </>
      );
  }
}

/** The inbox fixtures' plans re-pointed at the recorded points' subjects (and reasons), each
 *  a single point's plan, so a tab entity can select another listed plan as it would against
 *  the live stack. */
const recordedPlans: Plan[] = recordedPoints.flatMap((point, i) => {
  const plan = plans[i % plans.length];
  return plan ? [{ ...plan, subject: point.subject, resolves: [point.subject], lead: point.subject, situation: point.reason }] : [];
});

/** One portfolio plan over the recorded flagship and cleared points, led by the flagship. */
const recordedPortfolio: InboxItem = itemOf(
  { ...planWithLanes, subject: FLAGSHIP_SUBJECT, resolves: [FLAGSHIP_SUBJECT, CLEARED_SUBJECT], lead: FLAGSHIP_SUBJECT, situation: "The flagship and its sibling order share a short component." },
  {
    id: "plan/0500aaaabbbb",
    code: "0500",
    portfolio: { id: "portfolio/0500aaaabbbb", generation: 1 },
    resolves: [pointOf(FLAGSHIP_SUBJECT, { code: "0501" }), pointOf(CLEARED_SUBJECT, { code: "0502" })],
  },
);

/** The story operator's reviewer identity (entered in the header in the live app). */
const REVIEWER = "Dana Ops";

/** A command spy that reports success (the container resolves `{ ok }` per command). */
const succeeds = () => fn(async () => ({ ok: true }));

const baseArgs = {
  items: planItems(plans),
  autoApplied,
  policy,
  kpis,
  onApprove: succeeds(),
  onDismiss: succeeds(),
  onUndo: succeeds(),
  onUndoPlan: succeeds(),
  notice: null,
  defaultReviewer: REVIEWER,
  clock: fixedClock(FIXTURE_NOW),
  renderImpactTab: renderImpactTabFromFixtures,
};

const meta = {
  title: "Screens/RiskInbox",
  component: RiskInbox,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    safeStorage.clear();
  },
} satisfies Meta<typeof RiskInbox>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = { args: baseArgs };

export const Empty: Story = {
  args: {
    ...baseArgs,
    items: planItems(emptyPlans),
    autoApplied: [],
    kpis: { waiting: 0, revenueAtRisk: null, currency: null, lateOrdersNow: null, lateOrders: null },
  },
};

/** The sourced display framing renders: the header revenue/late-order KPIs, a plan-row
 *  severity fill, the option Delivery/Cost/Risk cells, a labelled change from/to row, and the
 *  auto-applied effect sentence. */
export const DisplayFraming: Story = {
  args: baseArgs,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The header KPIs: revenue at risk in its served currency (compact), late now and if
    // approved.
    await expect(canvas.getByText(/Revenue at risk/i)).toBeInTheDocument();
    await expect(canvas.getByText("$39.2K")).toBeInTheDocument();
    await expect(canvas.getByText(/Late orders if approved/i)).toBeInTheDocument();
    await expect(canvas.getByText("2 → 0")).toBeInTheDocument();
    // The plan rows show the do-nothing baseline's days late (not the recommended option's).
    const battery = rowOf(canvasElement, planNoLanes);
    await expect(battery).toHaveTextContent(/\+24\s*days late/i);
    // The selected plan's detail shows its severity tier, the baseline line and option Delivery.
    await expect(canvas.getAllByText(/Critical/i).length).toBeGreaterThan(0);
    await expect(canvas.getByTestId("baseline-line")).toHaveTextContent("sale.order/147 ships 4 Feb · 4 days late · $18,500 delayed");
    await expect(canvas.getAllByText("On time").length).toBeGreaterThan(0);
    await expect(canvas.getByText("4 d late")).toBeInTheDocument();
    await expect(canvas.getByText("$320")).toBeInTheDocument();
    // A change renders its served label and its from/to (the split's current and new value).
    await expect(canvas.getByText(/WH\/MO\/00318 · 40× City S1/)).toBeInTheDocument();
    await expect(canvas.getByText(/40× City S1 →/i)).toBeInTheDocument();
    // Money follows the served currency: no euro sign anywhere in the inbox.
    await expect(canvasElement.textContent ?? "").not.toMatch(/€/);
  },
};

/** An at-risk plan with no day count (baseline and options serve `at_risk`, slip 0, no
 *  completion): nothing reads "on time", the row shows no days-late number, and the do-nothing
 *  line states the risk and the order value only. */
export const AtRiskPlan: Story = {
  args: {
    ...baseArgs,
    items: planItems([planAtRisk]),
    autoApplied: [],
    kpis: { waiting: 1, revenueAtRisk: "20700.00", currency: "USD", lateOrdersNow: "1", lateOrders: "1" },
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByTestId("baseline-line")).toHaveTextContent("sale.order/1 · at risk · order $20,700");
    await expect(canvas.getAllByText("At risk").length).toBe(planAtRisk.options.length);
    await expect(rowOf(canvasElement, planAtRisk)).not.toHaveTextContent(/days late/i);
    await expect(canvasElement.textContent ?? "").not.toMatch(/on time/i);
  },
};

export const RefusedCommand: Story = {
  args: { ...baseArgs, notice: { kind: "error", message: "Undo refused: the reversal window has closed." } },
};

/** Selecting a plan card loads its slate into the detail column. */
export const SelectPlan: Story = {
  args: baseArgs,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The second plan row is headed by its recommended option's derived label.
    await userEvent.click(canvas.getByText("Expedite · Move · Notify"));
    await expect(canvas.getByRole("heading", { level: 2 })).toHaveTextContent("Expedite · Move · Notify");
    await expect(canvas.getByText(planNoLanes.situation, { selector: "p" })).toBeInTheDocument();
  },
};

/** Picking a different option updates the changes and the Approve label. */
export const PickOption: Story = {
  args: baseArgs,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const optionC = canvas.getByText("Re-promise · Move");
    await userEvent.click(optionC);
    await expect(canvas.getByRole("button", { name: /Approve plan C/i })).toBeInTheDocument();
  },
};

/** Toggling reasoning reveals the reasoning panel. */
export const ToggleReasoning: Story = {
  args: baseArgs,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.queryByText(/How the agent got here/i)).not.toBeInTheDocument();
    await userEvent.click(canvas.getAllByRole("button", { name: /Show reasoning/i })[0]!);
    await expect(canvas.getByText(/How the agent got here/i)).toBeInTheDocument();
  },
};

/** Toggling the lane view switches the segmented control's pressed option. */
export const ToggleLanes: Story = {
  args: baseArgs,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const asPlanned = canvas.getByRole("button", { name: /As planned/i });
    await userEvent.click(asPlanned);
    await expect(asPlanned).toHaveAttribute("aria-pressed", "true");
  },
};

/** Switching plans clears the rationale, so one plan's rationale cannot be recorded with
 *  another plan's promotion in the audit trail. */
export const RationaleResetsOnPlanSwitch: Story = {
  args: baseArgs,
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Rationale meant only for the first plan.");
    await expect(canvas.getByLabelText(/rationale/i)).toHaveValue("Rationale meant only for the first plan.");

    // Switch to the second plan: the rationale must clear.
    await userEvent.click(rowOf(canvasElement, planNoLanes));
    await expect(canvas.getByLabelText(/rationale/i)).toHaveValue("");

    // Approving the second plan records its own rationale, not the first plan's text.
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Air-freight is justified.");
    await userEvent.click(canvas.getByRole("button", { name: /Approve plan/i }));
    await expect(args.onApprove).toHaveBeenLastCalledWith(
      expect.objectContaining({ id: entryOf(planNoLanes).id }),
      expect.anything(),
      "Air-freight is justified.",
      REVIEWER,
    );
  },
};

/** The inbox row of the plan with `id` (its `data-plan` attribute). */
function rowById(canvasElement: HTMLElement, id: string): HTMLElement {
  const rows = [...canvasElement.querySelectorAll<HTMLElement>("[data-plan]")].filter((r) => r.getAttribute("data-plan") === id);
  if (rows.length !== 1) throw new Error(`expected one plan row for ${id}, found ${rows.length}`);
  return rows[0] as HTMLElement;
}

/** The inbox row of a fixture plan (its entry as `entryOf` builds it). */
function rowOf(canvasElement: HTMLElement, plan: Plan): HTMLElement {
  return rowById(canvasElement, entryOf(plan).id);
}

/** The inbox row of the listed plan that resolves `subject`. */
function rowResolving(canvasElement: HTMLElement, items: InboxItem[], subject: string): HTMLElement {
  const item = items.find((i) => i.entry.resolves.some((p) => p.subject === subject));
  if (!item) throw new Error(`no listed plan resolves ${subject}`);
  return rowById(canvasElement, itemId(item));
}

const recordedItems = planItems(recordedPlans);

/** Selecting a tab entity that is the subject of another listed point selects that point, by
 *  keyboard: the Graph re-projects for it, focus moves to the impact panel (the pressed button
 *  is replaced by a plain box), the selection is announced politely, the inbox follows, and no
 *  command is issued. The listed points are the recorded ones, and the flagship is selected
 *  first, so nothing depends on the listing order. */
export const SelectEntityOnTab: Story = {
  // Own spies, so the "no command issued" check cannot see another story's calls.
  args: { ...baseArgs, items: recordedItems, onApprove: succeeds(), onDismiss: succeeds(), onUndo: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(rowResolving(canvasElement, recordedItems, FLAGSHIP_SUBJECT));
    await userEvent.click(canvas.getByRole("button", { name: /^Graph$/i }));
    // The flagship point is selected; the other point's order is a selectable (button) node.
    const other = canvasElement.querySelector(`[data-entity="${CLEARED_SUBJECT}"]`);
    await expect(other?.tagName).toBe("BUTTON");
    await expect(canvasElement.querySelector(`[data-entity="${FLAGSHIP_SUBJECT}"]`)?.tagName).toBe("DIV");
    if (!(other instanceof HTMLElement)) throw new Error("the other point's order is not drawn");
    // Keyboard only: focus the node and press Enter.
    other.focus();
    await userEvent.keyboard("{Enter}");

    // Focus lands on the impact panel (not on <body>), which is labelled for the clicked
    // point, and the polite live region announces the selection.
    const panel = canvas.getByRole("region", { name: `Impact of ${shortId(CLEARED_SUBJECT)}` });
    await expect(document.activeElement).toBe(panel);
    await expect(canvas.getByTestId("selection-announcement")).toHaveTextContent(`Selected ${shortId(CLEARED_SUBJECT)}`);
    await expect(canvas.getByTestId("selection-announcement")).toHaveAttribute("aria-live", "polite");

    // Every tab projects the clicked point: the Graph shows its (cleared-rule) status.
    await expect(canvas.getByRole("status")).toHaveTextContent(/The rule no longer flags this order/);
    await expect(canvasElement.querySelector(`[data-entity="${CLEARED_SUBJECT}"]`)?.getAttribute("data-path")).toBe("on");
    await userEvent.click(canvas.getByRole("button", { name: /^Suppliers$/i }));
    await expect(canvas.getByTestId("impact-subject")).toHaveTextContent(CLEARED_SUBJECT);

    // The inbox follows the selection; nothing was committed.
    await userEvent.click(canvas.getByRole("button", { name: /^Risk inbox$/i }));
    await expect(rowResolving(canvasElement, recordedItems, CLEARED_SUBJECT)).toHaveAttribute("aria-pressed", "true");
    await expect(rowResolving(canvasElement, recordedItems, FLAGSHIP_SUBJECT)).toHaveAttribute("aria-pressed", "false");
    await expect(args.onApprove).not.toHaveBeenCalled();
    await expect(args.onDismiss).not.toHaveBeenCalled();
    await expect(args.onUndo).not.toHaveBeenCalled();
  },
};

/** The header tabs switch the main view between the Risk inbox and the four impact tabs, each
 *  driven by the selected point's subject via `readModel` (here fixture-backed). Selection is
 *  persisted in `localStorage` (`pp-view`). */
export const SwitchTabs: Story = {
  args: baseArgs,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Start on the Risk inbox hero.
    await expect(canvas.getByRole("heading", { level: 1 })).toHaveTextContent(/plans? need/i);

    // Timeline: the selected subject's orders vs promise dates.
    await userEvent.click(canvas.getByRole("button", { name: /^Timeline$/i }));
    await expect(canvas.getByText(/Promise timeline/i)).toBeInTheDocument();

    // Graph: the six-column supply graph and the selected point's path panel.
    await userEvent.click(canvas.getByRole("button", { name: /^Graph$/i }));
    await expect(canvas.getByText(/Selected point/i)).toBeInTheDocument();

    // Flow: the five pipeline columns.
    await userEvent.click(canvas.getByRole("button", { name: /^Flow$/i }));
    await expect(canvas.getByText(/chain across stages/i)).toBeInTheDocument();

    // Suppliers: the lead/on-time/exposure table, driven by the selected subject.
    await userEvent.click(canvas.getByRole("button", { name: /^Suppliers$/i }));
    await expect(canvas.getByText(/On-time · 90 d/i)).toBeInTheDocument();
    await expect(canvas.getByTestId("impact-subject")).toHaveTextContent(plans[0]!.subject);

    // Back to the inbox.
    await userEvent.click(canvas.getByRole("button", { name: /^Risk inbox$/i }));
    await expect(canvas.getByRole("heading", { level: 1 })).toBeInTheDocument();
  },
};

/** Approving issues the command with the reviewer's rationale, carrying the selected option.
 *  Its `rankKey` is what the container forwards as `option_rank_key`, so the promotion
 *  commits the option the operator sees. */
export const ApproveWithRationale: Story = {
  args: baseArgs,
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const approve = canvas.getByRole("button", { name: /Approve plan A/i });
    await expect(approve).toBeDisabled();
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Expedite is worth the freight cost.");
    await expect(approve).toBeEnabled();
    await userEvent.click(approve);
    // The selected (recommended) option A rides the approval, carrying its stable rankKey.
    await expect(args.onApprove).toHaveBeenLastCalledWith(
      expect.anything(),
      expect.objectContaining({ key: "A", rankKey: "00000" }),
      "Expedite is worth the freight cost.",
      REVIEWER,
    );
    // A successful approval clears the rationale it carried.
    await waitFor(() => expect(canvas.getByLabelText(/rationale/i)).toHaveValue(""));
  },
};

/** Switching the option before approving changes which option (and thus which `rankKey`)
 *  the promotion carries: the operator's choice is what gets committed, not a default. */
export const ApproveAfterSwitchingOption: Story = {
  args: baseArgs,
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    // Pick plan C (Re-promise) instead of the recommended A.
    await userEvent.click(canvas.getByText("Re-promise · Move"));
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Re-promise is the safer call.");
    await userEvent.click(canvas.getByRole("button", { name: /Approve plan C/i }));
    await expect(args.onApprove).toHaveBeenLastCalledWith(
      expect.anything(),
      expect.objectContaining({ key: "C", rankKey: "00002" }),
      "Re-promise is the safer call.",
      REVIEWER,
    );
  },
};

// --- bound state, rows, commands ---

/** The inbox plus a "Simulate refresh" control that swaps in the next read. This is the same
 *  re-render a refetch after a command causes in the live container. */
function Refreshing({ next, ...props }: RiskInboxProps & { next: InboxItem[] }) {
  const [items, setItems] = useState(props.items);
  return (
    <>
      <RiskInbox {...props} items={items} />
      <button type="button" onClick={() => setItems(next)}>
        Simulate refresh
      </button>
    </>
  );
}

/** Relabel options A/B/C… by position, as the mapping does after a re-rank. */
function relettered(options: Option[]): Option[] {
  return options.map((o, i) => ({ ...o, key: String.fromCharCode(65 + i) }));
}

/** A new best option ranked ahead of the rest: every existing option moves one letter down. */
const newBest: Option = { ...planWithLanes.options[1]!, rankKey: "00000~0a1b2c3d4e5f", label: "Start earlier", recommended: true };
const planReRanked: Plan = {
  ...planWithLanes,
  options: relettered([newBest, ...planWithLanes.options.map((o) => ({ ...o, recommended: false }))]),
};
/** The slate after a refresh that dropped option B (`00001`). */
const planWithoutB: Plan = { ...planWithLanes, options: relettered(planWithLanes.options.filter((o) => o.rankKey !== "00001")) };

/** A rationale typed for plan B does not follow a refresh that removes B: the view moves the
 *  selection to the plan it shows afterwards (C), with an empty rationale, its own default
 *  option, and Approve disabled, so B's rationale cannot ride C's promotion. */
export const RationaleDoesNotFollowRefetch: Story = {
  args: { ...baseArgs, items: planItems([planWithLanes, planNoLanes, planAtRisk]), onApprove: succeeds() },
  render: (args) => <Refreshing {...args} next={planItems([planAtRisk, planWithLanes])} />,
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(rowOf(canvasElement, planNoLanes));
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Air-freight is justified for B.");
    await userEvent.click(canvas.getByRole("button", { name: /Simulate refresh/i }));

    await expect(canvas.getByText(planAtRisk.situation, { selector: "p" })).toBeInTheDocument();
    await expect(rowOf(canvasElement, planAtRisk)).toHaveAttribute("aria-pressed", "true");
    await expect(canvas.getByLabelText(/rationale/i)).toHaveValue("");
    await expect(canvas.getByRole("button", { name: /Approve plan A/i })).toBeDisabled();
    await expect(args.onApprove).not.toHaveBeenCalled();
    // The selection itself moved to C: the impact tabs follow the plan on screen, not B.
    await userEvent.click(canvas.getByRole("button", { name: /^Suppliers$/i }));
    // Exact match: `sale.order/1` is a prefix of `sale.order/142` (plan B).
    await expect(canvas.getByTestId("impact-subject").textContent).toBe(planAtRisk.subject);
  },
};

/** A refused approval keeps the rationale the operator typed (it is cleared only on success). */
const refusedApprove = fn(async () => ({ ok: false }));
export const ApproveRefusedKeepsRationale: Story = {
  args: { ...baseArgs, onApprove: refusedApprove },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Expedite is worth it.");
    await userEvent.click(canvas.getByRole("button", { name: /Approve plan A/i }));
    await expect(refusedApprove).toHaveBeenCalledTimes(1);
    // Let the refused outcome settle (and any state update it causes land), then the typed
    // rationale is still there.
    await refusedApprove.mock.results[0]?.value;
    await new Promise((resolve) => setTimeout(resolve, 0));
    await expect(canvas.getByLabelText(/rationale/i)).toHaveValue("Expedite is worth it.");
  },
};

/** The option is remembered by its rankKey: after a refresh re-ranks the slate (a new best
 *  option pushes the chosen one from B to C), the selection stays on the same option and
 *  approving carries the same rankKey. */
export const SelectionFollowsReRank: Story = {
  args: { ...baseArgs, items: planItems([planWithLanes]), onApprove: succeeds() },
  render: (args) => <Refreshing {...args} next={planItems([planReRanked])} />,
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByText("Expedite · Raise capacity · Move"));
    await expect(canvas.getByRole("button", { name: /Approve plan B/i })).toBeInTheDocument();
    await userEvent.click(canvas.getByRole("button", { name: /Simulate refresh/i }));

    // Same option (rankKey 00001), in position C after the re-rank, not the new recommended A.
    await userEvent.type(canvas.getByLabelText(/rationale/i), "The Saturday shift is still the call.");
    await userEvent.click(canvas.getByRole("button", { name: /Approve plan C/i }));
    await expect(args.onApprove).toHaveBeenLastCalledWith(
      expect.objectContaining({ id: entryOf(planWithLanes).id }),
      expect.objectContaining({ key: "C", rankKey: "00001" }),
      "The Saturday shift is still the call.",
      REVIEWER,
    );
  },
};

/** A chosen option that a refresh removed is reported as lost. Approve stays disabled until
 *  the operator chooses again; it does not fall back to another option. */
export const SelectionLost: Story = {
  args: { ...baseArgs, items: planItems([planWithLanes]), onApprove: succeeds() },
  render: (args) => <Refreshing {...args} next={planItems([planWithoutB])} />,
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByText("Expedite · Raise capacity · Move"));
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Saturday shift.");
    await userEvent.click(canvas.getByRole("button", { name: /Simulate refresh/i }));

    await expect(canvas.getByText("Your selected option is no longer in this plan; choose again.")).toBeInTheDocument();
    const approve = canvas.getByRole("button", { name: /^Approve$/ });
    await expect(approve).toBeDisabled();
    await userEvent.click(approve);
    await expect(args.onApprove).not.toHaveBeenCalled();
    // No option card is shown as selected.
    for (const card of canvas.getAllByRole("button", { pressed: true })) {
      await expect(card).not.toHaveTextContent(/^Plan /);
    }

    // Choosing again re-enables Approve for the chosen option.
    await userEvent.click(canvas.getByText("Re-promise · Move"));
    await expect(canvas.queryByText(/no longer in this plan/)).not.toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: /Approve plan B/i })).toBeEnabled();
  },
};

/** A plan with no option opens: its baseline and reasoning (with the Generation steps), and
 *  Dismiss, but no Approve. */
export const NoOptionPlanOpens: Story = {
  args: { ...baseArgs, items: planItems([planWithLanes, planNoOption]) },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const row = rowOf(canvasElement, planNoOption);
    await expect(within(row).getByText("no option")).toBeInTheDocument();
    await userEvent.click(row);
    await expect(canvas.getByRole("heading", { level: 2 })).toHaveTextContent(planNoOption.situation);
    await expect(canvas.getByTestId("baseline-line")).toHaveTextContent("sale.order/160 ships 10 Feb · 6 days late · $9,400 delayed");
    await expect(canvas.getAllByText("Set aside.")).toHaveLength(2);
    await expect(canvas.getByText(/attempt 1: rate_limited/)).toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: /^Dismiss$/ })).toBeEnabled();
    await expect(canvas.queryByRole("button", { name: /Approve/i })).not.toBeInTheDocument();
  },
};

/** A point whose slate is not attached yet is listed ("slate pending"), counted, and opens to
 *  a panel that says so and offers Dismiss. */
export const SlatePendingRow: Story = {
  args: { ...baseArgs, items: [...planItems([planWithLanes]), pendingItem], kpis: { ...kpis, waiting: 2 } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const row = rowById(canvasElement, pendingItem.entry.id);
    await expect(within(row).getByText("slate pending")).toBeInTheDocument();
    await userEvent.click(row);
    await expect(canvas.getByText(/has not attached a plan yet/)).toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: /^Dismiss$/ })).toBeEnabled();
    await expect(canvas.queryByRole("button", { name: /Approve/i })).not.toBeInTheDocument();
  },
};

/** A point whose plan read failed is listed ("plan unavailable") with the failure, and can be
 *  dismissed. */
export const PlanUnavailableRow: Story = {
  args: { ...baseArgs, items: [...planItems([planWithLanes]), unavailableItem], kpis: { ...kpis, waiting: 2 } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const row = rowById(canvasElement, unavailableItem.entry.id);
    await expect(within(row).getByText("plan unavailable")).toBeInTheDocument();
    await userEvent.click(row);
    await expect(canvas.getByText(/could not be read: plan read timed out/)).toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: /^Dismiss$/ })).toBeEnabled();
    await expect(canvas.queryByRole("button", { name: /Approve/i })).not.toBeInTheDocument();
  },
};

/** Each option carries its own part of the reasoning. */
const planWithOptionTraces: Plan = {
  ...planWithLanes,
  options: planWithLanes.options.map((o) => ({
    ...o,
    trace: {
      summary: { simulated: 1, rejected: 0, openAssumptions: 0 },
      steps: [{ tag: "Simulated", text: `Plan ${o.key} on its own: ${o.label} simulated end to end.`, rejected: false }],
    },
  })),
};

/** With reasoning shown, the selected option's own trace is shown next to the plan's, and it
 *  follows the option picked. */
export const OptionReasoningShown: Story = {
  args: { ...baseArgs, items: planItems([planWithOptionTraces]) },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: /Show reasoning/i }));
    await expect(canvas.getByText("How the agent got here")).toBeInTheDocument();
    await expect(canvas.getByText("This plan's reasoning · plan A")).toBeInTheDocument();
    await expect(canvas.getByText("Plan A on its own: Split · Move · Expedite simulated end to end.")).toBeInTheDocument();

    await userEvent.click(canvas.getByText("Re-promise · Move"));
    await expect(canvas.getByText("This plan's reasoning · plan C")).toBeInTheDocument();
    await expect(canvas.getByText("Plan C on its own: Re-promise · Move simulated end to end.")).toBeInTheDocument();
    await expect(canvas.queryByText(/Plan A on its own/)).not.toBeInTheDocument();
  },
};

/** Dismiss asks for a confirmed reason: nothing is issued while it is empty, and the typed
 *  reason rides the dismiss command. */
export const DismissAsksForReason: Story = {
  args: { ...baseArgs, onDismiss: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: /^Dismiss$/ }));
    const dialog = canvas.getByRole("dialog", { name: "Dismiss plan PL-0412?" });
    await expect(dialog).toHaveTextContent("Its 2 points are aborted; nothing is written to Odoo.");
    const confirm = within(dialog).getByRole("button", { name: /Dismiss plan/i });
    await expect(confirm).toBeDisabled();
    await expect(args.onDismiss).not.toHaveBeenCalled();

    await userEvent.type(within(dialog).getByLabelText(/Your reason/i), "supplier confirmed by phone");
    await userEvent.click(confirm);
    await expect(args.onDismiss).toHaveBeenCalledWith(
      expect.objectContaining({ id: entryOf(planWithLanes).id, portfolio: { id: "portfolio/0412e7c1d9a3", generation: 2 } }),
      "supplier confirmed by phone",
      REVIEWER,
    );
    await expect(canvas.queryByRole("dialog")).not.toBeInTheDocument();
  },
};

/** Undo asks for a confirmed reason too, and a cancelled confirmation issues nothing. */
export const UndoAsksForReason: Story = {
  args: { ...baseArgs, onUndo: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getAllByRole("button", { name: /^Undo$/ })[0]!);
    await userEvent.click(within(canvas.getByRole("dialog")).getByRole("button", { name: /Cancel/i }));
    await expect(args.onUndo).not.toHaveBeenCalled();

    await userEvent.click(canvas.getAllByRole("button", { name: /^Undo$/ })[0]!);
    await userEvent.type(canvas.getByLabelText(/Your reason/i), "customer cancelled the order");
    await userEvent.click(canvas.getByRole("button", { name: /Undo change/i }));
    await expect(args.onUndo).toHaveBeenCalledWith(expect.objectContaining({ subject: autoApplied[0]!.subject }), "customer cancelled the order", REVIEWER);
  },
};

/** No reviewer identity entered: every command control is disabled and the view asks for the
 *  identity; entering it enables them. */
export const NoReviewerNoCommands: Story = {
  args: { ...baseArgs, defaultReviewer: undefined, onApprove: succeeds(), onDismiss: succeeds(), onUndo: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByLabelText(/^Reviewer$/)).toHaveValue("");
    await expect(canvas.getAllByText("Enter your reviewer name to act").length).toBeGreaterThan(0);
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Expedite is worth it.");
    await expect(canvas.getByRole("button", { name: /Approve plan A/i })).toBeDisabled();
    await expect(canvas.getByRole("button", { name: /^Dismiss$/ })).toBeDisabled();
    for (const undo of canvas.getAllByRole("button", { name: /^Undo$/ })) await expect(undo).toBeDisabled();
    await userEvent.click(canvas.getAllByText("Expedite")[0]!);
    // The detail names the change and the point it closed by their served codes.
    await expect(canvas.getByText("AU-0117 · closes AP-0238")).toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: /Undo change/i })).toBeDisabled();
    await expect(args.onApprove).not.toHaveBeenCalled();
    await expect(args.onDismiss).not.toHaveBeenCalled();
    await expect(args.onUndo).not.toHaveBeenCalled();

    // Entering the identity enables the commands.
    await userEvent.type(canvas.getByLabelText(/^Reviewer$/), REVIEWER);
    await expect(canvas.queryByText("Enter your reviewer name to act")).not.toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: /Undo change/i })).toBeEnabled();
  },
};

/** A local noon, so "today" and "yesterday" are the same in every time zone. */
const NOON = new Date(2027, 0, 2, 12, 0, 0).getTime();
const epoch = (ms: number) => String(Math.floor(ms / 1000));
const HOUR = 3_600_000;

function applied(subject: string, appliedAt: number, undoUntil: number, title: string): AppliedChange {
  return { ...autoApplied[0]!, subject: `https://ekg.example/id/sale.order/${subject}`, appliedAt: epoch(appliedAt), undoUntil: epoch(undoUntil), title };
}

/** The inbox with a fake clock and a control that advances it. */
function WithFakeClock(props: RiskInboxProps) {
  const [clock] = useState(() => fakeClock(NOON));
  return (
    <>
      <RiskInbox {...props} clock={clock} />
      <button type="button" onClick={() => clock.advance(60_000)}>
        Advance the clock one minute
      </button>
    </>
  );
}

/** Undo is offered only while the reversal window is open, checked live: when the window
 *  closes while the inbox is open, Undo disappears without a reload and the view says when the
 *  window closed. */
export const UndoWindowCloses: Story = {
  args: { ...baseArgs, items: [], kpis: { ...kpis, waiting: 0 }, autoApplied: [applied("181", NOON - HOUR, NOON + 20_000, "Expedite")] },
  render: (args) => <WithFakeClock {...args} />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("button", { name: /^Undo$/ })).toBeEnabled();
    await expect(canvas.getByRole("button", { name: /Undo change/i })).toBeEnabled();
    await expect(canvas.getByText(/^Undo available until /)).toBeInTheDocument();

    await userEvent.click(canvas.getByRole("button", { name: /Advance the clock one minute/i }));
    await expect(canvas.queryByRole("button", { name: /^Undo$/ })).not.toBeInTheDocument();
    await expect(canvas.queryByRole("button", { name: /Undo change/i })).not.toBeInTheDocument();
    await expect(canvas.getAllByText(/^reversal window closed at /).length).toBe(2);
  },
};

/** The list shows today's changes plus earlier ones still undoable (with their date); the
 *  "applied today" KPI counts today's only. */
export const AppliedTodayOnly: Story = {
  args: {
    ...baseArgs,
    clock: fixedClock(NOON),
    autoApplied: [
      applied("190", NOON - 24 * HOUR, NOON - 16 * HOUR, "Closed yesterday"),
      applied("191", NOON - 20 * HOUR, NOON + HOUR, "Open from yesterday"),
      applied("192", NOON - HOUR, NOON + 7 * HOUR, "Applied this morning"),
    ],
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.queryByText("Closed yesterday")).not.toBeInTheDocument();
    await expect(canvas.getByText("Open from yesterday")).toBeInTheDocument();
    await expect(canvas.getByText("Applied this morning")).toBeInTheDocument();
    await expect(canvas.getByText("Applied by agent today").previousElementSibling).toHaveTextContent(/^1$/);
    // The earlier change shows its date.
    await expect(canvas.getByText("Open from yesterday").nextElementSibling).toHaveTextContent(/^1 Jan /);
  },
};

/** A failed refresh keeps the inbox and says it may be stale. */
export const StaleAfterFailedRefresh: Story = {
  args: { ...baseArgs, refreshError: "network error" },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("status")).toHaveTextContent(/Could not refresh the inbox \(network error\).*may be stale/);
    await expect(canvas.getByRole("heading", { level: 2 })).toBeInTheDocument();
  },
};

// --- plans as rows ---

/** Approving a portfolio plan issues one approval for the plan, carrying its portfolio and the
 *  generation shown and the selected option (the container sends it to the portfolio endpoint). */
export const ApprovePortfolioPlan: Story = {
  args: { ...baseArgs, items: [itemOf(planWithLanes)], onApprove: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Split and expedite.");
    await userEvent.click(canvas.getByRole("button", { name: /Approve plan A · 4 changes/i }));
    await expect(args.onApprove).toHaveBeenCalledTimes(1);
    await expect(args.onApprove).toHaveBeenCalledWith(
      expect.objectContaining({ id: "plan/0412e7c1d9a3", portfolio: { id: "portfolio/0412e7c1d9a3", generation: 2 } }),
      expect.objectContaining({ rankKey: "00000" }),
      "Split and expedite.",
      REVIEWER,
    );
  },
};

/** A single point's plan approves with no portfolio (the container keeps the point command). */
export const LonePlanKeepsPointCommands: Story = {
  args: { ...baseArgs, items: [itemOf(planNoLanes)], onApprove: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Air-freight.");
    await userEvent.click(canvas.getByRole("button", { name: /Approve plan A/i }));
    await expect(args.onApprove).toHaveBeenCalledWith(expect.objectContaining({ portfolio: null }), expect.anything(), "Air-freight.", REVIEWER);
  },
};

/** Approve waits while a point of the plan is not `proposed`, and says which one and why. */
export const ApproveWaitsForMember: Story = {
  args: { ...baseArgs, items: [waitingItem], onApprove: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/rationale/i), "Split and expedite.");
    const approve = canvas.getByRole("button", { name: /Approve plan A/i });
    await expect(approve).toBeDisabled();
    await expect(canvas.getByText("Approve is waiting for AP-0239 (open)")).toBeInTheDocument();
    await expect(within(rowById(canvasElement, itemId(waitingItem))).getByText("waiting for AP-0239 (open)")).toBeInTheDocument();
    await userEvent.click(approve);
    await expect(args.onApprove).not.toHaveBeenCalled();
  },
};

/** A portfolio whose joint slate is pending is listed and counted, resolving both points. */
export const PendingPortfolioListed: Story = {
  args: { ...baseArgs, items: [itemOf(planWithLanes), pendingPortfolioItem], kpis: { ...kpis, waiting: 2 } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("heading", { level: 1 })).toHaveTextContent("2 plans need your approval");
    const row = rowById(canvasElement, pendingPortfolioItem.entry.id);
    await expect(row).toHaveTextContent("slate pending");
    await expect(row).toHaveTextContent("Resolves AP-0240 · AP-0241");
    await userEvent.click(row);
    await expect(canvas.getByText(/has not attached a plan yet/)).toBeInTheDocument();
  },
};

/** The copy on the PL-0412-shaped plan: the row, the detail header, the plan cards, the
 *  changes header, Approve, the needs in words, the risk words, the reasoning summary and
 *  "If we do nothing", with no " · needs you" and no raw verb code. */
export const PlanCopy: Story = {
  args: { ...baseArgs, items: planItems(plans) },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const row = rowOf(canvasElement, planWithLanes);
    await expect(row).toHaveTextContent("PL-04122 orders · 2 work centers · 1 PO");
    await expect(canvas.getByText("PL-0412 · 2 orders · 2 work centers · 1 PO")).toBeInTheDocument();
    await expect(canvas.getByRole("heading", { level: 2 })).toHaveTextContent("Split · Move · Expedite");
    await expect(canvas.getByText(planWithLanes.situation, { selector: "p" })).toBeInTheDocument();
    await expect(canvas.getByTestId("baseline-line")).toHaveTextContent("sale.order/147 ships 4 Feb · 4 days late · $18,500 delayed");
    await expect(canvas.getAllByText("Plan A · 4 changes").length).toBe(1);
    await expect(canvas.getByText("Plan B · 3 changes")).toBeInTheDocument();
    await expect(canvas.getByText("Plan A · 4 changes · approved together")).toBeInTheDocument();
    await expect(canvas.getByRole("button", { name: "Approve plan A · 4 changes" })).toBeInTheDocument();
    await expect(canvas.getByText("$320 is above the $250 limit")).toBeInTheDocument();
    await expect(canvas.getByText("Splits a manufacturing order")).toBeInTheDocument();
    await expect(canvas.getByText("Low")).toBeInTheDocument();
    await expect(canvas.getAllByText("Medium").length).toBe(2);
    await expect(canvasElement.textContent ?? "").not.toMatch(/needs you$| · needs you/);
    await expect(canvasElement.textContent ?? "").not.toMatch(/split_production|expedite_po|reschedule_work_order/);
    await userEvent.click(canvas.getByRole("button", { name: /Show reasoning/i }));
    await expect(canvas.getAllByText("5 plans simulated · 2 rejected · 1 open assumption").length).toBeGreaterThan(0);
  },
};

/** The PL-0412 plan as the live read serves it: every entity id in its situation, trace,
 *  lanes, change rows and "If we do nothing" reads as its served label, with the id on
 *  hover, and no `https://` text is visible. Dates read "24 Feb → 11 Jan", a `1.0` reads "1",
 *  and the not-pre-approved needs read as one chip. */
export const PlanNames: Story = {
  args: { ...baseArgs, items: [itemOf(planWithIds)], policy: expediteOnlyPolicy, labels: liveLabels },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: /Show reasoning/i }));
    await expect(canvas.getByTestId("situation")).toHaveTextContent(
      "linked: WH/MO/00318 and WH/MO/00319 contend for work center Assembly Line beyond its capacity 1; S00147 · Stadtrad Leasing ships late.",
    );
    await expect(canvas.getByTestId("baseline-line")).toHaveTextContent("S00147 · Stadtrad Leasing ships 4 Feb · 4 days late · $18,500 delayed");
    // The trace's ids read as labels; its sentences are the capability's wording.
    await expect(canvasElement.textContent ?? "").toContain("read WH/MO/00318 ekg:produced City S1");
    await expect(canvasElement.textContent ?? "").toContain("Linked: WH/MO/00318, WH/MO/00319, S00147 · Stadtrad Leasing — contend for work center Assembly Line.");
    const fromTo = canvas.getAllByTestId("change-from-to").map((el) => el.textContent);
    await expect(fromTo).toEqual(["1 → 3", "Assembly Line → Paint Booth", "Assembly Line → Paint Booth", "24 Feb → 11 Jan"]);
    await expect(canvas.getAllByTestId("lane-name").map((el) => el.textContent)).toEqual(["Assembly Line", "Paint Booth"]);
    const chips = canvas.getAllByTestId("need-chip").map((el) => el.textContent);
    await expect(chips).toEqual(["Not pre-approved: Raise capacity · Reassign"]);
    // No id is visible as text; every id the screen names is on its element's title.
    await expect(canvasElement.textContent ?? "").not.toContain("https://");
    const named = [...canvasElement.querySelectorAll<HTMLElement>("[data-entity]")];
    await expect(named.length).toBeGreaterThan(0);
    for (const el of named) await expect(el.title).toContain(el.dataset.entity ?? "\u0000");
  },
};

/** The header clock reads the viewer's clock (`Ddd D Mon · HH:MM`) and ticks with it. */
export const HeaderClock: Story = {
  args: { ...baseArgs },
  render: (args) => <WithFakeClock {...args} />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByTestId("header-clock")).toHaveTextContent("Sat 2 Jan · 12:00");
    await userEvent.click(canvas.getByRole("button", { name: /Advance the clock one minute/i }));
    await expect(canvas.getByTestId("header-clock")).toHaveTextContent("Sat 2 Jan · 12:01");
  },
};

// --- "Approved by you today" ---

/** An approved plan is listed with Undo while its window is open; Undo asks for a reason and
 *  issues the plan's undo with it. */
export const ApprovedUndo: Story = {
  args: { ...baseArgs, approved, onUndoPlan: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText("Approved by you today")).toBeInTheDocument();
    const row = canvasElement.querySelector<HTMLElement>('[data-approved="plan/0398f1e2d3c4"]');
    if (!row) throw new Error("the approved plan is not listed");
    await expect(row).toHaveTextContent("PL-0398");
    await expect(row).toHaveTextContent("Resolves AP-0229 · AP-0228");
    await userEvent.click(within(row).getByRole("button", { name: /^Undo$/ }));
    const dialog = canvas.getByRole("dialog", { name: "Undo plan PL-0398?" });
    const confirm = within(dialog).getByRole("button", { name: /Undo plan/i });
    await expect(confirm).toBeDisabled();
    await userEvent.type(within(dialog).getByLabelText(/Your reason/i), "the supplier fell through");
    await userEvent.click(confirm);
    await expect(args.onUndoPlan).toHaveBeenCalledWith(expect.objectContaining({ id: "plan/0398f1e2d3c4" }), "the supplier fell through", REVIEWER);
  },
};

/** A plan whose undo reversed only part of it says how many changes remain, and keeps Undo. */
export const ApprovedPartlyUndone: Story = {
  args: { ...baseArgs, approved },
  play: async ({ canvasElement }) => {
    const row = canvasElement.querySelector<HTMLElement>('[data-approved="plan/0406c8d9e0f1"]');
    if (!row) throw new Error("the approved plan is not listed");
    await expect(row).toHaveTextContent("partly undone · 1 left");
    await expect(within(row).getByRole("button", { name: /^Undo$/ })).toBeEnabled();
    // A plan with no undo yet says nothing about remaining changes.
    const other = canvasElement.querySelector<HTMLElement>('[data-approved="plan/0398f1e2d3c4"]');
    await expect(other).not.toHaveTextContent(/partly undone/);
  },
};

/** Undo disappears from an approved plan when its window closes while the inbox is open. */
export const ApprovedUndoWindowCloses: Story = {
  args: {
    ...baseArgs,
    items: [],
    autoApplied: [],
    kpis: { ...kpis, waiting: 0 },
    approved: [{ ...approved[0]!, promotedAt: epoch(NOON - HOUR), undoUntil: epoch(NOON + 20_000) }],
  },
  render: (args) => <WithFakeClock {...args} />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("button", { name: /^Undo$/ })).toBeEnabled();
    await userEvent.click(canvas.getByRole("button", { name: /Advance the clock one minute/i }));
    await expect(canvas.queryByRole("button", { name: /^Undo$/ })).not.toBeInTheDocument();
    await expect(canvas.getByText(/^reversal window closed at /)).toBeInTheDocument();
  },
};

// --- tab selection follows plans ---

/** A portfolio's other point is selectable on a tab: selecting it keeps its portfolio plan
 *  selected and shows that point on the tabs; the Graph knows it is still an open point. */
export const PortfolioMemberOnTab: Story = {
  args: { ...baseArgs, items: [recordedPortfolio], onApprove: succeeds(), onDismiss: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: /^Graph$/i }));
    // The plan's lead point (the flagship) drives the tabs; its sibling point is selectable.
    const sibling = canvasElement.querySelector(`[data-entity="${CLEARED_SUBJECT}"]`);
    await expect(sibling?.tagName).toBe("BUTTON");
    if (!(sibling instanceof HTMLElement)) throw new Error("the sibling point's order is not drawn");
    await userEvent.click(sibling);
    await expect(canvas.getByRole("region", { name: `Impact of ${shortId(CLEARED_SUBJECT)}` })).toBeInTheDocument();
    await expect(canvas.getByRole("status")).toHaveTextContent(/still open for your decision/);
    // The inbox still shows the same portfolio plan selected; nothing was committed.
    await userEvent.click(canvas.getByRole("button", { name: /^Risk inbox$/i }));
    await expect(rowById(canvasElement, itemId(recordedPortfolio))).toHaveAttribute("aria-pressed", "true");
    await expect(args.onApprove).not.toHaveBeenCalled();
    await expect(args.onDismiss).not.toHaveBeenCalled();
  },
};

/** The Suppliers detail offers "Open action point · AP-XXXX" for a dependent order a listed
 *  plan resolves; "Review options" switches to the inbox with that plan selected. */
export const SupplierOpensThePlan: Story = {
  args: { ...baseArgs, items: recordedItems, onApprove: succeeds() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(rowResolving(canvasElement, recordedItems, CLEARED_SUBJECT));
    await userEvent.click(canvas.getByRole("button", { name: /^Suppliers$/i }));
    await expect(canvas.getByText(/^Open action point · AP-[0-9A-F]{4}$/)).toBeInTheDocument();
    await userEvent.click(canvas.getByRole("button", { name: "Review options" }));
    await expect(canvas.getByRole("heading", { level: 1 })).toHaveTextContent(/plans? need/);
    await expect(rowResolving(canvasElement, recordedItems, FLAGSHIP_SUBJECT)).toHaveAttribute("aria-pressed", "true");
    await expect(args.onApprove).not.toHaveBeenCalled();
  },
};

/** No listed plan resolves a supplier's dependent orders: no "Open action point" link. */
export const SupplierWithoutPlan: Story = {
  args: { ...baseArgs },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: /^Suppliers$/i }));
    await expect(canvas.getByText(/On-time · 90 d/i)).toBeInTheDocument();
    await expect(canvas.queryByText(/Open action point/)).not.toBeInTheDocument();
    await expect(canvas.queryByRole("button", { name: "Review options" })).not.toBeInTheDocument();
  },
};
