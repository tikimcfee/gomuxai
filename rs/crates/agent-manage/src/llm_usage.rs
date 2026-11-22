//! A host's LLM plan usage, as llm-exp's usage widget shows it: the share of each plan
//! limit used and when it resets, per provider.
//!
//! The figures are llm-exp's own (`llm-exp usage --json`, which asks each provider's API
//! with that machine's credentials). `agent` makes no such call and reads no credential: it
//! reads only the snapshot llm-exp caches for its widget, `~/.cache/llm-exp/usage-snapshot.json`,
//! as it is. A host without that file has no [`LlmUsage`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where llm-exp caches its snapshot, under the home directory.
pub const SNAPSHOT_PATH: &str = ".cache/llm-exp/usage-snapshot.json";

/// The snapshot. Every field but `path` and `error` is llm-exp's, under its own name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmUsage {
    /// The file it was read from.
    #[serde(default)]
    pub path: String,
    /// Why the file couldn't be read, when it couldn't; the rest is then empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// When llm-exp asked the providers (RFC 3339, with the offset it wrote).
    #[serde(default)]
    pub captured_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeLimits>,
    /// Z.AI's GLM coding plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glm: Option<QuotaBars>,
    /// Kimi Code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kimi: Option<QuotaBars>,
    /// agy (Antigravity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy: Option<QuotaBars>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrouter: Option<MoneyUsage>,
    /// A provider llm-exp couldn't ask, with its error.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, String>,
}

/// One limit: the share used, and when it resets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub pct: i64,
    /// RFC 3339; "" when the provider gave none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reset_at: String,
}

/// Claude's subscription limits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeLimits {
    /// The plan ("max", "pro").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    /// The 5-hour session.
    #[serde(default)]
    pub session: Limit,
    /// The week, all models.
    #[serde(default)]
    pub week_all: Limit,
    /// The week, the model `model_name` names.
    #[serde(default)]
    pub week_model: Limit,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_name: String,
    /// The extra-usage credit pool, on an account that has had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtraUsage {
    #[serde(default)]
    pub enabled: bool,
    /// Why it is off ("out_of_credits").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub disabled_reason: String,
    #[serde(default)]
    pub used_dollars: f64,
    #[serde(default)]
    pub limit_dollars: f64,
    #[serde(default)]
    pub pct: i64,
}

/// A plan's quota windows (GLM, Kimi Code, agy).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaBars {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    #[serde(default)]
    pub bars: Vec<QuotaBar>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaBar {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub pct: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reset_at: String,
    /// The amount used and the window's limit, when the provider says (0: it didn't).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub used: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub limit: i64,
}

/// An API key's spend (OpenRouter).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MoneyUsage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub used: f64,
    /// 0: the key has no limit.
    #[serde(default)]
    pub limit: f64,
    #[serde(default)]
    pub remaining: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// read_snapshot reads the snapshot llm-exp caches for its usage widget, when this host
