// Package allowcmd decides whether a shell command line is exactly one run
// of a given program with a safe subcommand, so a hook can approve it
// without a permission prompt. It errs toward "no": anything it can't prove
// safe from the text alone (a chain, an expansion, a variable, a path that
// isn't the program) is refused, and the caller's normal permission flow
// decides.
//
// The Go parses the line with a full shell parser (mvdan.cc/sh) and then
// refuses every node that isn't plain. This port has no parser: a small
// lexer accepts only the shapes it understands completely (plain words,
// '...' and "..." without expansions, a few redirections to /dev/null or
// between stdout and stderr, one trailing ";", line continuations between
// words) and refuses everything else at the first character it doesn't
// expect. It must never approve a line the Go refuses; it refuses a few the
// Go approves (see the tests' `refused_though_go_allows`), and it is
// stricter than the Go about how the shell finds the program and about root
// flags after the subcommand.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Finds a bare command name on PATH as the shell would from a working
/// directory: called with the name and that directory (see [`look_path`]).
pub type LookPathFn = Box<dyn Fn(&str, &str) -> Result<String> + Send + Sync>;

/// Checker checks command lines against one program and its safe subcommands.
pub struct Checker {
    /// The path of the binary that may run. Symlinks to it, and a bare name
    /// that look_path finds it under, count as the same program.
    pub program: String,
    /// The safe subcommands, one or two words each ("send", "state get").
    pub subcommands: Vec<String>,
    /// Finds a bare command name; None means [`look_path`].
    pub look_path: Option<LookPathFn>,
}

// LEADING_FLAGS are the root flags allowed before the subcommand. Others
// (--backend-config reads a file) are refused there.
const LEADING_FLAGS: &[&str] = &["-q", "--quiet"];

// ROOT_FLAGS are the root's persistent flags other than quiet. The CLI takes
// them after the subcommand too ("agent list --backend-config x.json" loads
// x.json and creates the store it names), so they are refused anywhere, as
// "--flag" or "--flag=value". Neither the Go's pflag nor the Rust's clap
// accepts an abbreviated long flag, so no prefix of these needs refusing.
const ROOT_FLAGS: &[&str] = &["--backend", "--backend-config", "--termd-ns"];

// root_flag: whether word is one of ROOT_FLAGS as the CLI would read it.
fn root_flag(word: &str) -> bool {
    let name = word.split_once('=').map_or(word, |(name, _)| name);
    ROOT_FLAGS.contains(&name)
}

impl Checker {
    /// A checker for program and subcommands that looks bare names up on PATH.
    pub fn new<S: AsRef<str>>(program: impl Into<String>, subcommands: &[S]) -> Self {
        Checker {
            program: program.into(),
            subcommands: subcommands.iter().map(|s| s.as_ref().to_string()).collect(),
            look_path: None,
        }
    }

    /// Ok if command, run in the directory cwd, is a single run of
    /// self.program with one of self.subcommands, and otherwise an error
    /// saying why not.
    pub fn check(&self, command: &str, cwd: &str) -> Result<()> {
        let words = literal_words(command)?;
        let Some(prog) = words.first() else {
            bail!("no command");
        };
        self.same_program(prog, cwd)?;
        let mut args = &words[1..];
        while let Some(first) = args.first()
            && LEADING_FLAGS.contains(&first.as_str())
        {
            args = &args[1..];
        }
        for sub in &self.subcommands {
            let parts: Vec<&str> = sub.split_whitespace().collect();
            // An empty subcommand would match anything; the Go would let it.
            if parts.is_empty() {
                continue;
            }
            if args.len() >= parts.len() && args.iter().zip(&parts).all(|(a, p)| a == p) {
                // Stricter than the Go, which stops looking at the safe
                // subcommand: a root flag after it still takes effect.
                if let Some(flag) = args.iter().find(|a| root_flag(a)) {
                    bail!("root flag {flag:?} after the subcommand");
                }
                return Ok(());
            }
        }
        match args.first() {
            None => bail!("no subcommand"),
            Some(a) => bail!("subcommand {a:?} isn't a safe one"),
        }
    }

