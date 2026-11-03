# Local substrate (Podman)

One stack (GraphDB, Restate, Hayhooks, Postgres, Odoo and the `ekg-backend`
engine), brought up and down together. `up` is self-provisioning: it starts
the substrate, waits for readiness, ensures a valid Odoo API key (self-healing into
`.env`), seeds the supply-risk scenario, provisions the GraphDB `ekg` repo, then
builds, starts, and registers the backend last (once its Odoo key + GraphDB
repo exist). So one command leaves a fully working, registered end-to-end stack,
with no host `cargo run` and no manual register.

```bash
just podman-local-up     # substrate + provision + seed + key, then build/start/register the backend
just podman-local-logs   # tail
just podman-local-down   # stop (containers removed; named volumes kept)
just podman-local-wipe   # destructive full reset: also deletes all volumes
just podman-build        # (re)build localhost/ekg-<app>:local for every apps/*/Dockerfile
just test-e2e            # run the gated e2e suite against the running stack
```

## Endpoints

| Service | URL | Notes |
| ------- | --- | ----- |
| GraphDB Workbench | http://localhost:7200 | RDF/OWL store + KG viz |
| Restate ingress | http://localhost:8080 | invoke durable handlers |
| Restate admin API | http://localhost:9070 | deployments, invocations |
| Restate UI | http://localhost:9070/ui/ | built-in console (deployments, invocations, state) |
| Hayhooks (cognition runtime) | http://localhost:1416 | `/status`, `/v1/chat/completions` (model `generate`), `/embed/run`; image built locally, started with the backend |
| Odoo | http://localhost:8069 | source system |
| Postgres | localhost:5432 | Odoo's DB only, not a governed-truth store |
| NATS monitoring | http://localhost:8222 | read-only; `scripts/wait-settled.sh` reads the monitor bridge's backlog from `/jsz?consumers=true` |
| ekg-backend `/api` | http://localhost:8090 | governed webhook + human oversight (`/api/ingest/odoo`, `/api/action-points`, `/api/health`, `/api/ready`) |
| ekg-backend agent tool gateway | http://127.0.0.1:8091/mcp | MCP (streamable HTTP) agents read the world model through; every request needs a bearer task token the engine mints; loopback-only, for host tests; Hayhooks reaches it at `http://ekg-backend:8091/mcp`. Not part of `/api` |
| ekg-backend skill routes | http://127.0.0.1:8091/skills | the agent skill versions a task token pins (`GET /skills`, `/skills/{name}`, `/skills/{name}/files/{path}`), read-only, behind the same bearer check; see [Agent skills](#agent-skills) |

| Langfuse (opt-in) | http://localhost:3010 | the trace backend, only after `just telemetry-up`; see [Traces](#traces-opt-in) |

The backend's Restate SDK endpoint (`:9080`) is not host-published: Restate
dials it over the compose network at `http://ekg-backend:9080`.

## Traces (opt-in)

Nothing is exported, and no trace backend runs, unless you ask for it:

```bash
just telemetry-up        # start Langfuse and its stores, print three lines for .env
just podman-local-up     # (re)start the backend and the runtime so they read the new .env
just telemetry-down      # stop the trace backend again; its volumes stay
```

`just telemetry-up` starts the compose profile `telemetry`: Langfuse's web and worker
services, and a Postgres, a ClickHouse, a Valkey and a MinIO of their own. That is six more
containers and about 3.6 GB of images, and none of the stores is published on the host. It
logs you in at http://localhost:3010 as `operator@ekg.local` / `ekg-local-only`, with a
project and its keys already there. These credentials are for a machine of your own only.

With the three printed lines in `.env`, the backend, the runtime and `just agent-eval` export
their spans there. An agent run is one trace: the engine's run span, the runtime's steps and
model calls under it, each tool call and skill read through the gateway, and the GraphDB
requests each of them made. A span carries names, versions, outcomes, counts and timings, and
never a task, a prompt, a tool argument or result, a query or a token.

The engine sends traces and reads nothing back. A trace backend that is down, slow or refusing
costs one line in the log and nothing else: no run, tool call or grade waits for it.
`just podman-local-down` and `just podman-local-wipe` stop the trace backend too, and the wipe
removes its volumes.

