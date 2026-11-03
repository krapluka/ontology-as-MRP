import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, ReactNode, RefObject } from "react";
import type { AppliedChange, ApprovedPlan, EntitySelection, InboxItem, InboxKpis, Option, Plan, PlanEntry, Policy } from "../viewmodel/types.ts";
import { itemId, itemLead } from "../viewmodel/types.ts";
import type { TabId } from "../data/hooks.ts";
import { PlanRow } from "../components/inbox/PlanRow.tsx";
import { ApprovedPlanRow } from "../components/inbox/ApprovedPlanRow.tsx";
import { OptionCard } from "../components/inbox/OptionCard.tsx";
import { ChangesList } from "../components/inbox/ChangesList.tsx";
import { NeedsChips } from "../components/inbox/NeedsChips.tsx";
import { WorkCenterLanes, type LaneMode } from "../components/inbox/WorkCenterLanes.tsx";
import { ReasoningPanel } from "../components/inbox/ReasoningPanel.tsx";
import { AutoAppliedRow } from "../components/inbox/AutoAppliedRow.tsx";
import { PolicyNote } from "../components/inbox/PolicyNote.tsx";
import { PolicyRules } from "../components/inbox/PolicyRules.tsx";
import { ReasonDialog } from "../components/inbox/ReasonDialog.tsx";
import { REVIEWER_HINT, ReviewerField } from "../components/inbox/ReviewerField.tsx";
import { Btn } from "../components/primitives/Btn.tsx";
import {
  formatDate,
  formatLocalDay,
  formatMoney,
  isToday,
  severityMeta,
  shortId,
  slipLabel,
  undoOpen,
  undoStatus,
} from "../viewmodel/format.ts";
import {
  appliedLabel,
  clockTime,
  collectIds,
  headerClock,
  planLabel,
  planOptionTitle,
  pointLabel,
  scopeLine,
  waitingText,
} from "../viewmodel/wording.ts";
import { EntityName, Named } from "../components/primitives/Named.tsx";
import { EntityLabels } from "../components/primitives/entityLabels.ts";
import { safeStorage } from "../viewmodel/safeStorage.ts";
import { systemClock, useNow, type Clock } from "../viewmodel/clock.ts";

/** The header tabs. "Risk inbox" is the hero; the other four are the impact tabs driven by
 *  the selected point's subject via `readModel`. Their ids are the stored `pp-view` values. */
const TABS = [
  { id: "inbox", label: "Risk inbox" },
  { id: "timeline", label: "Timeline" },
  { id: "graph", label: "Graph" },
  { id: "flow", label: "Flow" },
  { id: "suppliers", label: "Suppliers" },
] as const;

type ViewId = (typeof TABS)[number]["id"];

const LS_ITEM = "pp-item";
const LS_REASON = "pp-reason";
const LS_VIEW = "pp-view";
const LS_REVIEWER = "pp-reviewer";

/** How often the screen re-reads the clock: the undo window and "today" follow it. */
const NOW_INTERVAL_MS = 30_000;

function isViewId(value: string | null): value is ViewId {
  return value !== null && TABS.some((t) => t.id === value);
}

export type Notice = { kind: "error" | "info"; message: string };

/** What a command callback reports back: `ok` only when the command succeeded. */
export type CommandOutcome = { ok: boolean };

export type RiskInboxProps = {
  /** Every plan awaiting oversight, one typed row each (plan / no option / pending /
   *  unavailable), keyed by its plan id. */
  items: InboxItem[];
  autoApplied: AppliedChange[];
  /** The plans the operator approved today, plus earlier ones still undoable. */
  approved?: ApprovedPlan[];
  /** The active auto-apply policy, or null when the read surface serves none. */
  policy: Policy | null;
  kpis: InboxKpis;
  /** Approve: promote `option` of the plan `entry` with the rationale typed for that plan, as
   *  `reviewer`. One command for the plan (the portfolio promotion for a joint plan). The
   *  rationale is cleared only when this resolves `ok`. */
  onApprove: (entry: PlanEntry, option: Option, rationale: string, reviewer: string) => Promise<CommandOutcome>;
  /** Dismiss the plan `entry` with the operator's confirmed reason. */
  onDismiss: (entry: PlanEntry, reason: string, reviewer: string) => Promise<CommandOutcome>;
  /** Undo an auto-applied change with the operator's confirmed reason. */
  onUndo: (applied: AppliedChange, reason: string, reviewer: string) => Promise<CommandOutcome>;
  /** Undo a plan the operator approved, with their confirmed reason. */
  onUndoPlan?: (approved: ApprovedPlan, reason: string, reviewer: string) => Promise<CommandOutcome>;
  /** Plan ids (and auto-applied subjects) with a command in flight: their Approve / Dismiss /
   *  Undo are disabled (busy) until it settles, so a plan does not get two commands at once. */
  inFlight?: ReadonlySet<string>;
  /** A command's outcome or refusal reason. */
  notice?: Notice | null;
  /** The last refresh failed: the screen keeps the data it shows and says it may be stale. */
  refreshError?: string | null;
  /** The reviewer identity to start with when this browser has none stored (the build-time
   *  `VITE_EKG_REVIEWER`). The operator can change it; it is then remembered per browser. */
  defaultReviewer?: string | undefined;
  /** The time source for the undo window and "today" (the system clock unless injected). */
  clock?: Clock;
  /** Render one impact tab for the selected plan's point: its lead point, or the member the
   *  operator picked on a tab. Injected so the screen stays presentational: the live container
   *  wires it to the `readModel` hook, while stories wire it to fixtures. When omitted, the
   *  impact tabs show a "select a plan" hint. The third argument lets a tab select a plan from
   *  one of its entities: `selectable` holds every point the listed plans resolve, `onSelect`
   *  selects the plan that resolves one. Selecting re-projects every tab and commits nothing. */
  renderImpactTab?: (tab: TabId, subject: string, selection: EntitySelection) => ReactNode;
  /** The display labels the read serves for the entity ids on screen; an id without one shows
   *  its short form. Injected so the screen stays presentational. */
  labels?: ReadonlyMap<string, string>;
  /** Told the sorted, de-duplicated entity ids the screen shows whenever that set changes, so
   *  the container can read their labels. */
  onShownIds?: (ids: string[]) => void;
};

