//! The root command, `agent help`, the global flags, and running a command line
//! (Go: internal/cli/root.go).

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use agent_backend::Registry;
use agent_base::config::{self, Config};
use agent_base::gostd::filepath;
use agent_bus::{Conn, Publisher};
use agent_service::{AgentService, EventPublisher};
use agent_store::Store;
use agent_termd::Ops;
use anyhow::Context as _;
use clap::parser::ValueSource;
use clap::{ArgMatches, FromArgMatches as _};

use super::command::{ArgRule, Cmd, Def, Doc, Run};
use super::output::{flag_error, root_args, run_group, unknown_command, usage_error};
use super::{flags, help, history, orient, version};

pub const ROOT: Doc = Doc {
    use_line: "agent",
    short: "Coordinate AI agents running in g3d-termd sessions",
    long: r#"agent spawns AI agents (claude, aider, agy) in g3d-termd sessions and
coordinates them: messages, status, permission prompts, and shared state, on
this host or across hosts linked by the agent bus.

Run "agent" alone for a short orientation (who you are, your agents, the bus);
"agent help <command>" has each command's details and examples.

Results go to stdout; a hint about what to do next may follow on stderr
(-q drops it). Exit codes: 0 done, 1 failed; wait exits 2 when it times out,
and friction add exits 3 when it finds a similar entry instead of filing.

If agent gets in your way, "agent friction add" tells the people who fix it."#,
    example: r#"  agent spawn helper --backend claude
  agent send helper "summarize README.md" && agent wait helper --for idle --next
  agent state get helper _result"#,
    args: ArgRule::Custom(root_args),
    ..Doc::DEFAULT
};

/// The global flags (Go: the root's persistent flags), on every command.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Globals {
    #[arg(
        long = "backend",
        value_name = "string",
        default_value = "",
        help = "Backend for spawn or adopt: claude, aider, agy, or shell (spawn requires it or --backend-config; adopt auto-detects)"
    )]
    pub backend: String,
    #[arg(
        long = "backend-config",
        value_name = "string",
        default_value = "",
        help = "JSON file defining a custom backend (or app settings); pass it again to status/wait/watch for agents spawned with it"
    )]
    pub backend_config: String,
    #[arg(
        long = "quiet",
        short = 'q',
        help = "Don't print hints (results and errors still print)"
    )]
    pub quiet: bool,
    #[arg(
        long = "termd-ns",
        value_name = "string",
        default_value = "",
        help = "g3d-termd namespace (default: agents; never default or wall)"
    )]
    pub termd_ns: String,
}

/// The root command's own flags.
#[derive(clap::Args, Debug, Clone)]
pub struct RootArgs {
    // cobra's own --version flag would take -v, which status uses for verbose.
    #[arg(long = "version", help = "Print which build of agent this is")]
    pub version: bool,
    /// Words that aren't a command: refused by ROOT.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

// `agent help` replaces cobra's help command, which takes a word the root's Args
// accept for an unknown command and prints the root's help for it.
pub const HELP: Doc = Doc {
    use_line: "help [command]",
    short: "Help about any command",
    long: "Help prints a command's details and examples, or with no command, the list of commands.",
    example: r#"  agent help
  agent help wait
  agent help friction add"#,
    args: ArgRule::Any,
    skip_setup: true,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct HelpArgs {
    /// The command to explain, as its path of words.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn help_def() -> Def {
    leaf!(HELP, HelpArgs, run_help)
}

/// Prints a command's help, found by its words as the command line would find
/// it.
pub fn run_help(ctx: &Ctx, args: &HelpArgs) -> anyhow::Result<()> {
    let (cmd, rest) = find(tree(), &args.args);
    if let Some(word) = rest.first() {
        return Err(unknown_command(&cmd, word));
    }
    ctx.write_out(&help::render(&cmd))
}

/// The whole command tree: the root, its grouped commands in `agent help`'s
/// order, then help, then cobra's hidden completion command.
pub fn root_def() -> Def {
    // `agent` alone orients (orient.rs).
    let mut root = leaf!(ROOT, RootArgs, orient::run_orient);
    for group in super::command_groups() {
        root.groups.push((group.id, group.title));
        for mut def in group.defs {
            def.group_id = group.id;
            root.subs.push(def);
        }
    }
    root.subs.push(help_def());
    root.subs.push(super::completion::def());
    root
}

/// The tree, built once per process.
pub fn tree() -> &'static Def {
    static TREE: OnceLock<Def> = OnceLock::new();
    TREE.get_or_init(root_def)
}

/// The command at this path below `agent` ("task set"), for a run fn that
/// words an error with its usage (Go: the *cobra.Command a RunE gets).
pub fn command(path: &str) -> Cmd<'static> {
    Cmd::root(tree())
        .lookup(path)
        .unwrap_or_else(|| panic!("no command {path:?} in the tree"))
}

