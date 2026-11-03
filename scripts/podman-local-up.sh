#!/usr/bin/env bash
#
# Post-`up` provisioning for the local substrate (called by `just podman-local-up`
# after `podman compose … up -d`). Leaves the stack fully ready in one command:
#
#   1. wait for GraphDB + Odoo + Restate to be reachable (bounded, fails loud);
#   2. ensure a valid rpc-scoped Odoo API key (json2): keep the .env key
#      if it still authenticates, else mint one via `odoo shell` and upsert .env;
#   3. ensure a shared task-token secret for the agent tool gateway in .env;
#   4. seed the supply-risk scenario into Odoo, then provision the GraphDB `ekg` repo;
#   5. build, start and register the backend, then start the frontend.
#
# Idempotent: a warm substrate keeps its key and re-runs `seed`/`graphdb-provision`
# (both no-ops when already applied). Config comes from the repo-root .env with
# local defaults.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"

# Read a config value: an already-exported env var wins, else the repo-root .env,
# else the supplied default.
cfg() {
  local key="$1" default="${2:-}" val="${!1:-}"
  if [ -z "$val" ] && [ -f "$ENV_FILE" ]; then
    val="$(grep -E "^${key}=" "$ENV_FILE" | tail -1 | cut -d= -f2- || true)"
  fi
  printf '%s' "${val:-$default}"
}

ODOO_URL="$(cfg ODOO_URL http://localhost:8069)"
ODOO_DB="$(cfg ODOO_DB ekg)"
ODOO_USER="$(cfg ODOO_USER admin)"
ODOO_API_KEY="$(cfg ODOO_API_KEY '')"
GRAPHDB_URL="$(cfg GRAPHDB_URL http://localhost:7200)"
RESTATE_ADMIN_URL="$(cfg RESTATE_ADMIN_URL http://localhost:9070)"
RESTATE_INGRESS_URL="$(cfg RESTATE_INGRESS_URL http://localhost:8080)"
NATS_URL="$(cfg NATS_URL nats://localhost:4222)"
QDRANT_URL="$(cfg QDRANT_URL http://localhost:6333)"

# The backend's /api is published on the host for readiness probing + e2e; the
# Restate callback runs over the compose network (see below), not the host.
BACKEND_API_URL="$(cfg EKG_BACKEND_PUBLIC_URL http://localhost:8090)"
# The URL the Restate runtime (a peer container) dials the backend's SDK endpoint
# back at: compose-network DNS, independent of any host .env value
# (which may hold host.containers.internal for the `just backend` host-dev path).
BACKEND_CALLBACK_URL="http://ekg-backend:9080"

# Compose invocation mirroring the `just podman-local-up` recipe (compose file +
# repo-root .env), so the backend service reads the updated .env (minted key) when
# the script starts it.
COMPOSE_FILE="$REPO_ROOT/deploy/podman/local/compose.yaml"
compose() {
  local args=(compose -f "$COMPOSE_FILE")
  [ -f "$ENV_FILE" ] && args+=(--env-file "$ENV_FILE")
  podman "${args[@]}" "$@"
}

# --- 1. readiness ---

# Poll a URL until it returns HTTP 200, or fail loud after ~120s.
wait_http() {
  local name="$1" url="$2" i code
  for i in $(seq 1 60); do
    code="$(curl -s -m 4 -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || true)"
    if [ "$code" = "200" ]; then
      echo "  ✓ ${name} ready"
      return 0
    fi
    sleep 2
  done
  echo "✗ ${name} not ready after ~120s ($url last=$code)" >&2
  exit 1
}

# Poll a TCP host:port until it accepts a connection, or fail loud after ~120s.
# NATS speaks its own protocol on 4222 (not HTTP), so a TCP connect is the probe.
wait_tcp() {
  local name="$1" host="$2" port="$3" i
  for i in $(seq 1 60); do
    if (exec 3<>"/dev/tcp/${host}/${port}") 2>/dev/null; then
      exec 3>&- 3<&-
      echo "  ✓ ${name} ready"
      return 0
    fi
    sleep 2
  done
  echo "✗ ${name} not ready after ~120s (${host}:${port})" >&2
  exit 1
}

echo "waiting for the substrate..."
wait_http "GraphDB" "${GRAPHDB_URL%/}/rest/repositories"
wait_http "Odoo" "${ODOO_URL%/}/web/database/selector"
wait_http "Restate admin" "${RESTATE_ADMIN_URL%/}/deployments"
# Strip the nats:// scheme, then split host:port for the TCP probe.
nats_hostport="${NATS_URL#nats://}"
wait_tcp "NATS" "${nats_hostport%%:*}" "${nats_hostport##*:}"
wait_http "Qdrant" "${QDRANT_URL%/}/readyz"