function recommendedOption(plan: Plan): Option | null {
  return plan.options.find((o) => o.recommended) ?? plan.options[0] ?? null;
}

/** The rationale being typed, bound to the plan it was typed for: it is shown and sent only
 *  while that plan (by its id) is the one on screen. */
type Rationale = { id: string; text: string };
const NO_RATIONALE: Rationale = { id: "", text: "" };

/** The confirmation a Dismiss or Undo is waiting on (it needs the operator's reason). */
type Confirming =
  | { kind: "dismiss"; entry: PlanEntry }
  | { kind: "undo"; applied: AppliedChange }
  | { kind: "undo_plan"; approved: ApprovedPlan };

const NO_APPROVED: ApprovedPlan[] = [];
const NO_LABELS: ReadonlyMap<string, string> = new Map();

/** The ids a plan's details show: its lead, its trace, and every option's changes (object,
 *  from, params), lanes (work centers, work orders) and its part of the trace. */
function planIds(plan: Plan): string[] {
  const traceIds = (steps: Plan["trace"]["steps"]) => steps.flatMap((step) => collectIds(step.text));
  return [
    plan.lead,
    ...traceIds(plan.trace.steps),
    ...plan.options.flatMap((o) => [
      ...changeIds(o.changes),
      ...o.lanes.flatMap((lane) => [lane.workCenter, ...[...lane.before, ...lane.after].flatMap((bar) => collectIds(bar.workOrder))]),
      ...traceIds(o.trace.steps),
    ]),
  ];
}

/** The ids a list of changes shows: each object, and the ids in its from and params. */
function changeIds(changes: ReadonlyArray<Option["changes"][number]>): string[] {
  return changes.flatMap((c) => [c.object, ...collectIds(c.from ?? ""), ...c.params.flatMap((p) => collectIds(p.value))]);
}

/** The Risk inbox screen with the served entity labels in scope for every part it renders. */
export function RiskInbox(props: RiskInboxProps) {
  return (
    <EntityLabels.Provider value={props.labels ?? NO_LABELS}>
      <RiskInboxScreen {...props} />
    </EntityLabels.Provider>
  );
}

/**
 * The Risk inbox screen. Composes the presentational components with local screen state only:
 * the selected item, the per-plan selected option (by `rankKey`), the lane view, the reasoning
 * toggle, the rationale (bound to its plan) and the reviewer identity, plus per-browser
 * storage (`safeStorage`) for the last selection and the reviewer. Viewing, selecting and
 * toggling mutate nothing. State changes go through the injected commands (onApprove /
 * onDismiss / onUndo), each carrying the operator-entered reviewer; none can be issued while
 * it is empty.
 */
