# Architecture

How `rusty_inventrory` is put together, and why. The capability list it is
built against is in [`../CAPABILITIES.md`](../CAPABILITIES.md).

---

## Shape

```
inventory-tauri ──┐
                  ├──► inventory-core ──► one encrypted SQLite file
inventory-cli   ──┘
```

Every capability lives in `inventory-core`. The two frontends hold no logic
beyond presentation — which is the reason the same features are reachable from
a menu bar panel and from a terminal without either one being a second
implementation.

### `inventory-core` modules

| Module | Responsibility |
| --- | --- |
| `model` | Domain types: `SourceId`, `Conversation`, `Message`, `Role`, `Retention`, source status. |
| `paths` | Where each tool stores history, per platform. Overridable for tests. |
| `keychain` | Index key: create, fetch, fail closed. |
| `db` | Open/migrate the encrypted store; plaintext→encrypted conversion; entropy check. |
| `sources/*` | The six readers, plus the snapshot and VS Code-fork helpers. |
| `embed/*` | On-device embedding: hashing fallback, locally-trained LSA, and the linear algebra behind it. |
| `search` | BM25 + semantic retrieval, RRF fusion, meaning labelling. |
| `repo` | Resolving a conversation to the repository and files it touched. |
| `index` | The `Inventory` type: indexing, freeze/repair, retention, stats. |
| `capture` | Quick capture and the clipboard scratchpad. |
| `handoff` | Session resumption and hand-off primers. |
| `vectors` | Resident vector set, exact scan, and the clustered index over it. |
| `watch` | Debounced polling of the source stores, so the index stays live. |
| `update` | Update-check *policy*. Deliberately contains no transport. |
| `format` | Human-readable dates, relative times, byte sizes. |

---

## Decisions worth explaining

### Reading each source

Two rules hold everywhere:

1. **Read-only, always.** Nothing writes to, moves, or locks a file a tool
   owns.
2. **Tolerant parsing.** These are undocumented formats that change without
   notice. A record a parser does not recognise is skipped; it never fails the
   file.

**Snapshots.** Editors hold their SQLite stores open in WAL mode. Attaching
directly risks lock contention with a running app, and a read-only open may
refuse to recover the WAL — which would silently hide the most recent
conversations, exactly the ones the user wants. So the database and its `-wal`
and `-shm` sidecars are copied to a temporary directory and the *copy* is
opened. `sources::snapshot` has a test that writes to a WAL without
checkpointing and proves the snapshot still sees the data.

**The VS Code forks** (Cursor, Kiro, Antigravity) share Code's storage layout
and rename their chat keys freely between releases. So `sources::vscdb` finds
conversations *structurally* — it walks the JSON looking for anything shaped
like a message list — rather than reaching for known key paths. A renamed key
therefore does not take the source down. There is a test for exactly this
(`finds_conversations_under_unfamiliar_keys`).

### Freeze rather than delete

When a source fails to parse, the indexer:

- records the failure against that source and marks it **frozen**,
- **keeps every conversation already indexed from it**, still searchable,
- preserves `last_ok_at` so the UI can say when it last read cleanly,
- does **not** persist that pass's seen-file list, so the next run re-reads,
- leaves the other five sources completely unaffected.

Success clears the freeze, which is the whole of the self-repair behaviour.

The subtle part is that a corrupt store must produce an **error**, not an empty
result. Returning "no conversations" from an unreadable file would look
identical to "you have no conversations" — the outcome the freeze exists to
prevent. `vscdb::scan_fork` reads `sqlite_master` immediately after opening for
precisely this reason.

### Hybrid search

Keyword and semantic retrieval run separately and are fused by **Reciprocal
Rank Fusion**:

```
score(d) = Σ  w_list / (60 + rank_list(d))
```

RRF fuses *ranks*, not scores. BM25 values and cosine similarities are not on
comparable scales and their distributions shift with the query, so any weighted
score blend needs calibration that will not hold across six tools' very
different transcripts. Fusing ranks sidesteps the problem.

Recency enters as a **third ranked list** (weight 0.35) over the candidates
already retrieved, rather than as a score multiplier — so it can order
near-ties without ever swamping relevance.

**Query safety.** Every token is quoted and prefix-matched before it reaches
FTS5, so `NEAR(`, `-`, `*` and `"` typed by a user are text, not syntax.
Precision first: all terms ANDed; if that finds nothing, it widens to OR rather
than showing an empty page.

