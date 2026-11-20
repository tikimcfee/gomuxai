//! The bottom of the Rust `agent`: the Go packages `util`, `await`, `buildinfo`,
//! `backend/types`, `config` and `macos`, one module each, file for file
//! (docs/RUST-PORT.md has the map).
//!
//! `gostd` is not a Go package of ours: it holds the few pieces of Go's
//! standard library whose exact behaviour or words the port must keep
//! (`path/filepath`'s cleaning, `strconv.Quote`, `time.Duration`'s text,
//! `os.PathError`'s messages), so every crate prints what the Go prints.

pub mod await_;
pub mod buildinfo;
pub mod config;
pub mod gostd;
pub mod macos;
pub mod types;
pub mod util;
