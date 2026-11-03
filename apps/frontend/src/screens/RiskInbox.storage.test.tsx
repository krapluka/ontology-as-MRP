import { afterEach, describe, expect, test } from "vitest";
import { render, screen } from "@testing-library/react";
import { RiskInbox } from "./RiskInbox.tsx";
import { autoApplied, FIXTURE_NOW, kpis, planItems, plans, policy } from "../fixtures/inbox.ts";
import { fixedClock } from "../fixtures/clock.ts";

// Browser storage can be unavailable (blocked site data, a sandbox, a quota error): the inbox
// must still render. Each case swaps in a hostile `localStorage` and restores the in-memory
// one from src/test/setup.ts afterwards.
const memory = Object.getOwnPropertyDescriptor(globalThis, "localStorage");

function install(descriptor: PropertyDescriptor) {
  Object.defineProperty(globalThis, "localStorage", { ...descriptor, configurable: true });
  Object.defineProperty(window, "localStorage", { ...descriptor, configurable: true });
}

afterEach(() => {
  if (memory) install(memory);
});

function renderInbox() {
  render(
    <RiskInbox
      items={planItems(plans)}
      autoApplied={autoApplied}
      policy={policy}
      kpis={kpis}
      clock={fixedClock(FIXTURE_NOW)}
      onApprove={async () => ({ ok: true })}
      onDismiss={async () => ({ ok: true })}
      onUndo={async () => ({ ok: true })}
    />,
  );
}

const throwing = () => {
  throw new DOMException("The operation is insecure.", "SecurityError");
};

describe("RiskInbox with unavailable browser storage", () => {
  test("renders when every storage call throws", () => {
    install({ value: { getItem: throwing, setItem: throwing, removeItem: throwing, clear: throwing, key: throwing, length: 0 } });
    renderInbox();
    expect(screen.getByRole("heading", { level: 2 }).textContent).toBe("Split · Move · Expedite");
  });

  test("renders when merely reading localStorage throws", () => {
    install({ get: throwing });
    renderInbox();
    expect(screen.getByRole("heading", { level: 1 }).textContent).toMatch(/2 plans need your approval/);
  });
});
