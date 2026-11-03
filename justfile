# Development and stack-management recipes for the Enterprise Knowledge Graph demonstrator.

repo_name := lowercase(file_name(justfile_directory()))
repo_root := justfile_directory()
podman_local_compose := "deploy/podman/local/compose.yaml"

# Load repo-root .env into every recipe's environment (ODOO_*, GRAPHDB_URL,
# EKG_WEBHOOK_SECRET, …). Optional: a missing .env is not an error.
set dotenv-load := true

# --- Entry point ---

# Show available commands
default:
  @echo "Project: {{repo_name}}"
  @echo "Path: {{repo_root}}"
  @echo ""
  @just --list --justfile {{justfile()}}

# Show repository tree (2 levels)
tree:
  find {{repo_root}} -maxdepth 2 -type f -not -path '*/.git/*' -not -path '*/target/*' | sort

# --- Local substrate (Podman): one stack, self-provisioning ---

# After the containers start: wait for readiness, ensure a valid rpc-scoped Odoo API key
# (self-healing into .env), seed the supply-risk scenario, and provision the GraphDB `ekg`
# repo. Uses the repo-root .env.
# Bring up the whole local stack (GraphDB, Restate, Hayhooks, Postgres, Odoo), ready to use
podman-local-up:
  podman compose -f {{podman_local_compose}} $([ -f .env ] && echo --env-file .env) up -d
  bash scripts/podman-local-up.sh

# Every profile is named so the profiled services (`ekg-backend`, `frontend`, the trace
# backend) are torn down too; without them `down` leaves those containers running.
# Stop the whole local stack (containers removed; named volumes kept)
podman-local-down:
  podman compose --profile backend --profile frontend --profile telemetry -f {{podman_local_compose}} down

# Wipes the Odoo DB, the seeded scenario, and the minted API key (the next `up` re-seeds
# and re-mints). `--profile backend --profile frontend` so the backend and frontend are
# removed too; otherwise their containers keep running against the wiped substrate.
# Full reset, destructive: stop and delete all volumes
podman-local-wipe:
  podman compose --profile backend --profile frontend --profile telemetry -f {{podman_local_compose}} down -v

# Langfuse and its stores (six containers, about 3 GB of images), on http://localhost:3010.
# Opt-in: nothing else starts it, and the engine exports nothing until the printed lines are
# in .env. It keeps its traces in volumes of its own, which `just podman-local-wipe` removes.
# Start the local trace backend and print the settings that send traces to it
telemetry-up:
  #!/usr/bin/env bash
  set -euo pipefail
  podman compose --profile telemetry -f {{podman_local_compose}} $([ -f .env ] && echo --env-file .env) up -d \
    langfuse-postgres langfuse-clickhouse langfuse-cache langfuse-objects langfuse-worker langfuse-web
  [ -f .env ] && set -a && . ./.env && set +a
  port="${LANGFUSE_PORT:-3010}"
  echo "waiting for Langfuse on port ${port}..."
  waited=0
  until curl -fsS "http://localhost:${port}/api/public/health" >/dev/null 2>&1; do
    if [ "$waited" -ge 300 ]; then
      echo "Langfuse did not become healthy: see 'podman logs' of langfuse-web" >&2
      exit 1
    fi
    sleep 3
    waited=$((waited + 3))
  done
  key="$(printf '%s:%s' "${LANGFUSE_PUBLIC_KEY:-pk-lf-ekg-local}" "${LANGFUSE_SECRET_KEY:-sk-lf-ekg-local}" | base64 | tr -d '\n')"
  echo "Langfuse is up: http://localhost:${port} (${LANGFUSE_USER_EMAIL:-operator@ekg.local})"
  echo "To export traces to it, put these lines into .env:"
  echo
  echo "OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:${port}/api/public/otel"
  echo "EKG_TELEMETRY_STACK_ENDPOINT=http://langfuse-web:3000/api/public/otel"
  echo "OTEL_EXPORTER_OTLP_HEADERS=Authorization=Basic%20${key},x-langfuse-ingestion-version=4"

# Stop the local trace backend (containers removed; its volumes kept)
telemetry-down:
  podman compose --profile telemetry -f {{podman_local_compose}} stop \
    langfuse-web langfuse-worker langfuse-objects langfuse-cache langfuse-clickhouse langfuse-postgres
  podman compose --profile telemetry -f {{podman_local_compose}} rm -f \
    langfuse-web langfuse-worker langfuse-objects langfuse-cache langfuse-clickhouse langfuse-postgres

# Tail local stack logs
podman-local-logs:
  podman compose -f {{podman_local_compose}} logs -f

# Builds apps/<app>/Dockerfile as localhost/ekg-<app>:local from the repo-root build
# context (the backend needs the whole cargo workspace). A Dockerfile added to a new app
# is picked up without a recipe change. Layer-cached, so re-runs are cheap;
# `just podman-local-up` runs this before it starts the backend service.
# Build a local container image for every app that ships a Dockerfile
podman-build:
  #!/usr/bin/env bash
  set -euo pipefail
  shopt -s nullglob
  built=0
  for dockerfile in apps/*/Dockerfile; do
    app="$(basename "$(dirname "$dockerfile")")"
    echo "building localhost/ekg-${app}:local from ${dockerfile}"
    podman build -f "$dockerfile" -t "localhost/ekg-${app}:local" .
    built=$((built + 1))
  done
  [ "$built" -gt 0 ] || echo "no apps/*/Dockerfile found, nothing to build"

