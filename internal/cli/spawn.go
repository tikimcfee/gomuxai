package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var (
	spawnSkipPermissions bool
	spawnTrustWorkspace  bool
	spawnTask            string
	spawnResume          string
	spawnAttach          bool
	spawnHost            string
	spawnCwd             string
)

var spawnCmd = &cobra.Command{
	Use:   "spawn <name>[@host] [task-file]",
	Short: "Start a new agent in its own tmux session",
	Long: `Spawn creates a new agent in a tmux session, running the backend's CLI
(built in: claude, aider, agy).

Initial work, if given, is sent as the agent's first message:
  - --task <name>  : the task's content, from the task repository (DB)
  - [task-file]    : a request to read that file (claude/agy: "Please read and
                     execute the task in <file>", aider: "/read <file>")
Without either, send the first message yourself with "agent send".

When an agent runs spawn (AGENT_NAME set), the new agent is recorded as its
helper: "agent list" shows it under PARENT, and only that agent (or its own
parent, and so on up) or a person can clean it up. It starts in the
directory spawn runs from, so it gets that project's permission settings
(for Claude, its .claude/ settings on top of the user's); --skip-permissions
bypasses them.

A Claude agent reports its status, takes held messages, and has its safe
agent commands approved through the hook Claude Code runs for every session,
so spawn refuses one until "agent hooks install" has registered this
binary's hook.

Spawn returns once the CLI has had its startup time, and says the status it
found the agent in: "idle" means it is ready for a message.

--resume continues a Claude or agy session that already exists, as this
agent, instead of starting a new one: a session you ran in a terminal joins
the workgroup, and agent send reaches it from then on. It takes what agent
transcript takes (a session id, the start of one, a transcript's path, or a
workgroup agent's name for its last session) and the --backend of the
session's harness. The session must have ended (agent sessions shows its
status): quit the CLI running it first, since two CLIs on one session would both write its transcript. A
subagent's session can't be resumed on its own. The agent starts in the
directory the session ran in, whatever directory spawn runs from.

--attach opens the new agent's tmux session in this terminal once it
starts, as agent attach does (Ctrl-b d detaches; the agent keeps running).

--cwd starts the agent in that directory instead of the one spawn runs
from (a relative task file is then taken from spawn's directory).

A custom backend (--backend-config <file>) is only known through that file:
pass it again to status, wait, and watch for that agent.

On another host: name@host (or --host) starts the agent on that host
through the agent bus, as status, send and wait reach it there. That host's
bus must allow it: spawn is off for other hosts unless its --remote-ops lists
it (agent bus start --remote-ops list,status,capture,send,respond,spawn).
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
clean the agent up on its host.`,
	Example: `  agent spawn researcher --backend claude
  agent spawn researcher --backend claude --task research-auth
  agent spawn builder --backend claude tasks/build.md
  agent spawn helper --backend aider
  agent spawn auto-worker --backend claude --skip-permissions
  agent spawn reviewer --backend agy --trust-workspace
  agent spawn lead --backend claude --resume 3f34adff --attach
  agent spawn builder@hub --backend claude --cwd /srv/repo tasks/build.md
  agent spawn builder --host hub --backend claude --task build`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runSpawn,
}

func init() {
	spawnCmd.Flags().BoolVar(&spawnSkipPermissions, "skip-permissions", false,
		"Pass --dangerously-skip-permissions to Claude or Agy (use with caution)")
	spawnCmd.Flags().BoolVar(&spawnTrustWorkspace, "trust-workspace", false,
		"Confirm the CLI's workspace trust prompt on launch (use with caution)")
	spawnCmd.Flags().StringVar(&spawnTask, "task", "",
		"Reference a task from the repository by name")
	spawnCmd.Flags().StringVar(&spawnResume, "resume", "",
		"Continue this ended Claude or agy session as the agent (an id, the start of one, or a transcript's path)")
	spawnCmd.Flags().BoolVar(&spawnAttach, "attach", false,
		"Open the agent's tmux session in this terminal once it starts")
	spawnCmd.Flags().StringVar(&spawnHost, "host", "",
		"Start the agent on this host, through the agent bus (same as name@host)")
	spawnCmd.Flags().StringVar(&spawnCwd, "cwd", "",
		"Directory the agent starts in (on another host: an absolute path there)")
}

