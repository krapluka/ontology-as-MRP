/**
 * The view-model the presentational components consume.
 *
 * Every field here is either sourced (read straight from the plant-pulse-api contract) or
 * derived (computed from contract data by a rule the operator could reconstruct; each such
 * field is commented `derived`). The read surface serves the plan display framing: severity
 * tier, projected completion date and day-slip, order value, each change's from and to, an
 * auto-applied change's title/effect, and the revenue / late-order KPIs. These ride the opaque
 * `metrics` bag or a neutral KPI list, so they are sourced here.
 */

/** A served plan delivery outcome (the `delivery` display metric). */
export type Delivery = "on_time" | "late" | "at_risk";

/** One entry of the opaque, capability-filled metrics bag. The contract enumerates no
 *  metric by name; the frontend renders whatever keys it carries (sourced). */
export type Metric = { key: string; value: string };

/** A score: the opaque metrics bag plus the authoritative ranking value (sourced). */
export type Score = { metrics: Metric[]; rankKey: string };

/** One parameter of a change: the intended new value(s) (sourced). */
export type ChangeParam = { key: string; value: string };

/** One governed change an option would commit (sourced). */
export type Change = {
  verb: string;
  object: string;
  params: ChangeParam[];
  /** The recorded inverse, present only once the change is executed (auto-applied). */
  inverse: string | null;
  /** sourced: the target's current (from) value, or null when the contract serves none. */
  from: string | null;
  /** sourced: the object's display label (e.g. "P00001 · 10 × Battery Pack 500Wh"), or null
   *  when no name is materialized. The view then shows the shortened object id. */
  label: string | null;
};

/** `Generation`: a model attempt that yielded no candidate (its failure class and reason),
 *  or the time budget ending generation. Such a step is set aside, not simulated. */
export type TraceTag = "Simulated" | "Graph" | "Assumption" | "Generation";

/** One reasoning-trace step; `rejected` marks a candidate tried and set aside (sourced). */
export type TraceStep = { tag: TraceTag; text: string; rejected: boolean };

/** Counts over a trace, derived server-side from the steps (sourced here). */
export type TraceSummary = { simulated: number; rejected: number; openAssumptions: number };

export type Trace = { steps: TraceStep[]; summary: TraceSummary };

/** A structured reason a plan is human-gated (sourced, not parsed from prose). */
export type Need = { code: string; detail: string };

/** One before/after bar on a work-center lane (sourced). */
export type Bar = {
  workOrder: string;
  start: string;
  end: string;
  row: number;
  /** Whether this option's changes moved the bar between before and after (sourced). */
  changed: boolean;
};

/** A work-center lane: capacity band and before/after bars (sourced, a projection read). */
export type Lane = {
  workCenter: string;
  capacity: number | null;
  before: Bar[];
  after: Bar[];
};

/** One kind's count in an option's scope (sourced): how many distinct objects of a kind
 *  (`order`, `work center`, `purchase order`; opaque data, not schema names) its changes
 *  touch, as counted by the backend. */
export type ScopeCount = { kind: string; count: number };

