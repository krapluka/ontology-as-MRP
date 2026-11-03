#!/usr/bin/env bash
#
# Whole-graph SHACL consistency check, a non-blocking governance signal.
# Validates the settled `ekg` world model against the cross-entity
# consistency-shapes via GraphDB's bulk validate endpoint and prints a
# conformance summary. Never mutates or gates the write path.
#
# Requires the substrate up (`just podman-local-up`). Override target with
# GRAPHDB_URL (default http://localhost:7200).
#
#   just graphdb-validate

set -euo pipefail

GRAPHDB_URL="${GRAPHDB_URL:-http://localhost:7200}"
REPO_ID="ekg"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHAPES="${ROOT}/crates/ekg-semantic/ontology/consistency-shapes.ttl"

report="$(curl -fsS -X POST "${GRAPHDB_URL}/rest/repositories/${REPO_ID}/validate/text" \
    -H 'Content-Type: text/turtle' --data-binary "@${SHAPES}")"

if printf '%s' "${report}" | grep -q 'sh:conforms true'; then
    echo "✓ world model conforms to the cross-entity consistency shapes"
    exit 0
fi

echo "✗ cross-entity consistency violations (non-blocking signal):"
printf '%s\n' "${report}" | grep -E 'sh:focusNode|sh:resultPath|sh:resultMessage' \
    | sed -e 's/^[[:space:]]*/  /'
