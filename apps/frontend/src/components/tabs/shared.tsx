import type { CSSProperties, ReactNode } from "react";
import type { EntitySelection, Marker } from "../../viewmodel/types.ts";
import { shortId } from "../../viewmodel/format.ts";

/** The copy shown where a column / stage carries no `on_path` (the rules, the cause map or the
 *  cause path could not be read): the path status is unknown, which is not "not on the path". */
export const PATH_NOT_SERVED = "path not served";

/** The small uppercase kicker line above a tab (accent-700 on the page ground). */
export function Kicker({ children }: { children: ReactNode }) {
  return (
    <div style={{ fontSize: 12, letterSpacing: ".12em", textTransform: "uppercase", color: "var(--color-accent-700)" }}>
      {children}
    </div>
  );
}

/** A section label above a panel (11.5 px uppercase neutral-700). */
export function SectionLabel({ children }: { children: ReactNode }) {
  return (
    <div style={{ fontSize: 11.5, letterSpacing: ".08em", textTransform: "uppercase", color: "var(--color-neutral-700)" }}>
      {children}
    </div>
  );
}

/**
 * The at-risk square before an order: filled when the rules flag it, hollow when they served
 * "not at risk", and a dashed square named "risk status not served" when the marker is not
 * served, so an unknown status is not drawn as "not at risk".
 */
export function RiskDot({ atRisk, size = 9, name }: { atRisk: Marker; size?: number; name?: string | undefined }) {
  const label = name ?? (atRisk === true ? "At risk" : atRisk === false ? "Not flagged" : "Risk status not served");
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      style={{
        width: size,
        height: size,
        flex: "none",
        display: "inline-block",
        background: atRisk === true ? "var(--color-accent-900)" : "transparent",
        border: `1px ${atRisk === null ? "dashed" : "solid"} ${atRisk === true ? "var(--color-accent-900)" : "var(--color-neutral-600)"}`,
      }}
    />
  );
}

/**
 * An entity a tab draws. When its IRI is the subject of another listed point, it renders as a
 * button that selects that point (selecting commits nothing); otherwise it is a plain box.
 * `data-entity` carries the IRI so tests and the chain overlay can find it.
 */
export function EntityBox({
  iri,
  selection,
  subject,
  style,
  children,
  path,
}: {
  iri: string;
  selection: EntitySelection | undefined;
  subject: string;
  style: CSSProperties;
  children: ReactNode;
  /** The entity's `on_path` status, exposed as `data-path` (on / off / unserved). */
  path: Marker;
}) {
  const dataPath = path === true ? "on" : path === false ? "off" : "unserved";
  const selectable = selection !== undefined && iri !== subject && selection.selectable.has(iri);
  if (selectable) {
    return (
      <button
        type="button"
        className="pp-unstyled"
        data-entity={iri}
        data-path={dataPath}
        title={`Select the open point for ${shortId(iri)}`}
        onClick={() => selection.onSelect(iri)}
        style={{ ...style, cursor: "pointer", boxSizing: "border-box" }}
      >
        {children}
      </button>
    );
  }
  return (
    <div data-entity={iri} data-path={dataPath} style={{ ...style, boxSizing: "border-box" }}>
      {children}
    </div>
  );
}
