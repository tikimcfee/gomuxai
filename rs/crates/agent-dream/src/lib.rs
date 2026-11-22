//! Dreaming: a scheduled pass over finished work that consolidates what was
//! learned and curates the friction box (docs/DREAMING.md).

pub mod evidence;
pub mod gather;
pub mod memory;
pub mod review;
pub mod schedule;
pub mod scrub;
pub mod stale;
pub mod start_here;

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
            "two or more projects",
            "agent dream friction dup",
        ] {
            assert!(g.contains(want), "global brief lacks {want}");
        }
    }
}
