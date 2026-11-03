// Setup for the `storybook` Vitest project: every story runs in headless Chromium with the
// same project annotations Storybook itself applies. Loading the a11y addon's annotations
// here is what makes `a11y: { test: "error" }` (preview.ts) fail a story on a violation.
// The addon's own vitest setup file already runs the composed `beforeAll`, so it is not
// registered again here.
import * as a11yAddonAnnotations from "@storybook/addon-a11y/preview";
import { setProjectAnnotations } from "@storybook/react-vite";
import type { Preview } from "@storybook/react-vite";
import { checkA11yContract } from "./a11yContract.ts";
import previewAnnotations from "./preview.ts";

// Enforces the gate's a11y contract (see a11yContract.ts) after the a11y check has reported,
// and prints `todo`-excused violations, which the CLI would never show. Listed first because
// composed `afterEach` hooks run in reverse order, so the first-listed runs last.
const enforceA11yContract: Preview = {
  afterEach: ({ id, reporting, parameters, globals, storyGlobals }) => {
    const lines = checkA11yContract({
      id,
      reports: reporting.reports,
      parameters,
      globals,
      storyGlobals,
      storybookUi: import.meta.env.VITEST_STORYBOOK === "true",
    });
    for (const line of lines) console.warn(line);
  },
};

setProjectAnnotations([enforceA11yContract, a11yAddonAnnotations, previewAnnotations]);
