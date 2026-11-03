import { useState } from "react";
import type {
  EntitySelection,
  Sourcing,
  SupplierDetail,
  SupplierStock,
  SupplierSummary,
  SuppliersView,
} from "../../viewmodel/types.ts";
import { Blueprint } from "../primitives/Blueprint.tsx";
import { formatDate, formatMoney, formatOtd, labelOf } from "../../viewmodel/format.ts";
import { EntityBox, Kicker, PATH_NOT_SERVED, RiskDot, SectionLabel } from "../tabs/shared.tsx";
import { defaultSupplier, exposureOf, type DefaultPick } from "./defaultSupplier.ts";

const SOURCING_LABEL: Readonly<Record<Sourcing, string>> = { single: "Single", primary: "Primary", backup: "Backup" };
const SOURCE_TEXT: Readonly<Record<Sourcing, string>> = { single: "Single source", primary: "Primary source", backup: "Backup source" };

/**
 * The note above the detail: why this supplier is shown. `pick` is the default pick, or null
 * once the operator picked a row, in which case only the path status is stated. With
 * `on_path` unserved, the note does not imply the supplier is on the point's path.
 */
function detailNote(summary: SupplierSummary, pick: DefaultPick | null, pathServed: boolean): string | null {
  const notImplied = "it is not implied to be on this point's path.";
  if (!pathServed) {
    if (pick === null) return `${PATH_NOT_SERVED} — ${notImplied}`;
    if (pick.basis === "first") return `${PATH_NOT_SERVED} — showing the first supplier; ${notImplied}`;
    return pick.exposureComplete
      ? `${PATH_NOT_SERVED} — showing the highest exposure; ${notImplied}`
      : `${PATH_NOT_SERVED} — showing a supplier that may carry the highest exposure (not every exposure is served); ${notImplied}`;
  }
  if (summary.onPath === true) return "On this point's path.";
  if (pick === null) return null;
  if (pick.basis === "first") return "No supplier is on this point's path — showing the first supplier (exposure is not served).";
  return pick.exposureComplete
    ? "No supplier is on this point's path — showing the highest exposure."
    : "No supplier is on this point's path — showing a supplier that may carry the highest exposure (not every exposure is served).";
}

function SourcingChip({ sourcing }: { sourcing: Sourcing | null }) {
  if (sourcing === null) return <span style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>—</span>;
  const single = sourcing === "single";
  return (
    <span
      style={{
        padding: "2px 7px",
        fontSize: 11.5,
        whiteSpace: "nowrap",
        border: `1px solid ${single ? "var(--color-accent-700)" : "var(--color-divider)"}`,
        color: single ? "var(--color-accent-800)" : "var(--color-neutral-700)",
        background: single ? "var(--color-accent-100)" : "transparent",
      }}
    >
      {SOURCING_LABEL[sourcing]}
    </span>
  );
}

/** The on-time bar: a filled share of the OTD ratio, or "—" when the supplier serves no figure
 *  (no completed receipts in the window), which is not the same as 0%. */
function OtdBar({ otd }: { otd: number | null }) {
  const label = formatOtd(otd);
  if (label === null || otd === null) return <span style={{ fontSize: 12.5, color: "var(--color-neutral-700)" }}>—</span>;
  return (
    <span style={{ display: "flex", alignItems: "center", gap: 7 }}>
      <span style={{ flex: 1, height: 6, background: "var(--color-neutral-200)", position: "relative" }}>
        <span style={{ position: "absolute", left: 0, top: 0, bottom: 0, width: `${otd * 100}%`, background: otd < 0.85 ? "var(--color-accent-900)" : "var(--color-accent-400)" }} />
      </span>
      <span style={{ fontSize: 12.5, width: 36 }}>{label}</span>
    </span>
  );
}

function num(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1);
}

/** The stock gauge for one sourced component: on hand (fill), safety stock (line) and the
 *  30-day demand (diamond, only when served). A component below safety is drawn in the risk
 *  fill. Only served figures are drawn. */