**Labelling.** A hit retrieved only by the semantic arm is labelled, because
"a result with none of your words otherwise looks like a bug". The label is
only *"found by meaning"* when a model that actually models meaning ran;
otherwise it reads *"found by similarity"*. `SearchResponse::semantic_available`
carries that distinction to both frontends.

### The embedding model

This is the largest departure from the reviewed product, which ships a static
embedding model. Rather than download one at runtime — which would put the
product's single strongest claim, that nothing leaves the machine, in tension
with its own setup — the model is **trained locally from the user's own
conversations**:

- tf-idf term/document matrix over the indexed corpus,
- truncated SVD via a randomized range finder with power iterations, the small
  dense eigenproblem solved by cyclic Jacobi,
- term vectors scaled by √σ; a document or query is the idf-weighted mean of
  its term vectors, L2-normalised.

Terms that keep the same company end up with neighbouring vectors. That is what
makes "container" retrieve "pod stuck terminating" — learned from *this user's*
corpus rather than from a general-purpose model, which is arguably the better
fit for jargon, internal service names and project vocabulary that no
general-purpose model has seen.

Consequences, stated plainly:

- It needs a corpus. Below 32 conversations, a hashed-random-projection
  embedder stands in. It is honest about having no semantics
  (`is_semantic() == false`), and the UI degrades its label accordingly.
- The rank adapts down when the vocabulary is small, rather than refusing.
- The model is retrained when the corpus grows 1.5×, and **every vector is
  recomputed** when it is — a retrained model is a new vector space, so mixing
  old and new vectors would be meaningless.
- Out-of-vocabulary queries return a **zero vector**, so the semantic arm
  abstains. The tempting alternative — fall back to the hashing embedder — is
  wrong: its output lives in a different space from the stored document
  vectors, and comparing across the two produces arbitrary similarities that
  would then be shown under a "found by meaning" label.
- Randomness is seeded from a constant, so indexing the same corpus twice gives
  the same vectors.

`Embedder` is a trait, so dropping in a shipped static model later is a new
implementation, not a rewrite.

### Making semantic search scale

Two layers, and the first matters more than the second.

**The vectors live in memory.** The original implementation read every
embedding blob out of SQLite and decoded it on *every query* — as-you-type,
that is the same megabytes re-read and re-parsed per keystroke. `VectorSet`
loads them once into one contiguous `Vec<f32>`. The scan was never the
expensive part; the I/O and the decode were.

**An IVF index narrows the scan, above a measured threshold.** `IvfIndex`
clusters the set with k-means and probes only the nearest clusters. The
threshold is evidence, not intuition — `examples/vector_bench.rs`, 128
dimensions, time for one exact scan:

| vectors | per query |
|---------|-----------|
| 5,000 | 0.6 ms |
| 20,000 | 2.7 ms |
| 50,000 | 7.0 ms |
| 200,000 | 29 ms |
| 1,000,000 | 141 ms |

A heavy user with years of history across six tools lands in the tens of
thousands, where the exact scan is comfortably inside an as-you-type budget.
So `MIN_VECTORS_FOR_INDEX` is 50,000: below it the index would trade recall
for nothing.

Three rules make an approximate index safe in front of an exact one, all
adopted from `rusty_remind_me`'s `ann_index.rs`:

- **Never a source of truth.** No index, a stale one, or too few survivors
  after filtering all fall back to the exact scan. A search must never fail —
  or silently return less — because an optimisation was unavailable.
- **It narrows candidates; it does not score them.** The index picks a
  shortlist, exact cosines are computed over it, so scores are identical to
  the brute-force ones and RRF fusion cannot tell the index ran.
- **Staleness is detected.** The index records the model and vector count it
  was built from; either changing means it is ignored. A stale index quietly
  returning deleted conversations is worse than no index, because the results
  look plausible.

Clustering is plain k-means here rather than a bound C++ ANN library: it
reuses arithmetic already present for the embedder and keeps the promise that
this builds with nothing installed. It runs during an index pass, never on a
search path — a search must not silently pay for a build.

Two calibration notes worth keeping. Probing one cluster gives poor recall,
because a query near a boundary has its neighbours split across several, so at
least an eighth of clusters are probed. And recall must be measured against
*realistic* queries: a uniform-random query sits far from every cluster, which
makes "nearest cluster" nearly arbitrary. The test perturbs real members,
which is what a query embedding actually is.

### Keeping the index live

The source stores are stat-ed on an interval rather than watched through
filesystem events. Polling needs no extra dependency, behaves identically on
all three platforms, and a stat of a few hundred paths every few seconds is far
cheaper than the indexing it gates — in a crate whose whole pitch is that it
needs nothing installed, an event-based watcher would be a new dependency and a
new class of platform-specific failure.

