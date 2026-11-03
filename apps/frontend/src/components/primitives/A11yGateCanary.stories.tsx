import type { Meta, StoryObj } from "@storybook/react-vite";

/**
 * The a11y gate's self-test. This story renders a single low-contrast letter and is excused
 * with `test: "todo"`; `.storybook/vitest.setup.ts` then requires axe to report a
 * `color-contrast` violation for it. If an axe-core upgrade or a stale `a11y.config` in
 * preview.ts disables the contrast check (single characters are the case our config opts
 * into), this story fails the gate instead of everything staying green.
 */
const meta: Meta = {
  title: "Gate/A11yCanary",
  parameters: { a11y: { test: "todo" }, a11yCanary: true },
};
export default meta;

export const LowContrastSingleLetter: StoryObj = {
  render: () => <p style={{ fontSize: 12, color: "#b7b7ba" }}>x</p>,
};
