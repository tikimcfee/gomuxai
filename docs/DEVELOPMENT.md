# Developing gomuxai

Operational notes for people and coding agents changing `gomuxai` itself. For
how to coordinate agents with the tool, read [`CLAUDE.md`](../CLAUDE.md); for
internal design, read [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) and
[`docs/FLEET.md`](FLEET.md).

## Build environment

`gomuxai` builds two binaries that live side by side: the Rust CLI `agent`
(`rs/`) and the Go bus server `agent-busd` (`busd/`). `legacy/` is the Go `agent`
the Rust one replaced, kept only for the Go/Rust compatibility tests and the one
host still on the Go build ([`legacy/README.md`](../legacy/README.md)); `make
legacy` builds and vets it, and nothing else depends on it.

Keep build artifacts and scratch files on disk, never in `/tmp` (which is
`tmpfs`/RAM on Linux hosts):

```bash
export CARGO_BUILD_JOBS=12
export TMPDIR=/var/tmp
export CARGO_TARGET_DIR=/var/tmp/agent-rs-target
```

`Makefile` sets these defaults automatically when invoked via `make`.

## Pre-commit verification

Run the full check suite before committing:

```bash
make build                                                 # bin/agent and bin/agent-busd
make lint                                                  # cargo clippy -D warnings + go vet in busd/
cd rs && cargo fmt --all --check                           # must print nothing (make fmt formats)
make test                                                  # cargo test --workspace + go test in busd/
```

When working on a single crate inside `rs/`:

```bash
cd rs
cargo build -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
```

- Every CLI command's `short`, task group, and `example` lines are enforced by
  `help_layout` and `help_examples_parse` (`rs/crates/agent/src/cli/commands_test.rs`).
  When adding or changing flags or arguments, update the command's `example` in
  the same commit (and update `rs/crates/agent/testdata/` if help or usage
  fixtures in `rs/crates/agent/tests/help_fixtures.rs` apply).
- Errors should state what failed, what was found, and what to try next; action
  commands must never succeed silently with empty stdout. Wrap causes with
  `anyhow::Context` (`.context("...")`) and print errors with `{:#}` for
  single-line causal chains; use `agent_base::gostd` (`filepath`, `os`,
  `strconv`, `time`) for path cleaning, OS error wording, `%q` quoting, and
  duration formatting.
- Keep code plain and blocking (OS threads where concurrency is needed). Only
  `agent-bus` runs `tokio` internally behind synchronous functions.
- Update [`CLAUDE.md`](../CLAUDE.md), [`README.md`](../README.md),
  [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) (including "Known gaps"),
  [`docs/FLEET.md`](FLEET.md), [`docs/EVENTS.md`](EVENTS.md), and the shared
  skill and spawned-agent briefing (`rs/crates/agent-service/src/service/instructions.rs`)
  whenever behavior they describe changes.

## Local builds, stores, and test isolation

- `make build` writes `./bin/agent` and `./bin/agent-busd`. Because data lives
  beside the executable, `./bin/agent` uses `./bin/.gomuxai/` and does not touch
  `~/.local/bin/.gomuxai/`.
- By default, every `agent` binary on a host talks to the `agents` namespace of
  `g3d-termd`, so terminal names (`gomux-bus` and `agent-<name>`) are shared.
  Never run `agent bus start`, `bus stop`, or `bus run` against the default
  namespace from a scratch build if a real bus is running; pass
  `--termd-ns <scratch>` (or `GOMUXAI_TERMD_NS=<scratch>`) when testing manual
  sessions or a scratch bus.
- **Tests never touch the live system.** Automated tests must never touch the
  live `g3d-termd` socket directory, the running bus (ports `7420`/`7422`,
  session `gomux-bus`), `~/.local/bin/.gomuxai`, `~/.claude`, or `~/.gemini`.
  Always use temp dirs under `/var/tmp` (never `/tmp`), a temp `HOME`, a
  private `g3d-termd` namespace and socket dir, and ephemeral loopback ports.
- **`termd` in tests.** Every connection `agent-termd` makes checks at run time
  whether it is inside a Cargo test binary (by executable path:
  `<profile>/deps/<crate>-<hash>`, `agent_termd::paths::is_cargo_test_exe`). In
  one, it refuses unless a private `0700` socket directory was set up, and
  refuses any socket outside it, whatever crate the test lives in. To use
  `g3d-termd` in a test, add
  `agent-termd = { workspace = true, features = ["testing"] }` under
  `[dev-dependencies]`, then hold `agent_termd::testing::require()` (real
  `g3d-termd`, shared private dir per test binary) or `init()` for the whole
  test, or `let srv = agent_termd::testing::setup();` for a private dir of that
  test's own. Change environment variables only through `testing::EnvGuard` (or
  `TestEnv::setup_cli_private()` in `rs/crates/agent/src/cli/testenv.rs`). Child
  `agent` processes started by a test inherit `GOMUXAI_TEST_TERMD_DIR` and
  `G3D_TERMD_DIR`. Tests that only need the `Ops` trait use a fake and no
  daemon. Mark doc-tests that touch `termd` `no_run`.

- Three `agent run` tests make the kernel OOM-kill a tiny job inside its own
  memory-limited scope, to prove the limit kills the job and not the runner.
  Each kill is real: systemd logs it and a desktop shows a memory warning. So
  they run only when asked: `AGENT_TEST_OOM=1 cargo test -p agent-fleet -p agent`.
  Run them when you change `agent-fleet/src/run/`.
