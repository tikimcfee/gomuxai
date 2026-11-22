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
