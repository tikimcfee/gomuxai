# Roster

Agents default to one harness and one model. The mechanics to do otherwise
exist (`agent spawn --backend claude|agy|aider --model ...`, `NAME@HOST`),
but nothing helps an agent decide, and the briefing's own examples all say
`--backend claude`. The roster is where an agent looks before it spawns:
which harness and model each host can run, what each is good at, how fast
and costly it is, whether it is available now, and what past runs showed.

This document is the design. What is built is marked **built**; the rest is
the plan, in the order it gets built.

## What it holds

Two tables in each host's store (`gomuxai.db`).

`roster_entries`: one row per harness and model this host can run.

- `harness` (`claude`, `agy`, `aider`, or a custom backend's name) and
  `model` (the string `spawn --model` takes; empty means the harness's
  default), the key, with an optional `effort` (`low` ... `max`) for
  harnesses that take one: the same model at another effort is its own row.
- `good_at` and `avoid`: short free text, and `tasks`: task kinds it suits
  (see below).
- `speed` (`fast`, `medium`, `slow`) and `cost` (`low`, `medium`, `high`),
  plus `limits`: free text (a weekly cap, a context size).
- `enabled`: a person can switch a row off without deleting its notes.
- who set it, and when.

`roster_notes`: evaluations, append-only.

- harness, model, effort, and the task kind it was tried on;
- `verdict`: `good`, `mixed`, `bad`;
- `text` (what happened) and `why`;
- `source`: required, the session it came from as `harness:session-id`.
  `roster note` checks that the session is one `agent sessions` knows on
  this host, so a note lives on the host its session ran on. Notes are
  never edited; a later note can supersede one (`--supersedes ID`).
- `by`: the agent or person that wrote it (`dream-<id>` for a dream).

One note is one run. Nothing turns notes into a rule by itself: `roster`
shows each entry's notes counted by verdict and by distinct session, so a
reader sees whether a claim rests on one run or on five.

## Task kinds

A short fixed list, so `pick` can match without guessing: `edit` (small,
well-specified change), `bugfix`, `feature` (multi-file), `long-horizon`
(hours, many steps), `review`, `research` (reading and summarising),
`design`, `test`, `kernel` (performance-critical low-level code), `chat`
(quick question). `--tasks` and `--task` refuse anything else, and say the
list.

## Availability

Computed when read, never stored:

- the harness's CLI is on this host's `PATH` (`claude`, `agy`, `aider`, a
  custom backend's launch command);
- the row is enabled;
- later: plan limits from `agent manage view`'s usage data, so a model near
  its weekly limit reads `near limit` and `pick` ranks it down.

## Commands

```bash
agent roster                                  # this host and every linked host: entries, availability, note counts
agent roster --host hub                       # one host
agent roster show claude/claude-opus-5-5      # one entry with all its notes (HARNESS/MODEL[@EFFORT])
agent roster set claude/claude-haiku-5-5 --speed fast --cost low --tasks edit,test \
    --good-at "small precise edits" --avoid "deep debugging"
agent roster disable agy/some-model           # enable/disable; remove deletes a row (notes stay)
agent roster note claude/claude-haiku-5-5@low --task edit --verdict good \
    --session claude:3f3a9c... "did the rename across 12 files cleanly" --why "..."
agent roster pick --task edit                 # best available entry, as a ready spawn line
agent roster pick --task long-horizon --prefer fast --host any --json
```

`pick` ranks available entries: an entry whose `tasks` include the kind
scores up; each good note on that kind (distinct sessions) adds, bad ones
subtract; `--prefer fast|cheap|strong` breaks ties on speed or cost. It
prints the top choice as a command line
(`agent spawn NAME --backend agy --model ...`, with `@HOST` when it is
elsewhere) and the next two with one line of why each. It never spawns.

`roster`, `roster show` and `roster pick` are read-only and pre-allowed for
spawned agents. `roster note` only adds a note, like `friction add`, and is
pre-allowed too. `set`, `enable`, `disable` and `remove` prompt the person.

## Across hosts

Reads federate, writes stay local. `agent roster` asks each linked host for
its roster over the bus (a new read-only op, `roster`), the way `fleet`
does; nothing is copied between stores. A host that doesn't serve the op is
shown as such. `roster` is added to the default remote ops, but hosts with
saved bus settings need `agent bus restart --remote-ops <list>,roster`;
that is a person's call on each host.

## Teaching it

The briefing and the `agent` skill get a short section, "Choosing a
harness and model": check `agent roster pick` before a spawn; mix harnesses
for a second perspective (a review on another model sees what yours
misses); a fast small model for mechanical edits; within Claude Code, the
Agent tool's `model` option runs a subagent on another model without a new
agent; and record what you learned with `agent roster note`, citing the
session. The core-loop examples stop being all `--backend claude`.

## Dreaming

Dreams review ended sessions, so they can record model outcomes. They
write through `agent roster note` (with `--by dream-<id>`), under the same
citation check; the roster never gathers sessions itself. Interface agreed
with the dreaming lead before dreams write anything.

## Later

- Usage limits in availability (above).
- `spawn --effort` where the harness takes it.
- Automatic routing: `spawn --pick TASK` resolving through `pick`.
- A Roster view in the manager.
