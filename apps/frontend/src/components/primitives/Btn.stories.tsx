import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { Btn } from "./Btn.tsx";

const meta = {
  title: "Primitives/Btn",
  component: Btn,
  parameters: { layout: "padded" },
  args: { variant: "primary", children: "Approve option A", onClick: fn() },
} satisfies Meta<typeof Btn>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Idle: Story = {};

/** A command in flight: the button is disabled and `aria-busy`, so a click issues nothing. */
export const Busy: Story = {
  args: { busy: true, onClick: fn() },
  play: async ({ canvasElement, args }) => {
    const button = within(canvasElement).getByRole("button", { name: /Approve option A/i });
    await expect(button).toBeDisabled();
    await expect(button).toHaveAttribute("aria-busy", "true");
    await userEvent.click(button);
    await expect(args.onClick).not.toHaveBeenCalled();
  },
};