# Runs uv in a pinned container, so no host Python is needed; the resolution is universal,
# so the lock installs on amd64 and arm64 alike. Rebuild the image afterwards with
# `just podman-build`.
# Regenerate apps/hayhooks/requirements.txt (hash-locked) from requirements.in
hayhooks-lock:
  #!/usr/bin/env bash
  set -euo pipefail
  podman run --rm -v "{{repo_root}}/apps/hayhooks:/work:Z" -w /work \
    ghcr.io/astral-sh/uv:0.9.30-python3.12-bookworm-slim sh -c '
      uv pip compile --quiet --universal --generate-hashes --python-version 3.12 \
        --custom-compile-command "just hayhooks-lock" requirements.in -o requirements.txt'
  echo "✓ apps/hayhooks/requirements.txt regenerated"

# Idempotent; needs the stack up. Override with ODOO_URL/ODOO_DB/ODOO_USER/ODOO_API_KEY.
# Seed the at-risk supply-risk scenario (multi-level BOM) into Odoo
seed:
  cargo run -p ekg-odoo-seed

# Idempotent; needs the stack up. Override the target with GRAPHDB_URL (default
# http://localhost:7200).
# Provision the GraphDB `ekg` repository and load the supply-risk ontology + SHACL shapes
graphdb-provision:
  bash scripts/graphdb-provision.sh

# Validates the settled `ekg` graph against the cross-entity consistency-shapes. A
# non-blocking governance signal.
# Whole-graph SHACL consistency check
graphdb-validate:
  bash scripts/graphdb-validate.sh

# Derives ekg:precedes ordering and the non-direct edges ekg:used / ekg:fulfills /
# ekg:capacity. Idempotent; run after materialization.
# Run the structural process-graph derivations
graphdb-derive:
  bash scripts/graphdb-derive.sh

# --- Ingestion (governed source, materialize, notify) ---

# The host-dev path. `just podman-local-up` already builds, starts, and registers a
# containerized backend as part of the one-command stack; use that for the full e2e. Use
# `just backend` + `just restate-register` only to run the backend from the host against
# the running substrate.
#
# Runs the public /api Odoo webhook surface and the durable Restate ingest worker +
# reconciliation sweep. Needs the stack up and EKG_WEBHOOK_SECRET + ODOO_API_KEY set
# (loaded from .env). After it is running, register it once with `just restate-register`.
# Run the ingestion backend on the host
backend:
  cargo run -p ekg-backend

# EKG_SERVICE_CALLBACK_URL is the URL the runtime dials the backend at. For a host-run
# backend (`just backend`) that is host.containers.internal:9080 (the default here); the
# containerized backend is reached over the compose network at http://ekg-backend:9080 and
# is registered by `just podman-local-up`, so this recipe is not needed for it.
# Register a backend's Restate SDK endpoint with the runtime
restate-register:
  curl -fsS -X POST "${RESTATE_ADMIN_URL:-http://localhost:9070}/deployments" -H 'content-type: application/json' -d "{\"uri\": \"${EKG_SERVICE_CALLBACK_URL:-http://host.containers.internal:9080}\", \"force\": true}"
  @echo "✓ registered ingest deployment"

# Needs the deployment registered. RESTATE_INGRESS_URL default http://localhost:8080.
# Not idempotent: while a timer chain is already running, each trigger adds another chain,
# so use it only when none is (see deploy/podman/local/README.md).
# Trigger one reconciliation sweep pass (it then self-reschedules on its timer)
sweep-trigger:
  curl -fsS -X POST "${RESTATE_INGRESS_URL:-http://localhost:8080}/Sweep/default/run/send"
  @echo "✓ triggered reconciliation sweep"

