import type { ReactNode } from "react";

export type TagVariant = "accent" | "neutral" | "outline";

/**
 * A small label chip. `accent` marks the recommended option and applied state; `outline`
 * and `neutral` carry secondary labels. There is no severity variant: the contract serves no
 * per-plan severity tier.
 */
export function Tag({
  variant = "neutral",
  children,
  title,
}: {
  variant?: TagVariant;
  children: ReactNode;
  title?: string;
}) {
  return (
    <span className={`tag tag-${variant}`} title={title}>
      {children}
    </span>
  );
}
