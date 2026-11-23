//! `agent index serve` (docs/INDEX-V2.md, "A warm query process"): a
//! process on a Unix socket that keeps database connections, the query
//! embedder and the reranker's client warm. `agent index search` asks it
//! when it answers and runs cold otherwise; both go through [`answer`], so
//! the answer is the same either way.
//!
//! The protocol is one JSON line each way per connection: a [`Request`],
//! then a [`Response`]. The server refuses a request from another build of
//! `agent` or under another configuration (its fingerprint), and the client
//! then runs cold and says why.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Pg;
use super::search::{self, Lexical};
use crate::config::Config;
use crate::query::{SearchOptions, SearchResult};

/// A query embedder that threads may share.
pub type Embed = Arc<dyn Fn(&str) -> Result<Vec<f32>> + Send + Sync>;

/// One search, as the CLI asks it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Request {
    pub query: String,
    pub kind: String,
    pub project: String,
    /// Absolute and canonical, or "".
    pub under: String,
    /// 0: the configuration's.
    pub limit: usize,
    /// The caller's directory, canonical: its project is ranked first.
    pub cwd: String,
    pub lexical: String,
    /// The corpora to answer from, comma-separated ("": all of them when
    /// grouped, code and prose when flat).
    #[serde(default)]
    pub corpus: String,
    /// Rerank even when the configuration has it off.
    pub rerank: bool,
    /// Who asks (`AGENT_NAME`), for the query log.
    pub who: String,
    /// The asker's configuration fingerprint and build, which must match.
    pub fingerprint: String,
    pub build: String,
}

/// The answer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Response {
    pub result: Option<SearchResult>,
    /// The project the caller is in.
    pub caller: String,
    /// The limit used (what "more matched" compares with).
    pub limit: usize,
    /// A note for the person (the caller's project is ahead of its crawl).
    pub hint: Option<String>,
    pub error: Option<String>,
}

/// This binary's identity: its path, size and modification time. A server
/// and a client of different builds don't trust each other's answers.
pub fn build_id() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return String::new();
    };
    let md = std::fs::metadata(&exe).ok();
    let mtime = md
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{} {} {mtime}",
        exe.display(),
        md.map(|m| m.len()).unwrap_or(0)
    )
}

/// The one search path, cold or warm: the caller's project from its
/// directory, the search, the query log, and the behind-its-crawl hint.
pub fn answer(
    pg: &Pg,
    req: &Request,
    embed: Option<&Embed>,
    rerank: Option<&super::rerank::Client>,
) -> Result<Response> {
    let caller = super::view::project_at(pg, &req.cwd)?;
    let limit = if req.limit == 0 {
        pg.cfg.search.limit
    } else {
        req.limit
    };
    let o = SearchOptions {
        query: req.query.clone(),
        kind: req.kind.clone(),
        project: req.project.clone(),
        under: req.under.clone(),
        limit,
        caller_project: caller.clone(),
        scope: String::new(),
        corpora: req
            .corpus
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect(),
    };
    for c in &o.corpora {
        if !crate::config::CORPORA.contains(&c.as_str()) {
            bail!(
                "--corpus takes {}, not {c:?}",
                crate::config::CORPORA.join(", ")
            );
        }
    }
    let lexical = Lexical::resolve(pg, &req.lexical)?;
    let own;
    let rerank = if req.rerank || pg.cfg.reranker.enabled {
        match rerank {
            Some(r) => Some(r),
            None => {
                own = super::rerank::Client::new(&pg.cfg.reranker);
                Some(&own)
            }
        }
    } else {
        None
    };
    let f = embed.map(|e| e.as_ref() as &dyn Fn(&str) -> Result<Vec<f32>>);
    // No --kind: grouped by corpus (its `hits` are the flat answer); a
    // --kind (as --kind file): one flat list, as before groups existed.
    let mut r = if o.kind.is_empty() {
        search::search_grouped(pg, &o, f, lexical, rerank)?
    } else {
        search::search(pg, &o, f, lexical, rerank)?
    };
    r.lexical = Some(lexical.name().to_string());
    // A failed log doesn't fail the search.
    let _ = search::log_query(pg, &req.who, &r);
    let project = if req.project.is_empty() {
        &caller
    } else {
        &req.project
    };
    let mut hint = None;
    if !project.is_empty()
        && let Ok(Some((true, path, last))) = super::view::project_crawl(pg, project)
        && !last.is_empty()
    {
        let head = crate::git::head(Path::new(&path));
        if !head.is_empty() && head != last {
            let n = crate::git::commits_between(Path::new(&path), &last, &head);
            hint = Some(format!(
                "{project} is {n} commits ahead of its last crawl: agent index crawl --project {project}"
            ));
        }
    }
    Ok(Response {
        result: Some(r),
        caller,
        limit,
        hint,
        error: None,
    })
}