The debounce is the part that matters. A file whose `(mtime, size)` has just
changed is **deferred** until a later tick sees the same signature, so the
indexer never reads a transcript an agent is still appending to. Without it, a
live session would re-trigger indexing on every message — the exact opposite of
"reading each file once". Parsers already skip a truncated trailing line, so a
mid-write read costs correctness nothing; it costs *work*, repeatedly, on the
machine of someone who is mid-task.

Two cases the naive version gets wrong, both covered by tests:

- A file older than the grace window settles **immediately**. Otherwise every
  launch would sit through a full interval before touching the backlog already
  on disk, and old files are by definition not being written to now.
- A file that changes and reverts within one interval is **not** a change, and
  its pending entry is cleared.

`Watcher::prime()` records the current state without reporting it, so the first
tick after the startup index reports what changed *since* that index rather
than the whole disk again. A vanished file is forgotten but never triggers
indexing: deleting a transcript does not delete what was indexed from it.

### Encryption and the key

AES-256-GCM, pure Rust (`aes-gcm`), a per-machine 256-bit key held in the OS
keychain and never written into the file it unlocks. Earlier versions used
SQLCipher; that meant linking OpenSSL, and building OpenSSL from source means
Perl — a second, unrelated language toolchain just to open a database, which
is exactly the kind of thing "needs nothing installed" (see above) rules out.
`db.rs` now runs plain, unmodified SQLite (`rusqlite`'s `bundled` feature,
a C compiler and nothing else) against a private working copy, and seals
that whole file with AES-GCM instead of encrypting it page by page.

Concretely: `db::open` decrypts the on-disk file, in full, into a `TempDir`
SQLite never touches directly outside that copy; `Inventory::checkpoint`
folds the WAL into it and reseals it over the original with a fresh random
nonce. This trades continuous, per-transaction durability (SQLCipher's page
cipher meant every commit was durable on disk) for a dependency-free build:
a crash between two checkpoints loses whatever changed since the last one,
and leaves the stale plaintext copy in the OS temp directory rather than
nowhere. `checkpoint` is cheap to call often — it compares
`Connection::total_changes()` against the value as of the last seal and does
nothing when they match — so every long-running caller (the desktop app's
background loop, `inv watch`) calls it on a short interval rather than
relying on `Drop`, which a killed process, `std::process::exit`, or Tauri's
`app.exit()` never runs.

The important behaviour is the distinction between **no key yet** (normal first
run — mint one) and **keychain unreadable** (fatal). Collapsing them would let
a transient keychain error look like a fresh install and rebuild an index that
was never actually lost. `Error::KeyUnavailable` exists to make that
non-recoverable by construction, and a wrong key produces `KeyMismatch` rather
than a confusing "file is not a database" much later. A file sealed by the old
SQLCipher scheme decrypts as neither — it fails the container's magic-header
check outright — and gets `Error::LegacyIndexFormat`, the same "rebuild it,
nothing is lost" story as a missing key.

Plaintext indexes are converted by sealing a copy of the raw bytes, decrypting
it back and comparing byte-for-byte, and only then renaming the original to
`.plaintext.bak`. An interruption at any point leaves a working index behind.

`db::shannon_entropy` exists so `inv doctor` can report the same number the
product's security page invites you to verify.

### No network in the core

`inventory-core` links no HTTP client. `update.rs` holds the *policy* — opt-out,
once-a-day interval, version comparison — and defines an `UpdateTransport`
trait the shell implements. This turns "the app makes no network calls" from a
promise into something checkable with `cargo tree`.

### Attaching conversations to code