# --- 2. ensure a valid rpc-scoped Odoo API key ---

# Does $ODOO_API_KEY authenticate against the json2 API?
key_valid() {
  [ -n "$ODOO_API_KEY" ] || return 1
  local code
  code="$(curl -s -m 8 -o /dev/null -w '%{http_code}' \
    "${ODOO_URL%/}/json/2/res.users/search_count?db=${ODOO_DB}" \
    -H "Authorization: Bearer ${ODOO_API_KEY}" \
    -H 'content-type: application/json' -d '{"domain":[]}' 2>/dev/null || true)"
  [ "$code" = "200" ]
}

# Idempotently set KEY=VALUE in .env (replace in place, else append).
upsert_env() {
  local key="$1" val="$2" tmp
  touch "$ENV_FILE"
  tmp="$(mktemp)"
  awk -v k="$key" -v v="$val" '
    $0 ~ "^" k "=" { print k "=" v; found=1; next }
    { print }
    END { if (!found) print k "=" v }
  ' "$ENV_FILE" > "$tmp" && mv "$tmp" "$ENV_FILE"
}

mint_key() {
  local container db_host db_user db_pass out key
  container="$(podman ps --filter label=com.docker.compose.service=odoo -q | head -1)"
  [ -n "$container" ] || { echo "✗ odoo container not found (is the stack up?)" >&2; exit 1; }
  db_host="$(podman exec "$container" printenv HOST 2>/dev/null || echo postgres)"
  db_user="$(podman exec "$container" printenv USER 2>/dev/null || echo ekg)"
  db_pass="$(podman exec "$container" printenv PASSWORD 2>/dev/null || echo ekg-local-only)"
  # `odoo shell` does not inherit the container's DB env, so pass --db_* explicitly.
  # Resolve the user by login (not a hardcoded uid); scope the key to `rpc`.
  out="$(podman exec -i "$container" odoo shell -d "$ODOO_DB" --no-http \
      --db_host="$db_host" --db_port=5432 --db_user="$db_user" --db_password="$db_pass" 2>/dev/null <<PY || true
user = env['res.users'].search([('login','=','${ODOO_USER}')], limit=1)
key = env['res.users.apikeys'].with_user(user.id)._generate('rpc', 'ekg-local', False)
env.cr.commit()
print('APIKEY_BEGIN' + key + 'APIKEY_END')
PY
)"
  key="$(printf '%s' "$out" | grep -o 'APIKEY_BEGIN.*APIKEY_END' | sed 's/APIKEY_BEGIN//; s/APIKEY_END//' | tr -d '\r\n')"
  [ -n "$key" ] || { echo "✗ failed to mint an Odoo API key" >&2; exit 1; }
  printf '%s' "$key"
}

echo "ensuring a valid Odoo API key..."
if key_valid; then
  echo "  ✓ existing ODOO_API_KEY authenticates, keeping it"
else
  echo "  no valid key (missing/dead), minting a fresh rpc-scoped key"
  ODOO_API_KEY="$(mint_key)"
  upsert_env ODOO_API_KEY "$ODOO_API_KEY"
  echo "  ✓ minted 'ekg-local' key and wrote ODOO_API_KEY into .env"
fi

# --- 2b. ensure a shared task-token secret (agent tool gateway) ---
#
# The backend signs task tokens with EKG_TOOL_TOKEN_SECRET; host-run tests mint tokens
# with the same value. Generate 32 random bytes (hex) once when absent; keep it after.
echo "ensuring a task-token secret for the agent tool gateway..."
if [ -n "$(cfg EKG_TOOL_TOKEN_SECRET '')" ]; then
  echo "  ✓ EKG_TOOL_TOKEN_SECRET already set, keeping it"
else
  upsert_env EKG_TOOL_TOKEN_SECRET "$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
  echo "  ✓ generated EKG_TOOL_TOKEN_SECRET into .env"
fi

# --- 3. seed + provision ---

export ODOO_URL ODOO_DB ODOO_USER ODOO_API_KEY GRAPHDB_URL

echo "seeding the supply-risk scenario into Odoo..."
( cd "$REPO_ROOT" && cargo run --quiet -p ekg-odoo-seed )

echo "provisioning the GraphDB ekg repository..."
bash "$REPO_ROOT/scripts/graphdb-provision.sh"

