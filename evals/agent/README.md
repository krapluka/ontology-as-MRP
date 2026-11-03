# Agent eval set

Fixed cases for measuring the tool-using planning agent, and the checks that grade it. The
prompt-based path that produces today's proposals runs on
the same cases as a second arm. `DATASET.md` says what the set covers and what it does not.

```
set.yaml         the set's version and its categories
set.lock         the version and the digest of cases/ (written by `just agent-eval-lock`)
cases/<id>.yaml  one case per file
promotion.yaml   what the evidence must show before the agent may be considered for operators
DATASET.md       the dataset card
```

## Running it

A run calls the real model and spends tokens. No gate runs it.

```bash
just podman-build          # once: the runtime image
podman compose -f deploy/podman/local/compose.yaml up -d graphdb   # or `just podman-local-up`

just agent-eval                                     # every case, 3 trials each
just agent-eval --case late-inbound-single-line     # one case
just agent-eval --category should_abstain --trials 1
just agent-eval --profile process                   # the cases of one profile
just agent-eval --arm legacy                        # the prompt-based path on the same cases
just agent-eval --arm both                          # both arms, one paired report
```

It needs GraphDB, the runtime image and `MODEL_API_KEY`. It needs neither Restate nor Odoo, and
is best run with only GraphDB up: every case and every scored plan gets a repository of its own,
which costs GraphDB memory the full stack may not leave free. It never touches the stack's data:
each case is loaded into a repository of its own (`eval_*`), which is removed when the case ends.

The report goes to `.artifacts/agent-eval/<time>.json`: every trial's plan, score, grade and
run record, pass^k per case and slice, and what the run was measured with (engine version,
model, skill versions, tools, set version and digest, `k`, the step limit, the joint step
limit and the analyst step limit). A joint trial is one round: its planners side by side (`EKG_AGENT_PLANNERS`) on the semantic
findings, graded on the plan the case's scorer ranks best, or the engine's decline, with no model
call. The trial holds every run's record and the findings, and its tokens are their sum. A trial
whose investigator handed in no finding ends exhausted, and so does a run the provider cut off for length. The run record of a trial that ended without a plan holds `closing_answer`: what the
agent said last, cut to 2,000 characters. It is there to read a decline afterwards and takes
no part in grading.

A run stops starting trials once it has used `--max-tokens` (default 2,000,000) and is then
reported as incomplete. So is a run in which a trial still ended in an error after its last
attempt. An incomplete run exits non-zero.

Two runs are compared by their reports: nothing is committed as the last good run.

### Retries

A trial that ends in an error is attempted again, up to `--retries` further times (default 2,
`--retries 0` for none). An error is a run that could not be carried to an end (the model or
the runtime did not answer, stalled or was rate-limited) or an accepted plan that could not be
scored. A graded trial is never repeated: a pass, a fail, a rejection and a violation each
stand. An errored attempt has no grade, so a retry re-rolls no result.

The tokens of every attempt count towards `--max-tokens`, and no further attempt starts once the
bound is reached. The report keeps each errored attempt with its reason on the trial
(`errored_attempts`), the table lists them, and the run ends with a line per arm naming the
trials that were attempted again. A case that needs retries in every run is worth a look: the
provider may stall more often on what the model finds hard.

### Trials at the same time

Up to `--concurrency` trials of one case run at once (default 3). The cases still run one after
another. The grades, the results per case and slice and the order of trials in the report do
not depend on it. `--concurrency 1` is the sequential run.

A case in flight holds its own repository and one more per plan being scored, and GraphDB Free
allows five on a server. Before the first case the run counts the server's repositories and
lowers the bound to what there is room for, saying so; with the full stack up it usually runs
two at a time. It refuses to start when there is no room for a case and one simulation, naming
the repositories that occupy the server. `EKG_GRAPHDB_MAX_REPOSITORIES` tells it about a
server with another limit.

The report states the bound it used and the attempts it allowed (`run`). They are not part of
what the run was measured with: two runs that differ only in them are the same evidence.

### Narrowed runs

`--case`, `--category` and `--profile` (each repeatable, and combinable) run only the cases
they name. Use one to check a change quickly: an edit to a skill only the `process` profile
loads does not need the supply-risk cases to run.

