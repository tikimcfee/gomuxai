# Porting `agent` to Rust: the map and the rules

**The port is done (2026-11-20).** The Go this page refers to is under `legacy/`
([`legacy/README.md`](../legacy/README.md)), and the rules that outlive the port are in
[`docs/DEVELOPMENT.md`](DEVELOPMENT.md); the rest of this page is the record.

For everyone working on the rewrite. The Go code
under `internal/` is the behaviour spec until cutover; the Go tests say what must hold.

## Where things go

The workspace is `rs/`. Each crate is one or more Go packages, file for file
(`send.go` becomes `send.rs`), so anyone can find the Rust for a piece of Go.

| Crate | Go packages it replaces |
|---|---|
| `agent-base` | `util`, `await`, `buildinfo`, `backend/types` (as `types`), `config`, `macos` |
| `agent-allowcmd` | `allowcmd` (a strict lexer instead of `mvdan.cc/sh`) |
| `agent-store` | `storage` |
| `agent-termd` | `termd`, `termdwire`, `termdtest` (over the `g3d` and `g3d-wire` crates) |
| `agent-claudereg` | `claudereg` |
| `agent-transcript` | `transcript` |
| `agent-discovery` | `discovery` |
| `agent-backend` | `backend`, `backend/namedagents` |
| `agent-bus` | `natsbus` (client, contract, identity, RPC, events); the server stays Go, in `busd/` |
| `agent-export` | `export` |
| `agent-service` | `service`, `watcher` |
| `agent` | `cli`, `cmd/agent` (the binary) |

A crate depends only on crates above it in this table.

## Things every crate uses

- `agent_base::gostd` has Go standard-library behaviour whose exact output shows up in
  what we print: `filepath` (lexical `clean`, `join`, `rel`, ...), `os` (errors that
  read "open /x: no such file or directory", `read_file`, `mkdir_all`, ...), `strconv`
  (`quote` is Go's `%q`), `time` (`format_duration` is Go's "1h30m0s"). Use it instead
  of writing another.
- Go's `fmt.Errorf("x: %w", err)` is `.context("x")`. Print an error with `{:#}` to get
  the Go's one-line text.
- `agent_base::config` holds the termd namespace rules (`DEFAULT_TERMD_NAMESPACE`,
  `validate_termd_name`, `validate_termd_namespace`).

- **termd in tests.** Every connection `agent-termd` makes checks at run time whether
  it is in a Rust test binary (by where the executable lives: cargo's
  `<profile>/deps/<crate>-<hash>`). In one, it refuses unless a private socket dir was
  set up, and refuses any socket outside it, whatever crate the test is in. To use
  termd in a test: `agent-termd = { workspace = true, features = ["testing"] }` in
  `[dev-dependencies]`, then hold `agent_termd::testing::require()` (real `g3d-termd`,
  shared private dir) or `init()` for the whole test, or `let srv =
  agent_termd::testing::setup();` for a dir of the test's own. Change environment
  variables only through `testing::EnvGuard`. An `agent` binary a test starts inherits
  the private dir (`GOMUXAI_TEST_TERMD_DIR`, `G3D_TERMD_DIR`). Tests that only need the
  `Ops` trait use a fake and no termd. Mark doc tests that touch termd `no_run`.

## Rules

1. **Same behaviour, same words.** Everything a person or agent sees (stdout, stderr
   hints, error messages, help text, exit codes), every file written under `.gomuxai/`,
   the SQLite schema and the values in it, every bus subject and JSON payload, and the
   hook contract with Claude Code and agy stay exactly what the Go writes. A Rust host
   and a Go host share a bus, and a Rust binary opens a store a Go binary wrote. Where
   the Go has a bug, port the behaviour and note the bug in your report; don't fix it
   quietly.
2. **Names.** Go `FooBar` is Rust `foo_bar`; types keep their names. Keep the Go file
   and function layout unless Rust forces otherwise. Keep comments that explain why;
   they carry decisions that cost real incidents to learn.
3. **Errors.** `anyhow::Result` throughout. Where Go code tests for a particular error
   (`errors.Is`, `errors.As`, a sentinel like `ErrNoBus`), make it a `thiserror` type
   and check it with `downcast_ref`. Messages keep the Go text.
4. **Plain blocking code.** Threads where the Go uses goroutines. Only `agent-bus`
   uses tokio, inside, behind blocking functions.
5. **Traits only where the Go has more than one implementation** (test fakes count).
   Otherwise a struct.
6. **JSON** with serde, field names from the Go `json:` tags, `omitempty` as
   `skip_serializing_if`. A Go `time.Duration` field is integer nanoseconds. Times in
   the database and on the wire keep the Go text format exactly.
7. **Dependencies** come from `[workspace.dependencies]` in `rs/Cargo.toml`
   (`foo.workspace = true`). Need another? Add it there and say so in your report.
   For TLS types use `async_nats::rustls`, so there's one rustls.
8. **Tests.** Port the Go tests for your packages (`internal/<pkg>/*_test.go` and
   `tests/<pkg>/`), same names in snake_case, as unit tests or in the crate's
   `tests/`. Copy test data into the crate's `testdata/`. A test you can't port yet
   (it needs a crate that doesn't exist) goes in as `#[ignore = "needs <crate>"]`
   with its body sketched, and in your report.
9. **Tests never touch the live system.** Not the real g3d-termd socket dir, not the
   running bus (ports 7420/7422, session `gomux-bus`), not `~/.local/bin/.gomuxai`,
   not `~/.claude` or `~/.gemini`. Use temp dirs (under `/var/tmp`, never `/tmp`: it is
   RAM here), a temp `HOME`, a private termd namespace and socket dir, random ports.
   The fleet is running real work on this machine while you test.
10. **Builds.** `export CARGO_BUILD_JOBS=12 TMPDIR=/var/tmp
    CARGO_TARGET_DIR=/var/tmp/agent-rs-target-<your role>` (your own target dir, so
    builds don't wait on each other's lock). Build and test your crate only:
    `cargo build -p <crate>`, `cargo test -p <crate>`, `cargo clippy -p <crate>`.
    The Go still builds: `go build ./... && go test ./internal/<pkg>/` is how you check
    what the Go really does.
11. **Stay in your files.** Edit only the crates and paths your task names. If another
    crate lacks something you need, write to the lead (`agent send gomuxai-lead "..."`)
    rather than editing it. Don't `git commit`, `checkout`, `stash` or `reset`: several
    agents share this working tree, and the lead commits. Edit with your file-editing
    tools, not `sed -i` or scripts.
12. **Public repo.** No real host names (use `hub`, `laptop`, `mac`), user names, home
    paths (`~` or `/home/user`), LAN addresses (`192.0.2.x`), emails or signing
    identities in code, tests, docs or test data. Synthesize fixtures; never copy a
    real transcript.
13. **Report** when your task is done: one message to the lead with what's ported, test
    counts (`cargo test -p <crate>` summary line), what's left or ignored and why,
    and any Go behaviour that surprised you. Long reports go in
    `rs/reports/<role>.md`, with a one-line pointer in the message.