/** One ranked option on a plan's slate. */
export type Option = {
  /** derived: the slate label A/B/C from the option's rank position (best-first). */
  key: string;
  /** derived: a headline composed from the option's distinct change verbs, each worded by the
   *  fixed verb-label map (`split_production` reads "Split"). The contract serves no option
   *  verb. */
  label: string;
  /** sourced: the distinct objects its changes touch, per kind (empty when unserved). */
  scope: ScopeCount[];
  /** sourced: whether this is the agent's recommended option. */
  recommended: boolean;
  /** sourced: the authoritative ordering value (best-first). */
  rankKey: string;
  /** sourced: the policy reasons this option is human-gated (empty if none). */
  needs: Need[];
  /** sourced: the governed changes this option would commit, in listed order. */
  changes: Change[];
  /** sourced: the opaque display-metrics bag (server merges the derived cost/delivery). */
  metrics: Metric[];
  /** sourced: the served delivery outcome, or null when the bag carries none. */
  delivery: Delivery | null;
  /** derived: the Delivery label ("On time" / "N d late" / "At risk") from the served
   *  delivery and slip, or `null` when no delivery is served. */
  outcome: string | null;
  /** sourced: the severity tier (`critical`/`high`/`watch`/`on_track`) from the bag, or null. */
  severity: string | null;
  /** sourced: the projected completion/delivery date (ISO) from the bag, or null. */
  projectedCompletion: string | null;
  /** sourced: whole days the projected date slips past the original promise (signed), or null. */
  daySlip: number | null;
  /** sourced: the option's projected monetary cost from the bag, or null. */
  cost: string | null;
  /** sourced: the cost's ISO 4217 currency code, or null (then the cost renders plain). */
  costCurrency: string | null;
  /** sourced: the order's monetary value from the bag, or null. */
  orderValue: string | null;
  /** sourced: the order value's ISO 4217 currency code, or null. */
  currency: string | null;
  /** sourced: the before/after work-center lanes for this option (empty if none apply). */
  lanes: Lane[];
  /** sourced: this option's part of the reasoning trace. */
  trace: Trace;
};

/** A plan: a ranked slate over the points it resolves (one point, or every member of a
 *  portfolio). */
export type Plan = {
  /** sourced: the action-point subject the plan was read for (a listed plan: its lead). */
  subject: string;
  /** sourced: the subjects of the action points this plan resolves. */
  resolves: string[];
  /** sourced: the point whose baseline frames the plan: the one it resolves, or the member
   *  with the worst baseline. The impact tabs follow it. */
  lead: string;
  /** sourced: why the points need a plan: a portfolio's coupling reasons, or the point's
   *  deterministic monitor reason (not model-generated). */
  situation: string;
  /** sourced: the situation severity tier from the do-nothing baseline, or null. Drives the
   *  plan row's severity fill. */
  severity: string | null;
  /** sourced: the do-nothing baseline's delivery outcome, or null. */
  delivery: Delivery | null;
  /** sourced: whole days the do-nothing baseline slips the original promise (the row's
   *  "DAYS LATE" fill), or null when the baseline serves none. */
  daySlip: number | null;
  /** sourced: the do-nothing baseline's projected completion date (ISO), or null. */
  projectedCompletion: string | null;
  /** sourced: the order's monetary value (from the baseline's bag), or null. */
  orderValue: string | null;
  /** sourced: the order value's ISO 4217 currency code (from the baseline's bag), or null. */
  currency: string | null;
  /** sourced: the do-nothing baseline score. */
  baseline: Score;
  /** sourced: the ranked options, best-first. */
  options: Option[];
  /** sourced: the slate-level reasoning trace (incl. rejected candidates). */
  trace: Trace;
};

/** An action point's lifecycle state (sourced). */
export type PointState = "open" | "proposed" | "under_review" | "promoted" | "aborted" | "reversed" | "expired";

/** The lifecycle states in which an action point awaits human oversight. */
export type AwaitingState = "open" | "proposed" | "under_review";

/** One action point a plan resolves (sourced). */
export type ResolvedPoint = {
  /** sourced: the point's id. */
  id: string;
  /** sourced: the subject the point is keyed by. */
  subject: string;
  state: PointState;
  /** sourced: the deterministic monitor reason the point was opened. */
  reason: string;
  /** sourced: the point's short display code (four upper-case hex digits), distinct per
   *  point; the view shows it as `AP-{code}`. The view does not derive a label from an id or
   *  a digest. */
  code: string;
};

/** The portfolio a plan resolves, as its commands need it (sourced). */
export type PortfolioRef = { id: string; generation: number };

/** A plan as the inbox lists it (sourced): its id, the points it resolves, its portfolio (for
 *  a joint plan), when its slate was attached, and its situation. */
