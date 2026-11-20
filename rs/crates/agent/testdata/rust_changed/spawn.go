it (agent bus start --remote-ops list,status,capture,send,respond,friction,logs,spawn).
Paths are that host's: --cwd must be absolute there, and without it the
agent starts in the home directory of the user its bus runs as; a task file
is a path there, relative to --cwd. --task is read from this host's tasks
and sent as the first message. --resume, --attach, --skip-permissions,
--trust-workspace and --backend-config only work on the agent's own host:
run spawn there for those (so --cwd must be a directory the CLI already
trusts there, or a person there confirms its trust prompt). An agent
spawned by agent lead on host laptop is recorded on the other host as a
helper of lead@laptop, so it can reply with agent send lead@laptop; the
other host takes that name on trust. cleanup doesn't reach other hosts:
clean the agent up on its host.

Usage:
  agent spawn <name>[@host] [task-file] [flags]

Examples:
  agent spawn researcher --backend claude
  agent spawn researcher --backend claude --task research-auth
  agent spawn builder --backend claude tasks/build.md
  agent spawn helper --backend aider
  agent spawn auto-worker --backend claude --skip-permissions
  agent spawn reviewer --backend agy --trust-workspace
  agent spawn quick --backend claude --model claude-fable-5-1
  agent spawn lead --backend claude --resume 3f34adff --attach
  agent spawn lead --backend claude --continue
  agent spawn builder@hub --backend claude --cwd /srv/repo tasks/build.md
  agent spawn builder --host hub --backend claude --task build

Flags:
      --attach             Open the agent's session in this terminal once it starts
  -c, --continue           Continue the session the CLI's own --continue would pick here (claude, agy): its most recent one in this directory, or --cwd's
      --cwd string         Directory the agent starts in (on another host: an absolute path there)
