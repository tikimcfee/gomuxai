# Shared TUI Interaction — Mechanism Ideation

## Framing

The actual axis of choice is **where the "shared truth" lives**: the rendered byte-stream, the pty, the in-memory app state, or a message bus *above* the app. Every mechanism is a different answer to "what does the second participant attach to?" Sharing the screen is the easy part — `tmux` solved it decades ago. The hard part is **input**: two intentional actors poking at a single-cursor, mostly-modal TUI without stomping each other, while each maintains a coherent mental model of *why* the other moved. Bubbletea's runtime is single-threaded over a `Msg` channel, so any solution funnels into that channel — the question is what gets to enqueue, with what provenance, and who arbitrates when intents collide. A second axis: **fidelity vs. semantics** — do agents perceive raw VT100 frames (faithful but opaque to reasoning) or structured app state (legible but requires the TUI to expose itself)? LLM agents in particular reason poorly over ANSI escape sequences and cell grids; humans reason poorly over JSON state dumps. A good design probably gives each participant the substrate that matches their cognition, not a single shared one.

The temptation is to pick the most general mechanism. Resist it. The smallest demo that proves "two agents both moved a node and saw each other do it" is worth more than a perfectly abstracted bus.

---

## Mechanism A — tmux native (shared session + control mode)

### Vision
The TUI runs once inside a tmux session. The human attaches normally. The agent attaches as a second client to the *same* session via `tmux attach -t <name>`, or — more interestingly — speaks `tmux -C` (control mode) which gives a structured event stream of pane content and accepts `send-keys` as command input. Both clients see identical renders because tmux is the single source of truth for the pane's screen buffer.

### Screen sharing
Free. tmux already broadcasts pane updates to every attached client. The agent in control mode receives `%output` notifications with the literal bytes the pane emits; it can scrape those, or call `capture-pane -p` on demand for a snapshot. Humans see normal terminal output.

### Input multiplexing
Agent sends `send-keys -t <pane> '...'`. Human types directly. tmux serializes both into the pty's stdin in arrival order — there is no structural conflict resolution, just temporal interleaving. If the human is mid-keystroke for a node name and the agent fires `send-keys`, characters interleave inside the input field. **Conflict resolution is a social protocol layered on top**: e.g., the agent only sends keys when the TUI is in a known idle state, detected by polling `capture-pane` or by the TUI emitting sentinels.

### Infrastructure
- A named tmux session.
- Agent invokes `tmux` as a subprocess (the Bash tool already can).
- Optionally a thin `tmux -C` reader process to give the agent live frame deltas instead of poll-snapshots.
- Zero new daemons, zero changes to the TUI.

### Multi-agent fit
N agents = N tmux clients on the same session. Scales naturally for *viewing*. For *input*, the lack of arbitration gets worse linearly with N — the pty becomes a contended resource with no lock. Workable up to ~3 cooperative agents with explicit turn-taking; falls apart in a free-for-all.

### Trade-offs
- **Shines**: zero TUI changes, works today, faithful render, human and agent are symmetric peers.
- **Breaks down**: no input arbitration, agent sees ANSI bytes (poor LLM substrate), no way to express *intent* — only keystrokes. The agent can't say "select node X" without first scraping the screen to find where X is.

---

## Mechanism B — pty multiplexer (own the pty, fan out)

### Vision
A small daemon owns the pty that the TUI runs against. It exposes two sockets: a "viewer" socket that streams the raw TTY output to any number of subscribers, and a "writer" socket that accepts input events tagged with a participant ID. Effectively a custom tmux, but you control the policy. The human's terminal connects through a thin shim that pipes to/from the daemon; the agent connects directly via Unix socket.

### Screen sharing
Daemon tees pty output to all viewer subscribers. Each viewer is responsible for terminal emulation on their end (or the daemon emulates and ships a cell grid — pick one).

### Input multiplexing
Every input event carries `{participant_id, payload, timestamp}`. The daemon enforces a policy: round-robin token, exclusive write-lock with timeout, priority by role (human preempts), or queued bursts with idle gaps. Because the daemon sees every byte before it hits the pty, it can also *reject* or *delay* writes — which tmux cannot.

