.PHONY: build test clean install lint fmt macos

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

# Build for all platforms
build-all: build-linux build-darwin build-windows

build-linux:
	@mkdir -p $(BUILD_DIR)
	GOOS=linux GOARCH=amd64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-linux-amd64 $(MAIN_PATH)
	GOOS=linux GOARCH=arm64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-linux-arm64 $(MAIN_PATH)

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
	@id="$(CODESIGN_IDENTITY)"; [ -n "$$id" ] || id=$$(security find-identity -v -p codesigning | awk 'NR==1{print $$2}'); \
	[ -n "$$id" ] || { echo "no code-signing identity in the keychain (set CODESIGN_IDENTITY)"; exit 1; }; \
	codesign --force --options runtime --sign "$$id" $(BUILD_DIR)/$(BINARY_NAME) || \
	{ echo "codesign failed. Over SSH the login keychain is locked: run 'security unlock-keychain' first, or build in a terminal on the Mac."; exit 1; }
	codesign --verify --strict $(BUILD_DIR)/$(BINARY_NAME)
	@codesign -dv $(BUILD_DIR)/$(BINARY_NAME) 2>&1 | grep -E '^(Identifier|TeamIdentifier|Info.plist)'
	@echo "Built and signed $(BUILD_DIR)/$(BINARY_NAME)"

build-windows:
	@mkdir -p $(BUILD_DIR)
	GOOS=windows GOARCH=amd64 go build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/$(BINARY_NAME)-windows-amd64.exe $(MAIN_PATH)

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
