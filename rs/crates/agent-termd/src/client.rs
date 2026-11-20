use crate::chord::{caller_env, caller_env_os, parse_special_key, sanitize_request_env};
use crate::ops::{Ops, PaneInfo, Session};
use crate::paths::{attach_binary, check_test_server, validate_namespace, validate_terminal_id};
use crate::wire::{
    WireError, dial_namespace, ensure_daemon, is_not_running, is_wire_code, request, request_raw,
    with_read_conn, wrap_err,
};
use agent_base::gostd::strconv::quote;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// The daemon namespace gomuxai uses when none is configured: isolated from
/// interactive (`"default"`) and wall (`"wall"`) terminals.
pub const DEFAULT_NAMESPACE: &str = "agents";

/// Names the scratch socket directory a test binary runs its private
/// `g3d-termd` daemons in ([`crate::testing`]).
pub const TEST_SERVER_ENV: &str = "GOMUXAI_TEST_TERMD_DIR";

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;
const PASTE_END: &str = "\x1b[201~";

/// Configures a [`Client`].
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub namespace: String,
    pub daemon_bin: String,
    pub attach_bin: String,
    pub initial_cols: u16,
    pub initial_rows: u16,
}

/// The daemon's status document (op `"status"`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusDoc {
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub daemon: DaemonStatus,
    #[serde(default)]
    pub terminals: Vec<TerminalStatus>,
}

/// Describes the running `g3d-termd` holder and brain.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub proto: [u32; 2],
    #[serde(default)]
    pub ns: String,
    #[serde(default)]
    pub socket: String,
    #[serde(default)]
    pub log: String,
    #[serde(default)]
    pub exiting: bool,
    #[serde(default)]
    pub instance: String,
}

/// Describes one live terminal in [`StatusDoc`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStatus {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub handle: u32,
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub login: bool,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub pwd: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub processes: i64,
    #[serde(default)]
    pub subscribers: Vec<SubscriberStatus>,
}

/// Describes one active subscriber on a terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriberStatus {
    #[serde(default)]
    pub conn: u64,
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub flow: String,
}

/// Implements [`Ops`] against a `g3d-termd` namespace.
#[derive(Clone, Debug)]
pub struct Client {
    ns: String,
    daemon_bin: String,
    attach_bin: String,
    cols: u16,
    rows: u16,
}

/// Constructs a termd [`Client`] after validating its namespace.
pub fn new_client(opts: Options) -> Result<Client> {
    Client::new(opts)
}

impl Client {
    /// Constructs a termd [`Client`] after validating its namespace. The daemon
    /// binary is resolved lazily when a terminal is created so read-only
    /// commands and global hooks only need socket access.
    pub fn new(opts: Options) -> Result<Self> {
        check_test_server()?;
        let ns = if opts.namespace.is_empty() {
            DEFAULT_NAMESPACE.to_owned()
        } else {
            opts.namespace
        };
        validate_namespace(&ns)?;
        let cols = if opts.initial_cols == 0 {
            DEFAULT_COLS
        } else {
            opts.initial_cols
        };
        let rows = if opts.initial_rows == 0 {
            DEFAULT_ROWS
        } else {
            opts.initial_rows
        };
        Ok(Self {
            ns,
            daemon_bin: opts.daemon_bin,
            attach_bin: opts.attach_bin,
            cols,
            rows,
        })
    }

    /// Returns the daemon namespace this client manages.
    pub fn namespace(&self) -> &str {
        &self.ns
    }

    /// Returns the namespace's status document. When no daemon is running in
    /// the namespace, it returns an empty [`StatusDoc`] (`0` live terminals).
    pub fn status(&self) -> Result<StatusDoc> {
        #[derive(Deserialize)]
        struct StatusReplyBody {
            #[serde(default)]
            status: StatusDoc,
        }
        #[derive(Deserialize)]
        struct ListReplyBody {
            #[serde(default)]
            terminals: Vec<TerminalStatus>,
        }

        let ns = &self.ns;
        let res = with_read_conn(ns, |c| {
            if c.welcome.features.iter().any(|f| f == "status") {
                let reply: StatusReplyBody =
                    request(c, g3d_wire::Op::Status).map_err(|e| wrap_err("termd status", e))?;
                return Ok(reply.status);
            }
            let list: ListReplyBody =
                request(c, g3d_wire::Op::List).map_err(|e| wrap_err("termd list", e))?;
            Ok(StatusDoc {
                at: 0,
                daemon: DaemonStatus {
                    ns: ns.clone(),
                    proto: c.welcome.proto,
                    instance: c.welcome.instance.clone().unwrap_or_default(),
                    ..Default::default()
                },
                terminals: list.terminals,
            })
        });

        match res {
            Ok(doc) => Ok(doc),
            Err(err) if is_not_running(&err) => Ok(StatusDoc::default()),
            Err(err) => Err(err),
        }
    }

