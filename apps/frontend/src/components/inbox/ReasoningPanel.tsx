import type { Trace, TraceTag } from "../../viewmodel/types.ts";
import { traceSummaryLine } from "../../viewmodel/wording.ts";
import { Named } from "../primitives/Named.tsx";

const TAG_STYLE: Record<TraceTag, React.CSSProperties> = {
  Simulated: { background: "var(--color-accent-700)", color: "var(--color-bg)", border: "1px solid var(--color-accent-700)" },
  Graph: { background: "transparent", color: "var(--color-accent-800)", border: "1px solid var(--color-accent-500)" },
  Assumption: { background: "transparent", color: "var(--color-accent-800)", border: "1px dashed var(--color-accent-700)" },
  // Neutral, not accent: a failed model attempt produced no candidate to reason about.
  Generation: { background: "transparent", color: "var(--color-neutral-800)", border: "1px dashed var(--color-neutral-700)" },
};


/**
 * The structured reasoning trace: Simulated / Graph / Assumption / Generation steps with the
 * summary line ("5 plans simulated · 2 rejected · 1 open assumption", saying "no open
 * assumptions" rather than dropping a zero). Rejected candidates are kept and greyed rather
 * than hidden, so the operator can see what was ruled out. A Generation step, a model
 * attempt that produced no candidate, is shown set aside with its failure class and reason, so
 * the operator sees why a plan has fewer options than attempts. Counts come from the served
 * summary, so they cannot disagree with the steps. Every id a step carries is shown by its
 * label, with the id on hover. `title` names whose reasoning it is (the plan's, or the
 * selected option's own part of it); `legend` explains the step tags once when two panels are
 * shown together.
 */
export function ReasoningPanel({
  trace,
  title = "How the agent got here",
  legend = true,
}: {
  trace: Trace;
  title?: string;
  legend?: boolean;
}) {
  return (
    <div className="blueprint" style={{ padding: "14px 18px", display: "flex", flexDirection: "column", gap: 10 }}>
      <i className="corner tl" />
      <i className="corner tr" />
      <i className="corner bl" />
      <i className="corner br" />
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline", flexWrap: "wrap", gap: 8 }}>
        <span style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 18 }}>{title}</span>
        <span style={{ fontSize: 12.5, color: "var(--color-neutral-700)" }}>{traceSummaryLine(trace.summary)}</span>
      </div>
      {trace.steps.map((s, i) => (
        <div
          key={i}
          style={{ display: "grid", gridTemplateColumns: "18px 110px minmax(0,1fr)", gap: 10, alignItems: "baseline", fontSize: 13.5, lineHeight: 1.45 }}
        >
          <span style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>{i + 1}</span>
          <span
            style={{
              justifySelf: "start",
              padding: "1px 7px",
              fontSize: 11,
              letterSpacing: ".05em",
              textTransform: "uppercase",
              ...TAG_STYLE[s.tag],
            }}
          >
            {s.tag}
          </span>
          <span style={{ color: s.rejected ? "var(--color-neutral-700)" : "var(--color-text)" }}>
            {s.rejected ? <b style={{ fontWeight: 600 }}>{s.tag === "Generation" ? "Set aside. " : "Rejected. "}</b> : null}
            <Named text={s.text} />
          </span>
        </div>
      ))}
      {trace.steps.length === 0 ? (
        <div style={{ fontSize: 13, color: "var(--color-neutral-700)" }}>No reasoning steps served.</div>
      ) : null}
      {legend ? (
        <div style={{ fontSize: 12, color: "var(--color-neutral-700)", borderTop: "1px solid var(--color-divider)", paddingTop: 8 }}>
          Simulated: checked by the simulator. Graph: read from the world model. Assumption: not verified yet.
          Generation: a model attempt that produced no candidate.
        </div>
      ) : null}
    </div>
  );
}
