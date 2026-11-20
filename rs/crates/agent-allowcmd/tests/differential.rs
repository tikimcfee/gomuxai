// Differential check against the Go legacy/internal/allowcmd: generate a few
// thousand command lines, ask both implementations, and fail if this one
// allows anything the Go refuses. Run it with
//
//     cargo test -p agent-allowcmd --test differential -- --ignored --nocapture
//
// It builds a small Go driver from a temporary directory under /var/tmp,
// placed in the legacy Go module (legacy/) by `go build -overlay`, so nothing
// is written into the repo. Without legacy/ it says so and passes.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use common::{Fixture, SAFE, fixture};

const DRIVER: &str = r#"package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/tikimcfee/gomuxai/internal/allowcmd"
)

func main() {
	root := os.Args[1]
	var safe []string
	if err := json.Unmarshal([]byte(os.Args[2]), &safe); err != nil {
		panic(err)
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
	c := allowcmd.Checker{Program: filepath.Join(root, "bin", "agent"), Subcommands: safe, LookPath: look}
	sc := bufio.NewScanner(os.Stdin)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	w := bufio.NewWriter(os.Stdout)
	for sc.Scan() {
		var cmd string
		if err := json.Unmarshal(sc.Bytes(), &cmd); err != nil {
			panic(err)
		}
		if c.Check(cmd, root) == nil {
			fmt.Fprintln(w, "1")
		} else {
			fmt.Fprintln(w, "0")
		}
	}
	w.Flush()
}
"#;

// xorshift64*: deterministic, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

#[rustfmt::skip]
const PROGS: &[&str] = &[
    "agent", "agent", "agent", "./bin/agent", "bin/agent", "./link/agent", "'agent'", "\"agent\"",
    "ag''ent", "{FULL}", "'{FULL}'", "other", "./bin/other", "up/../bin/agent",
    "up/../../bin/agent", "sub/../bin/agent", "./bin/agent/", "link/agent/../agent", "$A",
    "=agent", "A=1 agent", "\\agent", "time agent", "exec agent", "command agent", "env agent",
    "nope", "agent/", "\"\"agent", "a\"gent\"", "./bin/./agent", "..//bin/agent",
];

const FLAGS: &[&str] = &["-q", "--quiet", "--", "-qq", "--json", "-q=1", "'-q'"];

#[rustfmt::skip]
const EXTRA_SUBS: &[&str] = &[
    "state", "state set", "spawn", "friction", "friction resolve", "approve", "Send", "sendx",
    "\"state get\"", "st''ate get", "'send'", "\"list\"", "''",
];

#[rustfmt::skip]
const ARGS: &[&str] = &[
    // Plain.
    "helper", "x", "a=b", "--for", "idle,waiting", "30m", "@h", "100%", "+1", "-v", "/dev/null",
    "1", "2", "12", "--now", "--force", "a:b", "x.y-z_w", "--", "-",
    // Quoted.
    "'a b'", "\"a b\"", "\"say \\\"hi\\\"\"", "'it''s'", "\"\\$5\"", "\"a\\\\\"", "''", "\"\"",
    "'a;b|c&d'", "\"a;b|c&d\"", "\"two\nlines\"", "'two\nlines'", "\"\\\n\"", "\"a\\b\"",
    "\"!\"", "'$(id)'", "\"\\`id\\`\"", "\"é\"", "'é'", "\"é\\\"\"", "\"\t\"", "\"#\"",
    "\"a\"'b'c", "'\\'", "\"\\\\\\\"\"",
    // Expansions and other specials.
    "\"$x\"", "\"`id`\"", "\"$(id)\"", "$HOME", "*", "?", "[a]", "{a,b}", "~", "!!", "#c", "\\;",
    "$'x'", "$\"x\"", "=ls", "\"\"=ls", "a\\ b", "$", "\"$\"", "a$", "\"a$\"", "%1", "a~b",
    "x^y", "(", ")", "a#b", "\u{a0}", "é", "\"$ \"", "\"a$\tb\"", "\"$\n\"", "\"$-\"", "\"$=x\"",
    "\"$~x\"", "\"$+x\"", "\"$$\"", "\"a$\\\"\"",
    // Root flags, which the Go lets through after the subcommand.
    "--backend-config", "--backend-config=x", "--termd-ns", "--backend=claude", "'--backend'",
];

#[rustfmt::skip]
const REDIRS: &[&str] = &[
    ">/dev/null", "2>&1", ">&2", "1>&2", "2>/dev/null", "&>/dev/null", "&>>/dev/null",
    ">>/dev/null", "> /dev/null", ">& 2", ">&\"2\"", "2&>/dev/null", "> out", "3>&1", "<in",
    "<<<x", ">|/dev/null", ">\"/dev/null\"", "0>/dev/null", ">&-", "2>&1>/dev/null",
    ">'/dev/null'", "1>/dev/null", "2>>/dev/null", ">&1", "2>&2", ">&/dev/null", "> \"\"",
    "2>&1-", "<&0", ">/dev/null.x", "01>&2", "&> /dev/null",
];

#[rustfmt::skip]
const PREFIXES: &[&str] = &[
    "", "", "", "", "", " ", "\t", "\n", ";", "! ", ">/dev/null ", "2>&1 ", "# x\n", "\\\n",
];

#[rustfmt::skip]
const SUFFIXES: &[&str] = &[
    "", "", "", "", "", "", ";", " ;", "\n", ";\n", " ", " &", " | sh", "; rm x", " # c", "\n;",
    ";;", "&&x", "\n\n", "; ", " \\\n", "\r\n", "&",
];

#[rustfmt::skip]
const SEPS: &[&str] = &[
    " ", " ", " ", " ", " ", " ", "\t", "  ", "", " \\\n", " \\\n  ", "\\\n", " \\\r\n", " \\\n\\\n",
];

#[rustfmt::skip]
const MUTATIONS: &[char] = &[
    ' ', '\t', '\n', ';', '&', '|', '<', '>', '(', ')', '$', '`', '\\', '\'', '"', '*', '?', '[',
    ']', '{', '}', '~', '!', '#', '^', '=', '%', '@', '+', ',', ':', '.', '/', '-', '0', '1', '2',
    'a', 'é', '\u{a0}', '\0', '\r',
];

fn generate(rng: &mut Rng, full: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    words.push(rng.pick(PROGS).replace("{FULL}", full));
    while rng.chance(15) {
        words.push(rng.pick(FLAGS).to_string());
    }
    if rng.chance(85) {
        words.push(
            if rng.chance(85) {
                rng.pick(SAFE)
            } else {
                rng.pick(EXTRA_SUBS)
            }
            .to_string(),
        );
    }
    for _ in 0..rng.below(4) {
        words.push(rng.pick(ARGS).to_string());
    }
    for _ in 0..rng.below(3) {
        if rng.chance(40) {
            let at = 1 + rng.below(words.len());
            words.insert(at.min(words.len()), rng.pick(REDIRS).to_string());
        }
    }
    let mut line = rng.pick(PREFIXES).to_string();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            line.push_str(rng.pick(SEPS));
        }
        line.push_str(w);
    }
    line.push_str(rng.pick(SUFFIXES));
    if rng.chance(50) {
        for _ in 0..1 + rng.below(3) {
            let mut chars: Vec<char> = line.chars().collect();
            let at = rng.below(chars.len() + 1);
            let c = MUTATIONS[rng.below(MUTATIONS.len())];
            match rng.below(3) {
                0 => chars.insert(at, c),
                1 if at < chars.len() => chars[at] = c,
                _ if at < chars.len() => {
                    chars.remove(at);
                }
                _ => chars.push(c),
            }
            line = chars.into_iter().collect();
        }
    }
    line
}

