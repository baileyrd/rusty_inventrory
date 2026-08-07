//! One private index for every AI coding conversation on your machine.
//!
//! `inventory-core` reads the local stores Claude Code, Codex, Cursor, Zed,
//! Kiro and Antigravity already write, merges them into a single encrypted
//! SQLite index, and searches that index by keyword and by meaning at once.
//!
//! It is a search tool, not a memory layer: nothing here injects context back
//! into any AI tool, and no conversation ever leaves the machine. The only
//! network call the crate is capable of making is an explicit update check.
//!
//! ```no_run
//! use inventory_core::{Inventory, SearchQuery};
//!
//! let mut inv = Inventory::open()?;
//! inv.index(false)?;
//! for hit in inv.search(&SearchQuery::new("container stuck"))?.hits {
//!     println!("{} — {}", hit.conversation.title, hit.matched_via.label());
//! }
//! # Ok::<(), inventory_core::Error>(())
//! ```

pub mod capture;
pub mod db;
pub mod embed;
pub mod error;
pub mod format;
pub mod handoff;
pub mod index;
pub mod keychain;
pub mod model;
pub mod paths;
pub mod repo;
pub mod search;
pub mod sources;
pub mod update;
pub mod vectors;
pub mod watch;

pub use error::{Error, Result};
pub use index::{
    FileHistory, FileHit, IndexReport, Inventory, RepoSummary, RetentionOption, SourceReport, Stats,
};
pub use model::{
    Clip, Conversation, Message, Note, ParsedConversation, Retention, Role, SourceId, SourceState,
    SourceStatus,
};
pub use repo::{Origin, RepoRef};
pub use search::{MatchedVia, SearchHit, SearchQuery, SearchResponse};
pub use watch::{WatchTick, Watcher};

/// The version reported in the command palette.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
