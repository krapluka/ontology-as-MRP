import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, within } from "storybook/test";
import { GraphTab } from "./GraphTab.tsx";
import { emptyGraphView, graphClearedView, graphNoOpenPoView, graphUnservedView, graphView } from "../../fixtures/graph.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, SELECTABLE_SUBJECTS } from "../../fixtures/scenario.ts";

const meta = {
  title: "Tabs/Graph",
  component: GraphTab,
  parameters: { layout: "padded" },
} satisfies Meta<typeof GraphTab>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The node box a tab drew for an IRI. */
function nodeOf(canvasElement: HTMLElement, iri: string): HTMLElement {
  const el = canvasElement.querySelector<HTMLElement>(`[data-entity="${iri}"]`);
  if (!el) throw new Error(`no node drawn for ${iri}`);
  return el;
}

const NEVER = /No node on this path is at risk/i;

/** The flagship point (recorded live): its chain is lit with its edges, the short components
 *  are filled as causes, and the rest of the plant is dimmed context. */
export const Default: Story = {
  args: { view: graphView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    for (const title of ["Customer", "Sales order", "Product · BOM", "Component", "Supplier", "Purchase order"]) {
      await expect(canvas.getAllByText(title).length).toBeGreaterThan(0);
    }
    // Lit: the selected order is on the path, at full strength.
    const order = nodeOf(canvasElement, FLAGSHIP_SUBJECT);
    await expect(order.dataset["path"]).toBe("on");
    await expect(order.style.opacity).toBe("1");
    // Dimmed: an order off the path is drawn as context.
    const other = nodeOf(canvasElement, CLEARED_SUBJECT);
    await expect(other.dataset["path"]).toBe("off");
    await expect(other.style.opacity).toBe("0.64"); // blended text is about 4.8 : 1 (AA)
    // Cause: every short component is filled; the on-path one is in the chain.
    const causes = canvasElement.querySelectorAll("[data-cause]");
    await expect(causes.length).toBeGreaterThan(0);
    const litCause = [...causes].map((c) => c.closest<HTMLElement>("[data-entity]")).find((n) => n?.dataset["path"] === "on");
    await expect(litCause).toBeTruthy();
    // Lit on-path edges.
    await expect(Number(canvas.getByTestId("graph-lit-edges").dataset["count"])).toBeGreaterThan(0);
    // The side panel lists the chain with the cause marked, and never denies risk.
    await expect(canvas.getAllByText(/— cause/).length).toBeGreaterThan(0);
    await expect(canvas.queryByText(NEVER)).not.toBeInTheDocument();
    await expect(canvas.queryByText(/Risk status not served/i)).not.toBeInTheDocument();
  },
};

/** An open point whose rule has cleared (recorded live): its order is served `at_risk="false"`
 *  and the inbox still lists it, so the panel says the rule does not flag it any more while
 *  the point stays open for a decision. */
export const ClearedRule: Story = {
  args: { view: graphClearedView, subject: CLEARED_SUBJECT, selection: { selectable: SELECTABLE_SUBJECTS, onSelect: fn() } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("status")).toHaveTextContent(
      "The rule no longer flags this order; the point is still open for your decision.",
    );
    await expect(nodeOf(canvasElement, CLEARED_SUBJECT).dataset["path"]).toBe("on");
    await expect(canvas.queryByText(NEVER)).not.toBeInTheDocument();
  },
};

/** The same cleared order projected for a subject the inbox does not list as awaiting a
 *  decision (an auto-applied change, a restored selection, or a plan already approved or
 *  dismissed): the panel states only that the rules do not flag it, not that a decision is
 *  still open. */
export const ClearedNotAnOpenPoint: Story = {
  args: {
    view: graphClearedView,
    subject: CLEARED_SUBJECT,
    selection: { selectable: new Set([...SELECTABLE_SUBJECTS].filter((s) => s !== CLEARED_SUBJECT)), onSelect: fn() },
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("status")).toHaveTextContent(/^The rules do not currently flag this order\.$/);
    await expect(canvas.queryByText(/open for your decision/i)).not.toBeInTheDocument();
    await expect(canvas.queryByText(NEVER)).not.toBeInTheDocument();
  },
};

/** The rules could not be read: no `at_risk` / `cause` anywhere and no `on_path` on the
 *  cause-dependent columns. The panel says "Risk status not served" and those columns say
 *  "path not served". */
export const RiskNotServed: Story = {
  args: { view: graphUnservedView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("status")).toHaveTextContent("Risk status not served");
    // Component, Supplier and Purchase order: the column heads and the chain steps.
    await expect(canvas.getAllByText("path not served").length).toBe(6);
    // Their nodes are neutral (neither lit nor dimmed); the rule-independent hops stay lit.
    await expect(canvasElement.querySelectorAll('[data-path="unserved"]').length).toBeGreaterThan(0);
    await expect(nodeOf(canvasElement, FLAGSHIP_SUBJECT).dataset["path"]).toBe("on");
    await expect(canvasElement.querySelectorAll("[data-cause]").length).toBe(0);
    await expect(canvas.queryByText(NEVER)).not.toBeInTheDocument();
  },
};

/** No purchase order is open inbound (all received): the Purchase-order column is empty. That
 *  is a known fact, so the panel says "none in the plant", not "path not served". */
export const NoOpenPurchaseOrders: Story = {
  args: { view: graphNoOpenPoView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const step = canvas.getAllByRole("listitem").find((li) => li.textContent?.startsWith("Purchase order"));
    await expect(step).toHaveTextContent("none in the plant");
    await expect(canvas.queryByText("path not served")).not.toBeInTheDocument();
  },
};

export const Empty: Story = {
  args: { view: emptyGraphView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/No graph rows/i)).toBeInTheDocument();
  },
};
