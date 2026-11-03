/**
 * Operator wording derived from served codes and values. Each helper is a fixed rule over
 * what the read surface serves: a code the rule does not know is shown as served, and a value
 * the rule needs but the read does not serve is omitted.
 */
import type { Need, Option, PlanEntry, Policy, ScopeCount } from "./types.ts";
import { waitingOn } from "./types.ts";
import { formatDate, formatMoney, shortId } from "./format.ts";
import VERB_LABELS from "./verbLabels.json";

/** The fixed code-to-label map over every verb the command surface defines
 *  (`ekg_kinetic::verbs`; a backend test pins that every code has a label). */
export const VERB_LABEL: Readonly<Record<string, string>> = VERB_LABELS;

/** A verb code as the operator reads it ("Split"), or the code as served when the map does
 *  not know it. */
export function verbLabel(code: string): string {
  return VERB_LABEL[code] ?? code;
}

/** A title composed from the distinct verbs of a list of changes, each worded by the fixed
 *  verb-label map ("Split · Expedite"), or "No changes" for none. */
export function verbsTitle(changes: ReadonlyArray<{ verb: string }>): string {
  const verbs = [...new Set(changes.map((c) => verbLabel(c.verb)))];
  return verbs.length ? verbs.join(" · ") : "No changes";
}

/** A plan's display id from its served code: `PL-{code}`. The full plan id stays in the
 *  row's title and accessible label; the view does not derive a label from an id or digest. */
export function planLabel(code: string): string {
  return `PL-${code}`;
}

/** A point's display id from its served code: `AP-{code}`. */
export function pointLabel(code: string): string {
  return `AP-${code}`;
}

/** An auto-applied change's display id from its served code: `AU-{code}`. */
export function appliedLabel(code: string): string {
  return `AU-${code}`;
}

/** The scope kinds in the order a scope line reads them, each with its singular and plural. */
const SCOPE_WORDS: ReadonlyArray<readonly [kind: string, one: string, many: string]> = [
  ["order", "order", "orders"],
  ["work center", "work center", "work centers"],
  ["purchase order", "PO", "PO"],
];

/** A scope as the row's scope line (`2 orders · 2 work centers · 1 PO`): the served counts
 *  in the fixed kind order, a kind the rule does not know after them as served, and nothing
 *  for an empty scope. */
export function scopeLine(scope: ReadonlyArray<ScopeCount>): string {
  const known = SCOPE_WORDS.flatMap(([kind, one, many]) => {
    const count = scope.find((c) => c.kind === kind)?.count;
    return count !== undefined && count > 0 ? [`${count} ${count === 1 ? one : many}`] : [];
  });
  const other = scope.filter((c) => c.count > 0 && !SCOPE_WORDS.some(([kind]) => kind === c.kind)).map((c) => `${c.count} ${c.kind}`);
  return [...known, ...other].join(" · ");
}

/** The risk-left bars as a word (1/2/3 is Low/Medium/High), or null for any other level. */
export function riskWord(level: number | null): string | null {
  return level === 1 ? "Low" : level === 2 ? "Medium" : level === 3 ? "High" : null;
}

/** The served cost limit, from the policy's `cost_within_limit` criterion. */
function costLimit(policy: Policy | null): string | null {
  const value = policy?.criteria.find((c) => c.rule === "cost_within_limit")?.value.trim();
  return value !== undefined && value.length > 0 && Number.isFinite(Number(value)) ? value : null;
}

/** Capitalize the first letter of a served sentence fragment. */
function sentence(text: string): string {
  return text.length > 0 ? text.charAt(0).toUpperCase() + text.slice(1) : text;
}

/**
 * A policy need as the operator reads it, from its served code and values:
 * - a cost over the limit: "{cost} is above the {limit} limit", both in the served currency
 *   (the option's cost currency, the limit's served currency), or the served detail when
 *   either amount, or a common currency, is not served;
 * - a promised-date move: "Moves a promised date";
 * - a structural change: "Splits a manufacturing order" or "Raises capacity";
 * - an unconfirmed supplier: "The supplier has not confirmed";
 * - any other code: its served detail.
 */