Every source records where a conversation was happening, as
`Conversation::project_path`. On its own that is a string with somebody's home
directory in it, so `repo.rs` turns it into two durable things: a repository
identity, and the repo-relative paths the conversation discussed. This is what
`inv why` reads, and it is a capability the reviewed product does not have —
see [`CRITIQUE.md`](CRITIQUE.md#2-it-indexes-the-wrong-artifact) for why it is
worth having.

**Identity is the remote, not the path.** `repos.key` is a normalised origin
URL, so `git@github.com:o/r.git` and `https://github.com/o/r` are one row, and
a checkout that moves — or exists at different paths on two machines — does not
fragment into several repositories. A repo with no remote falls back to its own
path, which still groups correctly on the machine where the work happened, and
a project that is not a git repository at all is keyed by path rather than
dropped.

**Git is read directly.** `.git/config` is an INI file and the only field
wanted is a remote URL. Linking libgit2 to read one line would put a C
toolchain back into a dependency graph that the sealing design went out of its
way to keep pure Rust, and shelling out to `git` would make indexing depend on
a binary that may not be installed. Worktrees and submodules — where `.git` is
a file pointing at state elsewhere — are followed via `gitdir`/`commondir`.

**File extraction is a heuristic, and is documented as one.** The tool-call
arguments in these transcripts carry exact paths, but the source parsers
flatten every message to text before the indexer sees it, so that structure is
gone by the time `repo.rs` runs. What is left is scanning for path-shaped
tokens: anything that resolves under the repository root on disk is taken, and
anything else needs a recognised source extension. A bare filename with no
directory is only accepted when it can be seen on disk, since otherwise every
mention of `index.js` anywhere would collapse into one entry.

Paths that no longer exist are kept deliberately. A file that was deleted or
renamed is precisely the one whose history is hard to recover by any other
means, and requiring files to exist would discard it.

**Linking never fails an index pass.** A conversation that cannot be placed is
still fully searchable by text. Trading the capability that works for the one
that is a best effort would be the wrong way round.

---

## Divergences from the reviewed product

Recorded rather than glossed over.

| Area | Reviewed product | Here | Why |
| --- | --- | --- | --- |
| **Platform** | macOS Apple Silicon only | macOS, Linux and Windows, all three built/linted/tested in CI | The core has no macOS-specific dependency; restricting it would be a choice, not a constraint. Path tables for macOS and Windows are inferred from each tool's conventions and have not yet been confirmed against a real install. |
| **Semantic model** | Shipped static embedding model | Trained locally from the user's corpus, hashed fallback until it can be | Avoids downloading a model, and adapts to project-specific vocabulary. Costs a cold-start period and cross-machine reproducibility. |
| **Linux keychain** | n/a | Secret Service | Costs a `libdbus-1-dev` build dependency. The kernel keyring (`keyutils`) avoids it but does not survive a reboot, which would silently regenerate the key and leave the existing index permanently unopenable — not a trade worth making. Headless machines supply `INVENTORY_INDEX_KEY`. |
| **Update transport** | Built in, auto-installing | Policy only; transport is a trait | Keeps the core provably network-free. Auto-install is a packaging concern. |
| **macOS transparency** | n/a | `macos-private-api` enabled | `WebviewWindowBuilder::transparent` does not exist on macOS without it, and the panel's translucent blurred card is its visual identity. It rules out Mac App Store distribution — not a constraint for a product sold direct, which this one is. Unconditional on Windows and Linux, which is why a Linux-only build never surfaced it. |
| **Clip source app** | Tagged with the app | Tagged on macOS via `osascript`; untagged elsewhere | No cross-platform way to get the frontmost app without extra permissions. Untagged beats guessed. |
| **Licensing** | Machine-bound, activated online once | Not implemented | Nothing to activate against. `inv palette` reports the licence line the product shows. |
| **Zed compressed rows** | Presumably decompressed | Skipped | Zed has stored the thread blob compressed in some versions; a row that will not decode is skipped and the rest of the table still indexes. Adding zstd is a small, isolated change. |

## Testing

74 tests, no network, no fixtures checked in as binaries.

- **Unit tests** cover each parser against realistic records, the timestamp and
  calendar round-trip, FTS query escaping, the linear algebra against known
  spectra, encryption round-trip and key mismatch, and version comparison.
- **`tests/end_to_end.rs`** builds a fixture machine with all six tools
  installed and drives the real pipeline: index all six, search across them,
  confirm re-indexing is idempotent, break a source and prove it freezes
  *without losing history* and then repairs itself, and exercise capture,
  scratchpad, resume, primer and retention.
- **Vector tests** cover the guarantees rather than the implementation: the
  index agrees with the exact scan on ≥90% of top hits and ≥85% of top-10,
  every hit it returns is scored exactly, a stale index is ignored, and a
  filter that guts the shortlist falls back rather than returning a short page.
- **Watcher tests** run against real files without a fixture machine, via
  `poll_paths_at`, and cover the debounce directly: a file being written is
  deferred until it settles, a growing file stays deferred across ticks, the
  backlog settles immediately, and priming suppresses the initial burst.
- The semantic capability has a dedicated test
  (`lsa_relates_words_that_share_context`) proving two words that never
  co-occur end up close when they share context.

`docs/make_fixture.py` builds a larger fixture for manual exercise; see the
README.
