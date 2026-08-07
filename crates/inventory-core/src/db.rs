//! The single encrypted file everything lives in.
//!
//! There is no page-level cipher here: SQLite itself never sees an encrypted
//! byte. `open` decrypts the on-disk file, in full, into a private SQLite
//! file that lives only in a per-process temp directory; the caller works
//! against that plaintext copy and calls `seal_to` (via
//! `Inventory::checkpoint`, see `index.rs`) to fold it back into a single
//! AES-256-GCM-sealed file. That trades continuous, per-transaction
//! durability for a build with nothing to install: no C compiler, no
//! OpenSSL, no Perl, anywhere in the dependency graph — pure Rust crypto
//! (`aes-gcm`) over a plain, unmodified SQLite (`rusqlite`'s `bundled`
//! feature, `cc` only).

use crate::keychain::KeyProvider;
use crate::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use rusqlite::Connection;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i32 = 3;

/// Marks the sealed container format, so a stray SQLite file — or an index
/// written by a version of this app that used SQLCipher — is never mistaken
/// for one of ours.
const MAGIC: &[u8; 8] = b"INVSEAL1";
const NONCE_LEN: usize = 12;

/// A live connection over a private, unencrypted working copy, plus what
/// `Inventory` needs to keep resealing it.
#[derive(Debug)]
pub struct Opened {
    pub conn: Connection,
    pub key: [u8; 32],
    pub plain_path: PathBuf,
    pub tempdir: tempfile::TempDir,
}

/// Open (creating if needed) the encrypted index at `path`.
///
/// Nothing is ever written to `path` here — see `seal_to` for that. A fresh
/// index starts as an empty working copy; an existing one is decrypted in
/// full into it.
pub fn open(path: &Path, key: &dyn KeyProvider) -> Result<Opened> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existed = path.exists();

    // An index with no key is not a first run — it is a lost key. Minting a
    // new one here would "succeed" and then fail to decrypt, which reads as
    // corruption rather than as what actually happened.
    if existed && !key.exists()? {
        return Err(Error::KeyUnavailable(format!(
            "{} exists but {} holds no key for it.\n\
             The index cannot be opened without the key it was written with. \
             On Linux this happens when the key was stored in the kernel keyring, \
             which does not survive a reboot; a Secret Service keyring does.\n\
             If you have the key, set INVENTORY_INDEX_KEY. If not, the index has to \
             be rebuilt: move it aside and re-index — your tools' own history is \
             untouched and will be read again.",
            path.display(),
            key.describe()
        )));
    }

    let key_bytes = parse_key(&key.get_or_create()?)?;

    let tempdir = tempfile::Builder::new()
        .prefix("inventory-index")
        .tempdir()?;
    let plain_path = tempdir.path().join("index.sqlite3");

    if existed {
        let sealed = std::fs::read(path)?;
        let plaintext = unseal(&sealed, &key_bytes, path)?;
        std::fs::write(&plain_path, plaintext)?;
    }

    let conn = Connection::open(&plain_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;

    Ok(Opened {
        conn,
        key: key_bytes,
        plain_path,
        tempdir,
    })
}

/// A 64-character hex string, the only shape `KeyProvider` ever hands back.
fn parse_key(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::KeyUnavailable(
            "the stored index key is not a 256-bit hex value".into(),
        ));
    }
    let mut bytes = [0u8; 32];
    for (byte, chunk) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        // `chunk` is two ASCII hex digits; `hex` was just validated above.
        *byte = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
    }
    Ok(bytes)
}

/// Decrypt a sealed blob read from `path` (used only for error messages).
fn unseal(sealed: &[u8], key: &[u8; 32], path: &Path) -> Result<Vec<u8>> {
    if sealed.len() < MAGIC.len() + NONCE_LEN || &sealed[..MAGIC.len()] != MAGIC {
        return Err(Error::LegacyIndexFormat {
            path: path.to_path_buf(),
        });
    }
    let nonce = Nonce::from_slice(&sealed[MAGIC.len()..MAGIC.len() + NONCE_LEN]);
    let ciphertext = &sealed[MAGIC.len() + NONCE_LEN..];
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher.decrypt(nonce, ciphertext).map_err(|_| {
        Error::KeyMismatch(format!(
            "{} exists but the key does not open it",
            path.display()
        ))
    })
}