    /// Stops the namespace's daemon and kills every terminal in it.
    pub fn stop_daemon(&self) -> Result<()> {
        let mut c = match dial_namespace(&self.ns) {
            Ok(c) => c,
            Err(err) if is_not_running(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        request::<g3d_wire::Empty>(&mut c, g3d_wire::Op::Stop)?;
        Ok(())
    }

    /// Returns the `g3d` executable and argv to replace this process with when
    /// attaching a local terminal to `session`.
    pub fn attach_command(&self, session: &str) -> Result<(String, Vec<String>)> {
        validate_terminal_id(session)?;
        let bin = attach_binary(&self.attach_bin, &self.daemon_bin)?;
        let argv = vec![
            "g3d".to_owned(),
            "--ns".to_owned(),
            self.ns.clone(),
            "attach".to_owned(),
            "--take".to_owned(),
            session.to_owned(),
        ];
        Ok((bin.to_string_lossy().into_owned(), argv))
    }

    /// Creates a new terminal running the user's shell in the current working
    /// directory.
    pub fn create_session(&self, name: &str, env: &HashMap<String, String>) -> Result<()> {
        self.create_session_in(name, "", env)
    }

    /// Creates a new terminal running the user's shell in `dir` (or the current
    /// working directory when `dir` is `""`).
    pub fn create_session_in(
        &self,
        name: &str,
        dir: &str,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        self.create_session_with_env(name, dir, caller_env_os(), env)
    }

    fn create_session_with_env(
        &self,
        name: &str,
        dir: &str,
        env_base: BTreeMap<String, String>,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        validate_terminal_id(name)?;
        let cwd = if !dir.is_empty() {
            Some(dir.to_owned())
        } else {
            std::env::current_dir()
                .ok()
                .map(|d| d.to_string_lossy().into_owned())
        };
        let mut c = ensure_daemon(&self.ns, &self.daemon_bin)?;
        let create = g3d_wire::Create {
            id: name.to_owned(),
            cols: self.cols,
            rows: self.rows,
            cwd: cwd.filter(|s| !s.is_empty()),
            env: sanitize_request_env(env),
            env_base: Some(env_base),
            if_exists: Some(g3d_wire::IfExists::Error),
            ..Default::default()
        };
        if let Err(err) = request_raw(&mut c, g3d_wire::Op::Create(create)) {
            if is_wire_code(&err, g3d_wire::code::EXISTS) {
                bail!("session {} already exists", quote(name));
            }
            return Err(wrap_err(format!("creating session {}", quote(name)), err));
        }
        Ok(())
    }

    /// Terminates a terminal and its entire process tree.
    pub fn kill_session(&self, name: &str) -> Result<()> {
        let mut c = match dial_namespace(&self.ns) {
            Ok(c) => c,
            Err(err) if is_not_running(&err) => bail!("session {} not found", quote(name)),
            Err(err) => return Err(err),
        };
        let kill = g3d_wire::Kill {
            id: name.to_owned(),
            signal: None,
        };
        if let Err(err) = request_raw(&mut c, g3d_wire::Op::Kill(kill)) {
            if is_wire_code(&err, g3d_wire::code::NO_TERMINAL) {
                bail!("session {} not found", quote(name));
            }
            return Err(wrap_err(format!("killing session {}", quote(name)), err));
        }
        Ok(())
    }

    /// Kills what runs in `session` and starts a fresh shell under the same
    /// terminal id, preserving its directory, original caller `env_base`, and
    /// request `env`.
    pub fn respawn_pane(&self, session: &str) -> Result<()> {
        let st = self.status()?;
        let found = st
            .terminals
            .iter()
            .find(|t| t.id == session)
            .ok_or_else(|| anyhow::anyhow!("session {} not found", quote(session)))?;
        let (base_env, req_env) = match self.terminal_env_of(session) {
            Some((b, r)) => (b, r),
            None => (caller_env_os(), HashMap::new()),
        };
        if let Err(err) = self.kill_session(session)
            && !err.to_string().contains("not found")
        {
            return Err(err);
        }
        let cwd = resolve_terminal_cwd(&found.pwd, &found.cwd);
        self.create_session_with_env(session, &cwd, base_env, &req_env)
    }

    fn query_terminal_env(&self, session: &str) -> Option<Vec<EnvEntry>> {
        #[derive(Deserialize)]
        struct EnvReplyBody {
            #[serde(default)]
            env: Vec<EnvEntry>,
        }

        let mut supported = false;
        let entries = with_read_conn(&self.ns, |c| {
            if !c.welcome.features.iter().any(|f| f == "env") {
                return Ok(Vec::new());
            }
            let reply: EnvReplyBody = request(
                c,
                g3d_wire::Op::Env(g3d_wire::EnvQuery {
                    id: session.to_owned(),
                }),
            )?;
            supported = true;
            Ok(reply.env)
        })
        .ok()?;

        supported.then_some(entries)
    }

    fn terminal_env_of(
        &self,
        session: &str,
    ) -> Option<(BTreeMap<String, String>, HashMap<String, String>)> {
        let entries = self.query_terminal_env(session)?;
        let mut base_pairs = Vec::new();
        let mut req_env = HashMap::new();
        for e in entries {
            match e.source.as_str() {
                "request" => {
                    req_env.insert(e.name, e.value);
                }
                "termd" => {
                    // Layer-6 vars are synthesized by g3d-termd on create.
                }
                _ => {
                    base_pairs.push(format!("{}={}", e.name, e.value));
                }
            }
        }
        Some((caller_env(&base_pairs), req_env))
    }

    /// Reports whether a live terminal named `name` exists.
    pub fn session_exists(&self, name: &str) -> Result<bool> {
        let st = self.status()?;
        Ok(st.terminals.iter().any(|t| t.id == name))
    }

    /// Lists every live terminal in the namespace.
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let st = self.status()?;
        Ok(st
            .terminals
            .into_iter()
            .map(|t| {
                let attached = t
                    .subscribers
                    .iter()
                    .any(|sub| sub.role == "own" || sub.role == "attach");
                Session {
                    name: t.id,
                    created: (t.created_at / 1000).to_string(),
                    attached,
                    session_id: format!("${}", t.handle),
                }
            })
            .collect())
    }

    /// Lists the ids of live terminals in the namespace.
    pub fn session_names(&self) -> Result<Vec<String>> {
        let st = self.status()?;
        Ok(st.terminals.into_iter().map(|t| t.id).collect())
    }

    /// Returns a variable from `session`'s environment (preferring the create
    /// request override over base environment), or `""`.
    pub fn session_env(&self, session: &str, name: &str) -> String {
        let Some(entries) = self.query_terminal_env(session) else {
            return String::new();
        };
        let mut fallback: Option<String> = None;
        for e in entries {
            if e.name != name {
                continue;
            }
            if e.source == "request" {
                return e.value;
            }
            fallback = Some(e.value);
        }
        fallback.unwrap_or_default()
    }

    fn with_writer(
        &self,
        session: &str,
        feature: &str,
        f: impl FnOnce(&mut g3d::Client, u32) -> Result<()>,
    ) -> Result<()> {
        let mut c = match dial_namespace(&self.ns) {
            Ok(c) => c,
            Err(err) if is_not_running(&err) => bail!("session {} not found", quote(session)),
            Err(err) => return Err(err),
        };

        if !feature.is_empty() && !c.welcome.features.iter().any(|x| x == feature) {
            bail!(
                "termd daemon {} lacks feature {}; upgrade g3d-termd",
                c.welcome.daemon,
                quote(feature)
            );
        }

        #[derive(Deserialize)]
        struct SubReply {
            handle: u32,
        }

        let sub: SubReply = match request(
            &mut c,
            g3d_wire::Op::Subscribe(g3d_wire::Subscribe {
                id: session.to_owned(),
                mode: g3d_wire::Mode::None,
                role: g3d_wire::Role::Attach,
                history_rows: None,
                flow: None,
                cols: None,
                rows: None,
            }),
        ) {
            Ok(s) => s,
            Err(err) => {
                if is_wire_code(&err, g3d_wire::code::NO_TERMINAL) {
                    bail!("session {} not found", quote(session));
                }
                return Err(err);
            }
        };

        let op_res = f(&mut c, sub.handle);
        // Synchronize through the terminal actor (g3d's done_writing pattern):
        // unsubscribe's reply arrives after any error event caused by INPUT.
        let _ = request_raw(
            &mut c,
            g3d_wire::Op::Unsubscribe(g3d_wire::HandleOnly { handle: sub.handle }),
        );
        op_res?;

        for frame in c.take_backlog() {
            if frame.ty == g3d_wire::frame::ty::EVENT
                && frame.stream == sub.handle
                && let Ok(g3d_wire::Event::Error { code, message }) =
                    serde_json::from_slice(&frame.payload)
            {
                return Err(anyhow::Error::new(WireError { code, message }));
            }
        }
        Ok(())
    }

    /// Writes literal bytes to `session`'s PTY without key-name parsing or
    /// bracketed-paste framing, matching `g3d send`.
    pub fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        let res = self.with_writer(session, "", |c, handle| {
            for chunk in text.as_bytes().chunks(1 << 20) {
                c.input(handle, chunk)?;
            }
            Ok(())
        });
        res.map_err(|err| wrap_err("sending keys", err))
    }

