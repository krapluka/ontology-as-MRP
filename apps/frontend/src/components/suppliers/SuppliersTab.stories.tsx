import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { SuppliersTab } from "./SuppliersTab.tsx";
import {
  emptySuppliersView,
  suppliersClearedView,
  suppliersExposurePartlyServedView,
  suppliersNoDemandView,
  suppliersUnservedView,
  suppliersView,
} from "../../fixtures/suppliers.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, SELECTABLE_SUBJECTS } from "../../fixtures/scenario.ts";

const meta = {
  title: "Tabs/Suppliers",
  component: SuppliersTab,
  parameters: { layout: "padded" },
} satisfies Meta<typeof SuppliersTab>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The name the detail panel shows (the selected supplier). */
function detailName(canvasElement: HTMLElement): string | null {
  return canvasElement.querySelector('[data-testid="supplier-detail-name"]')?.textContent ?? null;
}

/** The flagship point (recorded live): the table serves lead / sourcing / on-time / exposure,
 *  the detail opens on the first on-path supplier, and its gauge draws the demand diamond. */
export const Default: Story = {
  args: { view: suppliersView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText("On-time · 90 d")).toBeInTheDocument();
    const onPath = suppliersView.summaries.find((s) => s.onPath === true);
    await expect(detailName(canvasElement)).toBe(onPath?.label);
    await expect(canvas.getByRole("note")).toHaveTextContent("On this point's path.");
    // The sourcing chip (every seeded supplier is single, primary or backup).
    await expect(canvas.getAllByText("Single").length).toBeGreaterThan(0);
    // The demand diamond on the stock gauge (served for the on-path component).
    await expect(canvas.getAllByTestId("demand-marker").length).toBeGreaterThan(0);
    // All dependent orders, each with its at-risk dot.
    await expect(canvas.getAllByRole("img", { name: "At risk" }).length).toBeGreaterThan(0);
    // A supplier with no receipts in the window has its OTD omitted, not 0%.
    await expect(canvas.queryByText("0%")).not.toBeInTheDocument();
  },
};

/** An open point whose rule has cleared (recorded live): `on_path` is served but no supplier is
 *  on it, so the detail opens on the top-exposure supplier. */
export const ClearedRuleTopExposure: Story = {
  args: { view: suppliersClearedView, subject: CLEARED_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(suppliersClearedView.summaries.some((s) => s.onPath === true)).toBe(false);
    const top = [...suppliersClearedView.summaries].sort((a, b) => Number(b.exposure) - Number(a.exposure))[0];
    await expect(detailName(canvasElement)).toBe(top?.label);
    await expect(canvas.getByRole("note")).toHaveTextContent(/showing the highest exposure/);
  },
};

/** The rules could not be read: no `on_path`, no exposure. The detail opens on the first
 *  supplier in served order with a "path not served" note, without implying it is on the
 *  path. */
