// MAX_SUGGESTIONS caps how many close matches similar returns.
const MAX_SUGGESTIONS: usize = 3;

// Go's strings.ToLower, rune by rune.
fn to_lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// The candidates close to `name`, closest first, ignoring case: those within
/// one edit per three letters of the longer word (at least one; swapping two
/// neighbors counts as one edit), or that start with name when it has three
/// letters or more. So "stauts" suggests "status", while "bogus" suggests
/// neither "logs" nor "bus", two edits from a five-letter word. It returns at
/// most three, for "did you mean" suggestions.
pub fn similar<S: AsRef<str>>(name: &str, candidates: &[S]) -> Vec<String> {
    let lower = to_lower(name);
    let lower_len = lower.chars().count();
    let mut matches: Vec<(&str, usize)> = Vec::new();
    for c in candidates {
        let c = c.as_ref();
        if c == name {
            continue;
        }
        let lc = to_lower(c);
        let d = edit_distance(&lower, &lc);
        let max_dist = 1.max(lower_len.max(lc.chars().count()) / 3);
        if d <= max_dist || (lower_len >= 3 && lc.starts_with(&lower)) {
            matches.push((c, d));
        }
    }
    // A stable sort, as Go's sort.SliceStable.
    matches.sort_by_key(|&(_, d)| d);
    matches
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(s, _)| s.to_string())
        .collect()
}

// edit_distance is the edit distance between a and b, counting a swap of two
// neighbors ("lsit" for "list") as one edit, like an insertion, deletion, or
// substitution (optimal string alignment).
fn edit_distance(a: &str, b: &str) -> usize {
    let ra: Vec<char> = a.chars().collect();
    let rb: Vec<char> = b.chars().collect();
    let mut prev2 = vec![0usize; rb.len() + 1]; // row i-2, for swaps
    let mut prev: Vec<usize> = (0..=rb.len()).collect();
    let mut cur = vec![0usize; rb.len() + 1];
    for i in 1..=ra.len() {
        cur[0] = i;
        for j in 1..=rb.len() {
            let cost = usize::from(ra[i - 1] != rb[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && ra[i - 1] == rb[j - 2] && ra[i - 2] == rb[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
        }
        // prev2, prev, cur = prev, cur, prev2
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[rb.len()]
}

/// Joins items as "a", "a or b", or "a, b, or c", each formatted by `quote`
/// (e.g. `gostd::strconv::quote`, or None to leave them as they are).
pub fn or_list<S: AsRef<str>>(items: &[S], quote: Option<&dyn Fn(&str) -> String>) -> String {
    let q: Vec<String> = items
        .iter()
        .map(|s| match quote {
            Some(f) => f(s.as_ref()),
            None => s.as_ref().to_string(),
        })
        .collect();
    match q.len() {
        0 => String::new(),
        1 => q[0].clone(),
        2 => format!("{} or {}", q[0], q[1]),
        n => format!("{}, or {}", q[..n - 1].join(", "), q[n - 1]),
    }
}
