# macOS Code Signing Over SSH (`tools/macsign`)

`make macos` embeds `cmd/agent/Info.plist` and signs `bin/agent` with an Apple
code-signing identity so `agent bus run` can hold macOS's Local Network
permission inside a background `g3d-termd` session (Apple TN3179; see
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#macos-local-network)).

When builds are triggered over SSH, `codesign` succeeds in some sessions and
fails with `errSecInternalComponent` in others even while a user is logged in
at the unlocked Mac screen. This directory provides a diagnostic probe and
LaunchAgent recipe for unattended builds over SSH.

## Why daemon origin decides whether `codesign` works

- macOS tracks whether `login.keychain-db` is unlocked **per security (audit)
  session**. Logging in at the screen unlocks it in the `Aqua` (GUI) session
  only; each SSH connection (interactive or non-interactive, with or without
  `ssh -tt` or `ssh-add --apple-use-keychain`) runs in a separate `Background`
  session where the login keychain starts locked.
- Every terminal in `g3d-termd` is forked by the `g3d-termd` **daemon**, so
  shells inside `g3d-termd` inherit the security session of whatever started
  the daemon, not of the client that asked for the terminal.
- If `g3d-termd --ns agents` was started from a GUI terminal app or an `Aqua`
  LaunchAgent, `make macos` inside any `agents` terminal signs cleanly—even
  when triggered over SSH.
- If `g3d-termd --ns agents` exited (for example, after `--idle-exit`) and was
  later auto-started over SSH, its `launchctl managername` is `Background`.
  `security find-identity -v -p codesigning` still lists valid certificates,
  but `SecKeyCreateSignature` fails inside `securityd` with
  `CSSMERR_CSP_NO_USER_INTERACTION` (`-25308`) because no GUI prompt can be
  shown, and `codesign` exits with `errSecInternalComponent`.
- **Local Network permission does not depend on daemon origin.** Once
  `bin/agent` is signed and installed, `agent bus run` works from either an
  `Aqua` or `Background` daemon because `responsibility_spawnattrs_setdisclaim`
  makes the signed binary its own responsible process.

## Diagnose the current shell

Run the read-only probe in the shell or `g3d-termd` terminal where you plan to
build:

```bash
sh tools/macsign/check-signing-session.sh
```

It prints `launchctl managername` (`Aqua` vs `Background`), checks whether the
default keychain allows non-interactive use in this session, checks for expired
or missing signing identities (`CSSMERR_TP_CERT_EXPIRED`), and test-signs a
temporary copy of `/usr/bin/true`.

## Fix 1 (Recommended): `Aqua` LaunchAgent for `g3d-termd`

Keep the `agents` namespace daemon under `launchd` in the `gui/<uid>` (`Aqua`)
domain so it starts automatically on GUI login and never exits when idle:

1. Stop any SSH-started `agents` daemon when convenient (stopping the daemon
   ends sessions in that namespace; use `agent resume --all` afterwards):

   ```bash
   g3d --ns agents stop
   ```

2. Render [`com.gomuxai.termd-agents.plist`](com.gomuxai.termd-agents.plist)
   with your home path (`launchd` plists do not expand `~` or `$HOME`) and
   bootstrap it into the GUI domain:

   ```bash
   mkdir -p ~/Library/LaunchAgents
   sed "s|@HOME@|$HOME|g" tools/macsign/com.gomuxai.termd-agents.plist \
     > ~/Library/LaunchAgents/com.gomuxai.termd-agents.plist
   launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.gomuxai.termd-agents.plist
   ```

3. Verify the job and signing session:

   ```bash
   launchctl print "gui/$(id -u)/com.gomuxai.termd-agents"
   g3d --ns agents create --id macsign-check -- sh -lc 'launchctl managername'
   ```

Why the plist uses `--idle-exit 0` without `--daemon`: `launchd` supervises a
foreground process directly (`KeepAlive` restarts it if it crashes; `--daemon`
would fork and exit immediately, causing `launchd` to respawn in a loop), and
`--idle-exit 0` prevents the default 300-second empty-namespace exit so SSH
commands never race to start a `Background` daemon. `agent bus restart` also
reuses its existing terminal via `RespawnPane` rather than killing the last
session.

**Security trade-off:** any process running under your macOS UID (including
SSH sessions using an authorized key) can open a terminal in `agents` and use
items in the unlocked login keychain whose access control list already permits
the caller (such as `codesign`), matching the exposure of an open terminal tab
on the Mac.

## Fix 2 (Headless / CI Macs): Dedicated build keychain

When no user logs in at the Mac's screen after reboot, put **only** the Apple
Development signing identity into a dedicated keychain unlocked per build:

1. Export the Apple Development identity (`.p12`) from Keychain Access or
   Xcode, then create an isolated keychain with a random password stored in a
   `0600` file (never store your macOS login password in a file):

   ```bash
   PassFile=~/.config/gomuxai/buildkc-pass
   Keychain=~/Library/Keychains/gomux-build.keychain-db
   security create-keychain -p "$(cat "$PassFile")" "$Keychain"
   security set-keychain-settings "$Keychain"   # disable timeout/sleep auto-lock
   security list-keychains -d user -s $(security list-keychains -d user | tr -d '"') "$Keychain"
   security import cert.p12 -k "$Keychain" -P "<p12-password>" -T /usr/bin/codesign -T /usr/bin/security
   security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$(cat "$PassFile")" "$Keychain"
   ```

2. Unlock `gomux-build.keychain-db` in the build shell right before `make macos`:

   ```bash
   security unlock-keychain -p "$(cat ~/.config/gomuxai/buildkc-pass)" \
     ~/Library/Keychains/gomux-build.keychain-db
   make macos
   ```

**Trade-offs:** works from any SSH session without GUI login and keeps the rest
of `login.keychain-db` locked, at the cost of protecting the signing key with a
file password under your UID and re-importing the `.p12` when the certificate
renews annually.

## Summary of alternatives

| Approach | Needs GUI login after reboot | Per-build manual step | Notes |
|---|---|---|---|
| `Aqua` LaunchAgent (`com.gomuxai.termd-agents.plist`) | Yes (once per reboot) | None | Recommended when a user logs into the Mac desktop |
| Dedicated `gomux-build.keychain-db` | No | None | Recommended for headless Macs; re-import on cert renewal |
| GUI-domain one-shot signing LaunchAgent (`launchctl kickstart -k gui/<uid>/...`) | Yes | None | Narrower exposure (signs one fixed path), requires custom build hook |
| `security unlock-keychain` in SSH session | No | Type login password | Fine for a one-off build; unlock lasts only for that SSH session |
| `security set-key-partition-list` alone on `login.keychain-db` | — | — | **Does not fix SSH signing:** partition lists apply only after the keychain is unlocked in that session |
| `launchctl bsexec` / `sudo launchctl asuser` | — | — | **Not recommended:** `bsexec` loses the user keychain search list; passwordless `sudo launchctl` is equivalent to root |
