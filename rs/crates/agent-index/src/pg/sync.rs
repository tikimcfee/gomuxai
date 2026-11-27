//! The sync: v1's crawl ledger (`index.db`) mirrored into Postgres,
//! incrementally (docs/INDEX-V2.md, iteration 1; interim, until the crawl's
//! bookkeeping moves into Postgres).
//!
//! Each entity's row is worked out from the ledger (its card text, the terms
//! that split its identifiers, its labels, its children's names) and hashed
//! into a sync key; only rows whose key differs from Postgres's are written,
//! through COPY into a staging table and one upsert whose `RETURNING old.*,
//! new.*` says which rows' card text changed (queued for embedding) and which
//! files' content changed (cut into chunks again, from the file on disk).
//! Edges, aliases, dangling references, projects and roots are compared by a
//! digest of the whole table and replaced when it moved. Every step runs in
//! one transaction, so a reader sees the index before or after a sync, never
//! half of one.
//!
//! `rebuild` empties the schema's tables first and writes everything again
//! (keeping card vectors whose text is unchanged): the result is the same
//! rows an incremental sync reaches.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Serialize;

use super::{Pg, copy_row};
use crate::config::{Cards, Chunks as ChunkCfg, Config};
use crate::crawl::hash;
use crate::model::{ENTITY_COLS, Entity};
use crate::names;

/// What a sync did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub entities: usize,
    /// Of them, memory notes, friction entries and sessions.
    pub side: usize,
    pub side_edges: usize,
    /// What reading the sessions did (set by the caller that read them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<super::sessions::SessionsReport>,
    pub written: usize,
    pub deleted: usize,
    pub queued: usize,
    pub chunked_files: usize,
    pub chunks: usize,
    /// The entities whose chunks were cut again (the watcher embeds their
    /// windows).
    #[serde(skip)]
    pub rechunked: Vec<String>,
    pub labels: usize,
    /// Files whose recorded mtime moved (the stale check reads it).
    pub mtimes: usize,
    pub edges_replaced: bool,
    pub edges: usize,
    pub tables_replaced: Vec<String>,
    pub rebuild: bool,
    pub ms: u128,
    /// Milliseconds per step.
    pub phases: BTreeMap<String, u128>,
}

// What files go into chunks and how they are cut, what is counted, and
// what a card holds are in crate::config (`[chunks]`, `[cards]`).

/// Rows a COPY sends at a time.
const COPY_BATCH: usize = 20_000;

/// The text an entity's vector is made from: v1's card (its project and
/// path, title, summary or seed, why, and the names it defines), and a
/// model's asks when `[cards].asks` (they go after the names, so a card
/// without asks is as before asks existed).
#[allow(clippy::too_many_arguments)]
pub fn card(
    project: &str,
    rel: &str,
    title: &str,
    summary: &str,
    why: &str,
    symbols: &str,
    asks: &[String],
    cards: &Cards,
) -> String {
    // (A note outside any project starts with its name alone.)
    let mut s = if rel.is_empty() {
        project.to_string()
    } else if project.is_empty() {
        rel.to_string()
    } else {
        format!("{project}/{rel}")
    };
    for part in [title, summary] {
        if !part.is_empty() && !s.contains(part) {
            s.push('\n');
            s.push_str(part);
        }
    }
    if !why.is_empty() {
        s.push_str("\nWhy: ");
        s.push_str(why);
    }
    let mut seen = HashSet::new();
    let syms: Vec<&str> = symbols
        .split_whitespace()
        .filter(|w| seen.insert(*w))
        .take(cards.symbols)
        .collect();
    if !syms.is_empty() {
        s.push_str("\ndefines: ");
        s.push_str(&syms.join(", "));
    }
    if cards.asks && !asks.is_empty() {
        s.push_str("\nasks: ");
        s.push_str(&asks.join("; "));
    }
    s.chars().take(cards.max_chars).collect()
}

/// An entity's name as words: an identifier whole and split; a file's stem
/// whole and split and its extension; every word of a symbol's name.
pub fn name_terms_of(e: &Entity) -> String {
    match e.kind.as_str() {
        "symbol" => e
            .name
            .split_whitespace()
            .map(names::whole_and_split)
            .collect::<Vec<_>>()
            .join(" "),
        "file" => match e.name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => {
                format!("{} {}", names::whole_and_split(stem), ext.to_lowercase())
            }
            _ => names::whole_and_split(&e.name),
        },
        _ => names::whole_and_split(&e.name),
    }
}

/// A path's words: its parts (the parser would keep `a/b.rs` as one token),
/// and their camelCase and `_` splits, each once.
pub fn path_terms(rel: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for t in rel.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let t = t.to_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    for w in names::split_words(rel) {
        if seen.insert(w.clone()) {
            out.push(w);
        }
    }
    out.join(" ")
}

/// The words of a chunk's camelCase identifiers (`gatherGlobal` → `gather
/// global`), which the text search parser keeps as one token; snake_case it
/// splits itself.
pub fn camel_terms(text: &str) -> String {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if tok.len() < 3
            || !tok.chars().any(|c| c.is_uppercase())
            || !tok.chars().any(|c| c.is_lowercase())
        {
            continue;
        }
        let words = names::split_words(tok);
        if words.len() < 2 {
            continue;
        }
        for w in words {
            if seen.insert(w.clone()) {
                out.push(w);
            }
        }
    }
    out.join(" ")
}

/// One chunk of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub line_start: usize,
    pub line_end: usize,
    pub heading: String,
    /// The headings above it, outermost first, its own last (Markdown).
    pub path: Vec<String>,
    pub text: String,
}