A narrowed run is a look, not a record. It is no evidence for a promotion verdict, which
needs one complete paired run of the whole set. Its
report says what it was narrowed to (`narrowed_to`), and the run ends by naming the cases it
left out.

## Reading a run in Langfuse

A run can be read and compared with others in a trace backend. Nothing changes in how a run is
started or graded, and the report is the same with or without it.

```bash
just telemetry-up     # once: a local Langfuse on http://localhost:3010; put the printed lines into .env
just agent-eval       # as before
```

With trace export on, a run is an experiment run in Langfuse (under Experiments), one per arm,
named by the set version, the arm and when it started. Every trial attempt is one trace of it:
the case's id is its input, the engine's grade (`passed`, `failed`, `violation`, `error`) its
output, and under it are the agent's run, the runtime's steps and model calls, the tool calls
and the GraphDB requests, each with its timing. The experiment run carries what the report's
version tuple carries: the engine version, the model, the skill versions, the tools, the set's
version and digest, `k`, both step limits, and whether the run was narrowed. The report names
the experiment run of each arm (`experiment`), so a report leads to its traces.

What goes to Langfuse is ids, names, versions, grades, counts and timings. No case record, no
task text and no plan is sent, and no dataset is created there: the cases stay here. Langfuse
grades nothing and nothing is read back from it; the promotion verdict reads the report file.

## The arms

`--arm` (or `EKG_AGENT_EVAL_ARM`) chooses what is measured. Both arms get the same cases, the
same worlds, the same graders and the same `k`.

| Arm | What a trial is |
|---|---|
| `agent` (default) | One run of the tool-using agent through the gateway and the runtime. On a joint case, findings from the semantic layer, an investigator for a member it cannot decide, then a round of planners under the `portfolio` profile. |
| `legacy` | One call of the bounded prompt agent, the path that produces today's proposals: one prompt, one reply, parsed by the engine. On a joint case, one call of the joint prompt that drafts a portfolio's joint slate today. |
| `both` | The legacy arm on every case, then the agent, written as one paired report. |

A legacy trial ends `submitted` when the engine's parser accepts the reply, `declined` when the
reply proposes no action, and `rejected` when the parser refuses it (a verb outside the catalog,
a target outside the evidence, wrong parameters, too many actions). A rejected trial fails its
case. It is not a decline, a violation or an error, and it does not make the run incomplete.

The legacy arm runs `CHAT_MODEL` through the same runtime container and starts no gateway. The
prompt path reports no token usage and has no skills, tools, step limit or run records; a
legacy report says so and does not show them as zeros. `--max-tokens` bounds the agent only:
a legacy run is bounded by its call count, the cases times `k`.

A paired report holds both arms' reports and, per case, each arm's passes out of `k`, each arm's
violations, and whether both arms pass at `k`, one does, or neither. It gives pass^k per
category and profile for each arm and counts the cases on which the arms differ. It is
incomplete when either arm's run is. It sets no threshold and does not say which arm is good
enough.

## Promotion criteria

`promotion.yaml` states, ahead of the evidence, what a paired run and the shadow records must
show. Neither report has a threshold of its own; this file is where the thresholds are.

```yaml
version: 2                   # 1 is still read; only 2 may state `joint`
eval:
  set_version: 3             # required: the set these numbers were written for
  min_trials: 3              # required: k at least
  min_passed: 17             # required: cases the agent passes at k
  min_passed_by_category: { should_abstain: 2 }
  min_passed_by_profile: { process: 6 }
  max_violating_cases: 0     # cases in which a trial of the agent is a violation
  max_legacy_only_cases: 0   # cases the legacy arm passes at k and the agent does not
shadow:
  min_compared: 20           # required: compared records; below it no percentage is evaluated
  max_agent_failed_percent: 10      # of all records: the agent's run failed or was aborted
  max_legacy_better_percent: 20     # of the compared: the existing path's best ranks better
  min_promotions: 5                 # promotions taken on the paired slate
  min_executed_offered_percent: 60  # of those: the executed plan was offered by the agent
joint:                              # optional: with it, the criteria cover joint slates
  eval:
    min_passed: 3                   # required: joint cases the agent passes at k
    max_violating_cases: 0
    max_legacy_only_cases: 0
  shadow:                           # the same bounds as `shadow`, over joint records
    min_compared: 5                 # required
    max_agent_failed_percent: 20
    max_legacy_better_percent: 20
    min_promotions: 2
    min_executed_offered_percent: 50
rollback:                           # optional, as is each bound: none stated, none triggers
  max_consecutive_agent_failures: 2 # agent runs that may fail in a row in `agent` mode
  max_reversed_promotions: 0        # promotions of an agent slate that may be reversed
```

