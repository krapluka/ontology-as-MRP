import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { TimelineTab } from "./TimelineTab.tsx";
import { emptyTimelineView, timelineView } from "../../fixtures/timeline.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, RECORDED_ON, SELECTABLE_SUBJECTS } from "../../fixtures/scenario.ts";

const meta = {
  title: "Tabs/Timeline",
  component: TimelineTab,
  parameters: { layout: "padded" },
} satisfies Meta<typeof TimelineTab>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The flagship point (recorded live): every served order against its promise date with the
 *  point's order highlighted, and the plant's open inbound lines on their own lane. */
export const Default: Story = {
  args: { view: timelineView, subject: FLAGSHIP_SUBJECT, today: `${RECORDED_ON}T00:00:00` },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/Promise timeline/i)).toBeInTheDocument();
    // All served orders render, each with its promise tick.
    for (const order of timelineView.orders) {
      await expect(canvasElement.querySelector(`[data-entity="${order.entity}"]`)).not.toBeNull();
    }
    await expect(canvas.getAllByTestId("promise-tick")).toHaveLength(timelineView.orders.filter((o) => o.promise !== null).length);
    // Only the point's order is highlighted among the orders.
    const litOrders = timelineView.orders.filter((o) => canvasElement.querySelector(`[data-entity="${o.entity}"]`)?.getAttribute("data-path") === "on");
    await expect(litOrders.map((o) => o.entity)).toEqual([FLAGSHIP_SUBJECT]);
    // The inbound lane: every open line, the point's (late) one lit.
    await expect(canvas.getByText("Inbound purchase orders")).toBeInTheDocument();
    for (const line of timelineView.inbound) {
      await expect(canvasElement.querySelector(`[data-entity="${line.entity}"]`)).not.toBeNull();
    }
    await expect(canvas.getByText(/of promises at risk/)).toBeInTheDocument();
    await expect(canvas.getByText("Today")).toBeInTheDocument();
  },
};

const DAY_MS = 86_400_000;

/** The recorded orders whose ready date lands after / before their promise. */
const lateOrder = timelineView.orders.find((o) => o.ready !== null && o.promise !== null && Date.parse(o.ready) > Date.parse(o.promise));
const bufferedOrder = timelineView.orders.find((o) => o.ready !== null && o.promise !== null && Date.parse(o.ready) < Date.parse(o.promise));

/** A late order and a buffered order (recorded live; the ready dates come from the MOs that
 *  fulfil each order): the late one draws its ready bar up to the promise, the hatched gap past
 *  it and "+N days late"; the buffered one its ready bar, the dashed buffer and "N d buffer". */
export const LateAndBuffered: Story = {
  args: { view: timelineView, subject: FLAGSHIP_SUBJECT, today: `${RECORDED_ON}T00:00:00` },
  play: async ({ canvasElement }) => {
    if (!lateOrder || !bufferedOrder) throw new Error("the recording serves no late and buffered order pair; re-record it");
    const track = (entity: string) => {
      const el = canvasElement.querySelector<HTMLElement>(`[data-track="${entity}"]`);
      if (!el) throw new Error(`no timeline track for ${entity}`);
      return within(el);
    };
    const days = (a: string | null, b: string | null) => Math.round((Date.parse(a ?? "") - Date.parse(b ?? "")) / DAY_MS);

    const late = track(lateOrder.entity);
    await expect(late.getByTestId("ready-bar")).toBeInTheDocument();
    await expect(late.getByTestId("late-gap")).toBeInTheDocument();
    await expect(late.queryByTestId("buffer")).not.toBeInTheDocument();
    const slip = days(lateOrder.ready, lateOrder.promise);
    await expect(late.getByTestId("slip-label")).toHaveTextContent(`+${slip} ${slip === 1 ? "day" : "days"} late`);

    const buffered = track(bufferedOrder.entity);
    await expect(buffered.getByTestId("ready-bar")).toBeInTheDocument();
    await expect(buffered.getByTestId("buffer")).toBeInTheDocument();
    await expect(buffered.queryByTestId("late-gap")).not.toBeInTheDocument();
    await expect(buffered.getByTestId("slip-label")).toHaveTextContent(`${days(bufferedOrder.promise, bufferedOrder.ready)} d buffer`);
  },
};

/** An order that is the subject of another listed point is a button that selects that point.
 *  The selected point's own order is a plain row. */
export const SelectOrder: Story = {
  args: {
    view: timelineView,
    subject: FLAGSHIP_SUBJECT,
    today: `${RECORDED_ON}T00:00:00`,
    selection: { selectable: SELECTABLE_SUBJECTS, onSelect: fn() },
  },
  play: async ({ canvasElement, args }) => {
    await expect(canvasElement.querySelector(`[data-entity="${FLAGSHIP_SUBJECT}"]`)?.tagName).toBe("DIV");
    const other = canvasElement.querySelector(`[data-entity="${CLEARED_SUBJECT}"]`);
    await expect(other?.tagName).toBe("BUTTON");
    if (!(other instanceof HTMLElement)) throw new Error("the other point's order is not drawn");
    await userEvent.click(other);
    await expect(args.selection?.onSelect).toHaveBeenLastCalledWith(CLEARED_SUBJECT);
  },
};

/** No order is promised while inbound lines are open: an empty order list is a known fact, so
 *  the headline says so instead of "Risk status not served". */
export const InboundOnly: Story = {
  args: { view: { orders: [], inbound: timelineView.inbound }, subject: FLAGSHIP_SUBJECT, today: `${RECORDED_ON}T00:00:00` },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText("No promised orders")).toBeInTheDocument();
    await expect(canvas.queryByText(/not served/i)).not.toBeInTheDocument();
  },
};

export const Empty: Story = {
  args: { view: emptyTimelineView, subject: FLAGSHIP_SUBJECT },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/No timeline rows/i)).toBeInTheDocument();
  },
};
