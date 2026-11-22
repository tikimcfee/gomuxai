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

**built** (`rs/crates/agent-store/src/roster.rs`: plain rows; the rules
are in `rs/crates/agent-roster`). Two tables in each host's store (`gomuxai.db`).

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
- `by`: the agent or person that wrote it (`dream-<id>` for a dream;
  column `written_by`).

Writers go through `agent_roster::add_note(store, cfg, NoteInput)`, which
checks the kinds, verdict, effort and superseded note, and resolves the
source (a unique start of an id will do) against the session directories
`cfg` names; the note stores the full `harness:session-id`. A superseded
note stops counting. A note may name a harness and model with no entry:
`roster` lists those apart.

One note is one run. Nothing turns notes into a rule by itself: `roster`
shows each entry's notes counted by verdict and by distinct session, so a
reader sees whether a claim rests on one run or on five.

## Task kinds

**built**. A short fixed list, so `pick` can match without guessing: `edit` (small,
well-specified change), `bugfix`, `feature` (multi-file), `long-horizon`
(hours, many steps), `review`, `research` (reading and summarising),
`design`, `test`, `kernel` (performance-critical low-level code), `chat`
(quick question). `--tasks` and `--task` refuse anything else, and say the
list.

## Availability

Computed when read, never stored (**built**):

- the harness's CLI is on this host's `PATH` (`claude`, `agy`, `aider`; a
  custom backend isn't remembered after spawn, so its entry's harness name
  is looked for as its command);
- the row is enabled;
- its plan isn't at its limit, from the host's own llm-exp usage snapshot
  (`agent_manage::llm_usage::read_snapshot`, the file `agent manage view`
  reads), passed into the view (`agent_roster::Checks`): for claude the
  fullest of the session, the week, and the named model's week when the
  entry's model contains its name; for agy its fullest bar. At 90% an entry
  reads `near limit: week 93%, resets <when>` and `pick` takes 2 from its
  score; at 100% it is `off: at limit until <when>` and isn't picked. A
  snapshot over 10 minutes old, unreadable, or one where the provider
  failed counts as none: the entry says `limits not known`, in `show` and
  `--json` only. Each host judges its own limits, so they come over the bus
  with its view (`limit`, `near_limit`, `limits`; `limits_as_of` per host).

## Commands

**built** on this host (`--host` and other hosts come with the bus op).
`agent roster list` is `agent roster` under another name, for the
pre-allow list below.

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
scores 2; each distinct session with a good note on that kind adds 1, each
with a bad one takes 1 away; `--prefer fast|cheap|strong` breaks ties on
speed or cost (strong: dearest first), then more good sessions, then this
host before others. It
prints the top choice as a command line
(`agent spawn NAME --backend agy --model ...`, with `@HOST` when it is
elsewhere) and the next two with one line of why each. It never spawns.

### Saved picks

**built** (`agent_roster::save_pick`, `saved`, `check_saved`). A pick is a startup command that worked, so it can be kept and replayed.
`agent roster pick --task edit --save quick-edit` stores the chosen command
as version 1 of `quick-edit` (a third table, `roster_picks`: name, version,
the command's argv, harness, model, effort, host, task, the why, who saved
it, when). Saving the same name again adds version 2 only when the command
differs, and says so. `agent roster picks` lists them; `agent roster picks
quick-edit` shows every version; `agent roster pick --saved
quick-edit[@VERSION]` prints that exact command again, whatever the roster
says now, with a line saying whether its entry is still available. Each
saved pick keeps its revisions; the roster has no revert of its own.

### Self-explaining output

**built**: each output ends with a `Next:` line. An agent should understand each command's output without the docs, the way
`agent status` does. Each table has a one-line header saying what it is
("Roster on HOST: harnesses and models this host can run"); each entry says
its availability in words (`available`, `off: disabled`, `off: agy not on
PATH`); note counts read as `2 good, 1 bad from 3 sessions`; `pick` says why
it chose (`tasks include edit; 2 good notes on edit; fast`) and ends with
what to run next (the spawn line, `agent roster show ...` for the notes,
`--save NAME` to keep it). An empty roster says how to add an entry.

`roster list`, `roster show`, `roster picks` and `roster pick` are
read-only and pre-allowed for spawned agents. `roster note` only adds a
note, like `friction add`, and is pre-allowed too. `set`, `enable`,
`disable` and `remove` prompt the person. `pick --save` only adds a saved
pick's version, as `note` adds a note, and goes with `pick`. Bare `roster` isn't on the list: safe commands match as a prefix,
so it would pre-allow `roster set` and the rest too; the briefing says
`agent roster list`.

## Across hosts

**built** (op `roster` in agent-bus and agent-busd; the CLI's
`gather_rosters`). Reads federate, writes stay local. `agent roster` asks each linked host for
its roster over the bus (a new read-only op, `roster`), the way `fleet`
does; nothing is copied between stores. A host that doesn't serve the op is
shown as not read, with the fix, never as an error; `--host HOST` reads
one. `pick` ranks every read host's entries, and `pick --saved` checks a
saved pick on its own host. `roster` is added to the default remote ops, but hosts with
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
