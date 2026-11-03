// Regenerate the two typed clients from the committed contract snapshots.
//
// The snapshots (`schema.graphql`, `openapi.json`) are exported from the backend's code by
// `just frontend-contracts` (the `export-contracts` binary), never transcribed by hand or
// introspected from a live backend. This script only runs the offline generation over
// them: graphql-codegen from `schema.graphql` and the inbox operations, and
// openapi-typescript from `openapi.json`. It is deterministic, so re-running reproduces
// the committed output.

import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const run = (cmd, args) => execFileSync(cmd, args, { cwd: root, stdio: "inherit" });

// `--no`: use the pinned local binaries only. npx must fail when node_modules is missing
// instead of fetching a same-named package from the registry.
run("npx", ["--no", "graphql-codegen", "--config", "codegen.ts"]);
run("npx", ["--no", "openapi-typescript", "openapi.json", "-o", "src/data/openapi.d.ts"]);

console.log("contracts regenerated.");
