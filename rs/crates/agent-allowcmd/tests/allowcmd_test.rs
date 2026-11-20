// Ported from internal/allowcmd/allowcmd_test.go, plus the command lines of
// tests/service/allow_test.go and a table of adversarial refusals.

mod common;

use common::{SAFE, fixture};

#[test]
fn check_allows_safe_subcommands() {
    let (fx, c) = fixture();
    let full = fx.path("bin/agent");
    for sub in SAFE {
        for cmd in [
            format!("agent {sub}"),
            format!("{full} {sub}"),
            format!("'{full}' {sub} x"),
            format!("./bin/agent {sub}"),
            format!("bin/agent {sub}"),
            format!("./link/agent {sub}"),
            format!("agent -q {sub}"),
            format!("agent --quiet {sub} --json"),
        ] {
            if let Err(e) = c.check(&cmd, fx.root_str()) {
                panic!("check({cmd:?}) = {e:#}, want allowed");
            }
        }
    }
}

// Lines the Go allows, and so must this. The Go's own list first, then the
// forms CLAUDE.md names and a few more of the same shapes.
#[rustfmt::skip]
const ALLOWED: &[&str] = &[
    // From the Go.
    r#"agent send helper "review internal/auth for injection bugs""#,
    r#"agent send helper 'it''s $(not) run'"#,
    r#"agent send helper "say \"hi\" for \$5""#,
    "agent send helper \"two\nlines\"",
    "agent send helper@hub done",
    "agent wait a b --for idle,waiting --next --timeout 30m",
    r#"agent friction +1 12 "hit it again""#,
    "agent friction link 4 34@workstation",
    "agent friction links --dangling",
    "agent state get helper _result",
    "agent list 2>&1",
    "agent list >/dev/null 2>&1",
    "agent list 2>/dev/null",
    "agent list &>/dev/null",
    "agent list >&2",
    "agent list;",
    "agent capture helper --lines 100\n",
    // From tests/service/allow_test.go.
    "./bin/agent status worker -v",
    "./bin/agent -q list",
    // CLAUDE.md's examples and their neighbours.
    r#"agent send helper "run the tests; then report""#,
    r#"agent send helper 'a | b && c > d $(e) `f` ~ * ? [g] {h,i} #j !k'"#,
    r#"agent send lead "done: 3 issues, see notes.md (and #4!)""#,
    "agent -q list",
    "agent -q -q --quiet list",
    "agent list > /dev/null",
    "agent list >>/dev/null 2>>/dev/null",
    "agent list 1>&2",
    "agent list>/dev/null",
    "2>&1 agent list",
    ">/dev/null agent list",
    "agent list >\"/dev/null\"",
    "  \tagent list  ",
    "\n\nagent list ;\n\n",
    "agent\tlist\t--json",
    "agent send helper \"tab\there\"",
    "agent send helper 'naïve café — “quoted” ✓'",
    "agent send helper \"naïve café — ✓\"",
    r#"agent send helper "a\b \n stays""#,
    "agent send helper \"\\\\\"",
    r#"agent send helper "\`not run\`""#,
    "agent send helper ''",
    "agent send helper \"costs 5$ each, or a$\"",
    "agent send helper a=b --for=idle 100% +1 x:y,z @h",
    "agent send helper -- --now",
    "agent friction add \"logs can't show just one turn\" --kind missing",
    "agent send helper hi --force",
    "agent status helper@hub -v",
];

#[test]
fn check_allows() {
    let (fx, c) = fixture();
    for cmd in ALLOWED {
        if let Err(e) = c.check(cmd, fx.root_str()) {
            panic!("check({cmd:?}) = {e:#}, want allowed");
        }
    }
    let full = fx.path("bin/agent");
    let cmd = format!("{full} state get worker _result");
    c.check(&cmd, fx.root_str()).unwrap();
}

