/**
 * The command client: the only path by which the view mutates state. Approve calls promote,
 * dismiss calls abort and undo calls undo, each a typed POST against the REST command
 * endpoints carrying the operator's reviewer identity and the rationale (promotion) or reason
 * (dismiss / undo). A single point's plan uses the point endpoints; a portfolio's joint plan
 * uses the portfolio endpoints, carrying the portfolio and the generation the view shows, as
 * one command. A refused command returns its backend reason together with the HTTP status, so
 * the view can show the reason and tell a refusal (409 / 422: nothing written) from a
 * failed write-back (502: the outcome is uncertain).
 */
import type { components } from "./openapi.d.ts";

type PromoteInput = components["schemas"]["PromoteInput"];
type AbortInput = components["schemas"]["AbortInput"];
type UndoInput = components["schemas"]["UndoInput"];
type PortfolioPromoteInput = components["schemas"]["PortfolioPromoteInput"];
type PortfolioAbortInput = components["schemas"]["PortfolioAbortInput"];
type PortfolioUndoInput = components["schemas"]["PortfolioUndoInput"];
type Problem = components["schemas"]["Problem"];

/** A command's result. `status` is the HTTP status, or null when no response arrived (a
 *  network error: the request may or may not have reached the backend). */
export type CommandResult = { ok: true; status: number } | { ok: false; status: number | null; reason: string };

export type Commands = {
  promote(input: PromoteInput): Promise<CommandResult>;
  abort(input: AbortInput): Promise<CommandResult>;
  undo(input: UndoInput): Promise<CommandResult>;
  promotePortfolio(input: PortfolioPromoteInput): Promise<CommandResult>;
  abortPortfolio(input: PortfolioAbortInput): Promise<CommandResult>;
  undoPortfolio(input: PortfolioUndoInput): Promise<CommandResult>;
};

function isProblem(value: unknown): value is Problem {
  return typeof value === "object" && value !== null && "detail" in value;
}

export function createCommands(opts?: { baseUrl?: string; fetchFn?: typeof fetch }): Commands {
  const base = (opts?.baseUrl ?? "").replace(/\/$/, "");
  const doFetch = opts?.fetchFn ?? globalThis.fetch.bind(globalThis);

  async function post(path: string, body: unknown): Promise<CommandResult> {
    let res: Response;
    try {
      res = await doFetch(`${base}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    } catch (err) {
      return { ok: false, status: null, reason: err instanceof Error ? err.message : "network error" };
    }
    if (res.ok) return { ok: true, status: res.status };
    let reason = `Command refused (${res.status})`;
    try {
      const problem: unknown = await res.json();
      if (isProblem(problem)) reason = problem.detail || problem.title || reason;
    } catch {
      // no body or not JSON: keep the status-based reason.
    }
    return { ok: false, status: res.status, reason };
  }

  return {
    promote: (input) => post("/api/action-points/promote", input),
    abort: (input) => post("/api/action-points/abort", input),
    undo: (input) => post("/api/action-points/undo", input),
    promotePortfolio: (input) => post("/api/portfolios/promote", input),
    abortPortfolio: (input) => post("/api/portfolios/abort", input),
    undoPortfolio: (input) => post("/api/portfolios/undo", input),
  };
}
