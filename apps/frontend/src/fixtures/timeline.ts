/**
 * Timeline-tab fixtures: the recorded live projection of the seeded scenario, mapped through
 * the Timeline view-model: every promised order with the flagship order on the path, and the
 * plant's open inbound lines.
 */
import type { TimelineView } from "../viewmodel/types.ts";
import { mapTimeline } from "../data/tabMapping.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, rowsFor } from "./scenario.ts";

export const timelineRows = rowsFor(FLAGSHIP_SUBJECT, "timeline");
export const timelineView: TimelineView = mapTimeline(timelineRows);
export const timelineClearedView: TimelineView = mapTimeline(rowsFor(CLEARED_SUBJECT, "timeline"));

/** The empty Timeline (a subject the projection serves no rows for). */
export const emptyTimelineView: TimelineView = { orders: [], inbound: [] };
