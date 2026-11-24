//! The numbers, as the Python scripts computed them: hit@k, MRR@10,
//! nDCG@10 and the share of relevant files in the top 5 (run_eval.py), the
//! kinds' own checks (blind3_score.py), latency; paired bootstraps by
//! question or by session; and the run-against-run tables (cmp.py).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::boot::{self, Cell, PyRandom};
use super::{Results, Row};

const PROSE: &[&str] = &[".md", ".txt", ".rst", ".adoc", ".org"];

pub fn hit(r: &Row, k: usize) -> f64 {
    (r.rank > 0 && r.rank <= k) as u8 as f64
}

pub fn mrr(r: &Row) -> f64 {
    if r.rank > 0 { 1.0 / r.rank as f64 } else { 0.0 }
}

pub fn ndcg(r: &Row) -> f64 {
    let g = &r.grades;
    let gain = |v: i64| 2f64.powi(v as i32) - 1.0;
    let dcg: f64 = r
        .top
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, f)| gain(*g.get(f).unwrap_or(&0)) / ((i + 2) as f64).log2())
        .sum();
    let mut v: Vec<i64> = g.values().copied().collect();
    v.sort_by(|a, b| b.cmp(a));
    let ideal: f64 = v
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, x)| gain(*x) / ((i + 2) as f64).log2())
        .sum();
    if ideal > 0.0 { dcg / ideal } else { 0.0 }
}

/// The share of a question's graded files in the top 5.
pub fn share5(r: &Row) -> f64 {
    if r.grades.is_empty() {
        return 0.0;
    }
    r.top
        .iter()
        .take(5)
        .filter(|f| r.grades.contains_key(*f))
        .count() as f64
        / r.grades.len() as f64
}

/// multihop: every grade-2 file in the top 10.
pub fn all_grade2(r: &Row) -> f64 {
    let want: BTreeSet<&String> = r
        .grades
        .iter()
        .filter(|(_, g)| **g == 2)
        .map(|(f, _)| f)
        .collect();
    let top: BTreeSet<&String> = r.top.iter().take(10).collect();
    (!want.is_empty() && want.is_subset(&top)) as u8 as f64
}

/// cross: a grade-2 file from each project that has one, in the top 10.
pub fn each_project(r: &Row) -> f64 {
    let p = |f: &String| r.grade_project.get(f).cloned().unwrap_or_default();
    let want: BTreeSet<String> = r
        .grades
        .iter()
        .filter(|(_, g)| **g == 2)
        .map(|(f, _)| p(f))
        .collect();
    let got: BTreeSet<String> = r
        .top
        .iter()
        .take(10)
        .filter(|f| r.grades.get(*f) == Some(&2))
        .map(p)
        .collect();
    (!want.is_empty() && want.is_subset(&got)) as u8 as f64
}

/// why: a grade-2 prose file in the top 5.
pub fn prose5(r: &Row) -> f64 {
    r.top
        .iter()
        .take(5)
        .any(|f| r.grades.get(f) == Some(&2) && PROSE.iter().any(|x| f.to_lowercase().ends_with(x)))
        as u8 as f64
}

/// The p-th value as blind3_score.py's `med` picks it.
pub fn pct(xs: impl Iterator<Item = f64>, p: f64) -> Option<f64> {
    let mut v: Vec<f64> = xs.collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let i = ((p * v.len() as f64) as usize).min(v.len() - 1);
    Some(v[i])
}

fn ms(x: Option<f64>) -> String {
    x.map_or("-".into(), |v| format!("{v:.0}"))
}

/// The rows of one set and system, by question id.
pub type BySystem<'a> = BTreeMap<(String, String), BTreeMap<String, &'a Row>>;

pub fn index(rows: &[Row]) -> BySystem<'_> {
    let mut m: BySystem = BTreeMap::new();
    for r in rows {
        m.entry((r.set.clone(), r.system.clone()))
            .or_default()
            .insert(r.qid.clone(), r);
    }
    m
}

