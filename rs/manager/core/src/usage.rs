//! The usage page: each host's LLM plan limits, as llm-exp's usage widget shows them, then
//! the tokens the manager counted in that host's transcripts. A plain list of lines a UI
//! draws top to bottom (one section per host, foldable), with no layout in it.
//!
//! The limits are llm-exp's figures as the host's `agent manage view` read them from the
//! snapshot llm-exp caches (`HostView::llm_usage`): the share of each limit used and when
//! it resets. Nothing here recomputes them. The token counts are a different measure, from
//! the transcripts, and get a section of their own.

use std::collections::{BTreeMap, BTreeSet};

use agent_manage::llm_usage::{Limit, QuotaBar, QuotaBars};
use agent_manage::{HostView, LlmUsage, Tokens, View, human_tokens};
use chrono::{DateTime, Datelike, TimeZone, Utc};

/// A snapshot older than this is flagged: llm-exp's widget refreshes it every minute and
/// its cache lives 5 minutes, so an older one means nothing is refreshing it.
pub const STALE_MINUTES: i64 = 15;

/// How full a limit is, as the widget colors it: over 90% warns, over 100% is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Over,
}

impl Level {
    pub fn of(pct: i64) -> Level {
        if pct > 100 {
            Level::Over
        } else if pct > 90 {
            Level::Warn
        } else {
            Level::Ok
        }
    }
}

/// One line of the usage page.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageLine {
    /// A host's header; pressing it folds or unfolds the host's section.
    Host {
        host: usize,
        name: String,
        /// How fresh its figures are, or why there are none.
        note: String,
        /// The figures are older than [`STALE_MINUTES`].
        warn: bool,
        /// False when the host didn't answer; the note says why.
        reachable: bool,
        folded: bool,
    },
    /// A group heading: a provider ("claude · max"), or the transcripts' tokens.
    Heading { title: String, summary: String },
    /// One limit: its share used, as a bar, and when it resets.
    Limit {
        label: String,
        /// "37%", or "2,060 / 2,000 · 103%" where the provider gives amounts.
        value: String,
        /// The bar: the share used, 0 to 1 (a limit over 100% shows full).
        share: f32,
        level: Level,
        /// When it resets, in local time ("4:13pm", "Sun Nov 22, 10:00pm"); "" if unknown.
        resets: String,
        /// How long until then: "in 2h 13m", "now"; "" if unknown.
        resets_in: String,
    },
    /// A name and value without a bar (the transcripts' tokens, credits that are off).
    Item {
        name: String,
        value: String,
        detail: String,
    },
    /// A remark: "not available here", where the figures come from, a provider's error.
    Note { text: String, bad: bool },
}

/// The page's lines, one section per host; the hosts named in `folded` show only their
/// header. Reset times are shown in `tz`.
pub fn lines<Tz: TimeZone>(
    view: &View,
    now: DateTime<Utc>,
    tz: &Tz,
    folded: &BTreeSet<String>,
) -> Vec<UsageLine> {
    let mut out = Vec::new();
    for (i, h) in view.hosts.iter().enumerate() {
        let is_folded = folded.contains(&h.host);
        let (header, warn) = host_note(h, now, tz);
        out.push(UsageLine::Host {
            host: i,
            name: h.host.clone(),
            note: header,
            warn,
            reachable: h.reachable,
            folded: is_folded,
        });
        if is_folded || !h.reachable {
            continue;
        }
        match &h.llm_usage {
            None => out.push(note(
                "Plan limits: not available here (no llm-exp usage snapshot on this host).",
                false,
            )),
            Some(u) if !u.error.is_empty() => {
                out.push(note(&format!("Plan limits: {}: {}", u.path, u.error), true))
            }
            Some(u) => limit_lines(u, now, tz, &mut out),
        }
        token_lines(h, &mut out);
    }
    out
}

fn note(text: &str, bad: bool) -> UsageLine {
    UsageLine::Note {
        text: text.to_string(),
        bad,
    }
}

fn heading(title: &str, summary: String) -> UsageLine {
    UsageLine::Heading {
        title: title.to_string(),
        summary,
    }
}

/// The header's note: when llm-exp fetched the figures, or why there are none.
fn host_note<Tz: TimeZone>(h: &HostView, now: DateTime<Utc>, tz: &Tz) -> (String, bool) {
    if !h.reachable {
        let why = if h.error.is_empty() {
            "didn't answer"
        } else {
            &h.error
        };
        return (format!("unreachable: {why}"), false);
    }
    let Some(u) = &h.llm_usage else {
        return (String::new(), false);
    };
    let Some(at) = parse(&u.captured_at) else {
        return (String::new(), false);
    };
    let age = now.signed_duration_since(at);
    let ago = if age.num_seconds() < 60 {
        "just now".to_string()
    } else {
        format!("{} ago", span(age.num_seconds()))
    };
    (
        format!("llm-exp fetched {} ({ago})", clock(at, now, tz)),
        age.num_minutes() >= STALE_MINUTES,
    )
}

