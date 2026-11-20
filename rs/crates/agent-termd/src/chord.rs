use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};

/// Variables termd sets itself in layer 6 and refuses in `create.env` (though
/// `create.env_base` may carry them).
const LAYER6_RESERVED: &[&str] = &[
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "G3D_TERMD_ID",
    "G3D_TERMD_SOCKET",
    "PWD",
];

fn is_internal_g3d_var(name: &str) -> bool {
    name.starts_with("G3D_RELAY_") || name.starts_with("G3D_TERMD_")
}

fn is_shell_transient_var(name: &str) -> bool {
    matches!(name, "SHLVL" | "OLDPWD" | "PWD" | "_")
}

fn scrub_caller_map(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.retain(|k, v| !is_internal_g3d_var(k) && !is_shell_transient_var(k) && !v.contains('\0'));
    env
}

/// Filters `"NAME=VALUE"` environment entries to those `g3d-termd`'s
/// `create.env_base` accepts (via `g3d::env::caller_env`), while scrubbing
/// `G3D_RELAY_*` capability tokens, `G3D_TERMD_*` internal controls, and
/// shell-transient state so they are never transmitted over the termd socket.
pub fn caller_env<I, S>(environ: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let pairs = environ
        .into_iter()
        .filter_map(|kv| split_os_kv(kv.as_ref()));
    let (base, _skipped) = g3d::env::caller_env(pairs);
    scrub_caller_map(base)
}

/// Filters [`std::env::vars_os`] for `create.env_base`.
pub fn caller_env_os() -> BTreeMap<String, String> {
    let (base, _skipped) = g3d::env::caller_env(std::env::vars_os());
    scrub_caller_map(base)
}

fn split_os_kv(kv: &OsStr) -> Option<(OsString, OsString)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = kv.as_bytes();
        let eq = bytes.iter().position(|&b| b == b'=')?;
        Some((
            OsStr::from_bytes(&bytes[..eq]).to_os_string(),
            OsStr::from_bytes(&bytes[eq + 1..]).to_os_string(),
        ))
    }
    #[cfg(not(unix))]
    {
        let s = kv.to_str()?;
        let (k, v) = s.split_once('=')?;
        Some((OsString::from(k), OsString::from(v)))
    }
}

pub(crate) fn sanitize_request_env(
    env: &HashMap<String, String>,
) -> Option<BTreeMap<String, Option<String>>> {
    if env.is_empty() {
        return None;
    }
    let mut out = BTreeMap::new();
    for (k, v) in env {
        if !g3d_wire::env_name_ok(k)
            || LAYER6_RESERVED.contains(&k.as_str())
            || is_internal_g3d_var(k)
            || v.contains('\0')
            || k.len() + v.len() + 2 > g3d_wire::ENV_VALUE_MAX
        {
            continue;
        }
        out.insert(k.clone(), Some(v.clone()));
    }
    (!out.is_empty()).then_some(out)
}

/// One W3C key event for termd's `"key"` op.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub option_as_alt: String,
}

impl KeyEvent {
    pub(crate) fn into_wire_key(self, handle: u32) -> g3d_wire::Key {
        g3d_wire::Key {
            handle,
            code: self.code,
            key: self.key,
            mods: self.mods,
            text: self.text,
            action: None,
            // OptionAsAlt::Auto is omitted on the wire in g3d-wire (the 1.3 default),
            // which works identically whether or not option_as_alt is advertised.
            option_as_alt: g3d_wire::OptionAsAlt::Auto,
        }
    }
}

/// Rewrites tmux modifier prefixes (`"C-c"`, `"M-Left"`, `"C-M-x"`) into `"+"`
/// chord notation (`"ctrl+c"`, `"alt+Left"`, `"ctrl+alt+x"`). Strings like
/// `"-N2"` are left untouched so they do not masquerade as modifiers.
fn normalize_tmux_chord(input: &str) -> String {
    let mut mods: Vec<&str> = Vec::new();
    let mut rest = input;
    while rest.len() >= 3 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0] {
            b'C' | b'c' => {
                mods.push("ctrl");
                rest = &rest[2..];
            }
            b'M' | b'm' => {
                mods.push("alt");
                rest = &rest[2..];
            }
            b'S' | b's' => {
                mods.push("shift");
                rest = &rest[2..];
            }
            _ => break,
        }
    }
    if mods.is_empty() || rest.is_empty() {
        return input.to_owned();
    }
    mods.push(rest);
    mods.join("+")
}

fn canonical_g3d_mod(m: &str) -> Option<&'static str> {
    match m.to_ascii_lowercase().as_str() {
        "shift" | "s" => Some("shift"),
        "ctrl" | "control" | "c" => Some("ctrl"),
        // tmux M-x is Escape/Alt.
        "alt" | "opt" | "option" | "m" => Some("alt"),
        "meta" | "super" | "cmd" | "command" => Some("super"),
        _ => None,
    }
}

/// Maps a tmux key name (`"Enter"`, `"C-c"`, `"BSpace"`, `"Up"`, `"1"`) or
/// `g3d` chord (`"shift+enter"`, `"ctrl+c"`, `"alt+left"`) to a W3C
/// [`KeyEvent`] for `g3d-termd`'s `"key"` op, delegating key/layout resolution
/// to `g3d::chord::parse`. Returns `None` when `raw` is not a recognized key or
/// chord (e.g. `"-N2"`), allowing callers to fall back to literal bytes as
/// `tmux send-keys` does.
pub fn parse_special_key(raw: &str) -> Option<KeyEvent> {
    if raw.is_empty() {
        return None;
    }
    let chord = normalize_tmux_chord(raw);

    let (mods_part, key_part) = if chord == "+" {
        ("", "+")
    } else if let Some(m) = chord.strip_suffix("++") {
        (m, "+")
    } else if let Some((m, k)) = chord.rsplit_once('+') {
        (m, k)
    } else {
        ("", chord.as_str())
    };
    if key_part.is_empty() {
        return None;
    }

    let mut g3d_mods: Vec<&str> = Vec::new();
    if !mods_part.is_empty() {
        for part in mods_part.split('+') {
            if part.is_empty() {
                return None;
            }
            g3d_mods.push(canonical_g3d_mod(part)?);
        }
    }

    // Translate tmux-only named-key aliases that g3d::chord::parse doesn't list.
    let mapped_key: &str = if key_part.chars().count() > 1 {
        let norm: String = key_part
            .to_ascii_lowercase()
            .chars()
            .filter(|&c| c != '_' && c != '-')
            .collect();
        match norm.as_str() {
            "btab" => {
                g3d_mods.push("shift");
                "tab"
            }
            "bspace" => "backspace",
            "ppage" => "pageup",
            "npage" => "pagedown",
            "dc" => "delete",
            "ic" => "insert",
            _ => key_part,
        }
    } else {
        key_part
    };

    let rebuilt = if g3d_mods.is_empty() {
        mapped_key.to_owned()
    } else {
        format!("{}+{mapped_key}", g3d_mods.join("+"))
    };

    let parsed = g3d::chord::parse(&rebuilt).ok()?;
    Some(KeyEvent {
        code: parsed.code,
        key: parsed.key,
        mods: parsed.mods,
        text: parsed.text,
        action: String::new(),
        option_as_alt: String::new(),
    })
}
