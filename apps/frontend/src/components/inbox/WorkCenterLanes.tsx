import type { Bar, Lane } from "../../viewmodel/types.ts";
import { Seg } from "../primitives/Seg.tsx";
import { dayTicks, laneDomain, laneRange, parseLocal } from "../../viewmodel/lanes.ts";
import { entityText } from "../../viewmodel/wording.ts";
import { EntityName } from "../primitives/Named.tsx";
import { useEntityLabelMap } from "../primitives/entityLabels.ts";

export type LaneMode = "before" | "after";

const ROW_H = 20;
const BAR_H = 16;
/** The lane-name column, ticks included. */
const NAME_COL = "120px minmax(0,1fr)";

function LaneRow({ lane, bars, min, span }: { lane: Lane; bars: Bar[]; min: number; span: number }) {
  const rows = Math.max(1, ...bars.map((b) => b.row + 1));
  const cap = lane.capacity;
  const overCapacity = cap !== null && rows > cap;
  const labels = useEntityLabelMap();

  return (
    <div style={{ display: "grid", gridTemplateColumns: NAME_COL, borderTop: "1px solid var(--color-divider)" }}>
      <div style={{ display: "flex", flexDirection: "column", justifyContent: "center", paddingRight: 8, minWidth: 0 }}>
        <div data-testid="lane-name" style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 14.5, minWidth: 0 }}>
          <EntityName id={lane.workCenter} ellipsis />
        </div>
        <div style={{ fontSize: 11, color: "var(--color-neutral-700)" }}>
          {cap === null ? "capacity —" : `capacity ${cap}`}
        </div>
      </div>
      <div style={{ position: "relative", height: rows * ROW_H + 8, background: "var(--color-neutral-100)", overflow: "hidden" }}>
        {overCapacity && cap !== null ? (
          <div style={{ position: "absolute", left: 0, right: 0, top: cap * ROW_H + 4, borderTop: "1px dashed var(--color-accent-700)" }} />
        ) : null}
        {bars.map((b, i) => {
          const s = parseLocal(b.start);
          const e = parseLocal(b.end);
          // Skip a bar with an unparseable date instead of rendering it at NaN% left/width
          // (the domain guard only protects the axis).
          if (!Number.isFinite(s) || !Number.isFinite(e)) return null;
          const left = ((s - min) / span) * 100;
          const width = ((e - s) / span) * 100;
          const over = cap !== null && b.row >= cap && !b.changed;
          const bg = over ? "var(--color-bg)" : b.changed ? "var(--color-accent-700)" : "var(--color-accent-300)";
          const fg = b.changed && !over ? "var(--color-bg)" : "var(--color-text)";
          const border = over ? "1px dashed var(--color-accent-900)" : b.changed ? "1px solid var(--color-accent-800)" : "1px solid var(--color-accent-500)";
          const name = entityText(b.workOrder, labels);
          const full = name === b.workOrder ? name : `${name}\n${b.workOrder}`;
          return (
            <div
              key={`${b.workOrder}-${i}`}
              data-testid="lane-bar"
              title={`${full}${b.changed ? " · moved by plan" : ""}`}
              style={{
                position: "absolute",
                left: `${left}%`,
                width: `${Math.max(width, 2)}%`,
                top: b.row * ROW_H + 5,
                height: BAR_H,
                background: bg,
                border,
                color: fg,
                fontSize: 11,
                fontWeight: 500,
                padding: "0 4px",
                display: "flex",
                alignItems: "center",
                whiteSpace: "nowrap",
                overflow: "hidden",
              }}
            >
              <span style={{ minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{name}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

/**
 * The before/after work-center lanes for an option, headed by the date range of the bars
 * ("Work centers · 24 Jan–7 Feb") over day ticks. A segmented control toggles "As planned"
 * (before) and "With plan X" (after). Bars moved by the plan are flagged (accent-700 fill), an
 * over-capacity bar is flagged (paper fill and dashed border) and the capacity line is drawn.
 * Lane names and bar labels read as the labels the read serves for their ids, ending in an
 * ellipsis with the full label on hover when they do not fit. Renders nothing when the option
 * carries no lanes.
 */
export function WorkCenterLanes({
  lanes,
  mode,
  onModeChange,
  planKey,
}: {
  lanes: Lane[];
  mode: LaneMode;
  onModeChange: (mode: LaneMode) => void;
  planKey: string;
}) {
  if (lanes.length === 0) return null;
  const { min, span, served } = laneDomain(lanes);
  const range = laneRange(lanes);
  const ticks = served ? dayTicks(min, span) : [];

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}>
        <div style={{ fontSize: 11.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>
          Work centers{range ? ` · ${range}` : ""}
        </div>
        <Seg
          ariaLabel="lane view"
          value={mode}
          onChange={onModeChange}
          options={[
            { value: "before", label: "As planned" },
            { value: "after", label: `With plan ${planKey}` },
          ]}
        />
      </div>
      <div className="blueprint" style={{ padding: "0 8px 8px" }}>
        <i className="corner tl" />
        <i className="corner tr" />
        <i className="corner bl" />
        <i className="corner br" />
        {ticks.length > 0 ? (
          <div data-testid="lane-ticks" aria-hidden="true" style={{ display: "grid", gridTemplateColumns: NAME_COL }}>
            <div />
            <div style={{ position: "relative", height: 20 }}>
              {ticks.map((t) => (
                <div
                  key={t.left}
                  style={{ position: "absolute", left: `${t.left}%`, top: 0, bottom: 0, borderLeft: "1px solid var(--color-divider)", paddingLeft: 3, fontSize: 10.5, color: "var(--color-neutral-700)", whiteSpace: "nowrap" }}
                >
                  {t.label}
                </div>
              ))}
            </div>
          </div>
        ) : null}
        {lanes.map((lane) => (
          <LaneRow key={lane.workCenter} lane={lane} bars={mode === "before" ? lane.before : lane.after} min={min} span={span} />
        ))}
      </div>
      <div style={{ display: "flex", flexWrap: "wrap", gap: "6px 16px", fontSize: 12, color: "var(--color-neutral-700)" }}>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 16, height: 9, background: "var(--color-accent-700)" }} /> Moved by plan
        </span>
        <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <span style={{ width: 16, height: 9, background: "var(--color-bg)", border: "1px dashed var(--color-accent-900)" }} /> Over capacity
        </span>
      </div>
    </div>
  );
}
