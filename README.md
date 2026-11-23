# gomuxai

`agent` is a CLI that runs AI coding agents (Claude Code, agy, aider, or any
CLI you describe in JSON) in their own `g3d-termd` sessions, and lets them
coordinate: spawn helpers, message each other, wait on each other, share
state. You can attach to any session to watch an agent work, and steer it
from your own terminal with the same commands the agents use.

What it adds over a pile of terminal windows:
- **Status from the CLIs themselves.** For Claude Code and agy, status
  (idle, processing, waiting on a permission prompt, ended) comes from the
  CLI's own hooks and session records, not from scraping the screen. A
  coordinator can wait for a helper's turn to end, then read its reply.
  `agent statuses` says what each status means.
- **Permission prompts you can answer remotely**, after seeing the full tool
  call: `agent status -v`, then `agent approve --expect <tool>`.
- **A friction box.** Agents (and people) file what got in their way with
  `agent friction add`; resolved entries are the tool's changelog.
- **A bus across machines, fleet view, and memory-scoped jobs.** Pair hosts
  like SSH keys, address agents as `name@host`, start pre-approved cross-host
  templates, see stuck agents and stale handoffs in `agent fleet`, and run
  heavy builds in memory-limited scopes with `agent run`.

Agents learn the tool from [`CLAUDE.md`](CLAUDE.md), the manual they read.
How it works inside: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/FLEET.md`](docs/FLEET.md).

## Install

Needs Rust 1.96+ (for `agent`), Go 1.26+ (for `agent-busd`, the embedded NATS
server `agent bus run` starts), and `g3d-termd` (`g3d` for interactive `agent
attach`). The CLIs you want to run (`claude`, `agy`, `aider`) must be on your
`PATH`.

```bash
make build                     # builds bin/agent and bin/agent-busd
make install                   # installs both over the agent on PATH, else ~/.local/bin (BINDIR=… to choose)
```

All data lives next to the binary, in `.gomuxai/` (the database, templates,
promoted scripts, logs, per-agent files, bus keys). Here that's
`~/.local/bin/.gomuxai/`, with `agent-busd` beside `agent`. Each copy of the
binary has its own data, so use one copy per machine. Delete the folder to
reset.

Static `linux-arm64` binaries: `make arm64` builds `bin/linux-arm64/agent` and
`bin/linux-arm64/agent-busd` (needs `cargo-zigbuild` and the
`aarch64-unknown-linux-musl` target). There is no Windows build; use Linux
under WSL.

Install the `agent` skill for the CLIs found on your machine (and register
Claude Code's global hook in one step):

```bash
agent skill install            # installs SKILL.md for claude and/or agy, plus Claude's hook
agent skill install --harness claude   # or agy: installs for that one, found or not
agent skill show               # prints the concise skill text
```

Or manage Claude Code's hook directly:

```bash
agent hooks install            # adds "<this binary> hook" to ~/.claude/settings.json
agent hooks status             # per event: installed, missing, or another binary's
```

Claude Code then runs `agent hook` for every session on the machine. It
works out from the running processes whether the session is an agent's:
an agent's gets its status reported, held messages handed over, and its
safe `agent` commands approved; any other session is only recorded, so
`agent sessions` can say how it ended, and never sees output from the hook.
`install` leaves your other hooks and settings alone, backs the file up to
`.gomuxai/backups/` first, and does nothing when it's already current;
`agent hooks remove` takes the entries out. `agent spawn --backend claude`
refuses until the hook is installed. Sessions already running pick the
change up when restarted.

### macOS

Build on the Mac with `make macos`, then `make install`, if the Mac will link
to another machine over the bus. macOS only lets a process reach the local
network if it holds the Local Network permission, and processes inside a
background `g3d-termd` daemon can't inherit one from your terminal: the bus
link fails with `no route to host`, and macOS shows no alert. Because
`agent-busd` is the process that dials the LAN, `make macos` embeds an
`Info.plist` into and signs both `bin/agent` and `bin/agent-busd` so
`agent-busd` holds the permission itself.

- It signs with the first code-signing identity in your keychain; pick one
  with `make macos CODESIGN_IDENTITY=...`. An Apple Development or
  Developer ID certificate works; ad hoc signing doesn't keep the grant.
- Signing needs the login keychain unlocked in the Mac's GUI login session
  (over SSH, a `g3d-termd` daemon started over SSH fails in `codesign` with
  `errSecInternalComponent`). Run `make macos` in a terminal on the Mac, or in
  a `g3d-termd` daemon started from the GUI session (see
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#signing-over-ssh)).
- On the first `agent bus start`, macOS asks to allow "gomuxai" to find
  devices on the local network: allow it. The grant survives rebuilds.

A plain `make build` is fine on a Mac that only runs agents locally, or that
is the hub (accepting links needs no permission).

## Quick start

Run `agent` alone to see where you are:

```console
$ agent
You are a person at a terminal, on host laptop.
Agents here: none yet.
Bus: not set up (only needed to reach other hosts: agent bus --help)
Start with: agent spawn <name> --backend claude, then agent send <name> "<task>". All commands: agent help
```

Spawn a Claude agent. It starts in the current directory, with that
project's Claude settings:

```console
$ agent spawn helper --backend claude
Spawned agent helper (session agent-helper, backend claude).
It has no task yet: agent send helper "..."
It is idle: ready for a message.
```

Watch or steer it any time with `agent attach helper`, which opens its
session in your terminal via `g3d attach --take` (press `Ctrl-\` twice to
detach); `agent spawn --attach` spawns and attaches in one step. Give it a
task, and wait for the turn that message starts:

```console
$ agent send helper "Run the shell command: curl -sI https://example.com" \
    && agent wait helper --for idle,waiting --next --timeout 10m
