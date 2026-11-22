//! Coalescing held messages into a single structured delivery payload.
//!
//! Multiple held messages (including any replaced while pending) are rendered
//! as one chronological block, oldest first and newest last:
//! - a single active message with no superseded entries takes a minimal fast
//!   path (preserving existing single-message formatting);
//! - explicitly superseded messages keep their full text fenced under
//!   `[SUPERSEDED by #N — context only, do NOT act on this]` so relative
//!   follow-ups retain context, collapsing to a 140-char preview only when a
//!   Claude batch would exceed `CLAUDE_INLINE_SOFT_CAP`;
//! - multiple active messages from the same sender carry an explicit hint
//!   pointing to the newer message below, without silently discarding additive
//!   messages;
//! - batches exceeding Claude Code's 10,000-character inline `additionalContext`
//!   cap include a top manifest so Claude's 2,000-character preview alerts the
//!   model to read the full persisted file.

use std::collections::HashMap;

/// Soft inline threshold for Claude Code's 10,000-character `additionalContext`
/// cap (leaving room for the top manifest line).
pub const CLAUDE_INLINE_SOFT_CAP: usize = 9_500;

/// Maximum characters kept in a collapsed preview when a batch exceeds
/// `CLAUDE_INLINE_SOFT_CAP`.
const MAX_SUPERSEDED_PREVIEW: usize = 140;

/// One message in a claimed delivery batch, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoalescedMessage {
    pub id: i64,
    pub sender: String,
    pub content: String,
    /// Seconds elapsed since the message was queued (clamped >= 0).
    pub age_secs: i64,
    pub urgency: String,
    /// Non-zero when a newer message replaced this one while still pending.
    pub superseded_by: i64,
    pub topic: String,
}

impl CoalescedMessage {
    pub fn is_superseded(&self) -> bool {
        self.superseded_by > 0
    }
}

/// Formats a delivery batch into one prompt/context string.
///
/// `claude_header` adds Claude's `"A message arrived while you were working:"`
/// single-message prefix (which `agent-transcript` matches as `ORIGIN_DELIVERED`)
/// and inserts the >10k-character manifest warning when needed.
pub fn format_coalesced_batch(batch: &[CoalescedMessage], claude_header: bool) -> String {
    if batch.is_empty() {
        return String::new();
    }
    let active: Vec<&CoalescedMessage> = batch.iter().filter(|m| !m.is_superseded()).collect();
    if active.is_empty() {
        return String::new();
    }
    let superseded_count = batch.len() - active.len();

    // Fast path: 1 active message, 0 superseded.
    if active.len() == 1 && superseded_count == 0 {
        let m = active[0];
        let urgent = m.urgency == "interrupt";
        return match (claude_header, urgent) {
            (true, false) => format!("A message arrived while you were working:\n\n{}", m.content),
            (true, true) => format!(
                "An urgent message arrived while you were working:\n\n[URGENT INTERRUPT]\n{}",
                m.content
            ),
            (false, false) => m.content.clone(),
            (false, true) => format!("[URGENT INTERRUPT]\n{}", m.content),
        };
    }

    let latest_active_id = active.last().map(|m| m.id).unwrap_or(0);
    let full = render_sections(batch, &active, superseded_count, latest_active_id, false);

    if claude_header && full.chars().count() > CLAUDE_INLINE_SOFT_CAP {
        let compact = if superseded_count > 0 {
            render_sections(batch, &active, superseded_count, latest_active_id, true)
        } else {
            full
        };
        if compact.chars().count() <= CLAUDE_INLINE_SOFT_CAP {
            return compact;
        }
        let ids: Vec<String> = active
            .iter()
            .map(|m| {
                if m.id == latest_active_id {
                    format!("#{} (latest)", m.id)
                } else {
                    format!("#{}", m.id)
                }
            })
            .collect();
        let (head, body) = compact.split_once("\n\n").unwrap_or(("", compact.as_str()));
        let manifest = format!(
            "[NOTE: this batch is {} chars, exceeding Claude Code's 10,000-char inline hook limit — read the full persisted file before acting; active messages oldest→newest: {}]",
            compact.chars().count(),
            ids.join(", ")
        );
        return format!("{head}\n\n{manifest}\n\n{body}");
    }

    full
}

