//! Dreaming: a scheduled pass over finished work that consolidates what was
//! learned and curates the friction box (docs/DREAMING.md).

pub mod gather;
pub mod memory;
pub mod schedule;
pub mod scrub;

/// The fixed brief a dream agent gets, with `{digest}` standing for the
/// digest's path.
pub const BRIEF: &str = include_str!("brief.md");

/// The brief for a dream whose digest is at `digest_path`.
pub fn brief(digest_path: &str) -> String {
    BRIEF.replace("{digest}", digest_path)
}