/// Cuts a file into chunks: a Markdown file by its headings (outside code
/// fences), anything else in windows of `max_lines` lines; no chunk over
/// `max_bytes` (a longer line is cut into pieces).
pub fn chunk_text(text: &str, markdown: bool, c: &ChunkCfg) -> Vec<Chunk> {
    let (max_lines, max_bytes) = (c.max_lines, c.max_bytes);
    // Lines, a long one cut into pieces (all on its line number).
    let mut lines: Vec<(usize, &str)> = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let mut rest = l;
        while rest.len() > max_bytes {
            let mut cut = max_bytes;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            lines.push((i + 1, &rest[..cut]));
            rest = &rest[cut..];
        }
        lines.push((i + 1, rest));
    }
    // Sections: (heading, heading path, lines).
    type Section<'a> = (String, Vec<String>, Vec<(usize, &'a str)>);
    let mut sections: Vec<Section> = vec![(String::new(), Vec::new(), Vec::new())];
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut fence = false;
    for &(n, l) in &lines {
        let t = l.trim_start();
        if markdown && (t.starts_with("```") || t.starts_with("~~~")) {
            fence = !fence;
        }
        if markdown && !fence && t.starts_with('#') {
            let h = t.trim_start_matches('#');
            if h.starts_with(' ') || h.is_empty() {
                let level = t.len() - h.len();
                while stack.last().is_some_and(|(lv, _)| *lv >= level) {
                    stack.pop();
                }
                stack.push((level, h.trim().to_string()));
                let path = stack.iter().map(|(_, s)| s.clone()).collect();
                sections.push((h.trim().to_string(), path, Vec::new()));
            }
        }
        sections
            .last_mut()
            .expect("one section at least")
            .2
            .push((n, l));
    }
    let mut out = Vec::new();
    for (heading, path, ls) in sections {
        let mut cur: Vec<(usize, &str)> = Vec::new();
        let mut bytes = 0;
        let flush = |cur: &mut Vec<(usize, &str)>, out: &mut Vec<Chunk>| {
            if cur.iter().any(|(_, l)| !l.trim().is_empty()) {
                out.push(Chunk {
                    line_start: cur[0].0,
                    line_end: cur[cur.len() - 1].0,
                    heading: heading.clone(),
                    path: path.clone(),
                    text: cur.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n"),
                });
            }
            cur.clear();
        };
        for (n, l) in ls {
            if !cur.is_empty() && (cur.len() >= max_lines || bytes + l.len() + 1 > max_bytes) {
                flush(&mut cur, &mut out);
                bytes = 0;
            }
            bytes += l.len() + 1;
            cur.push((n, l));
        }
        flush(&mut cur, &mut out);
    }
    out
}

/// One entity as the sync writes it.
struct Out {
    e: Entity,
    /// code, prose, memory, friction (see [`corpus_of`]).
    corpus: String,
    /// A note's text to cut into chunks (memory); None for a file, whose
    /// chunks are cut from the file on disk.
    body: Option<String>,
    /// A note's chunks, cut already (a session's turns).
    side_chunks: Vec<SideChunk>,
    seed: String,
    summary: String,
    why: String,
    summary_key: String,
    summary_writer: String,
    summary_at: String,
    asks: Vec<String>,
    terms: String,
    labels_text: String,
    symbols: String,
    embed: bool,
    text_key: String,
    chunk_key: String,
    sync_key: String,
}

const STAGE_COLS: &str = "id, kind, project, parent, rel, name, path, file_kind, lang, size, lines, line, \
     content_key, title, signature, untracked, no_content, importance, seed, summary, why, summary_key, \
     summary_writer, summary_at, asks, terms, labels_text, symbols, embed, text_key, chunk_key, sync_key, corpus, mtime";

/// The corpus a crawled entity belongs to (docs/INDEX-V2.md, "Corpora,
/// composed"): a document (Markdown, text, reStructuredText: file kind
/// `doc`) and its sections are prose wherever they live, a README in a repo
/// as much as a note in a research tree; everything else the crawl finds is
/// code. One entity, one corpus.
pub fn corpus_of(kind: &str, file_kind: &str) -> &'static str {
    match (kind, file_kind) {
        ("file", "doc") | ("section", _) => "prose",
        _ => "code",
    }
}

/// What the chunks' meta records; changing it rewrites every chunk once.
const CHUNK_META_VERSION: &str = "m1";

impl Out {
    /// The row as the sync key hashes it.
    fn copy(&self, buf: &mut String) {
        self.copy_with(buf, None);
    }

    /// The row as it is staged: with `mtime` after it (not part of the sync
    /// key: a touched file isn't a changed one).
    fn copy_with(&self, buf: &mut String, mtime: Option<&str>) {
        let e = &self.e;
        let asks = pg_array(&self.asks);
        let (size, lines, line, imp) = (
            e.size.to_string(),
            e.lines.to_string(),
            e.line.to_string(),
            e.importance.to_string(),
        );
        let mut fields: Vec<&str> = vec![
            &e.id,
            &e.kind,
            &e.project,
            &e.parent,
            &e.rel,
            &e.name,
            &e.path,
            &e.file_kind,
            &e.lang,
            &size,
            &lines,
            &line,
            &e.content_key,
            &e.title,
            &e.signature,
            if e.untracked { "t" } else { "f" },
            &e.no_content,
            &imp,
            &self.seed,
            &self.summary,
            &self.why,
            &self.summary_key,
            &self.summary_writer,
            &self.summary_at,
            &asks,
            &self.terms,
            &self.labels_text,
            &self.symbols,
            if self.embed { "t" } else { "f" },
            &self.text_key,
            &self.chunk_key,
            &self.sync_key,
            &self.corpus,
        ];
        fields.extend(mtime);
        copy_row(buf, &fields);
    }
}

/// A label row, as both stores hold it.
type Label = (String, String, String, String, String, f64, String);

/// Labels by entity id.
type Labels = HashMap<String, Vec<Label>>;

/// A text array in Postgres's literal form (`{"a","b"}`).
fn pg_array(items: &[String]) -> String {
    let mut s = String::from("{");
    for (i, a) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        for c in a.chars() {
            if c == '"' || c == '\\' {
                s.push('\\');
            }
            s.push(c);
        }
        s.push('"');
    }
    s.push('}');
    s
}

/// A model's summary from Postgres's `summaries` (pg::summ): the content
/// key it was written against, summary, why, asks, writer, when.
pub type Overlay = HashMap<String, OverlayRow>;

/// One entity's model summary: key, summary, why, asks, writer, when.
pub type OverlayRow = (String, String, String, Vec<String>, String, String);

/// The latest live model summary of each entity (`summaries`, written by
/// `agent index summarize`). The ledger knows nothing of them; the sync
/// shows one over a seed while its content key is the entity's, so a file
/// that changed falls back to its seed until it is summarized again.
pub fn read_overlay(pg: &Pg) -> Result<Overlay> {
    read_overlay_in(pg, None, &[])
}

/// [`read_overlay`], of these projects' entities only (None: all).
/// With a scope, `keys` are its entities' content keys: every entity with a
/// live summary of one of them comes too (whatever its project), so a
/// summary shown by its content (see read_ledger) is the one a full sync
/// shows.
fn read_overlay_in(pg: &Pg, scope: Option<&[String]>, keys: &[String]) -> Result<Overlay> {
    let scope: Option<Vec<String>> = scope.map(<[String]>::to_vec);
    let rows = pg.query(
        "SELECT DISTINCT ON (entity_id) entity_id, content_key, summary, why, asks, writer,
                to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
         FROM summaries WHERE status = 'live'
             AND ($1::text[] IS NULL OR split_part(entity_id, ':', 1) = ANY($1)
                  OR entity_id IN (SELECT entity_id FROM summaries
                                   WHERE status = 'live' AND content_key = ANY($2)))
         ORDER BY entity_id, id DESC",
        &[&scope, &keys],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get(0),
                (r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6)),
            )
        })
        .collect())
}

