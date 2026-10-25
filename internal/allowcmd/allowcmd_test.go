package allowcmd

import (
	"errors"
	"os"
	"path/filepath"
	"testing"
)

var safe = []string{
	"send", "deliver", "list", "status", "wait", "events", "capture", "logs", "state get", "history",
	"friction add", "friction +1", "friction list", "friction show",
}

// fixture lays out root/bin/agent (the program), root/link/agent (a
// symlink to it), and root/bin/other (another binary).
func fixture(t *testing.T) (root string, c Checker) {
	t.Helper()
	root = t.TempDir()
	for _, dir := range []string{"bin", "link"} {
		if err := os.Mkdir(filepath.Join(root, dir), 0755); err != nil {
			t.Fatal(err)
		}
	}
	prog := filepath.Join(root, "bin", "agent")
	for _, f := range []string{prog, filepath.Join(root, "bin", "other")} {
		if err := os.WriteFile(f, []byte("#!/bin/sh\n"), 0755); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.Symlink(prog, filepath.Join(root, "link", "agent")); err != nil {
		t.Fatal(err)
	}
	look := func(name string) (string, error) {
		switch name {
		case "agent":
			return filepath.Join(root, "link", "agent"), nil
		case "other":
			return filepath.Join(root, "bin", "other"), nil
		}
		return "", errors.New("not found")
	}
	return root, Checker{Program: prog, Subcommands: safe, LookPath: look}
}

func TestCheckAllowsSafeSubcommands(t *testing.T) {
	root, c := fixture(t)
	full := filepath.Join(root, "bin", "agent")
	for _, sub := range safe {
		for _, cmd := range []string{
			"agent " + sub,
			full + " " + sub,
			"'" + full + "' " + sub + " x",
			"./bin/agent " + sub,
			"bin/agent " + sub,
			"./link/agent " + sub,
			"agent -q " + sub,
			"agent --quiet " + sub + " --json",
		} {
			if err := c.Check(cmd, root); err != nil {
				t.Errorf("Check(%q) = %v, want allowed", cmd, err)
			}
		}
	}
}

func TestCheckAllows(t *testing.T) {
	root, c := fixture(t)
	for _, cmd := range []string{
		`agent send helper "review internal/auth for injection bugs"`,
		`agent send helper 'it''s $(not) run'`,
		`agent send helper "say \"hi\" for \$5"`,
		`agent send helper "two
lines"`,
		`agent send helper@hub done`,
		`agent wait a b --for idle,waiting --next --timeout 30m`,
		`agent friction +1 12 "hit it again"`,
		`agent state get helper _result`,
		`agent list 2>&1`,
		`agent list >/dev/null 2>&1`,
		`agent list 2>/dev/null`,
		`agent list &>/dev/null`,
		`agent list >&2`,
		`agent list;`,
		"agent capture helper --lines 100\n",
	} {
		if err := c.Check(cmd, root); err != nil {
			t.Errorf("Check(%q) = %v, want allowed", cmd, err)
		}
	}
}

func TestCheckRefuses(t *testing.T) {
	root, c := fixture(t)
	for _, cmd := range []string{
		// Not a safe subcommand.
		"agent", "agent -q", "agent spawn helper --backend claude", "agent approve helper",
		"agent deny helper", "agent keys agent-helper Enter", "agent state set helper k v",
		"agent state", "agent cleanup helper", "agent friction resolve 3", "agent friction",
		"agent bus start", "agent help", "agent sendx helper hi", "agent Send helper hi",
		"agent --backend-config x.json status", "agent --quiet=true list",
		// More than one command, or not a plain one.
		`agent send x "y"; rm -rf ~`, `agent send x "y" && rm -rf ~`, `agent list || rm x`,
		`agent list | sh`, `agent list |& sh`, "agent list\nrm -rf ~", "agent list\r\nrm x",
		`agent list & rm x`, `agent list &`, `! agent list`, `(agent list)`, `{ agent list; }`,
		`time agent list`, `if agent list; then rm x; fi`, `agent list # rm x`, `# hi
agent list`,
		// Expansions, substitutions, and globs.
		`agent send x $(rm -rf ~)`, "agent send x `rm -rf ~`", `agent send x "$(rm -rf ~)"`,
		"agent send x \"`rm`\"", `agent send x <(rm)`, `agent send x >(rm)`, `agent send x $HOME`,
		`agent send x "$HOME"`, `agent send x ${A}`, `agent send x $((1+1))`, `agent send x *`,
		`agent send x what?`, `agent send x [ab]`, `agent send x {a,b}`, `agent send x ~`,
		`agent send x ~/f`, `agent send x $'\n'`, `agent send x $"hi"`, `agent send x a\ b`,
		`agent send x !!`, `agent send x =ls`, `agent send x @(a|b)`, `agent send x hi#there`,
		`agent send x ^a`,
		// Variables and assignments.
		`$A send x hi`, `"$A" send x hi`, `${A} list`, `A=1 agent list`, `PATH=/tmp agent list`,
		`env agent list`, `env A=1 agent list`, `command agent list`, `exec agent list`,
		`builtin agent list`, `sudo agent list`, `nohup agent list`, `xargs agent list`,
		`agent $SUB x`, `agent "$SUB" x`, `$(echo agent) list`,
		// Redirections that write or read something.
		`agent list > out`, `agent list >> out`, `agent list 2> out`, `agent list &> out`,
		`agent list >& out`, `agent list >/dev/null.x`, `agent list 3>&1`, `agent list >&3`,
		`agent list < in`, `agent list <<EOF
hi
EOF`, `agent list <<< hi`, `agent list >| out`, `agent list > "$F"`,
		// Some other program.
		"other list", "./bin/other list", filepath.Join(root, "bin", "other") + " list",
		"./missing/agent list", "nope list", "agent/ list", "/bin/sh -c 'agent list'",
		"sh -c 'agent list'", "bash -c \"agent list\"",
		// Quoting tricks that change the program or subcommand.
		`'agent list'`, `"agent spawn" x`, `agent "spawn" x`, `agent 'state set' k v`,
		`agent st''ate set k v`, `agent "" list`,
		// Unparseable.
		`agent send x "unterminated`, `agent send x 'open`, `agent list )`,
		"", "   ", ";",
	} {
		if err := c.Check(cmd, root); err == nil {
			t.Errorf("Check(%q) allowed it, want refused", cmd)
		}
	}
}

func TestCheckRelativePathNeedsCwd(t *testing.T) {
	root, c := fixture(t)
	if err := c.Check("./bin/agent list", ""); err == nil {
		t.Error("relative path with no cwd allowed")
	}
	if err := c.Check("./bin/agent list", filepath.Join(root, "link")); err == nil {
		t.Error("./bin/agent from the wrong cwd allowed")
	}
	if err := c.Check("../bin/agent list", filepath.Join(root, "link")); err != nil {
		t.Errorf("../bin/agent from link/: %v", err)
	}
}

func TestCheckSymlinkedProgram(t *testing.T) {
	// The configured program is itself reached through a symlink.
	root, c := fixture(t)
	c.Program = filepath.Join(root, "link", "agent")
	if err := c.Check("./bin/agent status x", root); err != nil {
		t.Errorf("Check through a symlinked Program: %v", err)
	}
}