/// Runs the command line this process was started with, printing an error as
/// cobra does ("Error: ..." on stderr). The exit code for its error is
/// exit::exit_code(err).
pub fn execute() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    execute_with(
        &argv,
        Box::new(super::output::GoStdio::Stdout),
        Box::new(super::output::GoStdio::Stderr),
    )
}

/// Runs a command line (the words after "agent") with these stdout and stderr,
/// and records the run in the command log (Go: Execute).
pub fn execute_with(argv: &[String], out: Writer, err: Writer) -> anyhow::Result<()> {
    execute_ctx(&mut Ctx::new(out, err), argv)
}

/// execute_with on a Ctx the caller made: a test's, with its own config.
pub fn execute_ctx(ctx: &mut Ctx, argv: &[String]) -> anyhow::Result<()> {
    ctx.argv = Some(argv.to_vec());
    let result = run_line(ctx, tree(), argv);
    if let Err(e) = &result {
        let _ = writeln!(ctx.err(), "Error: {e:#}");
    }
    if !ctx.run_recorded.load(Ordering::Relaxed) {
        history::record_command(ctx, argv, ctx.start, result.as_ref().err());
    }
    ctx.close_bus_publisher();
    result
}

/// Runs a command line on a test's Ctx as the CLI would, but writes no
/// command-log row and keeps the bus publisher open: a test runs several
/// lines on one Ctx. Each starts with no hint printed, as a new process does.
#[cfg(test)]
pub(crate) fn run_words(ctx: &mut Ctx, argv: &[&str]) -> anyhow::Result<()> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    ctx.hinted.store(false, Ordering::Relaxed);
    run_line(ctx, tree(), &argv)
}

/// Finds the command for a line, parses its flags and arguments, and runs it, in
/// cobra's order: flags, --help, --version, the argument rule, setup, required
/// and exclusive flags, then the command.
fn run_line(ctx: &mut Ctx, root: &Def, argv: &[String]) -> anyhow::Result<()> {
    let (cmd, rest) = find(root, argv);
    ctx.path = cmd.path_below_root();
    let matches = parse_flags(&cmd, &rest)?;
    ctx.parsed(&cmd, matches.clone())?;
    let def = cmd.def();
    if matches.get_one::<bool>("help") == Some(&true) {
        return ctx.write_out(&help::render(&cmd));
    }
    if !cmd.has_parent() && matches.get_one::<bool>("version") == Some(&true) {
        return ctx.write_out(&format!("{}\n", version::version_line()));
    }
    if !def.runnable() {
        return ctx.write_out(&help::render(&cmd));
    }
    let args = positionals(&matches);
    check_args(&cmd, &matches, &args)?;
    if !def.doc.skip_setup {
        ctx.setup(&cmd)?;
    }
    check_flag_rules(&cmd, &matches)?;
    match def.run {
        Run::Leaf(run) => run(ctx, &matches),
        Run::Group => run_group(ctx, &cmd, &args),
        Run::Topic => ctx.write_out(&help::render(&cmd)),
    }
}

/// Parses a command's flags and arguments: the words of the line but the ones
/// that named the command.
pub fn parse_flags(cmd: &Cmd, rest: &[String]) -> anyhow::Result<ArgMatches> {
    let parser = cmd.def().clap_command();
    parser.try_get_matches_from(rest).map_err(|e| {
        flag_error(
            cmd,
            &flags::pflag_message(&e, &cmd.def().clap_command(), rest),
        )
    })
}

/// A command's positional arguments.
pub fn positionals(m: &ArgMatches) -> Vec<String> {
    m.get_many::<String>("args")
        .map(|v| v.cloned().collect())
        .unwrap_or_default()
}

/// Checks the positional arguments against the command's rule; a command's own
/// rule (not the root's) says its usage and first example when it refuses them.
pub fn check_args(cmd: &Cmd, m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
    let rule = cmd.def().doc.args;
    rule.check(cmd, m, args).map_err(|err| {
        if cmd.has_parent() && !matches!(rule, ArgRule::Any) {
            usage_error(cmd, err)
        } else {
            err
        }
    })
}