    // same_program reports whether prog, as the shell would find it from
    // cwd, is self.program.
    fn same_program(&self, prog: &str, cwd: &str) -> Result<()> {
        let mut path = prog.to_string();
        if !prog.contains('/') {
            // The shell runs a keyword, builtin or (zsh) precommand modifier
            // before it looks at PATH, whatever PATH holds.
            if SHELL_WORDS.contains(&prog) {
                bail!("{prog} is a shell keyword or builtin");
            }
            let found = match &self.look_path {
                Some(look) => look(prog, cwd),
                None => look_path(prog, cwd),
            };
            path = found.map_err(|e| anyhow!("{prog} isn't on PATH: {e:#}"))?;
        }
        let mut full = PathBuf::from(&path);
        if !full.is_absolute() {
            if cwd.is_empty() || !Path::new(cwd).is_absolute() {
                bail!("relative path {path} with no working directory");
            }
            full = Path::new(cwd).join(&path);
        }
        let want = std::fs::canonicalize(&self.program)
            .with_context(|| format!("resolving {}", self.program))?;
        // The Go joins and cleans the path (so "a/../b" drops "a" by its
        // text) before resolving it; the kernel resolves "a/.." through
        // whatever "a" links to. Both have to land on the program.
        for candidate in [clean(&full), full] {
            let got = std::fs::canonicalize(&candidate)
                .with_context(|| format!("resolving {}", candidate.display()))?;
            if got != want {
                bail!("{prog} is {}, not {}", got.display(), want.display());
            }
        }
        Ok(())
    }
}

// SHELL_WORDS are the bash and zsh reserved words, builtins and precommand
// modifiers: a first word that is one of them never runs a file from PATH.
// The builtins a zsh module or bash loadable adds once loaded (from a
// person's rc file, say) are here too. The tests check the installed shells
// have none missing.
#[rustfmt::skip]
const SHELL_WORDS: &[&str] = &[
    // Reserved words (bash and zsh).
    "!", "{", "}", "[[", "]]", "case", "coproc", "do", "done", "elif", "else", "end", "esac",
    "fi", "for", "foreach", "function", "if", "in", "nocorrect", "noglob", "repeat", "select",
    "then", "time", "until", "while",
    // bash builtins.
    ".", ":", "[", "alias", "bg", "bind", "break", "builtin", "caller", "cd", "command",
    "compgen", "complete", "compopt", "continue", "declare", "dirs", "disown", "echo", "enable",
    "eval", "exec", "exit", "export", "false", "fc", "fg", "getopts", "hash", "help", "history",
    "jobs", "kill", "let", "local", "logout", "mapfile", "popd", "printf", "pushd", "pwd", "read",
    "readarray", "readonly", "return", "set", "shift", "shopt", "source", "suspend", "test",
    "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset", "wait",
    // zsh builtins bash lacks.
    "-", "autoload", "bindkey", "bye", "cap", "chdir", "clone", "compadd", "comparguments",
    "compcall", "compctl", "compdescribe", "compfiles", "compgroups", "compquote", "compset",
    "comptags", "comptry", "compvalues", "disable", "echotc", "echoti", "emulate", "float",
    "functions", "getcap", "getln", "integer", "limit", "log", "print", "private", "pushln", "r",
    "rehash", "sched", "setcap", "setopt", "stat", "ttyctl", "unfunction", "unhash", "unlimit",
    "unsetopt", "vared", "whence", "where", "which", "zcompile", "zformat", "zftp", "zle",
    "zmodload", "zparseopts", "zprof", "zpty", "zregexparse", "zsocket", "zstyle", "ztcp",
    // zsh builtins a module adds once zmodload loads it.
    "chgrp", "chmod", "chown", "example", "ln", "mkdir", "mv", "pcre_compile", "pcre_match",
    "pcre_study", "rm", "rmdir", "strftime", "sync", "syserror", "sysopen", "sysread", "sysseek",
    "syswrite", "zcurses", "zdelattr", "zf_chgrp", "zf_chmod", "zf_chown", "zf_ln", "zf_mkdir",
    "zf_mv", "zf_rm", "zf_rmdir", "zf_sync", "zgdbmpath", "zgetattr", "zlistattr", "zselect",
    "zsetattr", "zstat", "zsystem", "ztie", "zuntie",
    // bash loadable builtins (enable -f) bash's own list lacks.
    "accept", "basename", "csv", "cut", "dirname", "dsv", "fdflags", "finfo", "fltexpr",
    "getconf", "head", "id", "kv", "logname", "mkfifo", "mktemp", "pathchk", "printenv", "push",
    "realpath", "seq", "setpgid", "sleep", "strptime", "tee", "tty", "uname", "unlink", "whoami",
];