/// Encrypt `plaintext` with a fresh random nonce and write it over `dest`.
/// Written to a staging file and renamed into place, so an interruption
/// mid-write never corrupts the existing sealed file.
pub fn seal_to(dest: &Path, plaintext: &[u8], key: &[u8; 32]) -> Result<()> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| Error::other(format!("failed to seal the index: {e}")))?;

    let staging = dest.with_extension("sealing");
    {
        let mut f = std::fs::File::create(&staging)?;
        f.write_all(MAGIC)?;
        f.write_all(&nonce)?;
        f.write_all(&ciphertext)?;
        f.sync_all()?;
    }
    std::fs::rename(&staging, dest)?;
    Ok(())
}

/// Checkpoint `conn`'s WAL into its main file, then seal that file over
/// `dest`. The only correct way to persist a live connection: reading
/// `plain_path` directly, without checkpointing first, can silently miss
/// whatever is still sitting in the WAL.
pub fn seal(conn: &Connection, plain_path: &Path, dest: &Path, key: &[u8; 32]) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let plaintext = std::fs::read(plain_path)?;
    seal_to(dest, &plaintext, key)
}

/// Does this file look like one of our sealed indexes? Reported in `Stats`
/// and by `inv doctor`; only reads the header, never the whole file.
pub fn looks_encrypted(path: &Path) -> Result<bool> {
    read_header::<{ MAGIC.len() }>(path).map(|h| h.as_ref() == Some(MAGIC))
}

/// Is this an unencrypted SQLite file left over from before encryption
/// existed? Gates the one-time migration path in `Inventory::open_at`.
pub fn is_plaintext_sqlite(path: &Path) -> Result<bool> {
    const SQLITE_HEADER: [u8; 16] = *b"SQLite format 3\0";
    read_header::<16>(path).map(|h| h == Some(SQLITE_HEADER))
}

