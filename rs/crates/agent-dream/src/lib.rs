//! Dreaming: a scheduled pass over finished work that consolidates what was
//! learned and curates the friction box (docs/DREAMING.md).

pub mod evidence;
pub mod gather;
pub mod ideas;
pub mod memory;
pub mod review;
pub mod schedule;
pub mod scrub;
pub mod skip;
pub mod stale;
pub mod start_here;

/// The fixed brief an act run's agent gets, with `{item}` standing for the
/// path of its item file (docs/DREAMING-ACT.md, "The general actor").
pub const BRIEF_ACT: &str = include_str!("brief_act.md");

/// The act brief for a run whose item file is at `item_path`.
pub fn brief_act(item_path: &str) -> String {
    BRIEF_ACT.replace("{item}", item_path)
}

/// The fixed brief a dream agent gets, with `{digest}` standing for the
/// digest's path.
pub const BRIEF: &str = include_str!("brief.md");

/// The brief for a dream whose digest is at `digest_path`.
pub fn brief(digest_path: &str) -> String {
    BRIEF.replace("{digest}", digest_path)
}

/// The fixed brief the global dream's agent gets, with `{digest}` standing
/// for the digest's path.
pub const BRIEF_GLOBAL: &str = include_str!("brief_global.md");

/// The brief for a global dream whose digest is at `digest_path`.
pub fn brief_global(digest_path: &str) -> String {
    BRIEF_GLOBAL.replace("{digest}", digest_path)
}

/// The brief the global dream's synthesizer (the idea engine's generator)
/// gets: brief_global's rules, but it proposes ideas instead of writing.
pub const BRIEF_SYNTH: &str = include_str!("brief_synth.md");

/// The synthesizer's brief for a global dream whose digest is at
/// `digest_path`.
pub fn brief_synth(digest_path: &str) -> String {
    BRIEF_SYNTH.replace("{digest}", digest_path)
}

/// The brief the global dream's critic gets: check each idea against its
/// cited primary sources.
pub const BRIEF_CRITIC: &str = include_str!("brief_critic.md");

/// The critic's brief for a global dream whose digest is at `digest_path`.
pub fn brief_critic(digest_path: &str) -> String {
    BRIEF_CRITIC.replace("{digest}", digest_path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn briefs_split_the_work() {
        let p = super::brief("/d/digest.md");
        let g = super::brief_global("/d/digest.md");
        assert!(p.contains("/d/digest.md") && g.contains("/d/digest.md"));
        assert!(
            !p.contains("## Friction triage"),
            "project dreams leave friction alone"
        );
        assert!(p.contains("global dream"), "{p}");
        for want in [
            "## Friction triage",
            "start_here.md",
            "under 50 lines",
            "dream:<id>",
            "A trip is machine-wide",
            "agent dream friction dup",
        ] {
            assert!(g.contains(want), "global brief lacks {want}");
        }
        let s = super::brief_synth("/d/digest.md");
        let c = super::brief_critic("/d/digest.md");
        assert!(s.contains("/d/digest.md") && c.contains("/d/digest.md"));
        for want in [
            "agent dream idea --from",
            "agent dream done",
            "## Friction triage",
            "under 50 lines",
            "three separate sessions",
        ] {
            assert!(s.contains(want), "synthesizer brief lacks {want}");
        }
        assert!(
            !s.contains("agent dream memory put") && !s.contains("agent dream commit"),
            "the synthesizer proposes; code writes"
        );
        for want in [
            "agent dream judge <id> approve",
            "--checked",
            "agent dream ideas",
            "agent dream done",
        ] {
            assert!(c.contains(want), "critic brief lacks {want}");
        }
    }
}
