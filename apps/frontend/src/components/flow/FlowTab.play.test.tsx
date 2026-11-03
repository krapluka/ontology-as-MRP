import { describe, test } from "vitest";
import { render } from "@testing-library/react";
import { composeStories } from "@storybook/react-vite";
import * as stories from "./FlowTab.stories.tsx";

// Runs the Flow story play functions headlessly: each renders the recorded live projection
// and asserts one highlighted tile per stage, the chain, and the unserved-path rendering.
const { Default, PathNotServed, SelectPromisedTile, Empty } = composeStories(stories);

describe("FlowTab", () => {
  test("a promised tile of another listed point selects that point", async () => {
    const { container } = render(<SelectPromisedTile />);
    await SelectPromisedTile.play?.({ canvasElement: container, args: SelectPromisedTile.args });
  });

  test("highlights one on-path tile per stage and draws the chain", async () => {
    const { container } = render(<Default />);
    await Default.play?.({ canvasElement: container });
  });

  test("says path not served on stages without on_path and skips them in the chain", async () => {
    const { container } = render(<PathNotServed />);
    await PathNotServed.play?.({ canvasElement: container });
  });

  test("renders the empty state", async () => {
    const { container } = render(<Empty />);
    await Empty.play?.({ canvasElement: container });
  });
});