### Infrastructure
- A new Go daemon (a few hundred lines): pty allocation, fan-out, simple socket protocol.
- Connection shim for the human's terminal (or just `socat` + a wrapper script).
- Optionally an embedded VT100 emulator if you want to ship structured grid state instead of raw bytes.

### Multi-agent fit
Native. Participant ID is the primitive. Ports straight to NATS by replacing socket reads/writes with subject pubs/subs: `tui.<id>.output` fan-out, `tui.<id>.input` with the daemon as the sole subscriber. Arbitration policy lives in one place.

### Trade-offs
- **Shines**: real input arbitration, clean provenance, NATS-shaped from day one, TUI itself unchanged.
- **Breaks down**: you've reimplemented the boring 80% of tmux. Agents still see ANSI bytes unless you bolt on emulation. Debugging pty edge cases (resize, signals, alt-screen) is its own swamp.

---

## Mechanism C — TUI streams frames to a socket

### Vision
Modify the TUI's renderer to publish each frame (as a structured cell grid, or as ANSI bytes) to a socket instead of/in addition to stdout. Clients — human terminal renderer, agent observer — subscribe and render the frame however they want. The TUI is no longer bound to a pty; it's a frame producer.

### Screen sharing
The TUI ships a `Frame{cells, cursor, mode}` message after each Bubbletea `View()` call. A small "human renderer" client converts frames to ANSI and writes to the user's terminal. The agent client can consume the same frames as a 2D char array — much friendlier for an LLM than ANSI.

### Input multiplexing
Inputs flow back over the same socket as `Input{participant_id, key/event}`. The TUI's `Update()` loop receives them as Bubbletea `Msg`s. Now the TUI itself is the arbiter: it can branch on `participant_id`, ignore inputs in certain modes, show different cursors per participant, etc. This is the first mechanism where the *application* can express "agent A is dragging node 7, agent B can't drag it right now."

### Infrastructure
- TUI gets a new transport layer (replace `os.Stdout` writes with a publish; replace stdin reader with a subscription).
- Human-side renderer (~100 lines: read frame, paint terminal).
- Bus: Unix socket → eventually NATS subjects.

### Multi-agent fit
Excellent. Each participant is just another subscriber + publisher. The TUI can render *per-participant* views (e.g., overlay each agent's cursor in a different color in the human's view, while sending each agent only the cells in their region of interest). Maps cleanly to NATS: `tui.frames` (broadcast), `tui.input.<participant>` (per-agent input).

### Trade-offs
- **Shines**: clean separation of model/transport/render, agents get a sane substrate, multi-agent affordances become natural application-level features, easy to record/replay frames.
- **Breaks down**: you must touch the TUI. Frame-rate over a socket is fine on localhost but hits backpressure if a slow subscriber stalls — needs drop-or-coalesce policy. The human renderer is now your problem to keep working across terminal quirks.

---

## Mechanism D — Headless TUI + remote renderers (state replication)

### Vision
The "TUI" stops being a TUI. It's a node-graph runtime that emits *state diffs* to subscribers; each subscriber renders its own view from local state. The human runs a Bubbletea client that reduces diffs into a Bubbletea model and renders to their terminal. The agent runs a much smaller "client" that just keeps a Go struct in sync and exposes it as a queryable handle. Different participants can render different facets — the human sees the spatial graph, the agent sees a table of nodes-and-edges.

### Screen sharing
There is no shared screen. There is a shared *model*. The screen is a derived projection per participant. Two humans get the same projection; an agent gets whatever projection their client implements.

### Input multiplexing
Inputs become *intents*: `MoveNode{id: 7, to: (x,y), by: <participant>}`. The runtime is the single Update loop; it accepts intents from the bus, validates, applies, and emits the resulting diff. Conflict resolution is now domain-typed — two `MoveNode` intents on the same node can be ordered, rejected, or merged with full knowledge of *what* they mean, not just *which keystrokes* they were.

### Infrastructure
- Refactor the TUI into `runtime/` (state + reducer) and `client-tui/` (renderer).
- A pub/sub bus (Unix socket → NATS).
- A `state.proto` or equivalent typed diff vocabulary.

