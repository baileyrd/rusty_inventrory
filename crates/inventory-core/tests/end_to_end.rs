//! End-to-end: build a fixture machine with all six tools installed, index it,
//! and exercise the capabilities the product is sold on.

use inventory_core::keychain::StaticKey;
use inventory_core::model::SourceState;
use inventory_core::{Inventory, SearchQuery, SourceId};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// `paths` is configured by environment variable, which is process-global, so
/// tests that install a fixture home must not overlap.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _guard: MutexGuard<'static, ()>,
    /// Held only to keep the directory alive for the test's lifetime — the
    /// fixture reaches it through the path tables, not through this handle.
    _home: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(inventory_core::paths::HOME_ENV, home.path());
        std::env::set_var(inventory_core::paths::DATA_ENV, data.path());
        Fixture {
            _guard: guard,
            _home: home,
            data,
        }
    }

    fn index_path(&self) -> PathBuf {
        self.data.path().join("inventory.sqlite3")
    }

    fn open(&self) -> Inventory {
        Inventory::open_at(&self.index_path(), &StaticKey::new("f".repeat(64))).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var(inventory_core::paths::HOME_ENV);
        std::env::remove_var(inventory_core::paths::DATA_ENV);
    }
}

/// Where this platform actually expects a source to live.
///
/// The fixture used to hardcode Linux paths, which meant it silently tested
/// nothing on macOS and Windows — three of the six sources were simply absent.
/// Deriving the root from the real path table instead makes this suite a test
/// *of* the path tables on whichever platform it runs.
fn root_for(source: SourceId) -> PathBuf {
    inventory_core::paths::candidates(source)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{source} has no candidate root on this platform"))
}

/// The `state.vscdb` a VS Code fork's global storage lives in.
fn fork_store(source: SourceId) -> PathBuf {
    root_for(source).join("globalStorage").join("state.vscdb")
}

fn write(path: PathBuf, body: &str) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    path
}

fn vscdb(path: PathBuf, rows: &[(&str, String)]) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    for (k, v) in rows {
        conn.execute(
            "INSERT INTO ItemTable VALUES (?1,?2)",
            rusqlite::params![k, v],
        )
        .unwrap();
    }
    path
}

/// Fixture timestamps are relative to now so the suite does not depend on
/// what the wall clock happens to say.
fn recent() -> i64 {
    inventory_core::model::now_unix() - 3600
}