func runSpawn(cmd *cobra.Command, args []string) error {
	ref, remote, err := spawnTarget(args[0], spawnHost)
	if err != nil {
		return err
	}
	if remote {
		return runRemoteSpawn(ref, args[1:])
	}
	name := ref.Name
	var taskFile string
	var taskContent string

	// Priority: --task flag > positional task-file argument
	if spawnTask != "" {
		// Fetch task from DB
		task, err := getTask(spawnTask)
		if err != nil {
			return err
		}
		taskContent = task.Content
		// Use source file if available for reference
		if task.SourceFile != "" {
			taskFile = task.SourceFile
		}
	} else if len(args) > 1 {
		taskFile = args[1]
	}

	var cwd string
	if spawnCwd != "" {
		if spawnResume != "" {
			return fmt.Errorf("--cwd and --resume don't go together: a resumed agent starts where its session ran")
		}
		cwd = absPath(spawnCwd)
		if taskFile != "" {
			taskFile = absPath(taskFile) // the agent reads it from cwd
		}
	}

	var resume string
	if spawnResume != "" {
		s, err := resumableSession(getContext(), spawnResume, cfg.Backend)
		if err != nil {
			return err
		}
		// The agent starts where the session ran: its tmux session opens in
		// spawn's directory, and the CLI finds the session from there.
		if taskFile != "" {
			taskFile = absPath(taskFile)
		}
		if err := os.Chdir(s.Cwd); err != nil {
			return fmt.Errorf("entering %s, where session %s ran: %w", s.Cwd, s.SessionID, err)
		}
		resume = s.SessionID
	}

	opts := service.SpawnOptions{
		Name:            name,
		Backend:         cfg.Backend,
		TaskFile:        taskFile,
		TaskContent:     taskContent,
		SkipPermissions: spawnSkipPermissions,
		TrustWorkspace:  spawnTrustWorkspace,
		SpawnedBy:       os.Getenv("AGENT_NAME"),
		Resume:          resume,
		Cwd:             cwd,
	}

	agent, err := agentService.Spawn(getContext(), opts)
	if errors.Is(err, backend.ErrNotFound) {
		return withBackendConfigErr(err)
	}
	if err != nil {
		return err
	}

	where := ""
	if cwd != "" {
		where = "in " + cwd
	}
	fmt.Print(spawnSummary(agent, where, spawnTask, taskFile, resume))
	if info, err := agentService.StatusInfo(getContext(), name); err == nil {
		fmt.Println(spawnReadiness(name, info.Status))
	}
	if spawnAttach {
		return attachSession(agent.SessionName)
	}
	return nil
}

// spawnTarget reads spawn's name argument and --host: ref.Host is set, and
// remote true, when either names another host. Naming this host is a local
// spawn; naming two different hosts is an error.
func spawnTarget(arg, host string) (ref natsbus.AgentRef, remote bool, err error) {
	ref, remote, err = parseTarget(arg)
	if err != nil {
		return natsbus.AgentRef{}, false, err
	}
	if host == "" {
		return ref, remote, nil
	}
	if err := natsbus.ValidateHost(host); err != nil {
		return natsbus.AgentRef{}, false, err
	}
	argHost := ref.Host
	if strings.Contains(arg, "@") && argHost == "" {
		argHost = cfg.Host // name@<this host>, which parseTarget made bare
	}
	if argHost != "" && argHost != host {
		return natsbus.AgentRef{}, false, fmt.Errorf("%s names host %s, but --host says %s: give one", arg, argHost, host)
	}
	if !isRemoteHost(host) {
		return natsbus.AgentRef{Name: ref.Name}, false, nil
	}
	return natsbus.AgentRef{Name: ref.Name, Host: host}, true, nil
}

// runRemoteSpawn starts ref.Name on ref.Host through the agent bus. rest is
// spawn's arguments after the name: the task file, if any.
func runRemoteSpawn(ref natsbus.AgentRef, rest []string) error {
	for _, f := range []struct {
		set  bool
		flag string
	}{
		{spawnResume != "", "--resume"},
		{spawnAttach, "--attach"},
		{spawnSkipPermissions, "--skip-permissions"},
		{spawnTrustWorkspace, "--trust-workspace"},
		{backendConfig != "", "--backend-config"},
	} {
		if f.set {
			return fmt.Errorf("%s works only for an agent on this host: to spawn %s with it, run agent spawn on %s", f.flag, ref.Name, ref.Host)
		}
	}
	if spawnCwd != "" && !strings.HasPrefix(spawnCwd, "/") {
		return fmt.Errorf("--cwd %s: give an absolute path on %s (a relative one would be relative to wherever its bus runs)", spawnCwd, ref.Host)
	}
	req := natsbus.SpawnRequest{
		Backend: cfg.Backend,
		Cwd:     spawnCwd,
	}
	if me := os.Getenv("AGENT_NAME"); me != "" {
		req.SpawnedBy = natsbus.AgentRef{Name: me, Host: cfg.Host}.String()
	}
	if spawnTask != "" {
		task, err := getTask(spawnTask)
		if err != nil {
			return err
		}
		req.TaskContent = task.Content // its source file, if any, is a path here
	} else if len(rest) > 0 {
		req.TaskFile = rest[0]
	}

	var resp *natsbus.SpawnResponse
	err := callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
		resp, err = c.Spawn(getContext(), ref, req)
		return err
	})
	var re *natsbus.RemoteError
	switch {
	case errors.As(err, &re):
		return fmt.Errorf("%s couldn't spawn %s: %w", ref.Host, ref.Name, err)
	case err != nil:
		if strings.Contains(err.Error(), "no bus answering") {
			hint("%s serves spawn to other hosts only if its bus runs with --remote-ops listing spawn (agent bus status there shows what it allows)", ref.Host)
		}
		return err
	}

	agent := &storage.Agent{Name: ref.String(), SessionName: resp.Session, Backend: resp.Backend, SpawnedBy: resp.SpawnedBy}
	fmt.Print(spawnSummary(agent, fmt.Sprintf("on %s in %s", ref.Host, resp.Cwd), spawnTask, resp.TaskFile, ""))
	fmt.Println(remoteSpawnReadiness(ref, service.AgentStatus(resp.Status)))
	return nil
}

