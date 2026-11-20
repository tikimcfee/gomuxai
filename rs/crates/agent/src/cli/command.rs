//! The command tree's types: what a cobra.Command held for each command (its help
//! text, its argument rule, its flags and its run function), and the walk from
//! the root to a command. No Go file: this is the part of cobra the Go relied on.

use std::borrow::Cow;

use clap::{Arg, ArgAction, ArgMatches, Args as _};

use super::flags;
use super::root::{Ctx, Globals};

/// In a Doc's long text, stands for the path of an agent's terminal log in this
/// binary's data dir (see Doc::long_text).
pub const LOG_FILE: &str = "{log_file}";

/// A command's help text and rules: cobra.Command's Use, Aliases, Short, Long,
/// Example and Args, and the flag rules cobra checks before running it.
#[derive(Clone, Copy)]
pub struct Doc {
    /// cobra's Use: the command's name, then what its arguments are.
    pub use_line: &'static str,
    pub aliases: &'static [&'static str],
    pub short: &'static str,
    pub long: &'static str,
    /// Example lines, each indented two spaces, as `agent help` prints them.
    pub example: &'static str,
    /// What positional arguments it accepts (cobra's Args).
    pub args: ArgRule,
    pub hidden: bool,
    /// Flags that must be given (cobra's MarkFlagRequired).
    pub required: &'static [&'static str],
    /// Groups of flags of which at most one may be given
    /// (cobra's MarkFlagsMutuallyExclusive).
    pub exclusive: &'static [&'static [&'static str]],
    /// The command runs without the setup every other command gets (Go's root
    /// PersistentPreRunE): help, and version, which must work even when nothing
    /// else does. Neither opens the database.
    pub skip_setup: bool,
    /// The use line has no " [flags]" (cobra's DisableFlagsInUseLine).
    pub no_flags_in_use_line: bool,
}

impl Doc {
    pub const DEFAULT: Doc = Doc {
        use_line: "",
        aliases: &[],
        short: "",
        long: "",
        example: "",
        args: ArgRule::Any,
        hidden: false,
        required: &[],
        exclusive: &[],
        skip_setup: false,
        no_flags_in_use_line: false,
    };

    /// The long text as help prints it: with `{log_file}` (in logs's) replaced by
    /// where this binary keeps an agent's terminal log, which Go works out as it
    /// starts (config.DefaultConfig().LogDir).
    pub fn long_text(&self) -> Cow<'static, str> {
        if self.long.contains(LOG_FILE) {
            let dir = agent_base::config::default_config().log_dir;
            let file = agent_base::gostd::filepath::join(&[dir.as_str(), "<name>.log"]);
            return Cow::Owned(self.long.replace(LOG_FILE, &file));
        }
        Cow::Borrowed(self.long)
    }

    /// The command's name: the first word of its use line (cobra's Name).
    pub fn name(&self) -> &'static str {
        match self.use_line.find(' ') {
            Some(i) => &self.use_line[..i],
            None => self.use_line,
        }
    }
}

/// A check of a command's own flags and positional arguments, for an ArgRule that
/// depends on its flags (Go: a custom cobra.PositionalArgs).
pub type CustomArgs = fn(&Cmd, &ArgMatches, &[String]) -> anyhow::Result<()>;

/// What positional arguments a command accepts: cobra's Args validators, with
/// their error messages.
#[derive(Clone, Copy)]
pub enum ArgRule {
    /// No Args set: anything (cobra's legacyArgs, for a command with no
    /// subcommands, or a group, which reports an unknown subcommand itself).
    Any,
    /// cobra.NoArgs
    None,
    /// cobra.ExactArgs
    Exact(usize),
    /// cobra.MinimumNArgs
    Min(usize),
    /// cobra.MaximumNArgs
    Max(usize),
    /// cobra.RangeArgs
    Range(usize, usize),
    Custom(CustomArgs),
}

