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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

use super::{Pg, copy_row};
use crate::crawl::hash;
use crate::model::{ENTITY_COLS, Entity};
use crate::names;

/// What a sync did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub entities: usize,
    pub written: usize,
    pub deleted: usize,
    pub queued: usize,
    pub chunked_files: usize,
    pub chunks: usize,
    pub labels: usize,
    pub edges_replaced: bool,
    pub edges: usize,
    pub tables_replaced: Vec<String>,
    pub rebuild: bool,
    pub ms: u128,
    /// Milliseconds per step.
    pub phases: BTreeMap<String, u128>,
}

/// Files whose text goes into chunks: as v1's full-text table, the files
/// people read in a full-depth project.
pub const CHUNK_FILE_KINDS: &[&str] = &["source", "test", "doc", "config", "build"];
/// A chunk holds at most this many lines...
pub const CHUNK_LINES: usize = 400;
/// ...and this many bytes (a long line is cut).
pub const CHUNK_BYTES: usize = 32 * 1024;
/// Lexemes in fewer chunks than this aren't counted in `lexemes`: search
/// takes them for rare.
pub const LEXEME_FLOOR: i32 = 50;
/// A file's or dir's children named in its card, as v1.
const KIDS_IN_CARD: usize = 200;
/// Rows a COPY sends at a time.
const COPY_BATCH: usize = 20_000;

