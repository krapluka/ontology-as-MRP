import "@testing-library/dom";
import { cleanup } from "@testing-library/react";
import { setProjectAnnotations } from "@storybook/react-vite";
import { afterEach, beforeAll } from "vitest";
import previewAnnotations from "../../.storybook/preview.ts";

// Node 25 injects a non-functional global `localStorage` (the `--localstorage-file`
// warning) that shadows jsdom's. Install a clean in-memory Storage so the screen's
// getItem/setItem and the tests' clear() behave deterministically.
class MemoryStorage implements Storage {
  private map = new Map<string, string>();
  get length() {
    return this.map.size;
  }
  clear() {
    this.map.clear();
  }
  getItem(key: string) {
    return this.map.get(key) ?? null;
  }
  key(index: number) {
    return [...this.map.keys()][index] ?? null;
  }
  removeItem(key: string) {
    this.map.delete(key);
  }
  setItem(key: string, value: string) {
    this.map.set(key, String(value));
  }
}

const storage = new MemoryStorage();
Object.defineProperty(globalThis, "localStorage", { value: storage, configurable: true });
Object.defineProperty(window, "localStorage", { value: storage, configurable: true });

// Apply the Storybook preview annotations so portable stories (composeStories) render
// with the same globals/parameters as the workshop.
const project = setProjectAnnotations([previewAnnotations]);
beforeAll(project.beforeAll);

afterEach(() => cleanup());