/// look_path finds file in the directories named by PATH, as Go's
/// `exec.LookPath` does, but as the shell would see them from the directory
/// cwd: the first one where it is a non-directory this process may execute.
/// A hit in a relative directory (".", or an empty entry), looked for under
/// cwd, is an error, as Go's `exec.ErrDot`. So is a PATH entry starting with
/// "~" before the hit: bash expands it to a home directory when it looks a
/// command up, and this doesn't model that.
pub fn look_path(file: &str, cwd: &str) -> Result<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    look_path_in(file, &path, cwd)
}

// look_path_in is look_path with PATH given.
fn look_path_in(file: &str, path: &OsStr, cwd: &str) -> Result<String> {
    if file.is_empty() || file.contains('/') {
        bail!("exec: {file:?}: executable file not found in $PATH");
    }
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().as_encoded_bytes().starts_with(b"~") {
            bail!(
                "exec: {file:?}: PATH entry {:?} starts with ~, which bash expands",
                dir.display()
            );
        }
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join(file);
        // A relative entry is relative to the shell's directory, not this
        // process's.
        let full = if candidate.is_absolute() {
            candidate.clone()
        } else if !cwd.is_empty() && Path::new(cwd).is_absolute() {
            Path::new(cwd).join(&candidate)
        } else {
            bail!(
                "exec: {file:?}: relative PATH entry {:?} with no working directory",
                dir.display()
            );
        };
        if is_executable(&full) {
            if !candidate.is_absolute() {
                bail!("exec: {file:?}: cannot run executable found relative to current directory");
            }
            return candidate
                .into_os_string()
                .into_string()
                .map_err(|p| anyhow!("exec: {file:?}: path {p:?} isn't UTF-8"));
        }
    }
    bail!("exec: {file:?}: executable file not found in $PATH")
}

// is_executable: a non-directory that the effective user may execute (Go's
// findExecutable uses eaccess too).
fn is_executable(path: &Path) -> bool {
    use rustix::fs::{Access, AtFlags, CWD, accessat};
    match std::fs::metadata(path) {
        Ok(m) if !m.is_dir() => accessat(CWD, path, Access::EXEC_OK, AtFlags::EACCESS).is_ok(),
        _ => false,
    }
}

// clean is Go's filepath.Clean for an absolute path: "." and empty elements
// go, and ".." removes the element before it by its text.
fn clean(path: &Path) -> PathBuf {
    let mut parts: Vec<&OsStr> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(s),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut out = PathBuf::from("/");
    out.extend(parts);
    out
}