impl ArgRule {
    pub fn check(&self, cmd: &Cmd, m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
        let n = args.len();
        match *self {
            ArgRule::Any => Ok(()),
            ArgRule::None if n > 0 => Err(anyhow::anyhow!(
                "unknown command {} for {}",
                flags::go_quote(&args[0]),
                flags::go_quote(&cmd.command_path())
            )),
            ArgRule::None => Ok(()),
            ArgRule::Exact(want) if n != want => {
                anyhow::bail!("accepts {want} arg(s), received {n}")
            }
            ArgRule::Min(want) if n < want => {
                anyhow::bail!("requires at least {want} arg(s), only received {n}")
            }
            ArgRule::Max(want) if n > want => {
                anyhow::bail!("accepts at most {want} arg(s), received {n}")
            }
            ArgRule::Range(lo, hi) if n < lo || n > hi => {
                anyhow::bail!("accepts between {lo} and {hi} arg(s), received {n}")
            }
            ArgRule::Custom(f) => f(cmd, m, args),
            _ => Ok(()),
        }
    }
}

/// A leaf command's run function, on its parsed flags.
pub type RunFn = fn(&Ctx, &ArgMatches) -> anyhow::Result<()>;

/// What running a command does.
pub enum Run {
    /// A command that does something.
    Leaf(RunFn),
    /// A command that only groups subcommands: alone it prints its help, and an
    /// unknown subcommand is an error that suggests close ones (Go: runGroup).
    Group,
    /// Not runnable at all: cobra prints its help whatever it is given.
    Topic,
}

/// A section of `agent help`'s command list.
pub struct Group {
    pub id: &'static str,
    pub title: &'static str,
    pub defs: Vec<Def>,
}

/// One command of the tree.
pub struct Def {
    pub doc: &'static Doc,
    /// Adds the command's own flags and positional arguments to its clap parser:
    /// its Args struct's `augment_args`.
    pub flags: fn(clap::Command) -> clap::Command,
    pub run: Run,
    /// The `agent help` group it is listed in ("" for none: help).
    pub group_id: &'static str,
    /// The groups of its subcommands, by id and title (only the root has any).
    pub groups: Vec<(&'static str, &'static str)>,
    pub subs: Vec<Def>,
}

/// The positional arguments of a command that has no flags of its own: a group.
#[derive(clap::Args, Debug, Clone)]
pub struct GroupArgs {
    #[arg(hide = true)]
    pub args: Vec<String>,
}

impl Def {
    pub fn leaf(doc: &'static Doc, flags: fn(clap::Command) -> clap::Command, run: RunFn) -> Def {
        Def {
            doc,
            flags,
            run: Run::Leaf(run),
            group_id: "",
            groups: Vec::new(),
            subs: Vec::new(),
        }
    }

    /// A command that groups subcommands (Go: RunE runGroup).
    pub fn group(doc: &'static Doc, subs: Vec<Def>) -> Def {
        Def {
            doc,
            flags: GroupArgs::augment_args,
            run: Run::Group,
            group_id: "",
            groups: Vec::new(),
            subs,
        }
    }

    /// A command that isn't runnable, only a parent of others (cobra's completion).
    pub fn topic(doc: &'static Doc, subs: Vec<Def>) -> Def {
        Def {
            doc,
            flags: GroupArgs::augment_args,
            run: Run::Topic,
            group_id: "",
            groups: Vec::new(),
            subs,
        }
    }

