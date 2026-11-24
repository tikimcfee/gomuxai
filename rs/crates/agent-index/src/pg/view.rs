//! `show`, `tree` and `related` on Postgres, giving v1's view structs (so
//! v1's text renderers and `--json` shapes serve both stores). The
//! breadcrumb, the subtree and the neighbors are recursive CTEs; resolving
//! an argument reads `aliases`, `entities` and `projects` here, never the
//! crawl's ledger.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Result, bail};
use postgres::Row;

use super::Pg;
use crate::crawl::normalize;
use crate::extract::first_sentence;
use crate::model::{self, Entity, Summary};
use crate::view::{
    Brief, EdgeView, LabelView, Related, Resolved, SHOW_CHILDREN, Show, Tree, TreeLine, tokens,
};

/// An entity and the summary it shows, as one row's columns.
const COLS: &str = "e.id, e.kind, e.parent, e.project, e.rel, e.name, e.path, e.file_kind, e.lang, e.size, \
     e.lines, e.line, e.content_key, e.title, e.signature, e.untracked, e.no_content, e.importance, \
     e.seed, e.summary, e.why, e.summary_writer, e.summary_key, e.summary_at";

/// The entity in a row's first 18 columns (v1's column order).
pub fn entity_of(r: &Row) -> Entity {
    Entity {
        id: r.get(0),
        kind: r.get(1),
        parent: r.get(2),
        project: r.get(3),
        rel: r.get(4),
        name: r.get(5),
        path: r.get(6),
        file_kind: r.get(7),
        lang: r.get(8),
        size: r.get(9),
        lines: r.get(10),
        line: r.get(11),
        content_key: r.get(12),
        title: r.get(13),
        signature: r.get(14),
        untracked: r.get(15),
        no_content: r.get(16),
        importance: r.get(17),
    }
}

/// The summary in columns 18 to 23 of a [`COLS`] row.
fn summary_of(r: &Row) -> Option<Summary> {
    let writer: String = r.get(21);
    if writer.is_empty() {
        return None;
    }
    let summary: String = r.get(19);
    Some(Summary {
        text: if summary.is_empty() {
            r.get(18)
        } else {
            summary
        },
        why: r.get(20),
        writer,
        content_key: r.get(22),
        written_at: r.get(23),
    })
}

fn brief_of(r: &Row) -> Brief {
    let e = entity_of(r);
    let s = summary_of(r);
    Brief {
        id: e.id.clone(),
        kind: e.kind.clone(),
        file_kind: e.file_kind.clone(),
        name: e.name.clone(),
        title: e.display_title().to_string(),
        summary: s
            .as_ref()
            .map(|s| first_sentence(&s.text))
            .unwrap_or_default(),
        freshness: model::freshness(&e.content_key, s.as_ref()).to_string(),
        importance: e.importance,
        line: e.line,
    }
}

pub fn get(pg: &Pg, id: &str) -> Result<Option<(Entity, Option<Summary>)>> {
    Ok(pg
        .query_opt(
            &format!("SELECT {COLS} FROM entities e WHERE e.id = $1"),
            &[&id],
        )?
        .map(|r| (entity_of(&r), summary_of(&r))))
}

/// Projects as (name, path).
pub fn projects(pg: &Pg) -> Result<Vec<(String, String)>> {
    Ok(pg
        .query("SELECT name, path FROM projects ORDER BY name", &[])?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect())
}

/// One project's last crawl: (is git, its path, the commit crawled).
pub fn project_crawl(pg: &Pg, name: &str) -> Result<Option<(bool, String, String)>> {
    Ok(pg
        .query_opt(
            "SELECT is_git, path, crawled_commit FROM projects WHERE name = $1",
            &[&name],
        )?
        .map(|r| (r.get(0), r.get(1), r.get(2))))
}

/// The deepest project whose directory holds `dir`, or "".
pub fn project_at(pg: &Pg, dir: &str) -> Result<String> {
    Ok(projects(pg)?
        .into_iter()
        .filter(|(_, p)| dir == p || dir.starts_with(&format!("{p}/")))
        .max_by_key(|(_, p)| p.len())
        .map(|(n, _)| n)
        .unwrap_or_default())
}

pub fn project_count(pg: &Pg) -> Result<i64> {
    Ok(pg.query("SELECT count(*) FROM entities WHERE kind = 'project'", &[])?[0].get(0))
}

