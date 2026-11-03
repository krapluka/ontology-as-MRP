import type { Need, Option, Policy } from "../../viewmodel/types.ts";
import { needChips } from "../../viewmodel/wording.ts";

/**
 * The policy `needs`: the reasons a plan is human-gated, so the operator sees why the agent
 * did not auto-apply. Each is worded from its served code and values ("$320 is above the
 * $250 limit", "Moves a promised date"). The actions the policy does not pre-approve read as
 * one "Not pre-approved: …" statement naming the option's verbs, a code the rule does not
 * know shows its served detail, and no chip is shown twice. Empty when the option carries no
 * needs.
 */
export function NeedsChips({
  needs,
  option,
  policy,
}: {
  needs: Need[];
  /** The option's served cost (the cost need's amount) and its changes (the verbs a
   *  not-pre-approved need names). */
  option: Pick<Option, "cost" | "costCurrency" | "changes">;
  /** The active policy (the cost need's limit and its currency, the auto-applied verbs). */
  policy: Policy | null;
}) {
  if (needs.length === 0) return null;
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 6, flexWrap: "wrap", fontSize: 12.5 }}>
      <span style={{ color: "var(--color-neutral-700)" }}>Needs you:</span>
      {needChips(needs, option, policy).map((chip) => (
        <span
          key={chip.text}
          data-testid="need-chip"
          title={chip.detail}
          style={{ padding: "2px 8px", border: "1px solid var(--color-accent-500)", background: "var(--color-accent-100)", color: "var(--color-accent-800)" }}
        >
          {chip.text}
        </span>
      ))}
    </span>
  );
}
