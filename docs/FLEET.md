# Fleet features

What the Rust `agent` adds to the Go tool, and how each piece decides. One rule runs
through all of it: **decide from state, never from log lines.** State means the store
(SQLite), the CLIs' own session registries and transcripts' metadata, systemd, git, and
replies from other hosts' buses.

Everything here is additive on the bus. A host whose bus doesn't serve one of the new
ops (left out of its `--remote-ops`) doesn't answer it, and the caller says so: there
is no falling back to `list`.

## Bus additions

| What | Subject | Payload |
|---|---|---|
| op `fleet` | `gomux.<host>.rpc.fleet` | `FleetRequest{report?: bool}` → `FleetHost` (below) |
| op `template` | `gomux.<host>.rpc.template` | `TemplateRequest{action: "list"\|"start"\|"resume", template, from}` → `TemplateResponse{templates?, agent?}` |
| focus events | `gomux.<host>.focus` | `FocusEvent{host, agent, session, at, by}` (agent `""`: nothing has focus) |

`fleet` and `template` join `AllOps` and `DefaultRemoteOps`. `fleet` only reads, like
`list`. `template` is safe to serve by default because a host with no templates file
starts nothing. The hub's peer permissions gain publish `gomux.*.focus` (every host's
but the peer's own denied, as for status events) and subscribe `gomux.*.focus`.

## `agent fleet`

One view across this host and every trusted peer, queried at once (3s each; with no bus
running here, this host only, noted on stderr).

```
agent fleet                # the tree, per host
agent fleet --json         # the same as one JSON document (FleetView), for displays and scripts
agent fleet --host hub     # one host
agent fleet --report       # the baseline numbers
```

`FleetView` is `{at, hosts: [FleetHost]}`. A `FleetHost`:

| Field | From |
|---|---|
| `host`, `build`, `reachable` | the reply; `reachable: false` with `error` when the host didn't answer `fleet` |
| `agents[]` | the store's agents, as a tree by `spawned_by`: `name`, `session`, `spawned_by`, `backend`, `model`, `status`, `end_reason`, `detail`, `since` (when the status began), `held`, `oldest_held`, `session_id`, `session_name`, `cwd`, `flags[]`, `children[]` |
| `focus` | the host's focus (below) |
| `jobs[]` | running `agent run` jobs: `id`, `script`, `by`, `started_at`, `unit`, `memory_max` |
| `handoffs[]` | handoff docs older than their repo's last commit: `agent`, `path`, `doc_at`, `commit_at` |
| `friction` | `open`, `by_kind` |

Flags on an agent, each decided from the fields above:

- `stuck: waiting 14m` — status `waiting` for longer than 10 minutes.
- `stuck: 3 held, oldest 12m` — held messages older than 10 minutes, or 3 or more.
- `ended (quit), still recorded` — ended and not cleaned up.
- `name mismatch: session is "x"` — the CLI's session name or title isn't the agent's name.

The text view prints each host, its focus, the tree with flags, then jobs, stale
handoffs and friction counts, and leaves out sections that are empty.

`--report` prints a baseline per host: agents by status and backend, sessions in the
last 7 days by harness, messages sent per day over 7 days, friction open and resolved
by kind, and long sessions (running over 3 days, with their compaction counts).

## Name at launch

The agent's name is the CLI's session name: `claude --name <agent>` at `spawn` and
`resume` (`adopt` launches nothing, so it leaves a Claude session's name as is), and for
`agy` (which has no launch flag for a title) `/rename <agent>` typed automatically at
the conversation's first clean idle `Stop` (when no messages are held, no dialog is up,
and the input line is empty; recorded in `_named` so each conversation is named once,
including adopted and resumed `agy` sessions). `fleet` reads the name back from the
session's own metadata and flags a mismatch; it never renames a running session.

## Templates: starting agents across hosts

`<data dir>/templates.json` lists what this host agrees to start:

```json
{"templates": [
  {"name": "builder", "backend": "claude", "model": "claude-opus-5-5",
   "cwd": "/srv/project", "brief": "docs/lanes/builder.md"}
]}
```

```
agent template list [--host hub] [--json]
agent template start builder [--host hub]    # spawn agent "builder" from its template
agent template resume builder [--host hub]   # resume it if it has ended
agent template add builder --backend claude --cwd DIR [--brief FILE] [--model M] [--effort E]   # local only
agent template remove builder                                                      # local only
```

`template add` takes `--backend` (`claude`, `agy`, `aider`, or `shell`) from the global
`--backend` flag and requires `--cwd` (an existing directory, stored absolute);
`--brief` (relative to `--cwd` or this directory, must exist), `--model` and
`--effort` (claude and agy only) are optional, and a duplicate template name is refused until removed.

