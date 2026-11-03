import type { SupplierSummary } from "../../viewmodel/types.ts";
import { isServed } from "../../viewmodel/format.ts";

/** How the Suppliers tab chose the supplier it opens on. */
export type DefaultPick = {
  supplier: string;
  /** `on_path`: the first on-path supplier; `top_exposure`: the highest exposure (or one that
   *  may be highest); `first`: the first supplier in served row order. */
  basis: "on_path" | "top_exposure" | "first";
  /** Whether any row serves `on_path` (false: the path status is unknown). */
  pathServed: boolean;
  /** Whether every row serves its exposure (false: some are omitted, so "highest" is not
   *  known and the pick is a supplier that may carry the highest exposure). */
  exposureComplete: boolean;
};

/** A supplier's served exposure as a number, or null when it is omitted (unknown). */
export function exposureOf(summary: SupplierSummary): number | null {
  if (summary.exposure === null) return null;
  const n = Number(summary.exposure);
  return Number.isFinite(n) ? n : null;
}

/**
 * The supplier the Suppliers tab opens on:
 * - the first on-path supplier;
 * - otherwise (`on_path` served but no supplier on it, or `on_path` unserved) the top-exposure
 *   supplier when exposure is served on any row;
 * - else the first supplier in served row order.
 *
 * An omitted exposure is unknown, not the lowest: the read surface omits it only for a
 * supplier sourcing the cause of an at-risk order whose value is missing, so it is exposed.
 * It outranks a known 0 and cannot be ranked against a known positive figure. The pick is
 * then the first supplier, in served order, that may carry the highest exposure.
 */
export function defaultSupplier(summaries: ReadonlyArray<SupplierSummary>): DefaultPick | null {
  const first = summaries[0];
  if (!first) return null;
  const pathServed = isServed(summaries.map((s) => s.onPath));
  const known = summaries.map(exposureOf).filter((v): v is number => v !== null);
  const exposureComplete = known.length === summaries.length;

  const onPath = pathServed ? summaries.find((s) => s.onPath === true) : undefined;
  if (onPath) return { supplier: onPath.supplier, basis: "on_path", pathServed, exposureComplete };

  if (known.length === 0) return { supplier: first.supplier, basis: "first", pathServed, exposureComplete };
  const max = Math.max(...known);
  const mayBeTop = (s: SupplierSummary): boolean => {
    const v = exposureOf(s);
    return v === null || (v === max && (max > 0 || exposureComplete));
  };
  const top = summaries.find(mayBeTop) ?? first;
  return { supplier: top.supplier, basis: "top_exposure", pathServed, exposureComplete };
}
