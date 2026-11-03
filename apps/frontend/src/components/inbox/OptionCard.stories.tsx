import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { OptionCard } from "./OptionCard.tsx";
import { planWithLanes } from "../../fixtures/inbox.ts";

const [recommended, alt] = planWithLanes.options;

const meta = {
  title: "Inbox/OptionCard",
  component: OptionCard,
  parameters: { layout: "padded" },
} satisfies Meta<typeof OptionCard>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Recommended: Story = {
  args: { option: recommended!, selected: false, onSelect: () => {} },
};

export const Selected: Story = {
  args: { option: recommended!, selected: true, onSelect: () => {} },
};

/** A non-recommended alternative (the operator can inspect it, guarding automation bias). */
export const Alternative: Story = {
  args: { option: alt!, selected: false, onSelect: () => {} },
};

function OptionSlate() {
  const [rankKey, setRankKey] = useState(recommended!.rankKey);
  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(220px,1fr))", gap: 12 }}>
      {planWithLanes.options.map((o) => (
        <OptionCard key={o.rankKey} option={o} selected={o.rankKey === rankKey} onSelect={setRankKey} />
      ))}
    </div>
  );
}

/** The full slate, rank-ordered best-first; selecting one highlights it. */
export const Slate: Story = {
  args: { option: recommended!, selected: false, onSelect: () => {} },
  render: () => <OptionSlate />,
};

/**
 * Keyboard focus shows the ring (WCAG 2.4.7). The option card's bare button must keep the
 * global `:focus-visible` outline: an inline `all: unset` would remove it, and axe cannot
 * see that. Runs in real Chromium (the `storybook` Vitest project).
 */
export const KeyboardFocusRing: Story = {
  args: { option: recommended!, selected: false, onSelect: () => {} },
  play: async ({ canvasElement }) => {
    const button = within(canvasElement).getByRole("button");
    await userEvent.tab();
    await expect(button).toHaveFocus();
    const { outlineStyle, outlineWidth } = getComputedStyle(button);
    await expect(outlineStyle).toBe("solid");
    await expect(outlineWidth).toBe("2px");
  },
};

/**
 * The card's content fills its width. Guards the bare-button reset: if a button's
 * `align-items` becomes `flex-start` (some UA sheets default to it), every row shrinks to its
 * content and the Delivery · Cost · Risk grid bunches up. axe does not check layout.
 */
export const FillsCardWidth: Story = {
  args: { option: recommended!, selected: false, onSelect: () => {} },
  play: async ({ canvasElement }) => {
    const button = within(canvasElement).getByRole("button");
    const card = button.parentElement;
    if (card === null) throw new Error("the option button has no card container");
    // The button spans the card's content box,
    const { paddingLeft, paddingRight } = getComputedStyle(card);
    const cardContent = card.clientWidth - parseFloat(paddingLeft) - parseFloat(paddingRight);
    const width = button.getBoundingClientRect().width;
    await expect(Math.round(width)).toBe(Math.round(cardContent));
    // and each of its three rows (header, label, Delivery · Cost · Risk) spans the button.
    const rows = Array.from(button.children);
    await expect(rows).toHaveLength(3);
    for (const row of rows) {
      await expect(Math.round(row.getBoundingClientRect().width)).toBe(Math.round(width));
    }
  },
};

/** Selecting a card reports the option's `rankKey` (its slate identity), not its letter. */
export const ReportsRankKey: Story = {
  args: { option: alt!, selected: false, onSelect: fn() },
  play: async ({ canvasElement, args }) => {
    await userEvent.click(within(canvasElement).getByRole("button"));
    await expect(args.onSelect).toHaveBeenCalledWith(alt!.rankKey);
  },
};