/// What an old id is now, following renames (at most 20).
fn alias(pg: &Pg, id: &str) -> Result<Option<String>> {
    Ok(pg
        .query_opt(
            "WITH RECURSIVE a(id, n) AS (
                 SELECT new_id, 1 FROM aliases WHERE old_id = $1
                 UNION ALL SELECT x.new_id, a.n + 1 FROM a JOIN aliases x ON x.old_id = a.id WHERE a.n < 20)
             SELECT a.id FROM a JOIN entities e ON e.id = a.id ORDER BY a.n DESC LIMIT 1",
            &[&id],
        )?
        .map(|r| r.get(0)))
}

fn entity_at_path(pg: &Pg, path: &str) -> Result<Option<String>> {
    Ok(pg
        .query_opt(
            "SELECT id FROM entities WHERE path = $1 AND kind IN ('file', 'dir', 'project', 'root')
             ORDER BY CASE kind WHEN 'file' THEN 0 WHEN 'project' THEN 1 WHEN 'dir' THEN 2 ELSE 3 END LIMIT 1",
            &[&path],
        )?
        .map(|r| r.get(0)))
}

/// v1's resolve: an id, an old id, or a path (canonicalized against `cwd`).
pub fn resolve(pg: &Pg, arg: &str, cwd: &Path) -> Result<Resolved> {
    if get(pg, arg)?.is_some() {
        return Ok(Resolved {
            id: arg.to_string(),
            alias_of: None,
            nearest_of: None,
        });
    }
    if let Some(id) = alias(pg, arg)? {
        return Ok(Resolved {
            id,
            alias_of: Some(arg.to_string()),
            nearest_of: None,
        });
    }
    let looks_like_id = arg.starts_with('@')
        || arg
            .split_once(':')
            .is_some_and(|(p, _)| !p.is_empty() && !p.contains('/'));
    let raw = Path::new(arg);
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let canon = match std::fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(_) if looks_like_id => {
            bail!("no entity {arg} in the index: agent index search finds ids")
        }
        Err(e) => bail!("{}: {e}", abs.display()),
    };
    let path = normalize(&canon);
    if let Some(id) = entity_at_path(pg, &path)? {
        return Ok(Resolved {
            id,
            alias_of: None,
            nearest_of: None,
        });
    }
    let projects = projects(pg)?;
    let Some((pname, ppath)) = projects
        .iter()
        .filter(|(_, p)| path == *p || path.starts_with(&format!("{p}/")))
        .max_by_key(|(_, p)| p.len())
    else {
        bail!(
            "{path} is outside every indexed project: agent index root list shows what is indexed (a symlink is never followed out of its project)"
        );
    };
    let mut cur = Path::new(&path).parent();
    while let Some(c) = cur {
        let s = normalize(c);
        if s.len() < ppath.len() {
            break;
        }
        if let Some(id) = entity_at_path(pg, &s)? {
            return Ok(Resolved {
                id,
                alias_of: None,
                nearest_of: Some(path),
            });
        }
        cur = c.parent();
    }
    bail!("{path} isn't indexed yet: agent index crawl --project {pname}")
}

fn relation(kind: &str, outgoing: bool) -> String {
    match (kind, outgoing) {
        ("depends-on", true) => "depends on",
        ("depends-on", false) => "used by",
        ("documented-by", true) => "documented by",
        ("documented-by", false) => "documents",
        ("tests", true) => "tests",
        ("tests", false) => "tested by",
        ("co-changes", _) => "changes with",
        ("noted-in", true) => "noted in",
        ("noted-in", false) => "notes",
        ("about", true) => "about",
        ("about", false) => "has member",
        ("same-as", _) => "same as",
        ("implements", true) => "implements",
        ("implements", false) => "implemented by",
        // The side corpora's edges (docs/INDEX-V2.md, "Corpora, composed").
        ("touched", true) => "touched",
        ("touched", false) => "touched by sessions",
        ("reported-in", true) => "reported in",
        ("reported-in", false) => "reports",
        (k, _) => k,
    }
    .to_string()
}

