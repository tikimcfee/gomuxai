/// Quotes `s` as a single word for POSIX shells (and fish).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Joins words into a command line a shell would split back into them,
/// quoting only the words that need it.
pub fn shell_join<S: AsRef<str>>(words: &[S]) -> String {
    words
        .iter()
        .map(|w| {
            let w = w.as_ref();
            if w.is_empty() || w.chars().any(needs_quote) {
                shell_quote(w)
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn needs_quote(r: char) -> bool {
    if r.is_ascii_alphanumeric() {
        return false;
    }
    !"@%+=:,./_-".contains(r)
}

/// Splits a command line into words as a POSIX shell would (quotes and
/// escapes honored, nothing expanded), or an empty list if it doesn't parse.
///
/// The Go splits with mvdan.cc/sh's `shell.Fields`, with every variable
/// unset, and this follows what that gives: `$VAR` and `${VAR}` are empty
/// (and an unquoted empty one makes no word), `${VAR:-word}` is word, brace
/// expansion happens (`a{b,c}` is two words), `~` stays as written, a `#`
/// starting a word comments out the rest of the line, and an operator (`;`,
/// `|`, `&`, `<`, `>`, `(`, `)`), a command substitution, or an unclosed
/// quote doesn't parse. One difference: arithmetic (`$((1+2))`), which
/// mvdan evaluates, doesn't parse here. Where mvdan would hang or panic (a
/// brace sequence that overflows, some `${X@op}` forms), the line doesn't
/// parse either.
pub fn split_shell_words(line: &str) -> Vec<String> {
    let mut p = Parser {
        s: lexed_chars(line),
        i: 0,
    };
    p.fields().unwrap_or_default()
}

// lexed_chars is line as mvdan's lexer reads it: NUL bytes are skipped
// ("like bash"), and a CR right before a newline goes.
fn lexed_chars(line: &str) -> Vec<char> {
    let raw: Vec<char> = line.chars().collect();
    let mut out = Vec::with_capacity(raw.len());
    for (i, &c) in raw.iter().enumerate() {
        match c {
            '\0' => {}
            '\r' if raw.get(i + 1) == Some(&'\n') => {}
            c => out.push(c),
        }
    }
    out
}

// The most words one brace expansion may make: beyond it the line doesn't
// parse (mvdan would make them all, or loop forever on an overflow).
const MAX_BRACE_WORDS: usize = 1 << 16;

// A word as parsed, before brace expansion and field splitting.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Unit {
    Lit(char),    // unquoted literal: braces in it expand
    Quoted(char), // quoted or escaped literal
    Exp(char),    // from an unquoted expansion: blanks in it split fields
    Mark,         // quotes, even empty ones: the word is a field
}

struct Parser {
    s: Vec<char>,
    i: usize,
}

struct ParseError;

type Parsed<T> = Result<T, ParseError>;

fn is_operator(c: char) -> bool {
    matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')')
}

// is_blank: what splits the fields of an unquoted expansion (IFS).
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

// is_separator: what ends an unquoted word; mvdan takes a CR for a space.
fn is_separator(c: char) -> bool {
    is_blank(c) || c == '\r'
}

// The characters of a parameter name, and the parameters one character
// names ($@, $1).
fn param_name_rune(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn single_rune_param(c: char) -> bool {
    "@*#$?!-".contains(c) || c.is_ascii_digit()
}

fn valid_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && s.chars().all(param_name_rune)
}

// unit_char is the character a unit holds, if any.
fn unit_char(u: &Unit) -> Option<char> {
    match *u {
        Unit::Lit(c) | Unit::Quoted(c) | Unit::Exp(c) => Some(c),
        Unit::Mark => None,
    }
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    fn fields(&mut self) -> Parsed<Vec<String>> {
        let mut out = Vec::new();
        loop {
            // Blanks and line continuations between words.
            while let Some(c) = self.peek() {
                if is_separator(c) {
                    self.i += 1;
                } else if c == '\\' && self.peek_at(1) == Some('\n') {
                    self.i += 2;
                } else {
                    break;
                }
            }
            match self.peek() {
                None => return Ok(out),
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.i += 1;
                    }
                    continue;
                }
                Some(c) if is_operator(c) => return Err(ParseError),
                _ => {}
            }
            let word = self.word()?;
            for units in expand_braces(word)? {
                out.extend(split_fields(&units));
            }
        }
    }

    // word reads one word, up to an unquoted blank or operator.
    fn word(&mut self) -> Parsed<Vec<Unit>> {
        let mut w = Vec::new();
        while let Some(c) = self.peek() {
            if is_separator(c) || is_operator(c) {
                break;
            }
            self.i += 1;
            match c {
                '\\' => match self.peek() {
                    // A trailing backslash is dropped, but still makes a word.
                    None => w.push(Unit::Mark),
                    Some('\n') => self.i += 1, // a line continuation
                    Some(n) => {
                        self.i += 1;
                        w.push(Unit::Quoted(n));
                    }
                },
                '\'' => self.single_quoted(&mut w)?,
                '"' => self.double_quoted(&mut w)?,
                '`' => return Err(ParseError), // command substitution
                '$' => self.dollar(&mut w, false)?,
                c => w.push(Unit::Lit(c)),
            }
        }
        Ok(w)
    }

    // single_quoted reads the rest of a '...' string, its opening quote read.
    fn single_quoted(&mut self, w: &mut Vec<Unit>) -> Parsed<()> {
        w.push(Unit::Mark);
        loop {
            match self.peek() {
                None => return Err(ParseError),
                Some('\'') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(n) => {
                    self.i += 1;
                    w.push(Unit::Quoted(n));
                }
            }
        }
    }

    // double_quoted reads the rest of a "..." string, its opening quote read.
    fn double_quoted(&mut self, w: &mut Vec<Unit>) -> Parsed<()> {
        w.push(Unit::Mark);
        loop {
            let Some(c) = self.peek() else {
                return Err(ParseError);
            };
            self.i += 1;
            match c {
                '"' => return Ok(()),
                '\\' => match self.peek() {
                    Some('\n') => self.i += 1,
                    Some(n @ ('$' | '`' | '"' | '\\')) => {
                        self.i += 1;
                        w.push(Unit::Quoted(n));
                    }
                    _ => w.push(Unit::Quoted('\\')),
                },
                '`' => return Err(ParseError),
                '$' => self.dollar(w, true)?,
                c => w.push(Unit::Quoted(c)),
            }
        }
    }

    // dollar reads what follows a '$' (already read). Every variable is unset.
    fn dollar(&mut self, w: &mut Vec<Unit>, quoted: bool) -> Parsed<()> {
        // mvdan reads a name across line continuations ("$\<newline>X" is
        // $X), but not ${, a digit or a special character ("$\<newline>{"
        // is a $ and a {).
        let continuation = |p: &Self| p.peek() == Some('\\') && p.peek_at(1) == Some('\n');
        let mut j = self.i;
        while self.s.get(j) == Some(&'\\') && self.s.get(j + 1) == Some(&'\n') {
            j += 2;
        }
        if self
            .s
            .get(j)
            .is_some_and(|&c| c.is_ascii_alphabetic() || c == '_')
        {
            self.i = j;
        }
        match self.peek() {
            Some('(') => Err(ParseError), // command substitution or arithmetic
            Some('{') => {
                self.i += 1;
                self.braced_param(w, quoted)
            }
            Some('\'') if !quoted => {
                self.i += 1;
                self.ansi_c(w)
            }
            Some('"') if !quoted => {
                self.i += 1;
                self.double_quoted(w)
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => loop {
                if self.peek().is_some_and(param_name_rune) {
                    self.i += 1;
                } else if continuation(self) {
                    self.i += 2;
                } else {
                    return Ok(());
                }
            },
            Some(c) if c.is_ascii_digit() || "@*#?$!-".contains(c) => {
                self.i += 1;
                Ok(())
            }
            _ => {
                w.push(if quoted {
                    Unit::Quoted('$')
                } else {
                    Unit::Lit('$')
                });
                Ok(())
            }
        }
    }

    // braced_param reads ${...} after its "${" and adds its value: unquoted,
    // it splits at blanks (its quoted parts too: mvdan gives ${X:-"a b"} as
    // two words).
    fn braced_param(&mut self, w: &mut Vec<Unit>, quoted: bool) -> Parsed<()> {
        let value = self.param_exp()?;
        w.extend(value.chars().map(|c| {
            if quoted {
                Unit::Quoted(c)
            } else {
                Unit::Exp(c)
            }
        }));
        Ok(())
    }

    // param_exp reads ${...} after its "${" as mvdan's parser does (for
    // bash), and returns its value with every variable unset: empty, "0" for
    // a length, or the word of the - and :- forms. What mvdan refuses, or
    // fails to expand, doesn't parse.
    fn param_exp(&mut self) -> Parsed<String> {
        // A prefix: ${#X} is a length, ${!X} an indirection; ${%X} and ${+X}
        // are other shells'.
        let names_param = |c: Option<char>| {
            c.is_none_or(|c| single_rune_param(c) || param_name_rune(c) || c == '"')
        };
        let (mut length, mut excl) = (false, false);
        if let Some(c @ ('#' | '!' | '%' | '+')) = self.peek()
            && names_param(self.peek_at(1))
        {
            match c {
                '#' => length = true,
                '!' => excl = true,
                _ => return Err(ParseError),
            }
            self.i += 1;
        }
        // The parameter: one special character, digits, or a name.
        let name = match self.peek() {
            // ${"..."} is zsh's.
            None | Some('"') => return Err(ParseError),
            // ${#-word} is $# with a default, not the length of $-.
            Some('?' | '-') if length && self.peek_at(1) != Some('}') => {
                length = false;
                "#".to_string()
            }
            Some(c) if "?-@*#!$".contains(c) => {
                self.i += 1;
                c.to_string()
            }
            Some(_) => {
                let mut name = String::new();
                loop {
                    match self.peek() {
                        Some(c) if param_name_rune(c) => {
                            self.i += 1;
                            name.push(c);
                        }
                        Some('\\') if self.peek_at(1) == Some('\n') => self.i += 2,
                        _ => break,
                    }
                }
                let number = !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit());
                if !number && !valid_name(&name) {
                    return Err(ParseError); // ${1abc}, ${}
                }
                name
            }
        };
        // An index, ${X[1]}: of nothing, as X is unset.
        if self.peek() == Some('[') {
            if !valid_name(&name) {
                return Err(ParseError);
            }
            self.i += 1;
            loop {
                match self.peek() {
                    None | Some('}' | ' ' | '\t' | '\n') => return Err(ParseError),
                    Some(']') => break,
                    Some(_) => self.i += 1,
                }
            }
            self.i += 1;
        }
        let Some(op) = self.peek() else {
            return Err(ParseError);
        };
        self.i += 1;
        if op == '}' {
            if excl {
                return Err(ParseError); // ${!X}: X names no variable
            }
            return Ok(if length { "0".into() } else { String::new() });
        }
        if length {
            return Err(ParseError); // ${#X-a}: two operators
        }
        if excl {
            // ${!X@} and ${!X*} list the names starting with X: none.
            if matches!(op, '@' | '*') && self.peek() == Some('}') {
                self.i += 1;
                return Ok(String::new());
            }
            return Err(ParseError);
        }
        let doubled = |p: &mut Self| {
            if p.peek() == Some(op) {
                p.i += 1;
            }
        };
        match op {
            ':' => match self.peek() {
                Some('+' | '-' | '?' | '=') => {}
                // zsh's ${X:#pattern}, ${X:|array}, ${X:*array}.
                Some('#' | '|' | '*') => return Err(ParseError),
                _ => return self.slice(),
            },
            '+' | '-' | '?' | '=' => {}
            '%' | '#' | '^' | ',' | '/' => doubled(self),
            '@' => return self.param_op(&name),
            _ => return Err(ParseError),
        }
        let colon = op == ':';
        let kind = if colon { self.s[self.i] } else { op };
        if colon {
            self.i += 1;
        }
        // Each form's word is read, and expanded, even where it isn't used.
        let word = self.param_word(op == '/')?;
        if op == '/' && self.peek() == Some('/') {
            self.i += 1;
            self.param_word(false)?;
        }
        if self.peek() != Some('}') {
            return Err(ParseError);
        }
        self.i += 1;
        match kind {
            '-' => Ok(word),
            // mvdan: "environment is read-only", and "parameter not set".
            '=' | '?' => Err(ParseError),
            // ${X+word} of an unset X, removing patterns from or changing
            // the case of nothing: empty.
            _ => Ok(String::new()),
        }
    }

    // param_op reads the rest of ${X@op}, after its "@". Only Q, A, a, E
    // and P are taken: mvdan refuses any other, and panics on k, u, K, L and
    // U of an unset variable.
    fn param_op(&mut self, name: &str) -> Parsed<String> {
        let start = self.i;
        while self.peek().is_some_and(|c| c != '}') {
            self.i += 1;
        }
        if self.peek().is_none() {
            return Err(ParseError);
        }
        let op: String = self.s[start..self.i].iter().collect();
        self.i += 1;
        match op.as_str() {
            "Q" => Ok("''".into()),
            "A" => Ok(format!("{name}=''")),
            "a" | "E" | "P" => Ok(String::new()),
            _ => Err(ParseError),
        }
    }

    // param_word reads the word of a ${X-word} form, up to the closing brace
    // (or, for a replacement's pattern, up to a "/"), and expands it as mvdan
    // does: to one string, whether or not the ${...} is inside "...". Outside
    // quotes a backslash stays, with the character after it, and blanks and
    // operators are plain; '...', "..." and $'...' are quotes all the same,
    // and a backslash and newline go.
    fn param_word(&mut self, until_slash: bool) -> Parsed<String> {
        let mut s = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(ParseError);
            };
            match c {
                '}' => return Ok(s),
                '/' if until_slash => return Ok(s),
                '\\' => {
                    self.i += 1;
                    match self.peek() {
                        None => return Err(ParseError),
                        Some('\n') => self.i += 1,
                        Some(n) => {
                            self.i += 1;
                            s.push('\\');
                            s.push(n);
                        }
                    }
                }
                '`' => return Err(ParseError), // command substitution
                '\'' | '"' | '$' => {
                    self.i += 1;
                    let mut units = Vec::new();
                    match c {
                        '\'' => self.single_quoted(&mut units)?,
                        '"' => self.double_quoted(&mut units)?,
                        _ => self.dollar(&mut units, false)?,
                    }
                    s.extend(units.iter().filter_map(unit_char));
                }
                c => {
                    self.i += 1;
                    s.push(c);
                }
            }
        }
    }

    // slice reads the rest of ${X:offset} or ${X:offset:length}, after the
    // first ":". The offsets are arithmetic; only a plain number or name,
    // signed or not, is taken here. X is unset, so it is empty.
    fn slice(&mut self) -> Parsed<String> {
        let operand = |s: &str| {
            let t = s.trim_matches(|c| c == ' ' || c == '\t');
            let t = t.trim_start_matches(['-', '+']);
            !t.is_empty() && (t.bytes().all(|b| b.is_ascii_digit()) || valid_name(t))
        };
        let mut cur = String::new();
        let mut second = false;
        loop {
            match self.peek() {
                None => return Err(ParseError),
                Some('}') => {
                    self.i += 1;
                    return if operand(&cur) {
                        Ok(String::new())
                    } else {
                        Err(ParseError)
                    };
                }
                Some(':') if !second => {
                    if !operand(&cur) {
                        return Err(ParseError);
                    }
                    self.i += 1;
                    cur.clear();
                    second = true;
                }
                Some(c) => {
                    self.i += 1;
                    cur.push(c);
                }
            }
        }
    }

    // ansi_c reads the rest of a $'...' string.
    // Like bash's, mvdan's string ends at the first NUL an escape makes
    // ($'a\0b' is "a").
    fn ansi_c(&mut self, w: &mut Vec<Unit>) -> Parsed<()> {
        w.push(Unit::Mark);
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(ParseError);
            };
            self.i += 1;
            if c == '\'' {
                let end = out.find('\0').unwrap_or(out.len());
                w.extend(out[..end].chars().map(Unit::Quoted));
                return Ok(());
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some(e) = self.peek() else {
                return Err(ParseError);
            };
            self.i += 1;
            let ch = match e {
                'a' => '\x07',
                'b' => '\x08',
                'e' | 'E' => '\x1b',
                'f' => '\x0c',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\x0b',
                '\\' | '\'' | '"' | '?' => e,
                'x' => {
                    let v = self.digits(16, 2);
                    match v {
                        Some(v) => char::from_u32(v).unwrap_or('\u{fffd}'),
                        None => {
                            out.push('\\');
                            'x'
                        }
                    }
                }
                'u' | 'U' => {
                    let max = if e == 'u' { 4 } else { 8 };
                    match self.digits(16, max) {
                        Some(v) => char::from_u32(v).unwrap_or('\u{fffd}'),
                        None => {
                            out.push('\\');
                            e
                        }
                    }
                }
                '0'..='7' => {
                    self.i -= 1;
                    // Beyond a byte, Go's ParseUint gives its largest: 0xff.
                    let v = self.digits(8, 3).unwrap_or(0);
                    char::from_u32(v.min(0xff)).unwrap_or('\u{fffd}')
                }
                other => {
                    out.push('\\');
                    other
                }
            };
            out.push(ch);
        }
    }

    // digits reads up to max digits in radix, or None if there are none.
    fn digits(&mut self, radix: u32, max: usize) -> Option<u32> {
        let mut v: u32 = 0;
        let mut n = 0;
        while n < max {
            let Some(d) = self.peek().and_then(|c| c.to_digit(radix)) else {
                break;
            };
            v = v.wrapping_mul(radix).wrapping_add(d);
            self.i += 1;
            n += 1;
        }
        if n == 0 { None } else { Some(v) }
    }
}