/// A ledger filter on `project` for these projects (None: every row), and
/// its one parameter (a JSON array).
fn scope_sql(scope: Option<&[String]>) -> (&'static str, Vec<String>) {
    match scope {
        Some(ps) => (
            "project IN (SELECT value FROM json_each(?1))",
            vec![serde_json::to_string(ps).unwrap_or_default()],
        ),
        None => ("1", Vec::new()),
    }
}

/// Reads the ledger: every entity as the sync writes it (or only these
/// projects'), with its labels, and a model's summary from `overlay` where
/// it is fresh.
fn read_ledger(
    sq: &Connection,
    cfg: &Config,
    overlay: &Overlay,
    scope: Option<&[String]>,
) -> Result<(Vec<Out>, Labels)> {
    // A cut other than the default one is part of each file's chunk key, so
    // changing it cuts every file again.
    let cut = if cfg.default_chunking() {
        String::new()
    } else {
        format!("@{}x{}", cfg.chunks.max_lines, cfg.chunks.max_bytes)
    };
    let (inscope, arg) = scope_sql(scope);
    let arg = rusqlite::params_from_iter(arg.iter());
    // Another table's rows of the entities in scope.
    let of_entities = |col: &str| {
        if scope.is_some() {
            format!("{col} IN (SELECT id FROM entities WHERE {inscope})")
        } else {
            "1".to_string()
        }
    };
    let ents: Vec<Entity> = {
        let mut st = sq.prepare(&format!(
            "SELECT {ENTITY_COLS} FROM entities WHERE {inscope} ORDER BY id"
        ))?;
        st.query_map(arg.clone(), Entity::from_row)?
            .collect::<rusqlite::Result<_>>()?
    };
    // The summary each entity shows (a model's or person's over a seed, the
    // latest first), and its latest seed.
    let mut shown: HashMap<String, (String, String, String, String, String)> = HashMap::new();
    let mut seeds: HashMap<String, String> = HashMap::new();
    {
        let mut st = sq.prepare(&format!(
            "SELECT entity, text, why, writer, content_key, written_at FROM summaries
             WHERE status = 'live' AND text != '' AND {}
             ORDER BY entity, (writer = 'seed'), seq DESC",
            of_entities("entity")
        ))?;
        let mut rows = st.query(arg.clone())?;
        while let Some(r) = rows.next()? {
            let id: String = r.get(0)?;
            let text: String = r.get(1)?;
            let writer: String = r.get(3)?;
            if writer == "seed" && !seeds.contains_key(&id) {
                seeds.insert(id.clone(), text.clone());
            }
            shown.entry(id).or_insert_with(|| {
                (
                    text,
                    r.get(2).unwrap_or_default(),
                    writer,
                    r.get(4).unwrap_or_default(),
                    r.get(5).unwrap_or_default(),
                )
            });
        }
    }
    let mut labels: HashMap<String, Vec<Label>> = HashMap::new();
    {
        let mut st = sq.prepare(&format!(
            "SELECT entity, label, norm, compact, kind, origin, weight, evidence FROM labels
             WHERE {} ORDER BY entity, label, kind",
            of_entities("entity")
        ))?;
        let mut rows = st.query(arg.clone())?;
        while let Some(r) = rows.next()? {
            labels.entry(r.get(0)?).or_default().push((
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ));
        }
    }
    let mut kids: HashMap<String, Vec<String>> = HashMap::new();
    {
        // A child is in its parent's project (a project's own parent, the
        // root, is never in a scope).
        let mut st = sq.prepare(&format!(
            "SELECT parent, name FROM entities WHERE kind IN ('symbol', 'file', 'dir') AND {inscope}
             ORDER BY parent, importance DESC, name"
        ))?;
        let mut rows = st.query(arg.clone())?;
        while let Some(r) = rows.next()? {
            let v = kids.entry(r.get(0)?).or_default();
            if v.len() < cfg.cards.children {
                v.push(r.get(1)?);
            }
        }
    }
    let full: HashSet<String> = crate::roots::list_projects(sq)?
        .into_iter()
        .filter(|p| p.crawled_depth == "full")
        .map(|p| p.name)
        .collect();
    let ids: HashSet<&str> = ents.iter().map(|e| e.id.as_str()).collect();
    labels.retain(|k, _| ids.contains(k.as_str()));
    // Summaries by the content they describe: of several, the first entity's
    // by id, so the choice (and the row's sync key) is the same every sync.
    let mut by_key: HashMap<&str, &OverlayRow> = HashMap::new();
    {
        let mut owners: Vec<&String> = overlay.keys().collect();
        owners.sort();
        for id in owners {
            let row = &overlay[id];
            if !row.0.is_empty() {
                by_key.entry(row.0.as_str()).or_insert(row);
            }
        }
    }
    let mut out = Vec::with_capacity(ents.len());
    for e in ents {
        let (mut stext, mut why, mut writer, mut skey, mut sat) =
            shown.remove(&e.id).unwrap_or_default();
        let mut asks = Vec::new();
        // A model's summary where it is fresh, unless [summaries].withhold
        // names the entity (its seed shows).
        let withheld = cfg
            .summaries
            .withhold
            .iter()
            .any(|p| !p.is_empty() && e.id.starts_with(p.as_str()));
        // Its own, else one written for the same content under another id: a
        // file renamed or copied since the summaries were written keeps the
        // summary of its content, as the summarizer would copy it.
        let mine = overlay
            .get(&e.id)
            .filter(|o| o.0 == e.content_key)
            .or_else(|| by_key.get(e.content_key.as_str()).copied());
        if (writer.is_empty() || writer == "seed")
            && !withheld
            && let Some((key, s, w, a, wr, at)) = mine
            && !e.content_key.is_empty()
            && *key == e.content_key
        {
            (stext, why, writer, skey, sat) =
                (s.clone(), w.clone(), wr.clone(), key.clone(), at.clone());
            // [cards].asks off: no asks in the row (its tsv), card or vector.
            if cfg.cards.asks {
                asks = a.clone();
            }
        }
        let seed = seeds.remove(&e.id).unwrap_or_default();
        let summary = if writer == "seed" {
            String::new()
        } else {
            stext.clone()
        };
        let ls = labels.get(&e.id).map(Vec::as_slice).unwrap_or(&[]);
        let mut label_names: Vec<&str> = ls.iter().map(|l| l.0.as_str()).collect();
        label_names.sort();
        label_names.dedup();
        let labels_text = label_names.join(" | ");
        let symbols = kids
            .get(&e.id)
            .map(|v| {
                v.iter()
                    .map(|k| names::whole_and_split(k))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let rel_or_project = if e.rel.is_empty() {
            e.project.as_str()
        } else {
            e.rel.as_str()
        };
        let terms = format!("{} {}", name_terms_of(&e), path_terms(rel_or_project));
        let card_text = card(
            &e.project, &e.rel, &e.title, &stext, &why, &symbols, &asks, &cfg.cards,
        );
        let text_key = hash(&[card_text.as_bytes()]);
        let embed =
            e.kind != "root" && crate::crawl::wants_embedding(&e.kind, &e.rel, &e.file_kind);
        let chunk_key = if e.kind == "file"
            && full.contains(&e.project)
            && e.no_content.is_empty()
            && cfg.chunks.file_kinds.iter().any(|k| k == &e.file_kind)
        {
            format!("{}{cut}|{CHUNK_META_VERSION}", e.content_key)
        } else {
            String::new()
        };
        let corpus = corpus_of(&e.kind, &e.file_kind).to_string();
        let mut o = Out {
            e,
            corpus,
            body: None,
            side_chunks: Vec::new(),
            seed,
            summary,
            why,
            summary_key: skey,
            summary_writer: writer,
            summary_at: sat,
            asks,
            terms,
            labels_text,
            symbols,
            embed,
            text_key,
            chunk_key,
            sync_key: String::new(),
        };
        let mut row = String::new();
        o.copy(&mut row);
        let mut lrows = String::new();
        for l in ls {
            lrows.push_str(&format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\n",
                l.0, l.3, l.4, l.5, l.6
            ));
        }
        o.sync_key = hash(&[row.as_bytes(), lrows.as_bytes()]);
        out.push(o);
    }
    Ok((out, labels))
}

/// The corpora that come from outside the crawl: their entities are written
/// by the sync from what the caller read (memory notes from their
/// directories, friction entries from the agent store, sessions and what was
/// pasted into them from their transcripts).
pub const SIDE_CORPORA: &[&str] = &["memory", "friction", "session", "pasted"];

/// A chunk a side note brings cut already (a session's turns), with its meta
/// (the corpus is added).
#[derive(Debug, Clone, Default)]
pub struct SideChunk {
    pub line_start: usize,
    pub line_end: usize,
    pub heading: String,
    pub text: String,
    pub meta: serde_json::Value,
}

/// One note of a side corpus, as the caller read it.
#[derive(Debug, Clone, Default)]
pub struct SideNote {
    /// `memory:<project>/<file>`, `friction:<uid>`, `session:<harness>:<id>`.
    pub id: String,
    /// memory, friction, session.
    pub corpus: String,
    /// note, friction, session.
    pub kind: String,
    /// Unchanged since the last sync (its key matches): the row, chunks and
    /// edges it has stay, and nothing else here is read.
    pub keep: bool,
    /// Its content key ("": a hash of its text, title, seed and status).
    pub key: String,
    /// Its chunks, when it brings them cut ([`SideChunk`]); else `body` is
    /// cut as Markdown.
    pub chunks: Vec<SideChunk>,
    /// Absolute paths of files it worked on, with how (`read`, `edit`), for
    /// `touched` edges from it (sessions).
    pub touched: Vec<(String, String)>,
    /// The indexed project it is about, when known ("" otherwise).
    pub project: String,
    pub name: String,
    /// Where to open it ("" for a store row).
    pub path: String,
    pub title: String,
    /// One paragraph: what it is about.
    pub seed: String,
    /// Its whole text, cut into chunks.
    pub body: String,
    /// Paths it names (backticked), to tie it to the files they are.
    pub refs: Vec<String>,
    /// Its status or kind, a short line (friction: `open, bug`).
    pub status: String,
}

/// Side notes as the sync writes them, and the edges from the files they
/// name: (file, note) rows, `noted-in` for memory and `reported-in` for
/// friction, resolved against the crawled files by path (a path relative
/// to the note's project first, else one file's path ending with it).
fn side_rows(
    side: &[SideNote],
    files: &[(String, String, String)],
    projects: &HashMap<String, String>,
    cfg: &Config,
) -> (Vec<Out>, Vec<Vec<String>>) {
    // Files by their path in their project, and by base name.
    let mut by_rel: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    let mut by_base: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    let mut by_project_rel: HashMap<(&str, &str), &str> = HashMap::new();
    for (id, project, rel) in files {
        by_rel.entry(rel.as_str()).or_default().push((id, project));
        let base = rel.rsplit('/').next().unwrap_or(rel);
        by_base.entry(base).or_default().push((id, rel));
        by_project_rel.insert((project.as_str(), rel.as_str()), id.as_str());
    }
    // An absolute path's file: the deepest project holding it, then its path there.
    let mut roots: Vec<(&str, &str)> = projects
        .iter()
        .map(|(n, p)| (n.as_str(), p.as_str()))
        .collect();
    roots.sort_by_key(|(_, p)| std::cmp::Reverse(p.len()));
    let file_at = |abs: &str| -> Option<&str> {
        let (name, root) = roots
            .iter()
            .find(|(_, p)| abs.starts_with(&format!("{p}/")))?;
        by_project_rel
            .get(&(*name, &abs[root.len() + 1..]))
            .copied()
    };
    let mut outs = Vec::new();
    let mut edges = Vec::new();
    for n in side.iter().filter(|n| !n.keep) {
        let e = Entity {
            id: n.id.clone(),
            kind: n.kind.clone(),
            project: n.project.clone(),
            rel: n.name.clone(),
            name: n.name.clone(),
            path: n.path.clone(),
            file_kind: n.corpus.clone(),
            content_key: if n.key.is_empty() {
                hash(&[
                    n.body.as_bytes(),
                    n.title.as_bytes(),
                    n.seed.as_bytes(),
                    n.status.as_bytes(),
                ])
            } else {
                n.key.clone()
            },
            title: n.title.clone(),
            signature: n.status.clone(),
            ..Default::default()
        };
        // As the embedder makes it again from the row (project, rel, title,
        // the seed it shows), so the keys agree.
        let card_text = card(
            &e.project,
            &e.rel,
            &e.title,
            &n.seed,
            "",
            "",
            &[],
            &cfg.cards,
        );
        let chunk_key = if n.body.trim().is_empty() && n.chunks.is_empty() {
            String::new()
        } else {
            format!("{}|{CHUNK_META_VERSION}", e.content_key)
        };
        let mut o = Out {
            terms: format!(
                "{} {}",
                names::whole_and_split(&n.name),
                path_terms(&n.name)
            ),
            text_key: hash(&[card_text.as_bytes()]),
            e,
            corpus: n.corpus.clone(),
            body: Some(n.body.clone()),
            side_chunks: n.chunks.clone(),
            asks: Vec::new(),
            seed: n.seed.clone(),
            summary: String::new(),
            why: String::new(),
            summary_key: String::new(),
            summary_writer: if n.seed.is_empty() {
                String::new()
            } else {
                "seed".into()
            },
            summary_at: String::new(),
            labels_text: String::new(),
            symbols: String::new(),
            embed: true,
            chunk_key,
            sync_key: String::new(),
        };
        o.summary_key = o.e.content_key.clone();
        let mut row = String::new();
        o.copy(&mut row);
        o.sync_key = hash(&[row.as_bytes()]);
        outs.push(o);
        // A session's files: session → file, `touched`, weighed by how many
        // calls, the actions as evidence.
        let mut touched: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
        for (abs, how) in &n.touched {
            if let Some(f) = file_at(abs) {
                let t = touched.entry(f).or_default();
                t.0 += 1;
                t.1.insert(how.as_str());
            }
        }
        for (f, (count, how)) in touched {
            edges.push(vec![
                n.id.clone(),
                f.to_string(),
                "touched".into(),
                count.to_string(),
                n.corpus.clone(),
                how.into_iter().collect::<Vec<_>>().join(", "),
                String::new(),
                String::new(),
            ]);
        }
        let kind = if n.corpus == "memory" {
            "noted-in"
        } else {
            "reported-in"
        };
        let mut seen = HashSet::new();
        for r in &n.refs {
            let r = r.trim_start_matches("./").trim_end_matches('/');
            if r.is_empty() {
                continue;
            }
            // In the note's project, else the one file anywhere at that path,
            // else the one file whose path ends with it.
            let exact = by_rel.get(r).map(Vec::as_slice).unwrap_or(&[]);
            let own: Vec<&str> = exact
                .iter()
                .filter(|(_, p)| *p == n.project)
                .map(|(i, _)| *i)
                .collect();
            let target = if own.len() == 1 {
                Some(own[0])
            } else if exact.len() == 1 {
                Some(exact[0].0)
            } else if exact.is_empty() {
                let base = r.rsplit('/').next().unwrap_or(r);
                let ends: Vec<&str> = by_base
                    .get(base)
                    .into_iter()
                    .flatten()
                    .filter(|(_, rel)| {
                        rel.ends_with(r)
                            && (rel.len() == r.len() || rel[..rel.len() - r.len()].ends_with('/'))
                    })
                    .map(|(i, _)| *i)
                    .collect();
                (ends.len() == 1).then(|| ends[0])
            } else {
                None
            };
            if let Some(t) = target
                && seen.insert(t)
            {
                edges.push(vec![
                    t.to_string(),
                    n.id.clone(),
                    kind.to_string(),
                    "1".into(),
                    n.corpus.clone(),
                    r.to_string(),
                    String::new(),
                    String::new(),
                ]);
            }
        }
    }
    (outs, edges)
}

/// A digest of a query's rows, every column as text.
fn digest(sq: &Connection, sql: &str) -> Result<(String, Vec<Vec<String>>)> {
    let mut st = sq.prepare(sql)?;
    let n = st.column_count();
    let mut rows = st.query([])?;
    let mut all = Vec::new();
    let mut buf = String::new();
    while let Some(r) = rows.next()? {
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            let s = match r.get_ref(i)? {
                rusqlite::types::ValueRef::Null => String::new(),
                rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                rusqlite::types::ValueRef::Real(x) => x.to_string(),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                rusqlite::types::ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
            };
            v.push(s);
        }
        copy_row(&mut buf, &v.iter().map(String::as_str).collect::<Vec<_>>());
        all.push(v);
    }
    Ok((hash(&[buf.as_bytes()]), all))
}