// remoteSpawnReadiness is spawnReadiness for an agent on another host,
// where logs don't reach.
func remoteSpawnReadiness(ref natsbus.AgentRef, status service.AgentStatus) string {
	if status == service.AgentStatusEnded {
		return fmt.Sprintf("Its session has already ended: agent capture %s shows its screen.", ref)
	}
	return spawnReadiness(ref.String(), status)
}

// resumableSession finds the session --resume names and checks that an
// agent of backendName can continue it.
func resumableSession(ctx context.Context, arg, backendName string) (discovery.Session, error) {
	t, err := resolveTranscript(ctx, arg)
	if err != nil {
		return discovery.Session{}, err
	}
	return t.session, checkResumable(t.session, backendName)
}

// checkResumable says why an agent of backendName can't continue s, or nil.
func checkResumable(s discovery.Session, backendName string) error {
	switch {
	case s.Harness != discovery.HarnessClaude && s.Harness != discovery.HarnessAgy:
		return fmt.Errorf("session %s is a %s session: --resume continues claude and agy sessions", s.SessionID, s.Harness)
	case s.Harness != backendName:
		return fmt.Errorf("session %s is a %s session: spawn it with --backend %s", s.SessionID, s.Harness, s.Harness)
	case s.ParentSession != nil:
		return fmt.Errorf("session %s is a subagent's: resume the session that started it, %s", s.SessionID, s.ParentSession.SessionID)
	case s.Status != discovery.StatusEnded:
		owner := ""
		if s.Workgroup != nil {
			owner = fmt.Sprintf(" (agent %s runs it)", s.Workgroup.Name)
		}
		return fmt.Errorf("session %s is %s, not ended%s: quit the CLI running it first, since two CLIs on one session would both write its transcript", s.SessionID, s.Status, owner)
	case s.Cwd == "":
		return fmt.Errorf("session %s recorded no working directory to resume it in", s.SessionID)
	}
	if info, err := os.Stat(s.Cwd); err != nil || !info.IsDir() {
		return fmt.Errorf("session %s ran in %s, which is no longer a directory", s.SessionID, s.Cwd)
	}
	return nil
}

// spawnReadiness says what the status spawn found the new agent in means
// for sending to it.
func spawnReadiness(name string, status service.AgentStatus) string {
	switch status {
	case service.AgentStatusIdle:
		return "It is idle: ready for a message."
	case service.AgentStatusProcessing:
		return "It is processing (still starting, or working on its task)."
	case service.AgentStatusWaiting:
		return fmt.Sprintf("It is already waiting on a prompt: agent status %s -v shows it.", name)
	case service.AgentStatusEnded:
		return fmt.Sprintf("Its session has already ended: agent logs %s shows why.", name)
	}
	return fmt.Sprintf("Its status is %s so far: agent wait %s --for idle --timeout 1m waits until it's ready.", status, name)
}

// spawnSummary says what spawn did: the new agent, where it started (where,
// e.g. "on hub in /srv", if not the obvious place), its parent if an agent
// spawned it, and what it was sent first.
func spawnSummary(agent *storage.Agent, where, task, taskFile, resume string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "Spawned agent %s (session %s, backend %s)", agent.Name, agent.SessionName, agent.Backend)
	if where != "" {
		fmt.Fprintf(&b, " %s", where)
	}
	if agent.SpawnedBy != "" {
		fmt.Fprintf(&b, ", a helper of %s", agent.SpawnedBy)
	}
	b.WriteString(".\n")
	if resume != "" {
		fmt.Fprintf(&b, "It continues session %s.\n", resume)
	}
	switch {
	case task != "":
		fmt.Fprintf(&b, "Sent it task %s.\n", task)
	case taskFile != "":
		fmt.Fprintf(&b, "Asked it to read %s.\n", taskFile)
	case resume != "":
	default:
		fmt.Fprintf(&b, "It has no task yet: agent send %s \"...\"\n", agent.Name)
	}
	return b.String()
}
