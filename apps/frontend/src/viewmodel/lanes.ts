/**
 * The work-center lanes' time axis: the domain the served bars span, its date range for the
 * heading, and its day ticks, all derived from the bars alone.
 */
import type { Lane } from "./types.ts";
import { formatLocalDay } from "./format.ts";

/** A served naive date/time as local epoch ms: a bare date (`2027-01-25`) reads as its local
 *  midnight, like a date-time without a zone, so both place on the same local axis. */
export function parseLocal(value: string): number {
  return Date.parse(/^\d{4}-\d{2}-\d{2}$/.test(value) ? `${value}T00:00:00` : value);
}

export function laneDomain(lanes: Lane[]): { min: number; span: number; served: boolean } {
  const times: number[] = [];
  for (const lane of lanes) {
    for (const b of [...lane.before, ...lane.after]) {
      times.push(parseLocal(b.start), parseLocal(b.end));
    }
  }
  // Drop unparseable dates: a single bad value would turn min/max into NaN and collapse the
  // whole lane chart. Fall back to defaults when none remain.
  const finite = times.filter(Number.isFinite);
  const min = finite.length ? Math.min(...finite) : 0;
  const max = finite.length ? Math.max(...finite) : 1;
  return { min, span: Math.max(1, max - min), served: finite.length > 0 };
}

/** The lanes' date range from the bar domain (`24 Jan–7 Feb`, or `9–14 Jan` across months),
 *  or null when no bar date is readable. */
export function laneRange(lanes: Lane[]): string | null {
  const { min, span, served } = laneDomain(lanes);
  if (!served) return null;
  const first = formatLocalDay(min);
  const last = formatLocalDay(min + span);
  if (first === null || last === null) return null;
  if (first === last) return first;
  const [d1, m1] = first.split(" ");
  const [d2, m2] = last.split(" ");
  return m1 === m2 ? `${d1}–${d2} ${m2}` : `${first}–${last}`;
}

/** The day ticks across the bar domain: every local midnight inside it, placed at its share
 *  of the span and labelled with its day of the month (every other day past two weeks). */
export function dayTicks(min: number, span: number): { left: number; label: string }[] {
  const start = new Date(min);
  start.setHours(0, 0, 0, 0);
  if (start.getTime() < min) start.setDate(start.getDate() + 1);
  const days: number[] = [];
  for (const d = new Date(start); d.getTime() <= min + span; d.setDate(d.getDate() + 1)) days.push(d.getTime());
  const step = days.length > 14 ? 2 : 1;
  return days
    .filter((_, i) => i % step === 0)
    .map((t) => ({ left: ((t - min) / span) * 100, label: String(new Date(t).getDate()) }));
}
