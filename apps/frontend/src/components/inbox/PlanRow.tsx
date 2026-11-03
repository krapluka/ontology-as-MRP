import type { InboxItem } from "../../viewmodel/types.ts";
import { itemId } from "../../viewmodel/types.ts";
import { severityMeta } from "../../viewmodel/format.ts";
import { labelText, planLabel, pointLabel, proposedAgo, scopeLine, waitingText } from "../../viewmodel/wording.ts";
import { Named } from "../primitives/Named.tsx";
import { useEntityLabelMap } from "../primitives/entityLabels.ts";

/** The status chip of a row whose plan cannot be approved from the inbox (or null). */
function statusChip(item: InboxItem): string | null {
  switch (item.kind) {
    case "plan":
      return null;
    case "no_option":
      return "no option";
    case "pending":
      return "slate pending";
    case "unavailable":
      return "plan unavailable";
  }
}

/**
 * One inbox row: one plan awaiting a decision. The left block is the severity fill with the
 * plan's baseline days late (no number when the slip is not served, and no change count in
 * its place); then `PL-XXXX` and the scope of the recommended option
 * (`2 orders · 2 work centers · 1 PO`); the title (the recommended option's label, else the
 * situation); and `Resolves AP-… · AP-…` with "proposed N ago". A plan that cannot be approved
 * here carries a status chip ("no option", "slate pending", "plan unavailable", or "waiting for
 * AP-XXXX (open)"), so it is listed, not dropped. The full plan id is the row's label. The
 * title's ids read as their labels, and it is clamped to two lines with the full title on
 * hover.
 */
export function PlanRow({
  item,
  selected,
  now,
  onSelect,
}: {
  item: InboxItem;
  selected: boolean;
  /** The current time, epoch ms (for "proposed N ago"). */
  now: number;
  onSelect: (id: string) => void;
}) {
  const id = itemId(item);
  const plan = item.kind === "plan" || item.kind === "no_option" ? item.plan : null;
  const recommended = plan ? (plan.options.find((o) => o.recommended) ?? plan.options[0]) : undefined;
  const scope = recommended ? scopeLine(recommended.scope) : "";
  const sev = severityMeta(plan?.severity ?? null);
  const late = plan !== null && plan.daySlip !== null && plan.daySlip > 0;
  const chip = statusChip(item) ?? waitingText(item.entry);
  const title = recommended?.label ?? item.entry.situation;
  // The title as it reads, its ids by their labels, for the hover and the accessible name.
  const titleText = labelText(title, useEntityLabelMap());
  const resolves = `Resolves ${item.entry.resolves.map((p) => pointLabel(p.code)).join(" · ")}`;
  const opened = proposedAgo(item.entry.openedAt, now);
  const label = planLabel(item.entry.code);
  const accessible = [`${label} (${id})`, scope, chip, titleText, resolves, opened].filter(Boolean).join(" · ");

  return (
    <button
      type="button"
      className="pp-hover"
      aria-pressed={selected}
      aria-label={accessible}
      title={id}
      data-plan={id}
      onClick={() => onSelect(id)}
      style={{
        display: "grid",
        gridTemplateColumns: "68px minmax(0,1fr)",
        textAlign: "left",
        width: "100%",
        cursor: "pointer",
        font: "inherit",
        color: "inherit",
        border: `1px solid ${selected ? "var(--color-accent-700)" : "var(--color-divider)"}`,
        background: selected ? "var(--color-accent-100)" : "transparent",
      }}
    >
      <span
        title={sev?.label}
        style={{
          display: "flex",
          flexDirection: "column",
          justifyContent: "center",
          alignItems: "center",
          gap: 2,
          borderRight: `1px solid ${sev ? sev.border : "var(--color-divider)"}`,
          background: sev ? sev.background : "var(--color-neutral-100)",
          color: sev ? sev.color : "inherit",
        }}
      >
        {late ? (
          <span data-testid="days-late" style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 20, lineHeight: 1 }}>
            +{plan?.daySlip}
          </span>
        ) : null}
        <span style={{ fontSize: 10, letterSpacing: ".06em", textTransform: "uppercase", textAlign: "center", lineHeight: 1.2 }}>
          {late ? "days late" : plan?.delivery === "at_risk" ? "at risk" : plan?.delivery === "on_time" ? "on time" : ""}
        </span>
      </span>
      <span style={{ display: "flex", flexDirection: "column", gap: 3, padding: "8px 12px", minWidth: 0 }}>
        <span style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 12.5, color: "var(--color-neutral-700)" }}>
          <span style={{ fontWeight: 500, color: "var(--color-text)", flex: "none", whiteSpace: "nowrap" }}>{label}</span>
          {scope ? <span style={{ minWidth: 0, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{scope}</span> : null}
          {chip ? (
            <span className="tag tag-neutral" style={{ flex: "none" }}>
              {chip}
            </span>
          ) : null}
        </span>
        <span
          data-testid="plan-row-title"
          title={titleText}
          style={{
            fontFamily: "var(--font-heading)",
            fontWeight: 600,
            fontSize: 16.5,
            lineHeight: 1.15,
            display: "-webkit-box",
            WebkitLineClamp: 2,
            WebkitBoxOrient: "vertical",
            overflow: "hidden",
            overflowWrap: "anywhere",
          }}
        >
          <Named text={title} />
        </span>
        <span style={{ display: "flex", gap: 12, fontSize: 12.5, color: "var(--color-neutral-700)" }}>
          <span style={{ minWidth: 0, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{resolves}</span>
          {opened ? <span style={{ marginLeft: "auto", flex: "none", whiteSpace: "nowrap" }}>{opened}</span> : null}
        </span>
      </span>
    </button>
  );
}
