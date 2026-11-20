# Lane: gomuxai (the Rust rewrite of `agent`)

Lead: `gomuxai-lead`. Updated 2026-11-20. The brief this carried out is kept out of
the repository (it names people and machines); its Done list is quoted below.

## State

**Done.** `agent` is the Rust tool (`rs/`) plus the Go bus server it starts
(`busd/`, binary `agent-busd`). All three hosts of the fleet run build `0c2eab5`,
linked on one bus. The work is merged to `main` as one commit (`0c2eab5`); the
branch it was built on, with its history, stays local.

After that, the Go tool and what existed only for a mixed Go/Rust fleet were removed
(see Decisions). That change is not installed anywhere: the hosts' `0c2eab5` builds
work with it unchanged.

Done list (from the brief), each shown in the lead's conversation with its command
and output:

- [x] 1. `cargo test --workspace`: 990 passed, 0 failed, 5 ignored at the merge (985
      and 3 since the Go comparison tests were removed). Every Go test was ported or
      accounted for, and every Parity command is run end to end as a subprocess.
- [x] 2. The Rust `agent` is installed on the hub and on the Mac (signed, with the
      embedded Info.plist, built in the Mac's GUI-session termd daemon). `agent
      version` prints `agent 0.2.0 0c2eab5 ... rustc ...` on both.
- [x] 3. `agent bus status` on the hub: 2 of 2 peers linked. `agent list --host` works
      for both peers.
- [x] 4. spawn → send → wait → `state get _result` returned the reply for a claude and
      for an agy agent on the hub; `agent send <name>@<mac>` reached a Claude agent on
      the Mac, which replied.
- [x] 5. `agent fleet` shows all three hosts. `agent run` ran a script in its own
      memory-limited scope and recorded it. `agent template start --host <hub>`, run
      on the Mac, started the hub's template there. A `focus --follow` on the Mac saw
      the hub's focus set and cleared. `agent revive` brought an ended agy agent back
      on its own conversation, re-briefed from its handoff doc. `agent skill install`
      set up Claude Code and agy (hub and Mac).
- [x] 6. Static arm64 binaries built and, at the owner's word, installed on the phone
      (see Where things are for the path and sha256). The phone runs `0c2eab5` too.
- [x] 7. Friction triaged: the hub's one open entry (#51) is resolved by `agent
      revive`; the Mac's #13 was rechecked, is still true, and stays open with a note;
      the phone's box is empty. `agent fleet --report` printed the baseline.
- [x] 8. `CLAUDE.md`, `README.md`, `docs/ARCHITECTURE.md` (and `DEVELOPMENT.md`,
      `EVENTS.md`, `FLEET.md`) describe the Rust tool.
- [x] 9. Merged to `main` and pushed.

Decisions, and why:

- **The bus server stays Go**, as `agent-busd`: only the embedded nats-server, with
  the configuration the Go tool had. Rust has no embeddable NATS server, and this kept
  subjects, payloads, pins and permissions identical, so hosts were upgraded one at a
  time (a test links the old Go bus to the new one both ways). The Rust `agent bus
  run` starts it as a child and is its client and RPC server.
- **Allowlist without a shell parser**: a strict lexer that approves only one plain
  run of the binary. It is stricter than the Go's, on purpose (see Traps).
- **termd client**: the `g3d` and `g3d-wire` crates from glyph3d-js, as git
  dependencies pinned by commit in `rs/Cargo.toml`.
- **The Go code is gone from the tree.** It was kept in `legacy/` through the
  rollout, for four tests proving a Go host and a Rust host agree (the shared store,
  the mixed bus, bus identities, export output) and an allowlist differential. Once
  all three hosts ran Rust, the owner had it removed with those tests; commit
  `0c2eab5` is the last that has them.
- **No fallback for a peer that doesn't serve `fleet`.** `agent fleet` used to show
  such a host from its `list` reply (`partial: list only`). Now it is `unreachable`,
  with the error, and `FleetHost` has no `partial` field.
- **Help fixtures are the Rust tool's own snapshots** (they were the Go's output plus
  a list of allowed differences). `UPDATE_HELP_FIXTURES=1 cargo test -p agent --test
  help_fixtures` rewrites them; a test fails if a listed command has no fixture.
- **`agent_base::gostd` stays.** It is the format of what is on disk and on the wire,
  not a compatibility layer: replacing it would mean a store migration.
- **Mixed team**: three agy Carbon helpers on the wall (termd, transcript/docs, bus)
  and Opus subagents; every crate was reviewed by the other model, and each review's
  findings were fixed with regression tests. Reports are in `rs/reports/` (local).
- **`agent run` defaults**: `MemoryMax` 85% of RAM, `MemoryHigh` 75%,
  `MemorySwapMax=0` (with swap, a job over `MemoryMax` alone is never killed).

## Next

Nothing blocks. Left over, for whoever picks the lane up:

- **The phone image's pin.** The phone runs the Rust binaries because they were put
  on `/userdata` by hand, which its boot script leaves alone. Its image still pins
  the Go build. The phone's image lane should pin the bundle under
  Where things are when it next builds an image, and add `agent-busd` beside `agent`.
- **glyph3d lead**: tell them `agent` depends on their `g3d` and `g3d-wire` crates by
  git commit (their agent was ended when this was decided), and that `agent-termd`
  carries checks their client lacks (socket dir owner and mode, peer uid, scrubbed
  environment, a connect timeout): candidates to move into `g3d`.
- **oomd and termd**: `agent run` asks systemd to kill g3d-termd's units last
  (`ManagedOOMPreference=avoid`), and says when that isn't heeded. On the hub it
  isn't: oomd watches the root-owned user slice and ignores a user's own preference.
  A root-side change would have to survive termd's unit names changing per start.
  Not pursued: the job's own limit is what protects the terminal.
- **Mac friction #13** (a worktree's `./bin/agent` can't spawn a Claude agent to
  test with) is open. The end-to-end tests show the route (a scratch
  `CLAUDE_CONFIG_DIR` and `agent hooks install --settings`); a one-step way is unbuilt.
- **`agent revive` on macOS** resumes an idle pane or refuses; it can't take over a
  CLI restarted by hand there (it identifies processes through `/proc`).
- **agy titles**: an agy conversation is named after its first turn ends (`/rename`
  needs an active conversation), so `fleet` shows a name mismatch until then.
- **Known differences from the Go** that were left: the lists are in
  `rs/reports/*.md` under "known differences" (error wording from serde, a few shell
  word-splitting oddities, Unicode letter classes in friction's similar-entry check).

## Traps

- **The Go allowlist approved `agent list \` + CR + LF + `<another command>`** as one
  safe command, and the shell ran the second line. The Rust refuses it. Any host put
  back on a Go build (the `agent.go-84b9c94` rollback binary) gets the hole back.
- **Tests never touch the live fleet.** `agent-termd` refuses, at run time, any
  connection from a Rust test binary that isn't inside a private socket dir. Don't
  weaken it; `docs/DEVELOPMENT.md` says how tests get a private termd and bus.
- **Three `agent run` tests cause real kernel OOM kills** (and desktop memory
  warnings), so they run only with `AGENT_TEST_OOM=1`.
- **A scratch `g3d-termd` outlives its directory.** Stop it before deleting the dir.
- `/tmp` is RAM on the hub: build with `CARGO_TARGET_DIR` on disk (the Makefile does).
- **Installing over a running fleet**: `make install` renames both binaries into
  place; then `agent bus restart`. On a Mac, `make macos` first, in a shell of the
  GUI session, and compare the signing team with the installed binary before
  installing (`tools/macsign/README.md`).
- **Saved bus settings win over defaults.** A host upgraded from Go keeps its old
  `--remote-ops` in `bus/settings.json`; `fleet` and `template` must be added once
  (`agent bus restart --remote-ops ...,fleet,template`). All three hosts have them.
- The file-editing tools turn a single-backslash `\\uXXXX` in content into the real
  character. Write the doubled form.

## Where things are

- `rs/`: the Cargo workspace (`rs/crates/agent` is the binary; one crate per area,
  mapped in `docs/ARCHITECTURE.md`). `busd/`: the Go bus server.
  `packaging/macos/Info.plist`: embedded in both macOS binaries.
- `Makefile`: `build`, `test`, `lint`, `install`, `macos`, `arm64`.
- `docs/FLEET.md`: the design of fleet, focus, templates, run, skill, revive.
  `docs/RUST-PORT.md`: the porting rulebook, kept as the record.
- The arm64 bundle for the phone: `bin/arm64-0c2eab5/` in this checkout on
  the hub (`bin/` is not committed), with `SHA256SUMS`:
  - `agent`: `d8f57c16317c1ffd491d1c4e298a90953a6584a9f5634fb1da3fb7d134437236`
  - `agent-busd`: `a73c6ea539e043ef8f4b80715f0fb87dc9019e0e75f8db876c16a15c999346e9`
- Rollback on each host: the Go binary is kept beside the installed one as
  `agent.go-84b9c94`, and the store as it was before the first Rust open is
  `.gomuxai/backups/gomuxai-before-rust-20261120.db`.
- `rs/tasks/`, `rs/reports/` (local, not committed): each helper's task files and
  reports, including the six cross-model reviews.