/// blind3_score.py's paired comparison over `ids` both answered: (n, mean
/// first, mean second, difference, interval).
fn paired(
    a: &BTreeMap<String, &Row>,
    b: &BTreeMap<String, &Row>,
    f: fn(&Row) -> f64,
    ids: &[String],
    resamples: usize,
    seed: u64,
) -> Option<(usize, f64, f64, f64, f64, f64)> {
    let mut ids: Vec<&String> = ids
        .iter()
        .filter(|q| a.contains_key(*q) && b.contains_key(*q))
        .collect();
    ids.sort();
    if ids.is_empty() {
        return None;
    }
    let va: Vec<f64> = ids.iter().map(|q| f(a[*q])).collect();
    let vb: Vec<f64> = ids.iter().map(|q| f(b[*q])).collect();
    let d: Vec<f64> = va.iter().zip(&vb).map(|(x, y)| x - y).collect();
    let n = d.len() as f64;
    let (diff, lo, hi) = boot::paired(&d, resamples, seed);
    Some((
        d.len(),
        va.iter().sum::<f64>() / n,
        vb.iter().sum::<f64>() / n,
        diff,
        lo,
        hi,
    ))
}

fn line(label: &str, c: Option<(usize, f64, f64, f64, f64, f64)>, count: bool) -> String {
    match c {
        None => format!("| {label} | 0 | - | - | - |"),
        Some((n, ma, mb, d, lo, hi)) if count => format!(
            "| {label} | {n} | {ma:.2} ({:.0}) | {mb:.2} ({:.0}) | {d:+.3} [{lo:+.3}, {hi:+.3}] |",
            ma * n as f64,
            mb * n as f64
        ),
        Some((n, ma, mb, d, lo, hi)) => {
            format!("| {label} | {n} | {ma:.3} | {mb:.3} | {d:+.3} [{lo:+.3}, {hi:+.3}] |")
        }
    }
}

/// run_eval.py's comparison of a minus b over the queries both answered,
/// whole units resampled: (queries, units, recall@5, MRR, nDCG@10), each a
/// (difference, low, high).
#[allow(clippy::type_complexity)]
pub fn units_compare(
    a: &BTreeMap<String, &Row>,
    b: &BTreeMap<String, &Row>,
    resamples: usize,
    seed: u64,
) -> Option<(usize, usize, Vec<(f64, f64, f64)>)> {
    let ids: Vec<&String> = a.keys().filter(|q| b.contains_key(*q)).collect();
    if ids.is_empty() {
        return None;
    }
    let mut order: Vec<String> = Vec::new();
    let mut cells: BTreeMap<String, Cell> = BTreeMap::new();
    for q in &ids {
        let (ra, rb) = (a[*q], b[*q]);
        let u = ra.unit.clone();
        let c = cells.entry(u.clone()).or_insert_with(|| {
            order.push(u.clone());
            Cell {
                sums: vec![0.0; 3],
                n: 0,
            }
        });
        c.sums[0] += hit(ra, 5) - hit(rb, 5);
        c.sums[1] += mrr(ra) - mrr(rb);
        c.sums[2] += if ra.grades.is_empty() { 0.0 } else { ndcg(ra) }
            - if rb.grades.is_empty() { 0.0 } else { ndcg(rb) };
        c.n += 1;
    }
    let cells: Vec<Cell> = order.iter().map(|u| cells[u].clone()).collect();
    Some((
        ids.len(),
        cells.len(),
        boot::units(&cells, 3, resamples, seed),
    ))
}

fn fmt3(t: (f64, f64, f64)) -> String {
    format!("{:+.3} [{:+.3}, {:+.3}]", t.0, t.1, t.2)
}

