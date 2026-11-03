---
name: plan-portfolio
description: Plan for several coupled subjects at once from their findings, and hand in one plan with submit_plan that resolves all of them.
profiles: [portfolio]
tools: [submit_plan]
---

# Plan for coupled subjects together

Use this when your task names several members, says why they are coupled and gives a finding
for each. A change to one member changes the others: an inbound line that arrives earlier lets
the manufacturing order start earlier, and a work order that moves changes when the sales order
can ship. So you plan for all of them at once and hand in one plan.

Each member was read for you: its finding holds its state, its causes, the entities involved and
the facts they rest on. A finding from the records is established: do not argue with it. One marked
as an investigator's claim is what an agent concluded where the records could not settle it. A value
shown as withheld is text you do not need. Do not guess anything the task does not hold.

If the task gives you an approach, follow it. Other planners work on the same findings with other
approaches, and the plans are simulated and ranked, so a plan that takes your approach seriously is
worth more than one that hedges.

This skill is complete as written: it has no bundled files, so there is nothing to read with
`read_skill_file`. Do not load any other skill.

## Procedure

1. **Read every member's finding before you draft.** What the second holds often changes what
   the first needs.
2. **Find how they depend on each other.** The task says why the members are coupled. Work out
   the order of things: what has to arrive before what can start, and what has to end before
   what can ship. Say which cause sits upstream of the others.
3. **Draft one plan for all of them.** Start with the cause that sits upstream and check what
   it leaves open downstream. A member with a cause of its own needs an action for that cause.
   An action for one member counts for another only when both hang on the same thing, the same
   late line or the same contended work centre, and the findings show it: an earlier arrival
   does not free a work centre, and a moved work order does not bring a component. An
   `at_risk` member stays at risk until an action reaches its cause, also when its work still
   ends before the promised date.
4. **Pick targets from the task.** Every target must be an entity IRI the task names; copy it
   exactly. Use only the action types in `submit_plan`'s schema, each with exactly its
   parameters. Write dates as `YYYY-MM-DD HH:MM:SS`. A work order of another order cannot be a
   target. A reassignment's `new_work_center` must be a work centre a finding cites.
5. **Check a new time before you hand it in.** `reschedule_work_order`'s `new_date` is the work
   order's new start, and it keeps its duration. Put the new window beside every other work
   order the finding cites on that work centre, with full dates: no more may run at once than
   the capacity. A split does not resolve contention.
6. **Keep the dates consistent across members.** A work order cannot start before the component
   it needs arrives. A promise cannot sit before the end of the work that builds the order. A new
   promise has to sit behind an arrival the findings show.
7. **Prefer the plan that clears every member.** A plan that resolves one member and leaves
   another at risk is weaker than one that resolves both, even when it is smaller. When no plan
   clears every member, hand in the one that clears the most and say in the explanation which
   member stays at risk and why.
8. **Take earlier attempts into account.** If the task lists plans already tried with what each
   achieved, read how many members each left at risk. Propose a plan that leaves fewer, not a
   variation of one already tried.
9. **Go through the members before you submit.** For each member, say which action changes
   something for it, or that its finding says `on_plan` or `immovable`. An `at_risk` member with
   no action: add one, then submit. Put this member-by-member account into the explanation.
10. **Submit once.** Call `submit_plan` one time with all the actions and one explanation. If
    it answers `rejected`, fix every defect it lists and submit again.

## When to decline

The engine stops before you when no member has a cause an action can reach. If you are reading
this, at least one has. Decline only if every action you can name fails, by not calling
`submit_plan` and saying why in plain text. When an action helps one member and nothing helps the
other, that is a plan: hand it in and say what stays open.

## Guidance

- One plan, not one per member.
- Use the fewest actions that leave no member at risk. For two members with a cause each, that
  is two actions; one action is the whole plan only when the members share their cause.
- Prefer reversible, cheap actions: moving a date before splitting an order, an expedite before
  a re-promise to the customer.
- When a build misses its promise, move the build before you move the promise. A new promise is
  for when no move of the build can finish it in time; say in the explanation why none can.
- In the explanation, go member by member: the cause, what the plan changes for it, and what the
  read policy hid.
