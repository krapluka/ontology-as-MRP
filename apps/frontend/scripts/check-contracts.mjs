// Checks that the committed typed clients are what the committed contract snapshots generate.
// The backend's `contract_snapshots` test proves the snapshots match the code and this proves
// the clients match the snapshots, so a contract change cannot ship with a stale client.
//
// Hashes the generated files, regenerates them offline from `schema.graphql` and
// `openapi.json`, and fails if anything changed (leaving the regenerated files in place to
// review and commit). It compares against the working tree, not git, so it works on
// uncommitted edits too.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const graphqlDir = join(root, "src/data/graphql");

function generatedFiles() {
  const clients = readdirSync(graphqlDir).map((name) => join(graphqlDir, name));
  return [...clients, join(root, "src/data/openapi.d.ts")].sort();
}

function snapshot() {
  return new Map(
    generatedFiles().map((file) => [file, createHash("sha256").update(readFileSync(file)).digest("hex")]),
  );
}

const before = snapshot();
try {
  // stderr passes through, so a codegen validation error (e.g. an operation selecting a field
  // the updated schema removed) or a missing generator is shown, not swallowed.
  execFileSync("node", [join(root, "scripts/regen-contracts.mjs")], {
    cwd: root,
    stdio: ["ignore", "ignore", "inherit"],
  });
} catch {
  console.error("✗ regenerating the typed clients failed (see the generator output above). Fix it, then `just frontend-contracts`.");
  process.exit(1);
}
const after = snapshot();

const stale = [...new Set([...before.keys(), ...after.keys()])].filter((file) => before.get(file) !== after.get(file));
if (stale.length > 0) {
  console.error("✗ the typed clients are stale against the committed contract snapshots:");
  for (const file of stale) console.error(`  ${relative(root, file)}`);
  console.error("They have been regenerated in place: review and commit them (`just frontend-contracts`).");
  process.exit(1);
}
console.log("✓ typed clients match the committed contract snapshots");
