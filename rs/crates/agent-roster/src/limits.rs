//! Plan limits in availability (docs/ROSTER.md, "Availability"): how much
//! of its plan's limit an entry's harness has used, from llm-exp's usage
//! snapshot (agent_manage::llm_usage), which the caller reads and passes
//! in. The snapshot is per plan window, not per model, except Claude's one
//! named model.
use agent_manage::llm_usage::LlmUsage;
use chrono::{DateTime, Local, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

/// From this share of a window, an entry reads "near limit" and pick ranks
/// it down.
pub const NEAR_LIMIT_PCT: i64 = 90;

/// At this share, an entry is off until its window resets.
pub const AT_LIMIT_PCT: i64 = 100;

/// A snapshot older than this says nothing: the limits aren't known.
pub const STALE_AFTER: TimeDelta = TimeDelta::minutes(10);

/// How much pick takes away from an entry near its limit.
pub const NEAR_LIMIT_PENALTY: i64 = 2;

/// LimitView is the fullest window of an entry's plan, as the snapshot had
/// it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitView {
    /// Which window: "session", "week", "week fable", or agy's bar label.
    pub window: String,
    /// The share of it used, in percent.
    pub pct: i64,
    /// When it resets (RFC 3339); "" when the provider didn't say.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reset_at: String,
    /// When llm-exp captured the snapshot.
    pub captured_at: String,
}

impl LimitView {
    pub fn near(&self) -> bool {
        self.pct >= NEAR_LIMIT_PCT && !self.at()
    }

    pub fn at(&self) -> bool {
        self.pct >= AT_LIMIT_PCT
    }

    /// In words, for an entry: "week 22%, resets Fri 04:00", "near limit:
    /// week 93%, resets Fri 04:00", "at limit until Fri 04:00".
    pub fn describe(&self, now: DateTime<Utc>) -> String {
        let when = when(&self.reset_at, now);
        if self.at() {
            return format!("at limit until {when}");
        }
        let used = format!("{} {}%, resets {when}", self.window, self.pct);
        if self.near() {
            format!("near limit: {used}")
        } else {
            used
        }
    }
}

/// when says a reset time for a person: "Fri 04:00" within a week, else
/// its date; "an unknown time" when there is none.
pub fn when(reset_at: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(reset_at) else {
        return "an unknown time".to_string();
    };
    let t = t.with_timezone(&Utc);
    let local = t.with_timezone(&Local);
    if t > now && t - now < TimeDelta::days(6) {
        local.format("%a %H:%M").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// fresh returns the snapshot if it can be trusted at now: read without
/// error, and captured within STALE_AFTER.
pub fn fresh(usage: Option<&LlmUsage>, now: DateTime<Utc>) -> Option<&LlmUsage> {
    let u = usage?;
    if !u.error.is_empty() {
        return None;
    }
    let at = DateTime::parse_from_rfc3339(&u.captured_at).ok()?;
    let age = now - at.with_timezone(&Utc);
    if age > STALE_AFTER || age < -STALE_AFTER {
        return None;
    }
    Some(u)
}

/// entry_limit is the fullest window of the plan harness and model run on:
/// for claude, the highest of the session and the week, and the week of
/// its named model when the entry's model is that one; for agy, the
/// highest bar. None when that isn't known: no fresh snapshot, the provider
/// failed in it, or a harness it doesn't cover.
pub fn entry_limit(
    harness: &str,
    model: &str,
    usage: Option<&LlmUsage>,
    now: DateTime<Utc>,
) -> Option<LimitView> {
    let u = fresh(usage, now)?;
    if u.errors.contains_key(harness) {
        return None;
    }
    let mut windows: Vec<(String, i64, String)> = Vec::new();
    match harness {
        "claude" => {
            let c = u.claude.as_ref()?;
            windows.push(("session".into(), c.session.pct, c.session.reset_at.clone()));
            windows.push(("week".into(), c.week_all.pct, c.week_all.reset_at.clone()));
            let named = c.model_name.to_lowercase();
            if !named.is_empty() && model.to_lowercase().contains(&named) {
                windows.push((
                    format!("week {named}"),
                    c.week_model.pct,
                    c.week_model.reset_at.clone(),
                ));
            }
        }
        "agy" => {
            for b in &u.agy.as_ref()?.bars {
                windows.push((b.label.clone(), b.pct, b.reset_at.clone()));
            }
        }
        _ => return None,
    }
    // The fullest; on a tie, the first (session before week).
    let mut best: Option<(String, i64, String)> = None;
    for w in windows {
        if best.as_ref().is_none_or(|b| w.1 > b.1) {
            best = Some(w);
        }
    }
    let (window, pct, reset_at) = best?;
    Some(LimitView {
        window,
        pct,
        reset_at,
        captured_at: u.captured_at.clone(),
    })
}
