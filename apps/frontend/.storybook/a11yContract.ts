// The a11y gate's contract as a pure function, so every rule is unit-tested
// (src/test/a11yContract.test.ts). `.storybook/vitest.setup.ts` runs it after each story,
// once the a11y addon has reported.

/** The part of Storybook's `Report` (storybook/internal/csf) the contract reads. */
export type A11yReportLike = { type: string; status: "failed" | "passed" | "warning"; result: unknown };

export type A11yContractInput = {
  /** The story id, for messages. */
  id: string;
  /** Every report recorded for the story (`context.reporting.reports`). */
  reports: ReadonlyArray<A11yReportLike>;
  /** The story's composed parameters (`a11yCanary` marks the gate's self-test story). */
  parameters: Readonly<Record<string, unknown>>;
  /** The story's composed globals (`a11y.manual` is set by Storybook's UI test widget). */
  globals: Readonly<Record<string, unknown>>;
  /** The globals the story or its meta set themselves (`context.storyGlobals`). */
  storyGlobals: Readonly<Record<string, unknown>>;
  /** True when the run was started from Storybook's own UI (`VITEST_STORYBOOK === "true"`). */
  storybookUi: boolean;
};

type AxeResultItem = { id: string; help: string; nodes: ReadonlyArray<unknown>; error?: unknown };

function itemsOf(result: unknown, key: "violations" | "incomplete"): ReadonlyArray<AxeResultItem> {
  if (typeof result !== "object" || result === null || !(key in result)) return [];
  const items: unknown = (result as Record<string, unknown>)[key];
  return Array.isArray(items) ? (items as AxeResultItem[]) : [];
}

function isManual(globals: Readonly<Record<string, unknown>>): boolean {
  const a11y = globals.a11y;
  return typeof a11y === "object" && a11y !== null && "manual" in a11y && a11y.manual === true;
}

/**
 * Enforces the a11y contract for one finished story. Throws when it is broken, otherwise
 * returns the lines to print for `todo`-excused violations, which the CLI never shows.
 *
 * - A run started from Storybook's UI with its "Accessibility" option unchecked (the addon's
 *   default) is a local run and nothing is enforced: the widget sets `globals.a11y.manual`.
 *   A story or meta that sets `manual` itself is never excused. The CLI gate
 *   (`npm run test:stories`) pins `VITEST_STORYBOOK=false`, so it never runs in that mode.
 *   The UI setting lives per checkout in node_modules/.cache and `npm ci` resets it.
 * - No a11y report means the check was skipped (`test: "off"`, `disable`, `manual`). The only
 *   excuse is `test: "todo"`, which still reports.
 * - A rule whose check threw fails the story. axe records it as "incomplete" with `error`,
 *   which does not fail on its own (e.g. a stale `a11y.config` in preview.ts).
 * - The canary story (`parameters.a11yCanary`) must be reported as `color-contrast`, or the
 *   contrast check has silently stopped working.
 */
export function checkA11yContract(input: A11yContractInput): ReadonlyArray<string> {
  const { id, reports, parameters, globals, storyGlobals, storybookUi } = input;
  // Excused only when the UI widget, not the story or its meta, turned a11y off.
  if (storybookUi && isManual(globals) && !isManual(storyGlobals)) return [];

  const a11yReports = reports.filter((report) => report.type === "a11y");
  if (a11yReports.length === 0) {
    throw new Error(
      `${id}: the a11y check did not run (switched off via parameters or globals). Excuse a story only with a11y: { test: "todo" }`,
    );
  }

  const errored = a11yReports.flatMap((report) =>
    itemsOf(report.result, "incomplete")
      .filter((rule) => rule.error !== undefined)
      .map((rule) => rule.id),
  );
  if (errored.length > 0) {
    throw new Error(
      `${id}: a11y check(s) errored instead of running (${errored.join(", ")}). Is preview.ts a11y.config stale for the installed axe-core?`,
    );
  }

  if (parameters.a11yCanary === true) {
    const caught = a11yReports.some((report) =>
      itemsOf(report.result, "violations").some((violation) => violation.id === "color-contrast"),
    );
    if (!caught) {
      throw new Error(`${id}: the a11y canary was not caught, so the color-contrast check is not effective`);
    }
    return [];
  }

  return a11yReports
    .filter((report) => report.status === "warning")
    .flatMap((report) =>
      itemsOf(report.result, "violations").map(
        (violation) => `[a11y todo] ${id}: ${violation.id}: ${violation.help} (${violation.nodes.length} element(s))`,
      ),
    );
}