export type PlanEntry = {
  /** sourced: the plan id (`plan/` plus the digest of the points it resolves), which is the
   *  row's full label and title. */
  id: string;
  /** sourced: the plan's short display code; the view shows it as `PL-{code}`. */
  code: string;
  resolves: ResolvedPoint[];
  /** sourced: the portfolio (the joint plan's command target), or null for a single point. */
  portfolio: PortfolioRef | null;
  /** sourced: when the plan's slate was attached (epoch seconds), or null when unserved. */
  openedAt: string | null;
  /** sourced: the coupling reasons, or the single point's reason. */
  situation: string;
};

/** One row of the inbox: one plan awaiting oversight. Every point awaiting oversight is
 *  resolved by one listed plan, whether or not the plan can be shown; none is dropped.
 *  - `plan`: the plan has options to approve;
 *  - `no_option`: the slate carries no option (baseline + trace, Dismiss only);
 *  - `pending`: no slate is attached yet;
 *  - `unavailable`: the plan read failed (`reason` says why). */
export type InboxItem =
  | { kind: "plan"; entry: PlanEntry; plan: Plan }
  | { kind: "no_option"; entry: PlanEntry; plan: Plan }
  | { kind: "pending"; entry: PlanEntry }
  | { kind: "unavailable"; entry: PlanEntry; reason: string };

/** The plan id an inbox row stands for (its selection and in-flight key). */
export function itemId(item: InboxItem): string {
  return item.entry.id;
}

/** The subject the impact tabs follow for a row: its plan's lead point, else (while no plan
 *  is readable) the first point it resolves. */
export function itemLead(item: InboxItem): string {
  return item.kind === "plan" || item.kind === "no_option" ? item.plan.lead : (item.entry.resolves[0]?.subject ?? "");
}

/** The point of a row that is not `proposed` yet, or null when every point is ready. Approve
 *  waits for it, because a promotion claims every point from `proposed` only. */
export function waitingOn(entry: PlanEntry): ResolvedPoint | null {
  return entry.resolves.find((p) => p.state !== "proposed") ?? null;
}

/** A plan an operator approved (sourced): a promoted portfolio, or a point a human promoted
 *  on its own, with its undo window. */
export type ApprovedPlan = {
  id: string;
  /** sourced: the plan's short display code (`PL-{code}`). */
  code: string;
  resolves: ResolvedPoint[];
  portfolio: PortfolioRef | null;
  rankKey: string | null;
  changes: Change[];
  reviewer: string;
  rationale: string;
  /** Epoch seconds, or null if unknown. */
  promotedAt: string | null;
  /** End of the reversal window (epoch seconds), or null. An undo after it is refused. */
  undoUntil: string | null;
  reversible: boolean;
  /** sourced: how many committed changes are still unreversed (all of them when no undo ran). */
  remaining: number;
};

/** One policy criterion (sourced). */
export type Criterion = { rule: string; value: string };

/** A change an agent auto-applied under policy (sourced). */
export type AppliedChange = {
  actionPoint: string;
  subject: string;
  /** sourced: the change's short display code; the view shows it as `AU-{code}`. */
  code: string;
  /** sourced: the display code of the point it closed; the view shows it as `AP-{code}`. */
  actionPointCode: string;
  changes: Change[];
  criteria: Criterion[];
  /** Unix epoch seconds (stringly), or null if unknown. */
  appliedAt: string | null;
  /** End of the reversal window (epoch seconds), or null. An undo after it is refused. */
  undoUntil: string | null;
  reversible: boolean;
  /** sourced: the composed display title (its verbs) from the opaque bag, or null. */
  title: string | null;
  /** sourced: the composed effect sentence from the opaque bag, or null. */
  effect: string | null;
};

/** The active auto-apply policy as the operator sees it (sourced). */
export type Policy = {
  policyId: string;
  criteria: Criterion[];
  /** sourced: the ISO 4217 currency the cost limit is stated in, or null when unserved. */
  costLimitCurrency: string | null;
  undoWindow: string;
  autoApplyActions: string[];
};