#[test]
fn check_refuses() {
    let (fx, c) = fixture();
    let other = format!("{} list", fx.path("bin/other"));
    #[rustfmt::skip]
    let mut cmds: Vec<&str> = vec![
        // Not a safe subcommand.
        "agent", "agent -q", "agent spawn helper --backend claude", "agent approve helper",
        "agent deny helper", "agent keys agent-helper Enter", "agent state set helper k v",
        "agent state", "agent cleanup helper", "agent friction resolve 3", "agent friction",
        "agent friction unlink 4 34@workstation", "agent friction linkx 4 34@h",
        "agent bus start", "agent help", "agent sendx helper hi", "agent Send helper hi",
        "agent --backend-config x.json status", "agent --quiet=true list",
        // More than one command, or not a plain one.
        r#"agent send x "y"; rm -rf ~"#, r#"agent send x "y" && rm -rf ~"#, "agent list || rm x",
        "agent list | sh", "agent list |& sh", "agent list\nrm -rf ~", "agent list\r\nrm x",
        "agent list & rm x", "agent list &", "! agent list", "(agent list)", "{ agent list; }",
        "time agent list", "if agent list; then rm x; fi", "agent list # rm x", "# hi\nagent list",
        // Expansions, substitutions, and globs.
        "agent send x $(rm -rf ~)", "agent send x `rm -rf ~`", r#"agent send x "$(rm -rf ~)""#,
        "agent send x \"`rm`\"", "agent send x <(rm)", "agent send x >(rm)", "agent send x $HOME",
        r#"agent send x "$HOME""#, "agent send x ${A}", "agent send x $((1+1))", "agent send x *",
        "agent send x what?", "agent send x [ab]", "agent send x {a,b}", "agent send x ~",
        "agent send x ~/f", r"agent send x $'\n'", r#"agent send x $"hi""#, r"agent send x a\ b",
        "agent send x !!", "agent send x =ls", "agent send x @(a|b)", "agent send x hi#there",
        "agent send x ^a",
        // Variables and assignments.
        "$A send x hi", r#""$A" send x hi"#, "${A} list", "A=1 agent list", "PATH=/tmp agent list",
        "env agent list", "env A=1 agent list", "command agent list", "exec agent list",
        "builtin agent list", "sudo agent list", "nohup agent list", "xargs agent list",
        "agent $SUB x", r#"agent "$SUB" x"#, "$(echo agent) list",
        // Redirections that write or read something.
        "agent list > out", "agent list >> out", "agent list 2> out", "agent list &> out",
        "agent list >& out", "agent list >/dev/null.x", "agent list 3>&1", "agent list >&3",
        "agent list < in", "agent list <<EOF\nhi\nEOF", "agent list <<< hi", "agent list >| out",
        r#"agent list > "$F""#,
        // Some other program.
        "other list", "./bin/other list", &other, "./missing/agent list", "nope list",
        "agent/ list", "/bin/sh -c 'agent list'", "sh -c 'agent list'", "bash -c \"agent list\"",
        // Quoting tricks that change the program or subcommand.
        "'agent list'", r#""agent spawn" x"#, r#"agent "spawn" x"#, "agent 'state set' k v",
        "agent st''ate set k v", r#"agent "" list"#,
        // Unparseable.
        r#"agent send x "unterminated"#, "agent send x 'open", "agent list )", "", "   ", ";",
    ];
    // From tests/service/allow_test.go.
    cmds.extend([
        "./bin/agent spawn x --backend claude",
        "./bin/agent list; rm -rf ~",
        "$A list",
        "ls",
    ]);
    for cmd in cmds {
        if c.check(cmd, fx.root_str()).is_ok() {
            panic!("check({cmd:?}) allowed it, want refused");
        }
    }
}