# --- 4. build, start, and register the backend (last) ---
#
# The backend connects to Odoo (minted key) + GraphDB (ekg repo) on boot and fails
# loud if either is missing, so it starts last, from here.
# It joins the compose network, so Restate reaches its SDK endpoint at
# ekg-backend:9080 (no host.containers.internal hop). Registration and the first
# sweep are driven from the host against the published admin/ingress ports.

echo "building the frontend bundle (host npm)..."
( cd "$REPO_ROOT" && just frontend-build )

echo "building the app images..."
( cd "$REPO_ROOT" && just podman-build )

echo "starting the cognition runtime and the backend (compose profile: backend)..."
compose --profile backend up -d hayhooks ekg-backend

# The engine routes every model call through the runtime: wait for it first,
# bounded, and fail loud if it never serves its pipelines.
wait_http "hayhooks" "http://localhost:1416/status"

# Wait for the backend's own readiness gate (probes Restate/GraphDB/Odoo) on the
# published /api port, bounded, and fail loud if it never comes up.
wait_http "backend" "${BACKEND_API_URL%/}/api/ready"

echo "registering the backend deployment with Restate (${BACKEND_CALLBACK_URL})..."
curl -fsS -X POST "${RESTATE_ADMIN_URL%/}/deployments" \
  -H 'content-type: application/json' \
  -d "{\"uri\": \"${BACKEND_CALLBACK_URL}\", \"force\": true}" >/dev/null
echo "  ✓ registered ${BACKEND_CALLBACK_URL}"

# Publish the repository's agent skills into the durable skill registry, now that
# the registry object is registered. Idempotent: unchanged content is a no-op. After a wipe
# every skill's first version becomes active; a changed skill is published inactive and waits
# for `just skill-activate`, so a deploy never silently changes agent behaviour.
echo "publishing the agent skills..."
( cd "$REPO_ROOT" && RESTATE_INGRESS_URL="$RESTATE_INGRESS_URL" cargo run -q -p ekg-backend --bin skills -- publish )

# The container starts with EKG_SWEEP_BOOTSTRAP=false (no pre-registration 404), so
# trigger the first reconciliation sweep now that the deployment is registered, in stages.
# The sweep ingests every record concurrently, so an order could be
# judged (and proposed on) before the stock and inbound it depends on exist. First re-sync
# every allowlisted model but the orders and MOs, wait until that has settled, then run the
# full sweep, so orders and MOs always land on a materialized world.
# The default mirrors the backend's EKG_INGEST_ALLOWLIST default (apps/backend/src/config.rs);
# a model missing here only loses the staging (it lands with the full sweep), never breaks it.
default_allowlist="sale.order,sale.order.line,product.product,mrp.bom,mrp.bom.line,res.partner,product.supplierinfo,purchase.order,purchase.order.line,stock.picking,stock.quant,stock.warehouse.orderpoint,mrp.production,mrp.workorder,mrp.workcenter,mrp.routing.workcenter,mrp.workcenter.capacity,res.currency"
allowlist="$(cfg EKG_INGEST_ALLOWLIST "$default_allowlist")"
supply_models="$(printf '%s' "$allowlist" | awk -v RS=',' '
  { gsub(/[[:space:]]/, "") }
  $0 != "" && $0 != "sale.order" && $0 != "sale.order.line" && $0 != "mrp.production" {
    printf "%s\"%s\"", (n++ ? "," : ""), $0
  }')"
echo "first sweep, stage 1: materializing the supply facts (every model but orders and MOs)..."
curl -fsS --max-time 300 -X POST "${RESTATE_INGRESS_URL%/}/Sweep/default/resync" \
  -H 'content-type: application/json' -H "idempotency-key: first-sweep-supply-$(date +%s)-$$" \
  -d "{\"models\": [${supply_models}]}" >/dev/null
bash "$REPO_ROOT/scripts/wait-settled.sh"
echo "first sweep, stage 2: the full reconciliation sweep (orders and MOs)..."
curl -fsS -X POST "${RESTATE_INGRESS_URL%/}/Sweep/default/run/send" >/dev/null
echo "  ✓ sweep triggered"

# --- 5. start the frontend (its own service: nginx serves the bundle + proxies /api) ---
# Its image (localhost/ekg-frontend:local) was built by `just podman-build` above. nginx
# reverse-proxies /api to ekg-backend over the compose network, so no host wiring is needed.
echo "starting the frontend service (compose profile: frontend)..."
compose --profile frontend up -d frontend
echo "  ✓ frontend up"

echo "✓ stack ready: backend at ${BACKEND_API_URL}, operator UI at http://localhost:3000 (no manual steps)"
