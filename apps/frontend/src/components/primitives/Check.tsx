import { CheckIcon } from "./icons.tsx";

/** The accent check mark used on applied changes and satisfied criteria. */
export function Check({ size = 20 }: { size?: number }) {
  return (
    <span style={{ color: "var(--color-accent-700)", display: "inline-flex" }}>
      <CheckIcon size={size} />
    </span>
  );
}
