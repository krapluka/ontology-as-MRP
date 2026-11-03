import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { WorkCenterLanes, type LaneMode } from "./WorkCenterLanes.tsx";
import { planWithLanes } from "../../fixtures/inbox.ts";
import { expect, within } from "storybook/test";
import { EntityLabels } from "../primitives/entityLabels.ts";
import type { Lane } from "../../viewmodel/types.ts";

const recommended = planWithLanes.options[0]!;

function LanesDemo({ initial }: { initial: LaneMode }) {
  const [mode, setMode] = useState<LaneMode>(initial);
  return (
    <div style={{ maxWidth: 640 }}>
      <WorkCenterLanes lanes={recommended.lanes} mode={mode} onModeChange={setMode} planKey={recommended.key} />
    </div>
  );
}

const meta = {
  title: "Inbox/WorkCenterLanes",
  component: LanesDemo,
  parameters: { layout: "padded" },
} satisfies Meta<typeof LanesDemo>;

export default meta;
type Story = StoryObj<typeof meta>;

export const AsPlanned: Story = { args: { initial: "before" } };
export const WithPlan: Story = { args: { initial: "after" } };

/** The width the lanes column gets on the 1280 px screen (the narrowest supported). */
const COLUMN_AT_1280 = 420;

const WC = "https://ekg.example/id/mrp.workcenter/7";
const WO = "https://ekg.example/id/mrp.workorder/71";
const LONG_NAME = "Final Assembly & Quality Inspection Line North";
const LONG_BAR = "WH/MO/00318 · Assemble drive + frame and fit the controller";
const longLanes: Lane[] = [
  {
    workCenter: WC,
    capacity: 1,
    before: [{ workOrder: WO, start: "2027-01-25", end: "2027-01-26", row: 0, changed: false }],
    after: [{ workOrder: WO, start: "2027-01-27", end: "2027-01-28", row: 0, changed: true }],
  },
];

/** A long work-center label and a long bar label at the 1280 px screen's column width: the
 *  lane name ends in an ellipsis inside its 120 px column and the bar label inside its bar,
 *  each with the full label on hover; nothing overflows. */
export const LongLaneNames: Story = {
  args: { initial: "before" },
  render: () => (
    <EntityLabels.Provider value={new Map([[WC, LONG_NAME], [WO, LONG_BAR]])}>
      <div style={{ width: COLUMN_AT_1280 }}>
        <WorkCenterLanes lanes={longLanes} mode="before" onModeChange={() => {}} planKey="A" />
      </div>
    </EntityLabels.Provider>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const name = canvas.getByTestId("lane-name").querySelector<HTMLElement>("[data-entity]")!;
    await expect(name).toHaveTextContent(LONG_NAME);
    await expect(name.title).toContain(LONG_NAME);
    await expect(name.scrollWidth).toBeGreaterThan(name.clientWidth); // clipped, with an ellipsis
    await expect(getComputedStyle(name).textOverflow).toBe("ellipsis");
    const bar = canvas.getByTestId("lane-bar");
    await expect(bar.title).toContain(LONG_BAR);
    const label = bar.firstElementChild as HTMLElement;
    await expect(getComputedStyle(label).textOverflow).toBe("ellipsis");
    await expect(label.getBoundingClientRect().right).toBeLessThanOrEqual(bar.getBoundingClientRect().right + 0.5);
    // The lane row (name column and bars) holds all the text and overflows nothing. The
    // blueprint frame's decorative corners sit outside it.
    const row = canvas.getByTestId("lane-name").parentElement!.parentElement!;
    await expect(row.scrollWidth).toBeLessThanOrEqual(row.clientWidth);
    await expect(canvas.getByTestId("lane-name").parentElement!.getBoundingClientRect().width).toBe(120);
  },
};
