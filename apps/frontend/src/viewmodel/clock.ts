import { useEffect, useState } from "react";

/** A source of the current time plus a periodic tick. The live screen uses the system clock;
 *  stories and tests inject a fake one so a time-bound control (the undo window) can be
 *  driven deterministically. */
export type Clock = {
  /** The current time, epoch milliseconds. */
  now(): number;
  /** Call `tick` every `intervalMs`; returns the cancel function. */
  every(intervalMs: number, tick: () => void): () => void;
};

export const systemClock: Clock = {
  now: () => Date.now(),
  every: (intervalMs, tick) => {
    const id = setInterval(tick, intervalMs);
    return () => clearInterval(id);
  },
};

/** The current time (epoch ms), re-read every `intervalMs`, so a view that judges data
 *  against "now" (an undo window, "today") stays right while it stays open. */
export function useNow(intervalMs: number, clock: Clock = systemClock): number {
  const [now, setNow] = useState(() => clock.now());
  useEffect(() => {
    setNow(clock.now());
    return clock.every(intervalMs, () => setNow(clock.now()));
  }, [clock, intervalMs]);
  return now;
}