- A scratch `g3d-termd` you start by hand for a probe (your own `G3D_TERMD_DIR`)
  keeps running after you delete its directory. Stop it first
  (`G3D_TERMD_DIR=<dir> g3d --ns <ns> stop`); `pgrep -a g3d-termd` shows any left.

## Installing over a running fleet

- `make install` locates the `agent` already on `PATH` through any symlink (so
  `.gomuxai/` beside the real file stays the store; default `~/.local/bin`,
  override with `BINDIR=...`), builds if needed on Linux, and installs **both**
  `agent-busd` and `agent` by copying each to `<BINDIR>/<bin>.new` and
  atomically renaming (`mv -f`) over `<BINDIR>/<bin>`. Never `cp` directly over
  a live binary: Claude Code invokes `agent hook` on every hook event, and
  atomic rename avoids `ETXTBSY` or executing a half-written file.
- On macOS, run `make macos` first (in a shell inside the Mac's Aqua GUI login
  session so the login keychain is unlocked; see
  [`tools/macsign/README.md`](../tools/macsign/README.md)), then `make install`.
  `make macos` embeds `packaging/macos/Info.plist`
  (`io.github.tikimcfee.gomuxai`) into both `bin/agent` and `bin/agent-busd`
  and signs both with an Apple-issued identity (`CODESIGN_IDENTITY`), which
  `agent-busd` needs to hold Local Network permission inside `g3d-termd`.
- For static Linux `arm64` builds (`aarch64-unknown-linux-musl` + `GOARCH=arm64`),
  run `make arm64`, which writes `bin/linux-arm64/agent` and
  `bin/linux-arm64/agent-busd` with their SHA-256 digests (requires
  `cargo-zigbuild` and the `aarch64-unknown-linux-musl` target).
- After installing, restart the bus (`agent bus restart`, which reuses the
  `gomux-bus` session via `respawn_pane` so a running bus switches to the new
  `agent` and `agent-busd` binaries without losing its GUI security session).
  Running agents keep the `launch.sh` written when they were spawned; if
  spawn-time launch flags changed, respawn, `agent resume`, or `agent revive`
  them to pick up the new launch command, and run `agent skill install` if the
  skill text or hook registration changed.
- External displays consume `agent export --json --follow`,
  `agent transcript <session> --follow`, `agent fleet --json`, and
  `<store>/logs/<name>.log`. Keep their wire shapes (`docs/ARCHITECTURE.md`
  "Export", `docs/FLEET.md`, `docs/EVENTS.md`) and `Config::log_path` stable, or
  coordinate breaking changes across consumers.

## Parallel worktree workers

When splitting `gomuxai` work across multiple coding agents in parallel:

1. Create one git worktree per task (`git worktree add -b work/<slice> ../gomuxai-<slice> main`),
   give each worker its own target directory on disk
   (`CARGO_TARGET_DIR=/var/tmp/agent-rs-target-<slice>`) so parallel `cargo`
   builds do not wait on one lock, and assign disjoint crate or file ownership
   where practical. Common merge hotspots are `CLAUDE.md`,
   `docs/ARCHITECTURE.md`, `rs/Cargo.toml`, `rs/crates/agent/src/cli/mod.rs`,
   `rs/crates/agent-service/src/service/status.rs`, and
   `rs/crates/agent-store/src/sqlite.rs`.
2. Drop a `.claude/settings.local.json` in each worker worktree so workers can
   build, lint, and test without touching the live bus, killing shared
   sessions, or pushing/rebasing shared branches:

   ```json
   {
     "permissions": {
       "allow": [
         "Bash(cargo build:*)", "Bash(cargo check:*)", "Bash(cargo test:*)",
         "Bash(cargo clippy:*)", "Bash(cargo fmt:*)",
         "Bash(go build:*)", "Bash(go vet:*)", "Bash(go test:*)",
         "Bash(make build)", "Bash(make lint)", "Bash(make test)", "Bash(make fmt)",
         "Bash(git status:*)", "Bash(git diff:*)", "Bash(git log:*)", "Bash(git show:*)",
         "Bash(git add:*)", "Bash(git commit:*)",
         "Bash(./bin/agent help:*)", "Bash(./bin/agent version:*)"
       ],
       "deny": [
         "Bash(./bin/agent bus:*)", "Bash(bin/agent bus:*)", "Bash(agent bus:*)",
         "Bash(./bin/agent session kill:*)", "Bash(agent session kill:*)",
         "Bash(g3d kill:*)", "Bash(g3d stop:*)",
         "Bash(git push:*)", "Bash(git merge:*)", "Bash(git rebase:*)", "Bash(git reset --hard:*)"
       ]
     }
   }
   ```

3. Keep `.claude/` and task scratch files uncommitted. Have a single coordinator
   rebase each worker branch onto `main`, re-run `make lint && make test`
   (trait or struct signatures across crates can shift even on a clean rebase),
   and merge with `--ff-only`.

## Public repository hygiene

`gomuxai` is a public repository:

- Never commit real hostnames (use generic examples like `hub`, `laptop`,
  `mac`), usernames, home paths (use `~` or `/home/user`), private LAN IPs
  (use RFC 5737 `192.0.2.x` or `<hub-address>`), email addresses, or Apple
  signing identities / Team IDs in code, tests, docs, or commit messages.
- Never copy real user or agent transcripts into the repo. Test fixtures under
  `rs/crates/agent-transcript/testdata/` must be synthesized from observed
  record shapes with made-up text.
- Before every `git push`, scan the outgoing commit range
  (`git log -p origin/main..HEAD`) for personal hostnames, usernames, home
  paths, and private IPs, and push as a separate step only after a clean scan.