/// has one (`$HOME/` [`SNAPSHOT_PATH`]). Only the file is read: no provider is asked and no
/// credential touched. A file that can't be read gives a [`LlmUsage`] saying why; no file
/// (or no `$HOME`) gives None. `agent manage view` and the roster both read it here.
pub fn read_snapshot() -> Option<LlmUsage> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let path = std::path::Path::new(&home)
        .join(SNAPSHOT_PATH)
        .to_string_lossy()
        .into_owned();
    match std::fs::read(&path) {
        Ok(data) => Some(LlmUsage::parse(&path, &data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(LlmUsage::failed(&path, &format!("reading it: {e}"))),
    }
}

impl LlmUsage {
    /// The snapshot in `data`, read from `path`. A file that isn't a snapshot gives a
    /// [`LlmUsage`] with only `path` and `error`.
    pub fn parse(path: &str, data: &[u8]) -> LlmUsage {
        match serde_json::from_slice::<LlmUsage>(data) {
            Ok(mut u) => {
                u.path = path.to_string();
                u.error = String::new();
                u
            }
            Err(e) => LlmUsage::failed(path, &format!("not an llm-exp usage snapshot: {e}")),
        }
    }

    pub fn failed(path: &str, why: &str) -> LlmUsage {
        LlmUsage {
            path: path.to_string(),
            error: why.to_string(),
            ..LlmUsage::default()
        }
    }

    /// One line for a person: each provider's limits used, "claude max: session 0%, week
    /// 22%, week fable 12%; agy: gemini 5h 0%", then which providers failed.
    pub fn summary(&self) -> String {
        if !self.error.is_empty() {
            return self.error.clone();
        }
        let mut parts = Vec::new();
        let named = |name: &str, plan: &str| {
            if plan.is_empty() {
                name.to_string()
            } else {
                format!("{name} {plan}")
            }
        };
        if let Some(c) = &self.claude {
            let model = if c.model_name.is_empty() {
                "model"
            } else {
                &c.model_name
            };
            parts.push(format!(
                "{}: session {}%, week {}%, week {} {}%",
                named("claude", &c.plan),
                c.session.pct,
                c.week_all.pct,
                model.to_lowercase(),
                c.week_model.pct
            ));
        }
        for (name, q) in [("glm", &self.glm), ("kimi", &self.kimi), ("agy", &self.agy)] {
            if let Some(q) = q {
                let bars: Vec<String> = q
                    .bars
                    .iter()
                    .map(|b| format!("{} {}%", b.label, b.pct))
                    .collect();
                parts.push(format!("{}: {}", named(name, &q.plan), bars.join(", ")));
            }
        }
        if let Some(o) = &self.openrouter {
            if o.limit > 0.0 {
                parts.push(format!("openrouter: ${:.2} of ${:.2}", o.used, o.limit));
            } else {
                parts.push(format!("openrouter: ${:.2}", o.used));
            }
        }
        if !self.errors.is_empty() {
            let names: Vec<&str> = self.errors.keys().map(String::as_str).collect();
            parts.push(format!("failed: {}", names.join(", ")));
        }
        if parts.is_empty() {
            return "no provider in it".to_string();
        }
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot as llm-exp writes it, including a field this side doesn't know.
    const SNAPSHOT: &str = r#"{
      "captured_at": "2026-03-04T12:32:54.24553684-06:00",
      "source": "live",
      "claude": {
        "plan": "max",
        "session": {"pct": 0},
        "week_all": {"pct": 22, "reset_at": "2026-03-05T04:00:00.379427+00:00"},
        "week_model": {"pct": 12, "reset_at": "2026-03-05T04:00:00.379586+00:00"},
        "model_name": "Fable",
        "extra_usage": {"enabled": false, "disabled_reason": "out_of_credits",
                        "used_dollars": 0, "limit_dollars": 200, "pct": 0}
      },
      "glm": {"plan": "pro", "bars": [{"label": "5h", "pct": 40, "used": 400, "limit": 1000}]},
      "agy": {"bars": [{"label": "gemini 5h", "pct": 0, "reset_at": "2026-03-04T22:18:09Z"}]},
      "openrouter": {"used": 1.5, "limit": 10, "currency": "USD"},
      "errors": {"kimi": "token rejected"}
    }"#;

    #[test]
    fn a_snapshot_is_read_as_llm_exp_wrote_it() {
        let u = LlmUsage::parse(
            "/home/user/.cache/llm-exp/usage-snapshot.json",
            SNAPSHOT.as_bytes(),
        );
        assert_eq!(u.error, "");
        assert_eq!(u.path, "/home/user/.cache/llm-exp/usage-snapshot.json");
        assert_eq!(u.captured_at, "2026-03-04T12:32:54.24553684-06:00");
        let c = u.claude.as_ref().unwrap();
        assert_eq!((c.plan.as_str(), c.model_name.as_str()), ("max", "Fable"));
        assert_eq!(c.session, Limit::default());
        assert_eq!(
            (c.week_all.pct, c.week_all.reset_at.as_str()),
            (22, "2026-03-05T04:00:00.379427+00:00")
        );
        let x = c.extra_usage.as_ref().unwrap();
        assert_eq!(
            (x.enabled, x.disabled_reason.as_str(), x.limit_dollars),
            (false, "out_of_credits", 200.0)
        );
        let g = &u.glm.as_ref().unwrap().bars[0];
        assert_eq!((g.pct, g.used, g.limit), (40, 400, 1000));
        assert!(u.kimi.is_none());
        assert_eq!(u.agy.as_ref().unwrap().bars[0].label, "gemini 5h");
        assert_eq!(u.openrouter.as_ref().unwrap().limit, 10.0);
        assert_eq!(u.errors["kimi"], "token rejected");
        assert_eq!(
            u.summary(),
            "claude max: session 0%, week 22%, week fable 12%; glm pro: 5h 40%; \
             agy: gemini 5h 0%; openrouter: $1.50 of $10.00; failed: kimi"
        );

        // What `agent manage view` sends reads back the same.
        let again: LlmUsage = serde_json::from_str(&serde_json::to_string(&u).unwrap()).unwrap();
        assert_eq!(again, u);
    }

    #[test]
    fn a_file_that_isnt_a_snapshot_says_so() {
        let u = LlmUsage::parse("/x", b"{not json");
        assert!(
            u.error.starts_with("not an llm-exp usage snapshot"),
            "{u:?}"
        );
        assert_eq!((u.path.as_str(), u.claude.is_none()), ("/x", true));
    }
}