/// A machine with all six tools installed and one conversation each.
fn install_all_six() {
    let t = recent();
    let t_ms = t * 1000;
    // Claude Code — JSONL transcript.
    write(
        root_for(SourceId::ClaudeCode).join("-work-api/sess-1.jsonl"),
        &format!(
            r#"{{"type":"user","sessionId":"sess-1","cwd":"/work/api","gitBranch":"main","timestamp":{t},"message":{{"role":"user","content":"Monochromatic design with SVG icons"}}}}
{{"type":"assistant","timestamp":{t},"message":{{"role":"assistant","content":[{{"type":"text","text":"Use currentColor so the icons inherit."}}]}}}}"#
        ),
    );

    // Codex — rollout JSONL.
    write(
        root_for(SourceId::Codex).join("2026/08/05/rollout-1.jsonl"),
        &format!(
            r#"{{"id":"codex-1","timestamp":{t},"cwd":"/work/db"}}
{{"type":"message","role":"user","timestamp":{t},"content":[{{"type":"input_text","text":"Postgres index tuning for the search table"}}]}}
{{"type":"message","role":"assistant","timestamp":{t},"content":[{{"type":"output_text","text":"Add a partial index on updated_at."}}]}}"#
        ),
    );

    // Cursor / Kiro / Antigravity — VS Code fork stores.
    vscdb(
        fork_store(SourceId::Cursor),
        &[(
            "workbench.panel.aichat.view.aichat.chatdata",
            serde_json::json!({"tabs":[{"tabId":"cursor-1","chatTitle":"Git remote setup and waitlist",
                "lastUpdatedAt": t_ms,
                "bubbles":[{"type":1,"text":"how do I add a git remote"},
                           {"type":2,"text":"git remote add origin <url>"}]}]})
            .to_string(),
        )],
    );
    vscdb(
        fork_store(SourceId::Kiro),
        &[(
            "chat.sessions",
            serde_json::json!({"sessionId":"kiro-1","lastUpdatedAt": t_ms,
                "requests":[{"message":{"text":"Node-RED instance 404 error"},
                             "response":[{"value":"Check the base path setting."}]}]})
            .to_string(),
        )],
    );
    vscdb(
        fork_store(SourceId::Antigravity),
        &[(
            "agent.store",
            serde_json::json!({"id":"anti-1","lastUpdatedAt": t_ms,
                "messages":[{"role":"user","content":"Antigravity truncates tool arguments at 256 bytes"},
                            {"role":"assistant","content":"That is a known limit."}]})
            .to_string(),
        )],
    );

    // Zed — its own thread database.
    let zed = root_for(SourceId::Zed).join("threads.db");
    std::fs::create_dir_all(zed.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&zed).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, summary TEXT, updated_at TEXT, data BLOB)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads VALUES (?1,?2,?3,?4)",
        rusqlite::params![
            "zed-1",
            "Refactor the auth middleware into a shared hook",
            t,
            serde_json::json!({
                "project_path": "/work/web",
                "git_branch": "feat/auth",
                "messages":[
                    {"role":"user","segments":[{"type":"text","text":"refactor the auth middleware into a shared hook"}]},
                    {"role":"assistant","segments":[{"type":"text","text":"Extracted useAuth."}]}
                ]})
            .to_string()
        ],
    )
    .unwrap();
}

#[test]
fn indexes_all_six_sources_and_searches_across_them() {
    let fx = Fixture::new();
    install_all_six();

    let mut inv = fx.open();
    let report = inv.index(false).unwrap();

    // Every source read cleanly and contributed exactly one conversation.
    for entry in &report.per_source {
        assert_eq!(
            entry.state,
            Some(SourceState::Ok),
            "{:?} did not read cleanly: {:?}",
            entry.source,
            entry.error
        );
    }
    assert_eq!(
        report.total_added(),
        6,
        "expected one conversation per source"
    );

    let stats = inv.stats().unwrap();
    assert_eq!(stats.conversations, 6);
    for (source, n) in &stats.per_source {
        assert_eq!(*n, 1, "{source} contributed {n} conversations");
    }

    // The index is encrypted at rest and looks like it.
    assert!(stats.encrypted, "index should be encrypted");
    assert!(
        stats.entropy_bits_per_byte > 7.5,
        "entropy was {}",
        stats.entropy_bits_per_byte
    );

    // One search box, one result list, across every tool.
    let hits = inv
        .search(&SearchQuery::new("auth middleware"))
        .unwrap()
        .hits;
    assert!(!hits.is_empty());
    assert_eq!(hits[0].conversation.source, SourceId::Zed);
    assert_eq!(
        hits[0].conversation.title,
        "Refactor the auth middleware into a shared hook"
    );
    // Speaker attribution survived.
    let (_, messages) = inv.conversation(hits[0].conversation.id).unwrap();
    assert_eq!(messages[0].role, inventory_core::Role::User);
    assert_eq!(messages[1].role, inventory_core::Role::Assistant);

    // Matched words are highlighted in context.
    assert!(
        hits[0].snippet.contains('['),
        "expected a highlighted snippet, got {:?}",
        hits[0].snippet
    );

    // Filtering to one source.
    let mut q = SearchQuery::new("index");
    q.sources = vec![SourceId::Codex];
    let filtered = inv.search(&q).unwrap().hits;
    assert!(!filtered.is_empty());
    assert!(filtered
        .iter()
        .all(|h| h.conversation.source == SourceId::Codex));
}

