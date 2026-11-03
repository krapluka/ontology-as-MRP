import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import { ReasoningPanel } from "./ReasoningPanel.tsx";
import { planWithLanes } from "../../fixtures/inbox.ts";

const meta = {
  title: "Inbox/ReasoningPanel",
  component: ReasoningPanel,
  parameters: { layout: "padded" },
} satisfies Meta<typeof ReasoningPanel>;

export default meta;
type Story = StoryObj<typeof meta>;

/** A trace that retains two rejected candidates and one open assumption. */
export const WithRejectedCandidates: Story = {
  args: { trace: planWithLanes.trace },
};

/** A plan with no option: every model attempt failed, and the trace says why. A rate-limited
 *  attempt and a refused one are each set aside with their class and reason. */
export const WithFailedGeneration: Story = {
  args: {
    trace: {
      summary: { simulated: 1, rejected: 2, openAssumptions: 0 },
      steps: [
        { tag: "Simulated", text: "do-nothing baseline: delivery=late", rejected: false },
        {
          tag: "Generation",
          text: "attempt 1: rate_limited — cognition endpoint 429 Too Many Requests: Rate limit exceeded",
          rejected: true,
        },
        {
          tag: "Generation",
          text: 'attempt 2: refused — action ExpeditePo is missing required param "new_date"',
          rejected: true,
        },
      ],
    },
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getAllByText("Generation")).toHaveLength(2);
    await expect(canvas.getAllByText("Set aside.")).toHaveLength(2);
    await expect(
      canvas.getByText(/attempt 1: rate_limited — cognition endpoint 429/),
    ).toBeInTheDocument();
    await expect(canvas.getByText("1 plan simulated · 2 rejected · no open assumptions")).toBeInTheDocument();
    await expect(
      canvas.getByText(/Generation: a model attempt that produced no candidate/),
    ).toBeInTheDocument();
  },
};