/// Edges out from `id`, to `depth` hops, by recursive CTE: each reached
/// entity with the edge that reached it first (fewest hops, then weight).
/// Rows: (hop, kind, other, weight, origin, evidence, outgoing) and the
/// other's [`COLS`] when it is an entity.
/// One step of a walk: its hop, the edge, whether it goes out, and the
/// entity reached (None for an id no entity has).
type Step = (i32, EdgeView, bool, Option<Brief>);

/// Edges grouped by how they read, each with the entity at its other end.
type Grouped = BTreeMap<String, Vec<(EdgeView, Option<Brief>)>>;

fn walk(pg: &Pg, id: &str, depth: i32) -> Result<Vec<Step>> {
    let rows = pg.query(
        &format!(
            "WITH RECURSIVE w(hop, kind, other, weight, origin, evidence, outgoing, trail) AS (
                 SELECT 1, g.kind, g.other, g.weight, g.origin, g.evidence, g.outgoing, ARRAY[$1::text, g.other]
                 FROM (SELECT kind, dst AS other, weight, origin, evidence, true AS outgoing FROM edges WHERE src = $1
                       UNION ALL
                       SELECT kind, src, weight, origin, evidence, false FROM edges WHERE dst = $1) g
                 UNION ALL
                 SELECT w.hop + 1, g.kind, g.other, g.weight, g.origin, g.evidence, g.outgoing, w.trail || g.other
                 FROM w CROSS JOIN LATERAL (
                     SELECT kind, dst AS other, weight, origin, evidence, true AS outgoing FROM edges WHERE src = w.other
                     UNION ALL
                     SELECT kind, src, weight, origin, evidence, false FROM edges WHERE dst = w.other) g
                 WHERE w.hop < $2 AND NOT g.other = ANY(w.trail)
             )
             SELECT w.hop, w.kind, w.other, w.weight, w.origin, w.evidence, w.outgoing, {COLS}
             FROM w LEFT JOIN entities e ON e.id = w.other
             ORDER BY w.hop, w.weight DESC, w.other"
        ),
        &[&id, &depth],
    )?;
    let mut out = Vec::new();
    for r in rows {
        let kind: String = r.get(1);
        let outgoing: bool = r.get(6);
        let ev = EdgeView {
            relation: relation(&kind, outgoing),
            kind,
            other: r.get(2),
            weight: r.get(3),
            origin: r.get(4),
            evidence: r.get(5),
        };
        // The entity's columns start at 7.
        let brief = r.get::<_, Option<String>>(7).map(|_| {
            let e = Entity {
                id: r.get(7),
                kind: r.get(8),
                parent: r.get(9),
                project: r.get(10),
                rel: r.get(11),
                name: r.get(12),
                path: r.get(13),
                file_kind: r.get(14),
                lang: r.get(15),
                size: r.get(16),
                lines: r.get(17),
                line: r.get(18),
                content_key: r.get(19),
                title: r.get(20),
                signature: r.get(21),
                untracked: r.get(22),
                no_content: r.get(23),
                importance: r.get(24),
            };
            let writer: String = r.get(28);
            let s = (!writer.is_empty()).then(|| {
                let summary: String = r.get(26);
                Summary {
                    text: if summary.is_empty() {
                        r.get(25)
                    } else {
                        summary
                    },
                    why: r.get(27),
                    writer,
                    content_key: r.get(29),
                    written_at: r.get(30),
                }
            });
            Brief {
                id: e.id.clone(),
                kind: e.kind.clone(),
                file_kind: e.file_kind.clone(),
                name: e.name.clone(),
                title: e.display_title().to_string(),
                summary: s
                    .as_ref()
                    .map(|s| first_sentence(&s.text))
                    .unwrap_or_default(),
                freshness: model::freshness(&e.content_key, s.as_ref()).to_string(),
                importance: e.importance,
                line: e.line,
            }
        });
        out.push((r.get(0), ev, outgoing, brief));
    }
    Ok(out)
}

/// An entity's edges both ways, grouped by how they read from it (v1).
fn edges(pg: &Pg, id: &str) -> Result<Grouped> {
    let mut out: Grouped = BTreeMap::new();
    for (_, ev, outgoing, b) in walk(pg, id, 1)? {
        // co-changes is stored both ways: shown once.
        if ev.kind == "co-changes" && !outgoing {
            continue;
        }
        out.entry(ev.relation.clone()).or_default().push((ev, b));
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| {
            b.0.weight
                .total_cmp(&a.0.weight)
                .then_with(|| a.0.other.cmp(&b.0.other))
        });
    }
    Ok(out)
}

