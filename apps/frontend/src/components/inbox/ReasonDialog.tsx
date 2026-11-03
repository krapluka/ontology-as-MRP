import { useEffect, useId, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { Btn } from "../primitives/Btn.tsx";

/**
 * The confirmation a Dismiss or Undo requires: an accessible modal (`role="dialog"`,
 * `aria-modal`, focus kept inside, Escape cancels) with a required reason. Confirm stays
 * disabled until the trimmed reason is non-empty, and the typed reason is what the command
 * carries into the audit trail. Focus starts in the reason field and returns to the control
 * that opened the dialog.
 */
export function ReasonDialog({
  title,
  description,
  confirmLabel,
  onConfirm,
  onCancel,
}: {
  title: string;
  /** What the command will do, shown to the operator before confirming. */
  description: string;
  confirmLabel: string;
  onConfirm: (reason: string) => void;
  onCancel: () => void;
}) {
  const [reason, setReason] = useState("");
  const dialogRef = useRef<HTMLDivElement>(null);
  const fieldRef = useRef<HTMLTextAreaElement>(null);
  const titleId = useId();
  const descId = useId();
  const fieldId = useId();
  const trimmed = reason.trim();

  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    fieldRef.current?.focus();
    return () => opener?.focus();
  }, []);

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Escape") {
      e.stopPropagation();
      onCancel();
      return;
    }
    if (e.key !== "Tab") return;
    // Keep keyboard focus inside the dialog: wrap from the last control to the first.
    const focusable = [...(dialogRef.current?.querySelectorAll<HTMLElement>("textarea, button:not(:disabled)") ?? [])];
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (!first || !last) return;
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 10,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: 16,
        background: "color-mix(in srgb, var(--color-text) 35%, transparent)",
      }}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descId}
        onKeyDown={onKeyDown}
        className="blueprint"
        style={{ width: "min(460px, 100%)", padding: "16px 18px", display: "flex", flexDirection: "column", gap: 10, background: "var(--color-bg)" }}
      >
        <h2 id={titleId} style={{ fontSize: 20, margin: 0 }}>
          {title}
        </h2>
        <p id={descId} style={{ margin: 0, fontSize: 13.5, color: "var(--color-neutral-700)" }}>
          {description}
        </p>
        <label htmlFor={fieldId} style={{ fontSize: 12, color: "var(--color-neutral-700)" }}>
          Your reason (required, recorded in the audit trail)
        </label>
        <textarea
          id={fieldId}
          ref={fieldRef}
          required
          aria-required="true"
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          style={{ minHeight: 64, font: "inherit", fontSize: 13, padding: "6px 10px", border: "1px solid var(--color-divider)", background: "var(--color-surface)", resize: "vertical" }}
        />
        <div style={{ display: "flex", justifyContent: "flex-end", gap: 10 }}>
          <Btn variant="ghost" onClick={onCancel}>
            Cancel
          </Btn>
          <Btn variant="primary" disabled={trimmed.length === 0} onClick={() => onConfirm(trimmed)}>
            {confirmLabel}
          </Btn>
        </div>
      </div>
    </div>
  );
}