    /// Pastes text into `session` via termd's `paste` op (streamed in chunks
    /// when `paste_stream` is available), wrapping in bracketed paste when the
    /// program requested mode 2004 and stripping embedded bracket-end sequences.
    pub fn paste_text(&self, session: &str, text: &str) -> Result<()> {
        let clean = text.replace(PASTE_END, "");
        if clean.is_empty() {
            if !self.session_exists(session)? {
                bail!("session {} not found", quote(session));
            }
            return Ok(());
        }

        let res = self.with_writer(session, "paste", |c, handle| {
            let max_frame = if c.welcome.max_frame > 0 {
                c.welcome.max_frame as usize
            } else {
                g3d_wire::frame::DEFAULT_MAX_FRAME
            };
            let chunk_max = (1usize << 20).min((max_frame.saturating_sub(1024) / 6).max(4));
            let has_stream = c.welcome.features.iter().any(|f| f == "paste_stream");

            if !has_stream || clean.len() <= chunk_max {
                request_raw(
                    c,
                    g3d_wire::Op::Paste(g3d_wire::Paste {
                        handle,
                        text: clean,
                        allow_unsafe: Some(true),
                        more: None,
                    }),
                )?;
                return Ok(());
            }

            let mut rest = clean.as_str();
            let mut first = true;
            while !rest.is_empty() {
                let mut cut = rest.floor_char_boundary(rest.len().min(chunk_max));
                if cut == 0 {
                    cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
                }
                let piece = &rest[..cut];
                rest = &rest[cut..];
                let more = !rest.is_empty();
                let allow_unsafe = first.then_some(true);
                first = false;
                request_raw(
                    c,
                    g3d_wire::Op::Paste(g3d_wire::Paste {
                        handle,
                        text: piece.to_owned(),
                        allow_unsafe,
                        more: more.then_some(true),
                    }),
                )?;
            }
            Ok(())
        });

        res.map_err(|err| wrap_err("pasting text", err))
    }

