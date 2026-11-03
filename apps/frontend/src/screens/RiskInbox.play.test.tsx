import { beforeEach, describe, test } from "vitest";
import { render } from "@testing-library/react";
import { composeStories } from "@storybook/react-vite";
import * as stories from "./RiskInbox.stories.tsx";

// Runs the Storybook play functions headlessly (portable stories under vitest/jsdom, no
// browser). Each story renders from fixtures and its play drives the interaction.
const {
  Default,
  Empty,
  DisplayFraming,
  AtRiskPlan,
  SelectPlan,
  PickOption,
  ToggleReasoning,
  ToggleLanes,
  ApproveWithRationale,
  ApproveAfterSwitchingOption,
  RationaleResetsOnPlanSwitch,
  SwitchTabs,
  SelectEntityOnTab,
  RationaleDoesNotFollowRefetch,
  ApproveRefusedKeepsRationale,
  SelectionFollowsReRank,
  SelectionLost,
  NoOptionPlanOpens,
  SlatePendingRow,
  PlanUnavailableRow,
  OptionReasoningShown,
  DismissAsksForReason,
  UndoAsksForReason,
  NoReviewerNoCommands,
  UndoWindowCloses,
  AppliedTodayOnly,
  StaleAfterFailedRefresh,
  ApprovePortfolioPlan,
  LonePlanKeepsPointCommands,
  ApproveWaitsForMember,
  PendingPortfolioListed,
  PlanCopy,
  PlanNames,
  HeaderClock,
  ApprovedUndo,
  ApprovedPartlyUndone,
  ApprovedUndoWindowCloses,
  PortfolioMemberOnTab,
  SupplierOpensThePlan,
  SupplierWithoutPlan,
} = composeStories(stories);

beforeEach(() => localStorage.clear());

