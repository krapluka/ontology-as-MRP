import type { Criterion } from "../../viewmodel/types.ts";

/**
 * The criteria served with an auto-applied change. The read surface serves the current
 * policy's criteria, not the ones evaluated when the change was applied, so the label says
 * that and claims nothing about what was met at apply time. Hidden when none are served.
 */
export function PolicyRules({ criteria }: { criteria: Criterion[] }) {
  if (criteria.length === 0) return null;
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      <div style={{ fontSize: 11.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>
        Rules in the current policy:
      </div>
      {criteria.map((c) => (
        <div key={c.rule} style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
          <span style={{ color: "var(--color-neutral-700)" }}>{c.rule.replace(/_/g, " ")}</span>
          <span style={{ marginLeft: "auto" }}>{c.value}</span>
        </div>
      ))}
    </div>
  );
}