// Lines worth asking the Go about every run: the refusals this port makes
// on purpose, so the report can say which the Go allows.
const SEEDS: &[&str] = &[
    "agent \\\nlist",
    "agent send x \"é\\\"; rm x; \\\"\"",
    "agent send x \"\u{1b}[2J\"",
    "agent send x 'a\rb'",
    "agent list >&\"2\"",
    "agent list >& 2",
    "agent list 2>&1>/dev/null",
    "agent send x \"\"=ls",
    "./bin/agent/ list",
    "./link/agent/../agent list",
    "up/../bin/agent list",
    "agent send x \"$\"",
    "agent send x \"5$ each, a$\tb$\nc\"",
    "agent capture\r\n",
    "agent l\0ist",
    "agent list \"$\"\"$\"",
    "agent list 2>&1 \\\n",
    "agent list 2&>/dev/null",
    "agent list&>/dev/null",
    "agent list>/dev/null",
    "agent list &>>/dev/null",
    "agent send helper &>>/dev/null sh -c 'id'",
    "agent list --backend-config x.json",
    "agent status --termd-ns=wall",
    "agent list \\\r\nrm -rf x",
    "agent list \\\n",
    "agent wait a b \\\n  --for idle",
];

/// The legacy Go module (legacy/README.md), when this checkout still has it.
fn legacy_go_root() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../legacy");
    let root = std::fs::canonicalize(root).ok()?;
    root.join("internal/allowcmd").is_dir().then_some(root)
}