#   just sweep-resync stock.picking purchase.order purchase.order.line mrp.production product.supplierinfo
# Re-pulls and re-materializes every current record of the named allowlisted models,
# regardless of the sweep watermark, e.g. after a mapping change adds a field to records
# that have not changed at the source since. Leaves the watermark and the sweep timer
# untouched; a model outside the ingest allowlist is refused (nothing pulled). A shared
# handler, so it does not queue behind a running sweep pass; it prints the per-model
# resubmission report (or the refusal reason). Every call carries an idempotency key (printed;
# reuse it via EKG_RESYNC_IDEMPOTENCY_KEY). The key ends in a digest of the requested model
# set, so reusing it with the same models attaches to that invocation while other models
# always get a new key (Restate matches the key, not the body). If curl exits without an
# answer (e.g. it gave up after EKG_RESYNC_TIMEOUT_SECS, default 120, or the connection
# dropped after sending) the outcome is unknown: re-run with the printed key to attach to that
# same invocation (never a second re-sync), or inspect it in the Restate UI.
# Scoped re-sync (backfill) of the named allowlisted models, see deploy/podman/local/README.md
sweep-resync +models:
  #!/usr/bin/env bash
  set -euo pipefail
  body='{"models":['; sep=''
  for m in {{models}}; do
    [[ "$m" =~ ^[a-z0-9_.]+$ ]] || { echo "✗ not a model name: $m" >&2; exit 1; }
    body+="${sep}\"${m}\""; sep=','
  done
  body+=']}'
  # The key is bound to the requested models: a digest of the model set ends it (sorted,
  # de-duplicated: the handler treats `a b`, `b a` and `a a b` alike), so a reused key
  # (already ending in this digest) attaches, and a key reused for other models gets a new one.
  digest="$(printf '%s\n' {{models}} | sort -u | cksum | cut -d' ' -f1)"
  key="${EKG_RESYNC_IDEMPOTENCY_KEY:-resync-$(date +%s)-$$}"
  case "$key" in *-"$digest") ;; *) key="$key-$digest" ;; esac
  echo "re-sync idempotency key: $key"
  rc=0
  curl -sS --fail-with-body --max-time "${EKG_RESYNC_TIMEOUT_SECS:-120}" \
    -X POST "${RESTATE_INGRESS_URL:-http://localhost:8080}/Sweep/default/resync" \
    -H 'content-type: application/json' -H "idempotency-key: $key" -d "$body" || rc=$?
  echo
  # 22: an HTTP error answer (the body, e.g. the refusal reason, is shown above);
  # 6 / 7: DNS or connect failure (nothing was sent). Any other failure may have come after
  # the request was sent (28 timeout, 52 empty reply, 56 receive error, …): outcome unknown.
  if [ "$rc" != 0 ] && [ "$rc" != 22 ] && [ "$rc" != 6 ] && [ "$rc" != 7 ]; then
    echo "✗ curl exited $rc without an answer: the outcome is unknown (the request may not have been sent, or the re-sync may still be running or failing)." >&2
    echo "  Attach to the same invocation (never a second re-sync): EKG_RESYNC_IDEMPOTENCY_KEY=$key EKG_RESYNC_TIMEOUT_SECS=600 just sweep-resync {{models}}" >&2
    echo "  Or inspect it in the Restate UI: ${RESTATE_ADMIN_URL:-http://localhost:9070}/ui/" >&2
    exit "$rc"
  fi
  [ "$rc" = 0 ] || exit "$rc"
  echo "✓ re-sync submitted (records re-materialize asynchronously via the ingest object)"

# --- Agent skills (authored in git, versioned + activated in Restate) ---

# Offline: no stack needed. The gate runs the same check (the backend's skills_drift suite).
# Validate every skill under apps/hayhooks/skills against the engine's tool profiles
skills-validate:
  cargo run -q -p ekg-backend --bin skills -- validate

# Needs the stack up and the backend deployment registered (RESTATE_INGRESS_URL, default
# http://localhost:8080). Idempotent: unchanged content is a no-op; a skill's first version
# becomes active, a new version of a known skill stays inactive until `just skill-activate`.
# Validate, then publish every skill into the durable registry
skills-publish:
  cargo run -q -p ekg-backend --bin skills -- publish

#   just skill-activate propose-plan 1a2b3c4d tighter target guidance
# VERSION is a full version id or a unique prefix of at least 8 characters (see
# `just skills-list`); activating an earlier version is the rollback. Who activates is
# EKG_OPERATOR, else `git config user.name`, else $USER; it is recorded with the reason.
# Activate (or roll back to) a registered skill version, with a reason
skill-activate skill version +reason:
  cargo run -q -p ekg-backend --bin skills -- activate {{quote(skill)}} {{quote(version)}} --reason {{quote(reason)}}

# List every published skill: its versions, the active one, and the activation history
skills-list:
  cargo run -q -p ekg-backend --bin skills -- list

# Needs the stack up and the backend registered (RESTATE_INGRESS_URL, default
# http://localhost:8080):
#   just agent-mode shadow --until +8h --max-slates 5 --max-tokens 200000 --reason "collect shadow evidence"
#   just agent-mode agent --until +2d --max-slates 40 --max-tokens 900000 --reason "criteria met and signed"
#   just agent-mode legacy --reason "stop the agent"       # the kill switch
# --until is a UTC date(-time) or +<n>m|h|d. Who switches is EKG_OPERATOR, else
# `git config user.name`, else $USER; it is recorded with the reason in the mode's history.
# `agent` is refused without a sign-off for the registered criteria (`just agent-signoff`). In
# it a freshly opened point gets the agent's slate; a point's proposal, and with it every
# auto-applied change, still comes from the prompt path.
# Switch the agent's governed rollout mode (shadow, agent, or legacy = the kill switch)
[positional-arguments]
agent-mode +args:
  cargo run -q -p ekg-backend --bin rollout -- mode "$@"

# Show the agent's rollout mode, its window and budget use, why it stopped, the sign-offs and
# the history (a rollback's entry is made by `trigger:<name>`)
agent-status:
  cargo run -q -p ekg-backend --bin rollout -- status