describe("RiskInbox interactions", () => {
  test("renders the default screen from fixtures", async () => {
    const { container } = render(<Default />);
    await Default.play?.({ canvasElement: container });
  });

  test("renders the empty state", async () => {
    const { container } = render(<Empty />);
    await Empty.play?.({ canvasElement: container });
  });

  test("renders the sourced display framing (KPIs, severity, from→to)", async () => {
    const { container } = render(<DisplayFraming />);
    await DisplayFraming.play?.({ canvasElement: container });
  });

  test("an at-risk plan never reads on time", async () => {
    const { container } = render(<AtRiskPlan />);
    await AtRiskPlan.play?.({ canvasElement: container });
  });

  test("selecting a plan loads its slate", async () => {
    const { container } = render(<SelectPlan />);
    await SelectPlan.play?.({ canvasElement: container });
  });

  test("picking an option updates the approve label", async () => {
    const { container } = render(<PickOption />);
    await PickOption.play?.({ canvasElement: container });
  });

  test("toggling reasoning reveals the panel", async () => {
    const { container } = render(<ToggleReasoning />);
    await ToggleReasoning.play?.({ canvasElement: container });
  });

  test("toggling lanes flips the segmented control", async () => {
    const { container } = render(<ToggleLanes />);
    await ToggleLanes.play?.({ canvasElement: container });
  });

  test("approving issues the governed command with rationale and the selected option", async () => {
    const { container } = render(<ApproveWithRationale />);
    await ApproveWithRationale.play?.({ canvasElement: container });
  });

  test("switching the option changes which option the promotion carries", async () => {
    const { container } = render(<ApproveAfterSwitchingOption />);
    await ApproveAfterSwitchingOption.play?.({ canvasElement: container });
  });

  test("switching plans clears the rationale (audit-trail invariant)", async () => {
    const { container } = render(<RationaleResetsOnPlanSwitch />);
    await RationaleResetsOnPlanSwitch.play?.({ canvasElement: container });
  });

  test("clicking a tab entity with an open point selects that point and issues no command", async () => {
    const { container } = render(<SelectEntityOnTab />);
    await SelectEntityOnTab.play?.({ canvasElement: container, args: SelectEntityOnTab.args });
  });

  test("switching tabs renders each impact tab for the selected subject", async () => {
    const { container } = render(<SwitchTabs />);
    await SwitchTabs.play?.({ canvasElement: container });
  });

  // State bound to the shown plan, every awaiting row, the command rules.
  test("a rationale does not follow a refetch to another plan", async () => {
    const { container } = render(<RationaleDoesNotFollowRefetch />);
    await RationaleDoesNotFollowRefetch.play?.({ canvasElement: container, args: RationaleDoesNotFollowRefetch.args });
  });

  test("a refused approval keeps the rationale", async () => {
    const { container } = render(<ApproveRefusedKeepsRationale />);
    await ApproveRefusedKeepsRationale.play?.({ canvasElement: container });
  });

  test("the selection follows a re-ranked option by its rankKey", async () => {
    const { container } = render(<SelectionFollowsReRank />);
    await SelectionFollowsReRank.play?.({ canvasElement: container, args: SelectionFollowsReRank.args });
  });

  test("a vanished option is reported as lost and blocks Approve", async () => {
    const { container } = render(<SelectionLost />);
    await SelectionLost.play?.({ canvasElement: container, args: SelectionLost.args });
  });

  test("a plan with no option opens with its baseline and trace, Dismiss and no Approve", async () => {
    const { container } = render(<NoOptionPlanOpens />);
    await NoOptionPlanOpens.play?.({ canvasElement: container });
  });

  test("a slate-pending point is listed and opens to Dismiss", async () => {
    const { container } = render(<SlatePendingRow />);
    await SlatePendingRow.play?.({ canvasElement: container });
  });

  test("a plan-unavailable point is listed and opens to Dismiss", async () => {
    const { container } = render(<PlanUnavailableRow />);
    await PlanUnavailableRow.play?.({ canvasElement: container });
  });

  test("the selected option's own reasoning is shown and follows the pick", async () => {
    const { container } = render(<OptionReasoningShown />);
    await OptionReasoningShown.play?.({ canvasElement: container });
  });

  test("dismiss asks for a reason and sends the typed one", async () => {
    const { container } = render(<DismissAsksForReason />);
    await DismissAsksForReason.play?.({ canvasElement: container, args: DismissAsksForReason.args });
  });

  test("undo asks for a reason and sends the typed one", async () => {
    const { container } = render(<UndoAsksForReason />);
    await UndoAsksForReason.play?.({ canvasElement: container, args: UndoAsksForReason.args });
  });

  test("no reviewer identity disables every command", async () => {
    const { container } = render(<NoReviewerNoCommands />);
    await NoReviewerNoCommands.play?.({ canvasElement: container, args: NoReviewerNoCommands.args });
  });

  test("undo disappears when its window closes (fake clock)", async () => {
    const { container } = render(<UndoWindowCloses />);
    await UndoWindowCloses.play?.({ canvasElement: container });
  });

  test("the list is today plus still-undoable; the KPI counts today only", async () => {
    const { container } = render(<AppliedTodayOnly />);
    await AppliedTodayOnly.play?.({ canvasElement: container });
  });

  test("a failed refresh keeps the inbox under a stale banner", async () => {
    const { container } = render(<StaleAfterFailedRefresh />);
    await StaleAfterFailedRefresh.play?.({ canvasElement: container });
  });

  // Plans as rows, the approved list, the tab seam, the wording.
  test("ApprovePortfolioPlan", async () => {
    const { container } = render(<ApprovePortfolioPlan />);
    await ApprovePortfolioPlan.play?.({ canvasElement: container, args: ApprovePortfolioPlan.args });
  });

  test("LonePlanKeepsPointCommands", async () => {
    const { container } = render(<LonePlanKeepsPointCommands />);
    await LonePlanKeepsPointCommands.play?.({ canvasElement: container, args: LonePlanKeepsPointCommands.args });
  });

  test("ApproveWaitsForMember", async () => {
    const { container } = render(<ApproveWaitsForMember />);
    await ApproveWaitsForMember.play?.({ canvasElement: container, args: ApproveWaitsForMember.args });
  });

  test("PendingPortfolioListed", async () => {
    const { container } = render(<PendingPortfolioListed />);
    await PendingPortfolioListed.play?.({ canvasElement: container, args: PendingPortfolioListed.args });
  });

  test("PlanCopy", async () => {
    const { container } = render(<PlanCopy />);
    await PlanCopy.play?.({ canvasElement: container, args: PlanCopy.args });
  });

  test("PlanNames", async () => {
    const { container } = render(<PlanNames />);
    await PlanNames.play?.({ canvasElement: container, args: PlanNames.args });
  });

  test("HeaderClock", async () => {
    const { container } = render(<HeaderClock />);
    await HeaderClock.play?.({ canvasElement: container, args: HeaderClock.args });
  });

  test("ApprovedUndo", async () => {
    const { container } = render(<ApprovedUndo />);
    await ApprovedUndo.play?.({ canvasElement: container, args: ApprovedUndo.args });
  });

  test("ApprovedPartlyUndone", async () => {
    const { container } = render(<ApprovedPartlyUndone />);
    await ApprovedPartlyUndone.play?.({ canvasElement: container, args: ApprovedPartlyUndone.args });
  });

  test("ApprovedUndoWindowCloses", async () => {
    const { container } = render(<ApprovedUndoWindowCloses />);
    await ApprovedUndoWindowCloses.play?.({ canvasElement: container, args: ApprovedUndoWindowCloses.args });
  });

  test("PortfolioMemberOnTab", async () => {
    const { container } = render(<PortfolioMemberOnTab />);
    await PortfolioMemberOnTab.play?.({ canvasElement: container, args: PortfolioMemberOnTab.args });
  });

  test("SupplierOpensThePlan", async () => {
    const { container } = render(<SupplierOpensThePlan />);
    await SupplierOpensThePlan.play?.({ canvasElement: container, args: SupplierOpensThePlan.args });
  });

  test("SupplierWithoutPlan", async () => {
    const { container } = render(<SupplierWithoutPlan />);
    await SupplierWithoutPlan.play?.({ canvasElement: container, args: SupplierWithoutPlan.args });
  });
});
