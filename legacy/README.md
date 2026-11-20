# legacy: the Go `agent`

This is the Go implementation of `agent` that the Rust tool (`rs/`) replaced in
2026-11. It is a self-contained Go module (`github.com/tikimcfee/gomuxai`, the
same path as before, so its imports are unchanged): `make legacy` at the repo
root, or `go build ./... && go vet ./...` here, builds and vets it.

It is kept for two reasons only:

- The Rust tests that prove a Go host and a Rust host still work together
  build against it:
  - `rs/crates/agent-bus/tests/natsbus/fleet.rs`,
    `test_mixed_go_and_rust_fleet_both_directions`: a Go bus host
    (`internal/natsbus`) and a Rust one link both ways, hub and leaf.
  - `rs/crates/agent-bus/tests/natsbus/identity.rs`,
    `test_identity_rust_and_go_bidirectional_compat`: bus identities written
    by either side load, rename and link on the other.
  - `rs/crates/agent-store/tests/compat_test.rs`,
    `go_and_rust_share_a_database` (with
    `rs/crates/agent-store/testdata/gocompat/main.go`): a store either side
    writes reads the same from both (`internal/storage`).
  - `rs/crates/agent-export/src/export_test.rs`,
    `test_go_byte_for_byte_parity`: `agent export` JSON is byte for byte the
    Go's (`internal/export`).
  - `rs/crates/agent-allowcmd/tests/differential.rs` (ignored by default; run
    with `-- --ignored`): the Rust allow-hook checker allows nothing the Go's
    (`internal/allowcmd`) refuses.

  Each of them skips, saying so, when this directory is gone.
- One host of the fleet (the phone image) still runs the Go build.

Nothing else uses it. Delete this directory once no host runs the Go build and
those tests are retired.
