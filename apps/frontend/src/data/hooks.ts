import { useCallback, useEffect, useRef, useState } from "react";
import { useClient } from "urql";
import {
  ActivePolicyDocument,
  ApprovedPlansDocument,
  AutoAppliedDocument,
  HeadlineMetricsDocument,
  LabelsDocument,
  PlansDocument,
  ReadModelDocument,
  TabKind,
} from "./graphql/graphql.ts";
import type {
  AppliedChange,
  ApprovedPlan,
  FlowView,
  GraphView,
  InboxItem,
  InboxKpis,
  Policy,
  SuppliersView,
  TimelineView,
} from "../viewmodel/types.ts";
import { mapAppliedChange, mapApproved, mapEntry, mapKpis, mapPlan, mapPolicy } from "./mapping.ts";
import { mapFlow, mapGraph, mapSuppliers, mapTimeline, toRowMaps } from "./tabMapping.ts";
import { systemClock, type Clock } from "../viewmodel/clock.ts";
import { localDayStart } from "../viewmodel/wording.ts";

export type InboxData = {
  /** `loading` until the first load lands; `error` only when the first load failed (nothing
   *  to show); `ready` once data has loaded, and it stays `ready` through later refreshes. */
  status: "loading" | "ready" | "error";
  /** Every plan awaiting oversight, one typed row each. */
  items: InboxItem[];
  autoApplied: AppliedChange[];
  /** The plans the operator approved today, plus earlier ones still undoable. */
  approved: ApprovedPlan[];
  policy: Policy | null;
  kpis: InboxKpis;
  /** The first load failed: there is no data, so the view shows an error page. */
  initialError: string | null;
  /** A refresh failed after data had loaded: the last data is kept, and the view says it may
   *  be stale instead of showing an error page. Cleared by the next good load. */
  refreshError: string | null;
};

const EMPTY: InboxData = {
  status: "loading",
  items: [],
  autoApplied: [],
  approved: [],
  policy: null,
  kpis: { waiting: 0, revenueAtRisk: null, currency: null, lateOrdersNow: null, lateOrders: null },
  initialError: null,
  refreshError: null,
};