function StockGauge({ stock }: { stock: SupplierStock }) {
  const { onHand, safety, demand } = stock;
  const max = Math.max(onHand ?? 0, safety ?? 0, demand ?? 0, 1) * 1.1;
  const below = onHand !== null && safety !== null && onHand < safety;
  const name = stock.stockOf ? labelOf(stock.stockOf, stock.label) : (stock.label ?? "component");
  return (
    <Blueprint padding="12px 14px" style={{ display: "flex", flexDirection: "column", gap: 9 }}>
      <div style={{ display: "flex", flexWrap: "wrap", alignItems: "baseline", gap: "4px 10px" }}>
        <span style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 18 }}>{name}</span>
        {stock.cause === true ? <span style={{ fontSize: 12, color: "var(--color-accent-800)" }}>cause</span> : null}
        <span style={{ marginLeft: "auto", fontSize: 12.5 }}>{stock.sourcing ? SOURCE_TEXT[stock.sourcing] : ""}</span>
      </div>
      <div style={{ position: "relative", height: 12, background: "var(--color-neutral-200)" }}>
        {onHand !== null ? (
          <div style={{ position: "absolute", left: 0, top: 0, bottom: 0, width: `${(onHand / max) * 100}%`, background: below ? "var(--color-accent-900)" : "var(--color-accent-400)" }} />
        ) : null}
        {safety !== null ? (
          <div title={`Safety ${num(safety)}`} style={{ position: "absolute", left: `${(safety / max) * 100}%`, top: -4, bottom: -4, width: 2, background: "var(--color-text)" }} />
        ) : null}
        {demand !== null ? (
          <div
            data-testid="demand-marker"
            title={`30-day demand ${num(demand)}`}
            style={{ position: "absolute", left: `${(demand / max) * 100}%`, top: 2, width: 8, height: 8, marginLeft: -4, border: "1.5px solid var(--color-accent-900)", background: "var(--color-bg)", transform: "rotate(45deg)" }}
          />
        ) : null}
      </div>
      <div style={{ display: "flex", flexWrap: "wrap", gap: "4px 14px", fontSize: 12.5, color: "var(--color-neutral-700)" }}>
        <span>
          <b style={{ fontWeight: 500, color: "var(--color-text)" }}>{onHand === null ? "—" : num(onHand)}</b> on hand
        </span>
        {safety !== null ? <span>safety {num(safety)}</span> : null}
        {demand !== null ? <span>demand {num(demand)}</span> : null}
      </div>
    </Blueprint>
  );
}