/// The summary of a run, as Markdown.
pub fn summary(res: &Results, resamples: usize, seed: u64) -> String {
    let by = index(&res.rows);
    let mut o = String::new();
    let _ = writeln!(o, "## Per set and system\n");
    let _ = writeln!(
        o,
        "| set | system | n | hit@1 | hit@5 | hit@10 | MRR@10 | nDCG@10 | share of graded in top 5 | median ms | p90 ms | median ms in the index | p90 |"
    );
    let _ = writeln!(o, "|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for ((set, system), rows) in &by {
        let n = rows.len() as f64;
        let at = |k| {
            let c: f64 = rows.values().map(|r| hit(r, k)).sum();
            format!("{:.2} ({c:.0})", c / n)
        };
        let graded = rows.values().any(|r| !r.grades.is_empty());
        let mean = |f: fn(&Row) -> f64| rows.values().map(|r| f(r)).sum::<f64>() / n;
        let _ = writeln!(
            o,
            "| {set} | {system} | {} | {} | {} | {} | {:.3} | {} | {} | {} | {} | {} | {} |",
            rows.len(),
            at(1),
            at(5),
            at(10),
            mean(mrr),
            if graded {
                format!("{:.3}", mean(ndcg))
            } else {
                "-".into()
            },
            if graded {
                format!("{:.2}", mean(share5))
            } else {
                "-".into()
            },
            ms(pct(rows.values().map(|r| r.ms), 0.5)),
            ms(pct(rows.values().map(|r| r.ms), 0.9)),
            ms(pct(rows.values().filter_map(|r| r.own_ms), 0.5)),
            ms(pct(rows.values().filter_map(|r| r.own_ms), 0.9)),
        );
    }
    // The graded sets at their own scope (blind3_score.py's tables).
    let graded_sets: BTreeSet<&String> = by
        .iter()
        .filter(|(_, r)| r.values().any(|x| !x.grades.is_empty()))
        .map(|((s, _), _)| s)
        .collect();
    for set in graded_sets {
        let sys = |name: &str| by.get(&(set.clone(), name.to_string()));
        let configs: BTreeSet<String> = by
            .keys()
            .filter(|(s, y)| s == set && y.ends_with(":index@scope"))
            .map(|(_, y)| y.trim_end_matches(":index@scope").to_string())
            .collect();
        if let Some(rg) = sys("rg@scope") {
            for cfg in &configs {
                let Some(idx) = sys(&format!("{cfg}:index@scope")) else {
                    continue;
                };
                let ids: Vec<String> = idx.keys().cloned().collect();
                let p = |f| paired(idx, rg, f, &ids, resamples, seed);
                let _ = writeln!(o, "\n## {set}: {cfg} minus rg, at the question's scope\n");
                let _ = writeln!(
                    o,
                    "| measure | questions | index | rg | difference [95% CI] |\n|---|---|---|---|---|"
                );
                let _ = writeln!(
                    o,
                    "{}",
                    line("a relevant file in top 5 (primary)", p(|r| hit(r, 5)), true)
                );
                let _ = writeln!(
                    o,
                    "{}",
                    line("the top result relevant", p(|r| hit(r, 1)), true)
                );
                let _ = writeln!(o, "{}", line("MRR@10", p(mrr), false));
                let _ = writeln!(o, "{}", line("nDCG@10", p(ndcg), false));
                let kinds: BTreeSet<&str> = idx
                    .values()
                    .map(|r| r.kind.as_str())
                    .filter(|k| !k.is_empty())
                    .collect();
                if !kinds.is_empty() {
                    let _ = writeln!(
                        o,
                        "\nBy kind (a relevant file in top 5):\n\n| kind | questions | index | rg | difference [95% CI] |\n|---|---|---|---|---|"
                    );
                    for k in &kinds {
                        let of: Vec<String> = idx
                            .iter()
                            .filter(|(_, r)| r.kind == *k)
                            .map(|(q, _)| q.clone())
                            .collect();
                        let _ = writeln!(
                            o,
                            "{}",
                            line(
                                k,
                                paired(idx, rg, |r| hit(r, 5), &of, resamples, seed),
                                true
                            )
                        );
                    }
                    let of = |k: &str| -> Vec<String> {
                        idx.iter()
                            .filter(|(_, r)| r.kind == k)
                            .map(|(q, _)| q.clone())
                            .collect()
                    };
                    let _ = writeln!(
                        o,
                        "\nThe kinds' own checks:\n\n| check | questions | index | rg | difference [95% CI] |\n|---|---|---|---|---|"
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "multihop: every grade-2 file in top 10",
                            paired(idx, rg, all_grade2, &of("multihop"), resamples, seed),
                            true
                        )
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "cross: a grade-2 file from each project in top 10",
                            paired(idx, rg, each_project, &of("cross"), resamples, seed),
                            true
                        )
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "why: a grade-2 prose file in top 5",
                            paired(idx, rg, prose5, &of("why"), resamples, seed),
                            true
                        )
                    );
                }
                if let Some(cwd) = sys(&format!("{cfg}:index@cwd")) {
                    let ids: Vec<String> = cwd.keys().cloned().collect();
                    let _ = writeln!(
                        o,
                        "\nThe index with its soft project prior from cwd (questions with a project):\n\n| pair | questions | first | second | difference [95% CI] |\n|---|---|---|---|---|"
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "index@cwd vs rg in the project, top 5",
                            paired(cwd, rg, |r| hit(r, 5), &ids, resamples, seed),
                            true
                        )
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "index@cwd vs index limited to the project, top 5",
                            paired(cwd, idx, |r| hit(r, 5), &ids, resamples, seed),
                            true
                        )
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "index@cwd vs rg in the project, MRR@10",
                            paired(cwd, rg, mrr, &ids, resamples, seed),
                            false
                        )
                    );
                }
                if let Some(ta) = sys("rg@tree_all") {
                    let ids: Vec<String> = ta.keys().cloned().collect();
                    let _ = writeln!(
                        o,
                        "\nTree questions, rg over the tree without the skips for shallow directories:\n\n| pair | questions | first | second | difference [95% CI] |\n|---|---|---|---|---|"
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "index vs rg@tree_all, top 5",
                            paired(idx, ta, |r| hit(r, 5), &ids, resamples, seed),
                            true
                        )
                    );
                    let _ = writeln!(
                        o,
                        "{}",
                        line(
                            "rg@scope vs rg@tree_all, top 5",
                            paired(rg, ta, |r| hit(r, 5), &ids, resamples, seed),
                            true
                        )
                    );
                }
            }
        }
        // Configuration against configuration.
        let cfgs: Vec<&String> = configs.iter().collect();
        for i in 0..cfgs.len() {
            for j in i + 1..cfgs.len() {
                let (Some(a), Some(b)) = (
                    sys(&format!("{}:index@scope", cfgs[i])),
                    sys(&format!("{}:index@scope", cfgs[j])),
                ) else {
                    continue;
                };
                let ids: Vec<String> = a.keys().cloned().collect();
                let p = |f| paired(a, b, f, &ids, resamples, seed);
                let _ = writeln!(
                    o,
                    "\n## {set}: {} minus {}, index@scope\n",
                    cfgs[i], cfgs[j]
                );
                let _ = writeln!(
                    o,
                    "| measure | questions | first | second | difference [95% CI] |\n|---|---|---|---|---|"
                );
                let _ = writeln!(
                    o,
                    "{}",
                    line("a relevant file in top 5", p(|r| hit(r, 5)), true)
                );
                let _ = writeln!(
                    o,
                    "{}",
                    line("the top result relevant", p(|r| hit(r, 1)), true)
                );
                let _ = writeln!(o, "{}", line("MRR@10", p(mrr), false));
                let _ = writeln!(o, "{}", line("nDCG@10", p(ndcg), false));
            }
        }
        // run_eval.py's graded comparisons, when its systems ran.
        let mut rows = Vec::new();
        for cfg in by
            .keys()
            .filter(|(s, y)| s == set && y.ends_with(":index_words@project"))
            .map(|(_, y)| y.trim_end_matches(":index_words@project").to_string())
        {
            for (label, a, b) in [
                (
                    "index limited to the project vs rg on plain words in the project",
                    format!("{cfg}:index_words@project"),
                    "rg_words@project".to_string(),
                ),
                (
                    "index with its own project prior vs rg on plain words in the project",
                    format!("{cfg}:index_words@cwd"),
                    "rg_words@project".into(),
                ),
                (
                    "index on the question as typed, limited to the project, vs rg on plain words in the project",
                    format!("{cfg}:index_sentence@project"),
                    "rg_words@project".into(),
                ),
                (
                    "index vs rg on plain words, whole tree",
                    format!("{cfg}:index_words@root"),
                    "rg_words@tree".into(),
                ),
                (
                    "index on the question as typed vs rg on plain words, whole tree",
                    format!("{cfg}:index_sentence@root"),
                    "rg_words@tree".into(),
                ),
                (
                    "index limited to the project vs the most-cited files",
                    format!("{cfg}:index_words@project"),
                    "most_cited_files".into(),
                ),
                (
                    "rg on plain words in the project vs the most-cited files",
                    "rg_words@project".into(),
                    "most_cited_files".into(),
                ),
            ] {
                if let (Some(x), Some(y)) = (sys(&a), sys(&b))
                    && let Some((n, _, d)) = units_compare(x, y, resamples, seed)
                {
                    rows.push(format!(
                        "| {label} ({cfg}) | {n} | {} | {} | {} |",
                        fmt3(d[2]),
                        fmt3(d[1]),
                        fmt3(d[0])
                    ));
                }
            }
        }
        if !rows.is_empty() {
            let _ = writeln!(
                o,
                "\n## {set}: first minus second, paired by question (run_eval.py's)\n\n| comparison | questions | nDCG@10 difference | MRR@10 difference | recall@5 difference |\n|---|---|---|---|---|"
            );
            for r in rows {
                let _ = writeln!(o, "{r}");
            }
        }
    }
    // Episodes: run_eval.py's comparisons and the searches to a target.
    let mut rows = Vec::new();
    let cfgs: BTreeSet<String> = by
        .keys()
        .filter(|(_, y)| y.contains(":index_patterns@"))
        .map(|(_, y)| y.split(':').next().unwrap_or("").to_string())
        .collect();
    for cfg in &cfgs {
        for (set, label) in [
            ("pattern", "each agent pattern"),
            ("episode", "an episode's patterns as one list"),
        ] {
            for (idx, rg, what) in [
                (
                    "project",
                    "project",
                    "index limited to the project vs rg in the project",
                ),
                (
                    "cwd",
                    "project",
                    "index with its own project prior vs rg in the project",
                ),
                ("root", "tree", "index vs rg, whole tree"),
            ] {
                if let (Some(a), Some(b)) = (
                    by.get(&(set.to_string(), format!("{cfg}:index_patterns@{idx}"))),
                    by.get(&(set.to_string(), format!("rg_patterns@{rg}"))),
                ) && let Some((n, u, d)) = units_compare(a, b, resamples, seed)
                {
                    rows.push(format!(
                        "| {label}: {what} ({cfg}) | {n} | {u} | {} | {} |",
                        fmt3(d[0]),
                        fmt3(d[1])
                    ));
                }
            }
        }
        for set in ["ask_opening", "ask_all"] {
            for (idx, rg, what) in [
                (
                    "project",
                    "project",
                    "index limited to the project vs rg on plain words",
                ),
                ("root", "tree", "index vs rg on plain words, whole tree"),
            ] {
                if let (Some(a), Some(b)) = (
                    by.get(&(set.to_string(), format!("{cfg}:index_words@{idx}"))),
                    by.get(&(set.to_string(), format!("rg_words@{rg}"))),
                ) && let Some((n, u, d)) = units_compare(a, b, resamples, seed)
                {
                    rows.push(format!(
                        "| {set}: {what} ({cfg}) | {n} | {u} | {} | {} |",
                        fmt3(d[0]),
                        fmt3(d[1])
                    ));
                }
            }
        }
    }
    if !rows.is_empty() {
        let _ = writeln!(
            o,
            "\n## Episodes: index minus baseline (whole sessions resampled)\n\n| comparison | queries | sessions | recall@5 difference | MRR difference |\n|---|---|---|---|---|"
        );
        for r in rows {
            let _ = writeln!(o, "{r}");
        }
        let _ = writeln!(
            o,
            "\n## Searches to reach a target file (top 10), the agent's patterns in its order\n\n| system | episodes | 1 | 2 | 3 | 4 | 5+ | never |\n|---|---|---|---|---|---|---|---|"
        );
        for ((set, system), rows) in &by {
            if set != "episode" || system.contains("_best") {
                continue;
            }
            let t: Vec<usize> = rows.values().filter_map(|r| r.turn).collect();
            let c = |k: usize| t.iter().filter(|x| **x == k).count();
            let _ = writeln!(
                o,
                "| {system} | {} | {} | {} | {} | {} | {} | {} |",
                t.len(),
                c(1),
                c(2),
                c(3),
                c(4),
                t.iter().filter(|x| **x >= 5).count(),
                c(0)
            );
        }
    }
    let _ = writeln!(o, "\n## Sets\n");
    for s in &res.sets {
        let _ = writeln!(
            o,
            "- {}: {} kept; {}. Dropped: {:?}",
            s.name, s.kept, s.detail, s.dropped
        );
    }
    if res.index_errors > 0 {
        let _ = writeln!(o, "\nIndex searches that failed: {}", res.index_errors);
    }
    o
}

