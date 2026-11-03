import { describe, test } from "vitest";
import { render } from "@testing-library/react";
import { composeStories } from "@storybook/react-vite";
import * as stories from "./SuppliersTab.stories.tsx";

// Runs the Suppliers story play functions headlessly: each renders the recorded live
// projection and asserts the three default selections, the demand marker, and the handling
// of unserved values.
const { Default, ClearedRuleTopExposure, PathNotServed, ExposurePartlyServed, SelectSupplier, SelectDependentOrder, NoDemandServed, Empty } =
  composeStories(stories);

describe("SuppliersTab", () => {
  test("opens on the first on-path supplier and draws the demand marker", async () => {
    const { container } = render(<Default />);
    await Default.play?.({ canvasElement: container });
  });

  test("opens on the top-exposure supplier when none is on the served path", async () => {
    const { container } = render(<ClearedRuleTopExposure />);
    await ClearedRuleTopExposure.play?.({ canvasElement: container });
  });

  test("opens on the first served supplier with a path-not-served note when on_path is unserved", async () => {
    const { container } = render(<PathNotServed />);
    await PathNotServed.play?.({ canvasElement: container });
  });

  test("a dependent order of another listed point selects that point", async () => {
    const { container } = render(<SelectDependentOrder />);
    await SelectDependentOrder.play?.({ canvasElement: container, args: SelectDependentOrder.args });
  });

  test("shows an omitted exposure as not served and never ranks it below a known 0", async () => {
    const { container } = render(<ExposurePartlyServed />);
    await ExposurePartlyServed.play?.({ canvasElement: container });
  });

  test("selecting a supplier swaps its detail", async () => {
    const { container } = render(<SelectSupplier />);
    await SelectSupplier.play?.({ canvasElement: container });
  });

  test("draws no demand marker when demand is not served", async () => {
    const { container } = render(<NoDemandServed />);
    await NoDemandServed.play?.({ canvasElement: container });
  });

  test("renders the empty state", async () => {
    const { container } = render(<Empty />);
    await Empty.play?.({ canvasElement: container });
  });
});
