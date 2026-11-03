import { describe, expect, test } from "vitest";
import { render, screen } from "@testing-library/react";
import { PlanRow } from "./PlanRow.tsx";
import { FIXTURE_NOW, itemOf, liveLabels, planWithIds, planWithLanes, pointOf } from "../../fixtures/inbox.ts";
import { EntityLabels } from "../primitives/entityLabels.ts";

// The row shows the display code the backend serves per point, so two points of one plan
// read apart. A point id derived in the view from the plan digest collided: mrp.production/1,
// /2 and /3 all read AP-17E3.
describe("PlanRow point ids", () => {
  test("two points of one plan with different served codes render different AP ids", () => {
    const ID = "https://ekg.example/id";
    const item = itemOf(
      { ...planWithLanes, resolves: [`${ID}/mrp.production/1`, `${ID}/mrp.production/2`] },
      {
        code: "0412",
        resolves: [pointOf(`${ID}/mrp.production/1`, { code: "17E3" }), pointOf(`${ID}/mrp.production/2`, { code: "5A0C" })],
      },
    );
    render(<PlanRow item={item} selected={false} now={FIXTURE_NOW} onSelect={() => {}} />);
    const row = screen.getByRole("button");
    expect(row.textContent).toContain("PL-0412");
    expect(row.textContent).toContain("Resolves AP-17E3 · AP-5A0C");
    // The full plan id stays the row's title and part of its accessible label.
    expect(row.getAttribute("title")).toBe(item.entry.id);
    expect(row.getAttribute("aria-label")).toContain(item.entry.id);
  });
});

describe("PlanRow title", () => {
  test("a situation title reads in names, is clamped to two lines, and carries the full title on hover", () => {
    // A pending plan is titled by its situation.
    const plan = { ...planWithIds, options: [] };
    render(
      <EntityLabels.Provider value={liveLabels}>
        <PlanRow item={itemOf(plan)} selected={false} now={FIXTURE_NOW} onSelect={() => {}} />
      </EntityLabels.Provider>,
    );
    const title = screen.getByTestId("plan-row-title");
    const text = "linked: WH/MO/00318 and WH/MO/00319 contend for work center Assembly Line beyond its capacity 1; S00147 · Stadtrad Leasing ships late.";
    expect(title.textContent).toBe(text);
    expect(title.getAttribute("title")).toBe(text);
    expect(title.style.webkitLineClamp).toBe("2");
    expect(title.style.overflow).toBe("hidden");
    expect(screen.getByRole("button").getAttribute("aria-label")).toContain(text);
  });
});
