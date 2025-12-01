# gomuxai

CLI tool for orchestrating AI coding agents in tmux sessions. Agents can use the same binary to spawn and communicate with other agents.

`tmux` is the only required external dependency from the binary - there is no setup needed. You should be able to copy the manually built or released binaries as you'd like.

## Install

```bash
make build    # builds ./agent
make install  # installs to GOPATH/bin
```

## Usage

```bash
# Spawn an agent
agent spawn researcher --backend claude

# Send a message
agent send researcher "analyze the auth module"

# Queue a message (delivered when idle)
agent queue researcher "summarize when done"

# Check status and capture output
agent status researcher
agent capture researcher --lines 100

# Clean up
agent cleanup researcher
```

## Backends

Built-in: `claude`, `aider`

Custom backends via JSON config:
```bash
agent spawn helper --backend-config my-backend.json
```

## Data

All state lives in `.gomuxai/` next to the binary. Delete the folder to reset.
