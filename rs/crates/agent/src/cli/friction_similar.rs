//! Whether a friction summary looks like an open entry's (Go:
//! internal/cli/friction_similar.go). Declared from friction.rs.

use std::collections::HashSet;
use std::sync::LazyLock;

use agent_store::Friction;

// A summary looks like another if they share at least SIMILAR_MIN_SHARED
// words and their Dice coefficient (shared words over average word count)
// reaches SIMILAR_MIN_SCORE. A wrong match costs the filer a turn, so both
// are set to miss rather than over-match.
pub const SIMILAR_MIN_SHARED: usize = 2;
pub const SIMILAR_MIN_SCORE: f64 = 0.6;
/// The matches add lists.
pub const SIMILAR_MAX: usize = 3;

/// Words that carry no topic: function words, and words nearly every summary
/// about this tool has.
static SUMMARY_STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    "a an the and or but if then than so to of in on at by for from with
		without as is are was were be been being it its this that these those there here when while
		which who what how why do does did doesn don didn not no can cannot could should would will
		just only also too very about into out up any some all my your our their me you we they i
		get gets got agent agents command commands"
        .split_whitespace()
        .collect()
});

/// The topic words of a summary: lowercased, split at anything but letters
/// and digits, plurals folded, stop words dropped.
pub fn summary_words(s: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    for w in s
        .to_lowercase()
        .split(|r: char| !r.is_alphabetic() && !r.is_numeric())
        .filter(|w| !w.is_empty())
    {
        let mut w = w;
        if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
            w = &w[..w.len() - 1];
        }
        if w.len() > 1 && !SUMMARY_STOP_WORDS.contains(w) {
            words.insert(w.to_string());
        }
    }
    words
}

/// Scores how alike two summaries' words are, from 0 to 1. Summaries that
/// name different agent commands (e.g. send and queue) are about different
/// things, however alike the rest.
pub fn summary_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.iter().filter(|w| b.contains(*w)).count();
    if shared < SIMILAR_MIN_SHARED {
        return 0.0;
    }
    let (ca, cb) = (command_words(a), command_words(b));
    if !ca.is_empty() && !cb.is_empty() && ca.is_disjoint(&cb) {
        return 0.0;
    }
    2.0 * shared as f64 / (a.len() + b.len()) as f64
}

/// The words that name a top-level agent command. The commands are the
/// agent's own (the help groups' commands), not every subcommand of the
/// root: help is one of those, and "help" in a summary is about some
/// command's help, so "help for wait" and "help for send" would share a
/// command and look alike.
pub fn command_words(words: &HashSet<String>) -> HashSet<String> {
    crate::cli::root::tree()
        .subs
        .iter()
        .filter(|d| !d.group_id.is_empty())
        .map(|d| d.name())
        .filter(|name| words.contains(*name))
        .map(str::to_string)
        .collect()
}

/// The entries whose summaries look like summary, closest first, at most
/// SIMILAR_MAX.
pub fn similar_friction<'a>(summary: &str, entries: &'a [Friction]) -> Vec<&'a Friction> {
    let words = summary_words(summary);
    let mut matches: Vec<(&Friction, f64)> = entries
        .iter()
        .map(|f| (f, summary_similarity(&words, &summary_words(&f.summary))))
        .filter(|(_, score)| *score >= SIMILAR_MIN_SCORE)
        .collect();
    // A stable sort, closest first.
    matches.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
    matches
        .into_iter()
        .take(SIMILAR_MAX)
        .map(|(f, _)| f)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_similarity_cases() {
        let tests = [
            (
                "wait --next returns immediately when the agent is already idle",
                "agent wait --next returns at once if already idle",
                true,
            ),
            ("status says unknown for aider agents", "status is unknown for aider", true),
            (
                "wait --timeout doesn't say what status it last saw",
                "wait timeout doesn't say the last status it saw",
                true,
            ),
            // Different commands, same complaint.
            (
                "send's help doesn't say messages are queued",
                "queue's help doesn't say messages are queued",
                false,
            ),
            ("help for wait is confusing", "help for send is confusing", false),
            (
                "send help doesn't say messages are queued",
                "queue help doesn't say a watcher is needed",
                false,
            ),
            (
                "no way to wait for several agents at once",
                "wait --timeout exit code is undocumented",
                false,
            ),
            ("cleanup refuses my own helper", "cleanup error is unclear", false),
            ("bug", "bug", false), // one word in common is not enough
        ];
        for (a, b, same) in tests {
            let score = summary_similarity(&summary_words(a), &summary_words(b));
            assert_eq!(
                score >= SIMILAR_MIN_SCORE,
                same,
                "{a:?} vs {b:?}: score {score:.2}, want same = {same}"
            );
        }
    }
}