function SupplierDetailPanel({
  summary,
  detail,
  labels,
  note,
  subject,
  selection,
}: {
  summary: SupplierSummary;
  detail: SupplierDetail | undefined;
  labels: SuppliersView["labels"];
  note: string | null;
  subject: string;
  selection: EntitySelection | undefined;
}) {
  const facts = [summary.leadTimeDays === null ? null : `${summary.leadTimeDays}d lead`, formatOtd(summary.otd) ? `${formatOtd(summary.otd)} on time` : null].filter(Boolean);
  const stock = detail?.stock ?? [];
  const openPos = detail?.openPos ?? [];
  const dependents = detail?.dependentOrders ?? [];
  // "Open action point · AP-XXXX": the supplier itself, or its first dependent order, when it
  // is a point a listed plan resolves. Offered only when the view can open that plan; it
  // commits nothing.
  const openPoint = (() => {
    const label = selection?.pointLabel;
    if (!label || !selection.onReview) return null;
    const candidates: { iri: string; name: string }[] = [
      { iri: summary.supplier, name: labelOf(summary.supplier, summary.label) },
      ...dependents.flatMap((o) => (o.order ? [{ iri: o.order, name: labelOf(o.order, o.label) }] : [])),
    ];
    for (const c of candidates) {
      const point = selection.selectable.has(c.iri) ? label(c.iri) : null;
      if (point) return { ...c, point };
    }
    return null;
  })();
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 16, minWidth: 0 }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
          <SourcingChip sourcing={summary.sourcing} />
          {facts.length ? <span style={{ fontSize: 13, color: "var(--color-neutral-700)" }}>{facts.join(" · ")}</span> : null}
        </div>
        <div data-testid="supplier-detail-name" style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: "clamp(24px,2.1vw,32px)", lineHeight: 1.1 }}>
          {labelOf(summary.supplier, summary.label)}
        </div>
        {note ? <div role="note" style={{ fontSize: 13.5, color: "var(--color-neutral-700)" }}>{note}</div> : null}
      </div>

      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        <SectionLabel>On hand · safety stock (line) · 30-day demand (dot)</SectionLabel>
        {stock.length === 0 ? (
          <div style={{ fontSize: 13, color: "var(--color-neutral-700)" }}>No sourced components served.</div>
        ) : (
          stock.map((s, i) => <StockGauge key={`${s.stockOf ?? "c"}-${i}`} stock={s} />)
        )}
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(160px,1fr))", gap: 18 }}>
        <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <SectionLabel>Open purchase orders</SectionLabel>
          {openPos.length === 0 ? (
            <div style={{ fontSize: 13, color: "var(--color-neutral-700)" }}>None open</div>
          ) : (
            openPos.map((po, i) => (
              <div key={`${po.po ?? "po"}-${i}`} style={{ display: "flex", justifyContent: "space-between", gap: 8, padding: "6px 0", borderBottom: "1px solid var(--color-divider)", fontSize: 13 }}>
                <span style={{ minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  <b style={{ fontWeight: po.onPath === true ? 600 : 500 }}>{po.po ? labelOf(po.po, po.label) : (po.label ?? "PO")}</b>
                  {po.quantity !== null ? ` · ${num(po.quantity)} pcs` : ""}
                  {po.product ? ` ${labelOf(po.product, labels[po.product])}` : ""}
                </span>
                <span style={{ whiteSpace: "nowrap", color: po.cause === true ? "var(--color-accent-900)" : "var(--color-text)", fontWeight: po.cause === true ? 600 : 400 }}>
                  {formatDate(po.arrival) ?? "—"}
                  {po.cause === true ? " · late" : ""}
                </span>
              </div>
            ))
          )}
        </div>
        <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <SectionLabel>Orders depending on it</SectionLabel>
          {dependents.length === 0 ? (
            <div style={{ fontSize: 13, color: "var(--color-neutral-700)" }}>No dependent orders.</div>
          ) : (
            dependents.map((o, i) => (
              <EntityBox
                key={`${o.order ?? "o"}-${i}`}
                iri={o.order ?? ""}
                path={o.onPath}
                subject={subject}
                selection={o.order ? selection : undefined}
                style={{ display: "flex", justifyContent: "space-between", gap: 8, padding: "6px 0", borderBottom: "1px solid var(--color-divider)", fontSize: 13, width: "100%" }}
              >
                <span style={{ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
                  <b style={{ fontWeight: 500 }}>{o.order ? labelOf(o.order, o.label) : (o.label ?? "order")}</b>
                  {formatMoney(o.orderValue, null) ? ` · ${formatMoney(o.orderValue, null)}` : ""}
                </span>
                <span style={{ display: "flex", alignItems: "center", gap: 5, whiteSpace: "nowrap" }}>
                  <RiskDot atRisk={o.atRisk} size={8} />
                  {formatDate(o.commitmentDate) ?? ""}
                </span>
              </EntityBox>
            ))
          )}
        </div>
      </div>

      {openPoint && selection?.onReview ? (
        <Blueprint padding="12px 14px" style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: 12, borderColor: "var(--color-accent-700)" }}>
          <div style={{ flex: 1, minWidth: 180 }}>
            <div style={{ fontSize: 11.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-accent-700)" }}>Open action point · {openPoint.point}</div>
            <div style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 18 }}>{openPoint.name}</div>
          </div>
          <button type="button" className="btn btn-secondary" style={{ fontSize: 14 }} onClick={() => selection.onReview?.(openPoint.iri)}>
            Review options
          </button>
        </Blueprint>
      ) : null}
    </div>
  );
}

const GRID = "minmax(0,1.7fr) 56px 78px minmax(90px,1fr) 88px";

/**
 * The Suppliers tab: the lead / sourcing / on-time / exposure table over the whole supplier
 * base, plus the selected supplier's detail (stock vs safety vs demand gauges, open purchase
 * orders, all dependent orders with their at-risk dot). It opens on the point's supplier (see
 * `defaultSupplier`); selecting a row shows that supplier. Presentational: an unserved OTD,
 * exposure or demand is omitted, not shown as 0.
 */