Sent to helper (it was idle).
helper: waiting
hint: helper is asking: permission: Bash: curl -sI https://example.com; agent status helper -v shows all of it, then agent approve helper or agent deny helper
```

`waiting` means it is blocked on a person: here, a permission prompt. See
exactly what it wants to run, then answer:

```console
$ agent status helper -v
helper: waiting
  source: native
  event:  PermissionRequest (1s ago)
  detail: permission: Bash: curl -sI https://example.com
  tool:   Bash
  input:  command: curl -sI https://example.com
          description: Fetch HTTP headers from example.com
$ agent approve helper --expect Bash
Approved helper's Bash prompt (permission: Bash: curl -sI https://example.com).
```

`--expect Bash` refuses if a different prompt replaced the one you read.
`agent deny helper` answers no (for Claude, that cancels the turn). Wait
again, then read its reply:

```console
$ agent wait helper --for idle,waiting --timeout 10m
helper: idle
$ agent state get helper _result
example.com answered `HTTP/2 200`; ...
```

`_result` holds a Claude or agy agent's latest reply; it is empty while a turn
is running. When you're done:

```console
$ agent cleanup helper
cleaned up "helper": killed session agent-helper; removed its record, state, queued messages, and files; its log stays (agent logs helper)
```

Other ways in:
- `agent spawn builder --backend claude tasks/build.md` sends the agent a
  request to read that file as its first message; `--task <name>` sends a
  task saved with `agent task set`.
- `agent spawn lead --backend claude --resume <session> --attach` brings
  a Claude or agy session you ran in a terminal into the workgroup: quit
  that CLI, and the agent continues the same conversation in its own
  session, in the directory it ran in, where `agent send` reaches it.
- `agent send` to a busy Claude or agy agent holds the message (`Held for
  helper until its turn ends`) and a hook hands it over: Claude's Stop when
  the turn ends, agy's PostInvocation when its current step ends;
  `agent send helper --now "..."` types it in at once instead.
- `agent queue helper "..."` delivers when the agent is next idle: for a
  Claude or agy agent it does what `send` does; for others, only while
  `agent watch` runs somewhere on the host.
- `agent wait a b c --for idle,waiting --next` waits on several agents at
  once and prints a line as each one gets there (`--any`: the first).
- `agent wait --any --for waiting` blocks until any agent on the host is at
  a prompt and says which one and what it's asking; `agent wait --follow
  --for waiting` prints each new prompt until Ctrl-C, for a spare terminal.
- `agent adopt <name>` (or `agent adopt <name> --session <session>`) gives
  an existing `g3d-termd` session a seat on the bus without restarting what is
  running in it; `agent cleanup <name>` unregisters it and leaves the
  session running. `agent resume` refuses an adopted agent: if its CLI
  exits, start it there yourself (`claude -c` continues its conversation).
- `agent spawn builder --backend claude --handoff docs/lanes/builder.md
  tasks/build.md` records the agent's handoff doc; if it ends, `agent revive
  builder` continues its session (or takes over a CLI you started in its pane)
  and re-briefs it from that doc. A Claude session is named `<agent>` at
  launch; an agy conversation is renamed after its first turn.
- `agent run build.sh` runs a heavy build or test script in its own
  memory-limited systemd scope (recording the script, sha256, limits, and exit
  code in `jobs`) so an OOM kills the job instead of the agent's terminal.
- Agents run these same commands. An agent's helpers show nested under their
  parent in `agent list`, and it can clean up only its own.

Every command has details and tested examples in `agent help <command>`.

### Other backends

Built in: `claude`, `aider`, `agy`. Anything else is a JSON file:

```json
{
  "name": "my-cli",
  "launch_command": "my-cli --some-flag",
  "init_message_format": "Please read %s",
  "idle_pattern": "^> $",
  "processing_pattern": "(Working|Thinking)"
}
```

```bash
agent spawn worker --backend-config my-cli.json
agent status worker --backend-config my-cli.json   # pass it again: it isn't remembered yet
```

Status for aider and custom backends comes from matching these patterns
against the screen, so it can be `unknown`.

## Two machines

Each machine runs one bus; one machine, the hub, accepts links from the
others. Pairing copies only public material. This example links a Linux hub
at `192.0.2.10` and a Mac named `mac`.

**1. Create identities.** On the hub, give the address peers will dial:

```console
hub$ agent bus init --url tls://192.0.2.10:7422
Created bus identity for host "hub" in ~/.local/bin/.gomuxai/bus
Public bundle: ~/.local/bin/.gomuxai/bus/hub.pub.json
  key UDZ7HRETTSGNRVHUYNVXT7RC…, certificate pin e4ff45de0734748eb8ad951c…