An unknown field, a missing required field or a percentage above 100 is refused. The file is
identified by a digest of its content, so any edit makes it another document. A version 1
document is read as before and keeps its digest; in it, `joint` is an unknown field.

The bounds count apart. `eval` counts every case of the set, `joint.eval` the joint cases
only. `shadow` counts the records of points, `joint.shadow` the joint records of portfolios.
Criteria without `joint` are judged as if no joint record existed, and they do not cover joint
slates: in `agent` mode entered under them, a portfolio's joint slate stays the prompt path's.

The file counts once it is registered, and only for evidence gathered afterwards:

```bash
just agent-criteria-register --reason "before the first window"   # needs the stack up
just agent-criteria                                               # what is registered
```

- **A shadow window** is tied to the registration that was active when it opened
  (`just agent-status` shows the digest on that entry of the history). A window opened without
  one, or under an earlier one, is evidence for nothing. A registration made while a window is
  open does not reach that window.
- **A paired run** (`--arm both`) writes the digest of the `promotion.yaml` that was on disk
  when it started into its report. A report cannot carry the digest of a file written after it.
  This shows the run came after the criteria were written, not that it came after they were
  registered.

```bash
just agent-promotion-verdict --eval .artifacts/agent-eval/<time>.json   # needs the stack up
```

The verdict prints one line per criterion: what is required, what was observed, and `holds`,
`FAILS` or `NO EVIDENCE`. The paired report counts when it carries the registered digest, is
complete, covers the whole set at the criteria's set version, and ran at least `min_trials`.
Shadow records count when their run started inside a window tied to the registration; the
others are counted as left out. All evidence has to be of one engine version, model and set of
skill versions. Missing evidence is a criterion that does not hold, not an error.

The verdict reads only, and it decides nothing. It says whether the registered criteria are met
and exits non-zero when they are not. It switches no mode.

### The sign-off

```bash
just agent-signoff --eval .artifacts/agent-eval/<time>.json --reason "reviewed the verdict"
```

A sign-off is an operator's recorded acceptance of the evidence. The command computes the
verdict as above and records it with the rollout: the verdict, the digest of the criteria and of
the report file, the engine version, model and skill versions the evidence is of, who signs, why
and when. `just agent-status` lists every sign-off.

It is refused, and records nothing, when:

- the verdict is not met (the refusal names the criteria that do not hold);
- the verdict is for other criteria than the registered ones;
- the signer is the one who registered those criteria: another operator has to sign.

There is no override. If the criteria are wrong, register new ones and gather evidence under
them. The engine compares operator names (`EKG_OPERATOR`); it cannot know that two names are two
people.

A sign-off switches nothing.

### The `agent` mode

```bash
just agent-mode agent --until +2d --max-slates 40 --max-tokens 900000 --reason "criteria met and signed"
```

The switch is refused unless the latest sign-off is for the registered criteria. Its entry in
the history names the sign-off it was entered under. The window end and the two budgets work as
in `shadow`: when one is reached, no further agent run starts, and the mode stays `agent`.

In `agent` mode a freshly opened point gets the agent's slate as its slate:

- The prompt path's slate is still produced beside it and recorded with the run, as in `shadow`,
  so the two stay comparable. The decision on the point lands on that record too.
- The point waits for the agent's run, within the slate guard. Its slate's trace ends with a
  step that says the agent produced it, with the engine version, model and skill versions.