export function SuppliersTab({ view, subject, selection }: { view: SuppliersView; subject: string; selection?: EntitySelection }) {
  // A pick belongs to the view it was made on, so a new projection (another point) reopens
  // on that point's default supplier.
  const [picked, setPicked] = useState<{ view: SuppliersView; supplier: string } | null>(null);
  const fallback = defaultSupplier(view.summaries);
  if (fallback === null) {
    return <p className="text-muted" style={{ fontSize: 13 }}>No supplier rows for this subject.</p>;
  }
  const pickedHere = picked !== null && picked.view === view ? picked.supplier : null;
  const selectedIri = pickedHere ?? fallback.supplier;
  const current = view.summaries.find((s) => s.supplier === selectedIri) ?? view.summaries[0];
  if (!current) return null;
  const note = detailNote(current, pickedHere === null ? fallback : null, fallback.pathServed);
  const exposureServed = view.summaries.some((s) => s.exposure !== null);
  const components = new Set(Object.values(view.detailBySupplier).flatMap((d) => d.stock.map((s) => s.stockOf)));

  return (
    <div style={{ display: "grid", gridTemplateColumns: "minmax(0,1.55fr) minmax(320px,1fr)", gap: "clamp(20px,2.6vw,40px)", alignItems: "start" }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 14, minWidth: 0 }}>
        <Kicker>
          Suppliers · {view.summaries.length} active · {components.size} purchased components
        </Kicker>
        <div style={{ fontSize: 14 }}>
          <div style={{ display: "grid", gridTemplateColumns: GRID, gap: 8, padding: "8px", borderBottom: "1px solid var(--color-text)", fontSize: 11.5, letterSpacing: ".06em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>
            <span>Supplier</span>
            <span>Lead</span>
            <span>Sourcing</span>
            <span>On-time · 90 d</span>
            <span style={{ textAlign: "right" }}>
              {exposureServed ? "Exposed" : "Exposed · not served"}
            </span>
          </div>
          {view.summaries.map((s) => (
            <SupplierRow key={s.supplier} summary={s} detail={view.detailBySupplier[s.supplier]} labels={view.labels} exposureServed={exposureServed} selected={s.supplier === current.supplier} onPick={(supplier) => setPicked({ view, supplier })} />
          ))}
        </div>
      </div>

      <SupplierDetailPanel summary={current} detail={view.detailBySupplier[current.supplier]} labels={view.labels} note={note} subject={subject} selection={selection} />
    </div>
  );
}

function SupplierRow({
  summary,
  detail,
  labels,
  exposureServed,
  selected,
  onPick,
}: {
  summary: SupplierSummary;
  detail: SupplierDetail | undefined;
  labels: SuppliersView["labels"];
  /** Whether any row serves exposure: then this row's omitted exposure is unknown ("not
   *  served"), distinct from a known 0 ("—"). */
  exposureServed: boolean;
  selected: boolean;
  onPick: (supplier: string) => void;
}) {
  const supplies = (detail?.stock ?? []).map((s) => (s.stockOf ? labelOf(s.stockOf, s.label) : s.label)).filter(Boolean);
  const pos = (detail?.openPos ?? []).map((p) => (p.po ? labelOf(p.po, p.label ?? labels[p.po]) : p.label)).filter(Boolean);
  const value = exposureOf(summary);
  const exposed = value !== null && value > 0;
  const exposureText = exposed ? formatMoney(summary.exposure, null) : value === null && exposureServed ? "not served" : "—";
  return (
    <div data-entity={summary.supplier} data-path={summary.onPath === true ? "on" : summary.onPath === false ? "off" : "unserved"}>
      <button
        type="button"
        className="pp-unstyled"
        aria-pressed={selected}
        onClick={() => onPick(summary.supplier)}
        style={{
          display: "grid",
          gridTemplateColumns: GRID,
          gap: 8,
          alignItems: "center",
          width: "100%",
          cursor: "pointer",
          padding: "10px 8px",
          borderBottom: "1px solid var(--color-divider)",
          background: selected ? "var(--color-accent-100)" : "transparent",
        }}
      >
        <span style={{ display: "flex", alignItems: "center", gap: 9, minWidth: 0 }}>
          <span
            role="img"
            aria-label={summary.onPath === true ? "On the point's path" : summary.onPath === false ? "Off the point's path" : "Path not served"}
            style={{ width: 9, height: 9, flex: "none", background: summary.onPath === true ? "var(--color-accent-900)" : "transparent", border: `1px ${summary.onPath === null ? "dashed" : "solid"} ${summary.onPath === true ? "var(--color-accent-900)" : "var(--color-neutral-600)"}` }}
          />
          <span style={{ minWidth: 0 }}>
            <span style={{ display: "block", fontWeight: 500, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{labelOf(summary.supplier, summary.label)}</span>
            <span style={{ display: "block", fontSize: 12, color: "var(--color-neutral-700)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
              {[supplies.join(", "), pos.length ? pos.join(", ") : "no open PO"].filter(Boolean).join(" · ")}
            </span>
          </span>
        </span>
        <span style={{ whiteSpace: "nowrap" }}>
          {summary.leadTimeDays === null ? "—" : `${summary.leadTimeDays}d`}
        </span>
        <span>
          <SourcingChip sourcing={summary.sourcing} />
        </span>
        <span>
          <OtdBar otd={summary.otd} />
        </span>
        <span data-testid="supplier-exposure" style={{ textAlign: "right", whiteSpace: "nowrap", fontWeight: exposed ? 600 : 400, fontSize: exposureText === "not served" ? 12.5 : undefined, color: exposureText === "not served" ? "var(--color-neutral-700)" : undefined }}>
          {exposureText}
        </span>
      </button>
    </div>
  );
}
