import type { ApprovedPlan } from "../../viewmodel/types.ts";
import { Check } from "../primitives/Check.tsx";
import { Btn } from "../primitives/Btn.tsx";
import { formatLocalDay, isToday, undoOpen, undoStatus } from "../../viewmodel/format.ts";
import { clockTime, planLabel, pointLabel, verbsTitle } from "../../viewmodel/wording.ts";

/**
 * One plan in the "Approved by you today" list: its verbs as the title, then
 * `PL-XXXX · HH:MM · Resolves AP-…`, and "partly undone · N left" after an undo that reversed
 * only part of it. Undo is offered only while the plan is reversible and its reversal window
 * is still open at `now` (checked against the live clock, so it disappears when the window
 * closes); otherwise the row says why. `busy` marks a command in flight; `disabled` is set
 * while no reviewer is entered.
 */
export function ApprovedPlanRow({
  approved,
  now,
  onUndo,
  busy = false,
  disabled = false,
}: {
  approved: ApprovedPlan;
  /** The current time, epoch ms. */
  now: number;
  onUndo: (approved: ApprovedPlan) => void;
  busy?: boolean;
  disabled?: boolean;
}) {
  const at = clockTime(approved.promotedAt);
  const day = approved.promotedAt !== null && !isToday(approved.promotedAt, now) ? formatLocalDay(Number(approved.promotedAt) * 1000) : null;
  const when = day && at ? `${day} ${at}` : (day ?? at);
  const resolves = `Resolves ${approved.resolves.map((p) => pointLabel(p.code)).join(" · ")}`;
  const total = approved.changes.length;
  const partly = approved.remaining > 0 && approved.remaining < total ? `partly undone · ${approved.remaining} left` : null;
  const open = undoOpen(approved, now);
  const status = undoStatus(approved, now);

  return (
    <div
      data-approved={approved.id}
      style={{
        display: "grid",
        gridTemplateColumns: "24px minmax(0,1fr) auto",
        gap: 10,
        alignItems: "center",
        padding: "7px 12px",
        border: "1px solid var(--color-divider)",
      }}
    >
      <Check size={18} />
      <div style={{ minWidth: 0 }} title={approved.id}>
        <div style={{ fontSize: 14, fontWeight: 500, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
          {verbsTitle(approved.changes)}
        </div>
        <div style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>
          {[planLabel(approved.code), when, resolves, partly].filter(Boolean).join(" · ")}
        </div>
      </div>
      {open ? (
        <Btn variant="ghost" style={{ fontSize: 13 }} title={status} busy={busy} disabled={disabled} onClick={() => onUndo(approved)}>
          Undo
        </Btn>
      ) : (
        <span style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>{status}</span>
      )}
    </div>
  );
}