- The point gets the prompt path's slate instead when the agent's run fails, is stopped,
  exceeds its guard, is not allowed to start (window closed, budget spent), or is of another
  version than the signed one. The record says which slate was shown and why
  (`just agent-shadow-report` shows it per record and counts both). No point is left without a
  slate because of the agent.

What does not change:

- **A point's proposal still comes from the prompt path**, and a policy's auto-promotion acts
  on that proposal. No plan of the agent is written back without a person promoting it.
- A portfolio's joint slate comes from the prompt path, and the agent's is only recorded,
  unless the mode was entered under criteria that state `joint` bounds. Then a freshly formed
  portfolio gets the agent's joint slate under the same rules as a point: within the slate
  guard, with a trace step naming the agent after the step that says why the members are
  coupled, and the prompt path's joint slate in every case where a point would get the prompt
  path's slate. When neither joint slate can be recorded, the members get their own slates.
- The slates of members after a failed joint slate, and of points released from a portfolio,
  come from the prompt path.
- Records of an `agent` window are not evidence for the verdict: only `shadow` windows count.

### Rollback

In `agent` mode the rollout switches itself back to `legacy` when:

| Trigger | When |
|---|---|
| `version_changed` | an agent run reports another engine version, model or version of a skill than the signed one (a skill only one side names is no difference) |
| `agent_failures` | more agent runs failed in a row than `max_consecutive_agent_failures`; a completed run ends the row |
| `promotion_reversed` | more promotions of an agent slate were reversed since the mode was entered than `max_reversed_promotions`; the undo of a promoted portfolio whose joint slate was the agent's counts, in whole or in part, once |
| `criteria_changed` | other criteria were registered |

A joint run counts like a point's run for `version_changed` and `agent_failures`, also when it
is only recorded.

A rollback is a switch like any other: `just agent-status` shows it in the history, made by
`trigger:<name>`, with what it observed as the reason. It stops agent runs in flight, and their
points get the prompt path's slate. The mode stays `legacy` until an operator enters `agent`
again, under the same condition as the first time. `just agent-mode legacy --reason …` stays the
kill switch.

| Setting | Default | |
|---|---|---|
| `EKG_AGENT_EVAL_ARM` | `agent` | the arm to run (`--arm`) |
| `EKG_AGENT_EVAL_TRIALS` | 3 | trials per case (`--trials`) |
| `EKG_AGENT_EVAL_MAX_TOKENS` | 2000000 | the token bound (`--max-tokens`) |
| `EKG_AGENT_EVAL_RETRIES` | 2 | further attempts of a trial that errored (`--retries`) |
| `EKG_AGENT_EVAL_CONCURRENCY` | 3 | trials of one case at the same time (`--concurrency`) |
| `EKG_GRAPHDB_MAX_REPOSITORIES` | 5 | the repositories the GraphDB server holds; lowers the concurrency when the server is full |
| `EKG_AGENT_EVAL_GATEWAY_PORT` | 8791 | where the run's tool gateway listens |
| `EKG_AGENT_EVAL_RUNTIME_PORT` | 1417 | where the recipe publishes the runtime |
| `EKG_AGENT_EVAL_RUNTIME_URL` | `http://localhost:1417` | where the run reaches the runtime (set by the recipe) |

The run reads `GRAPHDB_URL`, `MODEL_API_KEY`, `MODEL_BASE_URL`, `CHAT_MODEL`, `AGENT_MODEL`,
`AGENT_MODEL_API_KEY`, `EKG_AGENT_MAX_STEPS`, `EKG_AGENT_JOINT_MAX_STEPS`,
`EKG_AGENT_JOINT_RUN_SECS`, `EKG_AGENT_ANALYST_MAX_STEPS`, `EKG_AGENT_ANALYST_RUN_SECS`,
`EKG_AGENT_TEXT_MAX_CHARS` and `EKG_COGNITION_TOTAL_TIMEOUT_SECS` as
the stack does. An agent
trial of a single-subject case takes at most `EKG_AGENT_MAX_STEPS` steps (default 12) within
`EKG_COGNITION_TOTAL_TIMEOUT_SECS` (default 120). A joint case's analysts take at most
`EKG_AGENT_ANALYST_MAX_STEPS` steps (default 10) within `EKG_AGENT_ANALYST_RUN_SECS` (default
240), and its planner at most `EKG_AGENT_JOINT_MAX_STEPS` (default 8) within
`EKG_AGENT_JOINT_RUN_SECS` (default 120). The report names the three step limits. An agent trial reads a record's text only up to
`EKG_AGENT_TEXT_MAX_CHARS` (default 80): a longer name reaches it as a note that text was
withheld, which is what the injection cases with long text show. The read policy is always the default one, whatever
`EKG_READ_POLICY_*` says, so a case that hides a fact hides the same fact on every machine. The
skills are the ones in `apps/hayhooks/skills` as they are on disk, published or not.

