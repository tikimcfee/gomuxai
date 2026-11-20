//! Go `internal/service/model.go`.

use std::sync::LazyLock;

use regex::Regex;

/// modelPattern is what spawn --model takes: a model name or alias as CLIs
/// spell them ("opus", "claude-fable-5-1", "claude-opus-5-5[1m]",
/// "gemini-3.8-flash-high", "openrouter/x-ai/grok"). It is shell-quoted on
/// the launch line anyway; the pattern keeps a name from another host to
/// something that reads as a model.
static MODEL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:/@\[\]+-]{0,127}$").unwrap());

/// Says why model can't be passed to a CLI's --model, or Ok.
pub fn validate_model(model: &str) -> anyhow::Result<()> {
    if !MODEL_PATTERN.is_match(model) {
        anyhow::bail!(
            "invalid model {}: a model name is letters, digits and . _ : / @ [ ] + -, starting with a letter or digit",
            agent_base::gostd::strconv::quote(model)
        );
    }
    Ok(())
}

/// modelRefusal is the error for spawn --model on a backend that takes none.
pub(crate) fn model_refusal(backend_name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "backend {backend_name} takes no --model: claude, agy and aider do (for another, put the model in its launch command)"
    )
}