/// cobra's ValidateRequiredFlags and ValidateFlagGroups (mutually exclusive).
fn check_flag_rules(cmd: &Cmd, m: &ArgMatches) -> anyhow::Result<()> {
    let doc = cmd.def().doc;
    let changed = changed_flags(cmd, m);
    let missing: Vec<&str> = doc
        .required
        .iter()
        .copied()
        .filter(|f| !changed.contains(*f))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!("required flag(s) \"{}\" not set", missing.join("\", \""));
    }
    for group in doc.exclusive {
        let mut set: Vec<&str> = group
            .iter()
            .copied()
            .filter(|f| changed.contains(*f))
            .collect();
        if set.len() > 1 {
            set.sort();
            anyhow::bail!(
                "if any flags in the group [{}] are set none of the others can be; [{}] were all set",
                group.join(" "),
                set.join(" ")
            );
        }
    }
    Ok(())
}

/// The long names of the flags given on the command line.
fn changed_flags(cmd: &Cmd, m: &ArgMatches) -> HashSet<String> {
    let parser = cmd.def().clap_command();
    parser
        .get_arguments()
        .filter(|a| !a.is_positional())
        .filter(|a| m.value_source(a.get_id().as_str()) == Some(ValueSource::CommandLine))
        .filter_map(|a| a.get_long().map(str::to_string))
        .collect()
}

/// cobra's Find: follows the words of the line that aren't flags (nor the values
/// of flags that take one) down the tree, and returns the command they lead to
/// and the line without those words.
pub fn find<'a>(root: &'a Def, args: &[String]) -> (Cmd<'a>, Vec<String>) {
    let mut cmd = Cmd::root(root);
    let mut args = args.to_vec();
    loop {
        let words = strip_flags(&args, &cmd);
        let Some(next) = words.first() else { break };
        let Some(sub) = cmd.def().find_sub(next) else {
            break;
        };
        args = args_minus_first(&args, next, &cmd);
        cmd = cmd.child(sub);
    }
    (cmd, args)
}

/// Whether the flag a word names (by long name, or one short letter) is a bool
/// flag of cmd, which takes no separate value (pflag's NoOptDefVal). Not -h: cobra
/// only adds it to the command it found, so while finding it, -h and --help are
/// unknown flags that take the next word ("agent -h send" prints agent's help).
fn takes_no_value(cmd: &Cmd, word: &str) -> bool {
    let parser = cmd.def().clap_command();
    let mut flags = parser.get_arguments().filter(|a| a.get_id() != "help");
    let arg = if let Some(long) = word.strip_prefix("--") {
        flags.find(|a| a.get_long() == Some(long))
    } else {
        let short = word[1..].chars().next();
        flags.find(|a| a.get_short().is_some() && a.get_short() == short)
    };
    arg.is_some_and(flags::is_bool)
}

/// Whether a word is a flag that takes the next word as its value: a flag that
/// isn't cmd's bool flag (an unknown one too), without "=", either long or a
/// single short letter.
fn flag_with_value(cmd: &Cmd, s: &str) -> bool {
    if s.contains('=') {
        return false;
    }
    (s.starts_with("--") || (s.starts_with('-') && s.len() == 2)) && !takes_no_value(cmd, s)
}

/// cobra's stripFlags: the words of args that aren't flags or their values.
fn strip_flags(args: &[String], cmd: &Cmd) -> Vec<String> {
    let mut words = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        i += 1;
        if s == "--" {
            break;
        }
        if flag_with_value(cmd, s) {
            // "--flag value": skip the value, or stop if it is the last word.
            if args.len() - i <= 1 {
                break;
            }
            i += 1;
        } else if !s.is_empty() && !s.starts_with('-') {
            words.push(s.clone());
        }
    }
    words
}

/// cobra's argsMinusFirstX: args without the first word x that isn't a flag or a
/// flag's value.
fn args_minus_first(args: &[String], x: &str, cmd: &Cmd) -> Vec<String> {
    let mut pos = 0;
    while pos < args.len() {
        let s = &args[pos];
        if s == "--" {
            break;
        }
        if flag_with_value(cmd, s) {
            pos += 2;
            continue;
        }
        if !s.starts_with('-') && s == x {
            let mut out = args.to_vec();
            out.remove(pos);
            return out;
        }
        pos += 1;
    }
    args.to_vec()
}

/// Where a command writes: stdout or stderr, or a buffer in tests.
pub type Writer = Box<dyn Write + Send>;