#[test]
fn check_relative_path_needs_cwd() {
    let (fx, c) = fixture();
    assert!(
        c.check("./bin/agent list", "").is_err(),
        "relative path with no cwd allowed"
    );
    assert!(
        c.check("./bin/agent list", "bin").is_err(),
        "relative cwd allowed"
    );
    assert!(
        c.check("./bin/agent list", &fx.path("link")).is_err(),
        "./bin/agent from the wrong cwd allowed"
    );
    if let Err(e) = c.check("../bin/agent list", &fx.path("link")) {
        panic!("../bin/agent from link/: {e:#}");
    }
}

#[test]
fn check_symlinked_program() {
    // The configured program is itself reached through a symlink.
    let (fx, mut c) = fixture();
    c.program = fx.path("link/agent");
    if let Err(e) = c.check("./bin/agent status x", fx.root_str()) {
        panic!("check through a symlinked program: {e:#}");
    }
}

#[test]
fn check_path_must_resolve_both_ways() {
    let (fx, c) = fixture();
    // up -> sub/deep, so by the kernel up/../.. is root, but by its text
    // up/../../bin/agent is /bin/agent: the Go refuses it, and so must this.
    assert!(c.check("up/../../bin/agent list", fx.root_str()).is_err());
    // By its text up/../bin/agent is root/bin/agent (the Go allows it), but
    // the shell would run root/sub/bin/agent, which isn't there.
    assert!(c.check("up/../bin/agent list", fx.root_str()).is_err());
    // A path through a plain directory is fine either way.
    c.check("sub/../bin/agent list", fx.root_str()).unwrap();
    c.check(
        &format!("{} list", fx.path("sub/deep/../../bin/agent")),
        "/",
    )
    .unwrap();
}

// Lines the Go allows that this refuses on purpose. Each is exotic; the
// comment says why it is refused.
#[rustfmt::skip]
const REFUSED_THOUGH_GO_ALLOWS: &[&str] = &[
    // A line continuation outside quotes anywhere but between two words.
    "agent list \\\n",
    "\\\nagent list",
    "agent\\\nlist",
    // A backslash before CR LF: the Go reads a line continuation, the shell
    // an escaped CR and then the end of the command.
    "agent \\\r\nlist",
    // In a GBK or Big5 locale the shell reads "é\" as one character, so the
    // escaped quote would close the string and "; rm x" would run.
    "agent send x \"é\\\"; rm x; \\\"\"",
    // Control characters (other than newline and tab) inside quotes.
    "agent send x \"\u{1b}[2J\"",
    "agent send x 'a\rb'",
    // A dup target that is quoted (zsh may read it as a file name), or set
    // apart from >& by a blank.
    "agent list >&\"2\"",
    "agent list >& 2",
    // A redirection run straight into another.
    "agent list 2>&1>/dev/null",
    // A leading = after an empty quoted part (zsh's =cmd).
    "agent send x \"\"=ls",
    // A path the shell can't run: a trailing slash on a file, and a path
    // whose ".." goes through a symlink elsewhere.
    "./bin/agent/ list",
    "./link/agent/../agent list",
    "up/../bin/agent list",
    // A first word that is a shell builtin, even if PATH found the program.
    // (Covered with a checker whose program is named like one, below.)
];

#[test]
fn refused_though_go_allows() {
    let (fx, c) = fixture();
    for cmd in REFUSED_THOUGH_GO_ALLOWS {
        assert!(
            c.check(cmd, fx.root_str()).is_err(),
            "check({cmd:?}) allowed it, want refused"
        );
    }
    // A program named like a builtin: the shell runs the builtin. The last
    // six are zsh's, which the review found missing (1.3).
    for name in [
        "test", "disable", "compadd", "compset", "echotc", "echoti", "ttyctl",
    ] {
        let (fx, mut c) = fixture();
        let bin = fx.path(&format!("bin/{name}"));
        std::fs::copy(fx.path("bin/agent"), &bin).unwrap();
        c.program = bin.clone();
        c.look_path = Some(Box::new(move |_, _| Ok(bin.clone())));
        assert!(
            c.check(&format!("{name} list"), fx.root_str()).is_err(),
            "{name} list allowed"
        );
        c.check(&format!("./bin/{name} list"), fx.root_str())
            .unwrap();
    }
}

