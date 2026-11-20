// Go tests/util: ansi_test.go, shell_test.go, suggest_test.go.

use agent_base::gostd::strconv::quote;
use agent_base::util;

#[test]
fn test_strip_ansi() {
    let tests = [
        ("no ansi codes", "hello world", "hello world"),
        ("color codes", "\x1b[32mgreen\x1b[0m text", "green text"),
        ("cursor movement", "\x1b[2J\x1b[Hstart", "start"),
        (
            "complex sequence",
            "\x1b[1;31;40mBold Red on Black\x1b[0m",
            "Bold Red on Black",
        ),
        ("empty string", "", ""),
        ("only ansi codes", "\x1b[0m\x1b[32m\x1b[0m", ""),
    ];
    for (name, input, expected) in tests {
        assert_eq!(
            util::strip_ansi(input),
            expected,
            "{name}: strip_ansi({input:?})"
        );
    }
}

#[test]
fn test_sanitize_terminal() {
    let tests = [
        ("plain text", "hello world", "hello world"),
        ("keeps newline and tab", "a\tb\nc", "a\tb\nc"),
        ("unicode", "❯ héllo", "❯ héllo"),
        (
            "drops escape, keeps its printable tail",
            "\x1b[31mred\x1b[0m",
            "[31mred[0m",
        ),
        ("drops carriage return", "fake\rreal", "fakereal"),
        ("drops bell and backspace", "a\x07b\x08c", "abc"),
        ("drops DEL", "a\x7fb", "ab"),
        ("drops C1 CSI", "a\u{009b}31mb", "a31mb"),
    ];
    for (name, input, expected) in tests {
        assert_eq!(
            util::sanitize_terminal(input),
            expected,
            "{name}: sanitize_terminal({input:?})"
        );
    }
}

#[test]
fn test_sanitize_styled() {
    let tests = [
        ("plain text", "a\tb\nc", "a\tb\nc"),
        (
            "keeps SGR",
            "\x1b[1;38;5;196mred\x1b[0m \x1b[2mdim\x1b[m",
            "\x1b[1;38;5;196mred\x1b[0m \x1b[2mdim\x1b[m",
        ),
        (
            "keeps colon SGR",
            "\x1b[38:2::1:2:3mx",
            "\x1b[38:2::1:2:3mx",
        ),
        ("drops title", "a\x1b]0;pwned\x07b", "a]0;pwnedb"),
        (
            "drops cursor moves and clears",
            "a\x1b[2J\x1b[Hb",
            "a[2J[Hb",
        ),
        ("drops private modes", "a\x1b[?1049hb", "a[?1049hb"),
        ("drops C1 CSI", "a\u{009b}31mb", "a31mb"),
        ("drops carriage return", "fake\rreal", "fakereal"),
    ];
    for (name, input, expected) in tests {
        assert_eq!(
            util::sanitize_styled(input),
            expected,
            "{name}: sanitize_styled({input:?})"
        );
    }
}

