//! `agent completion` and its bash, zsh, fish and powershell subcommands: cobra's
//! own hidden completion command, which the Go binary has without a file of its
//! own. Not ported: generating the scripts means porting cobra's, and the hidden
//! `__complete` command they call back, which this tree doesn't have.

use super::command::{ArgRule, Def, Doc};
use super::not_ported;
use super::root::Ctx;

// `agent completion`

pub const DOC: Doc = Doc {
    use_line: "completion",
    short: "Generate the autocompletion script for the specified shell",
    long: r#"Generate the autocompletion script for agent for the specified shell.
See each sub-command's help for details on how to use the generated script.
"#,
    args: ArgRule::None,
    hidden: true,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::topic(
        &DOC,
        vec![
            completion_bash_def(),
            completion_zsh_def(),
            completion_fish_def(),
            completion_powershell_def(),
        ],
    )
}

// `agent completion bash`

pub const COMPLETION_BASH: Doc = Doc {
    use_line: "bash",
    short: "Generate the autocompletion script for bash",
    long: r#"Generate the autocompletion script for the bash shell.

This script depends on the 'bash-completion' package.
If it is not installed already, you can install it via your OS's package manager.

To load completions in your current shell session:

	source <(agent completion bash)

To load completions for every new session, execute once:

#### Linux:

	agent completion bash > /etc/bash_completion.d/agent

#### macOS:

	agent completion bash > $(brew --prefix)/etc/bash_completion.d/agent

You will need to start a new shell for this setup to take effect.
"#,
    args: ArgRule::None,
    no_flags_in_use_line: true,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent completion bash`.
#[derive(clap::Args, Debug, Clone)]
pub struct CompletionBashArgs {
    #[arg(long = "no-descriptions", help = "disable completion descriptions")]
    pub no_descriptions: bool,
    /// The positional arguments, checked against COMPLETION_BASH.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn completion_bash_def() -> Def {
    leaf!(COMPLETION_BASH, CompletionBashArgs, run_completion_bash)
}

pub fn run_completion_bash(_ctx: &Ctx, _args: &CompletionBashArgs) -> anyhow::Result<()> {
    not_ported("completion bash")
}

// `agent completion zsh`

pub const COMPLETION_ZSH: Doc = Doc {
    use_line: "zsh",
    short: "Generate the autocompletion script for zsh",
    long: r#"Generate the autocompletion script for the zsh shell.

If shell completion is not already enabled in your environment you will need
to enable it.  You can execute the following once:

	echo "autoload -U compinit; compinit" >> ~/.zshrc

To load completions in your current shell session:

	source <(agent completion zsh)

To load completions for every new session, execute once:

#### Linux:

	agent completion zsh > "${fpath[1]}/_agent"

#### macOS:

	agent completion zsh > $(brew --prefix)/share/zsh/site-functions/_agent

You will need to start a new shell for this setup to take effect.
"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent completion zsh`.
#[derive(clap::Args, Debug, Clone)]
pub struct CompletionZshArgs {
    #[arg(long = "no-descriptions", help = "disable completion descriptions")]
    pub no_descriptions: bool,
    /// The positional arguments, checked against COMPLETION_ZSH.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn completion_zsh_def() -> Def {
    leaf!(COMPLETION_ZSH, CompletionZshArgs, run_completion_zsh)
}

pub fn run_completion_zsh(_ctx: &Ctx, _args: &CompletionZshArgs) -> anyhow::Result<()> {
    not_ported("completion zsh")
}

// `agent completion fish`

pub const COMPLETION_FISH: Doc = Doc {
    use_line: "fish",
    short: "Generate the autocompletion script for fish",
    long: r#"Generate the autocompletion script for the fish shell.

To load completions in your current shell session:

	agent completion fish | source

To load completions for every new session, execute once:

	agent completion fish > ~/.config/fish/completions/agent.fish

You will need to start a new shell for this setup to take effect.
"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent completion fish`.
#[derive(clap::Args, Debug, Clone)]
pub struct CompletionFishArgs {
    #[arg(long = "no-descriptions", help = "disable completion descriptions")]
    pub no_descriptions: bool,
    /// The positional arguments, checked against COMPLETION_FISH.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn completion_fish_def() -> Def {
    leaf!(COMPLETION_FISH, CompletionFishArgs, run_completion_fish)
}

pub fn run_completion_fish(_ctx: &Ctx, _args: &CompletionFishArgs) -> anyhow::Result<()> {
    not_ported("completion fish")
}

// `agent completion powershell`

pub const COMPLETION_POWERSHELL: Doc = Doc {
    use_line: "powershell",
    short: "Generate the autocompletion script for powershell",
    long: r#"Generate the autocompletion script for powershell.

To load completions in your current shell session:

	agent completion powershell | Out-String | Invoke-Expression

To load completions for every new session, add the output of the above command
to your powershell profile.
"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent completion powershell`.
#[derive(clap::Args, Debug, Clone)]
pub struct CompletionPowershellArgs {
    #[arg(long = "no-descriptions", help = "disable completion descriptions")]
    pub no_descriptions: bool,
    /// The positional arguments, checked against COMPLETION_POWERSHELL.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn completion_powershell_def() -> Def {
    leaf!(
        COMPLETION_POWERSHELL,
        CompletionPowershellArgs,
        run_completion_powershell
    )
}

pub fn run_completion_powershell(
    _ctx: &Ctx,
    _args: &CompletionPowershellArgs,
) -> anyhow::Result<()> {
    not_ported("completion powershell")
}
