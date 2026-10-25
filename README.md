# gomuxai

`agent` is a CLI that runs AI coding agents (Claude Code, agy, aider, or any
CLI you describe in JSON) in their own tmux sessions, and lets them
coordinate: spawn helpers, message each other, wait on each other, share
state. You can attach to any session to watch an agent work, and steer it
from your own terminal with the same commands the agents use.

What it adds over a pile of tmux windows:
- **Status from the CLIs themselves.** For Claude Code and agy, status
  (idle, processing, waiting on a permission prompt, exited) comes from the
  CLI's own hooks and session list, not from scraping the screen. A
  coordinator can wait for a helper's turn to end, then read its reply.
- **Permission prompts you can answer remotely**, after seeing the full tool
  call: `agent status -v`, then `agent approve --expect <tool>`.
- **A friction box.** Agents (and people) file what got in their way with
  `agent friction add`; resolved entries are the tool's changelog.
- **A bus across machines.** Pair hosts like SSH keys and address an agent
  on another host as `name@host`.

Agents learn the tool from [`CLAUDE.md`](CLAUDE.md), the manual they read.
How it works inside: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Install

Needs Go (the version in `go.mod`, currently 1.26) and tmux. The CLIs you
want to run (`claude`, `agy`, `aider`) must be on your `PATH`.

```bash
make build                     # builds bin/agent
cp bin/agent ~/.local/bin/     # anywhere you like
```

All data lives next to the binary, in `.gomuxai/` (the database, logs,
per-agent files, bus keys). Here that's `~/.local/bin/.gomuxai/`. Each copy
of the binary has its own data, so use one copy per machine. Delete the
folder to reset.

Release binaries: `make build-linux` and `make build-darwin` cross-compile
into `bin/`. There is no Windows build; use the Linux one under WSL.

### macOS

Build on the Mac with `make macos` if the Mac will link to another machine
over the bus. macOS only lets a process reach the local network if it holds
the Local Network permission, and processes inside tmux can't inherit one
from your terminal: the bus link fails with `no route to host`, and macOS
shows no alert. `make macos` embeds an Info.plist and signs the binary so it
can hold the permission itself.

- It signs with the first code-signing identity in your keychain; pick one
  with `make macos CODESIGN_IDENTITY=...`. An Apple Development or
  Developer ID certificate works; ad hoc signing doesn't keep the grant.
- Over SSH the login keychain is locked and `codesign` fails
  (`errSecInternalComponent`). Run `make macos` in a terminal on the Mac,
  or `security unlock-keychain` first.
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

Watch it any time with `tmux attach -t agent-helper` (detach with
`Ctrl-b d`). Give it a task, and wait for the turn that message starts:

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

`_result` holds a Claude agent's latest reply; it is empty while a turn is
running. When you're done:

```console
$ agent cleanup helper
cleaned up "helper": killed session agent-helper; removed its record, state, queued messages, and files; its log stays (agent logs helper)
```

Other ways in:
- `agent spawn builder --backend claude tasks/build.md` sends the agent a
  request to read that file as its first message; `--task <name>` sends a
  task saved with `agent task set`.
- `agent queue helper "..."` delivers when the agent is next idle, but only
  while `agent watch` runs somewhere on the host.
- `agent wait a b c --for idle,waiting --next` waits on several agents at
  once and prints a line as each one gets there (`--any`: the first).
- `agent wait --any --for waiting` blocks until any agent on the host is at
  a prompt and says which one and what it's asking; `agent wait --follow
  --for waiting` prints each new prompt until Ctrl-C, for a spare terminal.
- Agents run these same commands. An agent's helpers show under PARENT in
  `agent list`, and it can clean up only its own.

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

The host name is the short hostname unless `AGENT_HOST` is set. An identity
is bound to the name it was created with.

**2. Exchange bundles and trust them.** Copy each `<host>.pub.json` to the
other machine (`scp`, or `agent bus export` prints it), then on each:

```console
mac$ agent bus trust hub.pub.json
Trusted "hub": key UDZ7HRETTSGNRVHUYNVXT7RC…, certificate pin e4ff45de0734748eb8ad951c…
This host will link to it at tls://192.0.2.10:7422 (unless it is the hub).
Restart the bus to apply: agent bus stop && agent bus start ...
```

Compare the key and pin with what `bus init` printed on the other machine.
They are what each side pins; a mismatch means you copied the wrong file.

**3. Open the port on the hub.** Peers dial TCP 7422. With ufw:

```bash
sudo ufw allow from 192.0.2.0/24 to any port 7422 proto tcp
```

**4. Start.** The bus runs in the tmux session `gomux-bus`:

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
  remote ops allowed: list, status, capture, send, respond
```