#[test]
fn test_terminal_lines() {
    let pad512 = format!("a{}b", " ".repeat(512));
    let tests: Vec<(&str, &str, Vec<&str>)> = vec![
        ("empty", "", vec![]),
        ("plain lines", "one\ntwo\n", vec!["one", "two"]),
        (
            "carriage return breaks the line",
            "one\rtwo\r\nthree",
            vec!["one", "two", "three"],
        ),
        (
            "cursor forward becomes spaces",
            "\x1b[1mBash\x1b[22m(run\x1b[1Cthe\x1b[Ctests\x1b[3Cnow)",
            vec!["Bash(run the tests   now)"],
        ),
        (
            // Claude's TUI writes "  ⎿  Sent message to ..." this way.
            "cursor to column pads to it",
            "\x1b[38;5;246m  ⎿  \x1b[39mSent\x1b[11Gmessage\x1b[19Gto\x1b[22G\"air1\"",
            vec!["  ⎿  Sent message to \"air1\""],
        ),
        (
            "cursor to an earlier column is dropped",
            "abcdef\x1b[2Gx",
            vec!["abcdefx"],
        ),
        (
            "cursor down keeps the column",
            "filepath.Join(tmpDir,\x1b[K\r\x1b[2C\x1b[1B   \"logs\"),",
            vec!["filepath.Join(tmpDir,", r#"     "logs"),"#],
        ),
        (
            "cursor position starts a line at its column",
            "top\x1b[22;3Hprompt",
            vec!["top", "  prompt"],
        ),
        (
            "huge cursor forward is capped",
            "a\x1b[999999999Cb",
            vec![pad512.as_str()],
        ),
        (
            "OSC sequences",
            "\x1b]0;✳ Claude Code\x07title\x1b]3008;start=3c07;type=shell\x1b\\mark\x1b]7501;cut off",
            vec!["titlemark"],
        ),
        (
            "private modes, charsets, and save/restore",
            "\x1b[?2026h\x1b[?25l\x1b(B\x1b7text\x1b8\x1b[?25h\x1b[>4;2m\x1b[2 q",
            vec!["text"],
        ),
        ("sequence cut off at the end", "done\x1b[38;5", vec!["done"]),
        ("other controls", "a\x07b\x08c\x7fd\u{009b}e", vec!["abcde"]),
        (
            "blank lines and trailing spaces",
            "one   \r\n\r\n   \x1b[K\r\ntwo\x1b[5C\n",
            vec!["one", "two"],
        ),
        (
            "redraws collapse",
            concat!(
                "\x1b[H\r\x1b[18B✻ Thinking…\x1b[24;1H",
                "\x1b[H\r\x1b[18B✻ Thinking…\x1b[24;1H",
                "\x1b[H\r\x1b[18B✶ Thinking…\x1b[24;1H",
                "\x1b[H\r\x1b[18B✻ Thinking…",
            ),
            vec!["✻ Thinking…", "✶ Thinking…", "✻ Thinking…"],
        ),
    ];
    for (name, input, expected) in tests {
        assert_eq!(
            util::terminal_lines(input),
            expected,
            "{name}: terminal_lines({input:?})"
        );
    }
}

#[test]
fn test_shell_join() {
    let tests: Vec<(Vec<&str>, &str)> = vec![
        (vec![], ""),
        (
            vec![
                "wait",
                "helper@hub",
                "--for",
                "idle,waiting",
                "--timeout=10m",
            ],
            "wait helper@hub --for idle,waiting --timeout=10m",
        ),
        (
            vec!["send", "helper", "review auth.go"],
            "send helper 'review auth.go'",
        ),
        (
            vec!["friction", "add", "it's broken", ""],
            r"friction add 'it'\''s broken' ''",
        ),
        (vec!["keys", "s", "$HOME", "a*b"], "keys s '$HOME' 'a*b'"),
    ];
    for (words, want) in tests {
        assert_eq!(util::shell_join(&words), want, "shell_join({words:?})");
    }
}

// What mvdan.cc/sh's shell.Fields gives (with every variable unset), which
// the Go's SplitShellWords returns; recorded from the Go.
#[test]
fn test_split_shell_words() {
    let tests: Vec<(&str, Vec<&str>)> = vec![
        ("/usr/bin/agent hook", vec!["/usr/bin/agent", "hook"]),
        ("'/a b/agent' hook", vec!["/a b/agent", "hook"]),
        ("\"/a b/agent\" hook", vec!["/a b/agent", "hook"]),
        (r"a\ b c", vec!["a b", "c"]),
        ("$HOME/x hook", vec!["/x", "hook"]),
        ("${HOME}/x", vec!["/x"]),
        ("${X:-def} y", vec!["def", "y"]),
        ("~/bin/agent hook", vec!["~/bin/agent", "hook"]),
        ("~ x", vec!["~", "x"]),
        ("~user/x", vec!["~user/x"]),
        ("a{b,c}d", vec!["abd", "acd"]),
        ("{1..3}", vec!["1", "2", "3"]),
        ("a;b", vec![]),
        ("a | b", vec![]),
        ("a && b", vec![]),
        ("echo $(ls)", vec![]),
        ("echo `ls`", vec![]),
        ("a # comment", vec!["a"]),
        ("a#b", vec!["a#b"]),
        (r#""a\"b\$c\\d\e""#, vec![r#"a"b$c\d\e"#]),
        (r"'it'\''s'", vec!["it's"]),
        ("", vec![]),
        ("   ", vec![]),
        ("x \"\" y", vec!["x", "", "y"]),
        ("x $E y", vec!["x", "y"]),
        ("x \"$E\" y", vec!["x", "", "y"]),
        ("a=b c", vec!["a=b", "c"]),
        ("a > f", vec![]),
        ("\"unterminated", vec![]),
        ("a\\", vec!["a"]),
        ("a(b", vec![]),
        ("echo $1 $@ $#", vec!["echo"]),
        (r"$'a\nb'", vec!["a\nb"]),
        ("a\\\nb", vec!["ab"]),
        ("a\nb", vec!["a", "b"]),
        ("*.go", vec!["*.go"]),
        ("a{b}c", vec!["a{b}c"]),
        ("{a,b}{c,d}", vec!["ac", "ad", "bc", "bd"]),
        ("\"{a,b}\"", vec!["{a,b}"]),
        ("$E{a,b}", vec!["a", "b"]),
        ("x$", vec!["x$"]),
        ("x$E\"y\"", vec!["xy"]),
        (r"a\$b", vec!["a$b"]),
        ("a!b", vec!["a!b"]),
        ("[ab]", vec!["[ab]"]),
        ("a=~/x", vec!["a=~/x"]),
        ("x ~/y", vec!["x", "~/y"]),
        ("~", vec!["~"]),
        ("~/", vec!["~/"]),
        ("${X:=d}", vec![]),
        ("\"${X:-a b}\"", vec!["a b"]),
        ("${X:-a b}", vec!["a", "b"]),
        ("${#X}", vec!["0"]),
        ("x${E:-a b}y", vec!["xa", "by"]),
        ("${X:?m}", vec![]),
        ("${X:+y}", vec![]),
        ("{a..c}", vec!["a", "b", "c"]),
        ("{3..1}", vec!["3", "2", "1"]),
        ("{1..10..3}", vec!["1", "4", "7", "10"]),
        ("{a,{b,c}}", vec!["a", "b", "c"]),
        ("$\"x\"", vec!["x"]),
        ("'$E'", vec!["$E"]),
        ("${X:-\"a b\"}", vec!["a", "b"]),
        ("${X:-$Y}", vec![]),
        ("{,}", vec!["", ""]),
        ("x{,}", vec!["x", "x"]),
        ("{01..03}", vec!["01", "02", "03"]),
        ("${X%%a}", vec![]),
        ("${}", vec![]),
        (r"a\x", vec!["ax"]),
        (r#""a\x""#, vec![r"a\x"]),
        (concat!(r"$'\x41\101\", "u00e9'"), vec!["AA\u{e9}"]),
        ("{a..c..2}", vec!["a", "c"]),
        ("{-2..2}", vec!["-2", "-1", "0", "1", "2"]),
        ("{a,b", vec!["{a,b"]),
        ("a}b,c", vec!["a}b,c"]),
        ("${X:-}", vec![]),
        ("\"${X:-}\"", vec![""]),
        ("a&b", vec![]),
        ("a b #c\nd", vec!["a", "b", "d"]),
    ];
    for (line, want) in tests {
        assert_eq!(
            util::split_shell_words(line),
            want,
            "split_shell_words({line:?})"
        );
    }
}

// The review's cases (rs/reports/review-1.md, 3.3 to 3.8) and their
// neighbours, with what the Go's SplitShellWords gives for each (recorded by
// running it); none may panic.
#[test]
fn test_split_shell_words_review() {
    let tests: Vec<(&str, Vec<&str>)> = vec![
        // 3.3: brace sequences that overflow an i64.
        ("echo {1..2..-9223372036854775808}", vec!["echo", "1", "2"]),
        (
            "echo {1..2..9223372036854775807}",
            vec!["echo", "1", "-9223372036854775808", "-1"],
        ),
        (
            "echo {9223372036854775807..9223372036854775806}",
            vec!["echo", "9223372036854775807", "9223372036854775806"],
        ),
        (
            "echo {-9223372036854775808..-9223372036854775807}",
            vec!["echo", "-9223372036854775808", "-9223372036854775807"],
        ),
        // The Go never returns (it counts up from i64::MIN); here it doesn't
        // parse.
        ("echo {9223372036854775806..9223372036854775807..2}", vec![]),
        // 3.4: a quoted } inside ${...}.
        (r#"echo ${X:-"a}b"}"#, vec!["echo", "a}b"]),
        (r#"echo "${X:-'a}b'}""#, vec!["echo", "a}b"]),
        (r#"echo "${X:-'a b'}""#, vec!["echo", "a b"]),
        (r#"echo "${X:-"a b"}""#, vec!["echo", "a b"]),
        ("echo ${X:-'a b'}", vec!["echo", "a", "b"]),
        (r#"echo ${X:-${Y:-"}"}}"#, vec!["echo", "}"]),
        (r#"echo "${X:-$'a}b'}""#, vec!["echo", "a}b"]),
        // Outside quotes, a backslash in the word stays.
        (r"echo ${X:-a\ b}", vec!["echo", r"a\", "b"]),
        (r#"echo "${X:-a\}b}""#, vec!["echo", r"a\}b"]),
        // 3.5: a line continuation in a default word inside "...".
        ("echo \"${X:-a\\\nb}\"", vec!["echo", "ab"]),
        // 3.6: forms mvdan refuses.
        ("echo ${1abc}", vec![]),
        ("echo ${!X}", vec![]),
        ("echo ${#X-a}", vec![]),
        ("echo ${X*}", vec![]),
        ("echo ${X:}", vec![]),
        ("echo ${X:1+}", vec![]),
        ("echo ${X[}", vec![]),
        ("echo ${$X}", vec![]),
        ("echo ${X@Z}", vec![]),
        ("echo ${X:+$(ls)}", vec![]),
        // ... and ones it takes.
        ("echo ${#-x}", vec!["echo", "x"]),
        ("echo ${#!}", vec!["echo", "0"]),
        ("echo ${##}", vec!["echo", "0"]),
        ("echo ${!X@}", vec!["echo"]),
        ("echo ${X: -1}", vec!["echo"]),
        ("echo ${X:1:2}", vec!["echo"]),
        ("echo ${X/a/b/c}", vec!["echo"]),
        ("echo ${X[0]:-a}", vec!["echo", "a"]),
        ("echo ${X@Q}", vec!["echo", "''"]),
        ("echo \"${X@A}\"", vec!["echo", "X=''"]),
        // 3.7: $'...' ends at a NUL.
        (r"echo $'a\x00b'", vec!["echo", "a"]),
        (r"echo $'a\0b'", vec!["echo", "a"]),
        (r"echo x$'a\0b'y", vec!["echo", "xay"]),
        (r"echo $'\u0000x'", vec!["echo", ""]),
        // 3.8: brace sequences.
        ("echo {1..3..-2}", vec!["echo", "1", "2", "3"]),
        ("echo {3..1..2}", vec!["echo", "3", "2", "1"]),
        ("echo {3..1..-2}", vec!["echo", "3", "1"]),
        (
            "echo {-01..03}",
            vec!["echo", "0-1", "00", "01", "02", "03"],
        ),
        (
            "echo {01..10}",
            vec![
                "echo", "01", "02", "03", "04", "05", "06", "07", "08", "09", "010",
            ],
        ),
        ("echo {00..2}", vec!["echo", "0", "1", "2"]),
        (
            "echo {Z..a}",
            vec!["echo", "Z", "[", "", "]", "^", "_", "`", "a"],
        ),
        (
            "echo x{Z..a}y",
            vec![
                "echo", "xZy", "x[y", "xy", "x]y", "x^y", "x_y", "x`y", "xay",
            ],
        ),
        ("echo {1..5..2..x}", vec!["echo", "1", "3", "5"]),
        ("echo {1..2..3..4}", vec!["echo", "1"]),
        ("echo {1..5..x}", vec!["echo", "{1..5..x}"]),
        // How mvdan's lexer reads: CR LF is LF, a lone CR splits words
        // outside quotes, NUL goes, a lone trailing backslash is a word.
        ("echo a\r\nb", vec!["echo", "a", "b"]),
        ("echo a\rb", vec!["echo", "a", "b"]),
        ("echo 'a\r\nb'", vec!["echo", "a\nb"]),
        ("echo \"a\rb\"", vec!["echo", "a\rb"]),
        ("echo a\0b", vec!["echo", "ab"]),
        ("echo \\", vec!["echo", ""]),
        ("echo a \\", vec!["echo", "a", ""]),
        // A name read across a line continuation; ${ isn't.
        ("echo [0]$X\\\na", vec!["echo", "[0]"]),
        ("echo $\\\nb", vec!["echo"]),
        ("echo $\\\n{X}", vec!["echo", "${X}"]),
        // 3.9, left as it is: the Go gives ["echo", "3"].
        ("echo $((1+2))", vec![]),
    ];
    for (line, want) in tests {
        assert_eq!(
            util::split_shell_words(line),
            want,
            "split_shell_words({line:?})"
        );
    }

    // 3.3: a step of i64::MAX wraps around, as Go's int does: a, then
    // every character from ` down to NUL (a backslash on its own goes), then
    // U+FFFD for -1.
    let mut want = vec!["echo".to_string(), "a".to_string()];
    for n in (0..=96u8).rev() {
        want.push(if n == b'\\' {
            String::new()
        } else {
            char::from(n).to_string()
        });
    }
    want.push("\u{fffd}".to_string());
    assert_eq!(
        util::split_shell_words("echo {a..c..9223372036854775807}"),
        want
    );
}

#[test]
fn test_similar() {
    let names = [
        "helper",
        "helper2",
        "builder",
        "reviewer",
        "lead-build",
        "Helper3",
    ];
    let tests: Vec<(&str, Vec<&str>)> = vec![
        ("helpr", vec!["helper", "helper2", "Helper3"]),
        ("helper", vec!["helper2", "Helper3"]), // itself excluded
        ("lead", vec!["lead-build"]),           // a prefix
        ("zzz", vec![]),
        ("b", vec![]),  // a one-letter prefix is too loose
        ("le", vec![]), // so is two
    ];
    for (name, want) in tests {
        assert_eq!(util::similar(name, &names), want, "similar({name:?})");
    }
}

// Short words allow one edit, so a typo finds its command while an unrelated
// word two edits from short ones finds nothing.
#[test]
fn test_similar_scales_with_length() {
    let commands = [
        "spawn", "list", "cleanup", "send", "queue", "status", "state", "wait", "logs", "bus",
        "keys", "key", "help",
    ];
    let tests: Vec<(&str, Vec<&str>)> = vec![
        ("bogus", vec![]), // two edits from logs and bus
        ("sett", vec![]),  // two edits from send
        ("lsit", vec!["list"]),
        ("stauts", vec!["status", "state"]),
        ("stat", vec!["status", "state"]),
        ("spwan", vec!["spawn"]),
        ("hlep", vec!["help"]),
        ("kye", vec!["key"]), // a swap is one edit; keys is two
        ("cleanpu", vec!["cleanup"]),
    ];
    for (name, want) in tests {
        let mut got = util::similar(name, &commands);
        got.sort();
        let mut want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(got, want, "similar({name:?})");
    }
}

#[test]
fn test_or_list() {
    let tests: Vec<(Vec<&str>, &str)> = vec![
        (vec![], ""),
        (vec!["a"], r#""a""#),
        (vec!["a", "b"], r#""a" or "b""#),
        (vec!["a", "b", "c"], r#""a", "b", or "c""#),
    ];
    for (items, want) in tests {
        assert_eq!(
            util::or_list(&items, Some(&quote)),
            want,
            "or_list({items:?})"
        );
    }
    assert_eq!(
        util::or_list(&["a", "b"], None),
        "a or b",
        "or_list unquoted"
    );
}