// --- read-model tab view-models ---
//
// The four impact tabs ride the opaque `readModel(subject, tab)` rows. Each tab's view-model
// maps its neutral rows to typed props. A value the row omits maps to `null` here so the
// presentational layer can drop it.
//
// The three markers the read surface derives from the monitor's own rules (`at_risk`,
// `on_path`, `cause`) are tri-state: `true` / `false` when served, `null` when the row omits
// the key. `null` means "not served" (the rules or the cause path could not be read), and
// the view must not render it as a negative.

/** A served boolean marker, or `null` when the row does not serve it (unknown, not false). */
export type Marker = boolean | null;

/** The sourcing role of a supplier (for a component, or its strongest over all components). */
export type Sourcing = "single" | "primary" | "backup";

/** One order on the Timeline tab: its promise tick and, when served, its ready date. */
export type TimelineOrder = {
  /** sourced: the order/entity IRI the row is about. */
  entity: string;
  /** sourced: the order's display name (`ekg:name`), or null when unserved. */
  label: string | null;
  /** sourced: the promise (commitment) date (ISO), or null when the row served none. */
  promise: string | null;
  /** sourced: the ready/completion date (ISO), or null when unserved. */
  ready: string | null;
  /** sourced: the order's monetary value, or null when unserved. */
  orderValue: string | null;
  atRisk: Marker;
  onPath: Marker;
};

/** One open inbound purchase-order line on the Timeline tab (one per PO and product). */
export type TimelineInbound = {
  /** sourced: the purchase-order IRI. */
  entity: string;
  label: string | null;
  /** sourced: the arrival date (earliest open shipment ETA, else the planned date). */
  date: string;
  product: string | null;
  /** sourced: the open (not yet received) quantity. */
  quantity: number | null;
  supplier: string | null;
  onPath: Marker;
  cause: Marker;
};

/** The Timeline view-model: every promised order plus the plant's open inbound lines. */
export type TimelineView = { orders: TimelineOrder[]; inbound: TimelineInbound[] };

/** One directed edge on the Graph tab, in its stored RDF direction (sourced). */
export type GraphEdge = { source: string; predicate: string; target: string; onPath: Marker };

/** One typed node on the Graph tab. */
export type GraphNode = {
  /** sourced: the node IRI. */
  node: string;
  /** sourced: the capability role (`Customer`, `SalesOrder`, …), or null. */
  role: string | null;
  /** sourced: the display name (`ekg:name`), or null when unserved. */
  label: string | null;
  atRisk: Marker;
  onPath: Marker;
  cause: Marker;
};

/** The Graph view-model: the plant-wide supply graph as typed nodes and directed edges. */
export type GraphView = { nodes: GraphNode[]; edges: GraphEdge[] };

/** The five Flow pipeline stages, in column order. */
export type FlowStage = "suppliers" | "on_order" | "in_stock" | "in_production" | "promised";

/** One entity placed in a Flow pipeline stage, with the tile facts its row serves. */
export type FlowEntity = {
  /** sourced: the entity IRI (a supplier, PO, component product, MO / work order, order). */
  entity: string;
  /** sourced: the pipeline stage the entity sits in. */
  stage: FlowStage;
  label: string | null;
  atRisk: Marker;
  onPath: Marker;
  cause: Marker;
  /** suppliers: lead time in days and sourcing role. */
  leadTimeDays: number | null;
  sourcing: Sourcing | null;
  /** on_order: the vendor, product, open quantity and arrival. */
  supplier: string | null;
  product: string | null;
  quantity: number | null;
  arrival: string | null;
  /** in_stock (a component product): total on-hand and the safety minimum. */
  onHand: number | null;
  safety: number | null;
  /** in_production: the lifecycle state and, for a work order, the MO it is part of. */
  lifecycleState: string | null;
  partOf: string | null;
  /** promised: the commitment date and order value. */
  commitmentDate: string | null;
  orderValue: string | null;
};

