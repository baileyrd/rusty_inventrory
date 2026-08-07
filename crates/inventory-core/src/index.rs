//! The index itself: the type everything else is built on.

use crate::db;
use crate::embed::{self, encode_vector, Embedder, HashingEmbedder, LsaEmbedder};
use crate::keychain::{self, KeyProvider};
use crate::model::*;
use crate::repo;
use crate::search::{self, SearchQuery, SearchResponse};
use crate::sources::{self, ScanContext};
use crate::vectors::{IvfIndex, VectorCache, VectorSet};
use crate::{Error, Result};
use rusqlite::Connection;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

const SETTING_RETENTION: &str = "retention";
const SETTING_SCRATCHPAD: &str = "scratchpad_enabled";
const SETTING_LAST_INDEX: &str = "last_index_at";

/// Retrain once the corpus has grown by half again, rather than on every pass:
/// training is the only expensive thing the indexer does.
const RETRAIN_GROWTH_FACTOR: f64 = 1.5;

pub struct Inventory {
    conn: Connection,
    path: PathBuf,
    /// The private, unencrypted working copy `conn` is actually open
    /// against. Lives only inside `_tempdir`.
    plain_path: PathBuf,
    /// Deletes the working copy when the last handle to it goes away.
    _tempdir: tempfile::TempDir,
    key: [u8; 32],
    /// `conn.total_changes()` as of the last successful seal, so
    /// `checkpoint` can skip resealing when nothing has changed.
    sealed_at: Cell<u64>,
    embedder: Box<dyn Embedder>,
    /// Vectors held in memory so a search does not re-read and re-decode every
    /// embedding blob on each keystroke. Dropped whenever embeddings change,
    /// and rebuilt on the next search.
    cache: RefCell<Option<VectorCache>>,
}

#[derive(Debug, Clone, Default)]
pub struct SourceReport {
    pub source: Option<SourceId>,
    pub state: Option<SourceState>,
    pub conversations_added: usize,
    pub conversations_updated: usize,
    pub messages_indexed: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub per_source: Vec<SourceReport>,
    pub retrained: bool,
    pub embeddings_written: usize,
    pub pruned: usize,
    /// Whether the corpus is now large enough to carry a clustered index.
    pub indexed_vectors: bool,
    pub elapsed_ms: u128,
}

