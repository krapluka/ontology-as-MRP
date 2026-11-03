import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import { ChangesList } from "./ChangesList.tsx";
import { NeedsChips } from "./NeedsChips.tsx";
import { expediteOnlyPolicy, liveLabels, planWithIds, planWithLanes, policy } from "../../fixtures/inbox.ts";
import { planOptionTitle } from "../../viewmodel/wording.ts";
import { EntityLabels } from "../primitives/entityLabels.ts";
import type { Change } from "../../viewmodel/types.ts";

const recommended = planWithLanes.options[0]!;

function ChangesAndNeeds() {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 16, maxWidth: 520 }}>
      <ChangesList heading={`${planOptionTitle(recommended.key, recommended.changes.length)} · approved together`} changes={recommended.changes} />
      <NeedsChips needs={recommended.needs} option={recommended} policy={policy} />
    </div>
  );
}

const meta = {
  title: "Inbox/ChangesAndNeeds",
  component: ChangesAndNeeds,
  parameters: { layout: "padded" },
} satisfies Meta<typeof ChangesAndNeeds>;

export default meta;
type Story = StoryObj<typeof meta>;

export const MultiChangeWithNeeds: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Verbs by their label, not the raw code.
    await expect(canvas.getByText("Split")).toBeInTheDocument();
    await expect(canvas.queryByText("split_production")).not.toBeInTheDocument();
    // The needs in words, in the served currency.
    await expect(canvas.getByText("$320 is above the $250 limit")).toBeInTheDocument();
    await expect(canvas.getByText("Splits a manufacturing order")).toBeInTheDocument();
  },
};

/** The width the changes column gets on the 1280 px screen (the narrowest supported). */
const COLUMN_AT_1280 = 420;

const LONG_WC = "https://ekg.example/id/mrp.workcenter/7";
const LONG_WC_2 = "https://ekg.example/id/mrp.workcenter/8";
const longLabels = new Map([
  ...liveLabels,
  [LONG_WC, "Final Assembly & Quality Inspection Line North"],
  [LONG_WC_2, "Powder Coating & Curing Booth South Annex"],
]);
const liveOption = planWithIds.options[0]!;
const longChanges: Change[] = [
  ...liveOption.changes,
  {
    verb: "reassign_work_center",
    object: "https://ekg.example/id/mrp.workorder/3181",
    params: [{ key: "work_center", value: LONG_WC_2 }],
    inverse: null,
    from: LONG_WC,
    label: null,
  },
];

function LongValues() {
  return (
    <EntityLabels.Provider value={longLabels}>
      <div style={{ display: "flex", flexDirection: "column", gap: 16, width: COLUMN_AT_1280 }}>
        <ChangesList heading={planOptionTitle(liveOption.key, longChanges.length)} changes={longChanges} />
        <NeedsChips needs={liveOption.needs} option={{ ...liveOption, changes: longChanges }} policy={expediteOnlyPolicy} />
      </div>
    </EntityLabels.Provider>
  );
}

/** Long served values at the 1280 px screen's column width: a long from/to wraps below its
 *  verb and object instead of squeezing the object, nothing overflows its row, and the needs
 *  read as one "Not pre-approved" chip. */
export const LongChangeValues: Story = {
  render: () => <LongValues />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const rows = [...canvasElement.querySelectorAll<HTMLLIElement>("li")];
    await expect(rows.length).toBe(longChanges.length);
    for (const row of rows) await expect(row.scrollWidth).toBeLessThanOrEqual(row.clientWidth);
    // The long from/to sits on its own line, below the object (which keeps its line).
    const long = rows.at(-1)!;
    const fromTo = long.querySelector<HTMLElement>("[data-testid=change-from-to]")!;
    const object = long.firstElementChild as HTMLElement;
    await expect(fromTo.offsetTop).toBeGreaterThan(object.offsetTop);
    await expect(fromTo).toHaveTextContent("Final Assembly & Quality Inspection Line North → Powder Coating & Curing Booth South Annex");
    await expect(canvas.getAllByTestId("need-chip").map((el) => el.textContent)).toEqual(["Not pre-approved: Raise capacity · Reassign"]);
  },
};
