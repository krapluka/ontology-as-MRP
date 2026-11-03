import type { EntitySelection, TimelineInbound, TimelineOrder, TimelineView } from "../../viewmodel/types.ts";
import { formatDate, formatLocalDay, formatMoney, labelOf } from "../../viewmodel/format.ts";
import { EntityBox, Kicker, RiskDot } from "../tabs/shared.tsx";

const DAY = 86_400_000;
const ROW_H = 50;
const LANE_H = 60;
const LABEL_COL = "minmax(200px,270px)";

/** A calendar domain: from a few days before the earliest date (or today) to a week past the
 *  latest, so every served mark sits inside. Unparseable dates are ignored. */
function domain(view: TimelineView, today: string | undefined): { min: number; span: number } {
  const times: number[] = [];
  const add = (iso: string | null | undefined) => {
    if (!iso) return;
    const t = Date.parse(iso);
    if (Number.isFinite(t)) times.push(t);
  };
  add(today);
  for (const o of view.orders) {
    add(o.promise);
    add(o.ready);
  }
  for (const i of view.inbound) add(i.date);
  const lo = times.length ? Math.min(...times) : 0;
  const hi = times.length ? Math.max(...times) : DAY;
  const min = lo - 3 * DAY;
  return { min, span: Math.max(DAY, hi + 7 * DAY - min) };
}

function at(iso: string | null | undefined, min: number, span: number): number | null {
  if (!iso) return null;
  const t = Date.parse(iso);
  return Number.isFinite(t) ? ((t - min) / span) * 100 : null;
}

function pct(n: number): string {
  return `${n}%`;
}

/** One order row: the bar from today to its ready date (when served), the hatched gap past the
 *  promise or the dashed buffer before it, and the promise tick. Only served marks are drawn. */
