//! The baselines, as the Python harness ran them: `rg` on a plain query's
//! words (files ranked by distinct words matched, then matching lines), `rg`
//! on an agent's own pattern (regex, then grep's basic syntax, then fixed),
//! and `fd` for a glob. Over the tree they skip what the index never reads.

use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use std::time::{Duration, Instant};

use super::paths::{Paths, inside};
use super::words::{GLOB_KINDS, bre_to_ere};

/// run_eval.py's list of what `agent index crawl` never reads.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pixi",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "dist",
    "build",
    ".gomuxai",
    ".cache",
];

/// One system's answer to one query.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    /// Canonical files, best first, at most the limit.
    pub top: Vec<String>,
    /// How many files it found before the cut.
    pub found: usize,
    pub secs: f64,
    pub mode: String,
    /// The index's own milliseconds (the sum of its stages).
    pub own_ms: Option<f64>,
}

pub struct Baselines<'a> {
    pub paths: &'a Paths,
    pub limit: usize,
    pub max_words: usize,
    pub timeout: Duration,
    /// rg's flags that hold it to what the index reads.
    pub like_index: Vec<String>,
    word_cache: HashMap<(String, String), (BTreeMap<String, u64>, f64)>,
    pattern_cache: HashMap<(String, String, String, bool), Answer>,
}

impl<'a> Baselines<'a> {
    pub fn new(paths: &'a Paths, limit: usize, max_words: usize, extra_skips: &[String]) -> Self {
        let mut like_index = vec!["--max-filesize".to_string(), "1M".to_string()];
        for d in SKIP_DIRS
            .iter()
            .map(|d| d.to_string())
            .chain(extra_skips.iter().cloned())
        {
            like_index.push("-g".into());
            like_index.push(format!("!**/{d}/"));
        }
        Baselines {
            paths,
            limit,
            max_words,
            timeout: Duration::from_secs(120),
            like_index,
            word_cache: HashMap::new(),
            pattern_cache: HashMap::new(),
        }
    }