## A case

```yaml
id: late-inbound-single-line        # the file name without .yaml
category: late_inbound              # one of set.yaml's categories
profile: supply-risk                # supply-risk or process; portfolio for a joint case
subject: { model: sale.order, id: 1 }   # or `members` and `coupling`, see below
records:                            # the case's world, as source records
  - { model: sale.order, id: 1, fields: { name: S00001, commitment_date: "2027-03-16T00:00:00" } }
tried: []                           # plans already tried, each with the rank key it scores
expect:
  outcome: plan                     # plan, or abstain
  beats_baseline: true              # the default for a plan
  metrics:                          # bounds on the scorer's metrics
    risk_left: { at_most: 0 }       # equals, at_most, at_least
  differs_from_tried: true          # the default when tried is not empty
  forbid:                           # in an accepted plan, any of these is a violation
    actions: []
    targets: []
    text: []                        # in a parameter value or the explanation
    in_params: []                   # in a parameter value only
reference:                          # a plan that meets the expectation (required for `plan`)
  actions:
    - { action_type: expedite_po, target: { model: purchase.order.line, id: 40 }, params: { new_date: "2027-03-12 08:00:00" } }
futile: []                          # plans that do not beat doing nothing (required for `abstain`)
notes: Why the expected outcome is the right one.
```

### A joint case

A joint case plans for several coupled subjects at once. It names them instead of a subject,
says why they are coupled, and runs under a profile listed in `set.yaml`'s `joint_profiles`
(`portfolio`, the two capability profiles composed):

```yaml
profile: portfolio
members:                            # at least two, each among the records
  - { model: sale.order, id: 1 }
  - { model: mrp.production, id: 2 }
coupling:                           # why they are coupled, one reason each
  - Manufacturing order WH/MO/00002 builds what sales order S00001 promised.
```

Everything else is as in any case. A case that names both a subject and members, or neither,
is refused, and so is a member named twice, a member that is not among the records, members
without a coupling, and a profile that does not fit: members under a capability's profile, or
one subject under `portfolio`. A category marked `joint: true` in `set.yaml` holds joint cases
only.

A trial of a joint case is one plan for all the members. The agent arm runs the agent once,
with a task token whose subjects are the members; it reads each record under the policy of the
capability the record belongs to, and its `submit_plan` takes the action types of both
catalogs. The plan is simulated once and scored by the scorer that ranks a portfolio's joint
slate, against doing nothing for the group, so the metric to bound is usually
`members_at_risk`. The group is the one the case names: the monitor's grouping rules take no
part.

Entities are named by source model and id. Record fields are the source's field names with
string values; a reference to other records is written `"[40]"`. The reference and futile plans
are never shown to the agent. They let the set prove itself without a model.

## Adding or changing a case

1. Write or edit the file under `cases/`.
2. Check it against the simulator with `just agent-eval-check`. It needs GraphDB only and no
   model; run it with the rest of the stack down, because it creates and drops many
   repositories in a short time and `just test-e2e` leaves it out for that reason. It prints what doing nothing and the reference plan score, and fails when the reference
   does not pass, a futile plan helps, or a tried plan's rank key is not the real one.
3. Raise `version` in `set.yaml`, run `just agent-eval-lock`, and update `DATASET.md` (its
   version line and its case table). `promotion.yaml` names the set version its numbers were
   written for; update it too, and register it again.
4. `cargo test -p ekg-backend --test it agent_eval_set` checks the lock, the coverage and the
   card.
