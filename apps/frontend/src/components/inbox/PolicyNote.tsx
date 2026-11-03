import type { Criterion, Policy } from "../../viewmodel/types.ts";
import { formatMoney } from "../../viewmodel/format.ts";

/** The served cost limit that means "no limit is configured". The policy then auto-applies
 *  no cost-bearing change at all. */
const UNSET = "unset";

/** Words a policy rule and its served value as a clause, or returns null (the clause is
 *  omitted) when the served value does not state the rule. The cost limit renders in the
 *  currency the policy serves (a plain number when it serves none). An unset cost limit is
 *  not a clause of "when it …"; it is stated on its own. */
function clause({ rule, value }: Criterion, currency: string | null): string | null {
  const served = value.trim();
  if (served.length === 0) return null;
  switch (rule) {
    case "cost_within_limit": {
      const limit = formatMoney(served, currency);
      return served === UNSET || limit === null ? null : `costs ${limit} or less`;
    }
    case "no_promised_date_move":
      return served === "true" ? "moves no promised date" : null;
    case "single_step_reversible":
      return served === "true" ? "can be reversed in one step" : null;
    case "supplier_confirmed":
      return `the supplier has confirmed (order state ${served})`;
    case "notify_only":
      return `only notifies (${served})`;
    default:
      return `${rule.replace(/_/g, " ")} (${served})`;
  }
}

/** Join clauses as a sentence does: "a, b, c and d". */
function joinClauses(clauses: string[]): string {
  if (clauses.length <= 1) return clauses.join("");
  return `${clauses.slice(0, -1).join(", ")} and ${clauses[clauses.length - 1]}`;
}

const NOTE_STYLE = {
  border: "1px dashed var(--color-accent-400)",
  padding: "10px 12px",
  fontSize: 12.5,
  display: "flex",
  flexDirection: "column",
  gap: 6,
} as const;

/**
 * The active auto-apply policy as sentences, composed from the served criteria. Each clause
 * renders its served value or is omitted; an unset cost limit reads "no cost-bearing change
 * is auto-applied". With no policy served, it says so instead of showing a placeholder
 * policy.
 */
export function PolicyNote({ policy }: { policy: Policy | null }) {
  if (policy === null) {
    return (
      <div style={NOTE_STYLE}>
        <p style={{ margin: 0 }}>Auto-apply policy not served.</p>
      </div>
    );
  }
  const clauses = policy.criteria.map((c) => clause(c, policy.costLimitCurrency)).filter((c): c is string => c !== null);
  const costUnset = policy.criteria.some((c) => c.rule === "cost_within_limit" && c.value.trim() === UNSET);
  const sentences =
    policy.criteria.length === 0
      ? ["The agent waits for you on every change."]
      : [
          ...(clauses.length ? [`The agent applies a change by itself only when it ${joinClauses(clauses)}.`] : []),
          ...(costUnset ? ["No cost-bearing change is auto-applied."] : []),
          "Everything else waits for you.",
        ];

  return (
    <div style={NOTE_STYLE}>
      <p style={{ margin: 0 }}>{sentences.join(" ")}</p>
      <div style={{ fontSize: 11.5, color: "var(--color-neutral-700)" }}>
        {[policy.undoWindow, `policy ${policy.policyId}`].filter((part) => part.length > 0).join(" · ")}
      </div>
    </div>
  );
}
