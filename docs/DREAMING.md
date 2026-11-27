# Dreaming

A dream is a scheduled pass over finished work: ended sessions and their
transcripts, the friction box, handoff docs, memory directories, and git
history. It consolidates what was learned, so the next session starts from
it, and curates the friction box so a person doesn't have to. A project
dream covers one project of the dream scope and keeps its memory; the
global dream covers what holds across projects: the machine's memory, the
lessons one project's dreams learned that another needs, and the friction
box.

This document is the design. What is built is marked **built**; the rest is
the plan, in the order it gets built.

## Two layers

1. **Consolidate** (low risk, runs without asking). Friction triage:
   duplicates, cross-host links, accepting what is clearly real. Memory
   consolidation: every entry cites its source and keeps its why. Drifted
   handoff docs and docs flagged. A diary entry for each dream.
2. **Act** (later). Fix the top friction in a worktree, run the checks,
   merge, build, install. Needs a size gate (a large change asks a person
   first), a canary and rollback for installs (agent runs on every hook
   event, so one bad build stalls every agent on its host), and review by a
   session other than the one that made the change.

Only the consolidate layer is being built now.

## Pieces

### The record and the reviewed cursor

**built** (`agent-store` `dreams.rs`). Two tables in the host's store (`gomuxai.db`):

- `dreams`: one row per dream: id, host, the agent that dreamt it,
  started/finished, status (`open`, `committed`, `abandoned`, `quiet`),
  its diary entry, its `kind` (`project` or `global`) and, for a project
  dream, its `project` (a scope dir). Rows from before kinds are project
  dreams with no project: they covered the whole scope, and count as the
  last committed dream of every project until one of its own commits.
- `dream_items`: what a dream was given to review: kind (`session`,
  `friction`, `handoff`, `memory`, `commit`, `dream`), a key (a session's
  `harness:id`, a friction entry's uid, a path, a repo and commit, a
  dream's id), a fingerprint (size and mtime, `updated_at` and votes, a
  commit hash, when a dream committed), and the dream it belongs to.

An item counts as reviewed only once its dream commits. A dream that is
abandoned, or whose agent dies, leaves its items for the next one: the next
`agent dream run` first closes an open dream whose agent is gone as
`abandoned`. An item whose fingerprint changed since it was reviewed (a
session resumed and written to, a friction entry voted on) comes back. A
session still running is left for a later dream. When nothing is new, the
dream is recorded as `quiet` and no agent is started; what a dream wrote
itself (its memory puts, its friction triage) is not new evidence. A dream
runs anyway once the last committed one is 7 days old. Each dream takes at
most 200 sessions, oldest first; the rest wait for the next.