Any other OTLP backend works the same way: set `OTEL_EXPORTER_OTLP_ENDPOINT` (and
`OTEL_EXPORTER_OTLP_HEADERS` if it wants a key). Only OTLP over HTTP is exported.

## Configuration

- **Model / OpenRouter creds**: repo-root `.env` (copy `.env.example`; set
  `MODEL_API_KEY`, `EMBED_API_KEY`). The `just` recipes pass it via `--env-file`.
  Only the Hayhooks service receives the key: the backend routes every model call
  through Hayhooks (`EKG_COGNITION_ROUTE=runtime`), and compose blanks the key for it.
  Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export Hayhooks' traces to an OTLP backend; with
  it unset nothing is exported.
  Defaults: chat `openai/gpt-oss-120b`, embeddings `qwen/qwen3-embedding-8b`.
  The chat model is configuration: set `CHAT_MODEL` to swap it, e.g.
  `qwen/qwen3-30b-a3b-instruct-2507` for a faster, non-reasoning option.
- **Odoo / Postgres creds**: `app.defaults.env` (local-only placeholders, never
  production secrets).
- **GraphDB license**: GraphDB 11 requires a license even for the free tier
  *before any triple write* (repo creation works without it, but data loads fail
  with `500 "No license was set"`). Obtain a free license from Ontotext
  (<https://www.ontotext.com/products/graphdb/>) and drop the file at
  `deploy/podman/local/graphdb.license`. It is gitignored (a credential) and
  mounted read-only into the container; `GDB_JAVA_OPTS` in `compose.yaml` points
  GraphDB at it (`-Dgraphdb.license.file=…`), so a file-based license takes
  priority over a Workbench upload and the substrate stays reproducible. The gated
  `ekg-semantic` integration test relies on this too (it writes to the licensed
  local store via `GRAPHDB_URL`).

## Starter cases

`just seed` (run by `up`) loads five cases, so a fresh stack shows both oversight paths:
plans that wait for a human and changes the agent applied under policy. Dates are relative
to the seed day; re-seeding updates the same records in place.

| # | Case | Path | Why |
| - | ---- | ---- | --- |
| 1 | VeloMax 10× E-Bike, three MOs on the Assembly Line, the rush order | human review (joint plan) | battery short; MOs over capacity; rush promised before it can be built |
| 2 | Bergwerk Bikes 25× Retrofit Kit · Hub Motor | human review (single order) | short 10 motors; the only inbound is an unconfirmed MotorWorks RFQ above the cost limit, after the promise |
| 3 | VeloMax 30× Drivetrain Service Kit | applied by the agent (best-effort) | short 20 gear sets; expediting the confirmed PartsHub line (150) brings it in on time |
| 4 | Urbanride Service 18× Frame Service Pack | applied by the agent (best-effort) | frames 3 below safety stock, demand covered; notify the owner or expedite the confirmed FrameCo line (180) |
| 5 | Stadtrad Leasing 40× City Bike S1, with an E-MTB T4 MO | human review (split portfolio) | 10 controllers arrive after the promise from an unconfirmed RFQ; the S1 and T4 work orders overlap on Assembly and Paint (days 17–18) |

`up` stages the first sweep so every order is judged against materialized stock and inbound:
it re-syncs every allowlisted model but `sale.order`, `sale.order.line` and `mrp.production`,
waits until the stack has settled (`scripts/wait-settled.sh`: Restate idle and the monitor
bridge drained), then runs the full sweep. `just test-e2e` waits on the same script.

Cases 3 and 4 depend on the model's proposal: a proposal the policy gates (e.g. a
re-promise) waits for a human with its reasons instead. A member of a portfolio is never
auto-applied on its own: it waits for its portfolio's joint plan.

**Demo auto-apply policy.** `compose.yaml` runs the backend with
`EKG_AUTOPROMOTE_ACTIONS=expedite_po,notify_owner`, `EKG_AUTOPROMOTE_COST_LIMIT=250` and
`EKG_AUTOPROMOTE_COST_LIMIT_CURRENCY=USD` whenever `.env` leaves them empty or unset. To gate
every plan, name no verb in `.env` (`EKG_AUTOPROMOTE_ACTIONS=none`) and restart the backend.
The backend's own default stays empty, so every plan is human-gated.

