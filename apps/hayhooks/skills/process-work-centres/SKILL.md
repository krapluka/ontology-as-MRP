---
name: process-work-centres
description: Before planning for a flagged manufacturing order, read the schedule of each of its work centres with list_work_centre_schedule and decline when nothing overlaps; before reassigning a work order, list the real work centres with list_work_centres and pick one of them, never inventing a work-center id.
profiles: [process, portfolio]
tools: [list_work_centre_schedule, list_work_centres, submit_plan]
---

# Check the schedule, then pick a real work centre

Use this together with `propose-plan` for a manufacturing order that was flagged for
contention or an infeasible schedule. The flag is a prompt to look, not a finding: the other
orders on the order's work centres are not in its evidence, so you read their schedules before
you plan anything. The same holds for the plant's work centres when your plan moves a work
order to another one (`reassign_work_center`): you look them up instead of guessing.

This skill is complete as written: it has no bundled files, so there is nothing to read with
`read_skill_file`.

## Check the schedule first

1. **Read each work centre's schedule.** For every work centre one of the order's work orders
   runs on, call `list_work_centre_schedule` with that work centre's IRI. It returns every
   work order scheduled there, other orders' included, with the facts the read policy lets you
   see, such as its order, start and end. It answers only for the order's own work centres.
2. **Look for an overlap beyond capacity.** On each work centre, count how many work orders
   run at the same time as one of the order's work orders, and compare the count with the work
   centre's capacity.
3. **Decline when there is nothing to resolve.** When no work centre shows more work orders
   at once than its capacity, and the order's last work order ends before the order's
   promised date, hand in no plan: say what you checked and that nothing overlaps.
4. **Read, do not target.** A work order of another order is evidence only. A plan that
   targets one is refused: resolve the contention by moving the flagged order's own work, or
   by raising a capacity it may act on.

## Check a new time before you hand it in

When your plan moves a work order with `reschedule_work_order`, `new_date` is its new start
and the work order keeps its duration.

1. **Work out the new window.** Its start is `new_date`; its end is that start plus the work
   order's duration.
2. **Compare it with the schedule you read, with full dates.** Put the new window beside
   every other work order on that work centre. The same time on another day is another time:
   a work order that ends on the 9th at 18:00 is still running on the 8th at 18:00.
3. **Count again.** The new window is clear of another work order only when it starts at or
   after that work order's end, or ends at or before its start. With the work order at its new
   window, no more work orders may run at once than the work centre's capacity. If more do,
   choose another time and check again.
4. **Keep the promise.** The order's last work order still has to end before the order's
   promised date.

Say in the explanation which work order's end you moved behind, with its date.

## Pick a work centre for a reassignment

1. **List the work centres.** Call `list_work_centres`. It returns every work centre: its IRI
   and the facts the read policy lets you see, such as its name and capacity. Omissions mean
   facts exist that you cannot see.
2. **Pick one that can take the work.** Prefer a work centre other than the one the work order
   runs on now, with capacity to spare, and one the order's evidence does not already show as
   contended.
3. **Name it exactly.** Set `new_work_center` to the IRI from the list, copied exactly. A
   work centre that is not in the list does not exist: `submit_plan` refuses it, naming the
   value.

## Guidance

- Reassign only when moving the work order resolves the contention; a reschedule or a
  capacity raise may be the smaller change, a reschedule only when the new time passes the
  check above.
- A split does not resolve contention: `split_production` leaves the original work order
  where it is and adds a lot, so as many work orders run at once as before, or more. Do not
  use it for a work centre that is over capacity.
- Say in the explanation which work centre you chose and why, from what the list showed you.
