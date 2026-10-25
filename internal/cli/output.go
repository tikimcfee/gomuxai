package cli

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"sync"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Every command prints its result on stdout and at most one hint, a
// suggested next step, on stderr. A hint is only printed after a check
// showed the step is needed, and --quiet drops it.

var (
	quiet   bool
	hintOut io.Writer = os.Stderr
	hinted  bool
)

// hint prints a suggested next step on stderr: the first one per command,
// unless --quiet.
func hint(format string, a ...any) {
	if quiet || hinted {
		return
	}
	hinted = true
	fmt.Fprintf(hintOut, "hint: "+format+"\n", a...)
}

// notice tells, on stderr, what a command whose stdout is a stream of data
// is doing, unless --quiet.
func notice(format string, a ...any) {
	if !quiet {
		fmt.Fprintf(hintOut, format+"\n", a...)
	}
}

// printJSON prints v as indented JSON on stdout, for --json. &, < and >
// print as themselves, so commands read as typed.
func printJSON(v any) error {
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	return enc.Encode(v)
}

// plural returns "1 agent" or "3 agents".
func plural(n int, noun string) string {
	if n == 1 {
		return fmt.Sprintf("1 %s", noun)
	}
	return fmt.Sprintf("%d %ss", n, noun)
}

var explainOnce sync.Once

// explainArgErrors makes every command's argument errors show its usage
// line and first example, so a missing argument says what goes there.
func explainArgErrors(root *cobra.Command) {
	explainOnce.Do(func() {
		var walk func(*cobra.Command)
		walk = func(c *cobra.Command) {
			// The root's Args only reject unknown commands, which say so.
			if v := c.Args; v != nil && c.HasParent() {
				c.Args = func(cmd *cobra.Command, args []string) error {
					if err := v(cmd, args); err != nil {
						return usageError(cmd, err)
					}
					return nil
				}
			}
			for _, sub := range c.Commands() {
				walk(sub)
			}
		}
		walk(root)
	})
}

// usageError adds cmd's usage line and first example to err.
func usageError(cmd *cobra.Command, err error) error {
	msg := fmt.Sprintf("%v\nUsage: %s", err, cmd.UseLine())
	if ex := firstExample(cmd); ex != "" {
		msg += "\nExample: " + ex
	}
	return fmt.Errorf("%s", msg)
}

// firstExample returns the first command line in cmd's Example.
func firstExample(cmd *cobra.Command) string {
	for _, line := range strings.Split(cmd.Example, "\n") {
		line = strings.TrimSpace(line)
		if strings.HasPrefix(line, "agent ") {
			return line
		}
	}
	return ""
}

// flagError adds close flag names, and where to see them all, to an unknown
// flag error.
func flagError(cmd *cobra.Command, err error) error {
	msg := err.Error()
	name, ok := strings.CutPrefix(msg, "unknown flag: --")
	if !ok {
		if short, ok := strings.CutPrefix(msg, "unknown shorthand flag: "); ok {
			return fmt.Errorf("%s (see %s --help for its flags)", short, cmd.CommandPath())
		}
		return fmt.Errorf("%s\nUsage: %s", msg, cmd.UseLine())
	}
	var names []string
	add := func(f *pflag.Flag) { names = append(names, f.Name) }
	cmd.Flags().VisitAll(add)
	cmd.InheritedFlags().VisitAll(add)
	if similar := util.Similar(name, names); len(similar) > 0 {
		for i, s := range similar {
			similar[i] = "--" + s
		}
		return fmt.Errorf("%s; did you mean %s?", msg, util.OrList(similar, nil))
	}
	return fmt.Errorf("%s (see %s --help for its flags)", msg, cmd.CommandPath())
}

// runGroup is the RunE of a command that only groups subcommands: alone it
// prints its help, and an unknown subcommand is an error that suggests
// close ones, as for the root command.
func runGroup(cmd *cobra.Command, args []string) error {
	if len(args) == 0 {
		return cmd.Help()
	}
	return unknownCommand(cmd, args[0])
}

// rootArgs is the root command's Args: any word that isn't a command is an
// unknown command, suggesting close ones by the same rule as runGroup.
// (Cobra's own suggestions take any command within two edits, so "bogus"
// suggested logs and bus.)
func rootArgs(cmd *cobra.Command, args []string) error {
	if len(args) > 0 {
		return unknownCommand(cmd, args[0])
	}
	return nil
}

// unknownCommand is the error for a word that isn't one of cmd's
// subcommands: close ones if any, and where to see them all.
func unknownCommand(cmd *cobra.Command, name string) error {
	msg := fmt.Sprintf("unknown command %q for %q", name, cmd.CommandPath())
	var names []string
	for _, sub := range cmd.Commands() {
		if sub.IsAvailableCommand() || sub.Name() == "help" {
			names = append(names, sub.Name())
		}
	}
	if s := util.Similar(name, names); len(s) > 0 {
		msg += "; did you mean " + util.OrList(s, nil) + "?"
	}
	help := "agent help"
	if cmd.HasParent() {
		help += " " + strings.TrimPrefix(cmd.CommandPath(), cmd.Root().Name()+" ")
	}
	return fmt.Errorf("%s (see %s)", msg, help)
}