    /// Encodes and sends a special key or chord via termd's `"key"` op
    /// (honoring Kitty keyboard flags, `modifyOtherKeys`, and `DECCKM`),
    /// falling back to literal `INPUT` bytes for unrecognized multi-character
    /// strings.
    pub fn send_special_key(&self, session: &str, key: &str) -> Result<()> {
        let Some(ev) = parse_special_key(key) else {
            return self.send_keys(session, key);
        };
        let res = self.with_writer(session, "key", |c, handle| {
            let wire_key = ev.into_wire_key(handle);
            request_raw(c, g3d_wire::Op::Key(wire_key))?;
            Ok(())
        });
        res.map_err(|err| wrap_err(format!("sending key {}", quote(key)), err))
    }

    /// Returns `session`'s visible screen and up to `lines` rows of scrollback
    /// as plain text.
    pub fn capture(&self, session: &str, lines: i32) -> Result<String> {
        self.capture_fmt(session, lines, g3d_wire::CaptureFormat::Plain)
    }

    /// Returns `session`'s visible screen and up to `lines` rows of scrollback
    /// with SGR styling escapes preserved and CRLF normalized to LF.
    pub fn capture_styled(&self, session: &str, lines: i32) -> Result<String> {
        self.capture_fmt(session, lines, g3d_wire::CaptureFormat::Vt)
    }

    fn capture_fmt(
        &self,
        session: &str,
        lines: i32,
        format: g3d_wire::CaptureFormat,
    ) -> Result<String> {
        #[derive(Deserialize)]
        struct CaptureBody {
            #[serde(default)]
            text: String,
        }

        let res = with_read_conn(&self.ns, |c| {
            let reply: CaptureBody = request(
                c,
                g3d_wire::Op::Capture(g3d_wire::Capture {
                    id: session.to_owned(),
                    format: Some(format),
                    history_rows: Some(lines.max(0) as u32),
                    unwrap: None,
                }),
            )?;
            Ok(reply.text)
        });

        let mut text = match res {
            Ok(t) => t,
            Err(err) if is_not_running(&err) || is_wire_code(&err, g3d_wire::code::NO_TERMINAL) => {
                bail!("session {} not found", quote(session))
            }
            Err(err) => return Err(wrap_err("capturing pane", err)),
        };

        text = text.replace("\r\n", "\n");
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        Ok(text)
    }