fn parse(t: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(t).ok().map(|t| t.to_utc())
}

/// A local time for a person: "4:13pm" today, "Sun Nov 22, 10:00pm" another day.
fn clock<Tz: TimeZone>(t: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String {
    let t = t.with_timezone(tz).naive_local();
    let today = now.with_timezone(tz).naive_local();
    let time = t.format("%-I:%M%P").to_string();
    if t.date() == today.date() {
        time
    } else if t.year() == today.year() {
        format!("{}, {time}", t.format("%a %b %-d"))
    } else {
        format!("{}, {time}", t.format("%a %b %-d %Y"))
    }
}

/// A span for a person, as the widget counts down: "4d 6h", "1h 58m", "12m".
fn span(secs: i64) -> String {
    let mins = (secs.max(0) + 30) / 60;
    let (days, hours, mins) = (mins / 1440, mins / 60 % 24, mins % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else {
        format!("{mins}m")
    }
}

/// "in 2h 13m", or "now" once it has passed; "" when unknown.
fn countdown(reset_at: &str, now: DateTime<Utc>) -> String {
    let Some(t) = parse(reset_at) else {
        return String::new();
    };
    let secs = t.signed_duration_since(now).num_seconds();
    if secs <= 0 {
        "now".to_string()
    } else {
        format!("in {}", span(secs))
    }
}

fn limit_line<Tz: TimeZone>(
    label: &str,
    pct: i64,
    amounts: Option<(i64, i64)>,
    reset_at: &str,
    now: DateTime<Utc>,
    tz: &Tz,
) -> UsageLine {
    let value = match amounts {
        Some((used, limit)) => format!("{} / {} · {pct}%", grouped(used), grouped(limit)),
        None => format!("{pct}%"),
    };
    UsageLine::Limit {
        label: label.to_string(),
        value,
        share: (pct.clamp(0, 100) as f32) / 100.0,
        level: Level::of(pct),
        resets: parse(reset_at)
            .map(|t| clock(t, now, tz))
            .unwrap_or_default(),
        resets_in: countdown(reset_at, now),
    }
}

/// 2060 as "2,060".
fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn dollars(x: f64) -> String {
    format!("${x:.2}")
}

/// "peak 94%": the fullest of a provider's limits, for its heading.
fn peak(pcts: impl Iterator<Item = i64>) -> String {
    pcts.max().map(|p| format!("peak {p}%")).unwrap_or_default()
}

fn titled(name: &str, plan: &str) -> String {
    if plan.is_empty() {
        name.to_string()
    } else {
        format!("{name} · {plan}")
    }
}

/// The providers in the widget's order: Claude, GLM, Kimi Code, agy, OpenRouter, then the
/// ones llm-exp couldn't ask.
fn limit_lines<Tz: TimeZone>(u: &LlmUsage, now: DateTime<Utc>, tz: &Tz, out: &mut Vec<UsageLine>) {
    if let Some(c) = &u.claude {
        let model = if c.model_name.is_empty() {
            "model".to_string()
        } else {
            c.model_name.to_lowercase()
        };
        let limits: [(String, &Limit); 3] = [
            ("session".to_string(), &c.session),
            ("week · all models".to_string(), &c.week_all),
            (format!("week · {model}"), &c.week_model),
        ];
        out.push(heading(
            &titled("claude", &c.plan),
            peak(limits.iter().map(|(_, l)| l.pct)),
        ));
        for (label, l) in &limits {
            out.push(limit_line(label, l.pct, None, &l.reset_at, now, tz));
        }
        if let Some(x) = &c.extra_usage {
            let money = format!("{} / {}", dollars(x.used_dollars), dollars(x.limit_dollars));
            if x.enabled {
                let mut line = limit_line("extra-usage credits", x.pct, None, "", now, tz);
                if let UsageLine::Limit { value, .. } = &mut line {
                    *value = format!("{money} · {}%", x.pct);
                }
                out.push(line);
            } else {
                let reason = if x.disabled_reason.is_empty() {
                    "off".to_string()
                } else {
                    format!("off ({})", x.disabled_reason.replace('_', " "))
                };
                out.push(UsageLine::Item {
                    name: "extra-usage credits".to_string(),
                    value: money,
                    detail: reason,
                });
            }
        }
    }
    let quotas: [(&str, &Option<QuotaBars>); 3] = [
        ("glm", &u.glm),
        ("kimi code", &u.kimi),
        ("agy · antigravity", &u.agy),
    ];
    for (name, q) in quotas {
        let Some(q) = q else { continue };
        out.push(heading(
            &titled(name, &q.plan),
            peak(q.bars.iter().map(|b| b.pct)),
        ));
        for b in &q.bars {
            out.push(quota_line(b, now, tz));
        }
        if q.bars.is_empty() {
            out.push(note("no limits in the snapshot", false));
        }
    }
    if let Some(o) = &u.openrouter {
        out.push(heading("openrouter", String::new()));
        if o.limit > 0.0 {
            let pct = (o.used / o.limit * 100.0) as i64;
            let mut line = limit_line("key spend", pct, None, "", now, tz);
            if let UsageLine::Limit { value, .. } = &mut line {
                *value = format!("{} / {}", dollars(o.used), dollars(o.limit));
            }
            out.push(line);
        } else {
            out.push(UsageLine::Item {
                name: "key spend".to_string(),
                value: dollars(o.used),
                detail: "no key limit".to_string(),
            });
        }
    }
    for (name, err) in &u.errors {
        out.push(heading(&format!("{name} · error"), String::new()));
        out.push(note(err, true));
    }
    if u.claude.is_none()
        && u.glm.is_none()
        && u.kimi.is_none()
        && u.agy.is_none()
        && u.openrouter.is_none()
        && u.errors.is_empty()
    {
        out.push(note("Plan limits: the snapshot names no provider.", false));
    }
}

fn quota_line<Tz: TimeZone>(b: &QuotaBar, now: DateTime<Utc>, tz: &Tz) -> UsageLine {
    let amounts = (b.limit > 0).then_some((b.used, b.limit));
    limit_line(&b.label, b.pct, amounts, &b.reset_at, now, tz)
}

/// The tokens the manager counted in the host's transcripts: every session's own model
/// calls, per harness. A different measure from the plan limits, labelled as such.
fn token_lines(h: &HostView, out: &mut Vec<UsageLine>) {
    let mut by: BTreeMap<&str, (Tokens, i64)> = BTreeMap::new();
    for s in &h.sessions {
        if let Some(u) = &s.usage {
            let e = by.entry(s.harness.as_str()).or_default();
            e.0.add(&u.tokens);
            e.1 += 1;
        }
    }
    let mut all = Tokens::default();
    for (t, _) in by.values() {
        all.add(t);
    }
    let mut summary = format!(
        "{} in {} model calls",
        human_tokens(all.total()),
        grouped(all.calls)
    );
    if h.usage_pending > 0 {
        summary.push_str(&format!("; {} sessions not counted yet", h.usage_pending));
    }
    out.push(heading("tokens in transcripts", summary));
    if by.is_empty() {
        out.push(note("No session's tokens are counted.", false));
        return;
    }
    for (harness, (t, sessions)) in &by {
        out.push(UsageLine::Item {
            name: harness.to_string(),
            value: human_tokens(t.total()),
            detail: format!(
                "{} sessions · input {}, output {}, cache read {}, cache write {}",
                grouped(*sessions),
                human_tokens(t.input),
                human_tokens(t.output),
                human_tokens(t.cache_read),
                human_tokens(t.cache_creation)
            ),
        });
    }
    out.push(note(
        "Counted by the manager from the sessions listed here; a different measure from the plan limits above.",
        false,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use chrono::{Duration, FixedOffset};

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn texts(lines: &[UsageLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match l {
                UsageLine::Host { name, note, .. } => format!("# {name} | {note}"),
                UsageLine::Heading { title, summary } => format!("## {title} | {summary}"),
                UsageLine::Limit {
                    label,
                    value,
                    resets,
                    resets_in,
                    ..
                } => format!("{label} | {value} | {resets} | {resets_in}"),
                UsageLine::Item {
                    name,
                    value,
                    detail,
                } => format!("{name} | {value} | {detail}"),
                UsageLine::Note { text, .. } => format!("> {text}"),
            })
            .collect()
    }

    #[test]
    fn each_host_shows_the_widgets_figures_then_its_tokens() {
        let now = fixture::default_now();
        let v = fixture::view(now);
        let lines = lines(&v, now, &utc(), &BTreeSet::new());
        let t = texts(&lines);
        let at = |s: &str| {
            t.iter()
                .position(|l| l == s)
                .unwrap_or_else(|| panic!("no {s:?} in {t:#?}"))
        };
        // hub: fetched 2 minutes before the view, at noon UTC.
        let hub = at("# hub | llm-exp fetched 11:58am (2m ago)");
        let claude = at("## claude · max | peak 94%");
        assert_eq!(t[claude + 1], "session | 37% | 2:13pm | in 2h 13m");
        assert_eq!(
            t[claude + 2],
            "week · all models | 94% | Sun Nov 22, 10:00pm | in 2d 10h"
        );
        assert_eq!(
            t[claude + 3],
            "week · opus | 61% | Sun Nov 22, 10:00pm | in 2d 10h"
        );
        assert_eq!(
            t[claude + 4],
            "extra-usage credits | $0.00 / $200.00 | off (out of credits)"
        );
        let glm = at("## glm · pro | peak 103%");
        assert_eq!(
            t[glm + 2],
            "weekly | 2,060 / 2,000 · 103% | Tue Nov 24, 12:00pm | in 4d 0h"
        );
        at("## agy · antigravity | peak 8%");
        let or = at("## openrouter | ");
        assert_eq!(t[or + 1], "key spend | $3.42 / $25.00 |  | ");
        let err = at("## kimi · error | ");
        assert_eq!(
            t[err + 1],
            "> refresh token rejected: run kimi login on this machine"
        );
        let tokens = t
            .iter()
            .position(|l| l.starts_with("## tokens in transcripts | "))
            .unwrap();
        assert!(hub < claude && claude < glm && glm < or && or < err && err < tokens);
        assert!(t[tokens + 1].starts_with("agy | "), "{}", t[tokens + 1]);

        // laptop didn't answer; phone answered with no snapshot.
        at("# laptop | unreachable: no bus answering for host laptop (no reply in 3s)");
        let phone = at("# phone | ");
        assert_eq!(
            t[phone + 1],
            "> Plan limits: not available here (no llm-exp usage snapshot on this host)."
        );
        assert!(
            t[phone + 2].ends_with("sessions not counted yet"),
            "{}",
            t[phone + 2]
        );

        // Levels as the widget colors them.
        let level = |label: &str| {
            lines
                .iter()
                .find_map(|l| match l {
                    UsageLine::Limit {
                        label: l,
                        level,
                        share,
                        ..
                    } if l == label => Some((*level, *share)),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(level("week · all models"), (Level::Warn, 0.94));
        assert_eq!(level("weekly"), (Level::Over, 1.0));
        assert_eq!(level("session"), (Level::Ok, 0.37));
    }

    #[test]
    fn a_folded_host_shows_only_its_header() {
        let now = fixture::default_now();
        let v = fixture::view(now);
        let folded = BTreeSet::from(["hub".to_string()]);
        let t = texts(&lines(&v, now, &utc(), &folded));
        assert!(t[0].starts_with("# hub"));
        assert!(t[1].starts_with("# laptop"), "{t:#?}");
        let UsageLine::Host { folded, .. } =
            &lines(&v, now, &utc(), &BTreeSet::from(["hub".into()]))[0]
        else {
            panic!()
        };
        assert!(*folded);
    }

    #[test]
    fn old_figures_and_unreadable_snapshots_are_flagged() {
        let now = fixture::default_now();
        let mut v = fixture::view(now);
        let u = v.hosts[0].llm_usage.as_mut().unwrap();
        u.captured_at = (now - Duration::hours(3)).to_rfc3339();
        let l = lines(&v, now, &utc(), &BTreeSet::new());
        assert_eq!(
            l[0],
            UsageLine::Host {
                host: 0,
                name: "hub".into(),
                note: "llm-exp fetched 9:00am (3h 00m ago)".into(),
                warn: true,
                reachable: true,
                folded: false,
            }
        );
        v.hosts[0].llm_usage = Some(LlmUsage::failed("/home/user/x.json", "reading it: denied"));
        let t = texts(&lines(&v, now, &utc(), &BTreeSet::new()));
        assert_eq!(t[1], "> Plan limits: /home/user/x.json: reading it: denied");
    }

    #[test]
    fn times_read_in_the_given_zone() {
        let now = fixture::default_now();
        let v = fixture::view(now);
        let cst = FixedOffset::west_opt(6 * 3600).unwrap();
        let t = texts(&lines(&v, now, &cst, &BTreeSet::new()));
        assert!(
            t.contains(&"session | 37% | 8:13am | in 2h 13m".to_string()),
            "{t:#?}"
        );
    }

    #[test]
    fn spans_and_amounts_read_like_the_widgets() {
        assert_eq!(span(59), "1m");
        assert_eq!(span(3 * 3600 + 5 * 60), "3h 05m");
        assert_eq!(span(4 * 86400 + 6 * 3600 + 59), "4d 6h");
        let now = fixture::default_now();
        assert_eq!(
            countdown(&(now - Duration::minutes(1)).to_rfc3339(), now),
            "now"
        );
        assert_eq!(countdown("", now), "");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(grouped(999), "999");
        assert_eq!(Level::of(90), Level::Ok);
        assert_eq!(Level::of(91), Level::Warn);
        assert_eq!(Level::of(101), Level::Over);
    }
}