/// cmp.py: two runs' rows of the same set and system, cut to the queries
/// both answered; per run the counts, then first minus second, resampled by
/// unit with `random.choice` as cmp.py does.
pub fn compare(
    a: &[Row],
    b: &[Row],
    set: Option<&str>,
    system: Option<&str>,
    resamples: usize,
    seed: u64,
) -> String {
    let (ia, ib) = (index(a), index(b));
    let mut o = String::new();
    for (key, ra) in &ia {
        if set.is_some_and(|s| s != key.0) || system.is_some_and(|s| s != key.1) {
            continue;
        }
        let Some(rb) = ib.get(key) else { continue };
        let ids: Vec<&String> = ra.keys().filter(|q| rb.contains_key(*q)).collect();
        if ids.is_empty() {
            continue;
        }
        let _ = writeln!(o, "### {} / {}\n", key.0, key.1);
        let _ = writeln!(
            o,
            "| run | n | recall@1 | recall@5 | recall@10 | MRR@10 | nDCG@10 | median ms (index) | median ms (whole) |\n|---|---|---|---|---|---|---|---|---|"
        );
        for (name, rows) in [("first", ra), ("second", rb)] {
            let rs: Vec<&Row> = ids.iter().map(|q| rows[*q]).collect();
            let n = rs.len() as f64;
            let rec = |k| rs.iter().map(|r| hit(r, k)).sum::<f64>();
            let graded = rs.iter().any(|r| !r.grades.is_empty());
            let _ = writeln!(
                o,
                "| {name} | {} | {:.2} ({:.0}) | {:.2} ({:.0}) | {:.2} ({:.0}) | {:.3} | {} | {} | {} |",
                rs.len(),
                rec(1) / n,
                rec(1),
                rec(5) / n,
                rec(5),
                rec(10) / n,
                rec(10),
                rs.iter().map(|r| mrr(r)).sum::<f64>() / n,
                if graded {
                    format!("{:.3}", rs.iter().map(|r| ndcg(r)).sum::<f64>() / n)
                } else {
                    "-".into()
                },
                ms(pct(rs.iter().map(|r| r.own_ms.unwrap_or(r.ms)), 0.5)),
                ms(pct(rs.iter().map(|r| r.ms), 0.5)),
            );
        }
        // cmp.py's resampling: units in sorted order, `choice` per slot.
        let mut groups: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
        let mut sorted = ids.clone();
        sorted.sort();
        for q in &sorted {
            groups.entry(ra[*q].unit.as_str()).or_default().push(q);
        }
        let keys: Vec<&str> = groups.keys().copied().collect();
        let diffs = |qs: &[&String]| -> (f64, f64) {
            let n = qs.len() as f64;
            let d5 = qs
                .iter()
                .map(|q| hit(ra[*q], 5) - hit(rb[*q], 5))
                .sum::<f64>()
                / n;
            let dm = qs.iter().map(|q| mrr(ra[*q]) - mrr(rb[*q])).sum::<f64>() / n;
            (d5, dm)
        };
        let (d5, dm) = diffs(&sorted);
        let mut rng = PyRandom::new(seed);
        let (mut s5, mut sm) = (Vec::with_capacity(resamples), Vec::with_capacity(resamples));
        for _ in 0..resamples {
            let mut qs: Vec<&String> = Vec::new();
            for _ in 0..keys.len() {
                qs.extend(groups[keys[rng.choice(keys.len())]].iter().copied());
            }
            let (x, y) = diffs(&qs);
            s5.push(x);
            sm.push(y);
        }
        s5.sort_by(|x, y| x.total_cmp(y));
        sm.sort_by(|x, y| x.total_cmp(y));
        let (l5, h5) = boot::interval(&s5);
        let (lm, hm) = boot::interval(&sm);
        let fa = sorted
            .iter()
            .filter(|q| hit(ra[**q], 5) > 0.0 && hit(rb[**q], 5) == 0.0)
            .count();
        let fb = sorted
            .iter()
            .filter(|q| hit(rb[**q], 5) > 0.0 && hit(ra[**q], 5) == 0.0)
            .count();
        let _ = writeln!(
            o,
            "\n| first vs second | n | units | recall@5 difference | MRR@10 difference | top-5 flips (first only / second only) |\n|---|---|---|---|---|---|"
        );
        let _ = writeln!(
            o,
            "| first vs second | {} | {} | {d5:+.3} [{l5:+.3}, {h5:+.3}] | {dm:+.3} [{lm:+.3}, {hm:+.3}] | {fa} / {fb} |",
            sorted.len(),
            keys.len()
        );
        let tops = sorted.iter().filter(|q| ra[**q].top != rb[**q].top).count();
        let _ = writeln!(
            o,
            "\nQueries whose top 10 differ: {tops} of {}.\n",
            sorted.len()
        );
    }
    if o.is_empty() {
        o.push_str("The runs share no set and system.\n");
    }
    o
}
