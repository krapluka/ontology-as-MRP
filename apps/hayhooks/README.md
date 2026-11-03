# Hayhooks: the cognition runtime

Serves the Haystack pipelines under `pipelines/` with
[Hayhooks](https://github.com/deepset-ai/hayhooks). The engine routes every model call through
this runtime by default (`EKG_COGNITION_ROUTE=runtime`).

Cognition is provider neutral: every pipeline targets an OpenAI-compatible endpoint, so
the same image runs against OpenRouter (the default), a local Ollama (`/v1`), vLLM, or any gateway.
You switch providers by environment, not code.

## Pipelines

| Pipeline   | Endpoint                                     | Request                                  | Response                                                    |
| ---------- | -------------------------------------------- | ---------------------------------------- | ----------------------------------------------------------- |
| `generate` | `POST /v1/chat/completions`, `model: "generate"` | OpenAI chat request, `stream: true`  | OpenAI chunks, a finish marker, then a final chunk with `usage` |
| `embed`    | `POST /embed/run`                            | `{ "texts": [str, ...] }`                | `{ "result": { "vectors": [[float]], "dimension": int } }` |
| `agent`    | `POST /agent/run`, `Authorization: Bearer <task token>` | `{ "task": str, "max_steps": int }` | `{ "result": { "termination", "steps", "input_tokens", "output_tokens", "model" } }`, plus `closing_answer` (the agent's last words, at most 2,000 characters) when the run ended `no_plan` |

The chat and embedding models are runtime configuration. The engine never chooses them.

**Governance:** the runtime only produces text and vectors, and runs agents that read and
propose. It writes nothing. Grounding, the plan check and every governed write stay in the
engine.

## Contract with the engine

- **The engine owns every retry and time bound.** The provider clients never retry
  (`max_retries=0`). Their timeout (`PROVIDER_TIMEOUT_SECS`, default 150) stays at or above the
  engine's total cap.
- **Provider failures pass through.** A failure before the first chunk is answered with the
  provider's own status, `Retry-After` / `X-RateLimit-Reset` and error object (as FastAPI
  `detail`), so the engine classifies it exactly as on the direct route. An unreachable or slow
  provider gives 503. A failure after the stream began closes the stream without a finish
  marker, and the engine never accepts that as an answer.
- **No durable state.** The runtime runs in live mode only (`HAYHOOKS_DURABLE_MODE=false`). The
  engine's Restate journal is the one durable record.
- **The runtime holds the credential.** `MODEL_API_KEY` is configured here; the engine reaches
  the runtime without it.

## Tools: the engine's MCP gateway

An agent in the runtime reads the governed world model only through the engine's agent tool
gateway: MCP over streamable HTTP at `http://ekg-backend:8091/mcp`, never GraphDB directly. The
runtime holds no data credential. The engine mints a short-lived task token per reasoning
task, naming the task's profile, subjects and allowed tools. The agent builds one toolset per
run with that token as its bearer and closes it after:

```python
from haystack_integrations.tools.mcp import MCPToolset, StreamableHttpServerInfo

toolset = MCPToolset(
    StreamableHttpServerInfo(url="http://ekg-backend:8091/mcp", token=task_token),
    eager_connect=False,
)
toolset.warm_up()      # lists exactly the tools the token allows
try:
    ...                # hand `toolset` to the agent for this run
finally:
    toolset.close()
```

The client is `mcp-haystack` (pinned in `requirements.in`); the `agent` pipeline uses it. Without
a valid token the gateway answers 401 and `warm_up()` fails. Every tool is read-only, filtered by
the read policy, and scoped to the task's subjects.

## Skills: authored here, served by the engine

A skill is a procedure an agent follows, as editable text. It is authored and reviewed
here, under `skills/<name>/`, but the runtime never reads this directory: skills are published
into the engine's durable skill registry, activated there, and served to each task by the
engine's gateway. The skills are `propose-plan` (every planning profile),
`process-work-centres` (the `process` and `portfolio` profiles), `plan-portfolio` (the `portfolio`
profile: one plan for several coupled subjects, from their findings), `analyse-member` (the
analyst profiles: an investigator's typed finding about a member the semantic layer cannot decide)
and `analyse-work-centres` (the `process-analyst` profile).

```text
skills/<name>/
  SKILL.md          ---
                    name: <name>                       # lowercase kebab-case, = the directory
                    description: <one line>            # shown to the agent up front
                    profiles: [supply-risk, process]   # the tool profiles it is written for
                    tools: [ground_entity, submit_plan]
                    ---
                    <markdown instructions>
  reference/*.md    optional bundled text files
```

- **Frontmatter** is exactly those four keys, as `key: value` lines, with `profiles` and `tools`
  as `[a, b]` lists. Anything else is refused, naming the line.
- **Files** are UTF-8 text with relative paths, at most 64 KiB each and 256 KiB per skill.
- **Drift check.** A skill may name only registered profiles, and only tools those profiles
  offer. `just skills-validate` checks every skill, and the full gate runs the same check
  (the backend's `skills_drift` suite), so renaming or removing a tool fails the PR.
- **Guidance, not rules.** State a rule only if a tool also enforces it. The tools enforce scope,
  the read policy, the verb catalog (`submit_plan`'s schema) and the plan bound; a badly written
  skill gives a worse plan, never an unsafe one.
- **Shipping a change.** `just skills-publish` registers a changed skill as a new, inactive
  version; `just skill-activate` makes it active (see `deploy/podman/local/README.md`).

An agent loads skills per run with the run's task token, which pins the skill versions active
when the engine minted it. `common/ekg_skills.py` provides the store:

```python
from ekg_skills import EngineSkillStore
from haystack.tools import SkillToolset

skills = SkillToolset(EngineSkillStore("http://ekg-backend:8091", task_token))
# load_skill / read_skill_file serve exactly the token's pinned versions
```

A skill the token does not pin raises `KeyError`, and a missing bundled file raises
`FileNotFoundError`. The store never serializes the token: `to_dict` keeps the URL only. The
`agent` pipeline uses it.

The plan itself is handed in through the gateway's `submit_plan` tool (in the `MCPToolset`).
It checks the whole plan against the profile's catalog, the task's scope and the per-plan bound,
and returns it normalized. A rejected plan is a tool error listing every defect, so the agent
can fix them all and submit again. Nothing is written: the plan is a proposal.

## The `agent` pipeline: one tool-using agent task

The engine's agent proposer runs one agent task per slate candidate here. The engine mints a
task token for the subject's capability profile, or for a portfolio's joint slate one token
for all its members under the `portfolio` profile, pinning the active skills, and posts
`{task, max_steps}` with the token as `Authorization: Bearer`, never in the body, because
Hayhooks logs request bodies. A request without a bearer is refused with 401.

Per run, the pipeline builds a Haystack `Agent` over three things, all scoped by that token:

- an `MCPToolset` on the engine's gateway (`EKG_TOOL_GATEWAY_URL` + `/mcp`), with the token as a
  `Secret` so the agent's span cannot export it;
- a `SkillToolset(EngineSkillStore)`, serving the pinned skill versions;
- `exit_conditions=["submit_plan", "submit_finding"]` and `max_agent_steps=max_steps`.

`submit_plan` accepting a plan, or `submit_finding` a finding, ends the run; a rejected one is a
tool error the model fixes and resubmits. A token holds one of the two. The answer maps what
`Agent.run()` already reports: `exit_reason` (either tool becomes `submitted`, `max_agent_steps`
becomes `step_limit`, a reply cut off by the provider `length`, any other reply without a plan
`no_plan`, an exception `error`), `step_count` and `token_usage`, plus the model. The toolset is closed after
each run, and nothing of the run is kept.

The plan of record comes from the engine, not from this answer. The gateway records, per
token, every tool call's outcome and the plan `submit_plan` accepted; the engine takes the plan
from that ledger after the run. Whatever the agent writes in its reply never becomes a plan.

A provider failure is answered with the provider's status, as on the `generate` pipeline, so the
engine classifies it the same way. The gateway URL and the model are runtime configuration, never
taken from a request.

## Config (env)

| Var                                                | Purpose                                   | Default                                       |
| -------------------------------------------------- | ----------------------------------------- | --------------------------------------------- |
| `MODEL_BASE_URL` / `MODEL_API_KEY` / `CHAT_MODEL`  | chat endpoint (`generate`)                | OpenRouter, `openai/gpt-oss-120b`            |
| `EMBED_BASE_URL` / `EMBED_API_KEY` / `EMBED_MODEL` | embeddings endpoint                       | OpenRouter, `qwen/qwen3-embedding-8b` (base and key fall back to `MODEL_*`) |
| `AGENT_MODEL` / `AGENT_MODEL_API_KEY`              | the `agent` pipeline's model and key (a separate key can carry its own spend limit) | fall back to `CHAT_MODEL` / `MODEL_API_KEY` |
| `EKG_TOOL_GATEWAY_URL`                             | the engine's tool gateway the agent reads through | `http://ekg-backend:8091`             |
| `PROVIDER_TIMEOUT_SECS`                            | the runtime's limit per provider call     | `150`                                         |
| `OTEL_EXPORTER_OTLP_ENDPOINT`                      | OTLP endpoint; telemetry is off when unset     | unset                                    |
| `OTEL_EXPORTER_OTLP_HEADERS`                       | headers of every export (a backend's key), `key=value` pairs, values percent-encoded | unset |
| `OTEL_EXPORTER_OTLP_PROTOCOL`                      | how spans are exported; the engine exports over HTTP, and Langfuse takes nothing else | `http/protobuf` (compose) |
| `OTEL_SERVICE_NAME`                                | service name on exported spans            | `ekg-cognition-runtime` (compose)             |
| `HAYSTACK_CONTENT_TRACING_ENABLED`                 | prompt and answer text in spans           | `false`                                       |

**Telemetry.** With an OTLP endpoint set (for example a Langfuse or other OTLP backend), each call's
spans continue the engine's `traceparent` and carry the model and token counts (`gen_ai.*`). With
content tracing off they carry no prompt or answer text. With no endpoint set, nothing is
exported. The engine exports its own spans to the same endpoint, so an agent run is one trace:
the runtime's spans hang under the engine's run span.

## Image and lock

The image is built here, not pulled: `Dockerfile` installs only the hash-locked
`requirements.txt` (wheels only) and bakes the pipelines in, so one tag always serves the same
code over the same dependencies.

```bash
just podman-build     # builds localhost/ekg-hayhooks:local (with every other app image)
just hayhooks-lock    # regenerate requirements.txt from requirements.in (in a container)
```

Edit the top-level pins in `requirements.in`, run `just hayhooks-lock`, then rebuild. `common/`
holds code the pipelines share (provider-failure mapping).

## Run

Via the local stack: `just podman-local-up` builds the image and starts it with the backend (see
`deploy/podman/local`).

Standalone:

```bash
podman run --rm -p 1416:1416 -e MODEL_BASE_URL -e MODEL_API_KEY -e CHAT_MODEL \
  localhost/ekg-hayhooks:local
```

`GET /status` lists the pipelines (`agent`, `embed`, `generate`) and reports `durable_mode`.
