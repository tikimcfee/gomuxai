use std::sync::LazyLock;

use regex::Regex;

// ansi_pattern matches ANSI escape sequences.
static ANSI_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());

/// Removes ANSI escape codes from a string.
pub fn strip_ansi(s: &str) -> String {
    ANSI_PATTERN.replace_all(s, "").into_owned()
}

fn is_terminal_control(r: char) -> bool {
    let u = r as u32;
    u < 0x20 || u == 0x7f || (0x80..=0x9f).contains(&u)
}

/// Removes everything that could act on a terminal rather than print: ESC
/// (and so every escape sequence), other C0 controls except newline and tab,
/// carriage return, DEL, and C1 controls. For text from another host, which
/// may be hostile.
pub fn sanitize_terminal(s: &str) -> String {
    s.chars()
        .filter(|&r| r == '\n' || r == '\t' || !is_terminal_control(r))
        .collect()
}

// sgr_pattern matches an SGR sequence (ESC [ params m), which only sets
// colors and text attributes.
static SGR_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;:]*m").unwrap());

/// `sanitize_terminal` keeping SGR sequences, so a styled screen from another
/// host keeps its colors, bold, and dim, and nothing else that could act on a
/// terminal: no titles, clipboard writes, cursor moves, or mode changes.
pub fn sanitize_styled(s: &str) -> String {
    let mut b = String::with_capacity(s.len());
    let mut pos = 0;
    for m in SGR_PATTERN.find_iter(s) {
        b.push_str(&sanitize_terminal(&s[pos..m.start()]));
        b.push_str(m.as_str());
        pos = m.end();
    }
    b.push_str(&sanitize_terminal(&s[pos..]));
    b
}

// terminal_escape_pattern matches the escape sequences in a raw terminal
// stream, for terminal_lines: OSC (titles, shell integration marks), ended by
// BEL or ST; DCS, SOS, PM, and APC, ended by ST; CSI, with its parameters,
// intermediates, and final byte captured; and the rest, ESC plus optional
// intermediates and a final byte (ESC 7, ESC ( B). A sequence cut off by the
// end of the stream still matches, so a log read while the agent writes
// doesn't leave half of one behind.
static TERMINAL_ESCAPE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\|$)",
        r"|\x1b[PX^_][^\x1b]*(?:\x1b\\|$)",
        r"|\x1b\[([0-?]*)([ -/]*)([@-~]|$)",
        r"|\x1b[ -/]*(?:[0-~]|$)",
    ))
    .unwrap()
});

// MAX_CURSOR_MOVE caps the spaces one cursor movement becomes, so
// ESC [ 999999999 C can't allocate a gigabyte.
const MAX_CURSOR_MOVE: i64 = 512;

/// Turns a raw terminal stream, such as a pipe-pane log of a TUI, into its
/// lines of text. TUIs redraw by moving the cursor rather than printing
/// spaces, so horizontal moves become spaces (or deleting them would glue
/// words together), and carriage return and vertical moves break the line.
/// Every other escape sequence and control character is dropped. Blank lines
/// are dropped, trailing spaces trimmed, and consecutive duplicate lines
/// (redraws) collapsed into one.
///
/// Columns are counted in chars, which is close enough for a log: a wide
/// character before a move shifts what follows by a column.
pub fn terminal_lines(s: &str) -> Vec<String> {
    struct State {
        lines: Vec<String>,
        line: String,
        col: i64,
    }
    impl State {
        fn break_line(&mut self) {
            let text = self.line.trim_end_matches([' ', '\t']);
            if !text.is_empty() && self.lines.last().is_none_or(|l| l != text) {
                self.lines.push(text.to_string());
            }
            self.line.clear();
            self.col = 0;
        }
        // move_to pads the line out to a 0-based column; moves left are dropped.
        fn move_to(&mut self, target: i64) {
            let n = target.min(self.col + MAX_CURSOR_MOVE) - self.col;
            if n > 0 {
                self.line.extend(std::iter::repeat_n(' ', n as usize));
                self.col += n;
            }
        }
        fn text(&mut self, t: &str) {
            for r in t.chars() {
                match r {
                    '\r' | '\n' => self.break_line(),
                    '\t' => {
                        self.line.push(r);
                        self.col += 1;
                    }
                    r if is_terminal_control(r) => {}
                    r => {
                        self.line.push(r);
                        self.col += 1;
                    }
                }
            }
        }
    }
    let mut st = State {
        lines: Vec::new(),
        line: String::new(),
        col: 0,
    };

    let mut pos = 0;
    for caps in TERMINAL_ESCAPE_PATTERN.captures_iter(s) {
        let m = caps.get(0).unwrap();
        st.text(&s[pos..m.start()]);
        pos = m.end();
        let (Some(params), Some(inter), Some(fin)) = (caps.get(1), caps.get(2), caps.get(3)) else {
            continue; // not a CSI
        };
        if !inter.as_str().is_empty() {
            continue; // a CSI with intermediates
        }
        let params = params.as_str();
        match fin.as_str() {
            "C" => {
                // cursor forward n
                // Saturating: Go's int wraps here, and its makeslice then
                // panics (a bug, reported); the cap is what was meant.
                let target = st.col.saturating_add(csi_param(params, 0));
                st.move_to(target);
            }
            "G" | "`" => st.move_to(csi_param(params, 0) - 1), // cursor to column n
            "A" | "B" | "d" | "e" => {
                // cursor up/down n, to row n: same column
                let c = st.col;
                st.break_line();
                st.move_to(c);
            }
            "E" | "F" => st.break_line(), // cursor to the start of the next/previous line
            "H" | "f" => {
                // cursor to row;column
                st.break_line();
                st.move_to(csi_param(params, 1) - 1);
            }
            _ => {}
        }
    }
    st.text(&s[pos..]);
    st.break_line();
    st.lines
}

// csi_param returns the i-th parameter of a CSI sequence, or 1 when it is
// missing or 0, the default for cursor movements. Private parameters
// (ESC [ ? 25 h) aren't numbers and also give 1.
fn csi_param(params: &str, i: usize) -> i64 {
    match params.split(';').nth(i).map(str::parse::<i64>) {
        Some(Ok(n)) if n >= 1 => n,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflowing_move_is_capped() {
        // Go panics here (col + MaxInt64 wraps); the move is capped instead.
        let want = format!("a{}b", " ".repeat(512));
        assert_eq!(terminal_lines("a\x1b[9223372036854775807Cb"), vec![want]);
    }
}