## Ingestion (governed source, materialize, notify)

The `ekg-backend` app hosts two HTTP surfaces: the public
`/api` Odoo webhook and the Restate SDK endpoint the runtime calls durable
handlers back into. An authenticated `POST /api/ingest/odoo` fat-pulls the record
and materializes it into the world model under durable execution; a reconciliation
sweep backfills missed changes and retracts archived records.

The source pull/write uses Odoo 19's json2 external API (`POST
/json/2/<model>/<method>`, `Authorization: Bearer <ODOO_API_KEY>`), the successor to
the `/jsonrpc` path deprecated in Odoo 19 and slated for removal in Odoo 22.
`ODOO_API_KEY` must therefore be an Odoo API key (`res.users.apikeys`, scope
`rpc`), not the login password. Mint one in the Odoo UI (Preferences > Account
Security > New API Key).

### Configuration (env)

| Var | Required | Default | Purpose |
| --- | --- | --- | --- |
| `EKG_WEBHOOK_SECRET` | yes | - | shared secret every webhook must present (header `x-ekg-webhook-secret`, constant-time compared) |
| `ODOO_API_KEY` | yes | - | Odoo API key (`res.users.apikeys`, scope `rpc`), sent as a Bearer token on the json2 external API (Odoo 19), not the login password |
| `EKG_INGEST_ALLOWLIST` | no | supply-risk models | comma-separated models the ingestor accepts/pulls/sweeps |
| `ODOO_URL` / `ODOO_DB` / `ODOO_USER` | no | `http://localhost:8069` / `ekg` / `admin` | Odoo connection |
| `GRAPHDB_URL` / `GRAPHDB_REPO` | no | `http://localhost:7200` / `ekg` | world-model store |
| `RESTATE_INGRESS_URL` | no | `http://localhost:8080` | where the endpoint submits durable work |
| `EKG_BACKEND_ADDR` | no | `0.0.0.0:8090` | public `/api` listen addr |
| `EKG_RESTATE_SERVICE_ADDR` | no | `0.0.0.0:9080` | Restate SDK endpoint listen addr |
| `EKG_TOOL_GATEWAY_PORT` | no | `8091` | the agent tool gateway's listen port (MCP at `/mcp`) |
| `EKG_TOOL_TOKEN_SECRET` | no | random per process | task-token signing secret, at least 32 bytes. Unset means a random per-process secret; `just podman-local-up` writes a shared one into `.env` so host tests can mint tokens. Several replicas must share one |
| `EKG_SERVICE_CALLBACK_URL` | no | host-run: `http://host.containers.internal:9080`; containerized: `http://ekg-backend:9080` | URL the runtime dials the backend at (registration); see the callback split below |
| `EKG_SWEEP_INTERVAL_SECS` | no | `300` | sweep self-reschedule interval |
| `EKG_SWEEP_BOOTSTRAP` | no | `true` | best-effort trigger the first sweep on startup. Not idempotent: every start against kept Restate state (e.g. each `just backend` restart) adds a sweep timer chain, so set `false` wherever a chain already runs (the containerized stack does), for tests and for multi-instance |
| `EKG_SIM_MAX_PROJECTIONS` | no | `2` | what-if projections the backend runs at once (each is a scratch GraphDB repository). GraphDB Free allows 5 repositories per server, and past that every concurrent import fails, so keep base + fixtures + this within 5 |
| `EKG_SIM_ORPHAN_MAX_AGE_SECS` | no | `900` | how old a what-if projection's scratch repository (`sim_*`) must be before the backend reclaims it at startup, as left behind by a killed process. Keep it above the longest time a projection is held (the slate budget plus its guard): a younger one may be another process's live projection |
| `EKG_COGNITION_MAX_RETRIES` | no | `3` | retries of a model call whose start met a 429, a 5xx or a connection error, with exponential backoff honouring `Retry-After` (a permanent `insufficient_quota` 429 is not retried) |
| `EKG_COGNITION_CONNECT_TIMEOUT_SECS` | no | `30` | how long a model call may take to start answering (its response headers); per attempt |
| `EKG_COGNITION_IDLE_TIMEOUT_SECS` | no | `30` | how long a streamed answer may go without a chunk before the call fails as `unavailable`; a slow but live answer is not cut off |
| `EKG_COGNITION_TOTAL_TIMEOUT_SECS` | no | `120` | the cap on one whole model call, once it holds its permit. It also bounds one agent run for a point |
| `EKG_AGENT_MAX_STEPS` | no | `12` | the step limit of one agent run for a point |
| `EKG_AGENT_JOINT_MAX_STEPS` | no | `8` | the step limit of a portfolio's planner run, which reads nothing and plans from its members' findings |
| `EKG_AGENT_JOINT_RUN_SECS` | no | `120` | the cap on a portfolio's planner run. At most `570`: the run's task token has to outlive it. The agent's joint slate is abandoned only after `EKG_SLATE_BUDGET_SECS` + this + `EKG_AGENT_ANALYST_RUN_SECS` + 30 s, and in `agent` mode a portfolio waits that long for it before the existing joint slate is shown |
| `EKG_AGENT_PLANNERS` | no | `3` | planners side by side in one round of a portfolio's slate, one per strategy, at most `3`. They count against the slate's attempts and the runtime's concurrency limit |
| `EKG_AGENT_PLANNER_READS` | no | `false` | gives a portfolio's planners read tools |
| `EKG_AGENT_ANALYST_MAX_STEPS` | no | `10` | the step limit of an investigator run, for a portfolio's member the semantic layer cannot decide |
| `EKG_AGENT_ANALYST_RUN_SECS` | no | `240` | the cap on an investigator run. At most `570`, as above. It shares the runtime's concurrency limit |
| `EKG_AGENT_TEXT_MAX_CHARS` | no | `80` | the most characters of a record's text (a name, a description) a tool hands to an agent run. A longer text is replaced by a note naming its length; IRIs, numbers and dates pass. A coupling reason in a portfolio's task is held to twice this. At least `20`. The world model and every other reader keep the full text |
| `EKG_COGNITION_MAX_CONCURRENT` | no | `2` | model generations in flight at once, process-wide (every agent shares one client); a burst of fresh points queues instead of tripping the endpoint's rate limit |
| `EKG_SLATE_BUDGET_SECS` | no | `150` | time budget for generating a slate's candidates. When spent, the loop stops and returns the baseline plus every option it already admitted, recording that the budget ended generation. The bridge abandons a hung loop only after budget + `EKG_COGNITION_TOTAL_TIMEOUT_SECS` + 30 s (logged) |
| `EKG_COMMAND_CLAIM_SECS` | no | `600` | how long a governed command (promote, auto-promote, undo, abort) holds its claim on an action point before the next command may take it over, audited as `claim_taken_over`. Must exceed the longest write-back: a plan's actions times the 60 s source request timeout, plus the rollback |