/// The small tables, replaced whole when their digest moved: (name, the
/// ledger's query, Postgres's columns).
const SMALL: &[(&str, &str, &str)] = &[
    (
        "roots",
        "SELECT id, path, depth, added_at FROM roots ORDER BY id",
        "id, path, depth, added_at",
    ),
    (
        "projects",
        "SELECT name, path, root, CASE explicit WHEN 0 THEN 'f' ELSE 't' END, depth, CASE is_git WHEN 0 THEN 'f' ELSE 't' END, root_commit, remote, last_commit, last_crawled_at, crawled_depth FROM projects ORDER BY name",
        "name, path, root, explicit, depth, is_git, identity, remote, crawled_commit, crawled_at, crawled_depth",
    ),
    (
        "aliases",
        "SELECT old_id, new_id FROM aliases ORDER BY old_id",
        "old_id, new_id",
    ),
    (
        "dangling",
        "SELECT doc, ref, line FROM dangling ORDER BY doc, ref",
        "doc, ref, line",
    ),
];

const EDGES_SQL: &str = "SELECT source, target, kind, weight, origin, evidence, source_key, target_key FROM edges ORDER BY source, target, kind";
const EDGE_COLS: &str = "src, dst, kind, weight, origin, evidence, src_key, dst_key";
const CHUNK_COLS: &str = "entity_id, ord, line_start, line_end, heading, text, terms, meta";

