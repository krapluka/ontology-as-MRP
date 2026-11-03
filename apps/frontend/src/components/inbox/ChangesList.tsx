import type { Change } from "../../viewmodel/types.ts";
import { changeValue, verbLabel } from "../../viewmodel/wording.ts";
import { EntityName, Named } from "../primitives/Named.tsx";

/** A change's from/to as served, each value worded ({@link changeValue}) and its ids read as
 *  labels: `from → to`, `→ to` when no from is served, `from →` when no to. Null when neither
 *  is served. */
function FromTo({ change }: { change: Change }) {
  const to = change.params.map((p) => p.value);
  const from = change.from;
  if (to.length === 0 && from === null) return null;
  return (
    <span data-testid="change-from-to" style={{ marginLeft: "auto", textAlign: "right", color: "var(--color-neutral-700)", overflowWrap: "anywhere" }}>
      {from !== null ? <Named text={changeValue(from, to[0] ?? null)} /> : null}
      {from !== null ? " →" : "→"}
      {to.map((value, i) => (
        <span key={i}>
          {i > 0 ? " ·" : null} <Named text={changeValue(value, from)} />
        </span>
      ))}
    </span>
  );
}

/**
 * The changes an option would commit: numbered `verb · object` rows (the verb worded by the
 * fixed label map, its code in the title) with the change's from/to beside them. The object
 * shows its served label, else the label the read serves for its id, else its short id, with
 * the id on hover. Each row wraps: the verb and object keep at least half the row, and a long
 * from/to moves below them instead of squeezing the object.
 */
export function ChangesList({ changes, heading }: { changes: Change[]; heading?: string }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
      {heading ? (
        <div style={{ fontSize: 11.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>
          {heading}
        </div>
      ) : null}
      <ol style={{ listStyle: "none", margin: 0, padding: 0, display: "flex", flexDirection: "column", gap: 6 }}>
        {changes.map((c, i) => (
          <li
            key={`${c.verb}-${c.object}-${i}`}
            style={{ display: "flex", flexWrap: "wrap", alignItems: "baseline", columnGap: 10, rowGap: 2, fontSize: 13.5 }}
          >
            <span style={{ display: "flex", alignItems: "baseline", gap: 10, flex: "1 1 50%", minWidth: "50%" }}>
              <span style={{ flex: "none", width: 18, color: "var(--color-accent-700)", fontWeight: 600 }}>{i + 1}</span>
              <span style={{ minWidth: 0 }}>
                <b style={{ fontWeight: 600 }} title={c.verb}>
                  {verbLabel(c.verb)}
                </b>{" "}
                · {c.label !== null ? <span title={c.object}>{c.label}</span> : <EntityName id={c.object} />}
              </span>
            </span>
            <FromTo change={c} />
          </li>
        ))}
      </ol>
    </div>
  );
}