/// A buffer to hand a command as its stdout or stderr, and read back after.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn writer(&self) -> Writer {
        Box::new(self.clone())
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a running command has to work with: the global flags, which flags were
/// given, its stdout and stderr, and what setup opens (Go: the package's shared
/// state: cfg, store, tmuxClient, backends, agentService, quiet, hinted). It is
/// built before the line is parsed and opens nothing. Setup builds the config
/// and opens the store, for every command but help and version, which must
/// work even when nothing else does (and --help, which runs no setup); the
/// termd client and the service are made the first time a command asks for
/// them ([`Ctx::termd`], [`Ctx::service`]).
///
/// Tests put in what Go's tests put in its package variables: a config for
/// setup to start from (`base_cfg`), or the config, store and termd client
/// themselves (`set_cfg`, `set_store`, `set_termd`) before calling a run fn.
pub struct Ctx {
    pub globals: Globals,
    /// The command below `agent`: "send", "state set"; "" for `agent` itself.
    pub path: String,
    /// When this run began (Go: runStart).
    pub start: SystemTime,
    changed: HashSet<String>,
    matches: Option<ArgMatches>,
    out: Mutex<Writer>,
    err: Mutex<Writer>,
    /// Whether a hint was printed: at most one per command.
    pub(crate) hinted: AtomicBool,
    /// Whether the run's command-log row is written: a run that execs another
    /// program (exec::exec_into) writes it first.
    pub(crate) run_recorded: AtomicBool,
    /// The config setup starts from instead of the default one (the data dir
    /// beside the binary): a test's.
    pub base_cfg: Option<Config>,
    /// Go: cfg. Set by setup.
    cfg: OnceLock<Config>,
    /// Go: store. Opened by store().
    store: OnceLock<Arc<Store>>,
    /// Go: tmuxClient. Opened by termd().
    termd: OnceLock<Arc<dyn Ops>>,
    /// Go: backends. Built by setup (setup_backends), or by backends() for a
    /// test that skipped setup.
    backends: OnceLock<Arc<Registry>>,
    /// Go: backendConfigErr: why --backend-config didn't load as a custom
    /// backend, for spawn's errors (the file may be an app config instead).
    backend_config_err: Option<String>,
    /// Go: agentService. Opened by service().
    service: OnceLock<Arc<AgentService>>,
    /// Go: busConnect's replacement in tests: a connector to a test bus. None
    /// connects to this host's bus (agent_bus::connect_local).
    bus_connector: Arc<Mutex<Option<BusConnector>>>,
    /// Go: busPublisher. Made by bus_publisher(), closed when the run ends.
    publisher: OnceLock<Arc<Publisher>>,
    /// Go: callerChain's replacement in tests.
    caller_chain: Mutex<Option<CallerChain>>,
    /// Go: cachedCaller*: the caller found, for this AGENT_NAME.
    cached_caller: Mutex<Option<(String, String)>>,
    /// Tests: exec_into records its execs here instead of making them, and
    /// finds every program named without a path at /usr/bin/<name> (Go:
    /// stubExec's execProcess and lookPath).
    pub exec_calls: Option<Arc<Mutex<Vec<super::exec::ExecCall>>>>,
    /// Tests: whether this terminal can be handed over, for attach over ssh
    /// (Go: attachTerminal); None asks stdin.
    pub attach_terminal: Option<bool>,
    /// The command line's words after "agent", as execute_ctx ran it, for the
    /// command-log row exec_into writes (Go: os.Args[1:]); None outside a run.
    pub(crate) argv: Option<Vec<String>>,
    /// What the command reads as its stdin (Go: os.Stdin): None reads the
    /// process's, except in unit tests, which read nothing (set_stdin).
    stdin: Mutex<Option<Box<dyn std::io::Read + Send>>>,
}

/// A Ctx's opened parts, to make Ctxs from on other threads (Ctx::share).
#[derive(Clone)]
pub struct CtxShare {
    globals: Globals,
    cfg: Config,
    store: Arc<Store>,
    termd: Arc<dyn Ops>,
    backends: Arc<Registry>,
    service: Arc<AgentService>,
    bus_connector: Arc<Mutex<Option<BusConnector>>>,
    publisher: Arc<Publisher>,
}

impl CtxShare {
    /// A Ctx over the shared parts, writing to out and err.
    pub fn ctx(&self, out: Writer, err: Writer) -> Ctx {
        let mut ctx = Ctx::new(out, err);
        ctx.globals = self.globals.clone();
        ctx.base_cfg = Some(self.cfg.clone());
        ctx.set_cfg(self.cfg.clone());
        ctx.set_store(Arc::clone(&self.store));
        ctx.set_termd(Arc::clone(&self.termd));
        ctx.set_backends(Arc::clone(&self.backends));
        ctx.set_service(Arc::clone(&self.service));
        ctx.bus_connector = Arc::clone(&self.bus_connector);
        let _ = ctx.publisher.set(Arc::clone(&self.publisher));
        // The command's run records itself; this one never does.
        ctx.run_recorded.store(true, Ordering::Relaxed);
        ctx
    }