fn copy_in(tx: &mut postgres::Transaction, table: &str, cols: &str, rows: &str) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut w = tx.copy_in(&format!("COPY {table} ({cols}) FROM STDIN"))?;
    w.write_all(rows.as_bytes())?;
    w.finish()
        .with_context(|| format!("copying into {table}"))?;
    Ok(())
}

/// Mirrors the ledger into Postgres (see the module's doc), leaving the side
/// corpora (memory, friction) as they are.
pub fn sync(sq: &Connection, pg: &Pg, rebuild: bool) -> Result<SyncReport> {
    sync_with(sq, pg, rebuild, None)
}

/// [`sync`], and with `side` the memory and friction corpora too: their
/// notes become exactly these (a note not given is removed). A rebuild
/// without `side` leaves them out until the next sync with it.
pub fn sync_with(
    sq: &Connection,
    pg: &Pg,
    rebuild: bool,
    side: Option<&[SideNote]>,
) -> Result<SyncReport> {
    sync_inner(sq, pg, rebuild, side, None)
}

/// The watcher's flush: mirrors only these projects' entities (their rows,
/// labels, chunks and file mtimes; edges and the small tables by digest as
/// always), leaving every other row as it is. The words' document counts
/// wait for the next full sync, which recounts them.
pub fn sync_projects(sq: &Connection, pg: &Pg, projects: &[String]) -> Result<SyncReport> {
    sync_inner(sq, pg, false, None, Some(projects))
}

/// The meta key that names the ledger a schema is synced from.
const LEDGER_KEY: &str = "ledger";

/// The ledger's file, canonical ("" for one in memory).
fn ledger_of(sq: &Connection) -> String {
    sq.path()
        .filter(|p| !p.is_empty())
        .map(|p| {
            std::fs::canonicalize(p)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| p.to_string())
        })
        .unwrap_or_default()
}

/// The ledger a schema was last synced from ("" when none is recorded).
pub fn synced_ledger(pg: &Pg) -> Result<String> {
    Ok(pg
        .query_opt("SELECT value FROM meta WHERE key = $1", &[&LEDGER_KEY])?
        .map(|r| r.get(0))
        .unwrap_or_default())
}

/// Makes `sq`'s file the ledger this schema is synced from: for a ledger
/// moved on purpose (a copy at a fixed path), since a sync from any other
/// ledger is refused.
pub fn adopt_ledger(sq: &Connection, pg: &Pg) -> Result<String> {
    let mine = ledger_of(sq);
    pg.execute(
        "INSERT INTO meta (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        &[&LEDGER_KEY, &mine],
    )?;
    Ok(mine)
}

