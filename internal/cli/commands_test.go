package cli

import (
	"strings"
	"testing"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

func TestCommandArgs(t *testing.T) {
	tests := []struct {
		cmd  *cobra.Command
		args []string
		ok   bool
	}{
		{spawnCmd, nil, false},
		{spawnCmd, []string{"researcher"}, true},
		{spawnCmd, []string{"researcher", "tasks/research.md"}, true},
		{spawnCmd, []string{"researcher", "tasks/research.md", "extra"}, false},
		{sendCmd, []string{"agent"}, false},
		{sendCmd, []string{"agent", "please", "analyze"}, true},
		{queueCmd, []string{"agent"}, false},
		{queueCmd, []string{"agent", "fix", "the", "bug"}, true},
		{captureCmd, nil, false},
		{captureCmd, []string{"agent"}, true},
		{listCmd, nil, true},
		{listCmd, []string{"unexpected"}, false},
		{statusCmd, []string{"agent"}, true},
		{statusCmd, []string{"a", "b"}, false},
		{cleanupCmd, nil, true}, // with --all; without, runCleanup refuses
		{cleanupCmd, []string{"agent"}, true},
		{cleanupCmd, []string{"a", "b"}, false},
		{watchCmd, []string{"unexpected"}, false},
		{sessionNewCmd, []string{"my-session"}, true},
		{sessionKillCmd, nil, false},
		{sessionListCmd, nil, true},
		{keysCmd, []string{"my-session"}, false},
		{keysCmd, []string{"my-session", "echo", "hello"}, true},
		{keyCmd, []string{"my-session"}, false},
		{keyCmd, []string{"my-session", "Enter"}, true},
		{keyCmd, []string{"my-session", "Enter", "extra"}, false},
		{clearCmd, []string{"my-session"}, true},
		{taskSetCmd, []string{"name"}, true}, // with -f
		{taskSetCmd, []string{"name", "content"}, true},
		{taskSetCmd, []string{"name", "content", "extra"}, false},
		{taskGetCmd, []string{"name"}, true},
		{taskListCmd, []string{"unexpected"}, false},
		{taskClearCmd, nil, false},
		{stateSetCmd, []string{"agent", "key"}, false},
		{stateSetCmd, []string{"agent", "key", "value"}, true},
		{stateGetCmd, []string{"agent", "key"}, true},
		{stateListCmd, []string{"agent"}, true},
		{stateClearCmd, nil, false},
		{logsCmd, nil, false},
		{logsCmd, []string{"agent"}, true},
		{reportCmd, nil, true}, // with --hook
		{reportCmd, []string{"idle", "detail", "extra"}, false},
		{approveCmd, []string{"agent"}, true},
		{denyCmd, nil, false},
		{waitCmd, []string{"agent"}, true},
		{eventsCmd, nil, true},
		{eventsCmd, []string{"a", "b"}, false},
		{busTrustCmd, nil, false},
		{busStartCmd, []string{"unexpected"}, false},
	}
	for _, tt := range tests {
		err := tt.cmd.ValidateArgs(tt.args)
		if (err == nil) != tt.ok {
			t.Errorf("%s %q: err = %v, want ok = %v", tt.cmd.CommandPath(), tt.args, err, tt.ok)
		}
	}
}

func TestFlagDefaults(t *testing.T) {
	tests := []struct {
		cmd        *cobra.Command
		flag, want string
	}{
		{captureCmd, "lines", "50"},
		{captureCmd, "raw", "false"},
		{logsCmd, "lines", "50"},
		{watchCmd, "interval", "2s"},
		{keysCmd, "no-enter", "false"},
		{waitCmd, "timeout", "0s"},
		{waitCmd, "interval", "0s"},
		{busStartCmd, "remote-ops", "[list,status,capture,send,respond]"},
	}
	for _, tt := range tests {
		f := tt.cmd.Flags().Lookup(tt.flag)
		if f == nil {
			t.Errorf("%s has no --%s", tt.cmd.CommandPath(), tt.flag)
			continue
		}
		if f.DefValue != tt.want {
			t.Errorf("%s --%s default = %q, want %q", tt.cmd.CommandPath(), tt.flag, f.DefValue, tt.want)
		}
	}
}

func TestSpawnRequiresBackend(t *testing.T) {
	old := cfg
	t.Cleanup(func() { cfg = old })
	err := setup(spawnCmd, []string{"researcher"})
	if err == nil || !strings.Contains(err.Error(), "--backend") || !strings.Contains(err.Error(), "--backend-config") {
		t.Errorf("err = %v, want one naming --backend and --backend-config", err)
	}
}

// Every example in the commands' help must name a real command, use only
// its flags, and pass an accepted number of arguments, so the help can't
// drift from what the commands take.
func TestHelpExamplesParse(t *testing.T) {
	var walk func(*cobra.Command)
	walk = func(c *cobra.Command) {
		for _, line := range strings.Split(c.Long, "\n") {
			if !strings.HasPrefix(line, "  ") {
				continue
			}
			for _, example := range strings.Split(line, "&&") {
				if words := exampleWords(example); len(words) > 0 && words[0] == "agent" {
					checkExample(t, c, words[1:])
				}
			}
		}
		for _, sub := range c.Commands() {
			walk(sub)
		}
	}
	walk(rootCmd)
}

func checkExample(t *testing.T, from *cobra.Command, args []string) {
	t.Helper()
	cmd, rest, err := rootCmd.Find(args)
	if err != nil || !cmd.Runnable() {
		t.Errorf("%s help: agent %s: no such command (%v)", from.CommandPath(), strings.Join(args, " "), err)
		return
	}
	// Parsing sets the commands' flag variables: put them back.
	defer cmd.Flags().VisitAll(func(f *pflag.Flag) {
		if f.Changed {
			if s, ok := f.Value.(pflag.SliceValue); ok {
				s.Replace(strings.Split(strings.Trim(f.DefValue, "[]"), ","))
			} else {
				f.Value.Set(f.DefValue)
			}
			f.Changed = false
		}
	})
	if err := cmd.ParseFlags(rest); err != nil {
		t.Errorf("%s help: agent %s: %v", from.CommandPath(), strings.Join(args, " "), err)
		return
	}
	if err := cmd.ValidateArgs(cmd.Flags().Args()); err != nil {
		t.Errorf("%s help: agent %s: %v", from.CommandPath(), strings.Join(args, " "), err)
	}
}

// exampleWords splits a help example into shell words, honoring double
// quotes and dropping a trailing "# comment" or "< input" redirect.
func exampleWords(s string) []string {
	s, _, _ = strings.Cut(s, " < ")
	var words []string
	var word strings.Builder
	inWord, quoted := false, false
	for _, r := range s {
		switch {
		case r == '"':
			quoted, inWord = !quoted, true
		case quoted:
			word.WriteRune(r)
		case r == ' ' || r == '\t':
			if inWord {
				words = append(words, word.String())
				word.Reset()
				inWord = false
			}
		case r == '#' && !inWord:
			return words
		default:
			word.WriteRune(r)
			inWord = true
		}
	}
	if inWord {
		words = append(words, word.String())
	}
	return words
}