// literal_words lexes command as one simple command and returns its words
// after quote removal. It refuses anything whose words or effect the text
// alone doesn't fix: more than one command, pipes and chains, subshells,
// assignments, expansions of any kind, globs, comments, and redirections
// other than to /dev/null or between stdout and stderr. Unquoted text is
// limited to characters that are plain in both bash and zsh.
//
// Line continuations. Outside quotes, bash and zsh delete a backslash and
// the newline right after it before they split the line into words, so
// "a \<newline>b" is "a b" and "a\<newline>b" is "ab". One is taken here
// for a blank only where deleting it and taking it for a blank are the same
// thing, and nothing around it reads differently:
//   - unquoted, with a blank before it and a word (not a redirection) both
//     before and after it, so the words on either side stay apart whatever
//     follows; inside "..." the shells delete it too (double_quoted does),
//     and inside '...' it is two plain characters;
//   - not at the start of the line, not the last thing on it (nor before a
//     ";" or newline): a continuation at the very end joins whatever the
//     caller puts after the command onto it;
//   - not two in a row, not run into a word ("agent\<newline>list" is one
//     word to the shell), not next to a redirection;
//   - a backslash before CR LF is no continuation: bash and zsh escape the
//     CR and end the command at the LF, so it stays refused (the Go's parser
//     takes it for one; this is stricter).
pub fn literal_words(command: &str) -> Result<Vec<String>> {
    if command.contains('\0') {
        bail!("has a NUL");
    }
    let mut lx = Lexer {
        s: command.chars().collect(),
        i: 0,
    };
    lx.skip(|c| matches!(c, ' ' | '\t' | '\n'));
    let mut words = Vec::new();
    // last_word: the last thing read was a word, not a redirection.
    let mut last_word = false;
    // continued: a line continuation came after it.
    let mut continued = false;
    loop {
        lx.skip(is_blank);
        // A line continuation (backslash, newline) is taken for a blank only
        // between two words, after a blank: see "Line continuations" above.
        if lx.peek() == Some('\\') && lx.peek_at(1) == Some('\n') {
            if !last_word || continued || !lx.after_blank() {
                bail!("a line continuation that isn't between two words");
            }
            lx.i += 2;
            continued = true;
            continue;
        }
        let redirection = match lx.peek() {
            None | Some('\n' | ';') => {
                if continued {
                    bail!("a line continuation that isn't between two words");
                }
                break;
            }
            Some('>' | '&') => {
                lx.redirect(None)?;
                true
            }
            Some(_) => {
                let w = lx.word()?;
                // A word of digits right before > is the redirection's file
                // descriptor ("2>&1"), not an argument.
                let fd = !w.quoted && w.text.bytes().all(|b| b.is_ascii_digit());
                match lx.peek() {
                    Some('>') if fd => {
                        lx.redirect(Some(w.text))?;
                        true
                    }
                    Some('&') if fd => bail!("not a plain command (!, &, or |&)"),
                    _ => {
                        words.push(w);
                        false
                    }
                }
            }
        };
        if redirection && continued {
            bail!("a line continuation that isn't between two words");
        }
        last_word = !redirection;
        continued = false;
    }
    // One ";" may end the command, on its line; then only blank lines.
    if lx.peek() == Some(';') {
        lx.i += 1;
    }
    lx.skip(|c| matches!(c, ' ' | '\t' | '\n'));
    if lx.peek().is_some() {
        bail!("more than one command");
    }
    // An assignment ("A=1 agent ...") comes before the command name, so any
    // = in the first word is refused, quoted or not.
    if let Some(first) = words.first()
        && first.text.contains('=')
    {
        bail!("sets variables");
    }
    Ok(words.into_iter().map(|w| w.text).collect())
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

// plain_unquoted: characters that neither bash nor zsh expands or treats
// specially in unquoted text (globs, braces, tildes, backslashes, history
// and extended-glob characters are all left out). A leading "=" is refused
// separately: zsh's =cmd is a path.
fn plain_unquoted(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)
}

// check_quoted refuses control characters inside quotes, except newline and
// tab. Nothing a person types needs them, and a NUL or CR can make what
// runs differ from what was read.
fn check_quoted(c: char) -> Result<()> {
    if c.is_control() && c != '\n' && c != '\t' {
        bail!("control character {c:?} inside quotes");
    }
    Ok(())
}

struct Word {
    text: String,
    // quoted: some part of the word was quoted.
    quoted: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum RedirOp {
    RdrOut, // >
    AppOut, // >>
    RdrAll, // &>
    DplOut, // >&
}

struct Lexer {
    s: Vec<char>,
    i: usize,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    // after_blank: the character before the current one is a space or tab.
    fn after_blank(&self) -> bool {
        self.i > 0 && is_blank(self.s[self.i - 1])
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn skip(&mut self, f: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&f) {
            self.i += 1;
        }
    }

    // word reads one word: plain unquoted characters, '...', and "..."
    // without expansions, run together. It stops before a blank, newline,
    // ";", ">" or "&", and refuses any other character.
    fn word(&mut self) -> Result<Word> {
        let mut text = String::new();
        let mut quoted = false;
        let mut parts = 0;
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\n' | ';' | '>' | '&' => break,
                '\'' => {
                    self.i += 1;
                    self.single_quoted(&mut text)?;
                    quoted = true;
                }
                '"' => {
                    self.i += 1;
                    self.double_quoted(&mut text)?;
                    quoted = true;
                }
                '$' if matches!(self.peek_at(1), Some('\'')) => bail!("$'...' quoting"),
                '$' if matches!(self.peek_at(1), Some('"')) => bail!(r#"$"..." quoting"#),
                // Stricter than the Go, which looks only at a word's first
                // part: ""=ls is refused too.
                '=' if text.is_empty() => bail!("unquoted leading = in a word"),
                c if plain_unquoted(c) => {
                    text.push(c);
                    self.i += 1;
                }
                c => bail!("unquoted {c:?}"),
            }
            parts += 1;
        }
        if parts == 0 {
            bail!("missing word");
        }
        Ok(Word { text, quoted })
    }