# Uses Restate's own state introspection (the `restate` CLI inside the Restate container):
#   just agent-shadow-list https://ekg.example/id/sale.order/42
# Each record holds the agent's slate, the slate the point got in the same step (`paired`),
# and, once the point is promoted, aborted or expired, that decision (`decision`).
# A portfolio's joint records are kept under its id (`just agent-shadow-list portfolio/<digest>`).
# Read a subject's agent shadow records as JSON, or list the subjects that have any
agent-shadow-list subject="":
  #!/usr/bin/env bash
  set -euo pipefail
  restate=(podman compose -f {{podman_local_compose}} exec -T restate restate)
  if [ -n {{quote(subject)}} ]; then
    "${restate[@]}" kv get --plain AgentShadow {{quote(subject)}}
  else
    "${restate[@]}" sql "SELECT service_key AS subject FROM state WHERE service_name = 'AgentShadow' ORDER BY service_key"
  fi

# Needs the stack up (the Restate ingress and admin endpoint). Read-only: it changes no shadow
# record, no action point and not the rollout mode.
#   just agent-shadow-report                        # the table
#   just agent-shadow-report --json report.json     # and the same as JSON
# Each record's agent slate beside the slate its point got and the decision on it, then totals
[positional-arguments]
agent-shadow-report *args:
  cargo run -q -p ekg-backend --bin rollout -- shadow-report "$@"

# Needs the stack up. Records evals/agent/promotion.yaml (or --file <path>) with the rollout:
# its content, its digest, who, why and when. A shadow window opened afterwards is tied to it.
#   just agent-criteria-register --reason "before the first window"
# Register the promotion criteria, so that evidence gathered afterwards counts for them
[positional-arguments]
agent-criteria-register *args:
  cargo run -q -p ekg-backend --bin rollout -- criteria register "$@"

# Show the registered promotion criteria and the registrations before them
agent-criteria:
  cargo run -q -p ekg-backend --bin rollout -- criteria show

# Needs the stack up and a paired eval report (`just agent-eval --arm both`) that was run under
# the registered criteria. Read-only, and it decides nothing: it exits non-zero when the
# criteria are not met.
#   just agent-promotion-verdict --eval .artifacts/agent-eval/<time>.json
# Check the registered criteria against a paired eval report and the shadow records
[positional-arguments]
agent-promotion-verdict *args:
  cargo run -q -p ekg-backend --bin rollout -- verdict "$@"

# Needs the stack up and a paired eval report that was run under the registered criteria.
# Computes the verdict as `just agent-promotion-verdict` does and records it with the rollout:
# the verdict, the digests of the criteria and of the report, the version the evidence is of,
# who signs and why. Refused when the criteria are not met, when the verdict is for other
# criteria than the registered ones, or when the signer registered them. It switches nothing.
#   just agent-signoff --eval .artifacts/agent-eval/<time>.json --reason "reviewed the verdict"
# Sign off the registered criteria against a paired eval report and the shadow records
[positional-arguments]
agent-signoff *args:
  cargo run -q -p ekg-backend --bin rollout -- signoff "$@"

# --- Agent evals (the real model on fixed cases; spends tokens; no gate runs it) ---

