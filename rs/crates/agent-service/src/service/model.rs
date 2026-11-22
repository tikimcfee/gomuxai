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

/// Says why effort can't be passed to a CLI's --effort, or Ok: it must be
/// one of the roster's efforts (agent_roster::EFFORTS), so a roster entry's
/// effort is always one spawn takes.
pub fn validate_effort(effort: &str) -> anyhow::Result<()> {
    if !agent_roster::EFFORTS.contains(&effort) {
        anyhow::bail!(
            "invalid effort {}: use {}",
            agent_base::gostd::strconv::quote(effort),
            agent_roster::EFFORTS.join(", ")
        );
    }
    Ok(())
}

/// Refuses a model that already names an effort (gemini-4-carbon-high)
/// given with --effort too, for a backend whose model ids carry the effort:
/// the two would name different variants, or the same one twice.
pub(crate) fn check_model_names_no_effort(
    backend_name: &str,
    model: &str,
    effort: &str,
) -> anyhow::Result<()> {
    if let Some(named) = agent_roster::EFFORTS
        .iter()
        .find(|e| model.ends_with(&format!("-{e}")))
    {
        let base = &model[..model.len() - named.len() - 1];
        anyhow::bail!(
            "model {model} already names effort {named}, and --effort says {effort}: for {backend_name}, give the model without it (--model {base} --effort {effort})"
        );
    }
    Ok(())
}

/// The error for spawn --effort on a backend that takes none.
pub(crate) fn effort_refusal(backend_name: &str) -> anyhow::Error {
    anyhow::anyhow!("backend {backend_name} takes no --effort: claude and agy do")
}

/// modelRefusal is the error for spawn --model on a backend that takes none.
pub(crate) fn model_refusal(backend_name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "backend {backend_name} takes no --model: claude, agy and aider do (for another, put the model in its launch command)"
    )
}