A `start` or `resume` request carries a template's name and who asks, nothing else: no
command, `cwd`, brief, or model. The host reads its own file and runs its own spawn. The
agent is named after the template; when an agent starts it, `spawned_by` is that agent
(`name@host` across the bus, `name` on the same host) so replies go back, and when a
person starts it, the agent has no parent (`spawned_by: ""`). A `from` that names this
host (in any case, or under a name it had before `agent bus rename`) is refused across
the bus, since `name@host` would resolve it to one of this host's own agents. `start`
refuses when the template's agent already exists (running, or ended and not cleaned up).
`start` records on the agent it spawns the state key `template`, the template's name
(written by `start` only, never from a request; an agent's own key, like `handoff`, so
resumes keep it). `template resume` brings back an ended agent only when this host's
own records show it was started from that template (its `template` key is the
template's name): any linked
host may ask for that one, rather than only the agent that started it, but an agent of
the same name started any other way (`agent spawn`, in another directory) is refused,
so the op never resumes an arbitrary agent by name, even where the bus doesn't serve
`resume`. Editing the file is local only (`remove` leaves any running agent of that
name untouched); no bus op writes it.

## Focus and follow

```
agent focus [--host hub] [--json]            # which agent has focus
agent focus helper [--json]                  # set it here (local only; publishes a FocusEvent)
agent focus --clear [--json]                 # clear it here (local only)
agent focus --follow [--host hub] [--json]   # a line now, then one per change, until Ctrl-C
```

Focus is one value per host, kept in this host's store (`host_state`, key `focus`), and
set or cleared only on its own host. Setting or clearing it publishes on
`gomux.<host>.focus`; `fleet` carries it for anyone who asks later. `--follow` prints a
one-line notice on stderr (`-q` drops it), reads the current value (from this host's
store locally, or op `fleet` for `--host`), and streams changes over the bus; with no
bus running locally, `agent focus --follow` re-reads this host's store every second
instead. Cleaning up the focused agent clears focus and publishes the change.

## `agent run`: heavy jobs

```
agent run build.sh [args...]          # run it in its own memory-limited scope, wait, exit with its code
agent run --memory-max 24G --memory-high 20G build.sh
agent run --memory-max 24G --no-limit-ok build.sh   # run even where 24G can't be applied
agent run --list [--all]              # running jobs (all: finished too)
agent run --show 17                   # one job's record, with the script as it ran
agent run --promote build.sh [--as nightly] [--force]   # copy it to the shared scripts dir
agent run nightly                     # run a promoted script by name
```

It takes a script on disk, not a command line. The script is read once; before
starting, it records a job in the store (`jobs`): id, path, sha256, the content read,
who started it (agent or person), host, cwd and start time; then the copy it runs,
the unit and the limits systemd applied; then the end time and exit code (or that it
never started, and why: `not_run`).

What runs is the job's own copy of the content read, `<data dir>/jobs/<id>/script`
(mode 0700, in directories of mode 0700), in the directory `run` was started in, so the
record is what ran whatever happens to the file meanwhile. A script with a `#!` line
runs with the interpreter it names (as the kernel would), an executable binary as
itself, anything else with `sh`. The difference from running the file itself: the
script's `$0` is the copy's path, so a script that finds its siblings by `$0` should
use `$AGENT_RUN_SCRIPT` (the original's absolute path); `$AGENT_JOB_ID` is the job's
id.

On Linux the script runs as `systemd-run --user --scope --unit
agent-job-<store>-<id>.scope -p MemoryHigh=... -p MemoryMax=... -p MemorySwapMax=...`,
where `<store>` is the first 8 hex digits of the sha256 of the data dir's absolute path
(every store's job ids start at 1, and all stores on an account share one user
manager). Its own scope with `MemoryHigh` and `MemoryMax` (defaults 75% and 85% of RAM)
and `MemorySwapMax=0`, so when memory runs out the job is what gets killed, not the
terminal that started it. The swap limit matters: on a machine with swap (zram counts),
a job over `MemoryMax` alone is never killed, it just swaps. systemd-run 254 and later
get `--expand-environment=no`, so the job's arguments reach it as given.

A job runs exactly once. systemd-run starts a tiny wrapper in the scope that stops
itself before anything of the job runs; `run` waits for that stop (so it knows the scope
exists), reads what systemd applied (`systemctl --user show <unit> -p MemoryHigh -p
MemoryMax -p MemorySwapMax`) and whether the kernel enforces it (the scope's cgroup has
`memory.max` and the rest: it doesn't where the memory controller isn't delegated to the
user's manager), and only then continues the wrapper into the script. What it prints
and records is what systemd applied; a difference from what was asked is said on
stderr. A limit given with `--memory-high`, `--memory-max` or `--memory-swap-max` that
can't be applied, for want of a scope or because systemd or the kernel doesn't hold
it, is an error and nothing runs (exit 1, the record `not_run`), unless `--no-limit-ok`.
Only the defaults fall back: where systemd-run is missing or can't create the scope,
the job runs plainly, without limits, said before it starts and in its record. A
systemd-run that dies in the scope before the script starts (a limit too small to
start anything) is the job's end, never a reason to run it again.

`SIGTERM` or `SIGHUP` to `run` stops the job, `systemctl --user stop <unit>` (or, run
plainly, the same signal to its process), records its end, and exits 128 + the signal.
`SIGINT` reaches the job from the terminal as before; `run` ignores it and records how
the job ended. A runner killed outright (`SIGKILL`) leaves the job running in its
scope; `--list` and `--show` find it ended later, and record when it was found so
(`found_gone_at`) with its end time and exit code unknown (null).

The environment gets `TMPDIR=/var/tmp` and `CARGO_BUILD_JOBS=12` unless the caller set
them (even to empty). Jobs run in parallel; there is no lock. On macOS the script runs
plainly and only the record is kept (an explicit limit is refused there too). Before
each run it also asks systemd to treat g3d-termd's user units as the last to kill
(`ManagedOOMPreference=avoid`), where systemd honours that; it says once if it can't.
It acts only on loaded units named exactly `g3d-termd-<namespace>-<n>.service` as
systemd lists them, never on a unit an alias led to or a not-found stub, and says when
it couldn't check that systemd-oomd sees the preference. systemd-oomd honours the
preference only on cgroups owned by root or by the owner of the cgroup it monitors;
where it monitors the root-owned user slice, a user's own units are not heeded, and
`run` says so. The job's own limit is what protects the terminal there.

Promoted scripts live in `<data dir>/scripts/<name>`, with the sha256 recorded at
promotion in the store (`scripts`). The record is what counts: running `<name>` reads
the file once and refuses it unless its sha256 is the recorded one; promoting a
different script under a recorded name needs `--force` whatever the file holds now
(edited or removed); promoting the recorded script again writes its file back.

## `agent skill install`

```
agent skill install                      # every harness found on this machine (CLI on PATH, or its config dir)
agent skill install --harness claude|agy # install for that one, found or not
agent skill show                         # print the skill text
```

One short text (`agent_service::skill_text`, `rs/crates/agent-service/src/service/instructions.rs`)
teaches a harness how to use `agent`: a map to `agent help`, not a manual. Claude Code
gets `<config dir>/skills/agent/SKILL.md` (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) and
the hook (`agent hooks install`, backing up `settings.json` to `<data dir>/backups/`);
`agy` gets `~/.gemini/config/skills/agent/SKILL.md`. Files that already match are left
untouched. A spawned agent's briefing (`instructions.md`) is built from the same text
plus who it is and who started it.

## Revive

```
agent revive helper                     # bring an ended agent back on its own conversation, re-briefed
agent revive helper --handoff docs/lanes/helper.md
```

`revive` is `resume` plus the handover: it continues the agent's own conversation
(claude or agy), or, when a person already restarted the CLI in the agent's pane, takes
that running session as the agent's without launching anything. Then it sends the
re-brief: where its handoff doc is and that it should read it and carry on. The
handoff doc is the agent's state key `handoff` (set by `spawn --handoff FILE` or
`agent state set`), else `docs/lanes/<name>.md` under its cwd when that exists; with
none, the re-brief says only that it was revived.

Revive never kills what it hasn't identified. It decides from the pane:

- **Take over** a CLI of the agent's backend it has identified: a process under the
  pane's shell, run by the same user, whose executable (resolved, not its argv text;
  for a script, the interpreter its `#!` line names running it) is the backend's CLI on
  PATH, in an interactive session (not `agy changelog`, `claude -p`, `claude auth
  login`). For Claude only a session Claude Code's session registry proves (pid and
  start time) counts. The process's pid and start time are checked again just before
  the record changes, and the change (old run closed, new run opened, the old run's
  status, reply and transcript cleared unless it runs the same session) is one store
  transaction. The taken-over agent then obeys send's rules like any other: messages
  are held while it is busy or at a dialog, held ones go first, and the deliverer types
  them once it is at its prompt. An agy started by hand has no hooks, so its status is
  read off its screen (its idle prompt, agy 1.5's mode hint included; its input box
  under a busy footer is processing; no box, as under the feedback survey, is
  unknown), and once its process is gone the agent reads `ended (quit)`.
- **Resume** when its session is gone, or the pane's shell sits at its prompt with no
  child processes.
- **Refuse** anything else (a process it can't identify, or a pane it can't read),
  naming the pid and command and saying what to do: look with `agent capture`, quit
  it and revive again, or `agent resume` to replace it knowingly.

The re-brief waits (up to 15s) for the agent to be at an idle prompt with an empty
input line, then goes in as `send` sends it, after any messages held for it;
otherwise it is held and goes in once the agent is (an agent resumed into a folder its
CLI hasn't trusted gets it once `agent approve` answers the trust prompt). It names the
doc's path, never its content. `--handoff FILE` takes a relative path from where revive
runs, a relative state key from the agent's directory; both are resolved with the
filesystem (symlinks, then `..`) and must be regular files. A state key that names
something else is an error before anything happens, naming the key and the path; it
never falls through to `docs/lanes/<name>.md`.

## Friction kinds and `_result` for agy

`friction add --kind` also takes `brief` (a brief was unclear), `handover` (a handover
lost something) and `skill` (the skill or briefing misled). An agy agent's final reply
for a turn is stored as `_result` at its Stop hook, read from its transcript, so
`agent state get <agy agent> _result` works as for Claude.