#[test]
fn reindexing_is_idempotent_and_skips_unchanged_files() {
    let fx = Fixture::new();
    install_all_six();

    let mut inv = fx.open();
    let first = inv.index(false).unwrap();
    assert_eq!(first.total_added(), 6);

    let second = inv.index(false).unwrap();
    assert_eq!(second.total_added(), 0, "re-index created duplicates");
    assert_eq!(
        second.total_updated(),
        0,
        "unchanged files should not be re-read"
    );
    assert_eq!(inv.stats().unwrap().conversations, 6);

    // A forced pass re-reads everything but still must not duplicate.
    let forced = inv.index(true).unwrap();
    assert_eq!(forced.total_added(), 0);
    assert_eq!(forced.total_updated(), 6);
    assert_eq!(inv.stats().unwrap().conversations, 6);
}

/// The headline resilience capability: "A tool changing its storage format can
/// no longer delete what Inventory had already indexed from it."
#[test]
fn a_broken_source_freezes_and_then_repairs_itself() {
    let fx = Fixture::new();
    install_all_six();
    let store = fork_store(SourceId::Cursor);

    let mut inv = fx.open();
    inv.index(false).unwrap();
    let before = inv.source_status().unwrap();
    let cursor_before = before
        .iter()
        .find(|s| s.source == SourceId::Cursor)
        .unwrap();
    assert_eq!(cursor_before.state, SourceState::Ok);
    assert_eq!(cursor_before.conversation_count, 1);
    let last_ok = cursor_before.last_ok_at.unwrap();

    // Cursor ships a format this version cannot read.
    let good = std::fs::read(&store).unwrap();
    std::fs::write(&store, b"this is definitely not a sqlite database").unwrap();

    let report = inv.index(true).unwrap();
    let frozen = report.frozen();
    assert_eq!(frozen.len(), 1, "expected exactly one frozen source");
    assert_eq!(frozen[0].source, Some(SourceId::Cursor));

    let during = inv.source_status().unwrap();
    let cursor = during
        .iter()
        .find(|s| s.source == SourceId::Cursor)
        .unwrap();
    assert_eq!(cursor.state, SourceState::Frozen);
    assert!(cursor.last_error.is_some());
    assert_eq!(
        cursor.last_ok_at,
        Some(last_ok),
        "the last successful read must still be reportable"
    );
    // The whole point: history is retained and still searchable.
    assert_eq!(
        cursor.conversation_count, 1,
        "frozen source lost its history"
    );
    let hits = inv.search(&SearchQuery::new("git remote")).unwrap().hits;
    assert!(
        hits.iter()
            .any(|h| h.conversation.source == SourceId::Cursor),
        "frozen source dropped out of search"
    );

    // Every other source kept working through it.
    assert!(during
        .iter()
        .filter(|s| s.source != SourceId::Cursor)
        .all(|s| s.state == SourceState::Ok));

    // "A source that breaks repairs itself once it can be read again."
    std::fs::write(&store, good).unwrap();
    inv.index(false).unwrap();
    let after = inv.source_status().unwrap();
    let cursor = after.iter().find(|s| s.source == SourceId::Cursor).unwrap();
    assert_eq!(cursor.state, SourceState::Ok);
    assert!(cursor.last_error.is_none());
    assert_eq!(cursor.conversation_count, 1);
}

#[test]
fn quick_capture_surfaces_what_you_already_worked_out() {
    let fx = Fixture::new();
    install_all_six();
    let mut inv = fx.open();
    inv.index(false).unwrap();

    let result = inv
        .capture("need to sort out postgres index tuning again")
        .unwrap();
    assert!(result.note.id > 0);
    assert!(
        result
            .related
            .hits
            .iter()
            .any(|h| h.conversation.source == SourceId::Codex),
        "capture did not surface the earlier Codex conversation"
    );
    assert_eq!(inv.notes(10).unwrap().len(), 1);
}