// expand_braces performs brace expansion on a word's unquoted braces:
// a{b,c}d is abd acd, {1..3} is 1 2 3; a brace with no comma or sequence in it
// ({b}) stays as written. More than MAX_BRACE_WORDS words don't parse.
fn expand_braces(w: Vec<Unit>) -> Parsed<Vec<Vec<Unit>>> {
    let mut from = 0;
    while let Some(off) = w[from..].iter().position(|u| *u == Unit::Lit('{')) {
        let open = from + off;
        if let Some((close, alts)) = brace_alternatives(&w, open)? {
            let mut out = Vec::new();
            for alt in alts {
                let mut next = w[..open].to_vec();
                // Each expansion is a word even when empty: {,} is two.
                next.push(Unit::Mark);
                next.extend(alt);
                next.extend_from_slice(&w[close + 1..]);
                out.extend(expand_braces(next)?);
                if out.len() > MAX_BRACE_WORDS {
                    return Err(ParseError);
                }
            }
            return Ok(out);
        }
        from = open + 1;
    }
    Ok(vec![w])
}

// brace_alternatives returns where the brace opened at open closes and what
// it expands to, or None if it doesn't expand.
#[allow(clippy::type_complexity)]
fn brace_alternatives(w: &[Unit], open: usize) -> Parsed<Option<(usize, Vec<Vec<Unit>>)>> {
    let mut depth = 0;
    let mut commas = Vec::new();
    let mut close = None;
    for (i, u) in w.iter().enumerate().skip(open + 1) {
        match u {
            Unit::Lit('{') => depth += 1,
            Unit::Lit('}') if depth == 0 => {
                close = Some(i);
                break;
            }
            Unit::Lit('}') => depth -= 1,
            Unit::Lit(',') if depth == 0 => commas.push(i),
            _ => {}
        }
    }
    let Some(close) = close else {
        return Ok(None);
    };
    if !commas.is_empty() {
        let mut alts = Vec::new();
        let mut start = open + 1;
        for c in commas.into_iter().chain(std::iter::once(close)) {
            alts.push(w[start..c].to_vec());
            start = c + 1;
        }
        return Ok(Some((close, alts)));
    }
    let inner = &w[open + 1..close];
    let Some(text) = inner
        .iter()
        .map(|u| match u {
            Unit::Lit(c) => Some(*c),
            _ => None,
        })
        .collect::<Option<String>>()
    else {
        return Ok(None);
    };
    let Some(items) = sequence(&text)? else {
        return Ok(None);
    };
    Ok(Some((close, items)))
}