export function needText(need: Need, option: Pick<Option, "cost" | "costCurrency">, policy: Policy | null): string {
  switch (need.code) {
    case "cost_over_limit": {
      const limit = costLimit(policy);
      const limitCurrency = policy?.costLimitCurrency ?? null;
      const currency = option.costCurrency ?? limitCurrency;
      if (limit === null || option.cost === null || currency === null) return sentence(need.detail);
      if (option.costCurrency !== null && limitCurrency !== null && option.costCurrency !== limitCurrency) return sentence(need.detail);
      const cost = formatMoney(option.cost, currency);
      const max = formatMoney(limit, currency);
      return cost && max ? `${cost} is above the ${max} limit` : sentence(need.detail);
    }
    case "promised_date_move":
      return "Moves a promised date";
    case "structural_change":
      if (/split/i.test(need.detail)) return "Splits a manufacturing order";
      if (/capacity/i.test(need.detail)) return "Raises capacity";
      return sentence(need.detail);
    case "supplier_unconfirmed":
      return "The supplier has not confirmed";
    default:
      return sentence(need.detail);
  }
}

/** How long ago a plan's slate was attached ("proposed 14 min ago"), or null when the time is
 *  not served. `now` is epoch ms. */
export function proposedAgo(openedAt: string | null, now: number): string | null {
  if (openedAt === null) return null;
  const secs = Number(openedAt);
  if (!Number.isFinite(secs)) return null;
  const minutes = Math.floor((now - secs * 1000) / 60_000);
  if (minutes < 1) return "proposed just now";
  if (minutes < 60) return `proposed ${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `proposed ${hours} h ago`;
  return `proposed ${Math.floor(hours / 24)} d ago`;
}

const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"] as const;
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"] as const;

/** The header clock from the viewer's clock (`Sun 3 Jan · 14:32`), in local time. */
export function headerClock(now: number): string {
  const d = new Date(now);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${DAYS[d.getDay()]} ${d.getDate()} ${MONTHS[d.getMonth()]} · ${hh}:${mm}`;
}

/** The start of the operator's local day `now` falls in, as Unix epoch seconds. This is the
 *  day the approved-plans read counts as "today". */
export function localDayStart(now: number): number {
  const d = new Date(now);
  d.setHours(0, 0, 0, 0);
  return Math.floor(d.getTime() / 1000);
}

