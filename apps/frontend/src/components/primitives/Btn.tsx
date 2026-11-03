import type { ButtonHTMLAttributes } from "react";

export type BtnVariant = "primary" | "secondary" | "ghost";

/** A button in the Industry look (square, hairline). Variants: primary / secondary / ghost.
 *  `busy` marks a command in flight: the button is disabled (so a second click issues
 *  nothing) and announces `aria-busy` until the command settles. */
export function Btn({
  variant = "secondary",
  className,
  type = "button",
  busy = false,
  disabled,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: BtnVariant; busy?: boolean }) {
  return (
    <button
      type={type}
      className={["btn", `btn-${variant}`, className].filter(Boolean).join(" ")}
      disabled={disabled || busy}
      aria-busy={busy || undefined}
      {...props}
    />
  );
}