function RiskInboxScreen({
  items,
  autoApplied,
  approved = NO_APPROVED,
  policy,
  kpis,
  onApprove,
  onDismiss,
  onUndo,
  onUndoPlan,
  inFlight,
  notice,
  refreshError,
  defaultReviewer,
  clock = systemClock,
  renderImpactTab,
  onShownIds,
}: RiskInboxProps) {
  const now = useNow(NOW_INTERVAL_MS, clock);
  // Listed: every change applied today, plus any earlier change that can still be undone.
  // The KPI counts today's only.
  const listedAuto = useMemo(() => autoApplied.filter((a) => isToday(a.appliedAt, now) || undoOpen(a, now)), [autoApplied, now]);
  const appliedToday = autoApplied.filter((a) => isToday(a.appliedAt, now)).length;

  const firstItem = items[0] ? itemId(items[0]) : (listedAuto[0]?.subject ?? "");
  const [item, setItem] = useState<string>(() => safeStorage.get(LS_ITEM) ?? firstItem);
  const [view, setView] = useState<ViewId>(() => {
    const stored = safeStorage.get(LS_VIEW);
    return isViewId(stored) ? stored : "inbox";
  });
  const [optByPlan, setOptByPlan] = useState<Record<string, string>>({});
  const [lane, setLane] = useState<LaneMode>("after");
  const [showReason, setShowReason] = useState<boolean>(() => safeStorage.get(LS_REASON) === "1");
  const [rationale, setRationale] = useState<Rationale>(NO_RATIONALE);
  const [reviewer, setReviewer] = useState<string>(() => safeStorage.get(LS_REVIEWER) ?? defaultReviewer ?? "");
  const [confirming, setConfirming] = useState<Confirming | null>(null);
  // The member a tab selection picked: the tabs show it while its plan is selected;
  // otherwise they follow the selected plan's lead point.
  const [tabPick, setTabPick] = useState<string | null>(null);

  useEffect(() => safeStorage.set(LS_ITEM, item), [item]);
  useEffect(() => safeStorage.set(LS_VIEW, view), [view]);
  useEffect(() => safeStorage.set(LS_REASON, showReason ? "1" : "0"), [showReason]);
  // The impact panel stays mounted across subject switches, so it can hold focus.
  const panelRef = useRef<HTMLElement>(null);
  const [announcement, setAnnouncement] = useState("");
  // Every point a listed plan resolves, to the plan that resolves it and its display id.
  const members = useMemo(() => {
    const byPoint = new Map<string, { id: string; label: string }>();
    for (const i of items) for (const p of i.entry.resolves) byPoint.set(p.subject, { id: itemId(i), label: pointLabel(p.code) });
    return byPoint;
  }, [items]);
  // A tab entity that is a point a listed plan resolves selects that plan and shows that point
  // on the tabs. It is the same local selection a plan row makes: every tab re-projects,
  // nothing is committed. The clicked entity's button is replaced by a plain box once it is
  // the tabs' subject, so focus moves to the impact panel first (otherwise it would drop to
  // the page), and the selection is announced. "Review options" opens that plan in the inbox.
  const selection = useMemo<EntitySelection>(
    () => ({
      selectable: new Set(members.keys()),
      onSelect: (subject: string) => {
        const plan = members.get(subject);
        if (!plan) return;
        panelRef.current?.focus();
        setItem(plan.id);
        setTabPick(subject);
        setAnnouncement(`Selected ${shortId(subject)}`);
      },
      pointLabel: (subject: string) => members.get(subject)?.label ?? null,
      onReview: (subject: string) => {
        const plan = members.get(subject);
        if (!plan) return;
        setItem(plan.id);
        setTabPick(subject);
        setView("inbox");
      },
    }),
    [members],
  );

  // What the detail column shows: the selected auto-applied change, else the selected row,
  // else the first row (or the first listed change). An item that stops resolving after a
  // refresh falls back, and the selection is moved to what is shown, so the highlighted row,
  // the panel and every piece of per-plan state agree.
  const selectedAuto = listedAuto.find((a) => a.subject === item) ?? null;
  const detailItem = selectedAuto ? null : (items.find((i) => itemId(i) === item) ?? items[0] ?? null);
  const shownAuto = selectedAuto ?? (detailItem ? null : (listedAuto[0] ?? null));
  const shownId = detailItem ? itemId(detailItem) : (shownAuto?.subject ?? "");
  useEffect(() => {
    if (shownId !== item) setItem(shownId);
  }, [shownId, item]);
  // The tabs follow the shown plan's lead point, or the member picked on a tab while it is
  // one of the shown plan's points; an auto-applied change drives them by its subject.
  const tabSubject = detailItem
    ? tabPick !== null && detailItem.entry.resolves.some((p) => p.subject === tabPick)
      ? tabPick
      : itemLead(detailItem)
    : (shownAuto?.subject ?? "");

  const detailPlan = detailItem?.kind === "plan" ? detailItem : null;
  // The option is remembered by its rankKey (not its position letter), per plan id. A
  // remembered key missing from the refreshed slate is reported as lost, not replaced.
  const storedKey = detailPlan ? optByPlan[detailPlan.entry.id] : undefined;
  const option = detailPlan
    ? storedKey !== undefined
      ? (detailPlan.plan.options.find((o) => o.rankKey === storedKey) ?? null)
      : recommendedOption(detailPlan.plan)
    : null;
  const selectionLost = detailPlan !== null && storedKey !== undefined && option === null;
  const rationaleText = rationale.id === shownId ? rationale.text : "";

  // The entity ids on screen: every listed plan's situation and lead, the shown plan's
  // details, and the approved and auto-applied changes. The container is told the set whenever
  // it changes, and serves their labels back.
  const shownIds = useMemo(() => {
    const ids = [
      ...items.flatMap((i) => [...collectIds(i.entry.situation), ...(i.kind === "plan" || i.kind === "no_option" ? [i.plan.lead] : [])]),
      ...(detailItem && (detailItem.kind === "plan" || detailItem.kind === "no_option") ? planIds(detailItem.plan) : []),
      ...approved.flatMap((a) => changeIds(a.changes)),
      ...listedAuto.flatMap((a) => changeIds(a.changes)),
    ];
    return [...new Set(ids)].sort();
  }, [items, detailItem, approved, listedAuto]);
  const shownKey = shownIds.join("\n");
  const onShownIdsRef = useRef(onShownIds);
  onShownIdsRef.current = onShownIds;
  useEffect(() => {
    onShownIdsRef.current?.(shownKey.length > 0 ? shownKey.split("\n") : []);
  }, [shownKey]);

  const reviewerId = reviewer.trim();
  const canCommand = reviewerId.length > 0;
  const busy = (id: string) => inFlight?.has(id) ?? false;

  async function approve(entry: PlanEntry, chosen: Option) {
    const sent = rationaleText;
    const outcome = await onApprove(entry, chosen, sent.trim(), reviewerId);
    // Cleared only on success: a refused or failed approval keeps what the operator typed.
    if (outcome?.ok) setRationale((cur) => (cur.id === entry.id && cur.text === sent ? NO_RATIONALE : cur));
  }

  function confirm(reason: string) {
    const pending = confirming;
    setConfirming(null);
    if (pending?.kind === "dismiss") void onDismiss(pending.entry, reason, reviewerId);
    else if (pending?.kind === "undo") void onUndo(pending.applied, reason, reviewerId);
    else if (pending?.kind === "undo_plan") void onUndoPlan?.(pending.approved, reason, reviewerId);
  }

  const dismissFor = (entry: PlanEntry) => () => setConfirming({ kind: "dismiss", entry });

  return (
    <div style={{ display: "flex", flexDirection: "column", minHeight: "100vh" }}>
      <header
        style={{
          minHeight: 58,
          display: "flex",
          alignItems: "center",
          flexWrap: "wrap",
          gap: 24,
          padding: "0 clamp(18px,2.2vw,36px)",
          borderBottom: "1px solid var(--color-divider)",
        }}
      >
        <span style={{ display: "flex", alignItems: "center", gap: 8, marginRight: 8 }}>
          <span style={{ width: 22, height: 22, border: "1px solid var(--color-text)", position: "relative" }}>
            <span style={{ position: "absolute", right: -3, bottom: -3, width: 8, height: 8, background: "var(--color-accent)" }} />
          </span>
          <span style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: 19, letterSpacing: ".1em", textTransform: "uppercase" }}>
            Plant Pulse
          </span>
        </span>
        <nav style={{ display: "flex", gap: 18 }}>
          {TABS.map((t) => {
            const active = t.id === view;
            return (
              <button
                key={t.id}
                type="button"
                className="pp-tab"
                aria-current={active ? "page" : undefined}
                onClick={() => setView(t.id)}
                style={{
                  cursor: "pointer",
                  fontFamily: "var(--font-heading)",
                  fontWeight: 600,
                  fontSize: 15.5,
                  paddingBottom: 4,
                  borderBottom: active ? "2px solid var(--color-accent)" : "2px solid transparent",
                  color: active ? "var(--color-text)" : "var(--color-neutral-700)",
                }}
              >
                {t.label}
              </button>
            );
          })}
        </nav>
        <div style={{ marginLeft: "auto", display: "flex", alignItems: "center", gap: 18 }}>
          <span data-testid="header-clock" style={{ fontSize: 13, color: "var(--color-neutral-700)", whiteSpace: "nowrap" }}>
            {headerClock(now)}
          </span>
          <ReviewerField
            value={reviewer}
            onChange={(value) => {
              setReviewer(value);
              safeStorage.set(LS_REVIEWER, value);
            }}
          />
        </div>
      </header>

      <main style={{ padding: "clamp(12px,1.8vh,24px) clamp(18px,2.2vw,36px) 60px" }}>
        {/* Always mounted, so a change of its text is announced (a polite live region). */}
        <div data-testid="selection-announcement" aria-live="polite" aria-atomic="true" style={VISUALLY_HIDDEN}>
          {announcement}
        </div>
        {refreshError ? (
          <div
            role="status"
            style={{ marginBottom: 16, padding: "8px 12px", fontSize: 13, border: "1px dashed var(--color-accent-900)", background: "var(--color-neutral-100)" }}
          >
            Could not refresh the inbox ({refreshError}). Showing the last data read — it may be stale.
          </div>
        ) : null}
        {notice ? (
          <div
            role="alert"
            style={{
              marginBottom: 16,
              padding: "8px 12px",
              fontSize: 13,
              border: `1px solid ${notice.kind === "error" ? "var(--color-accent-900)" : "var(--color-accent-500)"}`,
              background: "var(--color-accent-100)",
            }}
          >
            {notice.message}
          </div>
        ) : null}

        {view === "inbox" ? (
          <>
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-end", gap: 20, flexWrap: "wrap", marginBottom: 20 }}>
              <div>
                <div style={{ fontSize: 12, letterSpacing: ".12em", textTransform: "uppercase", color: "var(--color-accent-700)" }}>
                  Agent
                </div>
                <h1 style={{ fontSize: "clamp(22px,2vw,30px)", margin: 0 }}>
                  {kpis.waiting} {kpis.waiting === 1 ? "plan needs" : "plans need"} your approval
                </h1>
              </div>
              <div style={{ display: "flex", gap: 28, flexWrap: "wrap" }}>
                <Kpi value={String(kpis.waiting)} label="Plans waiting for you" />
                <Kpi value={String(appliedToday)} label="Applied by agent today" />
                {kpis.revenueAtRisk !== null ? (
                  <Kpi value={formatMoney(kpis.revenueAtRisk, kpis.currency, { compact: true }) ?? kpis.revenueAtRisk} label="Revenue at risk" />
                ) : null}
                <LateOrdersKpi now={kpis.lateOrdersNow} ifApproved={kpis.lateOrders} />
              </div>
            </div>

            <div style={{ display: "grid", gridTemplateColumns: "minmax(300px,0.8fr) minmax(0,2fr)", gap: "clamp(20px,2.4vw,36px)" }}>
              <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
                <Section label="Needs your approval" count={items.length}>
                  {items.length === 0 ? (
                    <Empty>No plans are waiting for you.</Empty>
                  ) : (
                    items.map((i) => (
                      <PlanRow
                        key={itemId(i)}
                        item={i}
                        now={now}
                        selected={detailItem !== null && itemId(i) === shownId}
                        onSelect={(id) => {
                          setItem(id);
                          setTabPick(null);
                        }}
                      />
                    ))
                  )}
                </Section>
                {approved.length > 0 ? (
                  <Section label="Approved by you today" count={approved.length}>
                    {approved.map((a) => (
                      <ApprovedPlanRow
                        key={a.id}
                        approved={a}
                        now={now}
                        onUndo={(plan) => setConfirming({ kind: "undo_plan", approved: plan })}
                        busy={busy(a.id)}
                        disabled={!canCommand || onUndoPlan === undefined}
                      />
                    ))}
                  </Section>
                ) : null}
                <Section label="Applied by agent · today and still undoable" count={listedAuto.length}>
                  {listedAuto.length === 0 ? (
                    <Empty>Nothing was auto-applied today, and nothing earlier can still be undone.</Empty>
                  ) : (
                    listedAuto.map((a) => (
                      <AutoAppliedRow
                        key={a.subject}
                        applied={a}
                        now={now}
                        selected={shownAuto !== null && a.subject === shownId}
                        onSelect={setItem}
                        onUndo={(applied) => setConfirming({ kind: "undo", applied })}
                        busy={busy(a.subject)}
                        disabled={!canCommand}
                      />
                    ))
                  )}
                </Section>
                <PolicyNote policy={policy} />
              </div>

              <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
                {detailPlan ? (
                  <PlanDetail
                    item={detailPlan}
                    policy={policy}
                    option={option}
                    selectionLost={selectionLost}
                    onPickOption={(rankKey) => setOptByPlan((s) => ({ ...s, [detailPlan.entry.id]: rankKey }))}
                    lane={lane}
                    onLane={setLane}
                    showReason={showReason}
                    onToggleReason={() => setShowReason((v) => !v)}
                    rationale={rationaleText}
                    onRationale={(text) => setRationale({ id: detailPlan.entry.id, text })}
                    onApprove={(chosen) => void approve(detailPlan.entry, chosen)}
                    onDismiss={dismissFor(detailPlan.entry)}
                    busy={busy(detailPlan.entry.id)}
                    canCommand={canCommand}
                  />
                ) : detailItem?.kind === "no_option" ? (
                  <NoOptionDetail
                    entry={detailItem.entry}
                    plan={detailItem.plan}
                    onDismiss={dismissFor(detailItem.entry)}
                    busy={busy(detailItem.entry.id)}
                    canCommand={canCommand}
                  />
                ) : detailItem?.kind === "pending" || detailItem?.kind === "unavailable" ? (
                  <PointDetail item={detailItem} onDismiss={dismissFor(detailItem.entry)} busy={busy(detailItem.entry.id)} canCommand={canCommand} />
                ) : shownAuto ? (
                  <AutoDetail
                    applied={shownAuto}
                    now={now}
                    showReason={showReason}
                    onToggleReason={() => setShowReason((v) => !v)}
                    onUndo={(applied) => setConfirming({ kind: "undo", applied })}
                    busy={busy(shownAuto.subject)}
                    canCommand={canCommand}
                  />
                ) : (
                  <Empty>Select a plan to review its options.</Empty>
                )}
              </div>
            </div>
          </>
        ) : (
          <ImpactTabPanel tab={view} subject={tabSubject} selection={selection} renderImpactTab={renderImpactTab} panelRef={panelRef} />
        )}
      </main>

      {confirming?.kind === "dismiss" ? (
        <ReasonDialog
          title={`Dismiss plan ${planLabel(confirming.entry.code)}?`}
          description={`${dismissedPoints(confirming.entry)} aborted; nothing is written to Odoo.`}
          confirmLabel="Dismiss plan"
          onConfirm={confirm}
          onCancel={() => setConfirming(null)}
        />
      ) : confirming?.kind === "undo_plan" ? (
        <ReasonDialog
          title={`Undo plan ${planLabel(confirming.approved.code)}?`}
          description={`Its ${confirming.approved.changes.length === 1 ? "change is" : `${confirming.approved.changes.length} changes are`} reversed in Odoo through the recorded inverse, as one governed step.`}
          confirmLabel="Undo plan"
          onConfirm={confirm}
          onCancel={() => setConfirming(null)}
        />
      ) : confirming?.kind === "undo" ? (
        <ReasonDialog
          title={`Undo ${shortId(confirming.applied.subject)}?`}
          description="The auto-applied change is reversed in the source system through its recorded inverse."
          confirmLabel="Undo change"
          onConfirm={confirm}
          onCancel={() => setConfirming(null)}
        />
      ) : null}
    </div>
  );
}

