/**
 * Suppliers-tab fixtures: the recorded live projection of the seeded scenario, mapped through
 * the Suppliers view-model. Covers the flagship point (its supplier on the path), an open
 * point whose rule has cleared (no supplier on its path), and the shape served when the rules
 * cannot be read (no `on_path`, no `exposure`).
 */
import type { SuppliersView } from "../viewmodel/types.ts";
import { mapSuppliers } from "../data/tabMapping.ts";
import { CLEARED_SUBJECT, FLAGSHIP_SUBJECT, rowsFor, rulesUnreadable } from "./scenario.ts";

export const suppliersRows = rowsFor(FLAGSHIP_SUBJECT, "suppliers");
export const suppliersView: SuppliersView = mapSuppliers(suppliersRows);
export const suppliersClearedView: SuppliersView = mapSuppliers(rowsFor(CLEARED_SUBJECT, "suppliers"));
export const suppliersUnservedView: SuppliersView = mapSuppliers(rulesUnreadable("suppliers", suppliersRows));

/** The same rows as served when no sales order commits inside the 30-day window: the read
 *  surface omits `demand`, so no gauge may draw a demand marker. */
export const suppliersNoDemandView: SuppliersView = mapSuppliers(
  suppliersRows.map((row) => Object.fromEntries(Object.entries(row).filter(([key]) => key !== "demand"))),
);

/** The cleared point's rows as served when an at-risk order carries no value: the read
 *  surface omits the exposure of every supplier whose sum would need that value (here the
 *  suppliers with a served non-zero exposure, which have one at-risk order behind them),
 *  keeps the known `0.00` of the rest, and the order's `dependent_order` rows lose their
 *  `order_value`. */
const clearedSuppliersRows = rowsFor(CLEARED_SUBJECT, "suppliers");
export const suppliersExposurePartlyServedView: SuppliersView = mapSuppliers(
  clearedSuppliersRows.map((row) => {
    const drop =
      row["kind"] === "supplier" && Number(row["exposure"] ?? 0) > 0
        ? "exposure"
        : row["kind"] === "dependent_order" && row["at_risk"] === "true"
          ? "order_value"
          : null;
    return drop === null ? row : Object.fromEntries(Object.entries(row).filter(([key]) => key !== drop));
  }),
);

/** The empty Suppliers (a subject the projection serves no rows for). */
export const emptySuppliersView: SuppliersView = { summaries: [], detailBySupplier: {}, labels: {} };