    /// Runs a command with the timeout; (stdout, exit code, seconds).
    fn run(&self, argv: &[String]) -> (String, i32, f64) {
        let t0 = Instant::now();
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return (String::new(), 127, t0.elapsed().as_secs_f64());
        };
        let mut out = Vec::new();
        let mut stdout = child.stdout.take();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = std::io::Read::read_to_end(s, &mut buf);
            }
            buf
        });
        let code = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.code().unwrap_or(1),
                Ok(None) if t0.elapsed() > self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break 124;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break 1,
            }
        };
        if let Ok(b) = reader.join() {
            out = b;
        }
        if code == 124 {
            out.clear();
        }
        (
            String::from_utf8_lossy(&out).into_owned(),
            code,
            t0.elapsed().as_secs_f64(),
        )
    }

    /// `rg -c`: file to matching lines, exit code, seconds.
    fn counts(&self, flags: &[String], pat: &str, root: &str) -> (BTreeMap<String, u64>, i32, f64) {
        let mut argv = vec!["rg".to_string(), "-c".into(), "--no-messages".into()];
        argv.extend(flags.iter().cloned());
        argv.extend(["-e".to_string(), pat.to_string(), root.to_string()]);
        let (out, code, dt) = self.run(&argv);
        let mut c = BTreeMap::new();
        for line in out.lines() {
            if let Some((f, n)) = line.rsplit_once(':')
                && !f.is_empty()
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
            {
                c.insert(f.to_string(), n.parse().unwrap_or(0));
            }
        }
        (c, code, dt)
    }

    /// Ranked paths, each mapped to its main checkout, the first copy kept,
    /// excluded ones passed over, cut to the limit.
    pub fn top(&self, ranked: &[String], exclude: &[String]) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for f in ranked {
            let c = self.paths.canon(f);
            if seen.contains(&c) || exclude.iter().any(|x| inside(&c, x)) {
                continue;
            }
            seen.insert(c.clone());
            out.push(c);
            if out.len() >= self.limit {
                break;
            }
        }
        out
    }

    fn rg_pattern(&self, kind: &str, pat: &str, root: &str, flags: &[String]) -> Answer {
        let mut tries: Vec<(&str, Vec<String>, String)> = vec![("regex", vec![], pat.to_string())];
        let basic = bre_to_ere(pat);
        if kind != "rg" && basic != pat {
            tries.push(("basic", vec![], basic));
        }
        tries.push(("fixed", vec!["-F".into()], pat.to_string()));
        let (mut total, mut counts, mut mode) = (0.0, BTreeMap::new(), "regex");
        for (m, how, p) in &tries {
            let mut f: Vec<String> = flags.to_vec();
            f.extend(how.iter().cloned());
            let (c, code, dt) = self.counts(&f, p, root);
            total += dt;
            counts = c;
            mode = m;
            if !counts.is_empty() || code == 124 {
                break;
            }
        }
        let mut ranked: Vec<String> = counts.keys().cloned().collect();
        ranked.sort_by(|a, b| counts[b].cmp(&counts[a]).then(a.cmp(b)));
        Answer {
            top: self.top(&ranked, &[]),
            found: counts.len(),
            secs: total,
            mode: mode.into(),
            own_ms: None,
        }
    }

    fn fd_pattern(&self, kind: &str, pat: &str, root: &str, like_index: bool) -> Answer {
        let mut argv: Vec<String> = vec!["fd".into(), "-t".into(), "f".into()];
        if like_index {
            argv.extend(["--size".to_string(), "-1mi".into()]);
            for d in SKIP_DIRS {
                argv.extend(["--exclude".to_string(), d.to_string()]);
            }
        }
        let mut pat = pat.to_string();
        if kind != "fd" {
            argv.push("--glob".into());
            if pat.contains('/') {
                argv.push("--full-path".into());
                if !pat.starts_with('*') && !pat.starts_with('/') {
                    pat = format!("**/{}", pat.trim_start_matches(['.', '/']));
                }
            }
        }
        argv.extend(["--".to_string(), pat, root.to_string()]);
        let (out, _, dt) = self.run(&argv);
        let mut files: Vec<String> = out.lines().map(str::to_string).collect();
        let found = files.len();
        files.sort_by(|a, b| {
            a.matches('/')
                .count()
                .cmp(&b.matches('/').count())
                .then(a.cmp(b))
        });
        Answer {
            top: self.top(&files, &[]),
            found,
            secs: dt,
            mode: "fd".into(),
            own_ms: None,
        }
    }

    /// The agent's own pattern: in the project as the agent ran it; over the
    /// tree (`like_index`) skipping what the index skips.
    pub fn rg_pattern_at(&mut self, kind: &str, pat: &str, root: &str, like_index: bool) -> Answer {
        let key = (
            kind.to_string(),
            pat.to_string(),
            root.to_string(),
            like_index,
        );
        if let Some(a) = self.pattern_cache.get(&key) {
            return a.clone();
        }
        let a = if GLOB_KINDS.contains(&kind) {
            self.fd_pattern(kind, pat, root, like_index)
        } else {
            let flags = if like_index {
                self.like_index.clone()
            } else {
                vec![]
            };
            self.rg_pattern(kind, pat, root, &flags)
        };
        self.pattern_cache.insert(key, a.clone());
        a
    }

    /// `rg -i -w -F` for each word (the first `max_words`), skipping what
    /// the index skips; files ranked by distinct words matched, then by
    /// matching lines. A word's result is reused, and its first cost charged
    /// to every query that uses it.
    pub fn rg_words(&mut self, words: &[String], root: &str) -> Answer {
        let mut per_file: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut total = 0.0;
        for w in words.iter().take(self.max_words) {
            let key = (root.to_string(), w.to_lowercase());
            if !self.word_cache.contains_key(&key) {
                let mut flags: Vec<String> = vec!["-i".into(), "-F".into(), "-w".into()];
                flags.extend(self.like_index.iter().cloned());
                let (c, _, dt) = self.counts(&flags, w, root);
                self.word_cache.insert(key.clone(), (c, dt));
            }
            let (c, dt) = &self.word_cache[&key];
            total += dt;
            for (f, n) in c {
                let e = per_file.entry(f.clone()).or_default();
                e.0 += 1;
                e.1 += n;
            }
        }
        let mut ranked: Vec<String> = per_file.keys().cloned().collect();
        ranked.sort_by(|a, b| {
            let (x, y) = (per_file[a], per_file[b]);
            y.0.cmp(&x.0).then(y.1.cmp(&x.1)).then(a.cmp(b))
        });
        Answer {
            top: self.top(&ranked, &[]),
            found: per_file.len(),
            secs: total,
            mode: "words".into(),
            own_ms: None,
        }
    }
}