    // single_quoted reads up to the closing ' (the opening one is read).
    fn single_quoted(&mut self, text: &mut String) -> Result<()> {
        loop {
            match self.next() {
                None => bail!("parsing: reached EOF without closing quote '"),
                Some('\'') => return Ok(()),
                Some(c) => {
                    check_quoted(c)?;
                    text.push(c);
                }
            }
        }
    }

    // double_quoted reads up to the closing " (the opening one is read),
    // refusing any expansion, and removes the backslashes that escape a
    // character inside double quotes: before $, `, ", \, and newline (which
    // goes too). A backslash before anything else stays.
    fn double_quoted(&mut self, text: &mut String) -> Result<()> {
        let mut prev: Option<char> = None;
        loop {
            let c = self.next();
            match c {
                None => bail!(r#"parsing: reached EOF without closing quote ""#),
                Some('"') => return Ok(()),
                // A $ that nothing follows ("5$ each", "a$") is plain text in
                // bash and zsh; any other starts an expansion.
                Some('$') if matches!(self.peek(), Some(' ' | '\t' | '\n' | '"')) => {
                    text.push('$');
                }
                Some('$' | '`') => bail!("expansion inside double quotes"),
                Some('\\') => {
                    // In a locale like GBK or Big5, the shell would read the
                    // last byte of a UTF-8 character and this backslash as
                    // one character, and the quote it escapes would close
                    // the string.
                    if prev.is_some_and(|p| !p.is_ascii()) {
                        bail!("backslash after a non-ASCII character inside double quotes");
                    }
                    match self.peek() {
                        Some(e @ ('$' | '`' | '"' | '\\')) => {
                            text.push(e);
                            self.i += 1;
                        }
                        Some('\n') => self.i += 1,
                        _ => text.push('\\'),
                    }
                }
                Some(c) => {
                    check_quoted(c)?;
                    text.push(c);
                }
            }
            prev = c;
        }
    }

    // redirect reads one redirection, with fd the digits before it, and
    // refuses it unless it is harmless: >/dev/null, 2>/dev/null,
    // &>/dev/null, >>/dev/null, 2>>/dev/null, 2>&1, and >&2.
    fn redirect(&mut self, fd: Option<String>) -> Result<()> {
        if fd.as_deref().is_some_and(|n| n != "1" && n != "2") {
            bail!("has a redirection");
        }
        let op = match (self.peek(), self.peek_at(1), self.peek_at(2)) {
            // Stricter than the Go: bash before 4.0 (macOS's /bin/bash is
            // 3.2) has no &>>; it reads &> then >, a syntax error.
            (Some('&'), Some('>'), Some('>')) => bail!("has a redirection (&>>)"),
            (Some('&'), Some('>'), _) if fd.is_none() => RedirOp::RdrAll,
            (Some('&'), _, _) => bail!("not a plain command (!, &, or |&)"),
            (Some('>'), Some('>'), _) => RedirOp::AppOut,
            (Some('>'), Some('&'), _) => RedirOp::DplOut,
            (Some('>'), Some('|' | '!' | '(' | '<'), _) => bail!("has a redirection"),
            (Some('>'), _, _) => RedirOp::RdrOut,
            _ => bail!("has a redirection"),
        };
        self.i += match op {
            RedirOp::RdrOut => 1,
            RedirOp::AppOut | RedirOp::RdrAll | RedirOp::DplOut => 2,
        };
        if op == RedirOp::DplOut {
            // The descriptor right after >&, unquoted: zsh may take a
            // quoted one for a file name.
            let target = self.word().map_err(|_| anyhow!("has a redirection"))?;
            if target.quoted || (target.text != "1" && target.text != "2") {
                bail!("has a redirection");
            }
        } else {
            self.skip(is_blank);
            let target = self.word().map_err(|_| anyhow!("has a redirection"))?;
            if target.text != "/dev/null" {
                bail!("has a redirection");
            }
        }
        // Nothing may follow the target directly (another redirection run
        // into it reads differently from shell to shell).
        if !matches!(self.peek(), None | Some(' ' | '\t' | '\n' | ';')) {
            bail!("has a redirection");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_words_removes_quotes() {
        let got = literal_words(r#"  agent send 'a b'"c \"d\" \$e \x"f;  "#).unwrap();
        assert_eq!(got, vec!["agent", "send", r#"a bc "d" $e \xf"#]);
        let got = literal_words("agent send x \"one\\\ntwo\"\n").unwrap();
        assert_eq!(got, vec!["agent", "send", "x", "onetwo"]);
        let got = literal_words("2>&1 agent list >/dev/null 1>>'/dev/null' &>/dev/null").unwrap();
        assert_eq!(got, vec!["agent", "list"]);
    }

    #[test]
    fn literal_words_line_continuations() {
        let got = literal_words("agent wait a b \\\n  --for idle \\\n\t--next").unwrap();
        assert_eq!(
            got,
            vec!["agent", "wait", "a", "b", "--for", "idle", "--next"]
        );
        let got = literal_words("agent send x \\\n\"a b\" \\\n'c'").unwrap();
        assert_eq!(got, vec!["agent", "send", "x", "a b", "c"]);
        for cmd in [
            "\\\nagent list",               // at the start
            "  \\\nagent list",             // at the start, after blanks
            "agent list \\\n",              // the last thing on the line
            "agent list \\\n  ",            // followed by blanks only
            "agent list \\\n;",             // before a ;
            "agent list \\\n\nrm x",        // before a newline
            "agent \\\n\\\nlist",           // two in a row
            "agent \\\n \\\nlist",          // two in a row, with a blank
            "agent\\\nlist",                // run into a word
            "agent \\\r\nlist",             // before CR LF
            "agent list >/dev/null \\\n-q", // after a redirection
            "agent list \\\n>/dev/null",    // before a redirection
            "agent list \\\n2>&1",          // before an fd redirection
            "agent list \\\n&>/dev/null",   // before &>
            "agent list > \\\n/dev/null",   // inside a redirection
        ] {
            assert!(
                literal_words(cmd).is_err(),
                "literal_words({cmd:?}) allowed it"
            );
        }
    }

    #[test]
    fn literal_words_refuses_append_both() {
        // bash 3.2 has no &>>.
        assert!(literal_words("agent list &>>/dev/null").is_err());
        assert!(literal_words("agent send helper &>>/dev/null sh -c 'id'").is_err());
        assert!(literal_words("&>>/dev/null agent list").is_err());
    }

    // An executable at dir/name.
    fn executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("allowcmd-unit-")
            .tempdir_in("/var/tmp")
            .unwrap()
    }

    #[test]
    fn look_path_refuses_a_tilde_entry_before_the_hit() {
        let dir = tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("opt").join("bin");
        let prog = executable(&bin, "agent");
        let cwd = root.to_str().unwrap();
        let bin_s = bin.to_str().unwrap();
        // bash would run ~/evil/agent if it is there; this can't tell.
        let path = format!("~/evil:{bin_s}:/usr/bin");
        let err = look_path_in("agent", OsStr::new(&path), cwd).unwrap_err();
        assert!(format!("{err:#}").contains("starts with ~"), "{err:#}");
        let path = format!("~user/bin:{bin_s}");
        assert!(look_path_in("agent", OsStr::new(&path), cwd).is_err());
        // After the hit, it changes nothing.
        let path = format!("{bin_s}:~/evil");
        let got = look_path_in("agent", OsStr::new(&path), cwd).unwrap();
        assert_eq!(got, prog.to_str().unwrap());

        // Through the checker, the review's case: refused.
        let mut c = Checker::new(prog.to_str().unwrap(), &["list"]);
        let path = format!("~/evil:{bin_s}:/usr/bin");
        c.look_path = Some(Box::new(move |name, cwd| {
            look_path_in(name, OsStr::new(&path), cwd)
        }));
        assert!(c.check("agent list", cwd).is_err());
        c.check(&format!("{} list", prog.display()), cwd).unwrap();
    }

    #[test]
    fn look_path_resolves_relative_entries_against_cwd() {
        // The checker runs in one directory (dir_a, no agent), the shell in
        // another (dir_b, with an agent of its own): PATH ":/opt/bin" or
        // ".:/opt/bin" makes the shell run dir_b/agent.
        let dir = tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("opt").join("bin");
        let prog = executable(&bin, "agent");
        let dir_b = root.join("b");
        executable(&dir_b, "agent");
        let dir_b = dir_b.to_str().unwrap().to_string();
        let bin_s = bin.to_str().unwrap().to_string();
        for path in [
            format!(":{bin_s}"),
            format!(".:{bin_s}"),
            format!("{bin_s}x::{bin_s}"),
        ] {
            let err = look_path_in("agent", OsStr::new(&path), &dir_b).unwrap_err();
            assert!(
                format!("{err:#}").contains("relative to current directory"),
                "PATH {path:?}: {err:#}"
            );
            let mut c = Checker::new(prog.to_str().unwrap(), &["list"]);
            let p = path.clone();
            c.look_path = Some(Box::new(move |name, cwd| {
                look_path_in(name, OsStr::new(&p), cwd)
            }));
            assert!(c.check("agent list", &dir_b).is_err(), "PATH {path:?}");
        }
        // A relative entry the shell finds nothing in is passed over.
        let path = format!("sub:.:{bin_s}");
        let root_s = root.to_str().unwrap();
        let got = look_path_in("agent", OsStr::new(&path), root_s).unwrap();
        assert_eq!(got, prog.to_str().unwrap());
        // A relative entry with no working directory to look in is refused.
        assert!(look_path_in("agent", OsStr::new(&path), "").is_err());
        assert!(look_path_in("agent", OsStr::new(&path), "rel").is_err());
        // Absolute entries only: cwd doesn't matter.
        let got = look_path_in("agent", OsStr::new(&bin_s), "").unwrap();
        assert_eq!(got, prog.to_str().unwrap());
    }

    // The words `script` prints, run by `shell` (bash or zsh, with no rc
    // files, in an empty environment); None if the shell isn't installed.
    fn shell_words(shell: &str, script: &str, args: &[&str]) -> Option<Vec<String>> {
        let home = tempdir();
        let out = std::process::Command::new(shell)
            .args(args)
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // Loading a module may print a line of its own.
        Some(
            text.lines()
                .filter(|l| !l.is_empty() && !l.contains(char::is_whitespace))
                .map(str::to_string)
                .collect(),
        )
    }

    #[test]
    fn shell_words_cover_the_installed_shells() {
        let checks: [(&str, &str, &[&str]); 3] = [
            ("bash", "compgen -b; compgen -k", &["--norc", "--noprofile"]),
            (
                "bash",
                // bash's loadable builtins, where it has them.
                r#"for f in /usr/lib/bash/* /usr/local/lib/bash/*; do
                     enable -f "$f" "${f##*/}" 2>/dev/null
                   done; compgen -b"#,
                &["--norc", "--noprofile"],
            ),
            (
                "zsh",
                r#"zmodload zsh/parameter
                   for d in $module_path; do
                     for f in $d/**/*.so(N); do zmodload ${${f#$d/}%.so} 2>/dev/null; done
                   done
                   print -rl -- ${(k)builtins} ${(k)reswords}"#,
                &["-f"],
            ),
        ];
        let mut missing = Vec::new();
        for (shell, script, args) in checks {
            let Some(words) = shell_words(shell, script, args) else {
                continue; // not installed
            };
            // The script ran: its shell's own builtins are in what it printed.
            let own = if shell == "bash" {
                "compgen"
            } else {
                "zmodload"
            };
            assert!(words.iter().any(|w| w == own), "{shell} printed {words:?}");
            for w in words {
                if !SHELL_WORDS.contains(&w.as_str()) {
                    missing.push(format!("{shell}: {w}"));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "not in SHELL_WORDS: {missing:?}");
    }

    #[test]
    fn root_flags_are_refused_after_the_subcommand() {
        for w in [
            "--backend",
            "--backend-config",
            "--termd-ns",
            "--backend=x",
            "--termd-ns=wall",
        ] {
            assert!(root_flag(w), "{w}");
        }
        for w in [
            "--backends",
            "--backend-configx",
            "-backend",
            "--quiet",
            "-q",
            "x --backend",
            "--back",
        ] {
            assert!(!root_flag(w), "{w}");
        }
    }

    #[test]
    fn clean_is_lexical() {
        assert_eq!(clean(Path::new("/a/b/../c/./d/")), PathBuf::from("/a/c/d"));
        assert_eq!(clean(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(clean(Path::new("//a//b")), PathBuf::from("/a/b"));
    }
}