// Lines the Go allows that this refuses on purpose, where the shell does
// something other than what the Go's checks assume.
#[rustfmt::skip]
const REFUSED_STRICTER_THAN_GO: &[&str] = &[
    // &>> doesn't exist in bash 3.2 (macOS's /bin/bash): a syntax error.
    "agent list &>>/dev/null",
    "agent send helper &>>/dev/null sh -c 'id'",
    "&>>/dev/null agent list",
    // A root flag after the subcommand still takes effect (review 1.5).
    "agent list --backend-config x.json",
    "agent list --backend-config=x.json",
    "./bin/agent list --backend-config /var/tmp/x.json",
    "agent status --termd-ns default",
    "agent status --termd-ns=wall",
    "agent send helper --backend claude",
    "agent send helper --backend=claude",
    "agent -q send helper hi --termd-ns x",
    "agent state get helper k --backend-config=y",
    "agent send helper \"--backend-config=x\"",
    "agent send helper '--termd-ns'",
    "agent send helper -- --backend-config x",
    "agent friction add s --kind bug --backend-config x",
];

#[test]
fn refused_stricter_than_go() {
    let (fx, c) = fixture();
    for cmd in REFUSED_STRICTER_THAN_GO {
        assert!(
            c.check(cmd, fx.root_str()).is_err(),
            "check({cmd:?}) allowed it, want refused"
        );
    }
    // Text that only contains a root flag, inside a longer word, is fine.
    for cmd in [
        "agent send helper \"don't pass --backend-config here\"",
        "agent send helper 'x --termd-ns=wall'",
        "agent send helper --backend-configs",
        "agent send helper x--backend",
        "agent list --backendx=1",
        "agent list -q",
        "agent list --quiet",
    ] {
        if let Err(e) = c.check(cmd, fx.root_str()) {
            panic!("check({cmd:?}) = {e:#}, want allowed");
        }
    }
}

#[test]
fn empty_subcommand_matches_nothing() {
    // The Go's strings.Fields("") is no words, which prefixes every line, so
    // an empty entry would allow anything (review 1.6); here it allows
    // nothing.
    let (fx, mut c) = fixture();
    for subs in [vec![""], vec!["  "], vec!["", "\t"]] {
        c.subcommands = subs.iter().map(|s| s.to_string()).collect();
        for cmd in ["agent spawn x", "agent", "agent list", "agent -q"] {
            assert!(
                c.check(cmd, fx.root_str()).is_err(),
                "subcommands {subs:?}: check({cmd:?}) allowed it"
            );
        }
    }
    // An empty entry next to a real one changes nothing about the real one.
    c.subcommands = vec![String::new(), "list".to_string()];
    c.check("agent list", fx.root_str()).unwrap();
    assert!(c.check("agent spawn x", fx.root_str()).is_err());
}

#[test]
fn line_continuations_between_words() {
    let (fx, c) = fixture();
    for cmd in [
        "agent \\\nlist",
        "agent wait helper-api helper-db \\\n  --for idle,waiting --next --timeout 30m",
        "agent send helper \\\n  \"run the tests\"",
        "agent -q \\\n\tlist \\\n  --json",
        "agent friction add \"summary\" \\\n  --kind bug",
    ] {
        if let Err(e) = c.check(cmd, fx.root_str()) {
            panic!("check({cmd:?}) = {e:#}, want allowed");
        }
    }
    for cmd in [
        "\\\nagent list",
        "agent list \\\n",
        "agent list \\\n\nrm x",
        "agent list \\\n; rm x",
        "agent\\\nlist",
        "agent \\\r\nlist",
        "agent list 2>&1 \\\n--json",
        "agent list \\\n2>&1",
        "agent \\\n\\\nlist",
        "agent \\\n#x\nlist",
        "agent send x \\\n=ls",
        "agent \\\nspawn x",
    ] {
        assert!(
            c.check(cmd, fx.root_str()).is_err(),
            "check({cmd:?}) allowed it, want refused"
        );
    }
}

