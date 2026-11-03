# ekg-odoo-seed

Idempotent seeder that loads the flagship delivery-promise & supply-risk
scenario into Odoo via JSON-RPC. It seeds *real* Odoo records (the ingestion
path pulls them for real), not RDF fixtures.

## Run

The stack must be up (Odoo reachable). `just podman-local-up` already seeds as part
of provisioning; run the seeder directly to re-apply after edits:

```bash
just podman-local-up   # brings up the stack and seeds it
just seed              # re-run the seeder on its own (or: cargo run -p ekg-odoo-seed)
```

Configuration (env, with local defaults):

| Var            | Default                 | Meaning                    |
| -------------- | ----------------------- | -------------------------- |
| `ODOO_URL`     | `http://localhost:8069` | Odoo base URL              |
| `ODOO_DB`      | `ekg`                   | database name              |
| `ODOO_USER`    | `admin`                 | login                      |
| `ODOO_API_KEY` | `admin`                 | password / API key         |

## What it seeds: an at-risk scenario

A multi-level BOM with the full set of edges the supply-risk agent
traverses:

```text
SalesOrder (VeloMax, 10× E-Bike, commitment_date = today+21)
  └─▶ E-Bike Model X ──BOM──┬─▶ Drive Subassembly ──BOM──┬─▶ Hub Motor 250W   (MotorWorks, Driveline: multi-source)
                            │                            ├─▶ Motor Controller (PartsHub)
                            │                            └─▶ Gear Set ×2       (PartsHub)
                            ├─▶ Aluminium Frame   (FrameCo, 14-day lead)
                            └─▶ Battery Pack 500Wh (CellTech, 45-day lead, single source)
```

Why the promise is at risk: the battery is single-sourced with a 45-day lead
time, on-hand (2) is below both the demand (10) and the safety stock (5), and the
only inbound PO of 10 batteries lands at `today+45`, after the `today+21`
customer promise. That is the drift "the plan" (SHACL + the SPARQL risk
rule) must detect and raise an action point for.

## Idempotency

Every record is registered under the `ekg_seed` external-ID namespace
(`ir.model.data`). Re-running upserts: it writes changed values onto the
existing records instead of creating duplicates. Edit a value in `main.rs`,
re-run, and the same record is updated in place. Dates are recomputed relative
to "today" on each run, so the scenario stays fresh.

Inspect the seeded records in Odoo (Sales / Purchase / Manufacturing), or filter
`ir.model.data` by module `ekg_seed`.
