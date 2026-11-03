import type { ReactNode } from "react";
import type { Option } from "../../viewmodel/types.ts";
import { Blueprint } from "../primitives/Blueprint.tsx";
import { Tag } from "../primitives/Tag.tsx";
import { Bars } from "../primitives/Bars.tsx";
import { asBarLevel, formatDate, formatMoney } from "../../viewmodel/format.ts";
import { planOptionTitle, riskWord } from "../../viewmodel/wording.ts";

/**
 * One ranked option card: "Plan K · n changes" (the derived slate key and change count), a
 * Recommended tag, the option's derived verb label, and the Delivery · Cost · Risk left trio.
 * Delivery is the served outcome ("On time" / "N d late" / "At risk") with the projected
 * completion date beneath it when served, Cost the projected cost in its served currency,
 * Risk left the residual-risk bars with their word (Low / Medium / High). A cell renders the
 * placeholder `—` when the contract serves no value. Selecting the card reports the option's
 * `rankKey`, its identity on the slate, and not its position letter, which a re-rank
 * reassigns.
 */
export function OptionCard({
  option,
  selected,
  onSelect,
}: {
  option: Option;
  selected: boolean;
  onSelect: (rankKey: string) => void;
}) {
  return (
    <Blueprint
      className="pp-hover"
      style={{
        padding: "10px 14px",
        cursor: "pointer",
        border: `1px solid ${selected ? "var(--color-accent-700)" : "var(--color-divider)"}`,
        background: selected ? "var(--color-accent-100)" : "transparent",
        display: "flex",
        flexDirection: "column",
        gap: 6,
      }}
    >
      <button
        type="button"
        aria-pressed={selected}
        onClick={() => onSelect(option.rankKey)}
        className="pp-unstyled"
        style={{
          cursor: "pointer",
          display: "flex",
          flexDirection: "column",
          gap: 6,
        }}
      >
        <span style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
          <span style={{ fontSize: 12, letterSpacing: ".12em", textTransform: "uppercase", color: "var(--color-accent-700)" }}>
            {planOptionTitle(option.key, option.changes.length)}
          </span>
          {option.recommended ? <Tag variant="accent">Recommended</Tag> : null}
        </span>
        <span style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 18, lineHeight: 1.1 }}>
          {option.label}
        </span>
        <span style={{ display: "grid", gridTemplateColumns: "repeat(3, minmax(0,1fr))", gap: 10, fontSize: 12, color: "var(--color-neutral-700)" }}>
          <MetricCell label="Delivery">
            <span style={{ color: "var(--color-text)", fontWeight: 500 }}>{option.outcome ?? "—"}</span>
            {formatDate(option.projectedCompletion) ? <span>{formatDate(option.projectedCompletion)}</span> : null}
          </MetricCell>
          <MetricCell label="Cost">
            <span style={{ color: "var(--color-text)", fontWeight: 500 }}>{formatMoney(option.cost, option.costCurrency) ?? "—"}</span>
          </MetricCell>
          <MetricCell label="Risk left">
            <RiskLeft option={option} />
          </MetricCell>
        </span>
      </button>
    </Blueprint>
  );
}

function MetricCell({ label, children }: { label: string; children: ReactNode }) {
  return (
    <span style={{ display: "flex", flexDirection: "column", gap: 3 }}>
      <span style={{ fontSize: 10.5, letterSpacing: ".06em", textTransform: "uppercase" }}>{label}</span>
      {children}
    </span>
  );
}

/** The residual-risk level as bars (a small-integer scale) with its word (1/2/3 is
 *  Low/Medium/High), or the literal value when it is not on that scale, or `—` when the
 *  contract serves no risk metric. */
function RiskLeft({ option }: { option: Option }) {
  const value = option.metrics.find((m) => m.key === "risk_left")?.value;
  if (value === undefined) return <span style={{ color: "var(--color-text)" }}>—</span>;
  const level = asBarLevel(value);
  if (level === null) return <span style={{ color: "var(--color-text)" }}>{value}</span>;
  const word = riskWord(level);
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
      <Bars value={level} label={`risk left ${value}`} />
      {word ? <span style={{ fontSize: 12.5, color: "var(--color-text)" }}>{word}</span> : null}
    </span>
  );
}