fn build_driver(repo: &std::path::Path, work: &std::path::Path) -> PathBuf {
    let src = work.join("main.go");
    std::fs::write(&src, DRIVER).unwrap();
    // A directory that exists only in the overlay, inside the module, so the
    // driver may import its internal package.
    let virt = repo
        .join("cmd")
        .join("allowcmd-differential-overlay")
        .join("main.go");
    let overlay = work.join("overlay.json");
    let json = serde_json::json!({"Replace": {virt.to_str().unwrap(): src.to_str().unwrap()}});
    std::fs::write(&overlay, json.to_string()).unwrap();
    let bin = work.join("driver");
    let out = Command::new("go")
        .current_dir(repo)
        .env("TMPDIR", "/var/tmp")
        .env("GOTMPDIR", "/var/tmp")
        .args(["build", "-overlay"])
        .arg(&overlay)
        .arg("-o")
        .arg(&bin)
        .arg("./cmd/allowcmd-differential-overlay")
        .output()
        .expect("running go");
    assert!(
        out.status.success(),
        "go build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    bin
}

fn go_decisions(driver: &std::path::Path, fx: &Fixture, lines: &[String]) -> Vec<bool> {
    let mut child = Command::new(driver)
        .arg(fx.root_str())
        .arg(serde_json::to_string(SAFE).unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = String::new();
    for l in lines {
        input.push_str(&serde_json::to_string(l).unwrap());
        input.push('\n');
    }
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(out.status.success());
    let got: Vec<bool> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l == "1")
        .collect();
    assert_eq!(got.len(), lines.len());
    got
}

#[test]
#[ignore = "differential; needs go"]
fn differential_against_go() {
    let Some(repo) = legacy_go_root() else {
        eprintln!("skipping allowcmd differential test: no legacy/ Go source in this checkout");
        return;
    };
    let work = tempfile::Builder::new()
        .prefix("allowcmd-diff-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let driver = build_driver(&repo, work.path());
    let (fx, c) = fixture();
    let full = fx.path("bin/agent");

    let cases: usize = std::env::var("ALLOWCMD_DIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut seen = HashSet::new();
    let mut lines: Vec<String> = SEEDS.iter().map(|s| s.to_string()).collect();
    seen.extend(lines.iter().cloned());
    let mut tries = 0;
    while lines.len() < cases && tries < cases * 20 {
        tries += 1;
        let l = generate(&mut rng, &full);
        if seen.insert(l.clone()) {
            lines.push(l);
        }
    }

    let go = go_decisions(&driver, &fx, &lines);
    let mut both = 0;
    let mut neither = 0;
    let mut rust_only = Vec::new();
    let mut go_only = Vec::new();
    for (l, &g) in lines.iter().zip(&go) {
        match (c.check(l, fx.root_str()).is_ok(), g) {
            (true, true) => both += 1,
            (false, false) => neither += 1,
            (true, false) => rust_only.push(l),
            (false, true) => go_only.push(l),
        }
    }
    let root = fx.root_str();
    let show = |l: &str| format!("{:?}", l.replace(root, "ROOT"));
    println!(
        "differential: {} lines; both allow {both}, both refuse {neither}, \
         only Rust allows {}, only Go allows {}",
        lines.len(),
        rust_only.len(),
        go_only.len()
    );
    for l in SEEDS {
        let g = go[lines.iter().position(|x| x == l).unwrap()];
        println!(
            "  seed {}: go {}",
            show(l),
            if g { "allows" } else { "refuses" }
        );
    }
    // Why this port refused what the Go allows, by reason, with an example.
    let mut reasons: Vec<(String, usize, String)> = Vec::new();
    for l in &go_only {
        let why = format!("{:#}", c.check(l, root).unwrap_err());
        let reason = match why.split_once(": ") {
            Some((head, _)) if head.starts_with("resolving ") => {
                "resolving the program path".to_string()
            }
            _ => why,
        };
        match reasons.iter_mut().find(|(r, _, _)| *r == reason) {
            Some((_, n, _)) => *n += 1,
            None => reasons.push((reason, 1, show(l))),
        }
    }
    reasons.sort_by_key(|r| std::cmp::Reverse(r.1));
    for (reason, n, example) in &reasons {
        println!("  only Go allows, {n} refused for: {reason}; e.g. {example}");
    }
    for l in &rust_only {
        println!("  ONLY RUST ALLOWS: {}", show(l));
    }
    assert!(
        rust_only.is_empty(),
        "{} lines Rust allows and Go refuses",
        rust_only.len()
    );
}
