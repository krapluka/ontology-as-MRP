import type { CSSProperties } from "react";
import { entityText, idText, splitIds } from "../../viewmodel/wording.ts";
import { useEntityLabelMap } from "./entityLabels.ts";

/** One line that ends in an ellipsis when it does not fit. */
const ELLIPSIS: CSSProperties = { display: "block", minWidth: 0, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" };

/**
 * An entity id as the operator reads it: its served label, else its short `model/id` form,
 * with the full id on hover. `ellipsis` keeps it to one line that ends in an ellipsis, and
 * then the hover also carries the full label.
 */
export function EntityName({ id, ellipsis = false }: { id: string; ellipsis?: boolean }) {
  const labels = useEntityLabelMap();
  const text = entityText(id, labels);
  const title = ellipsis && text !== id ? `${text}\n${id}` : id;
  return (
    <span data-entity={id} title={title} style={ellipsis ? ELLIPSIS : undefined}>
      {text}
    </span>
  );
}

/**
 * A served text with every id it carries shown by its label ({@link EntityName}), the plain
 * runs untouched. A URL the read serves no label for and that is not an entity id stays as
 * served. The text itself keeps its ids; only the view reads them as names.
 */
export function Named({ text }: { text: string }) {
  const labels = useEntityLabelMap();
  return (
    <>
      {splitIds(text).map((run, i) =>
        run.kind === "text" ? (
          run.text
        ) : (
          <span key={i} data-entity={run.id} title={run.id}>
            {idText(run.id, labels)}
          </span>
        ),
      )}
    </>
  );
}
