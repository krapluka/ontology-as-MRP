import { describe, expect, test } from "vitest";
import { checkA11yContract } from "../../.storybook/a11yContract.ts";
import type { A11yContractInput, A11yReportLike } from "../../.storybook/a11yContract.ts";

// Every rule of the a11y gate's contract, pinned with made-up inputs, so loosening one of
// them fails here.

const passed: A11yReportLike = { type: "a11y", status: "passed", result: { violations: [], incomplete: [] } };

function input(overrides: Partial<A11yContractInput> = {}): A11yContractInput {
  return { id: "story--x", reports: [passed], parameters: {}, globals: {}, storyGlobals: {}, storybookUi: false, ...overrides };
}

describe("checkA11yContract", () => {
  test("a story whose a11y check ran and passed yields nothing", () => {
    expect(checkA11yContract(input())).toEqual([]);
  });

  test("no a11y report (off / disable / manual) throws in the CLI gate", () => {
    expect(() => checkA11yContract(input({ reports: [] }))).toThrow(/a11y check did not run/);
    expect(() => checkA11yContract(input({ reports: [], globals: { a11y: { manual: true } } }))).toThrow(
      /a11y check did not run/,
    );
  });

  test("a non-a11y report does not count as the a11y check having run", () => {
    expect(() =>
      checkA11yContract(input({ reports: [{ type: "interactions", status: "passed", result: {} }] })),
    ).toThrow(/a11y check did not run/);
  });

  test("Storybook's UI run with Accessibility off is not enforced", () => {
    expect(checkA11yContract(input({ reports: [], globals: { a11y: { manual: true } }, storybookUi: true }))).toEqual([]);
  });

  test("a UI run where the story itself sets manual is never excused", () => {
    const manual = { a11y: { manual: true } };
    expect(() =>
      checkA11yContract(input({ reports: [], globals: manual, storyGlobals: manual, storybookUi: true })),
    ).toThrow(/a11y check did not run/);
  });

  test("a UI run with Accessibility on is enforced like the gate", () => {
    expect(() => checkA11yContract(input({ reports: [], globals: { a11y: { manual: false } }, storybookUi: true }))).toThrow(
      /a11y check did not run/,
    );
  });

  test("an errored rule (incomplete + error) throws", () => {
    const errored: A11yReportLike = {
      type: "a11y",
      status: "passed",
      result: { violations: [], incomplete: [{ id: "color-contrast", help: "", nodes: [], error: { message: "boom" } }] },
    };
    expect(() => checkA11yContract(input({ reports: [errored] }))).toThrow(/errored instead of running \(color-contrast\)/);
  });

  test("an incomplete rule without an error is not treated as errored", () => {
    const incomplete: A11yReportLike = {
      type: "a11y",
      status: "passed",
      result: { violations: [], incomplete: [{ id: "color-contrast", help: "", nodes: [] }] },
    };
    expect(checkA11yContract(input({ reports: [incomplete] }))).toEqual([]);
  });

  test("the canary must be caught as color-contrast", () => {
    const caught: A11yReportLike = {
      type: "a11y",
      status: "warning",
      result: { violations: [{ id: "color-contrast", help: "contrast", nodes: [{}] }], incomplete: [] },
    };
    expect(checkA11yContract(input({ reports: [caught], parameters: { a11yCanary: true } }))).toEqual([]);
    expect(() => checkA11yContract(input({ parameters: { a11yCanary: true } }))).toThrow(/canary was not caught/);
    const unrelated: A11yReportLike = {
      type: "a11y",
      status: "warning",
      result: { violations: [{ id: "image-alt", help: "alt text", nodes: [{}] }], incomplete: [] },
    };
    expect(() => checkA11yContract(input({ reports: [unrelated], parameters: { a11yCanary: true } }))).toThrow(
      /canary was not caught/,
    );
  });

  test("a todo (warning) report is printed, not thrown", () => {
    const warning: A11yReportLike = {
      type: "a11y",
      status: "warning",
      result: { violations: [{ id: "button-name", help: "Buttons must have discernible text", nodes: [{}] }], incomplete: [] },
    };
    expect(checkA11yContract(input({ reports: [warning] }))).toEqual([
      "[a11y todo] story--x: button-name: Buttons must have discernible text (1 element(s))",
    ]);
  });
});
