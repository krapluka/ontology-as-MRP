import type { StorybookConfig } from "@storybook/react-vite";

const config: StorybookConfig = {
  stories: ["../src/**/*.stories.@(ts|tsx)"],
  // addon-vitest runs every story (render, play and a11y) in headless Chromium as part of
  // `npm run test`. Without it `a11y: { test: "error" }` is not enforced.
  addons: ["@storybook/addon-a11y", "@storybook/addon-vitest"],
  framework: {
    name: "@storybook/react-vite",
    options: {},
  },
};

export default config;
