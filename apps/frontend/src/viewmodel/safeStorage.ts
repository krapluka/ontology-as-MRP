/**
 * Per-browser storage for the screen's conveniences (the last selection, the open tab, the
 * reasoning toggle, the reviewer identity). Browser storage can be unavailable (blocked site
 * data, a private window, a quota error, or a sandbox where merely reading `localStorage`
 * throws) and none of that may crash the inbox. Every access goes through here: a failed
 * read is "nothing stored" (`null`), a failed write is dropped.
 */
export const safeStorage = {
  get(key: string): string | null {
    try {
      return globalThis.localStorage.getItem(key);
    } catch {
      return null;
    }
  },
  set(key: string, value: string): void {
    try {
      globalThis.localStorage.setItem(key, value);
    } catch {
      // Storage unavailable: the value is not remembered.
    }
  },
  /** Forget everything stored (stories reset between runs through this). */
  clear(): void {
    try {
      globalThis.localStorage.clear();
    } catch {
      // Storage unavailable: nothing is stored to forget.
    }
  },
};
