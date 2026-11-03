/**
 * A small N-of-max bar meter: an integer metric value drawn as that many filled cells out of
 * `max`. When a metric is not an integer in range, callers render its value as text instead
 * (see OptionCard).
 */
export function Bars({
  value,
  max = 3,
  label,
}: {
  value: number;
  max?: number;
  label?: string;
}) {
  return (
    <span style={{ display: "inline-flex", gap: 3 }} role="img" aria-label={label ?? `${value} of ${max}`}>
      {Array.from({ length: max }, (_, i) => (
        <span
          key={i}
          style={{
            width: 10,
            height: 7,
            background: i < value ? "var(--color-accent-700)" : "var(--color-neutral-300)",
          }}
        />
      ))}
    </span>
  );
}