Copy it to each peer and run `agent bus trust` there.
```

On the Mac (and any other machine): `agent bus init`, no `--url`.

The host name is `AGENT_HOST` if set, else the machine's name: on macOS its
LocalHostName (Sharing settings), elsewhere the short hostname. An identity
is bound to the name it was created with, and once it exists that name is
the default, so a network that renames the machine (macOS can become "Mac")
doesn't change it.

**2. Exchange bundles and trust them.** Copy each `<host>.pub.json` to the
other machine (`scp`, or `agent bus export` prints it), then on each:

```console
mac$ agent bus trust hub.pub.json
Trusted "hub": key UDZ7HRETTSGNRVHUYNVXT7RC…, certificate pin e4ff45de0734748eb8ad951c…
This host will link to it at tls://192.0.2.10:7422 (unless it is the hub).
Restart the bus to apply: agent bus restart
```

Compare the key and pin with what `bus init` printed on the other machine.
They are what each side pins; a mismatch means you copied the wrong file.

**3. Open the port on the hub.** Peers dial TCP 7422. With ufw:

```bash
sudo ufw allow from 192.0.2.0/24 to any port 7422 proto tcp
```

**4. Start.** The bus runs in the `g3d-termd` session `gomux-bus` (namespace `agents`):

```bash
hub$ agent bus start --listen 0.0.0.0:7422
mac$ agent bus start
```

**5. Check.**

```console
hub$ agent bus status
hub: bus running (clients on tls://127.0.0.1:7420)
  hub, accepting peer links on 0.0.0.0:7422
  linked peers: 1; trusted: mac
  remote ops allowed: list, status, capture, send, respond, friction, logs, fleet, template, manage
```

Then address agents on the other machine as `name@host`:

```bash
agent list --host hub
agent send builder@hub "run the tests"
agent wait builder@hub --for idle,waiting --next --timeout 30m
agent status builder@hub -v
agent approve builder@hub --expect Bash
agent deliver builder@hub            # type its held messages now, if it is at its prompt
agent capture builder@hub
```

Peers may call only the ops in `--remote-ops`; raw `keys` stays local.
To let other machines start or resume pre-approved agents on the hub without
opening arbitrary remote spawn, define a template on the hub (stored locally in
`.gomuxai/templates.json`; `add` requires `--backend` and `--cwd`, with
optional `--brief`, `--model` and `--effort`):

```bash
hub$ agent template add builder --backend claude --cwd /home/user/project --brief docs/lanes/builder.md
mac$ agent template list --host hub
mac$ agent template start builder --host hub    # or: agent template resume builder --host hub
```

Started by a person, the agent has no parent; started by an agent, its parent
is `<caller>@<caller-host>` so replies go back.

Open `spawn` and `resume` are off for peers by default, since they start an
arbitrary process on the host. To allow them too, add them on the hub: `agent
bus restart --remote-ops
list,status,capture,send,respond,friction,logs,fleet,template,manage,spawn,resume`
(`remote ops allowed` then lists them). The bus saves its settings, so a later
`agent bus start`, after a reboot say, runs with the same ops (on Linux,
`agent bus service install` makes a systemd user unit start it at boot, and
`--resume a,b` resumes those agents after it). Then `agent
spawn builder@hub --backend claude --cwd /home/user/project` starts one on the
hub; `--cwd` and a task file are paths on the hub.
Listing `spawn` lets every linked peer run commands on the hub as the bus's
user (spawn an agent, then approve its prompts), so do it only if you'd
give those machines a shell there. `agent resume builder@hub` starts an
ended agent there again, and `agent logs builder@hub` prints what `agent
logs` prints there. `agent attach builder@hub` opens its session in your
terminal over ssh (the bus can't carry a terminal), so it needs `ssh hub`
to work from here; `agent bus ssh hub user@hub.lan` sets where ssh goes. Clean
it up on the hub. `state`, `queue`, and `revive` are per host: ask the agent
to `send` you what you need, or `capture` its screen. The bus's security model
(TLS 1.3 with pinned certificates, a key per host, no shared secrets) is
described in [`CLAUDE.md`](CLAUDE.md) and
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

**Later changes.** `agent bus untrust <host>` drops a peer (its bundle and
ssh destination) and restarts the bus; run it on both sides. `agent bus
rename <new-name>` renames this host, keeping its keys, restarts its bus as
the new name, and prints what each peer runs to follow it:

```console
mac$ agent bus rename studio
Renamed this host's bus identity from "mac" to "studio" in …/.gomuxai/bus
  same key UCITKEFZEAI3UG7XFYNTEKYMUCZNV4YY…, certificate pin f890ca201cadbb67a402a2c2…
…
  on hub: agent bus trust studio.pub.json --renames mac
hub$ agent bus trust studio.pub.json --renames mac
Trusted "studio" as mac renamed: same key UCITKEFZEAI3UG7XFYNTEKYMUCZNV4YY…, certificate pin f890ca201cadbb67a402a2c2…
```

`trust --renames` refuses a bundle whose keys aren't the old name's, and
updates the hub's references to the old name. `agent bus restart` restarts
the bus with the settings it runs with.

On the Mac, use a `make macos` build (see [macOS](#macos)). Without one, the
bus prints a note about it in its `gomux-bus` session
(`g3d --ns agents attach --take gomux-bus`).

## Seeing what's going on

| Command | Shows |
|---|---|
| `agent` | Who you are, this host, your agents and their status (and held messages), the bus, messages held for you |
| `agent list` | Every agent here in a nested tree with status, backend, parent, and session; `--host H` for another machine |
| `agent fleet [--host H] [--json] [--report]` | Agent tree across this host and trusted peers with stuck flags, focus, running jobs, stale handoff docs, friction, and optional 7-day `--report` ([`docs/FLEET.md`](docs/FLEET.md)) |
| `agent focus [NAME] [--clear] [--host H] [--follow] [--json]` | Show, set, or follow which agent has focus on a host (over the bus, or polling this host's store) |
| `agent run --list [--all]` / `agent run --show ID` | Running (or finished) `agent run` jobs, limits, exit codes, and the exact script run |
| `agent status NAME -v` | Status and what it means, where it came from, last event, and a pending prompt's full tool call |
| `agent statuses` | Every status and end reason (`ended (quit)`, `ended (gone)`, ...) with what it means |
| `agent hooks status` | Whether Claude Code runs this binary's hook on every event, and the hook's last errors |
| `agent capture NAME` | The agent's screen now, plus scrollback, as plain text; `--raw` keeps colors and styling |
| `agent logs NAME` | What it did, from its CLI's own transcript: prompts, thinking, tool calls, replies. Works after cleanup. `--terminal` for the raw terminal log |
| `agent history` | Every `agent` command run on this host, by whom, and its exit code; `--failed` for failures |
| `agent wait --follow --for waiting` | A line each time any agent here is at a new prompt, with what it's asking |
| `agent events` | Status events as they're reported (needs a running bus) |
| `agent export --json --follow` | The whole agent tree for a display or script, then a JSON line per change (spawn, status, cleanup); `--all` adds departed agents. Shapes in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#export) |
| `agent sessions [--json] [--all\|--limit N] [--since 2h] [--project P]` | The agent sessions on this host, newest 25 by default (`--json`: all), workgroup agents' and the Claude Code, agy and Kimi sessions people ran, in one shape ([`docs/EVENTS.md`](docs/EVENTS.md)) |
| `agent transcript NAME --follow` | One session's events (by agent name, session id or path) as JSON lines, live: what a display reads ([`docs/EVENTS.md`](docs/EVENTS.md)) |
| `agent friction list` | Open requests and complaints about the tool |

```console
$ agent logs helper --lines 3
18:31:19 prompt Run the shell command: curl -sI https://example.com
18:31:21 tool   Bash: curl -sI https://example.com → ok (11 lines)
18:31:35 say    example.com answered `HTTP/2 200`; Cloudflare served it from cache (`cf-cache-status: HIT`) as `text/html`. Here are th…
```

Most commands take `--json`. Results go to stdout, hints to stderr (`-q`
drops them). Exit codes: 0 done, 1 failed, 2 `wait` timed out, 3 `friction
add` found a similar entry (`agent run` exits with its script's exit code).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Bus on the Mac never links; `.gomuxai/bus/bus.log` says `no route to host` | macOS blocks LAN connections from `agent-busd` in a background daemon without its own Local Network permission | `make macos && make install` on the Mac, allow the alert on the first `agent bus start` |
| `make macos` fails in `codesign` with `errSecInternalComponent` | The shell isn't in the Mac's GUI login session (where the login keychain is unlocked) | Build in a terminal on the Mac, or in a `g3d-termd` daemon started in the GUI session |
| `no bus answering for host "X"` | X's bus is down, not linked to yours, or doesn't allow that op | `agent bus status` on both machines; check the firewall and the `--url` in the hub's bundle |
| `agent fleet` shows a linked host as `unreachable` | Its bus doesn't list `fleet` in `--remote-ops` (settings saved before `fleet` existed win over the defaults) | On that host: `agent bus restart --remote-ops <its list>,fleet,template,manage,roster` |
| `agent bus status` says not running, though `gomux-bus` exists | The session name is shared by every data dir; another copy of `agent` owns it | Use that copy, or stop its bus first |
| Queued messages never arrive (aider, custom backends) | No `agent watch` is running (`queue` says so) | Run `agent watch` in a spare terminal, or `agent send` instead |
| A held message hasn't arrived | The agent's turn (agy: its current step) is still running, or ended with no hook (Esc, Ctrl+C, deny), or something covers its input line (`it isn't on screen`) | `agent status NAME -v` shows `held:`; a deliverer types it 10s after the turn ends; `agent deliver NAME` (or `NAME@host`, over the bus) types it now if the agent is at its prompt; `agent capture NAME` shows what covers the input line |
| Agent stays `waiting` | A permission prompt, or a question for the user | `agent status NAME -v`, then `approve`/`deny`; for a question (e.g. AskUserQuestion), `agent capture NAME` and answer with `agent keys` |
| `send` refused | The agent's session has ended, or it is at a dialog (the Enter would answer it) and the message can't be held (`--now`, or not a Claude or agy agent) | Answer the dialog first; for an ended agent, `agent revive NAME` or `agent resume NAME`, or `agent logs NAME` then `agent cleanup NAME` |
| Status `ended (...)` or the session is gone | No CLI runs the agent's session any more: `quit` (it ended and said so), `gone` (it crashed, `g3d-termd` was stopped, or the machine rebooted), ... (`agent statuses` lists them all) | `agent resume NAME` continues its session (claude and agy), `agent revive NAME` continues it (or takes over a CLI restarted in its pane) and re-briefs from its handoff doc, `agent resume --all` every one a reboot ended (`--dry-run` first); `agent logs NAME` shows how it ended; `agent cleanup NAME` removes the record, `agent cleanup --ended` every ended one (`--dry-run` first) |
| After `agent revive`, an agy agent says it runs without hooks | A person started `agy` by hand in its pane, without `agent`'s `--add-dir` hooks | Quit `agy` in its pane and run `agent revive NAME` again so `agent` launches it |
| `agent run` says `runs without memory limits` or `couldn't ask systemd to spare g3d-termd's units` | On macOS or Linux without a user `systemd` manager, jobs run plainly with provenance only; where `systemd-oomd` watches the root-owned user slice, user `g3d-termd` units carry no `user.oomd_avoid` xattr | On Linux with a user `systemd`, the job's own `MemoryMax` + `MemorySwapMax=0` scope protects the terminal (`agent run --show ID` shows what applied) |
| `spawn --backend claude` says Claude Code's settings don't run this binary's hook | The global hook isn't installed, or runs another copy of `agent` (one hook serves one binary: its store hears the sessions) | `agent hooks install`; for another copy, spawn with the copy the hook runs, or `agent hooks install --force` (that copy's agents then stop reporting) |
| A Claude agent's status never changes, or held messages never arrive | Its CLI started before `agent hooks install`, or the hook is failing | `agent hooks status` shows the last errors; restart the agent's CLI to load the hook |
| A session `agent-NAME` with no agent record | Leftover session | `agent session kill agent-NAME` |
| Status `unknown` | aider and custom backends have no hooks; the screen matched no pattern | `agent capture NAME`; adjust the backend's patterns |
| A long message arrives in Claude as pasted content | Claude takes typed input over 800 characters, or with 3+ line breaks, as a paste, i.e. material. `send` hands such a message over by hook instead, but only to a Claude CLI the hook reaches | `agent hooks status`; restart the agent's CLI (`agent resume`) so it runs the hook |
| `send` says it typed a short line in place of the message, but the hook didn't take the whole message | The hook doesn't reach the session the agent's conversation runs in now | `agent status NAME -v` (a session sent to the background with `/bg` is followed through its transcript; `agent hooks status` shows hook errors) |
| `send` says the message is still in the input box after 3 Enters; `status` shows `unsent:` | Claude took each Enter as a newline or ignored it. `send` waits for the box to show the text before pressing Enter, and presses again while it stays (see ARCHITECTURE, "Submitting a typed message") | Don't send it again: `agent capture NAME` shows it, `agent key agent-NAME Enter` submits it |

Known gaps are listed in [`CLAUDE.md`](CLAUDE.md); open requests are in
`agent friction list`. If the tool gets in your way, `agent friction add
"..."`. The same problem filed on two hosts is linked with `agent friction
link ID ID@HOST`, so resolving either resolves both.