#[test]
fn the_scratchpad_is_off_until_it_is_turned_on() {
    let fx = Fixture::new();
    let inv = fx.open();

    assert!(
        !inv.scratchpad_enabled().unwrap(),
        "scratchpad must default to off"
    );
    assert!(!inv
        .remember_clip("secret token", Some("1Password"))
        .unwrap());
    assert!(
        inv.clips(10).unwrap().is_empty(),
        "nothing may be stored while off"
    );

    inv.set_scratchpad_enabled(true).unwrap();
    assert!(inv
        .remember_clip("cargo test --workspace", Some("Ghostty"))
        .unwrap());
    // Copying the same thing twice is one entry.
    assert!(!inv
        .remember_clip("cargo test --workspace", Some("Ghostty"))
        .unwrap());

    let clips = inv.clips(10).unwrap();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].app.as_deref(), Some("Ghostty"));
    assert!(inv
        .export_clips()
        .unwrap()
        .contains("cargo test --workspace"));

    assert_eq!(inv.clear_clips().unwrap(), 1);
    assert!(inv.clips(10).unwrap().is_empty());
}

#[test]
fn resume_and_primer_carry_a_conversation_elsewhere() {
    let fx = Fixture::new();
    install_all_six();
    let mut inv = fx.open();
    inv.index(false).unwrap();

    let claude = inv
        .search(&SearchQuery::new("monochromatic design"))
        .unwrap()
        .hits
        .into_iter()
        .find(|h| h.conversation.source == SourceId::ClaudeCode)
        .expect("claude code conversation");

    let cmd = inv.resume(claude.conversation.id).unwrap();
    assert_eq!(cmd.display(), "claude --resume sess-1");
    // /work/api does not exist on this machine, so it falls back — "even if
    // the project folder has moved".
    assert!(cmd.project_moved);
    assert!(cmd.transcript.contains("Monochromatic design"));

    let primer = inv.primer(claude.conversation.id).unwrap();
    assert!(primer.contains("What I was working on:"));
    assert!(primer.contains("Monochromatic design with SVG icons"));
    assert!(primer.contains("Claude Code"));

    // Zed has no external resume, so it must say so rather than guess.
    let zed = inv
        .search(&SearchQuery::new("auth middleware"))
        .unwrap()
        .hits
        .into_iter()
        .find(|h| h.conversation.source == SourceId::Zed)
        .unwrap();
    let err = inv.resume(zed.conversation.id).unwrap_err().to_string();
    assert!(err.contains("primer"), "unhelpful error: {err}");
}

#[test]
fn retention_windows_report_their_cost_and_apply() {
    let fx = Fixture::new();
    install_all_six();
    // One conversation from well outside every window but "everything".
    let old = inventory_core::model::now_unix() - 400 * 86_400;
    write(
        root_for(SourceId::ClaudeCode).join("-work-old/sess-old.jsonl"),
        &format!(
            r#"{{"type":"user","sessionId":"sess-old","timestamp":{old},"message":{{"role":"user","content":"ancient kubernetes incident postmortem"}}}}"#
        ),
    );

    let mut inv = fx.open();
    inv.index(false).unwrap();
    assert_eq!(inv.stats().unwrap().conversations, 7);

    let options = inv.retention_options().unwrap();
    assert_eq!(options.len(), 5);
    let all = options.last().unwrap();
    assert_eq!(all.retention, inventory_core::Retention::All);
    assert!(all.selected, "everything is the default window");
    assert!(all.bytes > 0, "on-disk cost should be reported per choice");
    // Windows are nested, so cost is monotonic.
    for pair in options.windows(2) {
        assert!(
            pair[0].bytes <= pair[1].bytes,
            "a wider window cannot cost less"
        );
    }

    // Narrowing the window drops the conversation outside it, and only that
    // one. The six fixture conversations are an hour old; this one is not.
    let old_id = inv
        .search(&SearchQuery::new("ancient kubernetes incident postmortem"))
        .unwrap()
        .hits
        .into_iter()
        .find(|h| h.conversation.external_id == "sess-old")
        .expect("the old conversation is indexed to begin with")
        .conversation
        .id;

    let pruned = inv.set_retention(inventory_core::Retention::Days7).unwrap();
    assert_eq!(pruned, 1, "only the year-old conversation should go");
    assert_eq!(inv.stats().unwrap().conversations, 6);
    assert!(
        inv.conversation(old_id).is_err(),
        "pruned conversation still readable"
    );
    // It must leave the search index too, not just the conversations table.
    assert!(
        !inv.search(&SearchQuery::new("ancient kubernetes incident postmortem"))
            .unwrap()
            .hits
            .iter()
            .any(|h| h.conversation.id == old_id),
        "pruned conversation still appears in search results"
    );
    // Everything inside the window is untouched.
    assert!(!inv
        .search(&SearchQuery::new("auth middleware"))
        .unwrap()
        .hits
        .is_empty());
}