function messageOf(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

/** A GraphQL error as the read reports it: its message and where in the response it arose. */
type FieldError = { message: string; path?: ReadonlyArray<string | number> | undefined };

/** The GraphQL errors a (partial) read carries, if any. */
function fieldErrors(error: unknown): FieldError[] {
  if (typeof error !== "object" || error === null || !("graphQLErrors" in error)) return [];
  const list = (error as { graphQLErrors?: unknown }).graphQLErrors;
  return Array.isArray(list) ? (list as FieldError[]) : [];
}

/** Whether a read error is only a set of per-entry plan field errors (`plans[i].plan…`): the
 *  list itself was served, and each failing entry is reported on its own. */
function onlyPlanFieldErrors(error: unknown): boolean {
  const errors = fieldErrors(error);
  const network = typeof error === "object" && error !== null && "networkError" in error && (error as { networkError?: unknown }).networkError;
  return !network && errors.length > 0 && errors.every((e) => e.path?.[0] === "plans" && e.path[2] === "plan");
}

/**
 * Loads the inbox from the read surface. The `plans` read lists every plan awaiting oversight,
 * one typed row each, keyed by its plan id: `plan` when it has options, `no_option` when its
 * slate is empty, `pending` when no slate is attached yet, `unavailable` when its plan read
 * failed (a field error on that entry only). Alongside it come the approved plans (today on the
 * operator's local day, from the injectable clock), the auto-applied list and the active
 * policy. `waiting` counts every row. The returned `refetch` is called after every command;
 * a failed refresh keeps the last data (`refreshError`) instead of replacing it.
 */
export function useInboxData(clock: Clock = systemClock): InboxData & { refetch: () => Promise<void> } {
  const client = useClient();
  const [data, setData] = useState<InboxData>(EMPTY);
  // Loads can overlap (two commands on different plans each refetch). Only the newest one
  // may land, so an older, slower response does not overwrite a newer read.
  const latest = useRef(0);
  // The clock is read at load time; held in a ref so a new clock object each render does not
  // re-create the load (and re-run it).
  const clockRef = useRef(clock);
  clockRef.current = clock;

  const load = useCallback(async () => {
    const seq = ++latest.current;
    const commit = (update: (d: InboxData) => InboxData) => {
      if (seq === latest.current) setData(update);
    };
    try {
      const since = String(localDayStart(clockRef.current.now()));
      const [plansRes, approvedRes, applied, policyRes, headline] = await Promise.all([
        client.query(PlansDocument, {}).toPromise(),
        client.query(ApprovedPlansDocument, { since }).toPromise(),
        client.query(AutoAppliedDocument, {}).toPromise(),
        client.query(ActivePolicyDocument, {}).toPromise(),
        client.query(HeadlineMetricsDocument, {}).toPromise(),
      ]);

      const plansError = plansRes.error && !(plansRes.data?.plans && onlyPlanFieldErrors(plansRes.error)) ? plansRes.error : undefined;
      const firstError = plansError ?? approvedRes.error ?? applied.error ?? policyRes.error ?? headline.error;
      if (firstError) throw firstError;

      const planErrors = fieldErrors(plansRes.error);
      const items = (plansRes.data?.plans ?? []).map<InboxItem>((e, i) => {
        const entry = mapEntry(e);
        const failed = planErrors.find((err) => err.path?.[1] === i);
        if (failed) return { kind: "unavailable", entry, reason: failed.message };
        if (!e.plan) return { kind: "pending", entry };
        const plan = mapPlan(e.plan, entry.situation);
        return plan.options.length > 0 ? { kind: "plan", entry, plan } : { kind: "no_option", entry, plan };
      });

      const autoApplied = (applied.data?.autoApplied ?? []).map(mapAppliedChange);
      const approved = (approvedRes.data?.approvedPlans ?? []).map(mapApproved);
      const policy = policyRes.data?.policy ? mapPolicy(policyRes.data.policy) : null;

      commit(() => ({
        status: "ready",
        items,
        autoApplied,
        approved,
        policy,
        kpis: mapKpis(items.length, headline.data?.headlineMetrics ?? []),
        initialError: null,
        refreshError: null,
      }));
    } catch (err) {
      const message = messageOf(err, "failed to load the inbox");
      commit((d) =>
        d.status === "ready" ? { ...d, refreshError: message } : { ...d, status: "error", initialError: message },
      );
    }
  }, [client]);

  useEffect(() => {
    void load();
  }, [load]);

  return { ...data, refetch: load };
}

/** The most ids one `labels` read may ask about (the read's own bound). */
const LABELS_PER_READ = 200;

/**
 * The display labels of the entity ids on screen: one `labels` read over the sorted,
 * de-duplicated ids not asked about yet, re-run when that set changes. A label is cached per
 * id for the session (an id the read cannot name is remembered as unnamed, so it is not asked
 * again); a read that fails caches nothing and the view keeps the short forms.
 */
export function useEntityLabels(ids: readonly string[]): ReadonlyMap<string, string> {
  const client = useClient();
  const cache = useRef(new Map<string, string | null>());
  const [labels, setLabels] = useState<ReadonlyMap<string, string>>(() => new Map());
  const key = [...new Set(ids)].sort().join("\n");

  useEffect(() => {
    const wanted = key.length > 0 ? key.split("\n") : [];
    const missing = wanted.filter((id) => !cache.current.has(id));
    if (missing.length === 0) return;
    let ignore = false;
    void (async () => {
      for (let at = 0; at < missing.length; at += LABELS_PER_READ) {
        const chunk = missing.slice(at, at + LABELS_PER_READ);
        const res = await client.query(LabelsDocument, { ids: chunk }).toPromise();
        const served = res.data?.labels;
        if (res.error || !Array.isArray(served)) return;
        for (const id of chunk) cache.current.set(id, null);
        for (const { id, label } of served) cache.current.set(id, label);
      }
      if (!ignore) {
        setLabels(new Map([...cache.current].filter((entry): entry is [string, string] => entry[1] !== null)));
      }
    })();
    return () => {
      ignore = true;
    };
  }, [client, key]);

  return labels;
}

/** The four impact tabs. The ids are also the values persisted in `localStorage`. */
export type TabId = "timeline" | "graph" | "flow" | "suppliers";

const TAB_KIND: Record<TabId, TabKind> = {
  timeline: TabKind.Timeline,
  graph: TabKind.Graph,
  flow: TabKind.Flow,
  suppliers: TabKind.Suppliers,
};

/** One tab's mapped view-model, discriminated by its tab id, so a consumer that has narrowed
 *  the tab gets that tab's view shape instead of a union of all four. */
export type TabView =
  | { tab: "timeline"; view: TimelineView }
  | { tab: "graph"; view: GraphView }
  | { tab: "flow"; view: FlowView }
  | { tab: "suppliers"; view: SuppliersView };

export type ReadModelData = {
  status: "idle" | "loading" | "ready" | "error";
  /** The mapped view for the requested tab, or null before the first successful load. */
  view: TabView | null;
  error: string | null;
};

/**
 * Loads one impact tab's read model for a subject: fetches `readModel(subject, tab)` and maps
 * its opaque rows through the per-tab view-model. Switching tab or subject re-fetches. With
 * no subject selected it stays idle (the tabs render an empty state).
 */
export function useReadModel(subject: string, tab: TabId): ReadModelData {
  const client = useClient();
  const [data, setData] = useState<ReadModelData>({ status: "idle", view: null, error: null });

  // `commit` gates every state write on the caller's staleness flag, so a slow response from a
  // superseded tab/subject switch cannot land the wrong tab's view over the current one.
  const load = useCallback(
    async (commit: (next: ReadModelData) => void) => {
      if (subject.length === 0) {
        commit({ status: "idle", view: null, error: null });
        return;
      }
      commit({ status: "loading", view: null, error: null });
      try {
        const res = await client.query(ReadModelDocument, { subject, tab: TAB_KIND[tab] }).toPromise();
        if (res.error) throw res.error;
        const rows = toRowMaps(res.data?.readModel.rows ?? []);
        const view: TabView =
          tab === "timeline"
            ? { tab, view: mapTimeline(rows) }
            : tab === "graph"
              ? { tab, view: mapGraph(rows) }
              : tab === "flow"
                ? { tab, view: mapFlow(rows) }
                : { tab, view: mapSuppliers(rows) };
        commit({ status: "ready", view, error: null });
      } catch (err) {
        commit({ status: "error", view: null, error: err instanceof Error ? err.message : "failed to load the tab" });
      }
    },
    [client, subject, tab],
  );

  useEffect(() => {
    let ignore = false;
    void load((next) => {
      if (!ignore) setData(next);
    });
    return () => {
      ignore = true;
    };
  }, [load]);

  return data;
}