### Multi-agent fit
This is what gomuxai wants to be. Every agent is a peer with the human; the runtime is just another subject on the bus. N scales by adding subscribers. Per-agent permissions become a runtime-level concern, not a transport hack.

### Trade-offs
- **Shines**: best fit for LLM cognition (typed intents, typed state), best fit for multi-agent scaling, best fit for replay/audit/branching. Future-proof.
- **Breaks down**: largest refactor. You've now built a CRDT-adjacent system; ordering, causality, and rollback are real concerns. Overkill for "two participants poke a graph." If the TUI's value is partly *being* a TUI (idiomatic terminal feel for the human), inverting it costs that.

---

## Mechanism E — API-driven TUI (screen as side-effect viewer)

### Vision
The TUI exposes an HTTP/RPC API for every meaningful action: `POST /nodes`, `PATCH /nodes/:id`, `POST /edges`. The human pokes the TUI via keystrokes as today; those keystrokes are translated *internally* into the same API calls. The agent skips the keyboard entirely and calls the API directly. The screen update happens because the API mutates the same state the TUI renders from. Minimum viable cohabitation.

### Screen sharing
The human watches the TUI normally. The agent has no screen — it has API responses and optionally an `/state` endpoint or an SSE stream of state changes. If the agent *wants* to see, it pulls `/state` and renders its own representation, or screenshots via `tmux capture-pane`.

### Input multiplexing
There is no shared input channel. Human inputs flow through the keyboard → TUI. Agent inputs flow through the API → TUI. They meet at the state mutation layer. Conflicts resolve at whatever granularity the API enforces (optimistic concurrency, last-write-wins, transactional endpoints — your choice).

### Infrastructure
- Add an HTTP server to the TUI.
- Refactor key handlers to call the same internal functions the API calls (so there's one mutation path).
- Optional SSE/WS for state streaming.

### Multi-agent fit
Good for tool-use agents. N agents is N HTTP clients. NATS-friendly: replace HTTP with request/reply on subjects. But each agent is *blind* unless you also build the state-stream side, which slides this toward Mechanism D.

### Trade-offs
- **Shines**: simplest "agent can do something" path. Matches how LLM tool-use already thinks. Doesn't require the agent to understand a TUI at all.
- **Breaks down**: agents and humans are no longer co-present. The agent isn't *watching* the human; it's poking a database that the human happens to be staring at. You lose the "they react to each other in real time" property unless you bolt on a stream — and once you do, you're halfway to D.

---

## Hybrid worth naming: C + E

The most pragmatic combination: **frame stream out, intent API in.** The human gets a faithful TUI render via the frame stream (Mechanism C's human renderer). The agent gets *both* a structured frame view (so it can "see" what the human is doing) *and* a typed intent API (so it doesn't have to fake keystrokes). One bus, two substrates, each cognitively matched. Conflicts resolve at the intent layer because that's the only mutation path; keystrokes from the human's renderer are also lifted into intents before hitting the runtime. This is probably where the design wants to land after the demo.

---

## What I'd build first

The smallest thing that proves "two participants both wired a node and watched each other do it":

- **Take Mechanism A as the substrate, no TUI changes.** Run the existing Bubbletea app inside a named tmux session. Human attaches. Agent drives via `tmux send-keys` and `tmux capture-pane -p` from Bash tool calls.
- **Add one sentinel: a status line in the TUI that prints `READY` when idle.** The agent only sends keys when the last `capture-pane` line equals `READY`. This is the dumbest possible arbitration and it's enough for a demo.
- **Script one shared task**: human creates node A, agent creates node B, human wires A→B. Record the tmux session with `asciinema` to prove both participants visibly contributed.
- **If and only if that works, prototype Mechanism C** — replace the TUI's stdout with a frame socket and write a 100-line human renderer. This is the real architectural step; everything beyond is refinement.
- **Defer Mechanism D until there are ≥2 agents and a real conflict to resolve.** Building typed intents before you've felt the pain of stomped keystrokes is premature; building them after is obvious.
