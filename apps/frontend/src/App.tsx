import { useMemo, useRef, useState } from "react";
import { Provider } from "urql";
import { RiskInbox, type CommandOutcome, type Notice } from "./screens/RiskInbox.tsx";
import { ImpactTab } from "./screens/ImpactTab.tsx";
import { createGraphqlClient } from "./data/client.ts";
import { createCommands, type CommandResult, type Commands } from "./data/commands.ts";
import { useEntityLabels, useInboxData } from "./data/hooks.ts";
import type { PlanEntry } from "./viewmodel/types.ts";
import { planLabel } from "./viewmodel/wording.ts";
import { systemClock, type Clock } from "./viewmodel/clock.ts";

/** The one point a single-point plan resolves, which is the point commands' subject. */
function soleSubject(entry: PlanEntry): string {
  return entry.resolves[0]?.subject ?? "";
}

/** The notice after a failed write-back: the backend may have changed the source record
 *  partly, so the outcome is stated as uncertain, not as refused or done. */
export const UNCERTAIN_OUTCOME = "Outcome uncertain: the write-back partly failed; refreshed. Check the source record.";

/** How the operator reads a command's result. A 502 (failed write-back), a 504 or no
 *  response at all, where the backend may still have written, is an uncertain outcome. Any
 *  other refusal (409 / 422) wrote nothing and carries its reason. */
function noticeFor(result: CommandResult, done: string): Notice {
  if (result.ok) return { kind: "info", message: done };
  if (result.status === 502) return { kind: "error", message: UNCERTAIN_OUTCOME };
  if (result.status === 504 || result.status === null) {
    return { kind: "error", message: `Outcome uncertain: ${result.reason}; refreshed. Check the source record.` };
  }
  return { kind: "error", message: result.reason };
}

/** The live Risk-inbox container: reads via the data hooks, writes via the command client,
 *  and re-fetches the reads after every command result (success, refusal or failure), so it
 *  does not keep showing a state the command may have changed. A portfolio's joint plan is
 *  approved, dismissed and undone through the portfolio endpoints, as one command carrying the
 *  portfolio and the generation the view shows. A single point's plan goes through the point
 *  endpoints. At most one command per plan is in flight. `commands` is injectable for tests. */
export function Inbox({ commands, clock = systemClock }: { commands?: Commands; clock?: Clock }) {
  const { status, items, autoApplied, approved, policy, kpis, initialError, refreshError, refetch } = useInboxData(clock);
  const client = useMemo(() => commands ?? createCommands(), [commands]);
  const [notice, setNotice] = useState<Notice | null>(null);
  // The ref is the synchronous guard (two clicks in one frame); the state drives `busy`.
  const inFlightRef = useRef(new Set<string>());
  const [inFlight, setInFlight] = useState<ReadonlySet<string>>(() => new Set());
  // The entity ids the screen shows, and the labels the read serves for them.
  const [shownIds, setShownIds] = useState<string[]>([]);
  const labels = useEntityLabels(shownIds);

  async function run(key: string, issue: () => Promise<CommandResult>, done: string): Promise<CommandOutcome> {
    if (inFlightRef.current.has(key)) return { ok: false };
    inFlightRef.current.add(key);
    setInFlight(new Set(inFlightRef.current));
    try {
      const result = await issue();
      setNotice(noticeFor(result, done));
      await refetch();
      return { ok: result.ok };
    } finally {
      inFlightRef.current.delete(key);
      setInFlight(new Set(inFlightRef.current));
    }
  }

  if (status === "error") {
    return (
      <main style={{ padding: 24 }}>
        <h1>Plant Pulse — Risk inbox</h1>
        <p role="alert" className="text-muted">
          Could not reach the plant-pulse-api read surface: {initialError}
        </p>
      </main>
    );
  }

  if (status === "loading") {
    return (
      <main style={{ padding: 24 }}>
        <h1>Plant Pulse — Risk inbox</h1>
        <p className="text-muted">Loading the inbox…</p>
      </main>
    );
  }

  return (
    <RiskInbox
      items={items}
      autoApplied={autoApplied}
      approved={approved}
      policy={policy}
      kpis={kpis}
      notice={notice}
      refreshError={refreshError}
      inFlight={inFlight}
      defaultReviewer={import.meta.env.VITE_EKG_REVIEWER}
      clock={clock}
      labels={labels}
      onShownIds={setShownIds}
      renderImpactTab={(tab, subject, selection) => <ImpactTab subject={subject} tab={tab} selection={selection} clock={clock} />}
      onApprove={(entry, option, rationale, reviewer) => {
        const context = `plan ${planLabel(entry.code)} (${entry.id}) option ${option.key}: ${option.label}`;
        // The promotion commits the option the operator selected, identified by its unique
        // rank_key. The backend validates it against the current slate and refuses a stale or
        // unknown one (409), which the notice shows after a refresh.
        return run(
          entry.id,
          () =>
            entry.portfolio
              ? client.promotePortfolio({
                  portfolio: entry.portfolio.id,
                  generation: entry.portfolio.generation,
                  option_rank_key: option.rankKey,
                  reviewer,
                  rationale,
                  context_seen: context,
                })
              : client.promote({ subject: soleSubject(entry), reviewer, rationale, context_seen: context, option_rank_key: option.rankKey }),
          `Approved plan ${planLabel(entry.code)}.`,
        );
      }}
      onDismiss={(entry, reason, reviewer) =>
        run(
          entry.id,
          () =>
            entry.portfolio
              ? client.abortPortfolio({ portfolio: entry.portfolio.id, generation: entry.portfolio.generation, reviewer, reason })
              : client.abort({ subject: soleSubject(entry), reviewer, reason }),
          `Dismissed plan ${planLabel(entry.code)}.`,
        )
      }
      onUndo={(applied, reason, reviewer) =>
        run(applied.subject, () => client.undo({ subject: applied.subject, reviewer, reason }), `Undid ${applied.subject}.`)
      }
      onUndoPlan={(plan, reason, reviewer) =>
        run(
          plan.id,
          () =>
            plan.portfolio
              ? client.undoPortfolio({ portfolio: plan.portfolio.id, reviewer, reason })
              : client.undo({ subject: plan.resolves[0]?.subject ?? "", reviewer, reason }),
          `Undid plan ${planLabel(plan.code)}.`,
        )
      }
    />
  );
}

export function App() {
  const client = useMemo(() => createGraphqlClient(), []);
  return (
    <Provider value={client}>
      <Inbox />
    </Provider>
  );
}