/** "Its point is" / "Its 2 points are": what a dismissal aborts. */
function dismissedPoints(entry: PlanEntry): string {
  const n = entry.resolves.length;
  return n === 1 ? "Its point is" : `Its ${n} points are`;
}

/** Hidden from sight but read by assistive technology (the live-region announcement). */
const VISUALLY_HIDDEN: CSSProperties = {
  position: "absolute",
  width: 1,
  height: 1,
  margin: -1,
  padding: 0,
  overflow: "hidden",
  clip: "rect(0 0 0 0)",
  whiteSpace: "nowrap",
  border: 0,
};

/** The impact-tab panel: renders the injected tab content for the selected point's subject.
 *  With no subject selected (empty inbox), it prompts the operator to pick a plan first,
 *  because the impact tabs are driven by the selected point. It is a labelled region that
 *  stays mounted across subject switches and takes programmatic focus (`tabIndex=-1`), so a
 *  selection made from a tab entity keeps keyboard focus inside the panel. */
function ImpactTabPanel({
  tab,
  subject,
  selection,
  renderImpactTab,
  panelRef,
}: {
  tab: TabId;
  subject: string;
  selection: EntitySelection;
  renderImpactTab: ((tab: TabId, subject: string, selection: EntitySelection) => ReactNode) | undefined;
  panelRef: RefObject<HTMLElement | null>;
}) {
  const label = subject.length === 0 ? "Impact of the selected plan" : `Impact of ${shortId(subject)}`;
  return (
    <section ref={panelRef} tabIndex={-1} aria-label={label} style={{ outlineOffset: 4 }}>
      {subject.length === 0 ? (
        <Empty>Select a plan in the Risk inbox to drive this tab.</Empty>
      ) : !renderImpactTab ? (
        <Empty>This tab is not wired to a read surface here.</Empty>
      ) : (
        renderImpactTab(tab, subject, selection)
      )}
    </section>
  );
}

