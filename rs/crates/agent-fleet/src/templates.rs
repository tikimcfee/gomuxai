//! Agent start templates (`<data dir>/templates.json`, see `docs/FLEET.md`
//! section `"Templates: starting agents across hosts"`).
//!
//! A host's `templates.json` lists every agent template this host agrees to
//! start on request. A start request carries only the template's name and who
//! asks; the host reads its own `templates.json` and builds the spawn from the
//! [`Template`] alone.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use agent_base::gostd::{filepath, os};
use agent_base::util::write_file_atomic;
use agent_bus::validate_agent;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

/// File name inside `<data dir>` holding this host's templates.
pub const TEMPLATES_FILE: &str = "templates.json";

/// Mode used when writing `<data dir>/templates.json`.
pub const TEMPLATES_FILE_MODE: u32 = 0o600;

/// Backends a template may name.
pub const BACKENDS: &[&str] = &["claude", "agy", "aider", "shell"];

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Returns `<data_dir>/templates.json`.
pub fn templates_path(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir.as_ref().join(TEMPLATES_FILE)
}

/// One template entry in `<data dir>/templates.json`.
///
/// Holds everything a `template start` needs to spawn the agent. Unknown JSON
/// fields in `templates.json` are kept in [`Template::extra`] so rewriting the
/// file preserves them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub model: String,
    #[serde(default, deserialize_with = "null_default")]
    pub cwd: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub brief: String,
    /// Unknown JSON fields preserved when `templates.json` is rewritten.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Template {
    /// Creates a template with no extra JSON fields.
    pub fn new(
        name: impl Into<String>,
        backend: impl Into<String>,
        model: impl Into<String>,
        cwd: impl Into<String>,
        brief: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            backend: backend.into(),
            model: model.into(),
            cwd: cwd.into(),
            brief: brief.into(),
            extra: serde_json::Map::new(),
        }
    }

    /// Returns the resolved path to `brief` (`None` when unset; resolved
    /// against `cwd` when relative).
    pub fn brief_path(&self) -> Option<PathBuf> {
        if self.brief.is_empty() {
            return None;
        }
        let p = Path::new(&self.brief);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(Path::new(&self.cwd).join(p))
        }
    }

    /// Validates this template against the host's filesystem:
    /// - `name` is a valid agent name (`agent_bus::validate_agent`),
    /// - `backend` is one of `claude`, `agy`, `aider`, or `shell`,
    /// - `cwd` is an existing absolute directory,
    /// - `brief`, if set, is an existing regular file.
    pub fn validate(&self) -> Result<()> {
        validate_agent(&self.name).with_context(|| format!("template {:?}", self.name))?;
        if !BACKENDS.contains(&self.backend.as_str()) {
            bail!(
                "template {:?}: unknown backend {:?} (use claude, agy, aider, or shell)",
                self.name,
                self.backend
            );
        }
        if self.cwd.is_empty() || !Path::new(&self.cwd).is_absolute() {
            bail!(
                "template {:?}: cwd {:?} must be an absolute directory path",
                self.name,
                self.cwd
            );
        }
        match fs::metadata(&self.cwd) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "template {:?}: cwd {} is not a directory",
                self.name,
                self.cwd
            ),
            Err(err) => bail!(
                "template {:?}: cwd {}: {}",
                self.name,
                self.cwd,
                os::errno_text(&err)
            ),
        }
        if let Some(brief) = self.brief_path() {
            match fs::metadata(&brief) {
                Ok(meta) if meta.is_file() => {}
                Ok(meta) if meta.is_dir() => bail!(
                    "template {:?}: brief {} is a directory, not a file",
                    self.name,
                    brief.display()
                ),
                Ok(_) => bail!(
                    "template {:?}: brief {} is not a regular file",
                    self.name,
                    brief.display()
                ),
                Err(err) => bail!(
                    "template {:?}: brief {}: {}",
                    self.name,
                    brief.display(),
                    os::errno_text(&err)
                ),
            }
        }
        Ok(())
    }

    /// Returns what is wrong with this template on disk (`None` when valid).
    pub fn problem(&self) -> Option<String> {
        self.validate().err().map(|e| format!("{e:#}"))
    }

    /// Converts this template to the wire [`agent_bus::Template`] shape,
    /// recording any validation failure in [`agent_bus::Template::problem`].
    pub fn to_wire(&self) -> agent_bus::Template {
        agent_bus::Template {
            name: self.name.clone(),
            backend: self.backend.clone(),
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            brief: self.brief.clone(),
            problem: self.problem().unwrap_or_default(),
        }
    }
}