fn render_sections(
    batch: &[CoalescedMessage],
    active: &[&CoalescedMessage],
    superseded_count: usize,
    latest_active_id: i64,
    collapse_superseded: bool,
) -> String {
    let mut latest_by_sender: HashMap<&str, i64> = HashMap::new();
    for m in active {
        if !m.sender.is_empty() {
            latest_by_sender.insert(m.sender.as_str(), m.id);
        }
    }
    let mut supersedes_map: HashMap<i64, Vec<i64>> = HashMap::new();
    for m in batch {
        if m.is_superseded() {
            supersedes_map
                .entry(m.superseded_by)
                .or_default()
                .push(m.id);
        }
    }

    let head = if superseded_count > 0 {
        format!(
            "{} messages arrived while you were working ({} active, {} superseded), oldest first — read all before acting; superseded messages are for context only, and newer messages from the same sender override earlier ones if they conflict:",
            batch.len(),
            active.len(),
            superseded_count
        )
    } else {
        format!(
            "{} messages arrived while you were working, oldest first — read all before acting; newer messages from the same sender override earlier ones if they conflict:",
            active.len()
        )
    };

    let mut sections = Vec::with_capacity(batch.len());
    for m in batch {
        let age = format_age(m.age_secs);
        if m.is_superseded() {
            let body = if collapse_superseded {
                collapsed_preview(&m.content, MAX_SUPERSEDED_PREVIEW)
            } else {
                m.content.clone()
            };
            sections.push(format!(
                "--- #{} ({age}) [SUPERSEDED by #{} — context only, do NOT act on this] ---\n{body}",
                m.id, m.superseded_by
            ));
            continue;
        }
        let mut notes: Vec<String> = Vec::new();
        if m.urgency == "interrupt" {
            notes.push("URGENT INTERRUPT".to_string());
        } else if m.urgency == "now" {
            notes.push("urgent".to_string());
        }
        if !m.topic.is_empty() {
            notes.push(format!("topic: {}", m.topic));
        }
        if let Some(replaced) = supersedes_map.get(&m.id) {
            let ids: Vec<String> = replaced.iter().map(|id| format!("#{id}")).collect();
            notes.push(format!("supersedes {}", ids.join(", ")));
        }
        if !m.sender.is_empty()
            && let Some(&newer_id) = latest_by_sender.get(m.sender.as_str())
            && newer_id != m.id
        {
            notes.push(format!(
                "earlier from {} — newer #{newer_id} follows",
                m.sender
            ));
        }
        if m.id == latest_active_id && active.len() > 1 {
            notes.push("latest".to_string());
        }
        let meta = if notes.is_empty() {
            age
        } else {
            format!("{age}; {}", notes.join("; "))
        };
        sections.push(format!("--- #{} ({meta}) ---\n{}", m.id, m.content));
    }

    format!("{head}\n\n{}", sections.join("\n\n"))
}

/// Short one-line user-visible banner for mid-turn hook delivery (Claude
/// `PostToolBatch` `systemMessage`).
pub fn mid_turn_banner(batch: &[CoalescedMessage]) -> String {
    let active = batch.iter().filter(|m| !m.is_superseded()).count();
    let superseded = batch.len() - active;
    let noun = if active == 1 { "message" } else { "messages" };
    if superseded > 0 {
        format!("[gomuxai] Delivered {active} held {noun} ({superseded} superseded) mid-turn")
    } else {
        format!("[gomuxai] Delivered {active} held {noun} mid-turn")
    }
}

fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        let m = s / 60;
        let r = s % 60;
        if r == 0 {
            format!("{m}m ago")
        } else {
            format!("{m}m{r}s ago")
        }
    } else {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        format!("{h}h{m}m ago")
    }
}

fn collapsed_preview(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    let first = trimmed.lines().next().unwrap_or("");
    let more_lines = trimmed.contains('\n');
    let mut chars = first.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() || more_lines {
        format!("{head} …")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: i64, sender: &str, content: &str, age_secs: i64) -> CoalescedMessage {
        CoalescedMessage {
            id,
            sender: sender.into(),
            content: content.into(),
            age_secs,
            urgency: "soon".into(),
            superseded_by: 0,
            topic: String::new(),
        }
    }

    #[test]
    fn single_message_fast_path() {
        let b = [msg(1, "lead", "[from lead@h] hi", 5)];
        assert_eq!(
            format_coalesced_batch(&b, true),
            "A message arrived while you were working:\n\n[from lead@h] hi"
        );
        assert_eq!(format_coalesced_batch(&b, false), "[from lead@h] hi");
    }

    #[test]
    fn multi_message_keeps_full_superseded_text_and_same_sender_hints() {
        let mut m1 = msg(
            41,
            "lead",
            "[from lead@h] Start approach A\nkeep Option<Token> in auth.rs",
            130,
        );
        m1.superseded_by = 43;
        let m2 = msg(42, "review", "[from review@h] Bug in auth.rs", 95);
        let m3 = msg(
            43,
            "lead",
            "[from lead@h] Switch to approach B, keep line 2 above",
            40,
        );
        let m4 = msg(44, "lead", "[from lead@h] Also skip slow tests", 10);

        let out = format_coalesced_batch(&[m1, m2, m3, m4], true);
        assert!(out.starts_with(
            "4 messages arrived while you were working (3 active, 1 superseded), oldest first"
        ));
        assert!(out.contains(
            "--- #41 (2m10s ago) [SUPERSEDED by #43 — context only, do NOT act on this] ---\n[from lead@h] Start approach A\nkeep Option<Token> in auth.rs"
        ));
        assert!(out.contains("--- #42 (1m35s ago) ---"));
        assert!(out.contains("supersedes #41"));
        assert!(out.contains("earlier from lead — newer #44 follows"));
        assert!(out.contains("--- #44 (10s ago; latest) ---"));
        let p41 = out.find("--- #41 ").unwrap();
        let p42 = out.find("--- #42 ").unwrap();
        let p43 = out.find("--- #43 ").unwrap();
        let p44 = out.find("--- #44 ").unwrap();
        assert!(
            p41 < p42 && p42 < p43 && p43 < p44,
            "must stay oldest→newest"
        );
    }

    #[test]
    fn claude_over_cap_adds_top_manifest() {
        let big = format!("[from lead@h] {}", "x".repeat(6000));
        let b = [msg(10, "lead", &big, 20), msg(11, "lead", &big, 5)];
        let out = format_coalesced_batch(&b, true);
        assert!(out.contains("exceeding Claude Code's 10,000-char inline hook limit"));
        assert!(out.contains("#10, #11 (latest)"));
        assert!(
            out.find("read the full persisted file").unwrap() < 400,
            "manifest must sit inside Claude's 2,000-char preview"
        );
    }
}
