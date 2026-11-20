.PHONY: all build test install macos arm64 clean lint fmt

# `agent` is two programs: the Rust CLI (rs/, binary `agent`) and the bus server it starts
# (busd/, Go, binary `agent-busd`: the embedded nats-server). They are built, installed and
# signed together, and live side by side: `agent` looks for `agent-busd` beside itself.

BUILD_DIR = bin
# Build output goes on disk, never in /tmp (RAM on some hosts).
export CARGO_TARGET_DIR ?= $(if $(wildcard /var/tmp),/var/tmp/agent-rs-target,$(CURDIR)/rs/target)
export CARGO_BUILD_JOBS ?= 12
CARGO = cargo
GOFLAGS = -trimpath
LDFLAGS = -s -w

all: build

build:
	@mkdir -p $(BUILD_DIR)
	cd rs && $(CARGO) build --release -p agent
	cd busd && go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(CURDIR)/$(BUILD_DIR)/agent-busd .
	cp $(CARGO_TARGET_DIR)/release/agent $(BUILD_DIR)/agent
	@echo "Built $(BUILD_DIR)/agent and $(BUILD_DIR)/agent-busd"

test:
	cd rs && $(CARGO) test --workspace
	cd busd && go test ./...

lint:
	cd rs && $(CARGO) clippy --workspace --all-targets -- -D warnings
	cd busd && go vet ./...

fmt:
	cd rs && $(CARGO) fmt --all

# Install over the agent this machine runs: the one on PATH, through any symlink to the real
# file (its store, .gomuxai/, lives beside the real file, so a second install location would
# also be a second, empty store), else ~/.local/bin. BINDIR overrides. Each binary is copied
# beside its target, then renamed over it: the hooks run `agent` on every event, and a rename
# never shows them a half-written file. On a Mac, run `make macos` first (signing).
BINDIR ?= $(patsubst %/,%,$(or $(dir $(realpath $(shell command -v agent 2>/dev/null))),$(HOME)/.local/bin/))
install:
	@[ "$$(uname)" != Darwin ] || [ -x $(BUILD_DIR)/agent ] || { echo "on a Mac, run make macos first: it builds and signs $(BUILD_DIR)/agent and $(BUILD_DIR)/agent-busd"; exit 1; }
	@[ "$$(uname)" = Darwin ] || $(MAKE) --no-print-directory build
	@mkdir -p $(BINDIR)
	@for b in agent-busd agent; do \
		cp $(BUILD_DIR)/$$b $(BINDIR)/$$b.new && mv -f $(BINDIR)/$$b.new $(BINDIR)/$$b || exit 1; \
	done
	@echo "Installed $(BINDIR)/agent and $(BINDIR)/agent-busd (the store: $(BINDIR)/.gomuxai)"
	@echo "Then: agent bus restart (a running bus keeps the old binaries until then)"

# macOS build that can hold its own Local Network permission, so the bus can link to peers on
# the LAN from inside a background g3d-termd session. The process that dials the LAN is
# agent-busd, so it is the one that needs the embedded Info.plist (__TEXT,__info_plist) and an
# Apple-issued signature (ad hoc signatures don't keep the grant); `agent` gets both too.
# macOS keys the grant on the bundle identifier, which both share. Run it on a Mac, in a shell
# of the GUI login session (tools/macsign/README.md). CODESIGN_IDENTITY defaults to the first
# valid identity in the keychain.
MACOS_INFO_PLIST = $(CURDIR)/packaging/macos/Info.plist
MACOS_IDENTIFIER = io.github.tikimcfee.gomuxai
CODESIGN_IDENTITY ?=

