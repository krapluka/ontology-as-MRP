import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import { PlanRow } from "./PlanRow.tsx";
import { itemId, type InboxItem } from "../../viewmodel/types.ts";
import {
  FIXTURE_NOW,
  itemOf,
  pendingItem,
  pendingPortfolioItem,
  planAtRisk,
  planNoLanes,
  planNoOption,
  planWithLanes,
  unavailableItem,
  waitingItem,
} from "../../fixtures/inbox.ts";

const meta = {
  title: "Inbox/PlanRow",
  component: PlanRow,
  parameters: { layout: "padded" },
  decorators: [(Story) => <div style={{ maxWidth: 440 }}>{Story()}</div>],
} satisfies Meta<typeof PlanRow>;

export default meta;
type Story = StoryObj<typeof meta>;

const base = { selected: false, now: FIXTURE_NOW, onSelect: () => {} };

/** PL-0412: a portfolio plan resolving two points. */
export const Portfolio: Story = {
  args: { ...base, item: itemOf(planWithLanes) },
  play: async ({ canvasElement }) => {
    const row = within(canvasElement).getByRole("button");
    await expect(row).toHaveTextContent("PL-0412");
    await expect(row).toHaveTextContent("2 orders · 2 work centers · 1 PO");
    await expect(row).toHaveTextContent("Resolves AP-0239 · AP-0233");
    await expect(row).toHaveTextContent("proposed 14 min ago");
    await expect(row).toHaveTextContent(/\+4\s*days late/);
    await expect(row).toHaveTextContent("Split · Move · Expedite");
    // The full plan id is the row's label (and its title).
    await expect(row).toHaveAttribute("title", "plan/0412e7c1d9a3");
    await expect(row.getAttribute("aria-label")).toContain("plan/0412e7c1d9a3");
  },
};

/** PL-0409: a single point's plan is a plan row too, its scope in the fixed order. */
export const Lone: Story = {
  args: { ...base, item: itemOf(planNoLanes) },
  play: async ({ canvasElement }) => {
    const row = within(canvasElement).getByRole("button");
    await expect(row).toHaveTextContent("PL-0409");
    await expect(row).toHaveTextContent("1 order · 1 work center · 1 PO");
    await expect(row).toHaveTextContent("Resolves AP-0231");
    await expect(row).toHaveTextContent("proposed 2 h ago");
  },
};

export const Selected: Story = {
  args: { ...base, item: itemOf(planWithLanes), selected: true },
};

/** A portfolio whose joint slate is pending: listed, resolving both points, no plan yet. */
export const PendingPortfolio: Story = {
  args: { ...base, item: pendingPortfolioItem },
  play: async ({ canvasElement }) => {
    const row = within(canvasElement).getByRole("button");
    await expect(within(row).getByText("slate pending")).toBeInTheDocument();
    await expect(row).toHaveTextContent("Resolves AP-0240 · AP-0241");
    // No slip is served: no number, and no change count in its place.
    await expect(within(row).queryByTestId("days-late")).not.toBeInTheDocument();
  },
};

/** A plan whose read failed is listed as "plan unavailable". */
export const Unavailable: Story = {
  args: { ...base, item: unavailableItem },
  play: async ({ canvasElement }) => {
    await expect(within(canvasElement).getByText("plan unavailable")).toBeInTheDocument();
  },
};

/** A portfolio plan with a point still `open`: the row says Approve waits for it. */
export const WaitingForMember: Story = {
  args: { ...base, item: waitingItem },
  play: async ({ canvasElement }) => {
    await expect(within(canvasElement).getByText("waiting for AP-0239 (open)")).toBeInTheDocument();
  },
};

/** An at-risk plan (slip 0, no day count): no number, and no change count instead. */
export const NoSlipServed: Story = {
  args: { ...base, item: itemOf(planAtRisk) },
  play: async ({ canvasElement }) => {
    const row = within(canvasElement).getByRole("button");
    await expect(within(row).queryByTestId("days-late")).not.toBeInTheDocument();
    await expect(row).not.toHaveTextContent(/days late/);
    await expect(row).toHaveTextContent(/at risk/i);
  },
};

function PlanList({ items }: { items: InboxItem[] }) {
  const [sel, setSel] = useState(items[0] ? itemId(items[0]) : "");
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      {items.map((i) => (
        <PlanRow key={itemId(i)} item={i} now={FIXTURE_NOW} selected={itemId(i) === sel} onSelect={setSel} />
      ))}
    </div>
  );
}

export const List: Story = {
  args: { ...base, item: itemOf(planWithLanes) },
  render: () => <PlanList items={[itemOf(planWithLanes), itemOf(planNoLanes)]} />,
};

/** A plan that cannot be approved here is still listed, with a status chip for each kind
 *  ("no option", "slate pending", "plan unavailable"). A plan with options has none. */
export const StatusChips: Story = {
  args: { ...base, item: itemOf(planWithLanes) },
  render: () => <PlanList items={[itemOf(planWithLanes), itemOf(planNoOption), pendingItem, unavailableItem]} />,
  play: async ({ canvasElement }) => {
    const rows = within(canvasElement).getAllByRole("button");
    await expect(rows).toHaveLength(4);
    await expect(rows[0]).not.toHaveTextContent(/no option|slate pending|plan unavailable/);
    await expect(within(rows[1]!).getByText("no option")).toBeInTheDocument();
    await expect(within(rows[2]!).getByText("slate pending")).toBeInTheDocument();
    await expect(within(rows[3]!).getByText("plan unavailable")).toBeInTheDocument();
    // A pending / unavailable row shows its situation and the point it resolves.
    await expect(rows[2]).toHaveTextContent(/Gear Set inbound slips past the promise/);
    await expect(rows[3]).toHaveTextContent(/Resolves AP-0172/);
  },
};
