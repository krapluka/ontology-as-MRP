import { describe, test } from "vitest";
import { render } from "@testing-library/react";
import { composeStories } from "@storybook/react-vite";
import * as stories from "./GraphTab.stories.tsx";

// Runs the Graph story play functions headlessly: each renders the recorded live projection
// and asserts the lit path, the dimmed context, the cause fill and the status messages.
const { Default, ClearedRule, ClearedNotAnOpenPoint, RiskNotServed, NoOpenPurchaseOrders, Empty } = composeStories(stories);

describe("GraphTab", () => {
  test("lights the on-path chain and edges, fills causes, dims the context", async () => {
    const { container } = render(<Default />);
    await Default.play?.({ canvasElement: container });
  });

  test("says the rule no longer flags an open point's cleared order", async () => {
    const { container } = render(<ClearedRule />);
    await ClearedRule.play?.({ canvasElement: container });
  });

  test("never calls a cleared order open for a decision when the inbox does not list its point", async () => {
    const { container } = render(<ClearedNotAnOpenPoint />);
    await ClearedNotAnOpenPoint.play?.({ canvasElement: container });
  });

  test("says risk status and path are not served when the rules cannot be read", async () => {
    const { container } = render(<RiskNotServed />);
    await RiskNotServed.play?.({ canvasElement: container });
  });

  test("tells an empty column apart from an unserved path", async () => {
    const { container } = render(<NoOpenPurchaseOrders />);
    await NoOpenPurchaseOrders.play?.({ canvasElement: container });
  });

  test("renders the empty state", async () => {
    const { container } = render(<Empty />);
    await Empty.play?.({ canvasElement: container });
  });
});