#[test]
fn an_index_written_with_one_key_will_not_open_with_another() {
    let fx = Fixture::new();
    {
        let inv = Inventory::open_at(&fx.index_path(), &StaticKey::new("a".repeat(64))).unwrap();
        inv.set_scratchpad_enabled(true).unwrap();
    }
    match Inventory::open_at(&fx.index_path(), &StaticKey::new("b".repeat(64))) {
        Err(inventory_core::Error::KeyMismatch(_)) => {}
        Err(other) => panic!("expected a key mismatch, got {other:?}"),
        Ok(_) => panic!("a wrong key opened the index"),
    }
}

/// A regression test for a real bug: `open_at` used to reseal the file
/// unconditionally on every open, including a pure read. That races a
/// concurrently running writer — the desktop app, say — checkpointing
/// between this open's read and its own forced reseal, and clobbers
/// whatever the writer just committed. A read-only open must leave the
/// on-disk bytes untouched.
#[test]
fn a_read_only_reopen_does_not_touch_the_sealed_file() {
    let fx = Fixture::new();
    {
        let inv = fx.open();
        inv.set_scratchpad_enabled(true).unwrap();
    }
    let before = std::fs::read(fx.index_path()).unwrap();

    {
        let inv = fx.open();
        // A real read, so this cannot be optimised away as unused.
        assert!(inv.scratchpad_enabled().unwrap());
    }
    let after = std::fs::read(fx.index_path()).unwrap();

    assert_eq!(
        before, after,
        "a read-only open reencrypted the index; it should have left the sealed bytes alone"
    );
}

/// Build a git repository on disk with a remote and one real source file.
fn git_repo(dir: &std::path::Path, remote: &str) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(
        dir.join(".git/config"),
        format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {remote}\n"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/auth.rs"), "// the file under discussion\n").unwrap();
}

/// The archaeology path end to end: a conversation recorded inside a checkout
/// is resolved to that repository, the files it discussed are extracted, and
/// the file can be asked what was said about it.
#[test]
fn a_conversation_is_attached_to_its_repository_and_files() {
    let fx = Fixture::new();
    let checkout = tempfile::tempdir().unwrap();
    git_repo(checkout.path(), "git@github.com:acme/api.git");
    let cwd = checkout.path().display().to_string();
    let t = recent();

    write(
        root_for(SourceId::ClaudeCode).join("-acme-api/sess-repo.jsonl"),
        &format!(
            r#"{{"type":"user","sessionId":"sess-repo","cwd":"{cwd}","gitBranch":"main","timestamp":{t},"message":{{"role":"user","content":"the token refresh in src/auth.rs races the retry"}}}}
{{"type":"assistant","timestamp":{t},"message":{{"role":"assistant","content":[{{"type":"text","text":"Moved the lock in src/auth.rs above the refresh, and left src/db.rs alone."}}]}}}}"#
        ),
    );

    let mut inv = fx.open();
    inv.index(false).unwrap();

    let repos = inv.repos().unwrap();
    assert_eq!(repos.len(), 1, "expected exactly one repo, got {repos:?}");
    assert_eq!(repos[0].key, "git:github.com/acme/api");
    assert_eq!(repos[0].name, "api");
    assert_eq!(repos[0].conversations, 1);

    // The file that exists on disk and was discussed twice.
    let history = inv
        .history_for_path(std::path::Path::new("src/auth.rs"), checkout.path(), 10)
        .unwrap();
    assert_eq!(history.path, "src/auth.rs");
    assert_eq!(history.hits.len(), 1, "{history:?}");
    assert_eq!(history.hits[0].mentions, 2);
    assert!(history.hits[0].conversation.title.contains("token refresh"));

    // A file mentioned once, which does not exist on disk — the history that
    // matters most is exactly the history of files that have since gone.
    let db = inv
        .history_for_path(std::path::Path::new("src/db.rs"), checkout.path(), 10)
        .unwrap();
    assert_eq!(db.hits.len(), 1, "a deleted file should still have history");

    // And a file nobody ever discussed.
    let quiet = inv
        .history_for_path(std::path::Path::new("src/quiet.rs"), checkout.path(), 10)
        .unwrap();
    assert!(quiet.hits.is_empty());
}