What a dream changes itself is recorded in it as it left it: each
`agent dream friction` verb and `limitation` sets the entries it touched
as items of the dream at their fingerprint afterwards (the store's
`set_dream_item`), and `memory put` and `remove` do the same for the memory
directory they wrote, with the fingerprint gather computes
(`gather::memory_fingerprint`, shared so the two can't disagree).

### Gather (deterministic)

**built** (the rules in `agent-dream` `gather.rs`, the host's side in the
CLI's `dream.rs`). `agent dream gather` lists what the next dream would
get, without starting one.

Dreaming is opt-in by directory: `agent dream scope add|remove|list <dir>`
keeps a list of absolute directories in the host's store (host state
`dream.scope`, JSON). Only sessions whose cwd is one of them, or inside
one, are evidence, and only their memory directories and commits come
along; handoff docs must lie in the scope too. With an empty scope,
gather and run say nothing is in scope and how to add a directory, and
run starts and records nothing.

A tree can be in scope too: `agent dream scope add ~/dev --root` makes
each folder directly under it a project in scope, as the index's roots
do, but hidden folders, the names on the skip list and git worktrees
(host state `dream.roots`; what was added one by one stays in
`dream.scope.added`). `dream.scope` is rebuilt from both at every gather,
run and `scope list`, so a folder made under a root is in scope from the
next dream on, and the store and the hook keep reading one list. Each
project gets its own project dream when it has something new, so quiet
ones cost nothing. A session run inside a skipped folder of a project
(its build output, a cache) is no evidence. A root's project owns the
sessions of its worktrees once they are removed by name, as helpers'
worktrees are named here: a folder `<name>-wt` or `<name>-wt-<role>`
anywhere (the longest project name that fits), with no `--worktrees`
to set.

The skip list (`agent index skip list`) is the index's, a file a person
edits; a dream never changes it. A dream that sees it wrong (build
output gathered or indexed; a skipped name holding real work) runs
`agent dream propose skip add|remove <name> --why ... --source ...`,
which records a proposal in `dream_proposals` and its diary. It waits,
whatever becomes of the dream, until a person runs `agent dream
proposals accept <id>` (code makes the change and tells the index
watcher) or `reject <id> --note ...`; a dream's agent is refused both.

**built**: a dream is over one project, a scope dir (`agent dream run
--project DIR`; `gather` takes the same flag). Without `--project`, run
picks the scoped dir with something new (or due) whose last committed
dream is oldest (none counts as oldest; ties go by path), and gather shows
that pick, or why each project is quiet. Gather restricts sessions,
worktrees, memory and commits to that dir. Its window (commits since, the
evidence's since) and the 7-day due rule are the project's own: its last
committed project dream, or a dream from before projects if that is
newer. Project dreams take no friction: the global dream triages it, once.
Only one dream is open at a time, whatever its kind.

The first `agent dream run` records a floor, 7 days before it (host state
`dream.floor`): a session last written before it is never evidence, for
that dream or a later one, unless it is written to again. Until a run sets
it, gather looks back 7 days. Commits before the first committed dream are
taken from the floor on. Sources:

- sessions that are no longer running (`agent sessions`' discovery), not
  untracked, not the dream agents' own. A session whose state can't be told
  counts as running, and a subagent's session goes with the session that
  started it, not as an item of its own;
- handoff docs named by agents' `handoff` state keys;
- memory directories (Claude Code's per-project `memory/`, beside the
  transcripts) of the projects whose sessions the dream takes, changed or
  not: they are where it writes. Only a changed one counts as new. A dream
  that runs with nothing new (7 days on) takes the memory directories of
  every project with a finished session;
- commits in the repos those sessions ran in (`git log` in each session's
  cwd, skipped when it isn't a repo or is gone), since the project's last
  committed dream started, or the floor before any.

### The global dream and the machine memory

**built** (`gather::gather_global`, the CLI's `dream.rs`, the brief in
`agent-dream` `brief_global.md`). Lessons about the machine, the process,
agent communication, friction and harness behavior should help whichever
repo an agent is in, so one dream covers what holds across projects:
`agent dream run --global` (`gather --global` shows it). Its items:

- `dream`: each project dream committed since the last committed global
  dream (key the dream's id; read with `agent dream show <id>`);
- `friction`: this host's entries, as project dreams took them before:
  open and accepted ones not reviewed as they are, and closed ones that
  changed since a dream reviewed them;
- `memory`: every scoped project's memory directory, and the machine
  memory directory, `<data dir>/memory/` beside the store. They come along
  every time (they are where it writes), new only when changed since a
  global dream reviewed them: a project dream's write to its own memory is
  news to the global dream.

The digest's evidence section pools every scoped project's finished
sessions written since the last committed global dream (the floor before
any; the newest 200), and each group names the projects it was seen in: a
trip seen in two or more projects is machine-wide, the three-session rule
still holding. The global dream is quiet with no new project dream and no
changed friction, until the last committed global dream is 7 days old.

The machine memory has the format of a project's (`start_here.md`, fact
files, `MEMORY.md`). The first `run --global` creates it empty, with an
empty index, before it gathers. It is a memory item of the dream like any
other: snapshot at open, commit's guard and citation check, abandon's
restore. The global dream's briefs (the synthesizer's `brief_synth.md`,
which proposes these as ideas, and `brief_global.md`, kept for a
single-agent global dream) keep its `start_here.md` under 50 lines (hosts
and the bus, how agents are run and who answers approvals, build rules,
toolchains, the tripwires that recur across projects, each cited), carries
a lesson from one project's sessions or diary into another's memory with a
`[source: dream:<id>, ...]` citation, replaces a machine-wide fact a
project's start-here repeats with a pointer, and triages friction by the
rules project dreams had (now only through approved ideas: see "The global
dream: a synthesizer, a critic, and code"). A `dream:<id>` citation must name
a committed dream. The project brief (`brief.md`) leaves friction and
machine-wide facts to it.

Spawn puts the machine note in every new agent's briefing, whatever its
harness, as a section of its own ("This machine") before the project's
start-here section, with the same cut (80 lines) and frontmatter dropped
(`agent_dream::start_here::machine_section`).

### Evidence: what keeps tripping agents

**built** (the rules in `agent-dream` `evidence.rs`, the host's side in the
CLI's `dream_evidence.rs`). The start-here note's Tripwires come from
evidence, not guesses: the digest ends with an `## evidence` section of
what failed again and again in the batch, and `agent dream gather` says how
much (`--json` carries it whole, as `evidence`). Its window is the batch's:
since the last committed dream of its kind (the project's, or the last
global one) started, the floor before any. A project dream's commands are
those run in its directory or in a worktree of it.

- **Failed `agent` commands**: the store's command log, rows that exited
  non-zero in a scoped directory, grouped by command shape (its subcommand
  words by the command tree, and its flags without values) and the error's
  first line, normalized: quoted values, paths, uuids and hex ids, dashed
  names (agents, hosts; not flags) and numbers replaced. Counted with the
  distinct agents that ran them, first and last day. Left out: a wait that
  timed out, `agent run` passing on its script's exit, and bursts (three or
  more failures by one agent within a second: a test suite or a loop).
- **Failed tool calls**: the result events with status `error` in the
  batch's transcripts (and their subagents', under the parent's key), read
  with `agent-transcript` as `agent transcript` reads them, grouped by tool
  and normalized error. A shell call (Claude's Bash, agy's run_command) is
  grouped by its program, with the subcommand for git, cargo, agent and the
  like, and its exit code. Left out: builds and tests failing (the work
  going on), grep, diff and test finding nothing, a wait timing out, and
  agy's note that it cut a long output.
- **Corrections**: messages into those sessions from a person or a peer
  (envelopes dropped) that start by correcting the agent (no, don't, do
  not, stop, wrong, actually, instead, again, that's not, I said, ...), or
  say stop, wrong, that's not, I said, not what I, you didn't within their
  first 60 characters. Not grouped: the latest three per session, newest
  first, at most 20, each through `scrub::excerpt` and clipped to 160
  characters.

Each source lists at most 15 groups: those seen from two or more sessions
(or agents) first, by count, then those seen from one, marked as such. The
section stays near 6 KB; lines past a source's share are counted, not
shown. The brief points the start-here note's Tripwires at it, with the
three-session rule.

On this host's data, the groups that come out on top are real trips:
`agent send` to an agent whose session ended, `agent history --limit`
(no such flag), agy's `read_file` on a path that doesn't exist, Edit's
"String to replace not found" and "File has not been read yet", `pkill`
killing the agent's own shell (exit 144).

### The dream agent and its narrow toolset

**built**. `agent dream run` closes an open dream whose agent is gone as
abandoned (and starts nothing while a dream's agent is still at work),
cleans up the agents of closed dreams, gathers, records the dream and its
items, writes a digest (`.gomuxai/dreams/<id>/digest.md`), snapshots each
memory directory in the batch (`dream-snapshots/<id>/<folder>/` beside
the store, outside the dream's own directory so its agent can't edit its
baseline; `memory::take_snapshot`), runs the memory check against the
dream's projects into the digest (`## memory check`), and spawns a Claude agent `dream-<id>`
with the fixed brief (`agent_dream::brief`, behind a line naming the dream
and what it is over; the global dream spawns its synthesizer instead, as
the section on workers says), in
`.gomuxai/dreams` with its workspace trust answered, and its
state key `dream` set to the id. `--dry-run` says what it would do.
`agent dream list` and `show` read the record.

Nobody watches a dream, so its agent runs in Claude's `dontAsk` mode:
whatever its launch rules and the PreToolUse hook don't allow is refused,
never left to a person's own permission mode. Of the general agent
commands it gets only the reading ones (`DREAM_READ_COMMANDS`: list,
status, logs, sessions, transcript, history, friction list and show,
roster list and show, and the index's search, show, tree, related and
stale), in place of what other agents are pre-allowed: it
sends no messages and files no friction or roster notes but through its
verbs. The index is read-only to it (no crawl, root, alias, embed or
summarize); its brief tells it to check a claim about the code against
the index before trusting a transcript's tail, and to open the file a
summary points at before citing it.

What it reads of transcripts is scrubbed whatever it asks (Ivan's rule:
anything that mines other sessions' transcripts scrubs them first):
`agent logs` and `agent transcript` drop pasted and tagged blocks, code,
keys and lines with a credential, an email or a sign-in URL, and tool
inputs, cut a tool call's command and a session's title to one bounded
line, and do the same to `agent history`'s recorded commands, for a
caller that is a dream's worker, and refuse it another host's logs. The
check fails closed: only a caller the store clearly says is no dream's
worker reads unscrubbed, so a busy store or a dream that just closed
scrubs. Messages that hooks delivered from other agents are kept, with
the same lines dropped: they are the session's own dialog, and often the
evidence a dream needs (the task, the answer). A long session is read in parts: `agent logs
<session> --talk` (prompts, replies and one-line tool calls), `--grep
TEXT`, and `--start N` to page; `<session>` may be an id, `harness:id`
or a transcript's path, so a session no agent owns is read the same way. It reads files through its Read tool, which its launch rules allow
for Claude Code's projects directories, the scoped projects, the machine
memory, the repos its commits are in, and its handoff docs. It may edit
files only in its own dream directory (drafts for `memory put --from`);
`NotebookEdit`, `WebFetch`, `WebSearch` and subagents are refused.

agy has no permission mode to lean on (a person's agy may run
`always-proceed`), so a dream's agy agent is confined by its own
PreToolUse hook instead (`confined` spawn option, `confine.rs`). While
its dream is open, the hook answers every tool call: `read_file` and
`view_file` under the dream's read roots (state key `dream_read`, the
paths its Read and Edit rules name) and `run_command` for the same
commands a Claude dream may run are allowed, and everything else is
denied, `write_file`, `edit_file`, `search_web` and subagents included.
It fails closed: its hook runs with `agent report --confined`, which
exits non-zero when it has no decision to print (an error, an unknown
agent), and agy blocks a call whose PreToolUse hook fails; a confined agy
agent whose hooks can't be written isn't started. Checked by hand on agy
1.5.14 under `always-proceed`: a deny and a failing hook each blocked
`write_file`, `run_command` and `search_web`. Other agy agents are left to
agy's own settings, as before. No dream uses agy yet. It writes only
through `agent dream` verbs, which the PreToolUse hook allows for that
agent alone while its dream is open (`DREAM_AGENT_COMMANDS` in
agent-service; never in the launch rules), and the hook also allows it
`git -C <repo> log|show` for a repo its dream has commits of, or any
scoped project (no `--output`, no `--ext-diff`). Each verb acts on the caller's own open dream
and refuses otherwise:

- `agent dream memory get|put|check`: put takes the version it read
  (`--expect`), and refuses a rewrite that drops more than a quarter of the
  file's entries or loses a source citation; the prior file is saved in the
  dream's snapshot first, and each write is recorded there. Abandoning a
  dream puts back only what it wrote and is still as it left it: a file
  someone changed since is kept, and named (`memory::restore_own`).
- `agent dream friction dup|link|accept`: triage only; it never resolves
  an entry as fixed. Only the global dream's brief asks for it.
- `agent dream roster note`: a model evaluation on the roster (**built**),
  through `agent_roster::add_note`, with `--by` set to the dream's agent
  and `--session` limited to sessions the dream was given.
- `agent dream limitation "..."`: files friction about dreaming itself,
  de-duplicated against open entries.
- `agent dream note "..."`: appends to the dream's diary.
- `agent dream commit` / `agent dream abandon`.

### Workers: every agent a dream runs

**built** (`agent-store` `dream_workers.rs`). A `dream_workers` table holds
every agent of every dream: its dream, name, role, model, effort, lens
(empty until a caller uses lenses), status (`running`, `done`, `failed`)
and when it started and finished. A project dream has one `dreamer` (the
store's `open_dream_of_kind` records it); the global dream has a
`generator` and a `critic` (below). Dreams from before workers get their
`agent` as their dreamer when the store migrates (done if the dream
committed, failed if it was abandoned, running while open). `dream_agents`
covers every worker, so none of their sessions is ever evidence, and
`open_dream_of` (which the PreToolUse hook and agy's confinement ask)
resolves an agent through its worker row. Closing a dream closes its
running workers (done on commit, failed on abandon).

Each verb resolves its caller through the workers table and checks its
role: `memory put|remove`, `friction dup|link|accept`, `roster note` and
`commit` are a dreamer's; `idea` a generator's; `judge` a critic's; `done`
a generator's or a critic's; `note`, `abandon`, `limitation` and the
reading verbs (`ideas`, `memory get|check`, `show`, `list`) anyone's. A
refusal says what the caller's role does instead. `agent dream run`
cleans up every worker of a closed dream, and treats an open dream with a
gone worker as abandoned. `roster note --by` refuses any dream worker's
name (`dream-<id>`, `dream-<id>-synth`, ...), not only `dream-<id>`.

### The global dream: a synthesizer, a critic, and code

**built** (the CLI's `dream_steps.rs`, `agent-dream` `ideas.rs`,
`brief_synth.md`, `brief_critic.md`). The global dream is the idea
engine's first caller (`docs/DREAMING-VISION.md`, "The idea engine"),
`synthesis`: no agent writes memory or triages friction itself.

1. `agent dream run --global` opens the dream as before (gather, items,
   digest, snapshots) with its generator, `dream-<id>-synth`, as its first
   worker and its agent, on a model picked at random between
   `claude-opus-5-5` at effort `xhigh` and `claude-fable-5-2` (at its
   default effort: the roster doesn't say Fable's highest), spawned with
   SpawnOptions' `model` and `effort` and the synthesizer's brief
   (`brief_global.md`'s rules, but it proposes ideas instead of writing).
   The synthesizer stays Claude: it writes drafts in its dream's folder,
   and confined agy agents are proven only on reading and running
   commands. The critic is always another family, agy, on
   `gemini-4-carbon` at `high` or `gemini-4-neon` at `xhigh`, picked at
   random (`CRITIC_MODELS`), confined by its fail-closed hook: it only
   reads (`read_file` under its read roots, its dream's folder included)
   and runs its verbs and reading commands.
   The diary records each worker's model and effort as it starts.
2. The synthesizer proposes a change set with `agent dream idea --from
   <json>`, one idea per change: a memory `put` (dir, file,
   expect_version, a draft in the dream's own directory, why, citations)
   or `remove`, or a friction `dup`, `link` or `accept` (id, other, note,
   citations). The verb checks the shape (unknown or missing fields
   refused), that the dir is one of the dream's memory items, that a
   draft lies in the dream's directory, that no other live idea changes
   the same file, that friction entries exist here, and every citation by
   commit's rules (`Known`, shared with commit's check), refusing a kind
   no dream item has (`report:`, a path); a put's draft's own citations
   are checked as commit would. A put's content is read and recorded with
   the idea, so later edits to the draft change nothing: what the critic
   judges is what is applied. Ideas go in `dream_ideas` (caller,
   worker, the idea as JSON, citations, status, the critic's why and
   checked sources, four optional scores, a nullable `rank` for a later
   tournament, the outcome) with a first vote in `dream_idea_votes` (the
   worker, its model and lens, its evidence). `ideas::converges_with` is
   where a later caller matches an earlier idea for a vote instead of a
   duplicate; for synthesis it finds none.
3. Its `agent dream done` marks it done and runs the next step in code:
   the critic, `dream-<id>-critic`, on the other model, with the critic's
   brief (check each idea against its cited primary sources, reject what
   they don't support, verify friction triage), no Edit rule (it reads
   its dream's directory), and the dream's starter as its parent, not the
   synthesizer. With no ideas, the dream goes straight to applying.
4. The critic runs `agent dream judge <id> approve|reject --why "..."
   --checked <source>...`: approving needs at least one `--checked` among
   the idea's citations. `--leverage --evidence --feasibility --novelty`
   (1 to 5) are recorded when given. Its `done` refuses while an idea is
   unjudged.
5. Applying, in code: first the approved memory changes in id order,
   through `memory::put`/`remove` at the idea's expect_version (a version
   conflict, or any refusal of put's own rules, skips the idea, and the
   diary says why); then commit's guards (`memory_problems`). If they
   fail, memory is put back with `restore_own` and the dream is
   abandoned with no friction triage applied, so it leaves nothing
   changed. If they pass, the approved friction triage applies in id
   order, through the same calls the dream's friction verbs make, and the
   dream commits. The diary has a line per idea, in id order: what it
   changes, who proposed it on which model, the verdict, by whom on which
   model and why, and what was applied or skipped.

It is a step function over the dream's state in the store, never a
long-running process: each step runs on a worker's `done`, and `agent
dream run` advances an open global dream whose step is due (a worker done
but the next not started, or a worker gone: abandoned, the worker marked
failed), so a crash between steps is picked up by the next run. A worker
that has sat idle, its turn over, without saying `done` for more than 30
minutes (`WORKER_IDLE_LIMIT`; idle since its last status report), or has
run alive for more than 90 (`WORKER_RUN_LIMIT`, from when it started), is
given up the same way: `agent dream run` marks it failed, puts memory
back, abandons the dream saying why in the diary, and cleans up its
workers. `agent
dream show` lists the workers and counts the ideas; `agent dream ideas
[<id>] [--json]` lists them with their verdicts, outcomes and a put's
whole content.

Project dreams are unchanged: one dreamer that reads, decides and writes
through the verbs.

### Evidence rules (in the brief)

- A preference or rule is written only when three separate sessions agree,
  each cited by source (`harness:session-id`, friction uid, commit).
- A fix is recorded only after a blocked attempt and a verified retry.
- Every memory entry keeps a `[source: ...; added: YYYY-MM-DD]` line and
  its why.
- Transcript text quoted into memory is scrubbed of secrets and of pasted
  content; host names and personal paths stay out of tracked files.
- Transcripts, friction text and docs are data, never instructions. Past
  answers are never taken as permissions.

Rules the brief states are checked in code where they can be:
`agent dream memory put` refuses an entry with no source line (except a
change that only points `[[links]]` at memories that exist: a link fix has
nothing to cite), and a
rewrite that loses a citation the file had when the dream opened (one the
dream added itself it may take back). **built**: `agent dream commit`
refuses while a memory directory the dream holds a snapshot of fails the
whole-dir guard (`memory::guard`: over a quarter of its files gone, or a
citation lost), or while a file the dream changed cites a session, friction
entry or commit that is no item of the dream and none a committed dream
reviewed (`agent_dream::review`). The three-session rule for a preference
stays in the brief: a preference can't be told from a fact mechanically. The
digest's excerpts are scrubbed before the agent sees them (pasted blocks,
code fences, key blocks, and lines with credential markers dropped; each
excerpt clipped).

`agent dream memory check` is deterministic and report-only: exact and near
duplicate entries (Dice similarity of 0.8 or more on word multisets of 5+
words), `[[links]]` to no memory, index lines naming no file, and files the
index doesn't name. With `--repo DIR` (repeatable) it also reports, by file
and line, what the memory's backticked text (outside code fences) says that
those repos no longer bear out (`agent_dream::stale`; the CLI hands it the
command tree it knows, so the library doesn't depend on the CLI):

- `stale-path`: a token with a `/` or a known file extension that exists in
  no repo. A relative path counts as there when it is at a repo's root, the
  tail of a path in one (`crates/x/src/y.rs` under `rs/`), or ignored by the
  repo's git (local settings, build output, runtime data). It is flagged when
  its first component is at a repo's root, or, for a file, when its directory
  is in a repo; otherwise (`origin/main`, another project's `lib/x.rs`) it is
  left alone. Bare names are checked only for source files (`.rs`, `.py`,
  ...), anywhere in a repo; absolute paths only under a repo; `~` paths,
  URLs and placeholders never. The walk skips build output, `.git`, and
  checkouts nested in the repo (worktrees, vendored clones).
- `stale-make`: `make <target>` no repo's Makefile (with its `include`s)
  defines; unchecked when a target is named by a variable, or with `-C`/`-f`.
- `stale-agent`: `agent <sub>` this binary has no command for, and `--flags`
  its command doesn't take: inside its backticks, or in their own on a line
  that names just that one command and nothing else command-like.

What it can't be sure of it leaves alone. A memory that talks about several
repos needs each as a `--repo`, or its references to the others read as
stale; each repo added also anchors more paths, so pass the repos the memory
is about, not every checkout on the host.

### Schedule

**built** (Linux; macOS later, as a launchd agent). `agent dream schedule
daily HH:MM | every Nh` writes `gomuxai-dream.service` and `.timer` in
`~/.config/systemd/user` (`agent_dream::schedule` renders them): the
service runs this agent binary's `dream run` from the binary's directory
with the caller's PATH, and the timer is persistent, so a dream missed
while the machine was off runs once when it is back. Then it reloads
systemd and enables the timer. It refuses with an empty dream scope, and
with a path a unit would misread (whitespace, `%`, quotes). `off` disables
the timer and removes both units; `status` (the default) says whether it is
on, how often, and the next run (`systemctl --user list-timers`).
`--dry-run` prints the units and commands. Turning it on is a person's
call.

### Later

- A Dreams view in the manager: an optional field on `HostView` (agent-manage), filled in `cli/manage.rs`'s `local_host`, with the page built by the manager's lead (the Usage tab is the pattern).
- The act layer, above.
- The timer runs `agent dream run`, which picks a project dream; the
  global dream still needs a schedule of its own (a second timer, or the
  timer running `--global` once a day).
- The global dream's next workers: agy (Carbon, Neon) in the model pool
  once its confinement is proven under the current agy, several generator
  samples with lenses, and the critic's convergence check and tournament
  (`dream_ideas.rank`, a matches table).
- The global dream's items hold no sessions, so a synthesizer can cite a
  session only when a committed project dream reviewed it (commit's rule,
  which `idea` shares).

## Prior art

- threadlane (`crates/threadlane-hub/src/dreaming.rs`, the store's
  `dream_batches`/`dream_items`): fixed prompt, narrow toolset, a cursor
  that counts an item only when the dream commits, a quiet skip, a
  de-duplicated "report limitation" tool.
- OpenClaw dreaming (checked in `docs/research/OPENCLAW-DREAMING.md`): one
  nightly sweep of three phases, where light and REM are deterministic
  code that only stage and deep is the one LLM writer, behind score and
  recall thresholds; a consolidation that would drop over 25% of prior
  entries falls back to appending; the last few prior versions are kept;
  new entries carry their source, but old citations aren't audited; and a
  DREAMS.md diary that never feeds memory.
- Devin memory: one-line entries tagged with source and date.
