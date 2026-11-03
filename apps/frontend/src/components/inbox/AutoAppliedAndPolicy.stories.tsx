import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import { AutoAppliedRow } from "./AutoAppliedRow.tsx";
import { PolicyNote } from "./PolicyNote.tsx";
import { PolicyRules } from "./PolicyRules.tsx";
import type { AppliedChange, Policy } from "../../viewmodel/types.ts";
import { autoApplied, FIXTURE_NOW, policy } from "../../fixtures/inbox.ts";

function AppliedWithPolicy({ applied, policy: shown, now }: { applied: AppliedChange[]; policy: Policy | null; now: number }) {
  const [sel, setSel] = useState(applied[0]?.subject ?? "");
  const selected = applied.find((a) => a.subject === sel) ?? applied[0];
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 16, maxWidth: 480 }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {applied.map((a) => (
          <AutoAppliedRow key={a.subject} applied={a} now={now} selected={a.subject === sel} onSelect={setSel} onUndo={() => {}} />
        ))}
      </div>
      {selected ? <PolicyRules criteria={selected.criteria} /> : null}
      <PolicyNote policy={shown} />
    </div>
  );
}

const meta = {
  title: "Inbox/AutoAppliedAndPolicy",
  component: AppliedWithPolicy,
  parameters: { layout: "padded" },
  args: { applied: autoApplied, policy, now: FIXTURE_NOW },
} satisfies Meta<typeof AppliedWithPolicy>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The served criteria are labelled as the current policy's (not as rules met at apply time),
 *  and each policy clause renders its served value. */
export const AppliedWithCriteriaAndPolicy: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText("Rules in the current policy:")).toBeInTheDocument();
    // The limit in the policy's served currency, the clauses joined with a final "and".
    await expect(
      canvas.getByText(/costs \$250 or less, moves no promised date, can be reversed in one step and the supplier has confirmed \(order state purchase\)\./),
    ).toBeInTheDocument();
    await expect(canvas.getAllByRole("button", { name: "Undo" })).toHaveLength(2);
    // The row meta "HH:MM · object": the applied time and the first change's object (the
    // cost of an applied change is not served, so it is omitted).
    await expect(canvas.getByText(/^\d{2}:\d{2} · P00095$/)).toBeInTheDocument();
  },
};

/** A policy that serves no limit currency states its limit as a plain number. */
export const LimitWithoutCurrency: Story = {
  args: { policy: { ...policy, costLimitCurrency: null } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/costs 250 or less/)).toBeInTheDocument();
    await expect(canvasElement.textContent ?? "").not.toMatch(/\$250/);
  },
};

/** An unset cost limit means no cost-bearing change is auto-applied, not "costs unset or
 *  less". */
export const UnsetCostLimit: Story = {
  args: { policy: { ...policy, criteria: [{ rule: "cost_within_limit", value: "unset" }, ...policy.criteria.slice(1)] } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText(/No cost-bearing change is auto-applied\./)).toBeInTheDocument();
    await expect(canvas.getByText(/only when it moves no promised date/)).toBeInTheDocument();
    await expect(canvasElement.textContent ?? "").not.toMatch(/unset/);
  },
};

/** No served criteria: the rules header is hidden (no empty "rules" claim), and the policy
 *  says the agent waits on every change. */
export const EmptyCriteria: Story = {
  args: { applied: autoApplied.map((a) => ({ ...a, criteria: [] })), policy: { ...policy, criteria: [] } },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.queryByText(/Rules in the current policy/)).not.toBeInTheDocument();
    await expect(canvas.getByText("The agent waits for you on every change.")).toBeInTheDocument();
  },
};

/** A change that is not reversible offers no Undo and says so; a closed window says when it
 *  closed. */
export const NonReversibleChange: Story = {
  args: {
    applied: [
      { ...autoApplied[0]!, reversible: false },
      { ...autoApplied[1]!, undoUntil: String(FIXTURE_NOW / 1000 - 60) },
    ],
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
    await expect(canvas.getByText("not reversible")).toBeInTheDocument();
    await expect(canvas.getByText(/^reversal window closed at /)).toBeInTheDocument();
  },
};

/** No policy served: the note says so instead of showing a placeholder policy. */
export const PolicyNotServed: Story = {
  args: { policy: null },
  play: async ({ canvasElement }) => {
    await expect(within(canvasElement).getByText("Auto-apply policy not served.")).toBeInTheDocument();
  },
};
