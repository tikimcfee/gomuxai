//! Package manifests, read just enough for `depends-on` edges, package-name
//! labels and seeds: `Cargo.toml` (path and workspace deps), `go.mod`,
//! `package.json` and `pyproject.toml`. No TOML parser: a line reader that
//! knows these files' common shapes.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// What one manifest says.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// `cargo`, `go`, `npm`, `python`.
    pub ecosystem: String,
    pub name: String,
    pub description: String,
    pub deps: Vec<Dep>,
    /// Cargo's `[workspace.dependencies]` with a path: name → path.
    pub workspace_paths: BTreeMap<String, String>,
    pub is_workspace: bool,
}

/// One dependency: by name, with a path when the manifest gives one.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Dep {
    pub name: String,
    pub path: Option<String>,
    /// `foo.workspace = true`: the path is the workspace's.
    pub workspace: bool,
}

/// The manifest kind of a file name, if it is one.
pub fn is_manifest(name: &str) -> bool {
    matches!(
        name,
        "Cargo.toml" | "go.mod" | "package.json" | "pyproject.toml"
    )
}

pub fn parse(name: &str, text: &str) -> Option<Manifest> {
    match name {
        "Cargo.toml" => Some(cargo(text)),
        "go.mod" => Some(gomod(text)),
        "package.json" => npm(text),
        "pyproject.toml" => Some(pyproject(text)),
        _ => None,
    }
}

static KV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*([A-Za-z0-9_.\-"']+)\s*=\s*(.*)$"#).unwrap());
static STR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^"((?:[^"\\]|\\.)*)""#).unwrap());
static PATH_IN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bpath\s*=\s*"([^"]+)""#).unwrap());
static WS_IN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bworkspace\s*=\s*true\b"#).unwrap());

fn string_value(v: &str) -> Option<String> {
    STR.captures(v.trim()).map(|c| c[1].replace("\\\"", "\""))
}

fn cargo(text: &str) -> Manifest {
    let mut m = Manifest {
        ecosystem: "cargo".into(),
        ..Default::default()
    };
    let mut section = String::new();
    // `[dependencies.foo]`: the dep a table names.
    let mut table_dep: Option<usize> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if t.starts_with('[') {
            section = t.trim_matches(['[', ']']).trim().to_string();
            table_dep = None;
            if section == "workspace" {
                m.is_workspace = true;
            }
            let dep_table = section
                .rsplit_once('.')
                .filter(|(head, _)| head.ends_with("dependencies"));
            if let Some((head, dep)) = dep_table
                && head != "workspace.dependencies"
            {
                m.deps.push(Dep {
                    name: dep.trim_matches('"').to_string(),
                    path: None,
                    workspace: false,
                });
                table_dep = Some(m.deps.len() - 1);
            }
            continue;
        }
        let Some(c) = KV.captures(t) else { continue };
        let key = c[1].trim_matches(['"', '\'']).to_string();
        let val = c[2].trim();
        if let Some(i) = table_dep {
            if key == "path" {
                m.deps[i].path = string_value(val);
            } else if key == "workspace" && val.starts_with("true") {
                m.deps[i].workspace = true;
            }
            continue;
        }
        match section.as_str() {
            "package" => match key.as_str() {
                "name" => m.name = string_value(val).unwrap_or_default(),
                "description" => m.description = string_value(val).unwrap_or_default(),
                _ => {}
            },
            "workspace.dependencies" => {
                if let Some(p) = PATH_IN.captures(val) {
                    m.workspace_paths.insert(key, p[1].to_string());
                }
            }
            s if s.ends_with("dependencies") => {
                let (name, ws_key) = match key.strip_suffix(".workspace") {
                    Some(n) => (n.to_string(), val.starts_with("true")),
                    None => (key.clone(), false),
                };
                m.deps.push(Dep {
                    name,
                    path: PATH_IN.captures(val).map(|p| p[1].to_string()),
                    workspace: ws_key || WS_IN.is_match(val),
                });
            }
            _ => {}
        }
    }
    m
}