export const PathNotServed: Story = {
  args: { view: suppliersUnservedView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(detailName(canvasElement)).toBe(suppliersUnservedView.summaries[0]?.label);
    await expect(canvas.getByRole("note")).toHaveTextContent(/^path not served/);
    await expect(canvas.getByText("Exposed · not served")).toBeInTheDocument();
    await expect(canvas.queryByText("On this point's path.")).not.toBeInTheDocument();
    // After the operator picks another row, the note stops claiming "the first supplier".
    const second = suppliersUnservedView.summaries[1];
    if (!second) throw new Error("the recorded scenario serves a single supplier; re-record it");
    await userEvent.click(canvas.getByRole("button", { name: new RegExp(second.label ?? "", "i") }));
    await expect(detailName(canvasElement)).toBe(second.label);
    await expect(canvas.getByRole("note")).toHaveTextContent(/^path not served — it is not implied to be on this point's path\.$/);
  },
};

/** Exposure served on some rows and omitted on others (an at-risk order carries no value): the
 *  omitted one is unknown ("not served"), distinct from a known 0 ("—"). It is not ranked
 *  below a known 0, so the detail opens on it as a supplier that may be the most exposed. */
export const ExposurePartlyServed: Story = {
  args: { view: suppliersExposurePartlyServedView, subject: CLEARED_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const omitted = suppliersExposurePartlyServedView.summaries.filter((s) => s.exposure === null);
    const zero = suppliersExposurePartlyServedView.summaries.filter((s) => s.exposure !== null && Number(s.exposure) === 0);
    await expect(omitted.length).toBeGreaterThan(0);
    await expect(zero.length).toBeGreaterThan(0);
    const exposureOf = (supplier: string) =>
      canvasElement.querySelector(`[data-entity="${supplier}"] [data-testid="supplier-exposure"]`)?.textContent;
    for (const s of omitted) await expect(exposureOf(s.supplier)).toBe("not served");
    for (const s of zero) await expect(exposureOf(s.supplier)).toBe("—");
    await expect(canvas.getByText("Exposed")).toBeInTheDocument();
    await expect(detailName(canvasElement)).toBe(omitted[0]?.label);
    await expect(canvas.getByRole("note")).toHaveTextContent(/may carry the highest exposure/);
  },
};

/** Selecting another row swaps the detail; a component with no order in the 30-day window has
 *  no demand diamond (the recorded rows serve demand only when there is one). */
export const SelectSupplier: Story = {
  args: { view: suppliersView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const other = suppliersView.summaries.find((s) => s.onPath !== true);
    await userEvent.click(canvas.getByRole("button", { name: new RegExp(other?.label ?? "", "i") }));
    await expect(detailName(canvasElement)).toBe(other?.label);
    const stock = other ? (suppliersView.detailBySupplier[other.supplier]?.stock ?? []) : [];
    await expect(canvas.queryAllByTestId("demand-marker")).toHaveLength(stock.filter((s) => s.demand !== null).length);
  },
};

/** No order commits inside the 30-day window: `demand` is not served, so the gauges draw on
 *  hand and safety only, with no demand diamond and no demand figure. */
export const NoDemandServed: Story = {
  args: { view: suppliersNoDemandView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getAllByText(/on hand/).length).toBeGreaterThan(0);
    await expect(canvas.queryAllByTestId("demand-marker")).toHaveLength(0);
    await expect(canvas.queryByText(/^demand /)).not.toBeInTheDocument();
  },
};

/** In the selected supplier's "Orders depending on it", an order that is the subject of another
 *  listed point is a button that selects that point. The selected point's own order is a
 *  plain row. Picking the supplier first exercises the detail panel's own path. */
export const SelectDependentOrder: Story = {
  args: { view: suppliersView, subject: FLAGSHIP_SUBJECT, selection: { selectable: SELECTABLE_SUBJECTS, onSelect: fn() } },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const supplier = suppliersView.summaries.find((s) =>
      suppliersView.detailBySupplier[s.supplier]?.dependentOrders.some((o) => o.order === CLEARED_SUBJECT),
    );
    if (!supplier) throw new Error("no recorded supplier serves the other point's order; re-record it");
    await userEvent.click(canvas.getByRole("button", { name: new RegExp(supplier.label ?? "", "i") }));
    await expect(detailName(canvasElement)).toBe(supplier.label);

    await expect(canvasElement.querySelector(`[data-entity="${FLAGSHIP_SUBJECT}"]`)?.tagName).toBe("DIV");
    const other = canvasElement.querySelector(`[data-entity="${CLEARED_SUBJECT}"]`);
    await expect(other?.tagName).toBe("BUTTON");
    if (!(other instanceof HTMLElement)) throw new Error("the other point's order is not listed");
    await userEvent.click(other);
    await expect(args.selection?.onSelect).toHaveBeenLastCalledWith(CLEARED_SUBJECT);
  },
};

export const Empty: Story = {
  args: { view: emptySuppliersView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/No supplier rows/i)).toBeInTheDocument();
  },
};
