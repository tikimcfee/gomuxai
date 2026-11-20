# Developing gomuxai

Operational notes for people and coding agents changing `gomuxai` itself. For
how to coordinate agents with the tool, read [`CLAUDE.md`](../CLAUDE.md); for
internal design, read [`docs/ARCHITECTURE.md`](ARCHITECTURE.md).

## Pre-commit verification

Run the full check suite before committing:

```bash
go build ./...
go vet ./...
GOOS=darwin go vet ./internal/cli/
gofmt -l .                 # must print nothing
go test ./...
```

- Every CLI command's `Short`, task group, and `Example` lines are enforced by
  `TestHelpLayout` and `TestHelpExamplesParse` (`internal/cli/commands_test.go`).
  When adding or changing flags or arguments, update the command's `Example`
  in the same commit.
- Errors should state what failed, what was found, and what to try next; action
  commands must never succeed silently with empty stdout.
- Update [`CLAUDE.md`](../CLAUDE.md), [`README.md`](../README.md),
  [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) (including "Known gaps"),
  [`docs/EVENTS.md`](EVENTS.md), and the spawned-agent briefing
  (`internal/service/instructions.go`) whenever behavior they describe changes.

## Local builds, stores, and `g3d-termd` namespaces

- `make build` writes `./bin/agent`. Because data lives beside the executable,
  `./bin/agent` uses `./bin/.gomuxai/` and does not touch `~/.local/bin/.gomuxai/`.
- By default, every `agent` binary on a host talks to the `agents` namespace of
  `g3d-termd`, so terminal names (`gomux-bus` and `agent-<name>`) are shared.
  Never run `agent bus start`, `bus stop`, or `bus run` against the default
  namespace from a scratch build if a real bus is running; pass
  `--termd-ns <scratch>` (or `GOMUXAI_TERMD_NS=<scratch>`) when testing manual
  sessions or a scratch bus.
- Automated tests never touch the live `g3d-termd` socket directory: every
  package that talks to `g3d-termd` wires `termdtest.Run` in `TestMain` to use a
  private `0700` socket directory and enforces it via `termd.CheckTestServer`.

## Installing over a running fleet

- `make install` copies `bin/agent` to `<BINDIR>/agent.new` and atomically
  renames it over `<BINDIR>/agent`. Prefer `make install` (or `install -m 755`)
  over `cp`: Claude Code invokes `agent hook` on every hook event, and atomic
  rename avoids `ETXTBSY` or executing a half-written binary.
- On macOS, run `make macos` before `make install` so the installed binary
  keeps its embedded `Info.plist` and Apple code signature (see
  [`tools/macsign/README.md`](../tools/macsign/README.md)).
- After installing, restart the bus (`agent bus restart`, which reuses the
  `gomux-bus` session via `RespawnPane`). Running agents keep the `launch.sh`
  written when they were spawned; if spawn-time launch flags changed, respawn
  or `agent resume` them to pick up the new launch command.
- External displays consume `agent export --json --follow`,
  `agent transcript <session> --follow`, and `<store>/logs/<name>.log`. Keep
  their wire shapes (`docs/ARCHITECTURE.md` "Export", `docs/EVENTS.md`) and
  `config.LogPath` stable, or coordinate breaking changes across consumers.

## Parallel worktree workers

When splitting `gomuxai` work across multiple coding agents in parallel:

1. Create one git worktree per task (`git worktree add -b work/<slice> ../gomuxai-<slice> main`)
   and assign disjoint file ownership where practical. Common merge hotspots are
   `CLAUDE.md`, `docs/ARCHITECTURE.md`, `internal/service/status.go`, and
   `internal/storage/sqlite.go`.
2. Drop a `.claude/settings.local.json` in each worker worktree so workers can
   build, test, and inspect status without touching the live bus, killing
   shared sessions, or pushing/rebasing shared branches:

   ```json
   {
     "permissions": {
       "allow": [
         "Bash(go build:*)", "Bash(go vet:*)", "Bash(go test:*)", "Bash(gofmt:*)",
         "Bash(GOOS=darwin go vet:*)", "Bash(make build)",
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
   rebase each worker branch onto `main`, re-run `go build ./... && go test ./...`
   (signatures across packages can shift even on a clean rebase), and merge
   with `--ff-only`.

## Public repository hygiene

`gomuxai` is a public repository:

- Never commit real hostnames (use generic examples like `hub`, `laptop`,
  `mac`), usernames, home paths (use `~` or `/home/user`), private LAN IPs
  (use RFC 5737 `192.0.2.x` or `<hub-address>`), email addresses, or Apple
  signing identities / Team IDs in code, tests, docs, or commit messages.
- Never copy real user or agent transcripts into the repo. Test fixtures under
  `internal/transcript/testdata/` must be synthesized from observed record
  shapes with made-up text.
- Before every `git push`, scan the outgoing commit range (`git log -p origin/main..HEAD`)
  for personal hostnames, usernames, home paths, and private IPs, and push as a
  separate step only after a clean scan.
