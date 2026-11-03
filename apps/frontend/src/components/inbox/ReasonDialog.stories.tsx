import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { ReasonDialog } from "./ReasonDialog.tsx";

const meta = {
  title: "Inbox/ReasonDialog",
  component: ReasonDialog,
  parameters: { layout: "fullscreen" },
  args: {
    title: "Dismiss this point?",
    description: "The point is aborted; nothing is written to the source system.",
    confirmLabel: "Dismiss point",
    onConfirm: fn(),
    onCancel: fn(),
  },
} satisfies Meta<typeof ReasonDialog>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Confirm stays disabled while the reason is empty or only whitespace; focus starts in the
 *  reason field. */
export const ConfirmDisabledWhileEmpty: Story = {
  args: { onConfirm: fn(), onCancel: fn() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const dialog = canvas.getByRole("dialog", { name: /Dismiss this point/i });
    await expect(dialog).toHaveAttribute("aria-modal", "true");
    const field = canvas.getByLabelText(/Your reason/i);
    await expect(field).toHaveFocus();
    const confirm = canvas.getByRole("button", { name: /Dismiss point/i });
    await expect(confirm).toBeDisabled();
    await userEvent.type(field, "   ");
    await expect(confirm).toBeDisabled();
    await userEvent.click(confirm);
    await expect(args.onConfirm).not.toHaveBeenCalled();
  },
};

/** Escape cancels: nothing is confirmed. */
export const EscapeCancels: Story = {
  args: { onConfirm: fn(), onCancel: fn() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/Your reason/i), "half typed");
    await userEvent.keyboard("{Escape}");
    await expect(args.onCancel).toHaveBeenCalledTimes(1);
    await expect(args.onConfirm).not.toHaveBeenCalled();
  },
};

/** The typed reason (trimmed) is what Confirm passes on. */
export const PassesTypedReason: Story = {
  args: { onConfirm: fn(), onCancel: fn() },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.type(canvas.getByLabelText(/Your reason/i), "  supplier confirmed by phone ");
    await userEvent.click(canvas.getByRole("button", { name: /Dismiss point/i }));
    await expect(args.onConfirm).toHaveBeenCalledWith("supplier confirmed by phone");
    await expect(args.onCancel).not.toHaveBeenCalled();
  },
};

/** Tab from the last control wraps to the reason field: focus stays inside the dialog. */
export const FocusStaysInside: Story = {
  args: { onConfirm: fn(), onCancel: fn() },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const field = canvas.getByLabelText(/Your reason/i);
    await userEvent.type(field, "reason");
    canvas.getByRole("button", { name: /Dismiss point/i }).focus();
    await userEvent.tab();
    await expect(field).toHaveFocus();
    await userEvent.tab({ shift: true });
    await expect(canvas.getByRole("button", { name: /Dismiss point/i })).toHaveFocus();
  },
};