pub fn show(pg: &Pg, r: Resolved) -> Result<Show> {
    let Some((e, summary)) = get(pg, &r.id)? else {
        bail!("no entity {}", r.id);
    };
    let freshness = model::freshness(&e.content_key, summary.as_ref()).to_string();
    let breadcrumb: Vec<Brief> = pg
        .query(
            &format!(
                "WITH RECURSIVE up(id, n) AS (
                     SELECT parent, 1 FROM entities WHERE id = $1 AND parent <> ''
                     UNION ALL
                     SELECT e.parent, up.n + 1 FROM up JOIN entities e ON e.id = up.id
                     WHERE e.parent <> '' AND up.n < 100)
                 SELECT {COLS} FROM up JOIN entities e ON e.id = up.id ORDER BY up.n DESC"
            ),
            &[&e.id],
        )?
        .iter()
        .map(brief_of)
        .collect();
    let kids = pg.query(
        &format!(
            "SELECT {COLS} FROM entities e WHERE e.parent = $1
             ORDER BY e.importance DESC, CASE e.kind WHEN 'project' THEN 0 WHEN 'dir' THEN 1 WHEN 'file' THEN 2 ELSE 3 END, e.line, e.name"
        ),
        &[&e.id],
    )?;
    let (syms, rest): (Vec<&Row>, Vec<&Row>) = kids.iter().partition(|k| {
        let kind: String = k.get(1);
        e.kind == "file" && (kind == "symbol" || kind == "section")
    });
    let mut symbols: Vec<Brief> = syms.into_iter().map(brief_of).collect();
    if e.kind == "file" {
        let (lo, hi) = crate::crawl::prefix_range(&format!("{}#", e.id));
        for m in pg.query(
            &format!(
                "SELECT {COLS} FROM entities e WHERE e.id >= $1 AND e.id < $2 AND e.kind = 'section' AND e.parent <> $3"
            ),
            &[&lo, &hi, &e.id],
        )? {
            symbols.push(brief_of(&m));
        }
        symbols.sort_by_key(|b| b.line);
    }
    let children_total = rest.len();
    let children: Vec<Brief> = rest.into_iter().take(SHOW_CHILDREN).map(brief_of).collect();
    let labels: Vec<LabelView> = pg
        .query(
            "SELECT label, kind, origin, evidence FROM labels WHERE entity_id = $1
             ORDER BY origin <> 'person', kind, label",
            &[&e.id],
        )?
        .iter()
        .map(|r| LabelView {
            label: r.get(0),
            kind: r.get(1),
            origin: r.get(2),
            evidence: r.get(3),
        })
        .collect();
    let dangling: Vec<(String, i64)> = pg
        .query(
            "SELECT ref, line FROM dangling WHERE doc = $1 ORDER BY line",
            &[&e.id],
        )?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let edges = edges(pg, &e.id)?
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|(e, _)| e).collect()))
        .collect();
    Ok(Show {
        edges,
        entity: e,
        resolved: r,
        freshness,
        summary,
        breadcrumb,
        children,
        children_total,
        symbols,
        labels,
        dangling,
    })
}

pub fn related(pg: &Pg, id: &str) -> Result<Related> {
    let mut groups = BTreeMap::new();
    for (rel, es) in edges(pg, id)? {
        let v: Vec<(EdgeView, Brief)> = es.into_iter().filter_map(|(e, b)| Some((e, b?))).collect();
        if !v.is_empty() {
            groups.insert(rel, v);
        }
    }
    Ok(Related {
        id: id.to_string(),
        groups,
    })
}

/// One node of a subtree: its line, its sort key, its kind.
struct Node {
    line: TreeLine,
    importance: f64,
    kind_order: u8,
    at: i64,
}

