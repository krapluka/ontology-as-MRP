import { useId } from "react";

/** The hint shown while no reviewer identity is set (every command control is disabled). */
export const REVIEWER_HINT = "Enter your reviewer name to act";

/**
 * The operator's reviewer identity, recorded with every command. It stands in for
 * authentication: the operator enters it, the screen remembers it per browser, and while it
 * is empty no command can be issued.
 */
export function ReviewerField({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  const id = useId();
  const hintId = useId();
  const missing = value.trim().length === 0;
  return (
    <span style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
      <label htmlFor={id} style={{ color: "var(--color-neutral-700)" }}>
        Reviewer
      </label>
      <input
        id={id}
        type="text"
        autoComplete="name"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        aria-describedby={missing ? hintId : undefined}
        style={{ width: 170, font: "inherit", fontSize: 13, padding: "4px 8px", border: "1px solid var(--color-divider)", background: "var(--color-surface)" }}
      />
      {missing ? (
        <span id={hintId} style={{ color: "var(--color-accent-800)", fontSize: 12.5 }}>
          {REVIEWER_HINT}
        </span>
      ) : null}
    </span>
  );
}
