import type { TabId } from "../data/hooks.ts";
import type { EntitySelection } from "../viewmodel/types.ts";
import { useReadModel } from "../data/hooks.ts";
import { TimelineTab } from "../components/timeline/TimelineTab.tsx";
import { GraphTab } from "../components/graph/GraphTab.tsx";
import { FlowTab } from "../components/flow/FlowTab.tsx";
import { SuppliersTab } from "../components/suppliers/SuppliersTab.tsx";
import { systemClock, useNow, type Clock } from "../viewmodel/clock.ts";

/** How often the Timeline's "today" re-reads the clock. */
const TODAY_INTERVAL_MS = 60_000;

/**
 * The live impact-tab container: fetches `readModel(subject, tab)` via the hook, maps its
 * opaque rows through the per-tab view-model, and renders the matching tab component. It
 * only reads; switching tab or subject re-fetches. `selection` lets a tab entity that is a
 * point a listed plan resolves select that plan. The Timeline's "today" comes from the
 * injectable clock (the system clock unless one is given).
 */
export function ImpactTab({ subject, tab, selection, clock = systemClock }: { subject: string; tab: TabId; selection: EntitySelection; clock?: Clock }) {
  const { status, view, error } = useReadModel(subject, tab);
  const today = new Date(useNow(TODAY_INTERVAL_MS, clock)).toISOString();

  if (status === "error") {
    return (
      <p role="alert" className="text-muted" style={{ fontSize: 13 }}>
        Could not load this tab: {error}
      </p>
    );
  }
  if (view === null) {
    // A status region, so a re-projection after a tab-entity selection is announced.
    return (
      <p role="status" className="text-muted" style={{ fontSize: 13 }}>
        Loading…
      </p>
    );
  }

  switch (view.tab) {
    case "timeline":
      return <TimelineTab view={view.view} subject={subject} today={today} selection={selection} />;
    case "graph":
      return <GraphTab view={view.view} subject={subject} selection={selection} />;
    case "flow":
      return <FlowTab view={view.view} subject={subject} selection={selection} />;
    case "suppliers":
      return <SuppliersTab view={view.view} subject={subject} selection={selection} />;
  }
}
