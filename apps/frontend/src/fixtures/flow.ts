/**
 * Flow-tab fixtures: the recorded live projection of the seeded scenario, mapped through the
 * Flow view-model. Covers the flagship chain across the stages and the shape served when the
 * rules cannot be read (no `on_path` on suppliers / on order / in stock).
 */
import type { FlowView } from "../viewmodel/types.ts";
import { mapFlow } from "../data/tabMapping.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, rowsFor, rulesUnreadable } from "./scenario.ts";

export const flowRows = rowsFor(FLAGSHIP_SUBJECT, "flow");
export const flowView: FlowView = mapFlow(flowRows);
export const flowClearedView: FlowView = mapFlow(rowsFor(CLEARED_SUBJECT, "flow"));
export const flowUnservedView: FlowView = mapFlow(rulesUnreadable("flow", flowRows));

/** The empty Flow (a subject the projection serves no rows for). */
export const emptyFlowView: FlowView = { entities: [], labels: {} };