# Needs GraphDB up (GRAPHDB_URL, default http://localhost:7200), the runtime image
# (`just podman-build`) and MODEL_API_KEY. It does not need Restate or Odoo, and reads and
# writes nothing of the running stack: every case gets a repository of its own.
#   just agent-eval                                   # the whole set, 3 trials per case
#   just agent-eval --case late-inbound-single-line   # one case, to debug it
#   just agent-eval --profile process                 # one profile's cases, after a change to it
#   just agent-eval --arm legacy                      # the prompt-based path on the same cases
#   just agent-eval --arm both                        # both arms, one paired report
# A trial that errors is attempted again (--retries, default 2); up to three trials of a case
# run at once (--concurrency), fewer when GraphDB has no room for their repositories. A run
# narrowed with --case, --category or --profile is no evidence for a promotion verdict.
# The runtime runs as its own container, reading through this run's gateway
# (EKG_AGENT_EVAL_GATEWAY_PORT, default 8791), and is removed when the run ends.
# With OTEL_EXPORTER_OTLP_ENDPOINT set, the run and its runtime both export their spans there.
# Run the agent eval set against the real model and grade it
[positional-arguments]
agent-eval *args:
  #!/usr/bin/env bash
  set -euo pipefail
  port="${EKG_AGENT_EVAL_GATEWAY_PORT:-8791}"
  runtime_port="${EKG_AGENT_EVAL_RUNTIME_PORT:-1417}"
  name="ekg-agent-eval-runtime-$$"
  trap 'podman rm -f "$name" >/dev/null 2>&1 || true' EXIT
  # With an OTLP endpoint set, the runtime exports to it too, so a trace holds both sides.
  # The container reaches the host's endpoint by the host's name, over HTTP as the engine does.
  otel=()
  if [ -n "${OTEL_EXPORTER_OTLP_ENDPOINT:-}" ]; then
    endpoint="${OTEL_EXPORTER_OTLP_ENDPOINT//localhost/host.containers.internal}"
    otel=(
      -e OTEL_EXPORTER_OTLP_ENDPOINT="${endpoint//127.0.0.1/host.containers.internal}"
      -e OTEL_EXPORTER_OTLP_HEADERS="${OTEL_EXPORTER_OTLP_HEADERS:-}"
      -e OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
      -e OTEL_SERVICE_NAME=ekg-cognition-runtime
    )
  fi
  podman run -d --name "$name" -p "127.0.0.1:${runtime_port}:1416" \
    ${otel[@]+"${otel[@]}"} \
    -e MODEL_BASE_URL="${MODEL_BASE_URL:-https://openrouter.ai/api/v1}" \
    -e MODEL_API_KEY="${MODEL_API_KEY:-}" \
    -e CHAT_MODEL="${CHAT_MODEL:-openai/gpt-oss-120b}" \
    -e AGENT_MODEL="${AGENT_MODEL:-}" \
    -e AGENT_MODEL_API_KEY="${AGENT_MODEL_API_KEY:-}" \
    -e EKG_TOOL_GATEWAY_URL="http://host.containers.internal:${port}" \
    -e HAYHOOKS_DURABLE_MODE=false \
    -e HAYSTACK_CONTENT_TRACING_ENABLED=false \
    -e PROVIDER_TIMEOUT_SECS="${PROVIDER_TIMEOUT_SECS:-150}" \
    localhost/ekg-hayhooks:local >/dev/null
  echo "waiting for the runtime on port ${runtime_port}..."
  waited=0
  until curl -fsS "http://localhost:${runtime_port}/status" >/dev/null 2>&1; do
    if [ "$waited" -ge 120 ]; then
      echo "the runtime did not start: run 'just podman-build' first" >&2
      podman logs "$name" 2>&1 | tail -20 >&2
      exit 1
    fi
    sleep 2
    waited=$((waited + 2))
  done
  EKG_AGENT_EVAL_RUNTIME_URL="http://localhost:${runtime_port}" \
    CHAT_MODEL="${CHAT_MODEL:-openai/gpt-oss-120b}" \
    cargo run -q -p ekg-backend --bin agent-eval -- run "$@"

# Needs GraphDB only, and no model. Run it with the rest of the stack down: it creates and
# drops many repositories in a short time.
# Check every eval case against the simulator: its reference plan passes, its futile plans do not
agent-eval-check:
  EKG_E2E_REQUIRE_INFRA=1 cargo test -p ekg-e2e --test it agent_eval_fixtures_it -- --ignored --nocapture

# After a case changed: raise `version` in evals/agent/set.yaml, then run this.
# Renew the eval set's lock (its version and the digest of its cases)
agent-eval-lock:
  cargo run -q -p ekg-backend --bin agent-eval -- lock