function Kpi({ value, label }: { value: string; label: string }) {
  return (
    <div>
      <div style={{ fontFamily: "var(--font-heading)", fontWeight: 600, fontSize: "clamp(20px,1.6vw,26px)" }}>{value}</div>
      <div style={{ fontSize: 12.5, color: "var(--color-neutral-700)" }}>{label}</div>
    </div>
  );
}

/** The late-orders KPI, "late now → late if approved", from the two served counts. A lone
 *  served count renders under its own label; neither served renders nothing. */
function LateOrdersKpi({ now, ifApproved }: { now: string | null; ifApproved: string | null }) {
  if (ifApproved !== null) {
    return <Kpi value={now !== null ? `${now} → ${ifApproved}` : ifApproved} label="Late orders if approved" />;
  }
  return now !== null ? <Kpi value={now} label="Late orders now" /> : null;
}

function Section({ label, count, children }: { label: string; count: number; children: React.ReactNode }) {
  return (
    <section style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      <div style={{ display: "flex", justifyContent: "space-between", fontSize: 12, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>
        <span>{label}</span>
        <span>{count}</span>
      </div>
      {children}
    </section>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div style={{ fontSize: 13, color: "var(--color-neutral-700)", padding: "12px 0" }}>{children}</div>;
}

function ReasonToggle({ on, onToggle }: { on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      aria-pressed={on}
      onClick={onToggle}
      className="pp-unstyled"
      style={{ cursor: "pointer", display: "flex", alignItems: "center", gap: 8, fontSize: 13, color: "var(--color-neutral-700)" }}
    >
      <span>Show reasoning</span>
      <span style={{ position: "relative", width: 30, height: 16, border: `1px solid ${on ? "var(--color-accent-700)" : "var(--color-neutral-600)"}`, background: on ? "var(--color-accent-700)" : "transparent" }}>
        <span style={{ position: "absolute", top: 2, left: on ? 16 : 2, width: 10, height: 10, background: on ? "var(--color-bg)" : "var(--color-neutral-600)" }} />
      </span>
    </button>
  );
}

/** The action row's hint while no reviewer identity is entered (all commands disabled). */
function ReviewerHint({ canCommand }: { canCommand: boolean }) {
  return canCommand ? null : <span style={{ fontSize: 12.5, color: "var(--color-accent-800)" }}>{REVIEWER_HINT}</span>;
}

function DetailHeader({
  entry,
  plan,
  scope,
  title,
  showReason,
  onToggleReason,
}: {
  entry: PlanEntry;
  plan: Plan | null;
  scope: string | null;
  title: string;
  showReason?: boolean;
  onToggleReason?: () => void;
}) {
  return (
    <>
      <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
        {plan ? <SeverityChip severity={plan.severity} slip={plan.daySlip} delivery={plan.delivery} /> : null}
        <span style={{ fontSize: 13, color: "var(--color-neutral-700)" }} title={entry.id}>
          {[planLabel(entry.code), scope].filter(Boolean).join(" · ")}
        </span>
        {onToggleReason ? (
          <div style={{ marginLeft: "auto" }}>
            <ReasonToggle on={showReason ?? false} onToggle={onToggleReason} />
          </div>
        ) : null}
      </div>
      <h2 style={{ fontSize: "clamp(20px,1.8vw,27px)", margin: 0, overflowWrap: "anywhere" }}>
        <Named text={title} />
      </h2>
      <p data-testid="situation" style={{ fontSize: 13.5, lineHeight: 1.45, margin: 0, maxWidth: 880, overflowWrap: "anywhere" }}>
        <Named text={entry.situation} />
      </p>
      {plan ? <BaselineLine plan={plan} /> : null}
    </>
  );
}

function PlanDetail({
  item,
  policy,
  option,
  selectionLost,
  onPickOption,
  lane,
  onLane,
  showReason,
  onToggleReason,
  rationale,
  onRationale,
  onApprove,
  onDismiss,
  busy,
  canCommand,
}: {
  item: Extract<InboxItem, { kind: "plan" }>;
  policy: Policy | null;
  /** The selected option, or null when the remembered one left the slate. */
  option: Option | null;
  selectionLost: boolean;
  onPickOption: (rankKey: string) => void;
  lane: LaneMode;
  onLane: (m: LaneMode) => void;
  showReason: boolean;
  onToggleReason: () => void;
  rationale: string;
  onRationale: (v: string) => void;
  onApprove: (option: Option) => void;
  onDismiss: () => void;
  busy: boolean;
  canCommand: boolean;
}) {
  const { entry, plan } = item;
  // "each reversible" only when every change carries its recorded inverse (served data).
  const reversible = option !== null && option.changes.length > 0 && option.changes.every((c) => c.inverse !== null);
  // Approve waits while a point is not `proposed` (the promotion claims every point from it).
  const waiting = waitingText(entry);
  const title = recommendedOption(plan)?.label ?? entry.situation;
  return (
    <>
      <DetailHeader entry={entry} plan={plan} scope={option ? scopeLine(option.scope) : null} title={title} showReason={showReason} onToggleReason={onToggleReason} />

      <div style={{ display: "flex", flexDirection: "column", gap: 7 }}>
        {selectionLost ? (
          <div role="status" style={{ fontSize: 13, padding: "6px 10px", border: "1px solid var(--color-accent-900)", background: "var(--color-accent-100)" }}>
            Your selected option is no longer in this plan; choose again.
          </div>
        ) : null}
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(220px,1fr))", gap: 12 }}>
          {plan.options.map((o) => (
            <OptionCard key={o.rankKey} option={o} selected={o.rankKey === option?.rankKey} onSelect={onPickOption} />
          ))}
        </div>
      </div>

      {option ? (
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(320px,1fr))", gap: 16 }}>
          <ChangesList
            heading={`${planOptionTitle(option.key, option.changes.length)} · approved together${reversible ? ", each reversible" : ""}`}
            changes={option.changes}
          />
          <WorkCenterLanes lanes={option.lanes} mode={lane} onModeChange={onLane} planKey={option.key} />
        </div>
      ) : null}

      <div className="blueprint" style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
        <i className="corner tl" />
        <i className="corner tr" />
        <i className="corner bl" />
        <i className="corner br" />
        <label style={{ fontSize: 12, color: "var(--color-neutral-700)" }} htmlFor="rationale">
          Your rationale (recorded with the promotion)
        </label>
        <textarea
          id="rationale"
          value={rationale}
          onChange={(e) => onRationale(e.target.value)}
          placeholder="Why you are approving this plan…"
          style={{ minHeight: 54, font: "inherit", fontSize: 13, padding: "6px 10px", border: "1px solid var(--color-divider)", background: "var(--color-surface)", resize: "vertical" }}
        />
        <div style={{ display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap" }}>
          <Btn
            variant="primary"
            busy={busy}
            disabled={!canCommand || option === null || rationale.trim().length === 0 || waiting !== null}
            onClick={() => {
              if (option) onApprove(option);
            }}
          >
            {option ? `Approve plan ${option.key} · ${option.changes.length} ${option.changes.length === 1 ? "change" : "changes"}` : "Approve"}
          </Btn>
          <Btn variant="ghost" busy={busy} disabled={!canCommand} onClick={onDismiss}>
            Dismiss
          </Btn>
          {option ? <NeedsChips needs={option.needs} option={option} policy={policy} /> : null}
          {waiting ? (
            <span role="note" style={{ fontSize: 12.5, color: "var(--color-accent-800)" }}>
              Approve is {waiting}
            </span>
          ) : null}
          <ReviewerHint canCommand={canCommand} />
        </div>
      </div>

      {showReason ? (
        <>
          <ReasoningPanel trace={plan.trace} legend={option === null} />
          {option ? <ReasoningPanel title={`This plan's reasoning · plan ${option.key}`} trace={option.trace} /> : null}
        </>
      ) : null}
    </>
  );
}

/** A plan whose slate carries no option: its do-nothing baseline and the plan's reasoning,
 *  including the Generation steps that explain why no candidate survived, with Dismiss and no
 *  Approve (there is nothing to promote). */
function NoOptionDetail({ entry, plan, onDismiss, busy, canCommand }: { entry: PlanEntry; plan: Plan; onDismiss: () => void; busy: boolean; canCommand: boolean }) {
  return (
    <>
      <DetailHeader entry={entry} plan={plan} scope="no option" title={entry.situation} />
      <p style={{ margin: 0, fontSize: 13.5, color: "var(--color-neutral-700)" }}>
        The agent found no option to propose for this plan. Its reasoning below shows what it tried and why each attempt was set aside.
      </p>
      <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
        <Btn variant="secondary" busy={busy} disabled={!canCommand} onClick={onDismiss}>
          Dismiss
        </Btn>
        <ReviewerHint canCommand={canCommand} />
      </div>
      <ReasoningPanel trace={plan.trace} />
    </>
  );
}

const STATE_WORD: Readonly<Record<string, string>> = { under_review: "under review" };

/** A plan that cannot be shown: its slate is still pending, or its plan read failed. The
 *  panel says which, names its points, and offers Dismiss. */
function PointDetail({
  item,
  onDismiss,
  busy,
  canCommand,
}: {
  item: Extract<InboxItem, { kind: "pending" | "unavailable" }>;
  onDismiss: () => void;
  busy: boolean;
  canCommand: boolean;
}) {
  const { entry } = item;
  const points = entry.resolves.map((p) => `${pointLabel(p.code)} ${STATE_WORD[p.state] ?? p.state}`).join(" · ");
  return (
    <>
      <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
        <span className="tag tag-neutral">{item.kind === "pending" ? "slate pending" : "plan unavailable"}</span>
        <span style={{ fontSize: 13, color: "var(--color-neutral-700)" }} title={entry.id}>
          {planLabel(entry.code)} · {points}
        </span>
      </div>
      <h2 style={{ fontSize: "clamp(20px,1.8vw,27px)", margin: 0, overflowWrap: "anywhere" }}>
        <Named text={entry.situation} />
      </h2>
      <p style={{ margin: 0, fontSize: 13.5, color: "var(--color-neutral-700)" }}>
        {item.kind === "pending"
          ? "The agent has not attached a plan yet. It stays listed until it has one, or until you dismiss it."
          : `The plan could not be read: ${item.reason}. It stays listed; you can dismiss it.`}
      </p>
      <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
        <Btn variant="secondary" busy={busy} disabled={!canCommand} onClick={onDismiss}>
          Dismiss
        </Btn>
        <ReviewerHint canCommand={canCommand} />
      </div>
    </>
  );
}

/** The severity tag: the tier fill plus the do-nothing baseline's days-late count when it
 *  slips the promise. Renders nothing when the contract serves no severity tier. */
function SeverityChip({ severity, slip, delivery }: { severity: string | null; slip: number | null; delivery: Plan["delivery"] }) {
  const meta = severityMeta(severity);
  if (!meta) return null;
  const late = slip !== null && slip > 0;
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 6,
        padding: "2px 8px",
        fontSize: 12,
        fontWeight: 600,
        letterSpacing: ".04em",
        textTransform: "uppercase",
        background: meta.background,
        color: meta.color,
        border: `1px solid ${meta.border}`,
      }}
    >
      {meta.label}
      {late ? <span style={{ fontWeight: 500 }}>· {slipLabel(slip, delivery)}</span> : null}
    </span>
  );
}