// Adversarial lines, each refused; the comment names what it guards.
#[rustfmt::skip]
const ADVERSARIAL: &[&str] = &[
    // Chains and lists in every spelling.
    "agent list;rm x",                // ; with no blanks
    "agent list ; rm x",              // ; with blanks
    "agent list;;",                   // an empty command after ;
    ";agent list",                    // a leading ;
    "agent list\n;",                  // ; on a line of its own
    "agent list; ;",                  // two trailing ;
    "agent list && agent list",       // && even between safe commands
    "agent list||true",               // || with no blanks
    "agent list|sh",                  // | with no blanks
    "agent list&rm x",                // & with no blanks
    "agent list 2&>/dev/null",        // fd digits run into &
    "agent list 2>&1|sh",             // a pipe after a redirection
    "agent list>/dev/null|sh",        // a pipe right after a target
    "agent list\n\nrm x",             // a second command after a blank line
    "agent list;\nrm x",              // a second command after ; and newline
    "agent list\rrm x",               // a lone CR
    "agent list\u{2028}rm x",         // U+2028 isn't a newline to the shell
    "agent\u{0b}list",                // a vertical tab isn't a blank
    "agent\u{0c}list",                // nor is a form feed
    // Substitutions and expansions.
    "agent send x \"a$(id)\"",        // $( inside double quotes
    "agent send x a`id`b",            // backticks mid-word
    "agent send x \"${HOME}\"",       // ${ inside double quotes
    "agent send x \"$1\"",            // a positional parameter
    "agent send x \"$-\"",            // a special parameter ($-)
    "agent send x \"$=x\"",           // zsh's $= word splitting
    "agent send x \"a$\\\"\"",        // $ then an escaped quote ($\ isn't plain)
    "agent send x \"a\\\\$(id)\"",    // an escaped backslash, then a real $(
    "agent send x \"\\\\\"; rm x; \"\"", // "\\" closes the quote, then a chain
    "agent send x $[1+1]",            // old-style arithmetic
    "agent send x =(id)",             // zsh's =(...) process substitution
    "agent list > >(sh)",             // output process substitution as a target
    "agent send x $'a'",              // ANSI-C quoting
    "agent $'list'",                  // ANSI-C quoting in the subcommand
    "agent send x $\"a\"",            // locale quoting
    // Assignments and variables.
    "a+=1 agent list",                // an appending assignment
    "'A=1' agent list",               // = in the first word, even quoted
    ">/dev/null A=1 agent list",      // an assignment after a leading redirection
    "agent=x list",                   // = anywhere in the first word
    "$PWD/bin/agent list",            // a variable in the program path
    "~/bin/agent list",               // a tilde in the program path
    "agent send x a=~/f",             // a tilde after =
    "agent send x --to=~",            // a tilde after = at the end
    // Globs, brace and tilde expansion.
    "agent l?st",                     // ? in the subcommand
    "agent send x a*",                // * mid-word
    "agent send x **/f",              // zsh/bash globstar
    "agent {list,spawn}",             // brace expansion of the subcommand
    "agent send x a]",                // a lone ]
    "agent send x }",                 // a lone }
    "agent send x ~+",                // ~+ is $PWD
    "agent send x !(a)",              // extglob !(...)
    "agent send x +(a)",              // extglob +(...)
    "agent send x a^b",               // zsh extendedglob ^
    "agent send x x~y",               // zsh extendedglob ~
    // Redirections and here-documents.
    "agent list >/dev/nul",           // not quite /dev/null
    "agent list >/dev/../dev/null",   // the same file by another name
    "agent list > /dev/null.bak",     // /dev/null as a prefix
    "agent list 0>/dev/null",         // fd 0
    "agent list 12>/dev/null",        // a two-digit fd
    "agent list 01>&2",               // fd 1 spelled 01
    "agent list >&-",                 // closing stdout
    "agent list 2>&1-",               // moving a descriptor
    "agent list <>/dev/null",         // read-write open
    "agent list 0</dev/null",         // any input redirection
    "agent list <&0",                 // input dup
    "agent list <<-EOF\nx\nEOF",      // a tab-stripping here-doc
    "agent list >>&/dev/null",        // zsh's append-both
    "agent list >! /dev/null",        // zsh's clobber
    "agent list >>| out",             // zsh's append-clobber
    "agent list &>>out",              // append-both to a file
    "agent list >&/dev/null",         // >& with a file name
    "agent list 2>&1>out",            // a target run into another redirection
    "agent list >",                   // a missing target
    "agent list 2>",                  // a missing target after an fd
    "{fd}>/dev/null agent list",      // a named fd
    // Comments.
    "agent list #",                   // a bare #
    "#!/bin/sh\nagent list",          // a shebang line
    // Backslashes outside quotes.
    "agent send x a\\;rm",            // an escaped ;
    "\\agent list",                   // a backslash before the program
    "agent send x \\'",               // an escaped quote
    // History expansion.
    "agent send x !-1",               // !-1
    "^agent^rm list",                 // quick substitution
    // Control characters and NUL.
    "agent list\0; rm x",             // a NUL a C string would end at
    "agent send x 'a\0b'",            // a NUL inside quotes
    "agent send x \"\u{7f}\"",        // DEL inside quotes
    "agent send x \"\u{85}\"",        // a C1 control inside quotes
    // Unicode look-alikes and whitespace.
    "agent\u{a0}list",                // a no-break space as a separator
    "agent list\u{3000}",             // an ideographic space at the end
    "agent send x \u{201c}hi; rm x\u{201d}", // curly quotes aren't quotes
    "\u{430}gent list",               // a Cyrillic а in the program
    "agent \u{ff4c}ist",              // a fullwidth l in the subcommand
    "agent send x \u{ff07}a\u{ff07}", // fullwidth apostrophes
    "agent\u{200b} list",             // a zero-width space
    "\u{feff}agent list",             // a byte-order mark
    // Empty words and --.
    "'' agent list",                  // an empty first word
    "agent -- list",                  // -- before the subcommand
    "agent -q -- list",               // -- after a leading flag
    "agent -qq list",                 // stacked short flags
    "agent --q list",                 // a flag that only looks like -q
    // Keywords, builtins and precommand modifiers as the first word.
    "command -p agent list",          // command
    "time -p agent list",             // time
    "nocorrect agent list",           // zsh nocorrect
    "noglob agent list",              // zsh noglob
    "coproc agent list",              // coproc
    "( agent list )",                 // a subshell
    "- agent list",                   // zsh's - precommand
    "eval agent list",                // eval
    ". agent list",                   // source by .
    "[[ agent ]]",                    // a test
    // How the program resolves.
    "./bin/agent.x list",             // a neighbouring name
    "/bin/agent list",                // an absolute path elsewhere
    "agent2 list",                    // a longer name
    "AGENT list",                     // another case
    "up/../../bin/agent list",        // physical path is the program, textual isn't
    // Subcommand tricks.
    "agent 'state get'x",             // the two words run together
    "agent state\tset",               // a tab between words of a refused subcommand
    "agent '' send",                  // an empty word before the subcommand
    "agent sendx",                    // a prefix of a longer subcommand
];

#[test]
fn check_refuses_adversarial() {
    assert!(ADVERSARIAL.len() >= 60);
    let (fx, c) = fixture();
    for cmd in ADVERSARIAL {
        if c.check(cmd, fx.root_str()).is_ok() {
            panic!("check({cmd:?}) allowed it, want refused");
        }
    }
}