# Needs `just podman-local-up` first (which leaves the backend up + registered). Waits up
# to EKG_E2E_READY_TIMEOUT seconds (default 300) for the backend to be ready, asserts
# a ekg-backend:9080 deployment is registered, then runs the --ignored live tests one
# at a time, incl. transport_integration_it (an ETA slip opens an action point) against
# http://localhost:8090, and reports every failure. It does not wipe between tests:
# isolation is by per-run-unique subject ids, but `just podman-local-wipe` gives a pristine
# start.
# Gated live e2e suite against the running stack (waits for ready, one test at a time)
test-e2e:
  #!/usr/bin/env bash
  set -euo pipefail
  # Gated tests on the loud-skip helper fail (instead of printing SKIP and passing)
  # when a dependency is unreachable: this run is meant to prove behaviour.
  export EKG_E2E_REQUIRE_INFRA=1
  api="${EKG_BACKEND_PUBLIC_URL:-http://localhost:8090}"
  admin="${RESTATE_ADMIN_URL:-http://localhost:9070}"
  # Wait (bounded) for readiness: right after `podman-local-up` the backend answers 503 for a
  # while.
  timeout="${EKG_E2E_READY_TIMEOUT:-300}"
  echo "waiting up to ${timeout}s for the backend to be ready (${api%/}/api/ready)..."
  waited=0
  until curl -fsS "${api%/}/api/ready" >/dev/null 2>&1; do
    if [ "$waited" -ge "$timeout" ]; then
      echo "✗ backend not ready after ${timeout}s: run 'just podman-local-up' first" >&2
      exit 1
    fi
    sleep 3
    waited=$((waited + 3))
  done
  echo "checking a ekg-backend:9080 deployment is registered..."
  curl -fsS "${admin%/}/deployments" | grep -q 'ekg-backend:9080' \
    || { echo "✗ no ekg-backend:9080 deployment: run 'just podman-local-up' first" >&2; exit 1; }
  # Ready is not settled: right after bring-up the first sweep still ingests, monitors and
  # proposes, and a test started then misses its own window.
  # Wait until Restate is idle and the monitor bridge has drained its
  # event backlog. Its own bound: after a fresh bring-up the bridge proposes
  # on every seeded point inline, and one slow model call holds it for minutes.
  bash scripts/wait-settled.sh "${EKG_E2E_SETTLE_TIMEOUT:-900}"
  # The transport test drives the shared seeded order + global inventory, so it
  # must run first, against the fresh `podman-local-up` seed, before the other gated
  # tests (sweep/ingest) add component demand that would flip the trike early. The
  # rest self-isolate via per-run-unique subject ids, so order doesn't matter
  # for them; run them after, skipping the transport test so it isn't re-run polluted.
  # Suites not on the loud-skip helper print their own `SKIP:` and pass; the log
  # lets the run end by listing them, so a green run never hides a skipped proof.
  # Each crate's suites are modules of one `it` test binary. They share this one stack
  # (Odoo seed, `Sweep` claims, Restate routing, GraphDB's repository limit), so they run
  # one test at a time. `--no-fail-fast` keeps going past a failing crate, so one run
  # reports every failure.
  log="$(mktemp -t ekg-test-e2e.XXXXXX)"
  trap 'rm -f "$log"' EXIT
  status=0
  echo "running the transport milestone (against the fresh seed)..."
  cargo test -p ekg-e2e --test it transport_integration_it -- --ignored --nocapture \
    --test-threads=1 2>&1 | tee -a "$log" || status=$?
  echo "running the remaining gated e2e suite..."
  # ekg-cognition carries the runtime contract suite (the built runtime image against a fake
  # provider) and the direct route's live TLS smoke.
  # The eval set's case check is left out: it creates and drops some eighty repositories in a
  # minute, which with the whole stack running can push GraphDB out of memory. It checks the
  # set, not the stack, and has its own recipe (`just agent-eval-check`).
  cargo test -p ekg-e2e -p ekg-semantic -p ekg-kinetic -p ekg-cognition --no-fail-fast --test it -- \
    --ignored --nocapture --test-threads=1 \
    --skip a_transport_eta_slip_opens_an_action_point_end_to_end \
    --skip every_case_is_what_it_claims_to_be 2>&1 | tee -a "$log" || status=$?
  # The marker anywhere on a line: libtest may have printed a `test <name> ... ` prefix first.
  skipped="$(grep -o 'SKIP: .*' "$log" || true)"
  if [ -n "$skipped" ]; then
    echo "warning: $(printf '%s\n' "$skipped" | wc -l | tr -d ' ') gated test(s) skipped (suites not yet on the loud-skip helper ignore EKG_E2E_REQUIRE_INFRA):" >&2
    printf '%s\n' "$skipped" | sort | uniq -c >&2
  else
    echo "✓ no gated test skipped"
  fi
  if [ "$status" -ne 0 ]; then
    echo "✗ gated e2e suite failed: see the failures above (every failing test is listed)" >&2
    exit "$status"
  fi

# Slips the ETA and fires the ingest webhook, so the Cargo Trike order flips to at risk.
# On-demand only (never part of `up`). Needs the stack up + a backend running
# (containerized via `just podman-local-up`, or host `just backend`).
#   just transport-sim              # slip 60 days (at risk)
#   just transport-sim --slip 90    # slip further
#   just transport-sim --on-time    # restore an on-time ETA (covered)
# Transport simulator: slip the seeded inbound shipment's ETA past the customer promise
transport-sim *ARGS:
  cargo run --quiet -p ekg-transport-sim -- {{ARGS}}

# --- Supply chain / SBOM (ORT, OSS Review Toolkit) ---

# Covers Cargo, npm (apps/frontend's committed package-lock.json) and pip (apps/hayhooks'
# hash-locked requirements.txt), with resolved licenses, organized by crate/app/service, via
# the pinned ORT container image (no host JVM). Non-gating: cargo-deny stays the
# supply-chain gate. Only docs/sbom/report.html is committed; the rest is generated +
# gitignored. Uses the ort-minimal image; override with ORT_IMAGE=… / ORT_VERSION=…
# (default ort-minimal:84.0.0). See docs/sbom/README.md.
#   just sbom
#   ORT_VERSION=84.0.0 just sbom
# Generate a cross-ecosystem SBOM into docs/sbom/
sbom:
  bash scripts/sbom.sh

# --- Frontend (apps/frontend): the Plant Pulse operator UI ---

# Install the frontend's pinned dependencies (npm ci from the committed lockfile).
frontend-install:
  cd apps/frontend && npm ci

# Reinstall the frontend's pinned deps (`npm ci`) unless they are current: npm rewrites
# node_modules/.package-lock.json on every install, so it being newer than package-lock.json
# means the lockfile has not changed since. A missing node_modules always installs.
_frontend-deps:
  cd apps/frontend && if [ node_modules/.package-lock.json -nt package-lock.json ]; then echo "frontend deps current (node_modules newer than package-lock.json)"; else npm ci; fi

# The frontend image packages this prebuilt output (its Dockerfile carries no Node toolchain).
# Reinstalls deps first when the lockfile changed. `just podman-local-up` runs this before
# `just podman-build`.
# Build the production bundle (apps/frontend/dist) on the host
frontend-build: _frontend-deps
  cd apps/frontend && npm run build