/** The "If we do nothing" line, built from the do-nothing baseline's served framing of the
 *  plan's lead point: "{lead order} ships {date} · {n} days late · {value} delayed" (the lead
 *  read as its label, its id on hover; the slip without a plus sign). Each part is omitted
 *  when the baseline does not serve it, and the whole line when it serves none of them. */
function BaselineLine({ plan }: { plan: Plan }) {
  const parts: string[] = [];
  const ships = formatDate(plan.projectedCompletion);
  const slip = plan.delivery === "on_time" ? "on time" : plan.daySlip !== null && plan.daySlip > 0 ? `${plan.daySlip} ${plan.daySlip === 1 ? "day" : "days"} late` : slipLabel(null, plan.delivery);
  if (slip) parts.push(slip);
  const value = formatMoney(plan.orderValue, plan.currency);
  if (value) parts.push(plan.delivery === "late" ? `${value} delayed` : `order ${value}`);
  // Nothing served: no line.
  if (!ships && parts.length === 0) return null;
  return (
    <div style={{ display: "flex", alignItems: "center", flexWrap: "wrap", gap: 8, fontSize: 13 }}>
      <span style={{ width: 24, height: 9, background: "var(--hatch)", border: "1px solid var(--color-accent-900)" }} />
      <span style={{ color: "var(--color-neutral-700)" }}>If we do nothing:</span>
      <span data-testid="baseline-line" style={{ fontWeight: 500 }}>
        <EntityName id={plan.lead} />
        {[ships ? ` ships ${ships}` : "", ...parts.map((part) => ` · ${part}`)].join("")}
      </span>
    </div>
  );
}

