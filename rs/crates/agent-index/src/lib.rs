//! The index (docs/INDEX.md): one queryable map of a dev tree. Every project,
//! directory, file, symbol and doc section gets a stable id, its place in the
//! tree, typed edges to other entities, and a summary tied to the content key
//! it describes. This crate is the model, the crawl, the query side and its
//! ranking, and output fitting; the CLI's `agent index` commands parse flags
//! and print.
//!
//! Version 1 (docs/INDEX.md, "Version 1"): code writes everything, with no
//! model. Summaries are `seed`s from the code itself; the embedder is another
//! lane's, which drains `embed_queue` into `vectors`.

pub mod config;
pub mod crawl;
pub mod db;
pub mod edges;
#[cfg(feature = "embed")]
pub mod embed;
pub mod extract;
pub mod files;
pub mod git;
pub mod labels;
pub mod manifest;
pub mod model;
pub mod names;
pub mod pg;
pub mod query;
pub mod roots;
pub mod view;
pub mod walk;

#[cfg(test)]
mod followup_test;
#[cfg(test)]
mod index_test;
#[cfg(test)]
mod review_test;

pub use crawl::{CrawlOptions, CrawlReport, ProjectReport, crawl};
pub use db::{Index, SCHEMA_VERSION, path_beside};
pub use model::{Entity, Summary};
pub use query::{NoSemantic, SearchOptions, SearchResult, Semantic, search};