# Defaults to the podman stack's published backend (:8090, from `just podman-local-up`);
# for a host-run backend pass its origin, e.g. `just frontend-dev http://localhost:4000`
# (EKG_API_PORT). Reinstalls deps first when the lockfile changed.
# Start the frontend dev server (Vite on http://localhost:5173), proxying /api to the backend
frontend-dev origin="http://localhost:8090": _frontend-deps
  cd apps/frontend && EKG_API_ORIGIN={{origin}} npm run dev

# Exports schema.graphql + openapi.json + tab-rows.json (the tab row vocabulary) from the
# backend's code (no running backend, the GraphQL depth guard untouched), then regenerates the
# typed clients from them. The backend's `contract_snapshots` test fails whenever the
# committed snapshots drift.
# Regenerate the contract snapshots from code, then the typed clients
frontend-contracts: _frontend-deps
  cargo run -q -p ekg-backend --bin export-contracts -- apps/frontend
  cd apps/frontend && npm run regen-contracts

# typecheck + eslint + typed clients vs contract snapshots + the jsdom unit tests + every story
# in headless Chromium (play + a11y, colour contrast included) + a runtime-dependency audit +
# the app build + the Storybook build. Downloads Chromium on first run (cached); on a bare
# Linux host first run `cd apps/frontend && npm ci && npx --no playwright install --with-deps
# chromium` (the pinned local Playwright, never a registry fetch) for the system libraries.
# Needs network for `npm ci` / `npm audit` (like `cargo deny`). Part of `just verify`.
# Frontend quality gate for apps/frontend
frontend-check:
  cd apps/frontend && npm ci
  cd apps/frontend && npx --no playwright install chromium
  cd apps/frontend && npm run typecheck
  cd apps/frontend && npm run lint
  cd apps/frontend && npm run check-contracts
  cd apps/frontend && npm run test:unit
  cd apps/frontend && npm run test:stories
  cd apps/frontend && npm audit --omit=dev --audit-level=high
  cd apps/frontend && npm run build
  cd apps/frontend && npm run build-storybook

# Rust fmt + clippy (warnings denied) + the workspace tests (incl. the contract-snapshot drift
# check) + the whole frontend gate. The Rust supply-chain audit (`cargo deny`) is not part of
# it: it needs cargo-deny and the advisory database from the network, so it runs in `check`.
# Dependencies run in order (Rust first, fail fast), in this same just process; `--locked`
# fails instead of silently re-resolving an out-of-sync Cargo.lock (like `npm ci`).
# The full quality gate: run before opening a PR
verify: _verify-rust frontend-check
  @echo "note: the Rust supply-chain audit (cargo deny) is not part of verify; it runs in 'just check'"
  @echo "✓ verify passed"

# The Rust half of `verify`: fmt check, clippy with warnings denied, and the workspace tests.
_verify-rust:
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked

# --- Setup ---