/// First `N` bytes of `path`, or `None` if it doesn't exist or is shorter
/// than `N`.
fn read_header<const N: usize>(path: &Path) -> Result<Option<[u8; N]>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut f = std::fs::File::open(path)?;
    let mut header = [0u8; N];
    match f.read_exact(&mut header) {
        Ok(()) => Ok(Some(header)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Shannon entropy of the file in bits per byte. The security page invites
/// verification of exactly this number, so `inv doctor` reports it.
pub fn shannon_entropy(path: &Path) -> Result<f64> {
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(0.0);
    }
    let mut counts = [0u64; 256];
    for b in &bytes {
        counts[*b as usize] += 1;
    }
    let len = bytes.len() as f64;
    let mut h = 0.0;
    for c in counts {
        if c > 0 {
            let p = c as f64 / len;
            h -= p * p.log2();
        }
    }
    Ok(h)
}

/// Convert a plaintext index to a sealed one.
///
/// "Existing indexes are converted automatically, without touching the
/// original until the new one is proven." The original is only renamed to
/// `.plaintext.bak` after the replacement has been decrypted back and
/// checked byte-for-byte, so an interruption at any point leaves a working
/// index behind.
pub fn convert_plaintext_to_encrypted(
    path: &Path,
    key: &dyn KeyProvider,
) -> Result<Option<PathBuf>> {
    if !path.exists() || !is_plaintext_sqlite(path)? {
        return Ok(None);
    }
    let key_bytes = parse_key(&key.get_or_create()?)?;
    let plaintext = std::fs::read(path)?;

    let staging = path.with_extension("converting");
    seal_to(&staging, &plaintext, &key_bytes)?;

    // Prove the new file before the old one is disturbed.
    let sealed = std::fs::read(&staging)?;
    let roundtrip = unseal(&sealed, &key_bytes, &staging)
        .map_err(|e| Error::other(format!("converted index failed verification: {e}")))?;
    if roundtrip != plaintext {
        return Err(Error::other(
            "converted index does not match the original; original left untouched",
        ));
    }

    let backup = path.with_extension("plaintext.bak");
    std::fs::rename(path, &backup)?;
    std::fs::rename(&staging, path)?;
    Ok(Some(backup))
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS conversations (
            id            INTEGER PRIMARY KEY,
            source        TEXT    NOT NULL,
            external_id   TEXT    NOT NULL,
            title         TEXT    NOT NULL,
            project_path  TEXT,
            git_branch    TEXT,
            started_at    INTEGER NOT NULL,
            updated_at    INTEGER NOT NULL,
            message_count INTEGER NOT NULL DEFAULT 0,
            UNIQUE(source, external_id)
        );
        CREATE INDEX IF NOT EXISTS conversations_updated  ON conversations(updated_at DESC);
        CREATE INDEX IF NOT EXISTS conversations_source   ON conversations(source, updated_at DESC);

        CREATE TABLE IF NOT EXISTS messages (
            id              INTEGER PRIMARY KEY,
            conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            seq             INTEGER NOT NULL,
            role            TEXT    NOT NULL,
            text            TEXT    NOT NULL,
            created_at      INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS messages_conv ON messages(conversation_id, seq);

        -- Standalone (not external-content) so snippet() and highlight() work
        -- directly. rowid is always the conversation id.
        CREATE VIRTUAL TABLE IF NOT EXISTS conversations_fts USING fts5(
            title,
            body,
            tokenize = 'porter unicode61 remove_diacritics 2'
        );

        -- One row per source file already read, so a re-index reads each file
        -- once: unchanged mtime+size+digest means skip.
        CREATE TABLE IF NOT EXISTS seen_files (
            source TEXT    NOT NULL,
            path   TEXT    NOT NULL,
            mtime  INTEGER NOT NULL,
            size   INTEGER NOT NULL,
            digest TEXT    NOT NULL,
            PRIMARY KEY (source, path)
        );

        CREATE TABLE IF NOT EXISTS source_status (
            source      TEXT PRIMARY KEY,
            state       TEXT    NOT NULL,
            last_ok_at  INTEGER,
            last_error  TEXT,
            frozen_at   INTEGER
        );

        -- Quick capture (⌘⇧N).
        CREATE TABLE IF NOT EXISTS notes (
            id         INTEGER PRIMARY KEY,
            text       TEXT    NOT NULL,
            created_at INTEGER NOT NULL
        );

        -- Clipboard scratchpad (⌘⇧V). Off by default; see settings.
        CREATE TABLE IF NOT EXISTS clips (
            id         INTEGER PRIMARY KEY,
            text       TEXT    NOT NULL,
            app        TEXT,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS clips_recent ON clips(created_at DESC);

        -- Dense vectors for the semantic half of search.
        CREATE TABLE IF NOT EXISTS embeddings (
            conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
            model           TEXT NOT NULL,
            vec             BLOB NOT NULL
        );

        -- Trained embedding model state (vocabulary + term vectors).
        CREATE TABLE IF NOT EXISTS embedding_model (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            kind       TEXT    NOT NULL,
            dim        INTEGER NOT NULL,
            trained_at INTEGER NOT NULL,
            doc_count  INTEGER NOT NULL,
            payload    BLOB    NOT NULL
        );

        -- Clustered index over the embeddings, rebuilt when they are.
        -- `model` and `vectors` are the staleness key: either changing means
        -- the stored clusters no longer describe the live vectors.
        CREATE TABLE IF NOT EXISTS ann_index (
            id       INTEGER PRIMARY KEY CHECK (id = 1),
            model    TEXT    NOT NULL,
            vectors  INTEGER NOT NULL,
            built_at INTEGER NOT NULL,
            payload  BLOB    NOT NULL
        );

        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        -- The repository a conversation happened in. `key` is a normalised
        -- remote URL where there is one, so the same repo cloned to two paths
        -- — or simply moved — stays one row. `root` is only the last place it
        -- was seen and may no longer exist.
        CREATE TABLE IF NOT EXISTS repos (
            id     INTEGER PRIMARY KEY,
            key    TEXT NOT NULL UNIQUE,
            root   TEXT NOT NULL,
            remote TEXT,
            name   TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS conversation_repo (
            conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
            repo_id         INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
            -- 'recorded' (the source knew the working directory) or
            -- 'inferred' (it was reconstructed from paths in the transcript).
            origin          TEXT    NOT NULL
        );
        CREATE INDEX IF NOT EXISTS conversation_repo_by_repo ON conversation_repo(repo_id);

        -- Repo-relative paths a conversation mentioned, with how often. The
        -- join that answers "what was I thinking when I wrote this".
        CREATE TABLE IF NOT EXISTS conversation_file (
            conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            repo_id         INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
            path            TEXT    NOT NULL,
            mentions        INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (conversation_id, path)
        );
        CREATE INDEX IF NOT EXISTS conversation_file_lookup ON conversation_file(repo_id, path);
        "#,
    )?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    Ok(match rows.next()? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::StaticKey;

    #[test]
    fn round_trips_through_encryption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inventory.sqlite3");
        let key = StaticKey::new("a".repeat(64));

        {
            let opened = open(&path, &key).unwrap();
            opened
                .conn
                .execute(
                    "INSERT INTO settings(key,value) VALUES ('hello','world')",
                    [],
                )
                .unwrap();
            seal(&opened.conn, &opened.plain_path, &path, &opened.key).unwrap();
        }

        assert!(
            looks_encrypted(&path).unwrap(),
            "index should be encrypted at rest"
        );
        assert!(
            shannon_entropy(&path).unwrap() > 7.5,
            "encrypted file should look random"
        );

        let opened = open(&path, &key).unwrap();
        assert_eq!(
            get_setting(&opened.conn, "hello").unwrap().as_deref(),
            Some("world")
        );
    }

    #[test]
    fn wrong_key_is_an_error_not_a_fresh_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inventory.sqlite3");
        {
            let opened = open(&path, &StaticKey::new("a".repeat(64))).unwrap();
            seal(&opened.conn, &opened.plain_path, &path, &opened.key).unwrap();
        }
        let err = open(&path, &StaticKey::new("b".repeat(64))).unwrap_err();
        assert!(matches!(err, Error::KeyMismatch(_)), "got {err:?}");
    }

    /// The Linux reboot case, and the reason `KeyProvider::exists` exists:
    /// an index with no key must say so, not mint a fresh key and then fail to
    /// decrypt with what reads like a corruption error.
    #[test]
    fn an_index_with_no_key_reports_the_lost_key_rather_than_minting_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inventory.sqlite3");
        {
            let opened = open(&path, &StaticKey::new("d".repeat(64))).unwrap();
            seal(&opened.conn, &opened.plain_path, &path, &opened.key).unwrap();
        }

        // A provider holding nothing — the keychain came back empty.
        match open(&path, &StaticKey::new("")) {
            Err(Error::KeyUnavailable(msg)) => {
                assert!(msg.contains("holds no key"), "unhelpful message: {msg}");
                assert!(msg.contains("rebuilt"), "no recovery advice: {msg}");
            }
            Err(other) => panic!("expected a lost-key error, got {other:?}"),
            Ok(_) => panic!("opened an encrypted index with no key"),
        }
    }

    #[test]
    fn plaintext_index_converts_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inventory.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO settings VALUES ('kept','yes');",
            )
            .unwrap();
        }
        assert!(!looks_encrypted(&path).unwrap());

        let key = StaticKey::new("c".repeat(64));
        let backup = convert_plaintext_to_encrypted(&path, &key)
            .unwrap()
            .unwrap();

        assert!(backup.exists(), "original should be preserved");
        assert!(looks_encrypted(&path).unwrap());
        let opened = open(&path, &key).unwrap();
        assert_eq!(
            get_setting(&opened.conn, "kept").unwrap().as_deref(),
            Some("yes")
        );
    }

    /// A version of this app that used SQLCipher wrote pages we can no
    /// longer decrypt — no OpenSSL is linked any more. That must fail
    /// clearly, not be silently treated as a fresh index.
    #[test]
    fn a_legacy_sqlcipher_style_file_is_a_clear_error_not_a_fresh_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inventory.sqlite3");
        // SQLCipher's on-disk format is indistinguishable from random bytes.
        std::fs::write(&path, vec![0x42u8; 4096]).unwrap();

        let key = StaticKey::new("e".repeat(64));
        match open(&path, &key) {
            Err(Error::LegacyIndexFormat { .. }) => {}
            other => panic!("expected LegacyIndexFormat, got {other:?}"),
        }
    }
}