    pub fn name(&self) -> &'static str {
        self.doc.name()
    }

    /// cobra's Runnable.
    pub fn runnable(&self) -> bool {
        !matches!(self.run, Run::Topic)
    }

    /// The subcommand called `word`, by name or alias (cobra's findNext).
    pub fn find_sub(&self, word: &str) -> Option<&Def> {
        self.subs
            .iter()
            .find(|s| s.name() == word || s.doc.aliases.contains(&word))
    }

    /// cobra's HasAvailableSubCommands: any subcommand that isn't hidden and is
    /// runnable or a parent of such (help, which cobra excludes, is never
    /// counted alone: the root has others).
    pub fn has_available_subs(&self) -> bool {
        self.subs.iter().any(|s| s.is_available())
    }

    /// cobra's IsAvailableCommand, but for the parent's help command, which the
    /// callers treat by name.
    pub fn is_available(&self) -> bool {
        !self.doc.hidden && (self.runnable() || self.has_available_subs())
    }

    /// This command's clap parser: its own flags, the global ones, and -h, with
    /// pflag's syntax. Each command is parsed on its own, once the words before
    /// its flags have led to it (see root::find), as cobra does.
    pub fn clap_command(&self) -> clap::Command {
        let name = self.name();
        let cmd = clap::Command::new(name)
            .no_binary_name(true)
            .disable_help_flag(true)
            .disable_version_flag(true)
            .disable_help_subcommand(true)
            .args_override_self(true);
        let cmd = Globals::augment_args((self.flags)(cmd));
        let cmd = cmd.arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::SetTrue)
                .help(format!("help for {name}")),
        );
        flags::go_syntax(cmd)
    }
}

/// Builds a leaf's [`Def`] from its Doc, its clap Args struct, and its run fn,
/// which gets the parsed struct: `leaf!(DOC, Args, run)`.
macro_rules! leaf {
    ($doc:expr, $args:ty, $run:path) => {
        $crate::cli::command::Def::leaf(&$doc, <$args as ::clap::Args>::augment_args, |ctx, m| {
            let args = <$args as ::clap::FromArgMatches>::from_arg_matches(m)?;
            $run(ctx, &args)
        })
    };
}

/// A command found in the tree, with the commands above it: what a
/// *cobra.Command could say about its place (CommandPath, Parent, Root).
#[derive(Clone)]
pub struct Cmd<'a> {
    chain: Vec<&'a Def>,
}

impl<'a> Cmd<'a> {
    pub fn root(root: &'a Def) -> Cmd<'a> {
        Cmd { chain: vec![root] }
    }

    pub fn child(&self, sub: &'a Def) -> Cmd<'a> {
        let mut chain = self.chain.clone();
        chain.push(sub);
        Cmd { chain }
    }

    pub fn def(&self) -> &'a Def {
        self.chain[self.chain.len() - 1]
    }

    pub fn name(&self) -> &'static str {
        self.def().name()
    }

    pub fn has_parent(&self) -> bool {
        self.chain.len() > 1
    }

    pub fn parent(&self) -> Option<Cmd<'a>> {
        self.has_parent().then(|| Cmd {
            chain: self.chain[..self.chain.len() - 1].to_vec(),
        })
    }

    pub fn root_def(&self) -> &'a Def {
        self.chain[0]
    }

    /// "agent state set" (cobra's CommandPath).
    pub fn command_path(&self) -> String {
        self.chain
            .iter()
            .map(|d| d.name())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The path below the root: "state set", or "" for the root.
    pub fn path_below_root(&self) -> String {
        self.chain[1..]
            .iter()
            .map(|d| d.name())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// cobra's UseLine: "agent send <name>[@host] <message> [flags]".
    pub fn use_line(&self) -> String {
        let mut line = match self.parent() {
            Some(p) => format!("{} {}", p.command_path(), self.def().doc.use_line),
            None => self.def().doc.use_line.to_string(),
        };
        // Every command has flags: -h at least.
        if !self.def().doc.no_flags_in_use_line && !line.contains("[flags]") {
            line.push_str(" [flags]");
        }
        line
    }

    /// Walks down to a subcommand by its path below this one ("state set").
    pub fn lookup(&self, path: &str) -> Option<Cmd<'a>> {
        let mut cmd = self.clone();
        for word in path.split_whitespace() {
            let sub = cmd.def().find_sub(word)?;
            cmd = cmd.child(sub);
        }
        Some(cmd)
    }
}