/// The text an entity's vector is made from: v1's card (its project and
/// path, title, summary or seed, why, and the names it defines).
pub fn card(
    project: &str,
    rel: &str,
    title: &str,
    summary: &str,
    why: &str,
    symbols: &str,
) -> String {
    let mut s = if rel.is_empty() {
        project.to_string()
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
        .take(60)
        .collect();
    if !syms.is_empty() {
        s.push_str("\ndefines: ");
        s.push_str(&syms.join(", "));
    }
    s.chars().take(2000).collect()
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
    pub text: String,
}

/// Cuts a file into chunks: a Markdown file by its headings (outside code
/// fences), anything else in windows of [`CHUNK_LINES`] lines; no chunk over
/// [`CHUNK_BYTES`] (a longer line is cut into pieces).
pub fn chunk_text(text: &str, markdown: bool) -> Vec<Chunk> {
    // Lines, a long one cut into pieces (all on its line number).
    let mut lines: Vec<(usize, &str)> = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let mut rest = l;
        while rest.len() > CHUNK_BYTES {
            let mut cut = CHUNK_BYTES;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            lines.push((i + 1, &rest[..cut]));
            rest = &rest[cut..];
        }
        lines.push((i + 1, rest));
    }
    // Sections: (heading, lines).
    let mut sections: Vec<(String, Vec<(usize, &str)>)> = vec![(String::new(), Vec::new())];
    let mut fence = false;
    for &(n, l) in &lines {
        let t = l.trim_start();
        if markdown && (t.starts_with("```") || t.starts_with("~~~")) {
            fence = !fence;
        }
        if markdown && !fence && t.starts_with('#') {
            let h = t.trim_start_matches('#');
            if h.starts_with(' ') || h.is_empty() {
                sections.push((h.trim().to_string(), Vec::new()));
            }
        }
        sections
            .last_mut()
            .expect("one section at least")
            .1
            .push((n, l));
    }
    let mut out = Vec::new();
    for (heading, ls) in sections {
        let mut cur: Vec<(usize, &str)> = Vec::new();
        let mut bytes = 0;
        let flush = |cur: &mut Vec<(usize, &str)>, out: &mut Vec<Chunk>| {
            if cur.iter().any(|(_, l)| !l.trim().is_empty()) {
                out.push(Chunk {
                    line_start: cur[0].0,
                    line_end: cur[cur.len() - 1].0,
                    heading: heading.clone(),
                    text: cur.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n"),
                });
            }
            cur.clear();
        };
        for (n, l) in ls {
            if !cur.is_empty() && (cur.len() >= CHUNK_LINES || bytes + l.len() + 1 > CHUNK_BYTES) {
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
    seed: String,
    summary: String,
    why: String,
    summary_key: String,
    summary_writer: String,
    summary_at: String,
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
     summary_writer, summary_at, terms, labels_text, symbols, embed, text_key, chunk_key, sync_key";

impl Out {
    fn copy(&self, buf: &mut String) {
        let e = &self.e;
        let (size, lines, line, imp) = (
            e.size.to_string(),
            e.lines.to_string(),
            e.line.to_string(),
            e.importance.to_string(),
        );
        copy_row(
            buf,
            &[
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
                &self.terms,
                &self.labels_text,
                &self.symbols,
                if self.embed { "t" } else { "f" },
                &self.text_key,
                &self.chunk_key,
                &self.sync_key,
            ],
        );
    }
}

/// A label row, as both stores hold it.
type Label = (String, String, String, String, String, f64, String);

/// Labels by entity id.
type Labels = HashMap<String, Vec<Label>>;

/// Reads the ledger: every entity as the sync writes it, with its labels.
fn read_ledger(sq: &Connection) -> Result<(Vec<Out>, Labels)> {
    let ents: Vec<Entity> = {
        let mut st = sq.prepare(&format!("SELECT {ENTITY_COLS} FROM entities ORDER BY id"))?;
        st.query_map([], Entity::from_row)?
            .collect::<rusqlite::Result<_>>()?
    };
    // The summary each entity shows (a model's or person's over a seed, the
    // latest first), and its latest seed.
    let mut shown: HashMap<String, (String, String, String, String, String)> = HashMap::new();
    let mut seeds: HashMap<String, String> = HashMap::new();
    {
        let mut st = sq.prepare(
            "SELECT entity, text, why, writer, content_key, written_at FROM summaries
             WHERE status = 'live' AND text != '' ORDER BY entity, (writer = 'seed'), seq DESC",
        )?;
        let mut rows = st.query([])?;
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
        let mut st = sq.prepare(
            "SELECT entity, label, norm, compact, kind, origin, weight, evidence FROM labels ORDER BY entity, label, kind",
        )?;
        let mut rows = st.query([])?;
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
        let mut st = sq.prepare(
            "SELECT parent, name FROM entities WHERE kind IN ('symbol', 'file', 'dir')
             ORDER BY parent, importance DESC, name",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let v = kids.entry(r.get(0)?).or_default();
            if v.len() < KIDS_IN_CARD {
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
    let mut out = Vec::with_capacity(ents.len());
    for e in ents {
        let (stext, why, writer, skey, sat) = shown.remove(&e.id).unwrap_or_default();
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
        let card_text = card(&e.project, &e.rel, &e.title, &stext, &why, &symbols);
        let text_key = hash(&[card_text.as_bytes()]);
        let embed =
            e.kind != "root" && crate::crawl::wants_embedding(&e.kind, &e.rel, &e.file_kind);
        let chunk_key = if e.kind == "file"
            && full.contains(&e.project)
            && e.no_content.is_empty()
            && CHUNK_FILE_KINDS.contains(&e.file_kind.as_str())
        {
            e.content_key.clone()
        } else {
            String::new()
        };
        let mut o = Out {
            e,
            seed,
            summary,
            why,
            summary_key: skey,
            summary_writer: writer,
            summary_at: sat,
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

/// Mirrors the ledger into Postgres (see the module's doc).
pub fn sync(sq: &Connection, pg: &Pg, rebuild: bool) -> Result<SyncReport> {
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
    let (rows, labels) = read_ledger(sq)?;
    rep.entities = rows.len();
    let projects: HashMap<String, String> = crate::roots::list_projects(sq)?
        .into_iter()
        .map(|p| (p.name, p.path))
        .collect();
    phase(&mut rep, "read ledger");

    let mut c = pg.client();
    let mut tx = c.transaction()?;
    // One sync of a schema at a time: a second waits for the first's commit,
    // then finds what it left.
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext('agent-index sync ' || $1))",
        &[&pg.schema],
    )?;
    let run_id: i64 = tx
        .query_one(
            "INSERT INTO runs (kind, config, started) VALUES ('sync', $1, now()) RETURNING id",
            &[&serde_json::json!({"rebuild": rebuild})],
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
    let have: HashMap<String, String> = tx
        .query("SELECT id, sync_key FROM entities", &[])?
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    phase(&mut rep, "read postgres");

    // Entities gone from the ledger.
    let ledger_ids: HashSet<&str> = rows.iter().map(|o| o.e.id.as_str()).collect();
    let gone: Vec<String> = have
        .keys()
        .filter(|id| !ledger_ids.contains(id.as_str()))
        .cloned()
        .collect();
    if !gone.is_empty() {
        tx.execute("DELETE FROM entities WHERE id = ANY($1)", &[&gone])?;
        tx.execute("DELETE FROM labels WHERE entity_id = ANY($1)", &[&gone])?;
        tx.execute(
            "DELETE FROM embed_queue WHERE entity_id = ANY($1)",
            &[&gone],
        )?;
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
                o.copy(&mut buf);
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
    phase(&mut rep, "entities");

    // Chunks of the files whose content changed, cut from the file on disk.
    if !rechunk.is_empty() {
        let ids: Vec<String> = rechunk.iter().map(|(i, _)| i.clone()).collect();
        tx.execute("DELETE FROM chunks WHERE entity_id = ANY($1)", &[&ids])?;
        let by_id: HashMap<&str, &Out> = rows.iter().map(|o| (o.e.id.as_str(), o)).collect();
        let mut buf = String::new();
        for (id, ck) in &rechunk {
            if ck.is_empty() {
                continue;
            }
            let Some(o) = by_id.get(id.as_str()) else {
                continue;
            };
            let Some(dir) = projects.get(&o.e.project) else {
                continue;
            };
            let Some(bytes) = crate::crawl::read_inside(Path::new(dir), &o.e.rel) else {
                continue;
            };
            if crate::files::is_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            rep.chunked_files += 1;
            for (i, ch) in chunk_text(&text, o.e.lang == "markdown").iter().enumerate() {
                let (ord, a, b) = (
                    i.to_string(),
                    ch.line_start.to_string(),
                    ch.line_end.to_string(),
                );
                let terms = camel_terms(&ch.text);
                copy_row(&mut buf, &[id, &ord, &a, &b, &ch.heading, &ch.text, &terms]);
                rep.chunks += 1;
            }
            if buf.len() > 16 << 20 {
                copy_in(
                    &mut tx,
                    "chunks",
                    "entity_id, ord, line_start, line_end, heading, text, terms",
                    &buf,
                )?;
                buf.clear();
            }
        }
        copy_in(
            &mut tx,
            "chunks",
            "entity_id, ord, line_start, line_end, heading, text, terms",
            &buf,
        )?;
    }
    // Document frequencies, when the chunks changed (or were never counted).
    let counted: bool = tx
        .query_one("SELECT EXISTS (SELECT 1 FROM lexemes)", &[])?
        .get(0);
    if !rechunk.is_empty() || rebuild || !counted {
        tx.batch_execute(&format!(
            "DELETE FROM lexemes;
             INSERT INTO lexemes (config, word, ndoc)
             SELECT 'simple', word, ndoc FROM ts_stat('SELECT tsv_simple FROM chunks') WHERE ndoc >= {LEXEME_FLOOR}
             UNION ALL
             SELECT 'english', word, ndoc FROM ts_stat('SELECT tsv_en FROM chunks') WHERE ndoc >= {LEXEME_FLOOR};"
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
        tx.batch_execute(&format!(
            "DELETE FROM edges e WHERE NOT EXISTS (
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
    phase(&mut rep, "commit");
    rep.ms = start.elapsed().as_millis();
    Ok(rep)
}
