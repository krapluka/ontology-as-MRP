import { describe, test } from "vitest";
import { render } from "@testing-library/react";
import { composeStories } from "@storybook/react-vite";
import * as stories from "./TimelineTab.stories.tsx";

// Runs the Timeline story play functions headlessly: each renders the recorded live
// projection and asserts every served order with the on-path one highlighted.
const { Default, LateAndBuffered, SelectOrder, InboundOnly, Empty } = composeStories(stories);

describe("TimelineTab", () => {
  test("renders all served orders with the on-path order highlighted", async () => {
    const { container } = render(<Default />);
    await Default.play?.({ canvasElement: container });
  });

  test("draws a late order's gap and slip and a buffered order's buffer", async () => {
    const { container } = render(<LateAndBuffered />);
    await LateAndBuffered.play?.({ canvasElement: container });
  });

  test("an order row of another listed point selects that point", async () => {
    const { container } = render(<SelectOrder />);
    await SelectOrder.play?.({ canvasElement: container, args: SelectOrder.args });
  });

  test("says no promised orders, not 'not served', when only inbound is served", async () => {
    const { container } = render(<InboundOnly />);
    await InboundOnly.play?.({ canvasElement: container });
  });

  test("renders the empty state", async () => {
    const { container } = render(<Empty />);
    await Empty.play?.({ canvasElement: container });
  });
});
