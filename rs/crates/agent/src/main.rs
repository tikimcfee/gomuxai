// Command agent orchestrates AI agents in g3d-termd sessions (Go: cmd/agent/main.go).

fn main() {
    if let Err(err) = agent::cli::execute() {
        std::process::exit(agent::cli::exit::exit_code(&err));
    }
}