# Print the required toolchain and whether it is present
doctor:
  #!/usr/bin/env bash
  set -uo pipefail
  echo "Required tooling (install any that show 'missing'):"
  for t in cargo just lefthook cog git-cliff cargo-deny podman; do
    if command -v "$t" >/dev/null 2>&1; then
      printf "  ✓ %s\n" "$t"
    else
      printf "  ✗ %s (missing)\n" "$t"
    fi
  done
  # The active compiler (it honours rust-toolchain.toml) against the workspace's declared minimum
  # (Cargo.toml `rust-version`). rustup does not update an installed stable by itself, so an old
  # one fails on a dependency's minimum instead of saying why. Advisory only: nothing is changed.
  # EKG_DOCTOR_RUSTC_VERSION forces the compiler version (to see the warning).
  min="$(sed -nE 's/^rust-version = "([0-9.]+)".*/\1/p' Cargo.toml | head -1)"
  have="${EKG_DOCTOR_RUSTC_VERSION:-$(rustc --version 2>/dev/null | awk '{print $2}')}"
  if [ -n "$min" ] && [ -n "$have" ]; then
    if [ "$(printf '%s\n%s\n' "$min" "$have" | sort -V | head -1)" = "$min" ]; then
      printf "  ✓ rustc %s meets the workspace minimum %s\n" "$have" "$min"
    else
      echo ""
      echo "warning: rustc $have is older than the workspace minimum $min (Cargo.toml rust-version)."
      echo "  Update the toolchain:  rustup update stable"
    fi
  fi
  echo ""
  echo "Install everything with:  just setup-tools"
  echo "  (Rust tools via cargo-binstall or cargo install)"
  echo "  lefthook is a single binary: https://github.com/evilmartians/lefthook"
  # macOS scans every newly built executable before its first launch (30 to 90 s per test
  # binary, one file at a time machine-wide) unless the launching app is listed under Privacy &
  # Security > Developer Tools. There is no status query for that list (`DevToolsSecurity` is
  # debugger authorization only), so time a fresh probe's first launch, capped at 3 s. Advisory
  # only: no setting is changed. EKG_DOCTOR_PROBE_SECONDS forces the measured time.
  if [ "$(uname)" = Darwin ]; then
    secs="${EKG_DOCTOR_PROBE_SECONDS:-}"
    if [ -z "$secs" ]; then
      if command -v cc >/dev/null 2>&1; then
        probe="$(mktemp -d -t ekg-doctor)"
        printf 'int main(void){return 0;}\n' > "$probe/p.c"
        if cc -o "$probe/p" "$probe/p.c" 2>/dev/null; then
          secs="$(perl -MTime::HiRes=time -MPOSIX=:sys_wait_h -e '
            my $t0 = time; my $pid = fork // exit 1;
            if (!$pid) { exec $ARGV[0] or exit 127 }
            while (time - $t0 < 3) {
              if (waitpid($pid, WNOHANG) > 0) { printf "%.2f", time - $t0; exit }
              select(undef, undef, undef, 0.02);
            }
            kill "KILL", $pid; print "3.00"' "$probe/p")"
        fi
        rm -rf "$probe"
      else
        echo ""
        echo "note: no 'cc' (xcode-select --install), skipped the macOS first-launch scan check"
      fi
    fi
    if [ -n "$secs" ] && perl -e 'exit !($ARGV[0] > 0.5)' "$secs"; then
      echo ""
      echo "warning: macOS scans each newly built executable before its first launch (a fresh probe took"
      echo "  ${secs}s; capped at 3 s): every relinked test binary waits 30 to 90 s, one at a time. One-time fix:"
      echo "    sudo spctl developer-mode enable-terminal"
      echo "    then allow your terminal(s), editor, and tmux/zellij under"
      echo "    System Settings > Privacy & Security > Developer Tools, and restart them."
    fi
  fi

  # A bloated build cache slows every compile: rustc lists <target>/debug/deps to find each
  # dependency, and Cargo never prunes stale artifacts (~979k entries make for a 68 s listing).
  # Count entries, stopping at the limit so this stays fast.
  # Advisory only: nothing is deleted. EKG_DOCTOR_DEPS_FILES forces the count.
  deps_max="${EKG_DOCTOR_DEPS_MAX:-100000}"
  deps_dir="${CARGO_TARGET_DIR:-target}/debug/deps"
  deps_files="${EKG_DOCTOR_DEPS_FILES:-}"
  if [ -z "$deps_files" ]; then
    if [ -d "$deps_dir" ]; then
      deps_files="$(ls -f "$deps_dir" 2>/dev/null | head -n "$deps_max" | wc -l | tr -d ' ')"
    else
      deps_files=0
    fi
  fi
  if [ "$deps_files" -ge "$deps_max" ]; then
    echo ""
    echo "warning: the build cache is bloated: ${deps_dir} holds ${deps_files}+ entries (limit ${deps_max})."
    echo "  Every rustc lists that directory, so every compile slows down. Fix (one full rebuild after):"
    echo "    cargo clean"
  fi

# Install git hooks (fmt, clippy, conventional commits, branch policy, cargo-deny)
setup-hooks:
  lefthook install
  @echo "✓ git hooks installed"

# Install the dev tools (cocogitto, cargo-deny)
setup-tools:
  #!/usr/bin/env bash
  set -uo pipefail
  echo "Installing Rust dev tools (cocogitto, cargo-deny)..."
  if command -v cargo-binstall >/dev/null 2>&1; then
    cargo binstall -y cocogitto cargo-deny
  else
    echo "(tip: 'cargo install cargo-binstall' gives faster prebuilt installs)"
    cargo install cocogitto cargo-deny
  fi
  echo "Rust tools install to ~/.cargo/bin; make sure that dir is on your PATH."
  echo "✓ setup-tools done: run 'just doctor' (restart shell if PATH changed)"

# Bootstrap a fresh clone (installs tools + hooks, then reports status)
bootstrap:
  -just setup-tools
  just setup-hooks
  just doctor
  @echo ""
  @echo "✓ Bootstrap complete: run 'just check' to validate the workspace"

# --- Quality gates ---

# Format the workspace
fmt:
  cargo fmt --all

# Check formatting (CI)
fmt-check:
  cargo fmt --all -- --check

# Lint: formatting + clippy with warnings denied
lint: fmt-check
  cargo clippy --workspace --all-targets -- -D warnings

# Typecheck without producing artifacts
typecheck:
  cargo check --workspace --all-targets

# Run tests
test:
  cargo test --workspace

# Supply-chain gate: advisories, licenses, bans, sources
audit:
  cargo deny check

# Each check runs once; `lint`, `typecheck` and `test` stay for ad-hoc use.
# Maintainer gate: just verify (fmt, clippy, workspace tests, frontend) + cargo deny
check: verify audit
  @echo "✓ all gates passed"

# --- Release / changelog (git-cliff + cocogitto) ---

# Regenerate CHANGELOG.md from conventional commits
changelog:
  git-cliff -o CHANGELOG.md

# Preview unreleased changelog section
release-preview:
  git-cliff --unreleased

# Verify all commits on this branch are conventional
verify-commits:
  cog check

# --- API contract (OpenAPI, generated by the backend) ---

# Export the live OpenAPI document from a running backend to openapi/openapi.json
openapi-export:
  curl -fsS http://localhost:4000/api/openapi.json -o openapi/openapi.json
  @echo "✓ wrote openapi/openapi.json"

# Lint the OpenAPI document with vacuum (https://quobix.com/vacuum)
openapi-lint:
  vacuum lint -d openapi/openapi.json