fn sync_inner(
    sq: &Connection,
    pg: &Pg,
    rebuild: bool,
    side: Option<&[SideNote]>,
    scope: Option<&[String]>,
) -> Result<SyncReport> {
    let start = Instant::now();
    let mut rep = SyncReport {
        rebuild,
        ..Default::default()
    };
    let mut lap = Instant::now();
    let mut phase = |rep: &mut SyncReport, name: &str| {
        rep.phases
            .insert(name.to_string(), lap.elapsed().as_millis());
        lap = Instant::now();
    };
    let cfg = &pg.cfg;
    // A scope's content keys (what a summary can be shown by).
    let keys: Vec<String> = match scope {
        Some(_) => {
            let (inscope, arg) = scope_sql(scope);
            let mut st = sq.prepare(&format!(
                "SELECT DISTINCT content_key FROM entities
                 WHERE kind IN ('project', 'dir', 'file') AND content_key <> '' AND {inscope}"
            ))?;
            st.query_map(rusqlite::params_from_iter(arg.iter()), |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        }
        None => Vec::new(),
    };
    let overlay = read_overlay_in(pg, scope, &keys)?;
    let (mut rows, labels) = read_ledger(sq, cfg, &overlay, scope)?;
    let projects: HashMap<String, String> = crate::roots::list_projects(sq)?
        .into_iter()
        .map(|p| (p.name, p.path))
        .collect();
    let mut side_edges: Vec<Vec<String>> = Vec::new();
    // Side notes unchanged since the last sync: their rows, chunks and edges stay.
    let kept: Vec<String> = side
        .into_iter()
        .flatten()
        .filter(|n| n.keep)
        .map(|n| n.id.clone())
        .collect();
    if let Some(side) = side {
        let files: Vec<(String, String, String)> = rows
            .iter()
            .filter(|o| o.e.kind == "file")
            .map(|o| (o.e.id.clone(), o.e.project.clone(), o.e.rel.clone()))
            .collect();
        let (outs, edges) = side_rows(side, &files, &projects, cfg);
        rep.side = outs.len() + kept.len();
        rows.extend(outs);
        side_edges = edges;
    }
    rep.entities = rows.len() + kept.len();
    // Files' mtimes in the ledger (the stale check compares with them).
    let mtimes: HashMap<String, i64> = {
        let (inscope, arg) = scope_sql(scope);
        let mut st = sq.prepare(&format!(
            "SELECT id, mtime FROM entities WHERE kind = 'file' AND {inscope}"
        ))?;
        st.query_map(rusqlite::params_from_iter(arg.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    phase(&mut rep, "read ledger");

    let mut c = pg.client();
    let mut tx = c.transaction()?;
    // One sync of a schema at a time: a second waits for the first's commit,
    // then finds what it left.
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext('agent-index sync ' || $1))",
        &[&pg.schema],
    )?;
    // One ledger per schema: the summaries' content keys, the renames and
    // the aliases hang on it, and another ledger's crawl would re-key and
    // remove rows. A schema remembers the ledger its first sync came from
    // and refuses any other (adopt_ledger moves it on purpose).
    let mine = ledger_of(sq);
    let theirs: String = tx
        .query_opt("SELECT value FROM meta WHERE key = $1", &[&LEDGER_KEY])?
        .map(|r| r.get(0))
        .unwrap_or_default();
    if !mine.is_empty() && !theirs.is_empty() && mine != theirs {
        bail!(
            "schema {} is synced from the ledger {theirs}, not {mine}: set [ledger].path to it in the index configuration (or, to move the schema to this ledger on purpose, agent index sync --adopt-ledger)",
            pg.schema
        );
    }
    let run_id: i64 = tx
        .query_one(
            "INSERT INTO runs (kind, config, fingerprint, started) VALUES ('sync', $1, $2, now()) RETURNING id",
            &[
                &serde_json::json!({"rebuild": rebuild, "config": cfg, "scope": scope, "ledger": mine}),
                &cfg.fingerprint(),
            ],
        )?
        .get(0);
    if rebuild {
        tx.batch_execute(
            "CREATE TEMP TABLE keep_vec ON COMMIT DROP AS
                 SELECT id, vec_key, card_vec FROM entities WHERE card_vec IS NOT NULL;
             TRUNCATE entities, chunks, labels, edges, aliases, dangling, projects, roots, embed_queue;
             DELETE FROM meta;",
        )?;
    }
    if !mine.is_empty() && mine != theirs {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[&LEDGER_KEY, &mine],
        )?;
    }
    // Without side notes, their rows aren't this sync's to change; with a
    // scope, only its projects' rows are.
    let side_corpora: Vec<String> = SIDE_CORPORA.iter().map(|s| s.to_string()).collect();
    let have_rows = match (side.is_some(), scope) {
        (_, Some(ps)) => tx.query(
            "SELECT id, sync_key FROM entities WHERE project = ANY($1) AND corpus <> ALL($2::text[])",
            &[&ps.to_vec(), &side_corpora],
        )?,
        (true, None) => tx.query("SELECT id, sync_key FROM entities", &[])?,
        (false, None) => tx.query(
            "SELECT id, sync_key FROM entities WHERE corpus <> ALL($1::text[])",
            &[&side_corpora],
        )?,
    };
    let have: HashMap<String, String> = have_rows
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    phase(&mut rep, "read postgres");

    // Entities gone from the ledger.
    let ledger_ids: HashSet<&str> = rows.iter().map(|o| o.e.id.as_str()).collect();
    let kept_ids: HashSet<&str> = kept.iter().map(String::as_str).collect();
    let gone: Vec<String> = have
        .keys()
        .filter(|id| !ledger_ids.contains(id.as_str()) && !kept_ids.contains(id.as_str()))
        .cloned()
        .collect();
    if !gone.is_empty() {
        tx.execute("DELETE FROM entities WHERE id = ANY($1)", &[&gone])?;
        tx.execute("DELETE FROM labels WHERE entity_id = ANY($1)", &[&gone])?;
        tx.execute(
            "DELETE FROM embed_queue WHERE entity_id = ANY($1)",
            &[&gone],
        )?;
        // Other models' vectors (pg::vecs), which hold no key to entities.
        let tables: Vec<String> = tx
            .query(
                "SELECT tablename FROM pg_tables WHERE schemaname = $1 AND tablename LIKE 'vec\\_%'",
                &[&pg.schema],
            )?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        for t in tables {
            tx.execute(
                &format!("DELETE FROM {t} WHERE entity_id = ANY($1)"),
                &[&gone],
            )?;
        }
    }
    rep.deleted = gone.len();

    // Entities new or changed.
    let changed: Vec<&Out> = rows
        .iter()
        .filter(|o| have.get(&o.e.id) != Some(&o.sync_key))
        .collect();
    rep.written = changed.len();
    let mut rechunk: Vec<(String, String)> = Vec::new();
    if !changed.is_empty() {
        tx.batch_execute(&format!(
            "CREATE TEMP TABLE stage_entities ON COMMIT DROP AS SELECT {STAGE_COLS} FROM entities WITH NO DATA"
        ))?;
        for part in changed.chunks(COPY_BATCH) {
            let mut buf = String::new();
            for o in part {
                let m = mtimes.get(&o.e.id).copied().unwrap_or(0).to_string();
                o.copy_with(&mut buf, Some(&m));
            }
            copy_in(&mut tx, "stage_entities", STAGE_COLS, &buf)?;
        }
        let sets = STAGE_COLS
            .split(", ")
            .filter(|c| *c != "id")
            .map(|c| format!("{c} = excluded.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        // One upsert; RETURNING old and new (Postgres 18) says what to queue
        // for embedding and which files to cut into chunks again.
        let sql = format!(
            "WITH up AS (
                 INSERT INTO entities ({STAGE_COLS}) SELECT {STAGE_COLS} FROM stage_entities
                 ON CONFLICT (id) DO UPDATE SET {sets}, updated_at = now()
                 RETURNING new.id AS id, new.embed AS embed, (old.id IS NULL) AS fresh,
                           old.text_key AS old_tk, new.text_key AS new_tk,
                           coalesce(old.chunk_key, '') AS old_ck, new.chunk_key AS new_ck, new.rel AS rel,
                           new.project AS project
             ), dq AS (
                 DELETE FROM embed_queue q USING up WHERE q.entity_id = up.id AND NOT up.embed
                 RETURNING q.entity_id
             ), q AS (
                 INSERT INTO embed_queue (entity_id, text_key, reason)
                 SELECT id, new_tk, CASE WHEN fresh THEN 'new' ELSE 'changed' END FROM up
                 WHERE embed AND (fresh OR old_tk IS DISTINCT FROM new_tk)
                 ON CONFLICT (entity_id) DO UPDATE SET text_key = excluded.text_key,
                     reason = excluded.reason, queued_at = now()
                 RETURNING entity_id
             )
             SELECT id, new_ck, (SELECT count(*) FROM q) FROM up WHERE old_ck <> new_ck"
        );
        let out = tx.query(&sql, &[])?;
        for r in &out {
            rechunk.push((r.get(0), r.get(1)));
        }
        // The queued count, when no row needed chunks (the count rides on each row).
        rep.queued = match out.first() {
            Some(r) => r.get::<_, i64>(2) as usize,
            None => tx
                .query_one(
                    "SELECT count(*) FROM embed_queue q JOIN stage_entities s ON s.id = q.entity_id",
                    &[],
                )?
                .get::<_, i64>(0) as usize,
        };
        // Labels of the changed entities, written again.
        let ids: Vec<String> = changed.iter().map(|o| o.e.id.clone()).collect();
        tx.execute("DELETE FROM labels WHERE entity_id = ANY($1)", &[&ids])?;
        let mut buf = String::new();
        for o in &changed {
            for l in labels.get(&o.e.id).into_iter().flatten() {
                let w = l.5.to_string();
                copy_row(&mut buf, &[&o.e.id, &l.0, &l.1, &l.2, &l.3, &l.4, &w, &l.6]);
                rep.labels += 1;
            }
            if buf.len() > 8 << 20 {
                copy_in(
                    &mut tx,
                    "labels",
                    "entity_id, label, norm, compact, kind, origin, weight, evidence",
                    &buf,
                )?;
                buf.clear();
            }
        }
        copy_in(
            &mut tx,
            "labels",
            "entity_id, label, norm, compact, kind, origin, weight, evidence",
            &buf,
        )?;
    }
    // Files' mtimes, for the search's stale check (fresh::check): a written
    // row took its own; the rest (a file touched, not changed: its sync key
    // holds) are set apart, only where they moved.
    {
        let mut buf = String::new();
        for (id, m) in &mtimes {
            copy_row(&mut buf, &[id, &m.to_string()]);
        }
        tx.batch_execute(
            "CREATE TEMP TABLE stage_mtime (id text PRIMARY KEY, mtime bigint) ON COMMIT DROP",
        )?;
        copy_in(&mut tx, "stage_mtime", "id, mtime", &buf)?;
        rep.mtimes = tx.execute(
            "UPDATE entities e SET mtime = s.mtime FROM stage_mtime s
             WHERE e.id = s.id AND e.mtime IS DISTINCT FROM s.mtime",
            &[],
        )? as usize;
    }
    phase(&mut rep, "entities");

    // Chunks of the files whose content changed, cut from the file on disk.
    if !rechunk.is_empty() {
        let ids: Vec<String> = rechunk.iter().map(|(i, _)| i.clone()).collect();
        tx.execute("DELETE FROM chunks WHERE entity_id = ANY($1)", &[&ids])?;
        rep.rechunked = ids.clone();
        let by_id: HashMap<&str, &Out> = rows.iter().map(|o| (o.e.id.as_str(), o)).collect();
        let mut buf = String::new();
        for (id, ck) in &rechunk {
            if ck.is_empty() {
                continue;
            }
            let Some(o) = by_id.get(id.as_str()) else {
                continue;
            };
            // A side note's text is in hand; a file's is read from disk.
            let (text, markdown) = match &o.body {
                Some(b) => (std::borrow::Cow::Borrowed(b.as_str()), true),
                None => {
                    let Some(dir) = projects.get(&o.e.project) else {
                        continue;
                    };
                    let Some(bytes) = crate::crawl::read_inside(Path::new(dir), &o.e.rel) else {
                        continue;
                    };
                    if crate::files::is_binary(&bytes) {
                        continue;
                    }
                    (
                        std::borrow::Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
                        o.e.lang == "markdown",
                    )
                }
            };
            rep.chunked_files += 1;
            // Chunks a side note brought cut (a session's turns), with their meta.
            if !o.side_chunks.is_empty() {
                for (i, ch) in o.side_chunks.iter().enumerate() {
                    let (ord, a, b) = (
                        i.to_string(),
                        ch.line_start.to_string(),
                        ch.line_end.to_string(),
                    );
                    let mut meta = ch.meta.clone();
                    if let Some(m) = meta.as_object_mut() {
                        m.insert("corpus".into(), serde_json::json!(o.corpus));
                    } else {
                        meta = serde_json::json!({"corpus": o.corpus});
                    }
                    let terms = camel_terms(&ch.text);
                    let meta = meta.to_string();
                    copy_row(
                        &mut buf,
                        &[id, &ord, &a, &b, &ch.heading, &ch.text, &terms, &meta],
                    );
                    rep.chunks += 1;
                }
                continue;
            }
            for (i, ch) in chunk_text(&text, markdown, &cfg.chunks).iter().enumerate() {
                let (ord, a, b) = (
                    i.to_string(),
                    ch.line_start.to_string(),
                    ch.line_end.to_string(),
                );
                let terms = camel_terms(&ch.text);
                // The corpus (search filters chunks by it without a join) and
                // the heading path (a long note searchable by its part).
                let meta = serde_json::json!({"corpus": o.corpus, "headings": ch.path}).to_string();
                copy_row(
                    &mut buf,
                    &[id, &ord, &a, &b, &ch.heading, &ch.text, &terms, &meta],
                );
                rep.chunks += 1;
            }
            if buf.len() > 16 << 20 {
                copy_in(&mut tx, "chunks", CHUNK_COLS, &buf)?;
                buf.clear();
            }
        }
        copy_in(&mut tx, "chunks", CHUNK_COLS, &buf)?;
    }
    // Document frequencies of the crawled chunks (a memory note's words
    // don't make a code word common), when the chunks changed, or were never
    // counted this way at this floor.
    let floor = cfg.chunks.lexeme_floor;
    let how = format!("{floor} crawled");
    let counted: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM lexemes)
                AND coalesce((SELECT value FROM meta WHERE key = 'lexeme_floor'), '') = $1",
            &[&how],
        )?
        .get(0);
    if scope.is_some() {
        // The watcher's flush leaves the count (a scan of every chunk) to
        // the next full sync: a few files' words barely move it.
        if !rechunk.is_empty() {
            tx.execute("DELETE FROM meta WHERE key = 'lexeme_floor'", &[])?;
        }
    } else if !rechunk.is_empty() || rebuild || !counted {
        let crawled = "WHERE coalesce(meta->>''corpus'', ''code'') IN (''code'', ''prose'')";
        tx.batch_execute(&format!(
            "DELETE FROM lexemes;
             INSERT INTO lexemes (config, word, ndoc)
             SELECT 'simple', word, ndoc FROM ts_stat('SELECT tsv_simple FROM chunks {crawled}') WHERE ndoc >= {floor}
             UNION ALL
             SELECT 'english', word, ndoc FROM ts_stat('SELECT tsv_en FROM chunks {crawled}') WHERE ndoc >= {floor};
             INSERT INTO meta (key, value) VALUES ('lexeme_floor', '{how}')
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value;"
        ))?;
    }
    phase(&mut rep, "chunks");

    // Edges, by a digest of the whole table.
    let meta_get = |tx: &mut postgres::Transaction, k: &str| -> Result<String> {
        Ok(tx
            .query_opt("SELECT value FROM meta WHERE key = $1", &[&k])?
            .map(|r| r.get(0))
            .unwrap_or_default())
    };
    let (d, erows) = digest(sq, EDGES_SQL)?;
    rep.edges = erows.len();
    if meta_get(&mut tx, "edges")? != d {
        rep.edges_replaced = true;
        tx.batch_execute(&format!(
            "CREATE TEMP TABLE stage_edges ON COMMIT DROP AS SELECT {EDGE_COLS} FROM edges WITH NO DATA"
        ))?;
        let mut buf = String::new();
        for r in &erows {
            copy_row(&mut buf, &r.iter().map(String::as_str).collect::<Vec<_>>());
        }
        copy_in(&mut tx, "stage_edges", EDGE_COLS, &buf)?;
        // The side corpora's edges (origin memory, friction) are theirs.
        tx.batch_execute(&format!(
            "DELETE FROM edges e WHERE e.origin NOT IN ('memory', 'friction', 'session') AND NOT EXISTS (
                 SELECT 1 FROM stage_edges s WHERE s.src = e.src AND s.dst = e.dst AND s.kind = e.kind);
             INSERT INTO edges ({EDGE_COLS}) SELECT {EDGE_COLS} FROM stage_edges
             ON CONFLICT (src, dst, kind) DO UPDATE SET weight = excluded.weight, origin = excluded.origin,
                 evidence = excluded.evidence, src_key = excluded.src_key, dst_key = excluded.dst_key
             WHERE (edges.weight, edges.origin, edges.evidence, edges.src_key, edges.dst_key)
                 IS DISTINCT FROM (excluded.weight, excluded.origin, excluded.evidence, excluded.src_key, excluded.dst_key);"
        ))?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('edges', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[&d],
        )?;
    }
    // The side corpora's edges, made again from their notes.
    if side.is_some() {
        // A kept note's edges stay (they are what its last read made).
        tx.execute(
            "DELETE FROM edges WHERE origin IN ('memory', 'friction', 'session')
             AND NOT (src = ANY($1) OR dst = ANY($1))",
            &[&kept],
        )?;
        let mut buf = String::new();
        for r in &side_edges {
            copy_row(&mut buf, &r.iter().map(String::as_str).collect::<Vec<_>>());
        }
        copy_in(&mut tx, "edges", EDGE_COLS, &buf)?;
        rep.side_edges = side_edges.len();
    }
    for (table, sql, cols) in SMALL {
        let (d, trows) = digest(sq, sql)?;
        if meta_get(&mut tx, table)? == d {
            continue;
        }
        rep.tables_replaced.push(table.to_string());
        tx.batch_execute(&format!("DELETE FROM {table}"))?;
        let mut buf = String::new();
        for r in &trows {
            copy_row(&mut buf, &r.iter().map(String::as_str).collect::<Vec<_>>());
        }
        copy_in(&mut tx, table, cols, &buf)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[table, &d],
        )?;
    }
    phase(&mut rep, "edges and tables");

    if rebuild {
        // Vectors whose card text is unchanged come back.
        tx.batch_execute(
            "UPDATE entities e SET card_vec = k.card_vec, vec_key = k.vec_key
             FROM keep_vec k WHERE k.id = e.id AND k.vec_key = e.text_key;
             DELETE FROM embed_queue q USING entities e
             WHERE e.id = q.entity_id AND e.vec_key = e.text_key AND e.card_vec IS NOT NULL;",
        )?;
    }
    // Statistics, when much was written (a cold sync's plans depend on them).
    if rebuild || rep.written > 1000 || rep.chunks > 1000 {
        tx.batch_execute("ANALYZE entities; ANALYZE chunks; ANALYZE labels; ANALYZE edges;")?;
    }
    rep.ms = start.elapsed().as_millis();
    tx.execute(
        "UPDATE runs SET finished = now(), coverage = $2 WHERE id = $1",
        &[&run_id, &serde_json::to_value(&rep)?],
    )?;
    tx.commit()?;
    drop(c);
    phase(&mut rep, "commit");
    // After much was written, the visibility map (which pg_search's heap
    // checks and index-only scans read) and the statistics; VACUUM can't run
    // inside the transaction.
    if rebuild || rep.written > 1000 || rep.chunks > 1000 {
        // One statement each: several in one batch are one transaction.
        pg.batch("VACUUM (ANALYZE) entities")?;
        pg.batch("VACUUM (ANALYZE) chunks")?;
        phase(&mut rep, "vacuum");
    }
    rep.ms = start.elapsed().as_millis();
    Ok(rep)
}
