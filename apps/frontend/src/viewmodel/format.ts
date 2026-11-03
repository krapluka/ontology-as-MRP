/**
 * Display helpers shared by the presentational components. Each is a pure transform of
 * contract data that the operator could reconstruct from what is shown.
 */
import type { AppliedChange, Delivery } from "./types.ts";

/** Shortens an entity IRI to a readable tail (`…/sale.order/147` becomes `sale.order/147`).
 *  A change object already carrying a ` · ` label keeps the label part. */
export function shortId(id: string): string {
  const [iri, ...rest] = id.split(" · ");
  const tail = (iri ?? id).split("/id/").at(-1) ?? iri ?? id;
  const short = tail.replace(/^https?:\/\/[^/]+\/id\//, "");
  return rest.length ? `${short} · ${rest.join(" · ")}` : short;
}

/** An entity's served display name, else its shortened IRI. */
export function labelOf(iri: string, label: string | null | undefined): string {
  return label ?? shortId(iri);
}

/** Whether a tri-state marker is served at all on a set of rows (at least one carries it). */
export function isServed(markers: ReadonlyArray<boolean | null>): boolean {
  return markers.some((m) => m !== null);
}

/** Pluralize a change count ("1 change" / "4 changes"). */
export function changeCount(n: number): string {
  return `${n} ${n === 1 ? "change" : "changes"}`;
}

/** A risk metric value parsed as a small integer, or null when it is not one (callers render
 *  bars only for an integer level and fall back to text otherwise). */
export function asBarLevel(value: string): number | null {
  const n = Number(value);
  return Number.isInteger(n) && n >= 0 && n <= 3 ? n : null;
}

/** Format an epoch-seconds string as a local wall-clock time, or null if unparseable. */
export function epochToTime(epoch: string | null): string | null {
  if (!epoch) return null;
  const secs = Number(epoch);
  if (!Number.isFinite(secs)) return null;
  return new Date(secs * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** Parse an epoch-seconds string into epoch milliseconds, or null if absent/unparseable. */
function epochMs(epoch: string | null): number | null {
  if (!epoch) return null;
  const secs = Number(epoch);
  return Number.isFinite(secs) ? secs * 1000 : null;
}

/** Whether an auto-applied change can still be undone at `now` (epoch ms): it is reversible
 *  and its served reversal window ends after `now`. Checked against the live time, not the
 *  served fields alone, so Undo is withdrawn the moment the window closes. */
export function undoOpen(applied: Pick<AppliedChange, "reversible" | "undoUntil">, now: number): boolean {
  const until = epochMs(applied.undoUntil);
  return applied.reversible && until !== null && until > now;
}

/** Whether an epoch-seconds instant falls on the operator's local day of `now` (from local
 *  midnight up to the next one). An unknown instant is not "today". */
export function isToday(epoch: string | null, now: number): boolean {
  const at = epochMs(epoch);
  if (at === null) return false;
  const start = new Date(now);
  start.setHours(0, 0, 0, 0);
  const end = new Date(start);
  end.setDate(end.getDate() + 1);
  return at >= start.getTime() && at < end.getTime();
}

/** The undo state of an auto-applied change at `now`, as the operator reads it: open until a
 *  time, closed at a time, or not reversible at all. */
export function undoStatus(applied: Pick<AppliedChange, "reversible" | "undoUntil">, now: number): string {
  if (!applied.reversible) return "not reversible";
  const until = epochToTime(applied.undoUntil);
  if (undoOpen(applied, now)) return until ? `Undo available until ${until}` : "Undo available";
  return until ? `reversal window closed at ${until}` : "no reversal window served";
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"] as const;

/** Format an ISO date (`2027-01-29T…`) as `29 Jan`, or null if unparseable. Deterministic
 *  (no locale), so fixtures render identically everywhere. */
export function formatDate(iso: string | null): string | null {
  if (!iso) return null;
  const match = /^(\d{4})-(\d{2})-(\d{2})/.exec(iso);
  if (!match) return null;
  const month = Number(match[2]);
  const day = Number(match[3]);
  if (month < 1 || month > 12 || day < 1 || day > 31) return null;
  return `${day} ${MONTHS[month - 1]}`;
}

/** Formats an instant (epoch ms) as its local calendar day (`6 Jan`), or null if not finite.
 *  For a mark placed on a local-time axis (the served dates are naive, so `Date.parse` reads
 *  them as local time): a UTC rendering would name the previous day east of UTC. */
export function formatLocalDay(ms: number): string | null {
  if (!Number.isFinite(ms)) return null;
  const d = new Date(ms);
  return `${d.getDate()} ${MONTHS[d.getMonth()]}`;
}

/** Formats a decimal string as money in its served ISO 4217 currency (`20700.00` in `USD`
 *  is `$20,700`), or null if not a number. A value served without a currency (or with a code
 *  the runtime does not know) renders as a plain number (`20,700`); the view does not assume
 *  a currency. `compact` renders the header-KPI shape (`$27.7K`). Deterministic (`en-US`), so
 *  fixtures render identically everywhere. */
export function formatMoney(
  value: string | null,
  currency: string | null,
  { compact = false }: { compact?: boolean } = {},
): string | null {
  if (value === null) return null;
  const n = Number(value);
  if (!Number.isFinite(n)) return null;
  const shape: Intl.NumberFormatOptions = compact
    ? { notation: "compact", minimumFractionDigits: 0, maximumFractionDigits: 1 }
    : { minimumFractionDigits: 0, maximumFractionDigits: 0 };
  if (currency) {
    try {
      return new Intl.NumberFormat("en-US", { ...shape, style: "currency", currency }).format(n);
    } catch {
      // An unknown code is not a currency we can name: fall through to the plain number.
    }
  }
  return new Intl.NumberFormat("en-US", shape).format(n);
}

/** Read a served `delivery` value, or null when it is absent or not one the contract emits. */
export function asDelivery(value: string | null | undefined): Delivery | null {
  return value === "on_time" || value === "late" || value === "at_risk" ? value : null;
}

/** The severity-fill styling for a tier, or null for an unknown/absent tier (the view shows
 *  a severity fill only when the contract served a known tier). */
export type SeverityMeta = { label: string; background: string; color: string; border: string };
export function severityMeta(severity: string | null): SeverityMeta | null {
  switch (severity) {
    case "critical":
      return { label: "Critical", background: "var(--color-accent-900)", color: "var(--color-bg)", border: "var(--color-accent-900)" };
    case "high":
      return { label: "High", background: "var(--color-accent-700)", color: "var(--color-bg)", border: "var(--color-accent-700)" };
    case "watch":
      return { label: "Watch", background: "var(--color-accent-200)", color: "var(--color-accent-800)", border: "var(--color-accent-500)" };
    case "on_track":
      return { label: "On track", background: "transparent", color: "var(--color-neutral-700)", border: "var(--color-neutral-500)" };
    default:
      return null;
  }
}

/** Formats an on-time-delivery ratio in [0,1] as a whole-percent label (`0.9200` is `92%`),
 *  or null when unserved/out of range (a supplier with no figure renders nothing, not 0%). */
export function formatOtd(ratio: number | null): string | null {
  if (ratio === null || !Number.isFinite(ratio) || ratio < 0 || ratio > 1) return null;
  return `${Math.round(ratio * 100)}%`;
}

/** An option's Delivery cell from its served outcome: "On time", "N d late" (from a positive
 *  slip), "Late" (late with no day count served) or "At risk"; null when no outcome is served.
 *  "On time" only when the served delivery is `on_time`. */
export function deliveryLabel(delivery: Delivery | null, days: number | null): string | null {
  switch (delivery) {
    case "on_time":
      return "On time";
    case "late":
      return days !== null && days > 0 ? `${days} d late` : "Late";
    case "at_risk":
      return "At risk";
    default:
      return null;
  }
}

/** A day-slip as a short label, read together with the served delivery: "on time" only when
 *  delivery is `on_time`; a positive slip reads "+N days late"; otherwise the delivery itself
 *  ("late" / "at risk"), or null when neither is served. A slip of 0 on an `at_risk` or
 *  unserved delivery does not read "on time". */
export function slipLabel(days: number | null, delivery: Delivery | null): string | null {
  if (delivery === "on_time") return "on time";
  if (days !== null && days > 0) return `+${days} ${days === 1 ? "day" : "days"} late`;
  if (delivery === "late") return "late";
  if (delivery === "at_risk") return "at risk";
  return null;
}