// sequence expands the inside of a sequence brace as mvdan does: {1..5},
// {5..1..2}, {a..e}, {01..10} (01 02 ... 09 010). Its ends must be both
// numbers or both single letters, and a third part a number; otherwise it
// stays as written (None). A step against the direction is ignored, numbers
// wrap around as Go's int does, and a letter range runs through the ASCII
// between Z and a. More than MAX_BRACE_WORDS items don't parse.
fn sequence(text: &str) -> Parsed<Option<Vec<Vec<Unit>>>> {
    let parts: Vec<&str> = text.split("..").collect();
    if parts.len() < 2 {
        return Ok(None);
    }
    // Go's strconv.Atoi and a single ASCII letter.
    let number = |s: &str| s.parse::<i64>().ok();
    let letter = |s: &str| match s.as_bytes() {
        [b] if b.is_ascii_alphabetic() => Some(i64::from(*b)),
        _ => None,
    };
    let (from, to, chars) = match (number(parts[0]), number(parts[1])) {
        (Some(a), Some(b)) => (a, b, false),
        _ => match (letter(parts[0]), letter(parts[1])) {
            (Some(a), Some(b)) => (a, b, true),
            _ => return Ok(None),
        },
    };
    // mvdan checks the step only when there are exactly three parts.
    if parts.len() == 3 && number(parts[2]).is_none() {
        return Ok(None);
    }
    let upward = from <= to;
    let mut incr: i64 = if upward { 1 } else { -1 };
    if let Some(n) = parts.get(2).and_then(|s| number(s))
        && n != 0
        && (n > 0) == upward
    {
        incr = n;
    }
    let zeros = extra_leading_zeros(parts[0]).max(extra_leading_zeros(parts[1]));
    let mut items = Vec::new();
    let mut n = from;
    while (upward && n <= to) || (!upward && n >= to) {
        if items.len() >= MAX_BRACE_WORDS {
            return Err(ParseError);
        }
        // Quoted, so an item is never expanded again.
        let item: Vec<Unit> = if chars {
            // Go's string(rune(n)): the low 32 bits, U+FFFD if no char. A
            // backslash on its own escapes nothing and goes.
            match char::from_u32(n as i32 as u32).unwrap_or('\u{fffd}') {
                '\\' => Vec::new(),
                c => vec![Unit::Quoted(c)],
            }
        } else {
            format!("{}{n}", "0".repeat(zeros))
                .chars()
                .map(Unit::Quoted)
                .collect()
        };
        items.push(item);
        n = n.wrapping_add(incr);
    }
    Ok(Some(items))
}

// extra_leading_zeros: how many zeros start s, unless it is all zeros.
fn extra_leading_zeros(s: &str) -> usize {
    s.find(|c| c != '0').unwrap_or(0)
}

// split_fields splits an expanded word into fields at the blanks that came
// from unquoted expansions; a field exists if anything literal or quoted
// went into it.
fn split_fields(w: &[Unit]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut present = false;
    for u in w {
        match *u {
            Unit::Exp(c) if is_blank(c) => {
                if present {
                    out.push(std::mem::take(&mut cur));
                    present = false;
                }
            }
            Unit::Exp(c) | Unit::Lit(c) | Unit::Quoted(c) => {
                cur.push(c);
                present = true;
            }
            Unit::Mark => present = true,
        }
    }
    if present {
        out.push(cur);
    }
    out
}
