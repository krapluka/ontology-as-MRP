import { describe, expect, test } from "vitest";
import { render } from "@testing-library/react";
import { WorkCenterLanes } from "./WorkCenterLanes.tsx";
import { dayTicks, laneRange } from "../../viewmodel/lanes.ts";
import type { Lane } from "../../viewmodel/types.ts";
import { EntityLabels } from "../primitives/entityLabels.ts";

// A bar with an unparseable date must be skipped (not rendered at NaN% left/width) and must
// affect neither the axis domain nor any sibling bar.
describe("WorkCenterLanes date guards", () => {
  test("skips a bar with an unparseable date and emits no NaN styles", () => {
    const lane: Lane = {
      workCenter: "https://ekg.example/id/mrp.workcenter/assembly",
      capacity: 2,
      before: [
        { workOrder: "GOOD", start: "2027-01-25", end: "2027-01-27", row: 0, changed: false },
        { workOrder: "BAD", start: "not-a-date", end: "also-bad", row: 1, changed: false },
      ],
      after: [],
    };

    const { container } = render(
      <WorkCenterLanes lanes={[lane]} mode="before" onModeChange={() => {}} planKey="A" />,
    );

    // The well-formed bar renders; the unparseable one is dropped.
    expect(container.querySelector('[title^="GOOD"]')).not.toBeNull();
    expect(container.querySelector('[title^="BAD"]')).toBeNull();
    // No rendered inline style leaked a NaN position/size.
    expect(container.innerHTML).not.toMatch(/NaN/);
  });
});

describe("WorkCenterLanes date range and ticks", () => {
  const lane = (start: string, end: string): Lane => ({
    workCenter: "https://ekg.example/id/mrp.workcenter/assembly",
    capacity: 2,
    before: [{ workOrder: "S1", start, end, row: 0, changed: false }],
    after: [],
  });

  test("the heading states the bar domain's date range, across months too", () => {
    expect(laneRange([lane("2027-01-24", "2027-02-07")])).toBe("24 Jan–7 Feb");
    expect(laneRange([lane("2027-01-09T08:00:00", "2027-01-14T12:00:00")])).toBe("9–14 Jan");
    expect(laneRange([lane("bad", "worse")])).toBeNull();
    const { getByText } = render(<WorkCenterLanes lanes={[lane("2027-01-24", "2027-02-07")]} mode="before" onModeChange={() => {}} planKey="A" />);
    expect(getByText("Work centers · 24 Jan–7 Feb")).toBeTruthy();
  });

  test("the day ticks mark every local midnight in the domain, every other day past two weeks", () => {
    const min = new Date(2027, 0, 24, 8).getTime();
    const ticks = dayTicks(min, new Date(2027, 0, 27, 8).getTime() - min);
    expect(ticks.map((t) => t.label)).toEqual(["25", "26", "27"]);
    expect(ticks.every((t) => t.left > 0 && t.left < 100)).toBe(true);
    const wide = dayTicks(new Date(2027, 0, 12).getTime(), 30 * 86_400_000);
    expect(wide.length).toBeLessThanOrEqual(16);
  });
});


describe("WorkCenterLanes names", () => {
  test("lane names and bar labels read as labels, end in an ellipsis and carry the full name on hover", () => {
    const WC = "https://ekg.example/id/mrp.workcenter/1";
    const WO = "https://ekg.example/id/mrp.workorder/5";
    const lane: Lane = {
      workCenter: WC,
      capacity: 1,
      before: [{ workOrder: WO, start: "2027-01-25", end: "2027-01-27", row: 0, changed: false }],
      after: [],
    };
    const labels = new Map([
      [WC, "Assembly Line"],
      [WO, "WH/MO/00001 · Assemble drive + frame"],
    ]);
    const { getByTestId } = render(
      <EntityLabels.Provider value={labels}>
        <WorkCenterLanes lanes={[lane]} mode="before" onModeChange={() => {}} planKey="A" />
      </EntityLabels.Provider>,
    );
    const name = getByTestId("lane-name").querySelector<HTMLElement>("[data-entity]");
    expect(name?.textContent).toBe("Assembly Line");
    expect(name?.title).toBe(`Assembly Line\n${WC}`);
    expect(name?.style.textOverflow).toBe("ellipsis");
    expect((getByTestId("lane-name").parentElement?.parentElement as HTMLElement).style.gridTemplateColumns).toBe("120px minmax(0,1fr)");
    const bar = getByTestId("lane-bar");
    expect(bar.textContent).toBe("WH/MO/00001 · Assemble drive + frame");
    expect(bar.title).toBe(`WH/MO/00001 · Assemble drive + frame\n${WO}`);
    expect((bar.firstElementChild as HTMLElement).style.textOverflow).toBe("ellipsis");
  });
});
