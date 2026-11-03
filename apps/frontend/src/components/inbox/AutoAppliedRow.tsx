import type { AppliedChange } from "../../viewmodel/types.ts";
import { Check } from "../primitives/Check.tsx";
import { Btn } from "../primitives/Btn.tsx";
import { formatLocalDay, isToday, undoOpen, undoStatus } from "../../viewmodel/format.ts";
import { clockTime, entityText, verbLabel } from "../../viewmodel/wording.ts";
import { useEntityLabelMap } from "../primitives/entityLabels.ts";

/**
 * One auto-applied change in the "Applied by agent" list: its served title (else its first
 * change's verb/object), the meta "HH:MM · object" (the applied time, with its date when it
 * was not applied today, and the first change's object; the cost is not served for an applied
 * change, so it is omitted), and an Undo control. Undo is offered only while the change is
 * reversible and its reversal window is still open at `now`, checked against the live clock,
 * so it disappears when the window closes. Otherwise the row says the window closed (and
 * when) or that the change is not reversible. `busy` marks an undo in flight; `disabled` is
 * set while no reviewer is entered.
 */
export function AutoAppliedRow({
  applied,
  now,
  selected,
  onSelect,
  onUndo,
  busy = false,
  disabled = false,
}: {
  applied: AppliedChange;
  /** The current time, epoch ms. */
  now: number;
  selected: boolean;
  onSelect: (subject: string) => void;
  onUndo: (applied: AppliedChange) => void;
  busy?: boolean;
  disabled?: boolean;
}) {
  const first = applied.changes[0];
  const labels = useEntityLabelMap();
  const at = clockTime(applied.appliedAt);
  // The object's served label, else the label the read serves for its id, else its short id.
  const objectName = first ? (first.label ?? entityText(first.object, labels)) : null;
  const object = objectName?.split(" · ")[0] ?? null;
  const day = applied.appliedAt !== null && !isToday(applied.appliedAt, now) ? formatLocalDay(Number(applied.appliedAt) * 1000) : null;
  const open = undoOpen(applied, now);
  const status = undoStatus(applied, now);

  return (
    <div
      className="pp-hover"
      style={{
        display: "grid",
        gridTemplateColumns: "24px minmax(0,1fr) auto",
        gap: 10,
        alignItems: "center",
        padding: "7px 12px",
        border: `1px solid ${selected ? "var(--color-accent-700)" : "var(--color-divider)"}`,
        background: selected ? "var(--color-accent-100)" : "transparent",
      }}
    >
      <Check size={18} />
      <button
        type="button"
        aria-pressed={selected}
        onClick={() => onSelect(applied.subject)}
        className="pp-unstyled"
        style={{ cursor: "pointer", minWidth: 0 }}
      >
        <div style={{ fontSize: 14, fontWeight: 500, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
          {applied.title ?? (first ? `${verbLabel(first.verb)} · ${objectName}` : entityText(applied.subject, labels))}
        </div>
        <div style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>
          {[day && at ? `${day} ${at}` : (day ?? at), object].filter(Boolean).join(" · ")}
        </div>
      </button>
      {open ? (
        <Btn variant="ghost" style={{ fontSize: 13 }} title={status} busy={busy} disabled={disabled} onClick={() => onUndo(applied)}>
          Undo
        </Btn>
      ) : (
        <span style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>{status}</span>
      )}
    </div>
  );
}