/// v1's tree: the subtree under `id` (or every root and project when `id`
/// is empty) to `depth`, most important first, fitted to `budget` tokens.
/// One recursive CTE reads the subtree and each node's child count; the
/// walk and the budget are v1's.
pub fn tree(pg: &Pg, id: &str, depth: usize, budget: usize) -> Result<Tree> {
    let (tops, max, base): (Vec<Row>, usize, usize) = if id.is_empty() {
        (
            pg.query(
                &format!(
                    "SELECT {COLS} FROM entities e WHERE e.kind = 'root' OR (e.kind = 'project' AND e.parent = '')
                     ORDER BY e.kind DESC, e.name"
                ),
                &[],
            )?,
            depth,
            0,
        )
    } else {
        (Vec::new(), depth.max(1), 1)
    };
    let starts: Vec<String> = if id.is_empty() {
        tops.iter().map(|r| r.get(0)).collect()
    } else {
        vec![id.to_string()]
    };
    let rows = pg.query(
        &format!(
            "WITH RECURSIVE t(id, depth) AS (
                 SELECT e.id, 1 FROM entities e WHERE e.parent = ANY($1) AND e.kind NOT IN ('symbol', 'section')
                 UNION ALL
                 SELECT e.id, t.depth + 1 FROM t JOIN entities p ON p.id = t.id JOIN entities e ON e.parent = t.id
                 WHERE t.depth < $2 AND p.kind IN ('dir', 'project', 'root') AND e.kind NOT IN ('symbol', 'section'))
             SELECT {COLS}, t.depth,
                    (SELECT count(*) FROM entities c WHERE c.parent = e.id AND c.kind NOT IN ('symbol', 'section'))
             FROM t JOIN entities e ON e.id = t.id"
        ),
        &[&starts, &(max as i32)],
    )?;
    let node = |r: &Row, depth: usize, count: i64| {
        let b = brief_of(r);
        let kind: String = r.get(1);
        Node {
            kind_order: match kind.as_str() {
                "project" => 0,
                "dir" => 1,
                "file" => 2,
                _ => 3,
            },
            importance: r.get(17),
            at: r.get(11),
            line: TreeLine {
                depth,
                id: b.id,
                kind,
                name: b.name,
                title: b.title,
                summary: b.summary,
                children: count as usize,
            },
        }
    };
    let mut kids: HashMap<String, Vec<Node>> = HashMap::new();
    for r in &rows {
        let parent: String = r.get(2);
        let d: i32 = r.get(24);
        let n: i64 = r.get(25);
        // The depth is set again as the walk reaches it.
        kids.entry(parent).or_default().push(node(r, d as usize, n));
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| {
            b.importance
                .total_cmp(&a.importance)
                .then(a.kind_order.cmp(&b.kind_order))
                .then(a.at.cmp(&b.at))
                .then_with(|| a.line.name.cmp(&b.line.name))
        });
    }
    let mut w = Walk {
        kids: &mut kids,
        max,
        budget,
        base,
        used: 0,
        left_out: 0,
        lines: Vec::new(),
    };
    if id.is_empty() {
        // v1 prints the tops without a count.
        let tops: Vec<Node> = tops.iter().map(|r| node(r, 0, 0)).collect();
        let n = tops.len();
        for (i, t) in tops.into_iter().enumerate() {
            let tid = t.line.id.clone();
            if !(w.push(t.line) && w.walk(&tid, 1)) {
                w.left_out += n - i;
                break;
            }
        }
    } else {
        w.walk(id, 1);
    }
    Ok(Tree {
        root: id.to_string(),
        lines: w.lines,
        left_out: w.left_out,
        base,
    })
}

struct Walk<'a> {
    kids: &'a mut HashMap<String, Vec<Node>>,
    max: usize,
    budget: usize,
    base: usize,
    used: usize,
    left_out: usize,
    lines: Vec<TreeLine>,
}

impl Walk<'_> {
    fn push(&mut self, l: TreeLine) -> bool {
        let t = tokens(&crate::view::tree_line_text(&l, self.base));
        if self.budget > 0 && self.used + t > self.budget {
            return false;
        }
        self.used += t;
        self.lines.push(l);
        true
    }

    fn walk(&mut self, id: &str, depth: usize) -> bool {
        if depth > self.max {
            return true;
        }
        let kids = self.kids.remove(id).unwrap_or_default();
        let n = kids.len();
        for (i, mut k) in kids.into_iter().enumerate() {
            k.line.depth = depth;
            let kid = k.line.id.clone();
            let walks = matches!(k.line.kind.as_str(), "dir" | "project" | "root");
            let fits = self.push(k.line) && (!walks || self.walk(&kid, depth + 1));
            if !fits {
                self.left_out += n - i;
                return false;
            }
        }
        true
    }
}
