import type { Clock } from "../viewmodel/clock.ts";

/** A fake clock for stories and tests (fake timers without a test runner): time only moves
 *  when `advance` is called, and every interval that falls due in the advanced span ticks. */
export type FakeClock = Clock & { advance(ms: number): void };

export function fakeClock(startMs: number): FakeClock {
  let t = startMs;
  const timers = new Set<{ intervalMs: number; next: number; tick: () => void }>();
  return {
    now: () => t,
    every(intervalMs, tick) {
      const timer = { intervalMs, next: t + intervalMs, tick };
      timers.add(timer);
      return () => timers.delete(timer);
    },
    advance(ms) {
      t += ms;
      for (const timer of timers) {
        while (timer.next <= t) {
          timer.next += timer.intervalMs;
          timer.tick();
        }
      }
    },
  };
}

/** A clock that does not move: the fixed "now" of a story that is not about time. */
export function fixedClock(atMs: number): Clock {
  return { now: () => atMs, every: () => () => {} };
}
