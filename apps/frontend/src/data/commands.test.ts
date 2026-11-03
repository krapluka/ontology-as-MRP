import { describe, expect, test, vi } from "vitest";
import { createCommands } from "./commands.ts";

function okFetch() {
  return vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => new Response(null, { status: 200 }));
}

function problemFetch(status: number, detail: string) {
  return vi.fn(
    async (_url: RequestInfo | URL, _init?: RequestInit) =>
      new Response(JSON.stringify({ type: "about:blank", title: "Conflict", status, detail }), {
        status,
        headers: { "content-type": "application/problem+json" },
      }),
  );
}

describe("governed commands", () => {
  test("promote posts the reviewer + rationale + selected option to the promote endpoint", async () => {
    const fetchFn = okFetch();
    const commands = createCommands({ baseUrl: "http://api", fetchFn });
    const result = await commands.promote({
      subject: "s1",
      reviewer: "Dana Ops",
      rationale: "worth it",
      option_rank_key: "00001",
    });

    expect(result).toEqual({ ok: true, status: 200 });
    const [url, init] = fetchFn.mock.calls[0]!;
    expect(url).toBe("http://api/api/action-points/promote");
    expect(init?.method).toBe("POST");
    // The selected slate option's rank_key rides the command so the backend commits that
    // option's plan (not a default or the staged proposal).
    expect(JSON.parse(init?.body as string)).toMatchObject({
      subject: "s1",
      reviewer: "Dana Ops",
      rationale: "worth it",
      option_rank_key: "00001",
    });
  });

  test("promote surfaces a 409 refusal (stale/unknown option) without throwing", async () => {
    const fetchFn = problemFetch(409, "option_rank_key \"00009\" is not in the point's current slate");
    const commands = createCommands({ fetchFn });
    const result = await commands.promote({
      subject: "s1",
      reviewer: "Dana Ops",
      rationale: "x",
      option_rank_key: "00009",
    });
    expect(result).toEqual({ ok: false, status: 409, reason: 'option_rank_key "00009" is not in the point\'s current slate' });
  });

  test("abort posts the reviewer and the operator's reason to the abort endpoint", async () => {
    const fetchFn = okFetch();
    const commands = createCommands({ fetchFn });
    await commands.abort({ subject: "s1", reviewer: "Dana Ops", reason: "supplier confirmed by phone" });
    const [url, init] = fetchFn.mock.calls[0]!;
    expect(url).toBe("/api/action-points/abort");
    expect(JSON.parse(init?.body as string)).toEqual({ subject: "s1", reviewer: "Dana Ops", reason: "supplier confirmed by phone" });
  });

  test("undo posts the reviewer and the operator's reason to the undo endpoint", async () => {
    const fetchFn = okFetch();
    const commands = createCommands({ fetchFn });
    await commands.undo({ subject: "s1", reviewer: "Dana Ops", reason: "customer cancelled" });
    const [url, init] = fetchFn.mock.calls[0]!;
    expect(url).toBe("/api/action-points/undo");
    expect(JSON.parse(init?.body as string)).toEqual({ subject: "s1", reviewer: "Dana Ops", reason: "customer cancelled" });
  });

  test("abort's contracted 204 (no body) is a success", async () => {
    // The generated OpenAPI contract documents abort as 204 No Content, so a success must
    // not depend on reading a response body.
    const fetchFn = vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => new Response(null, { status: 204 }));
    const commands = createCommands({ fetchFn });
    const result = await commands.abort({ subject: "s1", reviewer: "Dana Ops", reason: "not needed" });
    expect(result).toEqual({ ok: true, status: 204 });
  });

  test("undo surfaces a refusal reason without throwing", async () => {
    const fetchFn = problemFetch(409, "undo requested after its reversal window has closed");
    const commands = createCommands({ fetchFn });
    const result = await commands.undo({ subject: "s1", reviewer: "Dana Ops", reason: "customer cancelled" });
    expect(result).toEqual({ ok: false, status: 409, reason: "undo requested after its reversal window has closed" });
  });

  test("a 422 validation refusal passes its status and detail through", async () => {
    const fetchFn = problemFetch(422, "reason must not be empty");
    const commands = createCommands({ fetchFn });
    const result = await commands.abort({ subject: "s1", reviewer: "Dana Ops", reason: " " });
    expect(result).toEqual({ ok: false, status: 422, reason: "reason must not be empty" });
  });

  test("a 502 failed write-back passes its status through, so the view can report it as uncertain", async () => {
    const fetchFn = problemFetch(502, "write-back partly applied");
    const commands = createCommands({ fetchFn });
    const result = await commands.promote({ subject: "s1", reviewer: "Dana Ops", rationale: "x", option_rank_key: "k" });
    expect(result).toEqual({ ok: false, status: 502, reason: "write-back partly applied" });
  });

  test("a network error becomes a refusal reason, not an exception", async () => {
    const fetchFn = vi.fn(async () => {
      throw new Error("connection refused");
    });
    const commands = createCommands({ fetchFn });
    const result = await commands.promote({ subject: "s1", reviewer: "Dana Ops", rationale: "x" });
    expect(result).toEqual({ ok: false, status: null, reason: "connection refused" });
  });

  test("a portfolio's plan is promoted, dismissed and undone through the portfolio endpoints, carrying the generation", async () => {
    const fetchFn = okFetch();
    const commands = createCommands({ fetchFn });
    await commands.promotePortfolio({
      portfolio: "portfolio/0412e7c1d9a3",
      generation: 2,
      option_rank_key: "00000~abc",
      reviewer: "Dana Ops",
      rationale: "split and expedite",
      context_seen: "plan PL-0412",
    });
    await commands.abortPortfolio({ portfolio: "portfolio/0412e7c1d9a3", generation: 2, reviewer: "Dana Ops", reason: "not needed" });
    await commands.undoPortfolio({ portfolio: "portfolio/0412e7c1d9a3", reviewer: "Dana Ops", reason: "supplier fell through" });
    const calls = fetchFn.mock.calls.map(([url, init]) => [url, JSON.parse(init?.body as string)]);
    expect(calls).toEqual([
      [
        "/api/portfolios/promote",
        {
          portfolio: "portfolio/0412e7c1d9a3",
          generation: 2,
          option_rank_key: "00000~abc",
          reviewer: "Dana Ops",
          rationale: "split and expedite",
          context_seen: "plan PL-0412",
        },
      ],
      ["/api/portfolios/abort", { portfolio: "portfolio/0412e7c1d9a3", generation: 2, reviewer: "Dana Ops", reason: "not needed" }],
      ["/api/portfolios/undo", { portfolio: "portfolio/0412e7c1d9a3", reviewer: "Dana Ops", reason: "supplier fell through" }],
    ]);
  });

  test("the portfolio commands share the refusal handling: 409, 422, 502 and a network error", async () => {
    const stale = await createCommands({ fetchFn: problemFetch(409, "generation 1 is stale") }).abortPortfolio({
      portfolio: "p",
      generation: 1,
      reviewer: "Dana Ops",
      reason: "x",
    });
    expect(stale).toEqual({ ok: false, status: 409, reason: "generation 1 is stale" });
    const invalid = await createCommands({ fetchFn: problemFetch(422, "reason must be non-empty") }).undoPortfolio({ portfolio: "p", reviewer: "Dana Ops", reason: " " });
    expect(invalid).toEqual({ ok: false, status: 422, reason: "reason must be non-empty" });
    const held = await createCommands({ fetchFn: problemFetch(502, "a member's abort did not record") }).abortPortfolio({
      portfolio: "p",
      generation: 1,
      reviewer: "Dana Ops",
      reason: "x",
    });
    expect(held).toEqual({ ok: false, status: 502, reason: "a member's abort did not record" });
    const down = await createCommands({
      fetchFn: vi.fn(async () => {
        throw new Error("connection refused");
      }),
    }).promotePortfolio({ portfolio: "p", generation: 1, option_rank_key: "k", reviewer: "Dana Ops", rationale: "x" });
    expect(down).toEqual({ ok: false, status: null, reason: "connection refused" });
  });
});