/// The same identity holds when the checkout moves, because conversations are
/// grouped by remote rather than by path. This is the case a path-keyed index
/// gets wrong, and the reason `repos.key` exists at all.
#[test]
fn a_moved_checkout_stays_one_repository() {
    let fx = Fixture::new();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    git_repo(first.path(), "https://github.com/acme/api.git");
    git_repo(second.path(), "git@github.com:acme/api.git");
    let t = recent();

    for (n, dir) in [(1, first.path()), (2, second.path())] {
        let cwd = dir.display().to_string();
        write(
            root_for(SourceId::ClaudeCode).join(format!("-acme-api/sess-{n}.jsonl")),
            &format!(
                r#"{{"type":"user","sessionId":"sess-{n}","cwd":"{cwd}","timestamp":{t},"message":{{"role":"user","content":"working on src/auth.rs again"}}}}"#
            ),
        );
    }

    let mut inv = fx.open();
    inv.index(false).unwrap();

    let repos = inv.repos().unwrap();
    assert_eq!(
        repos.len(),
        1,
        "the same remote cloned twice should be one repo, got {repos:?}"
    );
    assert_eq!(repos[0].conversations, 2);
}

/// `--repo` and `--file` narrow a normal search, so the archaeology data also
/// serves the search box it was built alongside.
#[test]
fn search_can_be_scoped_to_a_repository_and_a_file() {
    let fx = Fixture::new();
    let checkout = tempfile::tempdir().unwrap();
    git_repo(checkout.path(), "git@github.com:acme/api.git");
    let cwd = checkout.path().display().to_string();
    let t = recent();

    write(
        root_for(SourceId::ClaudeCode).join("-acme-api/sess-scope.jsonl"),
        &format!(
            r#"{{"type":"user","sessionId":"sess-scope","cwd":"{cwd}","timestamp":{t},"message":{{"role":"user","content":"the retry loop in src/auth.rs never terminates"}}}}"#
        ),
    );
    // A second conversation, same words, different project.
    write(
        root_for(SourceId::ClaudeCode).join("-other/sess-other.jsonl"),
        &format!(
            r#"{{"type":"user","sessionId":"sess-other","cwd":"/somewhere/else","timestamp":{t},"message":{{"role":"user","content":"the retry loop never terminates here either"}}}}"#
        ),
    );

    let mut inv = fx.open();
    inv.index(false).unwrap();

    let mut all = SearchQuery::new("retry loop");
    all.meaning = false;
    assert_eq!(inv.search(&all).unwrap().hits.len(), 2);

    let mut scoped = SearchQuery::new("retry loop");
    scoped.meaning = false;
    scoped.repo = Some("api".into());
    let hits = inv.search(&scoped).unwrap().hits;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].conversation.title.contains("retry loop"));

    let mut by_file = SearchQuery::new("retry loop");
    by_file.meaning = false;
    by_file.file = Some("src/auth.rs".into());
    assert_eq!(inv.search(&by_file).unwrap().hits.len(), 1);

    let mut missing = SearchQuery::new("retry loop");
    missing.meaning = false;
    missing.repo = Some("no-such-repo".into());
    assert!(inv.search(&missing).unwrap().hits.is_empty());
}
