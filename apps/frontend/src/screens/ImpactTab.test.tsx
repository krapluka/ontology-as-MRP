import { describe, expect, test } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { Provider, type Client } from "urql";
import { ImpactTab } from "./ImpactTab.tsx";
import { fakeClock } from "../fixtures/clock.ts";
import { FLAGSHIP_SUBJECT, RECORDED_ON, rowsFor } from "../fixtures/scenario.ts";

// The Timeline's "today" comes from the injectable clock, not the system clock: with a fake
// clock the marker sits where the clock says, and moves with it.
const rows = rowsFor(FLAGSHIP_SUBJECT, "timeline").map((row) => ({
  fields: Object.entries(row).map(([key, value]) => ({ key, value })),
}));

const client = {
  query: () => ({
    toPromise: async () => ({ data: { readModel: { subject: FLAGSHIP_SUBJECT, tab: "TIMELINE", rows } } }),
  }),
} as unknown as Client;

const DAY = 86_400_000;

describe("ImpactTab", () => {
  test("the Timeline's today marker follows the injected clock", async () => {
    const clock = fakeClock(new Date(`${RECORDED_ON}T12:00:00`).getTime());
    render(
      <Provider value={client}>
        <ImpactTab subject={FLAGSHIP_SUBJECT} tab="timeline" selection={{ selectable: new Set(), onSelect: () => {} }} clock={clock} />
      </Provider>,
    );
    const marker = await screen.findByText("Today");
    const before = Number.parseFloat(marker.style.left);
    expect(Number.isFinite(before)).toBe(true);
    await act(async () => clock.advance(3 * DAY));
    const after = Number.parseFloat(screen.getByText("Today").style.left);
    // Three days on a weeks-long axis move it by whole percentage points (real time drifting a
    // few milliseconds between renders would not).
    expect(after - before).toBeGreaterThan(1);
  });
});