/** A local wall-clock time `HH:MM` for an epoch-seconds string, or null if unparseable. */
export function clockTime(epoch: string | null): string | null {
  if (epoch === null) return null;
  const secs = Number(epoch);
  if (!Number.isFinite(secs)) return null;
  const d = new Date(secs * 1000);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/** "Plan K · n change(s)": how an option is called on its card and its changes header. */
export function planOptionTitle(key: string, changes: number): string {
  return `Plan ${key} · ${changes} ${changes === 1 ? "change" : "changes"}`;
}

/** The trace summary line: "{n} plans simulated · {r} rejected · {a} open assumption(s)",
 *  saying "no open assumptions" rather than dropping a zero. */
export function traceSummaryLine(summary: { simulated: number; rejected: number; openAssumptions: number }): string {
  const a = summary.openAssumptions;
  const assumptions = a === 0 ? "no open assumptions" : `${a} open assumption${a === 1 ? "" : "s"}`;
  return `${summary.simulated} ${summary.simulated === 1 ? "plan" : "plans"} simulated · ${summary.rejected} rejected · ${assumptions}`;
}

const STATE_WORD: Readonly<Record<string, string>> = { under_review: "under review" };

/** "waiting for AP-XXXX (open)": the point of a plan that Approve waits for (the promotion
 *  claims every point from `proposed` only), or null when every point is proposed. */
export function waitingText(entry: PlanEntry): string | null {
  const point = waitingOn(entry);
  return point ? `waiting for ${pointLabel(point.code)} (${STATE_WORD[point.state] ?? point.state})` : null;
}

/** An id-shaped token in a served text. The pattern is generic, so the view hardcodes no
 *  namespace: the read decides which ids are entities. */
const ID_PATTERN = /https?:\/\/[^\s<>"'(),;]+/g;

/** A served text split into plain runs and the ids it carries. */
export type TextRun = { kind: "text"; text: string } | { kind: "id"; id: string };

/** Split a served text into plain runs and ids, a trailing `.` or `:` left to the text (the
 *  sentence's punctuation, not the id's). */
export function splitIds(text: string): TextRun[] {
  const runs: TextRun[] = [];
  let at = 0;
  for (const match of text.matchAll(ID_PATTERN)) {
    const id = match[0].replace(/[.:]+$/, "");
    const start = match.index;
    if (start > at) runs.push({ kind: "text", text: text.slice(at, start) });
    runs.push({ kind: "id", id });
    at = start + id.length;
  }
  if (at < text.length) runs.push({ kind: "text", text: text.slice(at) });
  return runs;
}

/** The ids a served text carries, in order of appearance (duplicates kept). */
export function collectIds(text: string): string[] {
  return splitIds(text).flatMap((run) => (run.kind === "id" ? [run.id] : []));
}

/** Whether an id is a canonical entity id (`…/id/{model}/{number}`): only such an id may fall
 *  back to its short form; any other URL stays as served. */
export function isEntityId(id: string): boolean {
  return /^https?:\/\/[^/\s]+\/id\/[a-z0-9._]+\/\d+$/.test(id);
}

/** How an id reads: its served label, else the short form of an entity id, else as served. */
export function idText(id: string, labels: ReadonlyMap<string, string>): string {
  return labels.get(id) ?? (isEntityId(id) ? shortId(id) : id);
}

/** How a value known to be an entity id reads (a change's object, a lane's work center): its
 *  served label, else its short form. */
export function entityText(id: string, labels: ReadonlyMap<string, string>): string {
  return labels.get(id) ?? shortId(id);
}

/** A served text with its ids read as their labels, for a hover title or an accessible name,
 *  where the text cannot carry an element per id. */
export function labelText(text: string, labels: ReadonlyMap<string, string>): string {
  return splitIds(text)
    .map((run) => (run.kind === "id" ? idText(run.id, labels) : run.text))
    .join("");
}

/** A served date-time value (`2027-02-24T12:00:00`, `2027-01-11 12:00:00`). */
const DATE_TIME = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::\d{2}(?:\.\d+)?)?$/;
/** A whole number served with a decimal part of zeros (`1.0`). */
const WHOLE = /^(-?\d+)\.0+$/;

/**
 * A change's from or to value as the operator reads it:
 * - a date-time: "24 Feb", or "24 Feb 08:00" when the other side falls on the same day;
 * - a whole number served as `1.0`: "1";
 * - anything else as served (an entity id included; the view renders it as its label).
 */
export function changeValue(value: string, other: string | null): string {
  const date = DATE_TIME.exec(value.trim());
  if (date) {
    const day = formatDate(value.trim()) ?? value;
    const sameDay = other !== null && DATE_TIME.test(other.trim()) && other.trim().slice(0, 10) === value.trim().slice(0, 10);
    return sameDay ? `${day} ${date[4]}:${date[5]}` : day;
  }
  const whole = WHOLE.exec(value.trim());
  return whole ? (whole[1] ?? value) : value;
}

/**
 * An option's needs as the chips the operator reads. The needs whose actions the policy does
 * not pre-approve (`not_allowlisted`) read as one statement naming the option's verbs the
 * served policy does not auto-apply ("Not pre-approved: Raise capacity · Reassign"), or as
 * their de-duplicated served details when the policy is not served. Every other need is worded
 * by {@link needText}, and each chip is shown once. Each chip keeps the served details it
 * stands for.
 */
export function needChips(
  needs: ReadonlyArray<Need>,
  option: Pick<Option, "cost" | "costCurrency" | "changes">,
  policy: Policy | null,
): { text: string; detail: string }[] {
  const chips: { text: string; detail: string }[] = [];
  const add = (text: string, detail: string) => {
    const same = chips.find((c) => c.text === text);
    if (!same) chips.push({ text, detail });
    else same.detail = [...new Set([...same.detail.split("\n"), ...detail.split("\n")])].join("\n");
  };
  const gated = needs.filter((n) => n.code === "not_allowlisted");
  const details = [...new Set(gated.map((n) => n.detail))].join("\n");
  const verbs = policy ? [...new Set(option.changes.filter((c) => !policy.autoApplyActions.includes(c.verb)).map((c) => verbLabel(c.verb)))] : [];
  for (const need of needs) {
    if (need.code === "not_allowlisted") {
      if (verbs.length > 0) add(`Not pre-approved: ${verbs.join(" · ")}`, details);
      else add(sentence(need.detail), need.detail);
    } else {
      add(needText(need, option, policy), need.detail);
    }
  }
  return chips;
}
