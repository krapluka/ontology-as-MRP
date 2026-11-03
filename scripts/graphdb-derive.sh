#!/usr/bin/env bash
#
# Run the structural process-graph derivations over the settled `ekg` world model:
# ekg:precedes (operation ordering) and the non-direct process edges (ekg:used,
# ekg:fulfills, ekg:capacity). Deterministic and idempotent, so it can be re-run after
# each materialization/reconciliation pass. Not a monitor and not model-driven: a
# capability-layer derivation over governed edges.
#
# The ingest path runs these after every materialization and retraction; this script is a
# manual convenience.
#
# Requires the substrate up (`just podman-local-up`) and the world model materialized.
# Override target with GRAPHDB_URL (default http://localhost:7200).
#
#   just graphdb-derive

set -euo pipefail

GRAPHDB_URL="${GRAPHDB_URL:-http://localhost:7200}"
REPO_ID="ekg"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ONTO_DIR="${ROOT}/crates/ekg-semantic/ontology"

run_update() {
    local file="$1"
    printf 'deriving via %s\n' "$(basename "${file}")"
    curl -fsS -X POST "${GRAPHDB_URL}/repositories/${REPO_ID}/statements" \
        -H 'Content-Type: application/sparql-update' \
        --data-binary "@${file}" >/dev/null
}

run_update "${ONTO_DIR}/derive-precedes.rq"
run_update "${ONTO_DIR}/derive-process-edges.rq"

echo "✓ process-graph edges derived (precedes, used, fulfills, capacity)"