    /// A Ctx whose stdout and stderr go nowhere.
    pub fn quiet_ctx(&self) -> Ctx {
        self.ctx(Box::new(std::io::sink()), Box::new(std::io::sink()))
    }
}

/// What opens a connection to the bus (Go: busConnect's type,
/// natsbus.LocalConnector), shareable with the publisher.
pub type BusConnector = Arc<dyn Fn() -> anyhow::Result<Conn> + Send + Sync>;

/// This process and its ancestors, nearest first (Go: callerChain's type).
pub type CallerChain = Box<dyn Fn() -> anyhow::Result<Vec<i32>> + Send + Sync>;

/// Reports publish failures on stderr, except for the common case of no bus
/// running. Hook mode allows stderr; stdout stays clean (Go: warnPublisher).
pub struct WarnPublisher(pub Arc<Publisher>);

impl EventPublisher for WarnPublisher {
    fn publish_status(&self, ev: &agent_bus::StatusEvent) -> anyhow::Result<()> {
        let res = self.0.publish_status(ev);
        if let Err(e) = &res
            && !agent_bus::is_no_bus(e)
        {
            // On stderr as Go's os.Stderr is: eprintln! would panic on a
            // closed pipe.
            let _ = writeln!(
                super::output::GoStdio::Stderr,
                "agent: publishing status event: {e:#}"
            );
        }
        res
    }
}

impl Ctx {
    pub fn new(out: Writer, err: Writer) -> Ctx {
        Ctx {
            globals: Globals::default(),
            path: String::new(),
            start: SystemTime::now(),
            changed: HashSet::new(),
            matches: None,
            out: Mutex::new(out),
            err: Mutex::new(err),
            hinted: AtomicBool::new(false),
            run_recorded: AtomicBool::new(false),
            base_cfg: None,
            cfg: OnceLock::new(),
            store: OnceLock::new(),
            termd: OnceLock::new(),
            backends: OnceLock::new(),
            backend_config_err: None,
            service: OnceLock::new(),
            bus_connector: Arc::new(Mutex::new(None)),
            publisher: OnceLock::new(),
            caller_chain: Mutex::new(None),
            cached_caller: Mutex::new(None),
            exec_calls: None,
            attach_terminal: None,
            argv: None,
            stdin: Mutex::new(None),
        }
    }

