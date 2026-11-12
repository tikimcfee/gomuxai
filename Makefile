.PHONY: all build build-all build-linux build-darwin macos test test-coverage clean install lint fmt tidy dev run

# Build variables
BINARY_NAME=agent
BUILD_DIR=bin
MAIN_PATH=./cmd/agent

# Go variables
GOFLAGS=-trimpath
LDFLAGS=-s -w

# Default target
all: build

# Build the binary
build:
	@mkdir -p $(BUILD_DIR)
	go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME) $(MAIN_PATH)
	@echo "Built $(BUILD_DIR)/$(BINARY_NAME)"

# Cross-compile release binaries. There is no Windows target: agents run in
# tmux, which Windows doesn't have (use the Linux build under WSL).
build-all: build-linux build-darwin

build-linux:
	@mkdir -p $(BUILD_DIR)
	GOOS=linux GOARCH=amd64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-linux-amd64 $(MAIN_PATH)
	GOOS=linux GOARCH=arm64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-linux-arm64 $(MAIN_PATH)

# Plain macOS binaries, without the embedded Info.plist and signature: their
# bus can't link to peers over the LAN from inside tmux. Use `make macos` on
# the Mac for that.
build-darwin:
	@mkdir -p $(BUILD_DIR)
	GOOS=darwin GOARCH=amd64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-darwin-amd64 $(MAIN_PATH)
	GOOS=darwin GOARCH=arm64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-darwin-arm64 $(MAIN_PATH)

# macOS build that can hold its own Local Network permission, so the bus can
# link to peers on the LAN from inside tmux (see internal/macos). Run it on a
# Mac: cgo and the system linker embed cmd/agent/Info.plist, then codesign
# signs with an Apple-issued identity (ad hoc signatures don't keep the
# grant). CODESIGN_IDENTITY defaults to the first one in the keychain.
MACOS_INFO_PLIST=$(CURDIR)/cmd/agent/Info.plist
CODESIGN_IDENTITY?=

macos:
	@[ "$$(uname)" = Darwin ] || { echo "make macos runs on a Mac (it needs the system linker and codesign)"; exit 1; }
	@mkdir -p $(BUILD_DIR)
	CGO_ENABLED=1 go build $(GOFLAGS) -ldflags "$(LDFLAGS) -linkmode=external -extldflags=-Wl,-sectcreate,__TEXT,__info_plist,$(MACOS_INFO_PLIST)" -o $(BUILD_DIR)/$(BINARY_NAME) $(MAIN_PATH)
	@# Only numbered lines are identities: with none valid, find-identity -v
	@# prints "0 valid identities found". Without -v it also lists invalid
	@# ones, with the reason (an expired certificate: CSSMERR_TP_CERT_EXPIRED).
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
	codesign --force --options runtime --sign "$$id" $(BUILD_DIR)/$(BINARY_NAME) || { \
		echo "make macos: codesign failed with identity $$id (its error is above)."; \
		session=$$(launchctl managername 2>/dev/null); \
		if [ "$$session" != Aqua ]; then \
			echo "This shell isn't in the Mac's GUI login session (launchctl managername: $$session), and only there is the login keychain unlocked."; \
			echo "Over ssh, or in a tmux server that was started over ssh, codesign can't use the key (errSecInternalComponent). Build in a terminal on the Mac,"; \
			echo "or in a tmux server started from one or by a LaunchAgent (docs/ARCHITECTURE.md, macOS), or run security unlock-keychain in this shell first."; \
		fi; \
		exit 1; }
	codesign --verify --strict $(BUILD_DIR)/$(BINARY_NAME)
	@codesign -dv $(BUILD_DIR)/$(BINARY_NAME) 2>&1 | grep -E '^(Identifier|TeamIdentifier|Info.plist)'
	@echo "Built and signed $(BUILD_DIR)/$(BINARY_NAME)"

# Run tests
test:
	go test -v ./...

# Run tests with coverage
test-coverage:
	go test -v -coverprofile=coverage.out ./...
	go tool cover -html=coverage.out -o coverage.html
	@echo "Coverage report: coverage.html"

# Clean build artifacts
clean:
	rm -rf $(BUILD_DIR)
	rm -f coverage.out coverage.html

# Install to GOPATH/bin
install:
	go install $(GOFLAGS) -ldflags "$(LDFLAGS)" $(MAIN_PATH)

# Run linter
lint:
	golangci-lint run ./...

# Format code
fmt:
	go fmt ./...

# Tidy dependencies
tidy:
	go mod tidy

# Development build (no optimizations)
dev:
	@mkdir -p $(BUILD_DIR)
	go build -o $(BUILD_DIR)/$(BINARY_NAME) $(MAIN_PATH)

# Run the binary
run: build
	./$(BUILD_DIR)/$(BINARY_NAME)