### Bring it up

```bash
cp .env.example .env     # first time only; set MODEL_API_KEY etc. as needed
just podman-local-up     # substrate + provision + seed + key, then build/start/register the backend
```

That is the whole flow: `just podman-local-up` leaves the stack fully provisioned
and the backend up and registered: no separate `graphdb-provision`/`seed`/key
step, no `just backend`, no `just restate-register`. It self-heals `ODOO_API_KEY`
(keeps the `.env` key if it still authenticates, else mints a fresh `rpc`-scoped
`ekg-local` key and writes it back), then builds `localhost/ekg-backend:local`,
starts the `ekg-backend` service (compose profile `backend`), waits for
`/api/ready`, registers the `http://ekg-backend:9080` deployment, publishes the agent
skills (see [Agent skills](#agent-skills)), and triggers the first reconciliation sweep. Re-runs rebuild layer-cached and re-register with
`force:true`, but each re-run's sweep trigger adds a timer chain (see below). Inspect deployments/invocations/state in the Restate
UI (<http://localhost:9070/ui/>).

The backend container starts with `EKG_SWEEP_BOOTSTRAP=false` (the script triggers
the first sweep *after* registering, avoiding a pre-registration 404). `just
sweep-trigger` is the manual kick for when no timer is running. Check first: list
the unfinished `Sweep/default/run` invocations in the Restate UI. One chain is a running
pass or a scheduled one (a running pass and the successor it already scheduled are the same
chain). No timer runs when:

- a gated test's cleanup reported the stack's sweep timer as paused (its message names how
  many triggers to send);
- a pass failed terminally (a terminal error ends the pass before it reschedules);
- the chain was cancelled by hand.

It is not idempotent: every pass reschedules its successor, so each extra trigger adds
another timer chain (more frequent passes). That includes `just sweep-trigger`, a re-run of
`just podman-local-up`, and every backend start with `EKG_SWEEP_BOOTSTRAP=true` (e.g. a
`just backend` restart) against a stack whose timer already runs. To remove a surplus chain
without wiping anything, cancel its pending `Sweep/default/run` invocation in the Restate UI
(or `PATCH /invocations/<id>/cancel` on the admin API), keeping one.

**Backfilling a newly mapped field.** The sweep only re-pulls records whose source
`write_date` moved past its watermark, so a field newly added to the mapping contract never
reaches records that have not changed at the source since. `just sweep-resync <models…>`
re-pulls and re-materializes every current record of the named allowlisted models through
the normal ingest path (provenance, revisions, validation), without touching the watermark or
the timer; a model outside `EKG_INGEST_ALLOWLIST` is refused (HTTP 400, naming it). It runs
alongside a sweep pass rather than queueing behind it. Every call carries a printed
idempotency key, bound to the requested models (it ends in their digest, so reusing it for
other models starts a new invocation). If the recipe gets no answer (no response within
`EKG_RESYNC_TIMEOUT_SECS` (default 120), or the connection dropped after sending), it fails
with an *unknown* outcome. Re-run it with `EKG_RESYNC_IDEMPOTENCY_KEY=<the printed key>`
(and a larger timeout) to attach to that same invocation instead of starting a second re-sync,
or inspect it in the Restate UI. Examples:
`just sweep-resync stock.picking` (receipt completion: `ekg:completed_at`),
`just sweep-resync purchase.order mrp.production` (references: `ekg:name`),
`just sweep-resync purchase.order.line` (received quantity: `ekg:qty_received`),
`just sweep-resync product.supplierinfo` (sourced variant: `ekg:supplies_product`).

#### Agent skills

Agent skills are authored in git under `apps/hayhooks/skills/`, but versioned and
switched at runtime in the durable `SkillRegistry` (a Restate object on the backend
deployment). A version is the content digest of the skill's files, so it never changes once
published. The engine's tool gateway serves a task exactly the versions its token pins
(`/skills` on `:8091`, bearer task token), so switching a version never changes a running task.

```bash
just skills-validate                                   # offline: every skill vs the engine's tool profiles
just skills-publish                                    # validate, then publish (idempotent)
just skills-list                                       # versions, the active one, and the history
just skill-activate propose-plan 1a2b3c4d tighter target guidance   # activate a version
just skill-activate propose-plan 9f8e7d6c rollback: v2 loops        # an earlier one = rollback
```

- `just podman-local-up` publishes right after it registers the backend. After a wipe each
  skill's first version becomes active; on a warm stack unchanged skills are a no-op.
- A changed skill is published inactive: a deploy never silently changes agent
  behaviour. Activate it explicitly with `just skill-activate <skill> <version> <reason>`
  (a full version id or a unique prefix of at least 8 characters, from `just skills-list`).
  Activating an earlier version is the rollback; no version is ever deleted.
- Every activation (a first version's automatic one included) is recorded with who, why and
  when. Who is `EKG_OPERATOR`, else `git config user.name`, else `$USER`.
- The recipes reach the registry over `RESTATE_INGRESS_URL` (default `http://localhost:8080`);
  nothing is added to `/api`. Wiping Restate (`just podman-local-wipe`) forgets the history.

#### Agent rollout (shadow)

The tool-using agent runs in the cognition runtime, reads only through the tool
gateway, and hands its plan in with `submit_plan`. It is rolled out shadow first: it plans
on the same freshly opened points and freshly formed portfolios as the existing path, and its
slates are recorded apart: they never reach a point, a portfolio, the operator UI, `/api` or a
write-back. The mode is a governed, durable
setting (the `AgentRollout` Restate object):

- `legacy`: the default, and what a fresh stack runs: no agent run starts.
- `shadow`: until a window end, for at most a number of shadow slates and of tokens. When a
  bound is reached, shadow stops by itself and `just agent-status` says why
  (`window_closed`, `budget_exhausted`).

```bash
just agent-status                                      # mode, budget use, why it stopped, history
just agent-mode shadow --until +8h --max-slates 5 --max-tokens 200000 --reason "collect shadow evidence"
just agent-mode legacy --reason "stop shadow"          # the kill switch
just agent-shadow-list                                 # the subjects and portfolios with shadow records
just agent-shadow-list https://ekg.example/id/sale.order/1   # one subject's records (JSON)
just agent-shadow-list portfolio/0123456789ab          # one portfolio's joint records (JSON)
```

- `--until` is a UTC date or date-time (`2027-10-14T18:00:00`) or a span from now (`+90m`,
  `+8h`, `+2d`); it must be in the future.
- **Kill switch:** setting `legacy` stops new shadow runs, and a run in flight stops before its
  next candidate and is recorded `aborted`. Every switch is recorded with who, why and when (who:
  `EKG_OPERATOR`, else `git config user.name`, else `$USER`).
- **Portfolios are shadowed too.** A portfolio that gets a joint slate also gets the agent's
  joint slate, recorded only: one agent task plans for all the members, under the `portfolio`
  tool profile (the two capability profiles composed, each record read as its own capability
  reads it). Its record is kept under the portfolio's id, names the generation and the
  members, holds both joint slates, and receives the decision on the portfolio (promoted,
  dismissed, superseded, undone). The members' own slates after a failed joint slate, and the
  slate of a point released from a portfolio, are not offered to the agent.
- **The budget is shared.** A joint slate counts as one slate of `--max-slates`, and its tokens
  against `--max-tokens`, like a point's. It is up to four agent runs, each with both
  capabilities' tools and more to read, so a window with portfolios spends its token budget
  faster. Size the budgets for it.
- **Shadow records** (the `AgentShadow` Restate object, the last 20 per subject or portfolio) hold the
  agent's slate, one run record per candidate (termination, steps, tokens, tool calls by
  outcome, the defects of each rejected submission) and the version tuple (engine, model, pinned
  skills, tools). `just agent-shadow-list` reads them with Restate's own introspection
  (`restate kv get` / `restate sql`, the CLI inside the Restate container).
- Shadow runs one slate at a time by default (`EKG_AGENT_SHADOW_CONCURRENCY`), apart from the
  existing path's model calls; an agent run takes at most `EKG_AGENT_MAX_STEPS` steps (default
  12). A portfolio's findings come from the semantic layer; an investigator runs only where it
  cannot decide, at most `EKG_AGENT_ANALYST_MAX_STEPS` (default 10) within
  `EKG_AGENT_ANALYST_RUN_SECS` (default 240). Planners run per round, each at most
  `EKG_AGENT_JOINT_MAX_STEPS` (default 8) within `EKG_AGENT_JOINT_RUN_SECS` (default 120). The agent uses `AGENT_MODEL` / `AGENT_MODEL_API_KEY` on the runtime, falling back to
  `CHAT_MODEL` / `MODEL_API_KEY`; a separate key keeps shadow's spend and rate limit apart.
- Shadow needs the runtime route (`EKG_COGNITION_ROUTE=runtime`, the default); on the direct
  route the backend logs it as unavailable and runs none.

In `agent` mode (see `evals/agent/README.md` for the criteria, the sign-off and the rollback
triggers) a portfolio's joint slate depends on what the mode was entered under:

- **Criteria without joint bounds**, as every sign-off given before joint slates were measured:
  the portfolio's joint slate stays the existing path's, and the agent's is only recorded. Its
  record says so.
- **Criteria with joint bounds** (`joint:` in `promotion.yaml`, format 2): a freshly formed
  portfolio carries the agent's joint slate when its run completed in time, and the existing
  path's otherwise. No sign-off carries over: joint bounds are registered, evidenced (a paired
  eval of the set with its joint cases, a shadow window with joint records) and signed off anew.

A joint run counts for the rollback triggers like a point's run: its version against the signed
one, its failure in the row of failures, and the undo of a promoted portfolio whose joint slate
was the agent's as a reversed promotion.

**Deploying a new skill version ends a running `agent` mode.** A changed skill is published
inactive; once it is activated, the next agent run reports another skill version than the
signed one and the mode rolls itself back to `legacy` (`version_changed`). The change that
added joint slates changed `propose-plan` and `process-work-centres` (they are now also written
for the `portfolio` profile) and added `plan-portfolio`, so activating them is expected to do
exactly that. Re-entering `agent` takes the usual procedure: evidence under the new versions
and a sign-off.

#### Host-dev alternative (iterating on backend code)

To run the backend from the host against the running substrate instead of the
container:

```bash
just backend             # both surfaces on the host (reads ODOO_API_KEY + EKG_WEBHOOK_SECRET from .env)
just restate-register    # register host.containers.internal:9080 (the default)
```

#### Callback split (who registers what, on which port)

The Restate runtime dials a backend's SDK endpoint back at a registered URL. There
are two distinct conventions on distinct ports, so they never overwrite each
other:

| Backend | Callback URL registered | Registered by |
| --- | --- | --- |
| Containerized (`ekg-backend` service) | `http://ekg-backend:9080` (compose DNS) | `just podman-local-up` |
| Host-run (`just backend`) | `http://host.containers.internal:9080` | `just restate-register` |
| Gated in-process-endpoint tests (drift, propose and others) | `http://host.containers.internal:9091+offset` | the test's `common::spawn_endpoint` |

The containerized backend and the host-run gated tests use different ports
(`9080` vs `9091+offset`), so a running containerized stack and a test run coexist
without cross-registration. `transport_integration_it` drives the published `/api`
at `http://localhost:8090` and needs no endpoint of its own.

### Odoo webhook

Configure an Odoo Automation Rule (Settings > Technical > Automation Rules) on
each allowlisted model to `POST` to `http://<host>:8090/api/ingest/odoo` with body
`{"_model": "<model>", "_id": <id>}` and header `x-ekg-webhook-secret:
$EKG_WEBHOOK_SECRET`. The body is untrusted (thin-notify): the fact is always the
governed re-read the worker performs.

Models with a source scope in the mapping contract must use it as the rule's filter
domain, so out-of-scope records are never pinged (the worker would only refuse them as not
found):

| Model | Automation-rule filter domain |
| --- | --- |
| `stock.quant` | `[("location_id.usage", "=", "internal")]` (only internal stock is on hand) |
| `stock.picking` | `[("picking_type_code", "=", "incoming"), ("location_dest_id.usage", "=", "internal")]` (only receipts into stock deliver; a return never does, whether to a vendor or to inter-company transit, even when the receipt type has no return type and the return reuses it) |

### Gated live tests

```bash
cargo test -p ekg-e2e -- --ignored --nocapture   # the backend live suites (e2e/)
```

They provision a throwaway GraphDB repo, serve the worker in-process, register it,
and skip if a hop is unreachable, except that a suite on the loud-skip helper fails
instead when `EKG_E2E_REQUIRE_INFRA=1` is set, as `just test-e2e` does (that recipe ends by
listing any `SKIP` the other suites still printed). The five suites that claim `Sweep` from
the running stack (`sweep_e2e_it`, `sweep_bootstrap_it`, `sweep_resync_e2e_it`,
`sweep_resync_shared_e2e_it`, `sweep_scope_e2e_it`) note which deployment serves `Sweep`,
pause the stack's timer while they run, and afterwards, even on a failure or a hang, cancel
what they started and re-arm the timer on that same deployment; a cleanup that cannot fails
the test and names the remedy. `sweep_bootstrap_it` runs its pass under the stack's own key
(it tests the startup trigger), so that pass also advances the stack's sweep watermark over
partners only; changes to other models older than it are then left to their webhooks.

## Data

Durable data lives in podman-managed named volumes (`graphdb-data`,
`restate-data`, `postgres-data`, `odoo-data`); there are no host `./.data/` folders.
Wipe everything with:

```bash
just podman-local-wipe   # down -v: deletes all volumes (Odoo DB, seed, minted key)
```

The next `just podman-local-up` re-provisions from scratch: it re-seeds Odoo and mints
a fresh `ODOO_API_KEY`.