macos:
	@[ "$$(uname)" = Darwin ] || { echo "make macos runs on a Mac (it needs the system linker and codesign)"; exit 1; }
	@mkdir -p $(BUILD_DIR)
	cd rs && $(CARGO) rustc --release -p agent --bin agent -- -C link-arg=-Wl,-sectcreate,__TEXT,__info_plist,$(MACOS_INFO_PLIST)
	cp $(CARGO_TARGET_DIR)/release/agent $(BUILD_DIR)/agent
	cd busd && CGO_ENABLED=1 go build $(GOFLAGS) -ldflags "$(LDFLAGS) -linkmode=external -extldflags=-Wl,-sectcreate,__TEXT,__info_plist,$(MACOS_INFO_PLIST)" -o $(CURDIR)/$(BUILD_DIR)/agent-busd .
	@# Only numbered lines are identities: with none valid, find-identity -v prints
	@# "0 valid identities found". Without -v it also lists invalid ones, with the reason
	@# (an expired certificate: CSSMERR_TP_CERT_EXPIRED).
	@id="$(CODESIGN_IDENTITY)"; \
	[ -n "$$id" ] || id=$$(security find-identity -v -p codesigning | awk '/^ *[0-9]+\)/{print $$2; exit}'); \
	if [ -z "$$id" ]; then \
		invalid=$$(security find-identity -p codesigning | grep -E '^ *[0-9]+\)' | sed -E 's/^ *[0-9]+\) [0-9A-F]+ //'); \
		if echo "$$invalid" | grep -q CERT_EXPIRED; then \
			echo "make macos: the code-signing certificate has expired:"; echo "$$invalid" | sed 's/^/  /'; \
			echo "Renew it in Xcode (Settings > Accounts > your team > Manage Certificates > + > Apple Development), then run make macos again."; \
		elif [ -n "$$invalid" ]; then \
			echo "make macos: no valid code-signing identity; the keychain has these, which aren't:"; echo "$$invalid" | sed 's/^/  /'; \
			echo "Fix or replace the certificate (Xcode > Settings > Accounts > Manage Certificates), or set CODESIGN_IDENTITY."; \
		else \
			echo "make macos: no code-signing identity in the keychain: create an Apple Development certificate in Xcode (Settings > Accounts), or set CODESIGN_IDENTITY."; \
		fi; \
		exit 1; \
	fi; \
	for b in agent agent-busd; do \
		codesign --force --options runtime --identifier $(MACOS_IDENTIFIER) --sign "$$id" $(BUILD_DIR)/$$b || { \
			echo "make macos: codesign failed on $$b with identity $$id (its error is above)."; \
			session=$$(launchctl managername 2>/dev/null); \
			if [ "$$session" != Aqua ]; then \
				echo "This shell isn't in the Mac's GUI login session (launchctl managername: $$session), and only there is the login keychain unlocked."; \
				echo "Over ssh, or in a daemon started over ssh, codesign can't use the key (errSecInternalComponent). Build in a terminal on the Mac,"; \
				echo "or in a g3d-termd daemon started from one or by a LaunchAgent (tools/macsign/README.md)."; \
			fi; \
			exit 1; }; \
		codesign --verify --strict $(BUILD_DIR)/$$b || exit 1; \
		codesign -dv $(BUILD_DIR)/$$b 2>&1 | grep -E '^(Identifier|TeamIdentifier|Info.plist)'; \
	done
	@echo "Built and signed $(BUILD_DIR)/agent and $(BUILD_DIR)/agent-busd"

# Static arm64 Linux binaries (the phone image). Needs cargo-zigbuild
# (uv tool install cargo-zigbuild) and the aarch64-unknown-linux-musl target.
ARM64_DIR = $(BUILD_DIR)/linux-arm64
# uv puts zig inside the tool's own environment, where cargo-zigbuild doesn't look.
export CARGO_ZIGBUILD_ZIG_PATH ?= $(shell command -v zig 2>/dev/null || find $(HOME)/.local/share/uv/tools/cargo-zigbuild -type f -name zig 2>/dev/null | head -1)
arm64:
	@mkdir -p $(ARM64_DIR)
	cd rs && $(CARGO) zigbuild --release -p agent --target aarch64-unknown-linux-musl
	cp $(CARGO_TARGET_DIR)/aarch64-unknown-linux-musl/release/agent $(ARM64_DIR)/agent
	cd busd && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(CURDIR)/$(ARM64_DIR)/agent-busd .
	@file $(ARM64_DIR)/agent $(ARM64_DIR)/agent-busd
	@cd $(ARM64_DIR) && sha256sum agent agent-busd

clean:
	rm -rf $(BUILD_DIR)