/** The Flow view-model: the placed entities (the view buckets them by stage) plus the
 *  display names the rows serve, keyed by IRI, to name a referenced entity. */
export type FlowView = { entities: FlowEntity[]; labels: Readonly<Record<string, string>> };

/** One supplier summary row on the Suppliers tab. */
export type SupplierSummary = {
  /** sourced: the supplier IRI. */
  supplier: string;
  label: string | null;
  /** sourced: the sourcing lead time in whole days, or null when unserved. */
  leadTimeDays: number | null;
  /** sourced: the on-time-delivery ratio in [0,1], or null when the supplier has no
   *  completed receipts (omitted, not rendered as 0). */
  otd: number | null;
  /** sourced: the value of at-risk orders whose cause component it sources, or null when
   *  the at-risk decision is not served. */
  exposure: string | null;
  sourcing: Sourcing | null;
  onPath: Marker;
};

/** One open purchase-order line in a supplier's detail (sourced; null when unserved). */
export type SupplierOpenPo = {
  supplier: string;
  po: string | null;
  label: string | null;
  product: string | null;
  quantity: number | null;
  arrival: string | null;
  onPath: Marker;
  cause: Marker;
};

/** One sourced component's stock vs safety vs 30-day demand in a supplier's detail. */
export type SupplierStock = {
  supplier: string;
  stockOf: string | null;
  label: string | null;
  onHand: number | null;
  safety: number | null;
  /** sourced: the 30-day demand, or null when no order falls in the window (omitted). */
  demand: number | null;
  /** sourced: this supplier's sourcing role for the component. */
  sourcing: Sourcing | null;
  onPath: Marker;
  cause: Marker;
};

/** One order depending on a supplier's components (sourced). */
export type SupplierDependentOrder = {
  supplier: string;
  order: string | null;
  label: string | null;
  orderValue: string | null;
  commitmentDate: string | null;
  atRisk: Marker;
  onPath: Marker;
};

/** The per-supplier detail bundle (open POs, stock, dependent orders), grouped by supplier. */
export type SupplierDetail = {
  openPos: SupplierOpenPo[];
  stock: SupplierStock[];
  dependentOrders: SupplierDependentOrder[];
};

/** The Suppliers view-model: the summary table plus detail grouped by supplier IRI, and
 *  the display names the rows serve keyed by IRI. */
export type SuppliersView = {
  summaries: SupplierSummary[];
  detailBySupplier: Record<string, SupplierDetail>;
  labels: Readonly<Record<string, string>>;
};

/** How a tab lets the operator select a listed plan from an entity:
 *  `selectable` holds every point subject the listed plans resolve; `onSelect` selects the
 *  plan that resolves one (and shows that point on the tabs). `pointLabel` names a selectable
 *  subject's point (`AP-XXXX`, null for any other); `onReview` opens the plan that resolves it
 *  in the inbox. None of them commits anything. */
export type EntitySelection = {
  selectable: ReadonlySet<string>;
  onSelect: (subject: string) => void;
  pointLabel?: (subject: string) => string | null;
  onReview?: (subject: string) => void;
};

/** Header KPIs. `waiting` is a derived count (the "applied today" count is derived by the
 *  screen against the live clock, since "today" moves while it stays open); the
 *  revenue-at-risk and late-orders values are sourced from the read surface's opaque
 *  headline-metrics list (null when it serves none). */
export type InboxKpis = {
  /** derived: number of plans awaiting oversight, i.e. every listed inbox row. */
  waiting: number;
  /** sourced: total revenue exposed by orders awaiting oversight (a decimal), or null. */
  revenueAtRisk: string | null;
  /** sourced: the revenue-at-risk ISO 4217 currency code, or null. */
  currency: string | null;
  /** sourced: how many orders are late now (before any plan is approved), or null. */
  lateOrdersNow: string | null;
  /** sourced: how many orders the recommended plans still leave late, or null. */
  lateOrders: string | null;
};
