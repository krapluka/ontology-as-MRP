import { describe, expect, test } from "vitest";
import { render } from "@testing-library/react";
import { EntityName, Named } from "./Named.tsx";
import { EntityLabels } from "./entityLabels.ts";

// Every entity id the view shows reads as its served label with the id on hover, else its
// short form.

const MO = "https://ekg.example/id/mrp.production/1";
const WC = "https://ekg.example/id/mrp.workcenter/1";
const labels = new Map([[MO, "WH/MO/00001"]]);

function withLabels(node: React.ReactNode) {
  return render(<EntityLabels.Provider value={labels}>{node}</EntityLabels.Provider>);
}

describe("Named", () => {
  test("shows each id by its label, the id on its title, and leaves the plain text as served", () => {
    const { container } = withLabels(<Named text={`linked: ${MO} and ${WC}.`} />);
    expect(container.textContent).toBe("linked: WH/MO/00001 and mrp.workcenter/1.");
    const ids = [...container.querySelectorAll<HTMLElement>("[data-entity]")];
    expect(ids.map((el) => [el.textContent, el.title])).toEqual([
      ["WH/MO/00001", MO],
      ["mrp.workcenter/1", WC],
    ]);
  });

  test("a URL that is not an entity id and has no label stays as served", () => {
    const { container } = withLabels(<Named text="see https://docs.example/page" />);
    expect(container.textContent).toBe("see https://docs.example/page");
  });

  test("a text without ids renders as one plain text", () => {
    const { container } = withLabels(<p>{<Named text="Battery Pack 500Wh short." />}</p>);
    expect(container.querySelector("p")?.childElementCount).toBe(0);
  });
});

describe("EntityName", () => {
  test("shows the label with the id on hover, else the short form", () => {
    const { container } = withLabels(
      <>
        <EntityName id={MO} />
        <EntityName id={WC} />
      </>,
    );
    const [named, unnamed] = [...container.querySelectorAll<HTMLElement>("[data-entity]")];
    expect([named?.textContent, named?.title]).toEqual(["WH/MO/00001", MO]);
    expect([unnamed?.textContent, unnamed?.title]).toEqual(["mrp.workcenter/1", WC]);
  });

  test("with an ellipsis it keeps one line and its hover carries the full label and the id", () => {
    const { container } = withLabels(<EntityName id={MO} ellipsis />);
    const el = container.querySelector<HTMLElement>("[data-entity]");
    expect(el?.style.textOverflow).toBe("ellipsis");
    expect(el?.style.whiteSpace).toBe("nowrap");
    expect(el?.title).toBe(`WH/MO/00001\n${MO}`);
  });
});