function AutoDetail({
  applied,
  now,
  showReason,
  onToggleReason,
  onUndo,
  busy,
  canCommand,
}: {
  applied: AppliedChange;
  now: number;
  showReason: boolean;
  onToggleReason: () => void;
  onUndo: (a: AppliedChange) => void;
  busy: boolean;
  canCommand: boolean;
}) {
  const at = clockTime(applied.appliedAt);
  const day = applied.appliedAt !== null && !isToday(applied.appliedAt, now) ? formatLocalDay(Number(applied.appliedAt) * 1000) : null;
  const when = [day, at].filter(Boolean).join(" ");
  const open = undoOpen(applied, now);
  return (
    <>
      <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
        <span className="tag tag-accent">Applied by agent{when ? ` · ${when}` : ""}</span>
        <span style={{ fontSize: 13, color: "var(--color-neutral-700)" }} title={applied.actionPoint}>
          {appliedLabel(applied.code)} · closes {pointLabel(applied.actionPointCode)}
        </span>
        <div style={{ marginLeft: "auto" }}>
          <ReasonToggle on={showReason} onToggle={onToggleReason} />
        </div>
      </div>
      {applied.title ? (
        <h2 style={{ fontSize: "clamp(18px,1.6vw,24px)", margin: 0 }}>{applied.title}</h2>
      ) : null}
      {applied.effect ? (
        <p style={{ fontSize: 13.5, lineHeight: 1.45, margin: 0, color: "var(--color-neutral-700)" }}>{applied.effect}</p>
      ) : null}
      <ChangesList heading="Changes written" changes={applied.changes} />
      <PolicyRules criteria={applied.criteria} />
      <div style={{ display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap" }}>
        {open ? (
          <Btn variant="secondary" busy={busy} disabled={!canCommand} onClick={() => onUndo(applied)}>
            Undo change
          </Btn>
        ) : null}
        <span style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>{undoStatus(applied, now)}</span>
        {open ? <ReviewerHint canCommand={canCommand} /> : null}
      </div>
    </>
  );
}
