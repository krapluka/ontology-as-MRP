---
name: analyse-work-centres
description: For a manufacturing member, read the schedule of each of its work centres with list_work_centre_schedule, find what overlaps beyond capacity, and cite the schedule and the free work centres a planner needs.
profiles: [process-analyst]
tools: [list_work_centre_schedule, list_work_centres, submit_finding]
---

# Read a manufacturing member's work centres

Use this together with `analyse-member` for a manufacturing order. The other orders on its work
centres are not in its evidence, so you read their schedules before you decide its state. A
late component and a contended work centre are two causes: finding one says nothing about the
other.

This skill is complete as written: it has no bundled files, so there is nothing to read with
`read_skill_file`.

## Procedure

1. **Read each work centre's schedule.** For every work centre one of the order's work orders
   runs on, call `list_work_centre_schedule` with that work centre's IRI. It returns every work
   order scheduled there, other orders' included. `submit_finding` takes no finding until you
   have read one.
2. **Look for an overlap beyond capacity.** On each work centre, count how many work orders run
   at the same time as one of the order's work orders, with full dates, and compare the count
   with the work centre's capacity. More work orders at once than the capacity is a cause in
   itself, and it stays one when the work still ends before the promised date.
3. **Look at what could take the work.** When a work centre is over capacity, call
   `list_work_centres` to see the others and their capacity.
4. **Cite the schedule.** A planner has to pick a new time without reading the schedule. Under
   the cause, cite the start and the end of the order's own work orders and of every work order
   they overlap, the work centre's capacity, and the IRI and capacity of any work centre that
   could take the work. An IRI you do not cite cannot be chosen.

If the task says the order finishes after its promise, read the schedule all the same, then hand
in `at_risk` with a `misses_promise` cause citing the order's planned finish.

A work order of another order is evidence only: a plan cannot target it.