/// Asks the server at `socket`. An error when nothing answers there, or it
/// refused (another build or configuration): the caller then runs cold.
pub fn ask(socket: &Path, req: &Request, timeout: Duration) -> Result<Response> {
    let mut s = UnixStream::connect(socket)
        .with_context(|| format!("no index server at {}", socket.display()))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut buf = String::new();
    BufReader::new(&s).read_line(&mut buf)?;
    let r: Response = serde_json::from_str(&buf).context("the index server's answer")?;
    if let Some(e) = &r.error {
        bail!("{e}");
    }
    Ok(r)
}

/// What the server shares between its workers.
pub struct Server {
    pub url: String,
    pub schema: String,
    pub config: Config,
    pub embed: Option<Embed>,
}

fn handle(pg: &Pg, rr: &super::rerank::Client, embed: Option<&Embed>, build: &str, s: UnixStream) {
    let mut buf = String::new();
    let mut reader = BufReader::new(&s);
    let resp = match reader.read_line(&mut buf) {
        Err(e) => Response {
            error: Some(format!("reading the request: {e}")),
            ..Default::default()
        },
        Ok(_) => match serde_json::from_str::<Request>(&buf) {
            Err(e) => Response {
                error: Some(format!("the request isn't one: {e}")),
                ..Default::default()
            },
            Ok(req) if req.build != build => Response {
                error: Some("another build of agent serves this socket".into()),
                ..Default::default()
            },
            Ok(req) if req.fingerprint != pg.cfg.fingerprint() => Response {
                error: Some(format!(
                    "the server runs configuration {}, the search asks for {}",
                    pg.cfg.fingerprint(),
                    req.fingerprint
                )),
                ..Default::default()
            },
            Ok(req) => answer(pg, &req, embed, Some(rr)).unwrap_or_else(|e| Response {
                error: Some(format!("{e:#}")),
                ..Default::default()
            }),
        },
    };
    let mut out = serde_json::to_vec(&resp).unwrap_or_default();
    out.push(b'\n');
    let mut w = &s;
    let _ = w.write_all(&out);
}

/// Serves on `socket` until the process ends: `workers` threads, each with
/// its own database connection and reranker client, the embedder shared.
/// A socket file left by a server that is gone is replaced; one a live
/// server answers on is refused.
pub fn serve(srv: Server, socket: &Path, workers: usize) -> Result<()> {
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!("an index server already answers on {}", socket.display());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("removing the stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    serve_on(srv, listener, workers)
}

/// Serves on a bound listener (tests bind their own).
pub fn serve_on(srv: Server, listener: UnixListener, workers: usize) -> Result<()> {
    let build = build_id();
    let (tx, rx) = mpsc::channel::<UnixStream>();
    let rx = Arc::new(Mutex::new(rx));
    let mut conns = Vec::new();
    // Connect every worker before taking a request, so a bad URL fails now.
    for _ in 0..workers.max(1) {
        conns.push(Pg::connect_with(&srv.url, &srv.schema, srv.config.clone())?);
    }
    for pg in conns {
        let rx = Arc::clone(&rx);
        let embed = srv.embed.clone();
        let build = build.clone();
        std::thread::spawn(move || {
            let rr = super::rerank::Client::new(&pg.cfg.reranker);
            loop {
                let s = match rx.lock().map(|r| r.recv()) {
                    Ok(Ok(s)) => s,
                    _ => return,
                };
                handle(&pg, &rr, embed.as_ref(), &build, s);
            }
        });
    }
    for s in listener.incoming() {
        match s {
            Ok(s) => {
                if tx.send(s).is_err() {
                    bail!("the index server's workers are gone");
                }
            }
            Err(e) => eprintln!("index server: accepting a connection: {e}"),
        }
    }
    Ok(())
}

/// The socket's default place: `index.sock` in the data directory (beside
/// `index.db`).
pub fn default_socket(index_db: &Path) -> PathBuf {
    index_db
        .parent()
        .unwrap_or(Path::new("."))
        .join("index.sock")
}
