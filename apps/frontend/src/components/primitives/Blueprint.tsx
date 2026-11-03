import type { CSSProperties, ReactNode } from "react";

/**
 * The blueprint frame: a square, hairline-bordered box with the four "+" registration marks
 * at its corners.
 */
export function Blueprint({
  children,
  padding,
  style,
  className,
  as: Tag = "div",
}: {
  children: ReactNode;
  padding?: CSSProperties["padding"];
  style?: CSSProperties;
  className?: string;
  as?: "div" | "section" | "li";
}) {
  return (
    <Tag className={["blueprint", className].filter(Boolean).join(" ")} style={{ padding, ...style }}>
      <i className="corner tl" />
      <i className="corner tr" />
      <i className="corner bl" />
      <i className="corner br" />
      {children}
    </Tag>
  );
}