    /// Maps each live terminal's root process PID to its terminal id.
    pub fn pane_pids(&self) -> Result<HashMap<i32, String>> {
        let st = self.status()?;
        let mut out = HashMap::with_capacity(st.terminals.len());
        for t in st.terminals {
            if t.pid > 0 {
                out.insert(t.pid, t.id);
            }
        }
        Ok(out)
    }

    /// Returns the id, root PID, and working directory of `session`.
    pub fn first_pane(&self, session: &str) -> Result<PaneInfo> {
        let st = self.status()?;
        for t in st.terminals {
            if t.id == session {
                return Ok(PaneInfo {
                    id: t.id,
                    pid: t.pid,
                    cwd: resolve_terminal_cwd(&t.pwd, &t.cwd),
                });
            }
        }
        bail!("session {} not found", quote(session))
    }

    /// Maps a `G3D_TERMD_ID` value to its terminal name if live.
    pub fn session_of_pane(&self, pane: &str) -> Result<String> {
        let pane = pane.trim();
        if pane.is_empty() {
            return Ok(String::new());
        }
        if self.session_exists(pane)? {
            Ok(pane.to_owned())
        } else {
            Ok(String::new())
        }
    }
}

impl Ops for Client {
    fn create_session(&self, name: &str, env: &HashMap<String, String>) -> Result<()> {
        Client::create_session(self, name, env)
    }

    fn create_session_in(
        &self,
        name: &str,
        dir: &str,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        Client::create_session_in(self, name, dir, env)
    }

    fn kill_session(&self, name: &str) -> Result<()> {
        Client::kill_session(self, name)
    }

    fn respawn_pane(&self, session: &str) -> Result<()> {
        Client::respawn_pane(self, session)
    }

    fn session_exists(&self, name: &str) -> Result<bool> {
        Client::session_exists(self, name)
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        Client::list_sessions(self)
    }

    fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        Client::send_keys(self, session, text)
    }

    fn paste_text(&self, session: &str, text: &str) -> Result<()> {
        Client::paste_text(self, session, text)
    }

    fn send_special_key(&self, session: &str, key: &str) -> Result<()> {
        Client::send_special_key(self, session, key)
    }

    fn capture(&self, session: &str, lines: i32) -> Result<String> {
        Client::capture(self, session, lines)
    }

    fn capture_styled(&self, session: &str, lines: i32) -> Result<String> {
        Client::capture_styled(self, session, lines)
    }

    fn pane_pids(&self) -> Result<HashMap<i32, String>> {
        Client::pane_pids(self)
    }

    fn namespace(&self) -> &str {
        Client::namespace(self)
    }

    fn session_names(&self) -> Result<Vec<String>> {
        Client::session_names(self)
    }

    fn session_env(&self, session: &str, name: &str) -> String {
        Client::session_env(self, session, name)
    }

    fn first_pane(&self, session: &str) -> Result<PaneInfo> {
        Client::first_pane(self, session)
    }

    fn session_of_pane(&self, pane: &str) -> Result<String> {
        Client::session_of_pane(self, pane)
    }

    fn attach_command(&self, session: &str) -> Result<(String, Vec<String>)> {
        Client::attach_command(self, session)
    }
}

#[derive(Deserialize)]
struct EnvEntry {
    name: String,
    value: String,
    #[serde(default)]
    source: String,
}

/// Extracts a local directory path from a terminal's OSC 7 `pwd` (which shells
/// like macOS zsh report as `"file://hostname/abs/path"`) and falls back to
/// `fallback_cwd` when `osc7_pwd` is absent or not an existing local directory.
pub fn resolve_terminal_cwd(osc7_pwd: &str, fallback_cwd: &str) -> String {
    let trimmed = osc7_pwd.trim();
    let cand = if let Some(rest) = trimmed.strip_prefix("file://") {
        // Strip authority ("hostname") up to the leading '/' of the path, and
        // strip any query or fragment before percent-decoding.
        match rest.find('/') {
            Some(slash) => {
                let raw_path = &rest[slash..];
                let raw_path = raw_path.split_once(['?', '#']).map_or(raw_path, |(p, _)| p);
                percent_decode_utf8(raw_path).unwrap_or_default()
            }
            None => String::new(),
        }
    } else {
        trimmed.to_owned()
    };

    if cand.starts_with('/') && std::fs::metadata(&cand).is_ok_and(|m| m.is_dir()) {
        return cand;
    }
    fallback_cwd.to_owned()
}

fn percent_decode_utf8(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