impl From<Template> for agent_bus::Template {
    fn from(t: Template) -> Self {
        t.to_wire()
    }
}

impl From<&Template> for agent_bus::Template {
    fn from(t: &Template) -> Self {
        t.to_wire()
    }
}

impl From<agent_bus::Template> for Template {
    fn from(t: agent_bus::Template) -> Self {
        Self::new(t.name, t.backend, t.model, t.cwd, t.brief)
    }
}

/// Validates a slice of templates, checking each template and rejecting
/// duplicate names.
pub fn validate(templates: &[Template]) -> Result<()> {
    let mut seen = HashSet::with_capacity(templates.len());
    for t in templates {
        t.validate()?;
        if !seen.insert(t.name.as_str()) {
            bail!("duplicate template name {:?}", t.name);
        }
    }
    Ok(())
}

/// In-memory representation of `<data dir>/templates.json`, preserving unknown
/// top-level and per-template JSON fields across edits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Templates {
    #[serde(default, deserialize_with = "null_default")]
    pub templates: Vec<Template>,
    /// Unknown top-level JSON fields preserved when `templates.json` is rewritten.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Templates {
    /// Loads and validates `<data_dir>/templates.json`. A missing file returns
    /// an empty [`Templates`]; a malformed or invalid file returns an error
    /// naming the file and what is wrong.
    pub fn load(data_dir: impl AsRef<Path>) -> Result<Self> {
        Self::load_file(templates_path(data_dir))
    }

    /// Loads and validates `templates.json` from an explicit file path.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let Some(doc) = Self::read_raw(path)? else {
            return Ok(Self::default());
        };
        doc.validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(doc)
    }

    /// Parses `path` without running filesystem/field validation (used by
    /// [`remove`] so a template whose `cwd` or `brief` was deleted on disk can
    /// still be removed cleanly).
    fn read_raw(path: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                bail!("reading {}: {}", path.display(), os::errno_text(&e));
            }
        };
        let doc: Self = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("malformed {}: {e}", path.display()))?;
        Ok(Some(doc))
    }

    /// Validates every template and checks that no two share a name.
    pub fn validate(&self) -> Result<()> {
        validate(&self.templates)
    }

    /// Returns all templates in file order.
    pub fn list(&self) -> &[Template] {
        &self.templates
    }

    /// Looks up a template by name.
    pub fn get(&self, name: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Validates and adds `template`, refusing duplicate names.
    pub fn insert(&mut self, template: Template) -> Result<()> {
        template.validate()?;
        if self.get(&template.name).is_some() {
            bail!(
                "template {:?} already exists (remove it first with `agent template remove {}`)",
                template.name,
                template.name
            );
        }
        self.templates.push(template);
        Ok(())
    }

    /// Removes and returns the template named `name`, or errors if absent.
    pub fn remove_entry(&mut self, name: &str) -> Result<Template> {
        let Some(idx) = self.templates.iter().position(|t| t.name == name) else {
            bail!("no template named {name:?}");
        };
        Ok(self.templates.remove(idx))
    }

    /// Atomically writes `<data_dir>/templates.json` with mode `0600`,
    /// preserving unknown JSON fields.
    pub fn save(&self, data_dir: impl AsRef<Path>) -> Result<()> {
        self.save_file(templates_path(data_dir))
    }

    /// Atomically writes `path` with mode `0600`, preserving unknown JSON
    /// fields.
    pub fn save_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let parent_s = parent.to_string_lossy();
            os::mkdir_all(&parent_s, 0o755)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let mut out = serde_json::to_string_pretty(self)
            .with_context(|| format!("encoding {}", path.display()))?;
        out.push('\n');
        let path_str = filepath::clean(&path.to_string_lossy());
        write_file_atomic(&path_str, out.as_bytes(), TEMPLATES_FILE_MODE)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// Loads and validates `<data_dir>/templates.json` (missing file = empty).
pub fn load(data_dir: impl AsRef<Path>) -> Result<Templates> {
    Templates::load(data_dir)
}

/// Lists all templates in `<data_dir>/templates.json` (missing file = empty).
///
/// Individual templates whose `cwd` or `brief` has gone missing on disk are
/// still returned so callers can display them alongside their [`Template::problem`];
/// malformed JSON still fails naming the file.
pub fn list(data_dir: impl AsRef<Path>) -> Result<Vec<Template>> {
    let path = templates_path(data_dir);
    Ok(Templates::read_raw(&path)?.unwrap_or_default().templates)
}

/// Lists all templates in `<data_dir>/templates.json` as wire [`agent_bus::Template`]s,
/// marking any broken or duplicate entry via [`agent_bus::Template::problem`].
pub fn list_wire(data_dir: impl AsRef<Path>) -> Result<Vec<agent_bus::Template>> {
    let entries = list(data_dir)?;
    let mut seen = HashSet::with_capacity(entries.len());
    let mut out = Vec::with_capacity(entries.len());
    for t in &entries {
        let mut wire = t.to_wire();
        if !t.name.is_empty() && !seen.insert(t.name.as_str()) && wire.problem.is_empty() {
            wire.problem = format!("duplicate template name {:?}", t.name);
        }
        out.push(wire);
    }
    Ok(out)
}

/// Looks up `name` in `<data_dir>/templates.json`.
///
/// Validates only the named template (so a broken sibling template does not
/// block starting or resuming a healthy one), returning an error naming the
/// problem when `name` itself is invalid on disk.
pub fn get(data_dir: impl AsRef<Path>, name: &str) -> Result<Option<Template>> {
    let path = templates_path(data_dir);
    let Some(mut doc) = Templates::read_raw(&path)? else {
        return Ok(None);
    };
    let matches: Vec<usize> = doc
        .templates
        .iter()
        .enumerate()
        .filter_map(|(i, t)| (t.name == name).then_some(i))
        .collect();
    let Some(&idx) = matches.first() else {
        return Ok(None);
    };
    if matches.len() > 1 {
        bail!(
            "invalid {}: duplicate template name {:?}",
            path.display(),
            name
        );
    }
    let found = doc.templates.swap_remove(idx);
    found
        .validate()
        .with_context(|| format!("invalid {}", path.display()))?;
    Ok(Some(found))
}

/// Validates `template`, adds it to `<data_dir>/templates.json` (refusing
/// duplicate names), and writes the file atomically with mode `0600`.
pub fn add(data_dir: impl AsRef<Path>, template: Template) -> Result<()> {
    let path = templates_path(data_dir);
    let mut doc = Templates::load_file(&path)?;
    doc.insert(template)?;
    doc.validate()
        .with_context(|| format!("invalid {}", path.display()))?;
    doc.save_file(&path)
}

/// Removes the template named `name` from `<data_dir>/templates.json` and
/// rewrites the file atomically with mode `0600`, preserving unknown JSON
/// fields. Returns the removed [`Template`].
pub fn remove(data_dir: impl AsRef<Path>, name: &str) -> Result<Template> {
    let path = templates_path(data_dir);
    let Some(mut doc) = Templates::read_raw(&path)? else {
        bail!(
            "no template named {name:?} ({} does not exist)",
            path.display()
        );
    };
    let removed = doc.remove_entry(name)?;
    doc.validate()
        .with_context(|| format!("invalid {}", path.display()))?;
    doc.save_file(&path)?;
    Ok(removed)
}
