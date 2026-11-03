import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { storybookTest } from "@storybook/addon-vitest/vitest-plugin";

// The Risk-inbox frontend. Reads go to the plant-pulse-api GraphQL surface, the
// governed writes to its REST commands; both are proxied under /api in dev so the
// app talks to one origin. Defaults to the podman stack's published backend (:8090,
// from `just podman-local-up`); override with EKG_API_ORIGIN (e.g. a host-run backend
// on :4000 via EKG_API_PORT).
const apiOrigin = process.env.EKG_API_ORIGIN ?? "http://localhost:8090";
const dirname = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": { target: apiOrigin, changeOrigin: true },
    },
  },
  test: {
    projects: [
      {
        // Fast jsdom tests: view-model mapping, commands, hooks, and the composed-story
        // behaviour tests (`npm run test:unit` runs only this project).
        extends: true,
        test: {
          name: "unit",
          globals: true,
          environment: "jsdom",
          setupFiles: ["./src/test/setup.ts"],
          include: ["src/**/*.{test,spec}.{ts,tsx}"],
          css: true,
        },
      },
      {
        // Every story rendered in headless Chromium: its play function runs and the a11y
        // addon checks it with real layout + styles (so colour contrast is checked too).
        // Needs `npx playwright install chromium` once.
        extends: true,
        plugins: [storybookTest({ configDir: path.join(dirname, ".storybook") })],
        test: {
          name: "storybook",
          browser: {
            enabled: true,
            provider: "playwright",
            headless: true,
            instances: [{ browser: "chromium" }],
          },
          setupFiles: ["./.storybook/vitest.setup.ts"],
        },
      },
    ],
  },
});
