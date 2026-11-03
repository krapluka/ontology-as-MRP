import { afterEach, describe, expect, test, vi } from "vitest";
import { render } from "@testing-library/react";
import { FlowTab } from "./FlowTab.tsx";
import { mapFlow, type RowMap } from "../../data/tabMapping.ts";

const ID = "https://ekg.example/id";
const PO = `${ID}/purchase.order/1`;
const FRAME = `${ID}/product.product/3`;
const BATTERY = `${ID}/product.product/4`;

// The rows the read surface serves for a chain PO with two lines (one per PO and product):
// only the chain component's line (the battery) is on the path, the frame line is not.
const rows: RowMap[] = [
  { entity: `${ID}/res.partner/6`, stage: "suppliers", label: "CellTech GmbH", on_path: "true" },
  { entity: PO, stage: "on_order", label: "P00001", product: FRAME, quantity: "4.0", on_path: "false", cause: "false" },
  { entity: PO, stage: "on_order", label: "P00001", product: BATTERY, quantity: "10.0", on_path: "true", cause: "true" },
  { entity: BATTERY, stage: "in_stock", label: "Battery", on_hand: "2.0", safety: "5.0", on_path: "true" },
  { entity: `${ID}/sale.order/1`, stage: "promised", label: "S00001", on_path: "true" },
];

afterEach(() => vi.restoreAllMocks());

describe("FlowTab: a multi-line chain PO", () => {
  test("draws one tile per line without a key clash, and attaches the chain to the lit line", () => {
    const errors = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const { container, getByTestId } = render(<FlowTab view={mapFlow(rows)} subject={`${ID}/sale.order/1`} />);

    const tiles = [...container.querySelectorAll<HTMLElement>(`[data-stage="on_order"] [data-entity="${PO}"]`)];
    expect(tiles).toHaveLength(2);
    expect(tiles.map((t) => t.dataset["path"])).toEqual(["off", "on"]);
    expect(tiles[1]?.textContent).toContain("Battery");
    expect(errors).not.toHaveBeenCalled();

    // Suppliers, On order, In stock, Promised (In production has no tile): three links, the
    // first ending on the second On order tile (y = 44 head + 68 pitch + 30 half tile = 142).
    const chain = getByTestId("flow-chain");
    expect(chain.dataset["links"]).toBe("3");
    expect(chain.querySelector("path")?.getAttribute("d")).toContain("20.6 142 ");
  });
});