function OrderRow({ order, min, span, today, subject, selection }: { order: TimelineOrder; min: number; span: number; today: string | undefined; subject: string; selection: EntitySelection | undefined }) {
  const on = order.onPath === true;
  const due = at(order.promise, min, span);
  const ready = at(order.ready, min, span);
  const start = at(today, min, span) ?? 0;
  const late = due !== null && ready !== null && ready > due;
  const slip = late && order.promise && order.ready ? Math.round((Date.parse(order.ready) - Date.parse(order.promise)) / DAY) : null;
  const buffer = !late && due !== null && ready !== null && order.promise && order.ready ? Math.round((Date.parse(order.promise) - Date.parse(order.ready)) / DAY) : null;
  const end = Math.max(due ?? 0, ready ?? 0);
  const rowBg = on ? "var(--color-accent-100)" : "transparent";
  const sub = [formatMoney(order.orderValue, null), formatDate(order.promise) ? `due ${formatDate(order.promise)}` : null].filter(Boolean).join(" · ");
  return (
    <>
      <EntityBox
        iri={order.entity}
        path={order.onPath}
        subject={subject}
        selection={selection}
        style={{ height: ROW_H, display: "flex", alignItems: "center", gap: 10, borderTop: "1px solid var(--color-divider)", background: rowBg, paddingLeft: 8, borderLeft: `3px solid ${on ? "var(--color-accent-700)" : "transparent"}`, minWidth: 0, width: "100%" }}
      >
        <RiskDot atRisk={order.atRisk} />
        <span style={{ minWidth: 0 }}>
          <span style={{ display: "block", fontSize: 13.5, fontWeight: 500, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{labelOf(order.entity, order.label)}</span>
          <span style={{ display: "block", fontSize: 12, color: "var(--color-neutral-700)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{sub}</span>
        </span>
      </EntityBox>
      <div data-track={order.entity} style={{ position: "relative", height: ROW_H, borderTop: "1px solid var(--color-divider)", background: rowBg }}>
        {ready !== null ? (
          <div data-testid="ready-bar" title={`Ready ${formatDate(order.ready) ?? ""}`} style={{ position: "absolute", left: pct(start), width: pct(Math.max(Math.min(ready, due ?? ready) - start, 0.5)), top: 20, height: 10, background: "var(--color-accent-400)" }} />
        ) : null}
        {late && due !== null && ready !== null ? (
          <div data-testid="late-gap" title="Gap past the promise" style={{ position: "absolute", left: pct(due), width: pct(ready - due), top: 16, height: 18, background: "var(--hatch)", border: "1px solid var(--color-accent-900)" }} />
        ) : null}
        {!late && due !== null && ready !== null && due > ready ? (
          <div data-testid="buffer" title="Buffer" style={{ position: "absolute", left: pct(ready), width: pct(due - ready), top: 20, height: 10, background: "var(--color-accent-100)", border: "1px dashed var(--color-accent-500)" }} />
        ) : null}
        {due !== null ? (
          <div data-testid="promise-tick" title={`Promise ${formatDate(order.promise) ?? ""}`} style={{ position: "absolute", left: pct(due), top: 8, height: 34, width: 2, background: "var(--color-text)" }} />
        ) : null}
        {slip !== null || buffer !== null ? (
          <div data-testid="slip-label" style={{ position: "absolute", left: pct(end), top: 16, marginLeft: 10, fontSize: 12.5, whiteSpace: "nowrap", fontWeight: slip !== null ? 600 : 400, color: slip !== null ? "var(--color-accent-900)" : "var(--color-neutral-700)" }}>
            {slip !== null ? `+${slip} ${slip === 1 ? "day" : "days"} late` : `${buffer} d buffer`}
          </div>
        ) : null}
      </div>
    </>
  );
}

/** The inbound lane: every open inbound line as a diamond at its arrival, the point's (on-path)
 *  line filled and bold. */
function InboundLane({ inbound, min, span }: { inbound: TimelineInbound[]; min: number; span: number }) {
  const sorted = [...inbound].sort((a, b) => Date.parse(a.date) - Date.parse(b.date));
  return (
    <div style={{ position: "relative", height: LANE_H, borderTop: "1px solid var(--color-divider)", background: "var(--color-neutral-100)" }}>
      {sorted.map((line, i) => {
        const left = at(line.date, min, span);
        if (left === null) return null;
        const on = line.onPath === true;
        return (
          <div
            key={`${line.entity}-${line.product ?? ""}-${i}`}
            data-entity={line.entity}
            data-path={on ? "on" : line.onPath === false ? "off" : "unserved"}
            title={`Arrives ${formatDate(line.date) ?? ""}`}
            style={{ position: "absolute", left: pct(left), top: [6, 23, 40][i % 3], display: "flex", alignItems: "center", gap: 4, fontSize: 11.5, whiteSpace: "nowrap", color: on ? "var(--color-text)" : "var(--color-neutral-700)", fontWeight: on ? 600 : 400 }}
          >
            <span style={{ width: 8, height: 8, transform: "rotate(45deg)", marginLeft: -4, background: on ? "var(--color-accent-900)" : "var(--color-bg)", border: `1px solid ${on ? "var(--color-accent-900)" : "var(--color-neutral-500)"}` }} />
            {labelOf(line.entity, line.label)}
            {line.quantity !== null ? ` · ${line.quantity}×` : ""}
            {line.cause === true ? " · late" : ""}
          </div>
        );
      })}
    </div>
  );
}

/**
 * The Timeline tab: every served order on a calendar against its promise date, the selected
 * point's order highlighted (accent-100 row, accent-700 bar), with the plant's open inbound
 * purchase-order lines as diamonds on their own lane. Presentational: a date the rows do not
 * serve draws no mark.
 */
export function TimelineTab({ view, subject, today, selection }: { view: TimelineView; subject: string; today?: string; selection?: EntitySelection }) {
  if (view.orders.length === 0 && view.inbound.length === 0) {
    return <p className="text-muted" style={{ fontSize: 13 }}>No timeline rows for this subject.</p>;
  }
  const { min, span } = domain(view, today);
  const weeks: Array<{ left: number; label: string }> = [];
  // Week ticks on the local-time axis, labelled with their local day (not the UTC one).
  for (let t = min + 3 * DAY; t <= min + span; t += 7 * DAY) {
    const left = ((t - min) / span) * 100;
    if (left <= 96) weeks.push({ left, label: formatLocalDay(t) ?? "" });
  }
  const todayAt = at(today, min, span);
  const atRiskValue = view.orders.filter((o) => o.atRisk === true).reduce((sum, o) => sum + (Number(o.orderValue) || 0), 0);
  const riskServed = view.orders.some((o) => o.atRisk !== null);

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: "clamp(14px,2vh,20px)" }}>
      <div style={{ display: "flex", flexWrap: "wrap", alignItems: "flex-end", justifyContent: "space-between", gap: "14px 40px" }}>
        <div>
          <Kicker>
            Promise timeline · {view.orders.length} {view.orders.length === 1 ? "order" : "orders"} · {view.inbound.length} inbound
          </Kicker>
          <div style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: "clamp(24px,2.2vw,34px)", lineHeight: 1.12, marginTop: 2 }}>
            {view.orders.length === 0
              ? "No promised orders"
              : riskServed
                ? `${formatMoney(String(atRiskValue), null) ?? "0"} of promises at risk`
                : "Risk status not served"}
          </div>
        </div>
        <div style={{ display: "flex", flexWrap: "wrap", gap: "8px 20px", fontSize: 12.5, color: "var(--color-neutral-700)" }}>
          <span style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <span style={{ width: 22, height: 9, background: "var(--color-accent-400)" }} /> Materials covered
          </span>
          <span style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <span style={{ width: 22, height: 9, border: "1px dashed var(--color-accent-500)", background: "var(--color-accent-100)" }} /> Buffer
          </span>
          <span style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <span style={{ width: 22, height: 11, background: "var(--hatch)", border: "1px solid var(--color-accent-900)" }} /> Gap past promise
          </span>
          <span style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <span style={{ width: 2, height: 14, background: "var(--color-text)" }} /> Promise date
          </span>
          <span style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <span style={{ width: 8, height: 8, transform: "rotate(45deg)", background: "var(--color-accent-900)" }} /> Inbound PO
          </span>
        </div>
      </div>

      <div style={{ display: "grid", gridTemplateColumns: `${LABEL_COL} minmax(0,1fr)`, borderTop: "1px solid var(--color-divider)" }}>
        <div style={{ height: 30, display: "flex", alignItems: "center", fontSize: 11.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>Order</div>
        <div style={{ position: "relative", height: 30, overflow: "hidden" }}>
          {weeks.map((w) => (
            <div key={w.left} style={{ position: "absolute", left: pct(w.left), top: 0, bottom: 0, borderLeft: "1px solid var(--color-divider)", paddingLeft: 5, display: "flex", alignItems: "center", fontSize: 11.5, color: "var(--color-neutral-700)", whiteSpace: "nowrap" }}>
              {w.label}
            </div>
          ))}
          {todayAt !== null ? (
            <div style={{ position: "absolute", left: pct(todayAt), top: 0, bottom: 0, borderLeft: "2px solid var(--color-accent)", padding: "0 5px", display: "flex", alignItems: "center", fontSize: 11.5, fontWeight: 500, color: "var(--color-accent-700)", background: "var(--color-bg)" }}>
              Today
            </div>
          ) : null}
        </div>

        <div style={{ height: LANE_H, display: "flex", flexDirection: "column", justifyContent: "center", borderTop: "1px solid var(--color-divider)", paddingRight: 12 }}>
          <div style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 16 }}>Inbound purchase orders</div>
          <div style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>
            {view.inbound.length} open {view.inbound.length === 1 ? "line" : "lines"}
          </div>
        </div>
        <InboundLane inbound={view.inbound} min={min} span={span} />

        {view.orders.map((order) => (
          <OrderRow key={order.entity} order={order} min={min} span={span} today={today} subject={subject} selection={selection} />
        ))}
      </div>
    </div>
  );
}