fn gomod(text: &str) -> Manifest {
    let mut m = Manifest {
        ecosystem: "go".into(),
        ..Default::default()
    };
    let mut in_require = false;
    let mut replaces: BTreeMap<String, String> = BTreeMap::new();
    let mut in_replace = false;
    for line in text.lines() {
        let t = line.split("//").next().unwrap_or("").trim();
        if let Some(rest) = t.strip_prefix("module ") {
            m.name = rest.trim().trim_matches('"').to_string();
        } else if t.starts_with("require (") {
            in_require = true;
        } else if t.starts_with("replace (") {
            in_replace = true;
        } else if t == ")" {
            in_require = false;
            in_replace = false;
        } else if let Some(rest) = t.strip_prefix("require ") {
            if let Some(n) = rest.split_whitespace().next() {
                m.deps.push(Dep {
                    name: n.to_string(),
                    path: None,
                    workspace: false,
                });
            }
        } else if in_require {
            if let Some(n) = t.split_whitespace().next() {
                m.deps.push(Dep {
                    name: n.to_string(),
                    path: None,
                    workspace: false,
                });
            }
        } else if in_replace || t.starts_with("replace ") {
            let body = t.trim_start_matches("replace ").trim();
            if let Some((from, to)) = body.split_once("=>") {
                let from = from.split_whitespace().next().unwrap_or("").to_string();
                let to = to.split_whitespace().next().unwrap_or("").to_string();
                if to.starts_with('.') || to.starts_with('/') {
                    replaces.insert(from, to);
                }
            }
        }
    }
    for d in &mut m.deps {
        if let Some(p) = replaces.get(&d.name) {
            d.path = Some(p.clone());
        }
    }
    m
}

fn npm(text: &str) -> Option<Manifest> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut m = Manifest {
        ecosystem: "npm".into(),
        name: v["name"].as_str().unwrap_or("").to_string(),
        description: v["description"].as_str().unwrap_or("").to_string(),
        ..Default::default()
    };
    for sect in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(o) = v[sect].as_object() {
            for (k, val) in o {
                let s = val.as_str().unwrap_or("");
                let path = s
                    .strip_prefix("file:")
                    .or_else(|| s.strip_prefix("link:"))
                    .map(str::to_string);
                m.deps.push(Dep {
                    name: k.clone(),
                    path,
                    workspace: s.starts_with("workspace:"),
                });
            }
        }
    }
    Some(m)
}

/// A Python requirement's name: `foo-bar[x]>=1` → `foo-bar`.
fn req_name(r: &str) -> String {
    r.trim()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .next()
        .unwrap_or("")
        .to_string()
}

fn pyproject(text: &str) -> Manifest {
    let mut m = Manifest {
        ecosystem: "python".into(),
        ..Default::default()
    };
    let mut section = String::new();
    let mut in_deps = false;
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    static QUOTED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""([^"]+)"|'([^']+)'"#).unwrap());
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if in_deps {
            for c in QUOTED.captures_iter(t) {
                let r = c.get(1).or(c.get(2)).map(|x| x.as_str()).unwrap_or("");
                let n = req_name(r);
                if !n.is_empty() {
                    m.deps.push(Dep {
                        name: n,
                        path: None,
                        workspace: false,
                    });
                }
            }
            if t.contains(']') {
                in_deps = false;
            }
            continue;
        }
        if t.starts_with('[') {
            section = t.trim_matches(['[', ']']).trim().to_string();
            continue;
        }
        let Some(c) = KV.captures(t) else { continue };
        let key = c[1].trim_matches(['"', '\'']).to_string();
        let val = c[2].trim();
        match (section.as_str(), key.as_str()) {
            ("project", "name") | ("tool.poetry", "name") => {
                m.name = string_value(val).unwrap_or_default()
            }
            ("project", "description") | ("tool.poetry", "description") => {
                m.description = string_value(val).unwrap_or_default()
            }
            ("project", "dependencies") => {
                if val.starts_with('[') {
                    for c in QUOTED.captures_iter(val) {
                        let r = c.get(1).or(c.get(2)).map(|x| x.as_str()).unwrap_or("");
                        let n = req_name(r);
                        if !n.is_empty() {
                            m.deps.push(Dep {
                                name: n,
                                path: None,
                                workspace: false,
                            });
                        }
                    }
                    in_deps = !val.contains(']');
                }
            }
            ("tool.poetry.dependencies", k) if k != "python" => m.deps.push(Dep {
                name: k.to_string(),
                path: PATH_IN.captures(val).map(|p| p[1].to_string()),
                workspace: false,
            }),
            ("tool.uv.sources", k) => {
                if let Some(p) = PATH_IN.captures(val) {
                    sources.insert(k.to_string(), p[1].to_string());
                }
            }
            _ => {}
        }
    }
    for d in &mut m.deps {
        if let Some(p) = sources.get(&d.name) {
            d.path = Some(p.clone());
        }
    }
    m
}
