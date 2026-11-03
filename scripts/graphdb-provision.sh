#!/usr/bin/env bash
#
# Provision the GraphDB `ekg` world-model repository with SHACL validate-on-write
# enabled, and load the supply-risk ontology + SHACL write-shapes.
# Idempotent: the repo is created only if absent, and each named
# graph is PUT (replaced), not appended.
#
# SHACL is enabled via the JSON REST config (`isShacl=true`): the Turtle
# sail predicate is a no-op, and GraphDB cannot toggle SHACL after creation, so
# enabling it on an existing non-SHACL repo requires drop + recreate (see below).
#
# Requires the local substrate up (`just podman-local-up`) and python3. Override
# the target with GRAPHDB_URL (default http://localhost:7200); set FORCE_RECREATE=1
# to drop and recreate an existing repo (e.g. to turn SHACL on).
#
#   just graphdb-provision
#   FORCE_RECREATE=1 GRAPHDB_URL=http://graphdb:7200 scripts/graphdb-provision.sh

set -euo pipefail

GRAPHDB_URL="${GRAPHDB_URL:-http://localhost:7200}"
REPO_ID="ekg"
FORCE_RECREATE="${FORCE_RECREATE:-0}"

# Repo root (this script lives in <root>/scripts). The ontology under
# crates/ekg-semantic/ontology is the single source of truth (also embedded in
# the crate via include_str!).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ONTO_DIR="${ROOT}/crates/ekg-semantic/ontology"

# Named graphs. The neutral core and the capability ontology each have their own
# graph (each PUT-replaced idempotently); the write-shapes go into GraphDB's reserved
# SHACL graph so they are enforced transactionally on every write.
ONTOLOGY_CORE_GRAPH="https://ekg.example/graph/ontology-core"
ONTOLOGY_GRAPH="https://ekg.example/graph/ontology"
SHACL_SHAPES_GRAPH="http://rdf4j.org/schema/rdf4j#SHACLShapeGraph"

log() { printf '%s\n' "$*"; }

# --- wait for GraphDB to answer ---
log "waiting for GraphDB at ${GRAPHDB_URL} ..."
for _ in $(seq 1 60); do
    if curl -fsS "${GRAPHDB_URL}/rest/repositories" >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
if ! curl -fsS "${GRAPHDB_URL}/rest/repositories" >/dev/null 2>&1; then
    echo "✗ GraphDB did not become ready at ${GRAPHDB_URL}" >&2
    exit 1
fi

repo_exists() {
    curl -fsS "${GRAPHDB_URL}/rest/repositories" \
        | grep -q "\"id\"[[:space:]]*:[[:space:]]*\"${REPO_ID}\""
}

# --- create the SHACL-enabled repository (idempotent) ---
if repo_exists; then
    if [ "${FORCE_RECREATE}" = "1" ]; then
        log "FORCE_RECREATE=1: dropping existing repository '${REPO_ID}' ..."
        curl -fsS -X DELETE "${GRAPHDB_URL}/rest/repositories/${REPO_ID}" >/dev/null || true
    else
        log "repository '${REPO_ID}' already exists, leaving it in place"
        log "  (SHACL cannot be toggled in place; re-run with FORCE_RECREATE=1 to enable it)"
    fi
fi

if ! repo_exists; then
    log "creating SHACL-enabled repository '${REPO_ID}' (rdfsplus-optimized) ..."
    # Patch GraphDB's default JSON config: our id/title, SHACL on, rdfsplus ruleset,
    # context index on (named-graph-per-entity). SHACL can only be enabled via this param.
    curl -fsS "${GRAPHDB_URL}/rest/repositories/default-config/graphdb" \
        | python3 -c "
import json, sys
cfg = json.load(sys.stdin)
cfg['id'] = '${REPO_ID}'
cfg['title'] = 'EKG world model'
for key, val in (('isShacl', 'true'), ('ruleset', 'rdfsplus-optimized'),
                 ('enableContextIndex', 'true'), ('disableSameAs', 'true')):
    if key in cfg['params']:
        cfg['params'][key]['value'] = val
json.dump(cfg, sys.stdout)
" \
        | curl -fsS -X POST "${GRAPHDB_URL}/rest/repositories" \
            -H 'Content-Type: application/json' --data-binary @- >/dev/null
    log "repository '${REPO_ID}' created (SHACL validate-on-write enabled)"
fi

# --- load a Turtle file into a named graph (PUT replaces, so idempotent) ---
load_graph() {
    local file="$1" graph="$2"
    local encoded
    # The RDF4J protocol wants the context as an angle-bracketed, URL-encoded IRI.
    # Our graph IRIs use a fixed, known character set, so hand-encode the reserved
    # characters (`<>`, `:`, `/`, `#`) rather than depend on a urlencode helper.
    encoded="$(printf '%s' "<${graph}>" | sed -e 's/</%3C/g' -e 's/>/%3E/g' -e 's|/|%2F|g' -e 's/:/%3A/g' -e 's/#/%23/g')"
    log "loading $(basename "${file}") into <${graph}>"
    curl -fsS -X PUT \
        -H 'Content-Type: text/turtle' \
        --data-binary "@${file}" \
        "${GRAPHDB_URL}/repositories/${REPO_ID}/statements?context=${encoded}" >/dev/null
}

# Neutral core (governance base + PROV-aligned process metamodel) into its own graph,
# reused by every capability. Loaded first so specialisations resolve.
load_graph "${ONTO_DIR}/ekg-core.ttl"      "${ONTOLOGY_CORE_GRAPH}"
load_graph "${ONTO_DIR}/supply-risk.ttl"   "${ONTOLOGY_GRAPH}"
# Write-shapes into the reserved SHACL graph, so they are enforced on every write.
load_graph "${ONTO_DIR}/write-shapes.ttl"  "${SHACL_SHAPES_GRAPH}"

log "done: repository '${REPO_ID}' provisioned (SHACL on); ontology + write-shapes loaded."
log "consistency-shapes (whole-graph check, non-blocking) are checked with: just graphdb-validate"
log "risk rule: ${ONTO_DIR}/risk-rule.rq"
