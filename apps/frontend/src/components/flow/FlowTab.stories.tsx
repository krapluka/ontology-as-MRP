import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { FlowTab } from "./FlowTab.tsx";
import { emptyFlowView, flowUnservedView, flowView } from "../../fixtures/flow.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, SELECTABLE_SUBJECTS } from "../../fixtures/scenario.ts";

const meta = {
  title: "Tabs/Flow",
  component: FlowTab,
  parameters: { layout: "padded" },
} satisfies Meta<typeof FlowTab>;

export default meta;
type Story = StoryObj<typeof meta>;

const STAGES = ["suppliers", "on_order", "in_stock", "in_production", "promised"] as const;

/** The highlighted (on-path) tiles of each stage. */
function litTiles(canvasElement: HTMLElement): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const stage of STAGES) {
    counts[stage] = canvasElement.querySelectorAll(`[data-stage="${stage}"] [data-path="on"]`).length;
  }
  return counts;
}

/** The flagship point (recorded live): each stage draws its tile facts, one on-path tile is
 *  highlighted in every stage the chain reaches, and the chain links them. */
export const Default: Story = {
  args: { view: flowView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    for (const column of ["Suppliers", "On order", "In stock", "In production", "Promised"]) {
      await expect(canvas.getByText(column)).toBeInTheDocument();
    }
    // One highlighted tile per stage: the recorded chain reaches all five. In production
    // lights the MO that fulfils the flagship order (its origin names the order;
    // `ekg:fulfills` comes from `just graphdb-derive`, which the recorder requires).
    await expect(litTiles(canvasElement)).toEqual({ suppliers: 1, on_order: 1, in_stock: 1, in_production: 1, promised: 1 });
    await expect(canvasElement.querySelector(`[data-stage="promised"] [data-path="on"]`)?.getAttribute("data-entity")).toBe(FLAGSHIP_SUBJECT);
    await expect(canvasElement.querySelector(`[data-stage="in_production"] [data-path="on"]`)?.getAttribute("data-entity")).toMatch(/\/mrp\.production\/\d+$/);
    // The chain connects the five lit tiles (four links).
    await expect(canvas.getByTestId("flow-chain").dataset["links"]).toBe("4");
    // The tile facts: lead and sourcing, arrival, on hand vs safety, due date.
    await expect(canvas.getByText(/45d lead · single/)).toBeInTheDocument();
    await expect(canvas.getAllByText(/arrives \d+ \w+/).length).toBeGreaterThan(0);
    await expect(canvas.getByText(/2 on hand · safety 5/)).toBeInTheDocument();
    await expect(canvas.queryByText("path not served")).not.toBeInTheDocument();
  },
};

/** The rules could not be read: suppliers / on order / in stock carry no `on_path`, so they
 *  say "path not served", highlight nothing, and the chain skips them. */
export const PathNotServed: Story = {
  args: { view: flowUnservedView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getAllByText("path not served")).toHaveLength(3);
    const lit = litTiles(canvasElement);
    await expect([lit["suppliers"], lit["on_order"], lit["in_stock"]]).toEqual([0, 0, 0]);
    for (const stage of ["suppliers", "on_order", "in_stock"]) {
      const tiles = canvasElement.querySelectorAll(`[data-stage="${stage}"] [data-entity]`);
      await expect([...tiles].every((t) => t.getAttribute("data-path") === "unserved")).toBe(true);
    }
    // The rule-independent hops stay highlighted (the fulfilling MO and the order), so the
    // chain is the one link between them.
    await expect([lit["in_production"], lit["promised"]]).toEqual([1, 1]);
    await expect(canvas.getByTestId("flow-chain").dataset["links"]).toBe("1");
  },
};

/** A promised order that is the subject of another listed point is a tile button that selects
 *  that point. The selected point's own order is a plain tile. */
export const SelectPromisedTile: Story = {
  args: { view: flowView, subject: FLAGSHIP_SUBJECT, selection: { selectable: SELECTABLE_SUBJECTS, onSelect: fn() } },
  play: async ({ canvasElement, args }) => {
    await expect(canvasElement.querySelector(`[data-stage="promised"] [data-entity="${FLAGSHIP_SUBJECT}"]`)?.tagName).toBe("DIV");
    const other = canvasElement.querySelector(`[data-stage="promised"] [data-entity="${CLEARED_SUBJECT}"]`);
    await expect(other?.tagName).toBe("BUTTON");
    if (!(other instanceof HTMLElement)) throw new Error("the other point's order has no tile");
    await userEvent.click(other);
    await expect(args.selection?.onSelect).toHaveBeenLastCalledWith(CLEARED_SUBJECT);
  },
};

export const Empty: Story = {
  args: { view: emptyFlowView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/No flow rows/i)).toBeInTheDocument();
  },
};