Then address agents on the other machine as `name@host`:

```bash
agent list --host hub
agent send builder@hub "run the tests"
agent wait builder@hub --for idle,waiting --next --timeout 30m
agent status builder@hub -v
agent approve builder@hub --expect Bash
agent capture builder@hub
```

Peers may call only the ops in `--remote-ops`; raw `keys` stays local.
`state`, `logs`, and `queue` are per host: ask the agent to `send` you what
you need, or `capture` its screen. The bus's security model (TLS 1.3 with
pinned certificates, a key per host, no shared secrets) is described in
[`CLAUDE.md`](CLAUDE.md) and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

On the Mac, use a `make macos` build (see [macOS](#macos)). Without one, the
bus prints a note about it in its `gomux-bus` session
(`tmux attach -t gomux-bus`).

## Seeing what's going on

| Command | Shows |
|---|---|
| `agent` | Who you are, this host, your agents and their status, the bus, queued messages |
| `agent list` | Every agent here with STATUS and PARENT; `--host H` for another machine |
| `agent status NAME -v` | Status, where it came from, last event, and a pending prompt's full tool call |
| `agent capture NAME` | The agent's screen now, plus scrollback |
| `agent logs NAME` | What it did, from its CLI's own transcript: prompts, thinking, tool calls, replies. Works after cleanup. `--terminal` for the raw terminal log |
| `agent history` | Every `agent` command run on this host, by whom, and its exit code; `--failed` for failures |
| `agent wait --follow --for waiting` | A line each time any agent here is at a new prompt, with what it's asking |
| `agent events` | Status events as they're reported (needs a running bus) |
| `agent friction list` | Open requests and complaints about the tool |

```console
$ agent logs helper --lines 3
18:31:19 user  Run the shell command: curl -sI https://example.com
18:31:21 tool  Bash: curl -sI https://example.com → ok (11 lines)
18:31:35 reply example.com answered `HTTP/2 200`; Cloudflare served it from cache (`cf-cache-status: HIT`) as `text/html`. Here are th…
```

Most commands take `--json`. Results go to stdout, hints to stderr (`-q`
drops them). Exit codes: 0 done, 1 failed, 2 `wait` timed out, 3 `friction
add` found a similar entry.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Bus on the Mac never links; `.gomuxai/bus/bus.log` says `no route to host` | macOS blocks LAN connections from processes in tmux without their own Local Network permission | `make macos` on the Mac, allow the alert on the first `agent bus start` |
| `make macos` fails in `codesign` with `errSecInternalComponent` | Over SSH the login keychain is locked | Build in a terminal on the Mac, or `security unlock-keychain` first |
| `no bus answering for host "X"` | X's bus is down, not linked to yours, or doesn't allow that op | `agent bus status` on both machines; check the firewall and the `--url` in the hub's bundle |
| `agent bus status` says not running, though `gomux-bus` exists | The session name is shared by every data dir; another copy of `agent` owns it | Use that copy, or stop its bus first |
| Queued messages never arrive | No `agent watch` is running (`queue` says so) | Run `agent watch` in a spare terminal, or `agent send` instead |
| Agent stays `waiting` | A permission prompt, or a question for the user | `agent status NAME -v`, then `approve`/`deny`; for a question (e.g. AskUserQuestion), `agent capture NAME` and answer with `agent keys` |
| `send` refused | The agent is at a dialog (the Enter would answer it) or has exited | Answer the dialog first; for an exited agent, `agent logs NAME`, then `agent cleanup NAME` |
| Status `exited` or the tmux session is gone | The CLI quit or crashed | `agent logs NAME` shows how it ended; `agent cleanup NAME` removes the record |
| A tmux session `agent-NAME` with no agent record | Leftover session | `agent session kill agent-NAME` |
| Status `unknown` | aider and custom backends have no hooks; the screen matched no pattern | `agent capture NAME`; adjust the backend's patterns |
| A long message arrives in Claude as pasted content | Claude treats long typed input as a paste, i.e. data | Keep instructions short, or write them to a file and send its path |

Known gaps are listed in [`CLAUDE.md`](CLAUDE.md); open requests are in
`agent friction list`. If the tool gets in your way, `agent friction add
"..."`.