impl IndexReport {
    pub fn total_added(&self) -> usize {
        self.per_source.iter().map(|s| s.conversations_added).sum()
    }
    pub fn total_updated(&self) -> usize {
        self.per_source
            .iter()
            .map(|s| s.conversations_updated)
            .sum()
    }
    pub fn frozen(&self) -> Vec<&SourceReport> {
        self.per_source
            .iter()
            .filter(|s| s.state == Some(SourceState::Frozen))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct RetentionOption {
    pub retention: Retention,
    pub conversations: i64,
    /// Estimated on-disk cost of choosing this window.
    pub bytes: i64,
    pub selected: bool,
}

/// A repository the index has conversations for.
#[derive(Debug, Clone)]
pub struct RepoSummary {
    pub key: String,
    pub name: String,
    /// Where it was last seen. May no longer exist.
    pub root: PathBuf,
    pub remote: Option<String>,
    pub conversations: i64,
    pub files: i64,
    pub last_activity: i64,
}

/// One conversation that touched a given file.
#[derive(Debug, Clone)]
pub struct FileHit {
    pub conversation: Conversation,
    /// How often the conversation named the file — a rough stand-in for how
    /// central it was to the work.
    pub mentions: i64,
}

/// What is known about a file's history in the index.
#[derive(Debug, Clone, Default)]
pub struct FileHistory {
    /// `None` when the path is not inside any repository.
    pub repo: Option<repo::RepoRef>,
    /// The path, relative to the repository root.
    pub path: String,
    pub hits: Vec<FileHit>,
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub conversations: i64,
    pub messages: i64,
    pub per_source: Vec<(SourceId, i64)>,
    pub index_bytes: u64,
    pub encrypted: bool,
    pub entropy_bits_per_byte: f64,
    pub embedding_model: String,
    pub semantic_available: bool,
    pub embedded_conversations: i64,
    pub retention: Retention,
    pub last_index_at: Option<i64>,
    pub notes: i64,
    pub clips: i64,
    pub scratchpad_enabled: bool,
}

impl Inventory {
    /// Open the index at its standard location with the machine key.
    pub fn open() -> Result<Self> {
        let provider = keychain::default_provider();
        Inventory::open_at(&crate::paths::index_path(), provider.as_ref())
    }

    pub fn open_at(path: &Path, key: &dyn KeyProvider) -> Result<Self> {
        let existed = path.exists();
        // An index left over from before encryption is converted first, and
        // only replaced once the converted copy has been proven.
        if existed && db::is_plaintext_sqlite(path)? {
            db::convert_plaintext_to_encrypted(path, key)?;
        }
        let opened = db::open(path, key)?;
        let embedder = load_embedder(&opened.conn);
        // For an index that already exists on disk, the connection we just
        // opened is a faithful decrypt of it, so there is nothing to reseal
        // yet: seed `sealed_at` with the live `total_changes()` so the
        // mandatory `checkpoint()` below is a no-op. Forcing a reseal here
        // unconditionally would make every read-only open (`inv stats`, `inv
        // doctor`) race a concurrently running writer (the desktop app) and
        // could silently clobber a write made between that writer's last
        // checkpoint and this open. A brand-new index has no on-disk file
        // yet, so `u64::MAX` — which can never equal a real change count —
        // forces exactly one seal, the one that creates it.
        let sealed_at = if existed {
            opened.conn.total_changes()
        } else {
            u64::MAX
        };
        let inv = Inventory {
            conn: opened.conn,
            path: path.to_path_buf(),
            plain_path: opened.plain_path,
            _tempdir: opened.tempdir,
            key: opened.key,
            sealed_at: Cell::new(sealed_at),
            embedder,
            cache: RefCell::new(None),
        };
        inv.checkpoint()?;
        Ok(inv)
    }

    /// Reseal the on-disk index if anything has changed since the last
    /// seal. Cheap and safe to call often — a no-op when nothing changed.
    /// The only way writes made through this `Inventory` become durable, so
    /// every long-running caller (the desktop app's background loop, `inv
    /// watch`) must call it on its own schedule rather than relying on
    /// `Drop`, which a killed process or `std::process::exit` never runs.
    pub fn checkpoint(&self) -> Result<bool> {
        let changes = self.conn.total_changes();
        if changes == self.sealed_at.get() {
            return Ok(false);
        }
        db::seal(&self.conn, &self.plain_path, &self.path, &self.key)?;
        self.sealed_at.set(changes);
        Ok(true)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn embedder(&self) -> &dyn Embedder {
        self.embedder.as_ref()
    }

    /// Run `f` against the resident vectors, loading them if needed.
    fn with_vectors<R>(&self, f: impl FnOnce(&VectorCache) -> R) -> Result<R> {
        if self.cache.borrow().is_none() {
            let set = VectorSet::load(&self.conn, self.embedder.name())?;
            // A persisted index built from different vectors is discarded
            // rather than trusted: stale clusters returning deleted
            // conversations look plausible, which is what makes them worse
            // than having no index at all.
            let index = self.load_ann_index()?.filter(|i| i.matches(&set));
            *self.cache.borrow_mut() = Some(VectorCache { set, index });
        }
        let borrowed = self.cache.borrow();
        Ok(f(borrowed.as_ref().expect("just populated")))
    }

    fn invalidate_vectors(&self) {
        *self.cache.borrow_mut() = None;
    }

    fn load_ann_index(&self) -> Result<Option<IvfIndex>> {
        let payload: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT payload FROM ann_index WHERE id = 1", [], |r| {
                r.get(0)
            })
            .ok();
        Ok(payload.as_deref().and_then(IvfIndex::deserialize))
    }

    /// Cluster the embeddings, if there are enough of them to be worth it.
    fn rebuild_ann_index(&self) -> Result<bool> {
        let set = VectorSet::load(&self.conn, self.embedder.name())?;
        let Some(index) = IvfIndex::build(&set) else {
            // Below the threshold the exact scan is already fast enough, so any
            // stored index is now just a way to be wrong.
            self.conn.execute("DELETE FROM ann_index", [])?;
            return Ok(false);
        };
        self.conn.execute(
            "INSERT INTO ann_index(id, model, vectors, built_at, payload)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET model=excluded.model, vectors=excluded.vectors,
                 built_at=excluded.built_at, payload=excluded.payload",
            rusqlite::params![
                self.embedder.name(),
                set.len() as i64,
                now_unix(),
                index.serialize()
            ],
        )?;
        Ok(true)
    }

    // --- indexing ----------------------------------------------------------

    /// Read every installed source and merge it into the index.
    ///
    /// A source that fails is frozen: its already-indexed conversations stay
    /// searchable, the failure is recorded against it, and it is retried on
    /// the next call. One broken parser never takes the others down with it,
    /// and never deletes history.
    pub fn index(&mut self, force_full: bool) -> Result<IndexReport> {
        let started = std::time::Instant::now();
        let mut report = IndexReport::default();
        let retention = self.retention()?;
        let since = retention.cutoff(now_unix());

        for source in sources::all() {
            let id = source.id();
            let mut entry = SourceReport {
                source: Some(id),
                ..Default::default()
            };

            if !source.is_installed() {
                self.set_source_state(id, SourceState::Absent, None)?;
                entry.state = Some(SourceState::Absent);
                report.per_source.push(entry);
                continue;
            }

            let mut ctx = ScanContext::new(since, force_full);
            ctx.preload_seen(self.load_seen_files(id)?);

            match source.scan(&mut ctx) {
                Ok(conversations) => {
                    for parsed in &conversations {
                        match self.upsert(parsed)? {
                            Upsert::Inserted => entry.conversations_added += 1,
                            Upsert::Updated => entry.conversations_updated += 1,
                        }
                        entry.messages_indexed += parsed.messages.len();
                    }
                    self.save_seen_files(ctx.take_touched())?;
                    self.set_source_state(id, SourceState::Ok, None)?;
                    entry.state = Some(SourceState::Ok);
                }
                Err(e) => {
                    // Deliberately do not persist the touched-file list, so
                    // the next pass re-reads and can repair itself.
                    let detail = e.to_string();
                    self.set_source_state(id, SourceState::Frozen, Some(&detail))?;
                    entry.state = Some(SourceState::Frozen);
                    entry.error = Some(detail);
                }
            }
            report.per_source.push(entry);
        }

        report.pruned = self.prune_to_retention(retention)?;
        let (retrained, written) = self.refresh_embeddings()?;
        report.retrained = retrained;
        report.embeddings_written = written;

        // Clustering is done here, never on a search path: a search must not
        // silently pay for an index build, which would turn one slow query
        // into a pathologically slow one exactly when someone is waiting.
        report.indexed_vectors = self.rebuild_ann_index()?;
        self.invalidate_vectors();

        db::set_setting(&self.conn, SETTING_LAST_INDEX, &now_unix().to_string())?;
        report.elapsed_ms = started.elapsed().as_millis();
        Ok(report)
    }

    fn upsert(&self, parsed: &ParsedConversation) -> Result<Upsert> {
        let c = &parsed.conversation;
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM conversations WHERE source = ?1 AND external_id = ?2",
                rusqlite::params![c.source.slug(), c.external_id],
                |r| r.get(0),
            )
            .ok();

        let id = match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE conversations SET title=?1, project_path=?2, git_branch=?3,
                        started_at=?4, updated_at=?5, message_count=?6 WHERE id=?7",
                    rusqlite::params![
                        c.title,
                        c.project_path,
                        c.git_branch,
                        c.started_at,
                        c.updated_at,
                        c.message_count,
                        id
                    ],
                )?;
                self.conn
                    .execute("DELETE FROM messages WHERE conversation_id = ?1", [id])?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO conversations
                       (source, external_id, title, project_path, git_branch,
                        started_at, updated_at, message_count)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![
                        c.source.slug(),
                        c.external_id,
                        c.title,
                        c.project_path,
                        c.git_branch,
                        c.started_at,
                        c.updated_at,
                        c.message_count
                    ],
                )?;
                self.conn.last_insert_rowid()
            }
        };

        {
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO messages(conversation_id, seq, role, text, created_at)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;
            for m in &parsed.messages {
                stmt.execute(rusqlite::params![
                    id,
                    m.seq,
                    m.role.as_str(),
                    m.text,
                    m.created_at
                ])?;
            }
        }

        let body = parsed.body();
        self.conn
            .execute("DELETE FROM conversations_fts WHERE rowid = ?1", [id])?;
        self.conn.execute(
            "INSERT INTO conversations_fts(rowid, title, body) VALUES (?1,?2,?3)",
            rusqlite::params![id, c.title, body],
        )?;

        self.write_embedding(id, &format!("{}\n{}", c.title, body))?;
        self.link_repo(id, c, &body)?;

        Ok(if existing.is_some() {
            Upsert::Updated
        } else {
            Upsert::Inserted
        })
    }

    /// Attach a conversation to the repository and files it touched.
    ///
    /// Never fatal: a conversation that cannot be placed is still fully
    /// searchable by text, and failing an index pass over a repo lookup would
    /// trade the feature that works for the one that is a best effort.
    fn link_repo(&self, id: i64, c: &Conversation, body: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM conversation_file WHERE conversation_id = ?1",
            [id],
        )?;
        self.conn.execute(
            "DELETE FROM conversation_repo WHERE conversation_id = ?1",
            [id],
        )?;

        let located = c
            .project_path
            .as_deref()
            .and_then(repo::resolve)
            .map(|r| (r, repo::Origin::Recorded))
            .or_else(|| repo::infer(body).map(|r| (r, repo::Origin::Inferred)));
        let Some((repo_ref, origin)) = located else {
            return Ok(());
        };

        let repo_id = self.upsert_repo(&repo_ref)?;
        self.conn.execute(
            "INSERT INTO conversation_repo(conversation_id, repo_id, origin) VALUES (?1,?2,?3)",
            rusqlite::params![id, repo_id, origin.as_str()],
        )?;

        // Only hand the extractor a root it can actually test paths against.
        let root = repo_ref.root.is_dir().then_some(repo_ref.root.as_path());
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO conversation_file(conversation_id, repo_id, path, mentions)
             VALUES (?1,?2,?3,?4)",
        )?;
        for (path, mentions) in repo::extract_paths(body, root) {
            stmt.execute(rusqlite::params![id, repo_id, path, mentions])?;
        }
        Ok(())
    }

    fn upsert_repo(&self, r: &repo::RepoRef) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO repos(key, root, remote, name) VALUES (?1,?2,?3,?4)
             ON CONFLICT(key) DO UPDATE SET
                root = excluded.root, remote = excluded.remote, name = excluded.name",
            rusqlite::params![r.key, r.root.to_string_lossy(), r.remote, r.name],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM repos WHERE key = ?1", [&r.key], |row| {
                row.get(0)
            })?)
    }

    fn write_embedding(&self, id: i64, text: &str) -> Result<()> {
        let vec = self.embedder.embed(text);
        self.conn.execute(
            "INSERT INTO embeddings(conversation_id, model, vec) VALUES (?1,?2,?3)
             ON CONFLICT(conversation_id) DO UPDATE SET model=excluded.model, vec=excluded.vec",
            rusqlite::params![id, self.embedder.name(), encode_vector(&vec)],
        )?;
        self.invalidate_vectors();
        Ok(())
    }

    /// Train (or retrain) the semantic model from the user's own corpus, then
    /// re-embed everything so every vector comes from the same model.
    fn refresh_embeddings(&mut self) -> Result<(bool, usize)> {
        let doc_count: i64 =
            self.conn
                .query_row("SELECT count(*) FROM conversations", [], |r| r.get(0))?;
        if (doc_count as usize) < embed::MIN_DOCS_TO_TRAIN {
            return Ok((false, 0));
        }

        let trained_on: Option<i64> = self
            .conn
            .query_row(
                "SELECT doc_count FROM embedding_model WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .ok();
        let needs_training = match trained_on {
            None => true,
            Some(prev) => (doc_count as f64) > (prev as f64) * RETRAIN_GROWTH_FACTOR,
        };
        if !needs_training {
            // Still fill in anything missing a vector for the current model.
            return Ok((false, self.backfill_missing_embeddings()?));
        }

        let bodies = self.all_bodies()?;
        let Some(model) = LsaEmbedder::train(&bodies, embed::DEFAULT_DIM) else {
            return Ok((false, self.backfill_missing_embeddings()?));
        };

        self.conn.execute(
            "INSERT INTO embedding_model(id, kind, dim, trained_at, doc_count, payload)
             VALUES (1,?1,?2,?3,?4,?5)
             ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, dim=excluded.dim,
                 trained_at=excluded.trained_at, doc_count=excluded.doc_count,
                 payload=excluded.payload",
            rusqlite::params![
                model.name(),
                model.dim() as i64,
                now_unix(),
                doc_count,
                model.serialize()
            ],
        )?;

        self.embedder = Box::new(model);
        // The vector space changed, so every stored vector — and any index
        // built over them — is now meaningless.
        self.conn.execute("DELETE FROM embeddings", [])?;
        self.conn.execute("DELETE FROM ann_index", [])?;
        self.invalidate_vectors();
        Ok((true, self.backfill_missing_embeddings()?))
    }

    fn backfill_missing_embeddings(&self) -> Result<usize> {
        let pending: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.id, c.title || char(10) || coalesce(group_concat(m.text, char(10)), '')
                 FROM conversations c
                 LEFT JOIN messages m ON m.conversation_id = c.id
                 LEFT JOIN embeddings e
                        ON e.conversation_id = c.id AND e.model = ?1
                 WHERE e.conversation_id IS NULL
                 GROUP BY c.id",
            )?;
            let rows = stmt.query_map([self.embedder.name()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.flatten().collect()
        };

        let count = pending.len();
        for (id, text) in pending {
            self.write_embedding(id, &text)?;
        }
        Ok(count)
    }

    fn all_bodies(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.title || char(10) || coalesce(group_concat(m.text, char(10)), '')
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             GROUP BY c.id",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().collect())
    }

    // --- source status -----------------------------------------------------

    fn set_source_state(
        &self,
        id: SourceId,
        state: SourceState,
        error: Option<&str>,
    ) -> Result<()> {
        let now = now_unix();
        match state {
            SourceState::Ok => {
                // Success clears a freeze: "a source that breaks repairs
                // itself once it can be read again".
                self.conn.execute(
                    "INSERT INTO source_status(source, state, last_ok_at, last_error, frozen_at)
                     VALUES (?1,'ok',?2,NULL,NULL)
                     ON CONFLICT(source) DO UPDATE SET
                        state='ok', last_ok_at=?2, last_error=NULL, frozen_at=NULL",
                    rusqlite::params![id.slug(), now],
                )?;
            }
            SourceState::Frozen => {
                // last_ok_at is preserved: it is the "last successful read"
                // the UI shows when a source goes unreadable.
                self.conn.execute(
                    "INSERT INTO source_status(source, state, last_ok_at, last_error, frozen_at)
                     VALUES (?1,'frozen',NULL,?2,?3)
                     ON CONFLICT(source) DO UPDATE SET
                        state='frozen', last_error=?2,
                        frozen_at=coalesce(source_status.frozen_at, ?3)",
                    rusqlite::params![id.slug(), error, now],
                )?;
            }
            SourceState::Absent => {
                self.conn.execute(
                    "INSERT INTO source_status(source, state, last_ok_at, last_error, frozen_at)
                     VALUES (?1,'absent',NULL,NULL,NULL)
                     ON CONFLICT(source) DO UPDATE SET state='absent'",
                    rusqlite::params![id.slug()],
                )?;
            }
        }
        Ok(())
    }

    pub fn source_status(&self) -> Result<Vec<SourceStatus>> {
        let mut out = Vec::new();
        for id in SourceId::ALL {
            let row: Option<StatusRow> = self
                .conn
                .query_row(
                    "SELECT state, last_ok_at, last_error, frozen_at
                     FROM source_status WHERE source = ?1",
                    [id.slug()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .ok();

            let (state, last_ok_at, last_error, frozen_at) = match row {
                Some((s, ok, err, frozen)) => (
                    match s.as_str() {
                        "ok" => SourceState::Ok,
                        "frozen" => SourceState::Frozen,
                        _ => SourceState::Absent,
                    },
                    ok,
                    err,
                    frozen,
                ),
                // No status row: this source has never been through an index
                // pass, whether or not the tool is installed.
                None => (SourceState::Absent, None, None, None),
            };

            let (conversation_count, message_count): (i64, i64) = self.conn.query_row(
                "SELECT count(*), coalesce(sum(message_count),0)
                 FROM conversations WHERE source = ?1",
                [id.slug()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;

            out.push(SourceStatus {
                source: id,
                state,
                last_ok_at,
                last_error,
                frozen_at,
                conversation_count,
                message_count,
            });
        }
        Ok(out)
    }

    fn load_seen_files(&self, id: SourceId) -> Result<Vec<(SourceId, String, i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime, size FROM seen_files WHERE source = ?1")?;
        let rows = stmt.query_map([id.slug()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.flatten().map(|(p, m, s)| (id, p, m, s)).collect())
    }

    fn save_seen_files(&self, touched: Vec<(SourceId, String, i64, i64)>) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO seen_files(source, path, mtime, size, digest) VALUES (?1,?2,?3,?4,'')
             ON CONFLICT(source, path) DO UPDATE SET mtime=excluded.mtime, size=excluded.size",
        )?;
        for (src, path, mtime, size) in touched {
            stmt.execute(rusqlite::params![src.slug(), path, mtime, size])?;
        }
        Ok(())
    }

    // --- retention ---------------------------------------------------------

    pub fn retention(&self) -> Result<Retention> {
        Ok(db::get_setting(&self.conn, SETTING_RETENTION)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(Retention::All))
    }

    /// Change the window and immediately apply it. Narrowing deletes; the
    /// command palette shows the size of each choice first so the trade is
    /// visible before it is made.
    pub fn set_retention(&self, retention: Retention) -> Result<usize> {
        db::set_setting(&self.conn, SETTING_RETENTION, retention.slug())?;
        self.prune_to_retention(retention)
    }

    fn prune_to_retention(&self, retention: Retention) -> Result<usize> {
        let Some(cutoff) = retention.cutoff(now_unix()) else {
            return Ok(0);
        };
        let ids: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM conversations WHERE updated_at < ?1")?;
            let rows = stmt.query_map([cutoff], |r| r.get::<_, i64>(0))?;
            rows.flatten().collect()
        };
        for id in &ids {
            self.conn
                .execute("DELETE FROM conversations_fts WHERE rowid = ?1", [*id])?;
        }
        self.conn
            .execute("DELETE FROM conversations WHERE updated_at < ?1", [cutoff])?;
        if !ids.is_empty() {
            self.invalidate_vectors();
        }
        Ok(ids.len())
    }

    /// Every window with what it would cost on disk.
    pub fn retention_options(&self) -> Result<Vec<RetentionOption>> {
        let current = self.retention()?;
        let now = now_unix();
        let vector_bytes = (self.embedder.dim() * 4) as i64;
        let mut out = Vec::new();

        for retention in Retention::ALL {
            let cutoff = retention.cutoff(now).unwrap_or(0);
            let (conversations, text_bytes): (i64, i64) = self.conn.query_row(
                "SELECT count(DISTINCT c.id), coalesce(sum(length(m.text)),0)
                 FROM conversations c
                 LEFT JOIN messages m ON m.conversation_id = c.id
                 WHERE c.updated_at >= ?1",
                [cutoff],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            out.push(RetentionOption {
                retention,
                conversations,
                // Text is stored twice — once in `messages`, once in the FTS
                // index — plus one vector per conversation.
                bytes: text_bytes * 2 + conversations * vector_bytes,
                selected: retention == current,
            });
        }
        Ok(out)
    }

    // --- reading -----------------------------------------------------------

    /// Every repository the index has seen, most conversations first.
    pub fn repos(&self) -> Result<Vec<RepoSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.key, r.name, r.root, r.remote,
                    COUNT(DISTINCT cr.conversation_id),
                    (SELECT COUNT(DISTINCT cf.path)
                       FROM conversation_file cf WHERE cf.repo_id = r.id),
                    MAX(c.updated_at)
             FROM repos r
             JOIN conversation_repo cr ON cr.repo_id = r.id
             JOIN conversations c      ON c.id = cr.conversation_id
             GROUP BY r.id
             ORDER BY COUNT(DISTINCT cr.conversation_id) DESC, r.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RepoSummary {
                key: row.get(0)?,
                name: row.get(1)?,
                root: row.get::<_, String>(2)?.into(),
                remote: row.get(3)?,
                conversations: row.get(4)?,
                files: row.get(5)?,
                last_activity: row.get(6)?,
            })
        })?;
        Ok(rows.flatten().collect())
    }

    /// The conversations that touched a file, most recently active first.
    ///
    /// `path` is resolved the way a developer would type it: absolute, or
    /// relative to `from` (normally the current directory). The repository is
    /// discovered from the path itself, so this works from anywhere inside a
    /// checkout without naming the repo.
    pub fn history_for_path(&self, path: &Path, from: &Path, limit: usize) -> Result<FileHistory> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            from.join(path)
        };
        let Some(repo_ref) = repo::discover(&absolute) else {
            return Ok(FileHistory::default());
        };
        let Ok(relative) = absolute.strip_prefix(&repo_ref.root) else {
            return Ok(FileHistory::default());
        };
        let relative = relative.to_string_lossy().replace('\\', "/");

        let repo_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM repos WHERE key = ?1",
                [&repo_ref.key],
                |r| r.get(0),
            )
            .ok();
        let Some(repo_id) = repo_id else {
            return Ok(FileHistory {
                repo: Some(repo_ref),
                path: relative,
                hits: Vec::new(),
            });
        };

        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.source, c.external_id, c.title, c.project_path, c.git_branch,
                    c.started_at, c.updated_at, c.message_count, cf.mentions
             FROM conversation_file cf
             JOIN conversations c ON c.id = cf.conversation_id
             WHERE cf.repo_id = ?1 AND cf.path = ?2
             ORDER BY c.updated_at DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![repo_id, relative, limit as i64], |row| {
            let source: String = row.get(1)?;
            Ok(FileHit {
                conversation: Conversation {
                    id: row.get(0)?,
                    source: source.parse().unwrap_or(SourceId::ClaudeCode),
                    external_id: row.get(2)?,
                    title: row.get(3)?,
                    project_path: row.get(4)?,
                    git_branch: row.get(5)?,
                    started_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    message_count: row.get(8)?,
                },
                mentions: row.get(9)?,
            })
        })?;

        Ok(FileHistory {
            repo: Some(repo_ref),
            path: relative,
            hits: rows.flatten().collect(),
        })
    }

    pub fn search(&self, query: &SearchQuery) -> Result<SearchResponse> {
        self.with_vectors(|cache| search::search(&self.conn, cache, self.embedder.as_ref(), query))?
    }

    pub fn conversation(&self, id: i64) -> Result<(Conversation, Vec<Message>)> {
        let conversation = self
            .conn
            .query_row(
                "SELECT id, source, external_id, title, project_path, git_branch,
                        started_at, updated_at, message_count
                 FROM conversations WHERE id = ?1",
                [id],
                |row| {
                    let source: String = row.get(1)?;
                    Ok(Conversation {
                        id: row.get(0)?,
                        source: source.parse().unwrap_or(SourceId::ClaudeCode),
                        external_id: row.get(2)?,
                        title: row.get(3)?,
                        project_path: row.get(4)?,
                        git_branch: row.get(5)?,
                        started_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        message_count: row.get(8)?,
                    })
                },
            )
            .map_err(|_| Error::NoSuchConversation(id))?;

        let mut stmt = self.conn.prepare(
            "SELECT seq, role, text, created_at FROM messages
             WHERE conversation_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([id], |r| {
            let role: String = r.get(1)?;
            Ok(Message {
                seq: r.get(0)?,
                role: Role::parse(&role),
                text: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok((conversation, rows.flatten().collect()))
    }

    pub fn stats(&self) -> Result<Stats> {
        let conversations: i64 =
            self.conn
                .query_row("SELECT count(*) FROM conversations", [], |r| r.get(0))?;
        let messages: i64 = self
            .conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?;

        let mut per_source = Vec::new();
        for id in SourceId::ALL {
            let n: i64 = self.conn.query_row(
                "SELECT count(*) FROM conversations WHERE source = ?1",
                [id.slug()],
                |r| r.get(0),
            )?;
            per_source.push((id, n));
        }

        let embedded: i64 = self.conn.query_row(
            "SELECT count(*) FROM embeddings WHERE model = ?1",
            [self.embedder.name()],
            |r| r.get(0),
        )?;
        let notes: i64 = self
            .conn
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))?;
        let clips: i64 = self
            .conn
            .query_row("SELECT count(*) FROM clips", [], |r| r.get(0))?;

        Ok(Stats {
            conversations,
            messages,
            per_source,
            index_bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            encrypted: db::looks_encrypted(&self.path)?,
            entropy_bits_per_byte: db::shannon_entropy(&self.path)?,
            embedding_model: self.embedder.name().to_string(),
            semantic_available: self.embedder.is_semantic(),
            embedded_conversations: embedded,
            retention: self.retention()?,
            last_index_at: db::get_setting(&self.conn, SETTING_LAST_INDEX)?
                .and_then(|v| v.parse().ok()),
            notes,
            clips,
            scratchpad_enabled: self.scratchpad_enabled()?,
        })
    }

    pub fn scratchpad_enabled(&self) -> Result<bool> {
        // Off by default, deliberately.
        Ok(db::get_setting(&self.conn, SETTING_SCRATCHPAD)?.as_deref() == Some("on"))
    }

    pub fn set_scratchpad_enabled(&self, on: bool) -> Result<()> {
        db::set_setting(
            &self.conn,
            SETTING_SCRATCHPAD,
            if on { "on" } else { "off" },
        )
    }
}

/// Best-effort final seal. This is a safety net, not the durability
/// mechanism — see `checkpoint`'s doc comment. It only helps on the normal
/// return path (every CLI command except `inv watch`); it never runs after
/// `std::process::exit`, a killed process, or `app.exit()` skipping Rust's
/// unwind.
impl Drop for Inventory {
    fn drop(&mut self) {
        if let Err(e) = self.checkpoint() {
            tracing::error!("failed to seal the index on exit: {e}");
        }
    }
}

enum Upsert {
    Inserted,
    Updated,
}

/// `(state, last_ok_at, last_error, frozen_at)` as stored.
type StatusRow = (String, Option<i64>, Option<String>, Option<i64>);

fn load_embedder(conn: &Connection) -> Box<dyn Embedder> {
    let stored: Option<Vec<u8>> = conn
        .query_row(
            "SELECT payload FROM embedding_model WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .ok();
    match stored.as_deref().and_then(LsaEmbedder::deserialize) {
        Some(model) => Box::new(model),
        None => Box::new(HashingEmbedder::new(embed::DEFAULT_DIM)),
    }
}