    /// Reads the command's stdin to its end (Go: io.ReadAll(os.Stdin)): the
    /// process's, or what set_stdin put in. A unit test that put in nothing
    /// reads nothing, so it never waits on the test runner's terminal.
    pub fn read_stdin(&self) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        match stdin.as_mut() {
            Some(r) => {
                r.read_to_end(&mut buf)?;
            }
            None if cfg!(test) => {}
            None => {
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
            }
        }
        Ok(buf)
    }

    /// Puts in what the command reads as its stdin (for tests: a payload).
    pub fn set_stdin(&self, r: impl std::io::Read + Send + 'static) {
        *self.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(r));
    }

    /// A Ctx writing to buffers, for tests: (ctx, stdout, stderr).
    pub fn for_test() -> (Ctx, SharedBuf, SharedBuf) {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        (Ctx::new(out.writer(), err.writer()), out, err)
    }

    /// Takes in what parsing the line found: the global flags, and which flags
    /// were given.
    fn parsed(&mut self, cmd: &Cmd, m: ArgMatches) -> anyhow::Result<()> {
        self.globals = Globals::from_arg_matches(&m)?;
        self.changed = changed_flags(cmd, &m);
        self.matches = Some(m);
        Ok(())
    }

    /// The value of the command's bool flag of this id, false if it has none
    /// (for the command log, which runs whatever the command).
    pub(crate) fn bool_flag(&self, id: &str) -> bool {
        let Some(m) = &self.matches else { return false };
        m.try_get_one::<bool>(id)
            .ok()
            .flatten()
            .copied()
            .unwrap_or(false)
    }

    /// Whether the flag of this long name was given on the command line (Go:
    /// cmd.Flags().Changed).
    pub fn changed(&self, long: &str) -> bool {
        self.changed.contains(long)
    }

    /// -q: don't print hints.
    pub fn quiet(&self) -> bool {
        self.globals.quiet
    }

    /// The command's stdout (Go: os.Stdout, cmd.OutOrStdout()).
    pub fn out(&self) -> MutexGuard<'_, Writer> {
        self.out.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The command's stderr, where hints and notices go (Go: hintOut).
    pub fn err(&self) -> MutexGuard<'_, Writer> {
        self.err.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn write_out(&self, s: &str) -> anyhow::Result<()> {
        self.out().write_all(s.as_bytes())?;
        Ok(())
    }

    /// What Go's setup (the root's PersistentPreRunE) does before every command
    /// but help and version: the config, from the flags, then the data
    /// directories and the store. Every other command opens them, even one that
    /// never reads them (statuses, hooks, one addressed to another host): a bad
    /// data dir fails every command alike, and a fresh install's first commands
    /// find the database there to record them in. The termd client and the
    /// service, which Go's setup makes here too, are made by the first command
    /// that asks (termd(), service()): making them connects to nothing and can't
    /// fail once the namespace is valid.
    pub fn setup(&mut self, cmd: &Cmd) -> anyhow::Result<()> {
        let mut cfg = match &self.base_cfg {
            Some(c) => c.clone(),
            None => config::default_config(),
        };
        if !self.globals.backend_config.is_empty() {
            cfg = config::load_from_file(&self.globals.backend_config).context("loading config")?;
        }
        if !self.globals.backend.is_empty() {
            cfg.backend = self.globals.backend.clone();
        }
        if !self.globals.termd_ns.is_empty() {
            cfg.termd_namespace = self.globals.termd_ns.clone();
        }
        cfg.validate_termd()?;
        self.setup_backends(&mut cfg);

        // Only spawn needs a backend, and not with --pick or --saved, which
        // let the roster choose it.
        if cmd.name() == "spawn" && !self.changed("pick") && !self.changed("saved") {
            cfg.validate()
                .map_err(|e| self.with_backend_config_err(e))?;
        }
        self.cfg = OnceLock::from(cfg);
        self.store()?;
        Ok(())
    }

    /// Creates the backend registry, with the custom backend --backend-config
    /// configures, if it does. The file may be an app config instead (or
    /// both), so one that isn't a backend config is no error here; a custom
    /// backend is the default for spawn when neither the app config nor
    /// --backend names one (Go: setupBackends).
    pub fn setup_backends(&mut self, cfg: &mut Config) {
        // A test's registry (set_backends) stays, as its store and termd do.
        let backends = self
            .backends
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(Registry::new()));
        self.backend_config_err = None;
        if !self.globals.backend_config.is_empty() {
            match backends.load_from_file(&self.globals.backend_config) {
                Err(e) => self.backend_config_err = Some(format!("{e:#}")),
                Ok(b) => {
                    if cfg.backend.is_empty() {
                        cfg.backend = b.name().to_string();
                    }
                }
            }
        }
        self.backends = OnceLock::from(backends);
    }

    /// Adds why --backend-config didn't load as a custom backend, if it
    /// didn't, to err, an error about spawn's backend (Go:
    /// withBackendConfigErr).
    pub fn with_backend_config_err(&self, err: anyhow::Error) -> anyhow::Error {
        match &self.backend_config_err {
            None => err,
            Some(why) => anyhow::anyhow!(
                "{err:#} ({} didn't load as a backend config: {why})",
                self.globals.backend_config
            ),
        }
    }

    /// The backend registry (Go: backends): setup's, or the built-in
    /// backends for a test that put in no registry.
    pub fn backends(&self) -> &Arc<Registry> {
        self.backends.get_or_init(|| Arc::new(Registry::new()))
    }

    /// Puts in a backend registry (for tests: one with fakes registered).
    pub fn set_backends(&self, backends: Arc<Registry>) {
        let _ = self.backends.set(backends);
    }

    /// The agent service (Go: agentService), made the first time a command
    /// asks, over store() and termd(), its status events published on the bus
    /// (Go: setup's NewAgentService and SetEventPublisher(warnPublisher)).
    pub fn service(&self) -> anyhow::Result<&Arc<AgentService>> {
        if let Some(s) = self.service.get() {
            return Ok(s);
        }
        let mut svc = AgentService::new(
            self.cfg().clone(),
            Arc::clone(self.store()?),
            Arc::clone(self.termd()?),
            Arc::clone(self.backends()),
        );
        svc.set_event_publisher(Some(Box::new(WarnPublisher(Arc::clone(
            self.bus_publisher(),
        )))));
        let _ = self.service.set(Arc::new(svc));
        Ok(self.service.get().expect("set above"))
    }

    /// Puts in an agent service (for tests: one with a recording deliverer
    /// starter, say).
    pub fn set_service(&self, svc: Arc<AgentService>) {
        let _ = self.service.set(svc);
    }

    /// Opens a client connection to this host's agent bus (Go: busConnect).
    /// It returns agent_bus::ErrNoBus (agent_bus::is_no_bus) when no bus is
    /// running, so callers can fall back to local-only behavior. It must fail
    /// fast in that case: hooks call the CLI on every agent event. Tests
    /// point it at a test bus with set_bus_connect.
    pub fn bus_connect(&self) -> anyhow::Result<Conn> {
        let custom = self
            .bus_connector
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match custom {
            Some(f) => f(),
            None => {
                let cfg = self.cfg();
                agent_bus::connect_local(cfg.bus_dir(), &cfg.host)
            }
        }
    }

    /// Replaces what bus_connect (and the publisher) connect with (Go: tests
    /// assigning busConnect).
    pub fn set_bus_connect(&self, f: impl Fn() -> anyhow::Result<Conn> + Send + Sync + 'static) {
        *self.bus_connector.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    /// The publisher of this process's status events (Go: busPublisher). It
    /// connects (through bus_connect) on the first recorded report, so
    /// commands that never report never try the bus, and the run flushes it
    /// before the process exits (close_bus_publisher).
    pub fn bus_publisher(&self) -> &Arc<Publisher> {
        self.publisher.get_or_init(|| {
            let custom = Arc::clone(&self.bus_connector);
            let (dir, host) = self
                .cfg
                .get()
                .map(|c| (c.bus_dir(), c.host.clone()))
                .unwrap_or_default();
            Arc::new(Publisher::new(move || {
                let f = custom.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match f {
                    Some(f) => f(),
                    None => agent_bus::connect_local(&dir, &host),
                }
            }))
        })
    }

    /// What a Ctx for work off this command's thread needs: the bus's RPC
    /// handler, which serves each request with a Ctx of its own (Go: the
    /// package's shared state, which every goroutine reads). It opens the
    /// store, the termd client and the service now.
    pub fn share(&self) -> anyhow::Result<CtxShare> {
        Ok(CtxShare {
            globals: self.globals.clone(),
            cfg: self.cfg().clone(),
            store: Arc::clone(self.store()?),
            termd: Arc::clone(self.termd()?),
            backends: Arc::clone(self.backends()),
            service: Arc::clone(self.service()?),
            bus_connector: Arc::clone(&self.bus_connector),
            publisher: Arc::clone(self.bus_publisher()),
        })
    }

    /// Flushes buffered events, bounded by agent_bus::DEFAULT_FLUSH_TIMEOUT,
    /// so they aren't lost when the process exits (Go: closeBusPublisher). A
    /// run that made no publisher has nothing to close.
    pub fn close_bus_publisher(&self) {
        if let Some(p) = self.publisher.get()
            && let Err(e) = p.close()
        {
            let _ = writeln!(self.err(), "agent: {e:#}");
        }
    }

    /// The config setup built (Go: cfg). Every command but help and version
    /// has one; asking for it in those is a bug.
    pub fn cfg(&self) -> &Config {
        self.cfg
            .get()
            .expect("Ctx::cfg: setup didn't run (help and version have no config)")
    }

    /// Puts in the config, as setup does (for tests, which call run fns
    /// directly).
    pub fn set_cfg(&self, cfg: Config) {
        let _ = self.cfg.set(cfg);
    }

    /// Replaces the config (tests: Go's `cfg.Backend = ...` after setupCLI).
    /// A service already made keeps the config it was made with.
    pub fn replace_cfg(&mut self, cfg: Config) {
        self.cfg = OnceLock::from(cfg);
    }

    /// The data dir: the directory the database is in (`.gomuxai` beside the
    /// binary, unless an app config moves it), where the bus dir and promoted
    /// scripts live too.
    pub fn data_dir(&self) -> String {
        filepath::dir(&self.cfg().database_path)
    }

    /// This host's store, opened (and its directories created) the first time
    /// a command asks (Go: setup's EnsureDirs and NewSQLiteStorage).
    pub fn store(&self) -> anyhow::Result<&Arc<Store>> {
        if let Some(s) = self.store.get() {
            return Ok(s);
        }
        let cfg = self.cfg();
        refuse_default_data_dir_in_tests(cfg)?;
        cfg.ensure_dirs().context("creating directories")?;
        let s = Store::open(&cfg.database_path).context("initializing storage")?;
        let _ = self.store.set(Arc::new(s));
        Ok(self.store.get().expect("set above"))
    }

    /// Puts in an open store (for tests).
    pub fn set_store(&self, store: Arc<Store>) {
        let _ = self.store.set(store);
    }

    /// The store, if a command opened it.
    pub(crate) fn opened_store(&self) -> Option<&Arc<Store>> {
        self.store.get()
    }

    /// The g3d-termd client for the configured namespace, made the first time a
    /// command asks (Go: setup's termd.NewClient). Making it connects to
    /// nothing: each call does.
    pub fn termd(&self) -> anyhow::Result<&Arc<dyn Ops>> {
        if let Some(t) = self.termd.get() {
            return Ok(t);
        }
        let cfg = self.cfg();
        let client = agent_termd::Client::new(agent_termd::Options {
            namespace: cfg.termd_namespace.clone(),
            daemon_bin: cfg.termd_binary.clone(),
            attach_bin: cfg.g3d_binary.clone(),
            ..Default::default()
        })
        .context("initializing termd")?;
        let _ = self.termd.set(Arc::new(client));
        Ok(self.termd.get().expect("set above"))
    }

    /// Puts in a termd client (for tests).
    pub fn set_termd(&self, termd: Arc<dyn Ops>) {
        let _ = self.termd.set(termd);
    }

    /// The config the command log records with: setup's, else (help, or a line
    /// that failed before setup) a test's base config, else the default one.
    /// None in unit tests that gave neither: they must not write beside the
    /// test binary.
    pub(crate) fn record_cfg(&self) -> Option<Config> {
        if let Some(c) = self.cfg.get() {
            return Some(c.clone());
        }
        if let Some(c) = &self.base_cfg {
            return Some(c.clone());
        }
        if cfg!(test) {
            return None;
        }
        Some(config::default_config())
    }

    /// The name of the agent running this command (Go: callerAgent): as the
    /// processes show it, the agent whose pane it runs under, or whose
    /// conversation the Claude Code background session it runs in carries on.
    /// AGENT_NAME is inherited by everything an agent starts, so it counts only
    /// where the processes say nothing.
    ///
    /// A background session can run under another agent's pane with that
    /// one's AGENT_NAME (friction #48), so AGENT_NAME counts only for a
    /// process outside this termd's agent panes (detached from its session, or
    /// under a pane another agent binary's store records). Cached per
    /// AGENT_NAME (Go: cachedCaller*; a Ctx has one store).
    pub fn caller_agent(&self) -> String {
        let env = getenv("AGENT_NAME");
        // Go: tmuxClient, store or agentService nil (help, or before setup).
        if self.cfg.get().is_none() {
            return env;
        }
        let mut cached = self.cached_caller.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((e, name)) = &*cached
            && *e == env
        {
            return name.clone();
        }
        let name = self.resolve_caller(&env);
        *cached = Some((env, name.clone()));
        name
    }

    /// Go: resolveCaller.
    fn resolve_caller(&self, env: &str) -> String {
        let Ok(svc) = self.service() else {
            return env.to_string();
        };
        if let Ok(chain) = self.caller_chain()
            && let Ok((name, true)) = svc.caller_agent(&chain)
        {
            return name;
        }
        env.to_string()
    }

    /// This process and its ancestors, nearest first (Go: callerChain); tests
    /// replace it (set_caller_chain) to stand a process somewhere in the tree.
    pub fn caller_chain(&self) -> anyhow::Result<Vec<i32>> {
        if let Some(f) = &*self.caller_chain.lock().unwrap_or_else(|e| e.into_inner()) {
            return f();
        }
        agent_base::util::proc_ancestors(std::process::id() as i32)
    }

    /// Replaces caller_chain (for tests), and forgets the cached caller.
    pub fn set_caller_chain(
        &self,
        f: impl Fn() -> anyhow::Result<Vec<i32>> + Send + Sync + 'static,
    ) {
        *self.caller_chain.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
        *self.cached_caller.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// caller_agent(), except for a person typing at a terminal (stdin is one,
    /// and AGENT_NAME isn't set), say in an adopted agent's pane: they aren't
    /// the agent (Go: nonInteractiveCallerAgent).
    pub fn non_interactive_caller_agent(&self) -> String {
        if getenv("AGENT_NAME").is_empty() && stdin_is_terminal() {
            return String::new();
        }
        self.caller_agent()
    }
}

/// An environment variable as Go's os.Getenv reads it: "" only if it is unset
/// or empty. A value that isn't UTF-8 is still set (its bad bytes made
/// U+FFFD), never "": an AGENT_NAME of stray bytes is still an agent's.
pub fn getenv(key: &str) -> String {
    std::env::var_os(key)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A real isatty check: /dev/null, which agents' commands often have on stdin,
/// is also a character device (Go: stdinIsTerminal, in send.go).
pub fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// In this crate's unit tests, refuses the data dir beside the test binary: a
/// test that reaches the store without its own config would otherwise create
/// one there.
fn refuse_default_data_dir_in_tests(cfg: &Config) -> anyhow::Result<()> {
    if cfg!(test) && cfg.database_path == config::default_config().database_path {
        anyhow::bail!(
            "a test opened the store beside the test binary ({}): give the Ctx a config (base_cfg or set_cfg)",
            cfg.database_path
        );
    }
    Ok(())
}
