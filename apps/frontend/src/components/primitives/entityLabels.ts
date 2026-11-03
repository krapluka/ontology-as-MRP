import { createContext, useContext } from "react";

const NO_LABELS: ReadonlyMap<string, string> = new Map();

/** The display labels the read served for the entity ids on screen. */
export const EntityLabels = createContext<ReadonlyMap<string, string>>(NO_LABELS);

/** The labels in scope, for a text that cannot carry an element per id (a title, an
 *  accessible name). */
export function useEntityLabelMap(): ReadonlyMap<string, string> {
  return useContext(EntityLabels);
}
