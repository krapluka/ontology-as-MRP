import type { Preview } from "@storybook/react-vite";
// The same self-hosted faces main.tsx loads (tokens.css imports no font).
import "@fontsource/barlow/400.css";
import "@fontsource/barlow/500.css";
import "@fontsource/barlow/700.css";
import "@fontsource/barlow-condensed/400.css";
import "@fontsource/barlow-condensed/600.css";
import "../src/theme/tokens.css";

const preview: Preview = {
  parameters: {
    controls: { matchers: { color: /(background|color)$/i, date: /Date$/i } },
    backgrounds: {
      default: "industry",
      values: [{ name: "industry", value: "#f2f2f3" }],
    },
    a11y: {
      test: "error",
      // axe skips colour contrast on single-character and non-BMP text by default (reported
      // as "incomplete", which never fails the gate), e.g. a lone digit or letter. Check it.
      // `options` replaces the check's defaults wholesale, so this restates axe-core 4.13's
      // color-contrast defaults and flips only `ignoreLength` and `ignoreUnicode`. Omitting
      // the rest switches the contrast check off. Known gap: axe's rule matcher drops
      // punctuation-only text (a lone "—") before any check runs, so those placeholders are
      // kept on an AA colour by hand.
      config: {
        checks: [
          {
            id: "color-contrast",
            options: {
              ignoreUnicode: false,
              ignoreLength: true,
              ignorePseudo: false,
              boldValue: 700,
              boldTextPt: 14,
              largeTextPt: 18,
              contrastRatio: { normal: { expected: 4.5 }, large: { expected: 3 } },
              pseudoSizeThreshold: 0.25,
              shadowOutlineEmMax: 0.2,
              textStrokeEmMin: 0.03,
            },
          },
        ],
      },
    },
  },
};

export default preview;
