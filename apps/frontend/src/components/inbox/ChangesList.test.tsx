import { describe, expect, test } from "vitest";
import { render, screen } from "@testing-library/react";
import { ChangesList } from "./ChangesList.tsx";
import { NeedsChips } from "./NeedsChips.tsx";
import { EntityLabels } from "../primitives/entityLabels.ts";
import { expediteOnlyPolicy, liveLabels, planWithIds } from "../../fixtures/inbox.ts";

// The live-shaped PL-0412 option: its change rows read in names and formatted values, each
// from/to in its own wrapping element, and its needs as one chip.

const option = planWithIds.options[0]!;

describe("ChangesList", () => {
  test("rows read in names and formatted values, each from→to in its own wrapping element", () => {
    const { container } = render(
      <EntityLabels.Provider value={liveLabels}>
        <ChangesList changes={option.changes} />
      </EntityLabels.Provider>,
    );
    const rows = [...container.querySelectorAll<HTMLLIElement>("li")];
    expect(rows.map((li) => li.style.flexWrap)).toEqual(["wrap", "wrap", "wrap", "wrap"]);
    expect(screen.getAllByTestId("change-from-to").map((el) => el.textContent)).toEqual([
      "1 → 3",
      "Assembly Line → Paint Booth",
      "Assembly Line → Paint Booth",
      "24 Feb → 11 Jan",
    ]);
    // The from/to is a sibling of the verb-and-object block, not inside it.
    for (const li of rows) {
      const fromTo = li.querySelector("[data-testid=change-from-to]");
      expect(fromTo?.parentElement).toBe(li);
      expect(li.firstElementChild?.contains(fromTo ?? null)).toBe(false);
    }
    // The objects read as their labels, each with its id on hover; no id is shown as text.
    expect(rows[0]?.textContent).toContain("Raise capacity · Assembly Line capacity · City S1");
    expect(rows[1]?.textContent).toContain("Reassign · WH/MO/00318 · Assemble drive + frame");
    expect(container.textContent).not.toContain("https://");
    for (const el of container.querySelectorAll<HTMLElement>("[data-entity]")) expect(el.title).toBe(el.dataset.entity);
  });
});

describe("NeedsChips", () => {
  test("two reassigns and a capacity raise read as one chip, never twice", () => {
    render(<NeedsChips needs={option.needs} option={option} policy={expediteOnlyPolicy} />);
    expect(screen.getAllByTestId("need-chip").map((el) => el.textContent)).toEqual(["Not pre-approved: Raise capacity · Reassign"]);
  });
});
