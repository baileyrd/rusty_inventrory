//! Hybrid search: BM25 keyword retrieval and on-device semantic retrieval,
//! fused by Reciprocal Rank Fusion.
//!
//! RRF is used rather than a weighted score blend because BM25 scores and
//! cosine similarities are not on comparable scales and their distributions
//! shift with the query. Fusing *ranks* sidesteps the calibration problem
//! entirely, which is why it holds up across a corpus as heterogeneous as six
//! different tools' transcripts.
//!
//! A hit found only by the semantic side is labelled, because "a result with
//! none of your words otherwise looks like a bug".

use crate::embed::Embedder;
use crate::model::{Conversation, SourceId};
use crate::vectors::VectorCache;
use crate::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// RRF's smoothing constant. 60 is the value from the original paper and is
/// what keeps a single list's top hit from dominating the fusion.
const RRF_K: f64 = 60.0;
const W_KEYWORD: f64 = 1.0;
const W_SEMANTIC: f64 = 1.0;
/// Recency enters as a third ranked list rather than a score multiplier, so
/// it can nudge ordering without ever swamping relevance.
const W_RECENCY: f64 = 0.35;

/// How deep each retrieval arm goes before fusion.
const CANDIDATE_DEPTH: usize = 200;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    /// Empty means every source.
    pub sources: Vec<SourceId>,
    pub limit: usize,
    /// The ⌘M toggle. When off, this is pure keyword search.
    pub meaning: bool,
    /// Unix-seconds lower bound on `updated_at`.
    pub since: Option<i64>,
    /// Restrict to one repository, by `repos.key` or by name.
    pub repo: Option<String>,
    /// Restrict to conversations that touched this repo-relative path.
    pub file: Option<String>,
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        SearchQuery {
            text: text.into(),
            sources: Vec::new(),
            limit: 20,
            meaning: true,
            since: None,
            repo: None,
            file: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchedVia {
    /// The query's words are in this conversation.
    Keyword,
    /// Retrieved only by the embedding — none of the query's words are here.
    Meaning,
    Both,
}

impl MatchedVia {
    pub fn label(self) -> &'static str {
        match self {
            MatchedVia::Keyword => "keyword",
            MatchedVia::Meaning => "meaning",
            MatchedVia::Both => "keyword+meaning",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub conversation: Conversation,
    pub score: f64,
    /// Matched text with the query's words wrapped in `[` `]`. Empty when the
    /// hit came from the semantic arm alone.
    pub snippet: String,
    pub matched_via: MatchedVia,
    pub keyword_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    /// Name of the embedding model that ran.
    pub semantic_model: String,
    /// False when only the lexical fallback is available, in which case a
    /// "meaning" label would be overclaiming.
    pub semantic_available: bool,
    pub total_candidates: usize,
}

/// Turn user input into an FTS5 MATCH expression.
///
/// Every token is quoted (so `:`, `-`, `*` and friends in a query cannot be
/// read as FTS operators) and prefix-matched, then ANDed.
pub fn build_match_expression(text: &str, conjunctive: bool) -> Option<String> {
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.join(if conjunctive { " AND " } else { " OR " }))
}

fn source_filter(sources: &[SourceId]) -> String {
    if sources.is_empty() {
        return String::new();
    }
    let list = sources
        .iter()
        .map(|s| format!("'{}'", s.slug()))
        .collect::<Vec<_>>()
        .join(",");
    format!(" AND c.source IN ({list})")
}

/// The non-text filters, as a SQL fragment plus the values it binds.
///
/// Source slugs come from a closed enum and are inlined; the repo and file
/// names are user input and are bound, starting at `?{first}`. Both are
/// `EXISTS` subqueries rather than joins so a conversation touching a file
/// several times still produces one row.
fn scope_clause(query: &SearchQuery, first: usize) -> (String, Vec<String>) {
    let mut sql = source_filter(&query.sources);
    let mut values = Vec::new();
    let mut i = first;

    if let Some(repo) = &query.repo {
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM conversation_repo cr
                            JOIN repos r ON r.id = cr.repo_id
                           WHERE cr.conversation_id = c.id
                             AND (r.key = ?{i} OR r.name = ?{i}))"
        ));
        values.push(repo.clone());
        i += 1;
    }
    if let Some(file) = &query.file {
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM conversation_file cf
                           WHERE cf.conversation_id = c.id AND cf.path = ?{i})"
        ));
        values.push(file.clone());
    }
    (sql, values)
}

fn keyword_search(conn: &Connection, query: &SearchQuery) -> Result<Vec<(i64, String)>> {
    let run = |conjunctive: bool| -> Result<Vec<(i64, String)>> {
        let Some(expr) = build_match_expression(&query.text, conjunctive) else {
            return Ok(Vec::new());
        };
        let (scope, scope_values) = scope_clause(query, 4);
        let sql = format!(
            "SELECT c.id,
                    snippet(conversations_fts, 1, '[', ']', '…', 14) AS snip
             FROM conversations_fts f
             JOIN conversations c ON c.id = f.rowid
             WHERE conversations_fts MATCH ?1
               AND (?2 = 0 OR c.updated_at >= ?2){scope}
             ORDER BY bm25(conversations_fts, 4.0, 1.0)
             LIMIT ?3"
        );
        let since = query.since.unwrap_or(0);
        let depth = CANDIDATE_DEPTH as i64;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&expr, &since, &depth];
        params.extend(scope_values.iter().map(|v| v as &dyn rusqlite::ToSql));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.flatten().collect())
    };

    // Precision first; if every word together finds nothing, widen to any word
    // rather than showing an empty page.
    let strict = run(true)?;
    if strict.is_empty() {
        run(false)
    } else {
        Ok(strict)
    }
}

/// Conversations a source/recency filter permits, or `None` when everything
/// is permitted — which is the common case, and skips the query entirely.
fn allowed_ids(conn: &Connection, query: &SearchQuery) -> Result<Option<HashSet<i64>>> {
    if query.sources.is_empty()
        && query.since.is_none()
        && query.repo.is_none()
        && query.file.is_none()
    {
        return Ok(None);
    }
    let (scope, scope_values) = scope_clause(query, 2);
    let sql =
        format!("SELECT c.id FROM conversations c WHERE (?1 = 0 OR c.updated_at >= ?1){scope}");
    let since = query.since.unwrap_or(0);
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&since];
    params.extend(scope_values.iter().map(|v| v as &dyn rusqlite::ToSql));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| r.get::<_, i64>(0))?;
    Ok(Some(rows.flatten().collect()))
}

fn semantic_search(
    conn: &Connection,
    cache: &VectorCache,
    embedder: &dyn Embedder,
    query: &SearchQuery,
) -> Result<Vec<i64>> {
    let q = embedder.embed(&query.text);
    // An all-zero vector is the embedder abstaining: nothing in the query was
    // in its vocabulary. Scoring it would rank everything equally-unrelated.
    if q.iter().all(|x| *x == 0.0) {
        return Ok(Vec::new());
    }

    let allow = allowed_ids(conn, query)?;
    // A near-zero cosine is noise, not a match; including it would put
    // unrelated conversations under a "found by meaning" label.
    let hits = cache.search(&q, CANDIDATE_DEPTH, allow.as_ref(), 0.05);
    Ok(hits.into_iter().map(|(id, _)| id).collect())
}

pub fn search(
    conn: &Connection,
    cache: &VectorCache,
    embedder: &dyn Embedder,
    query: &SearchQuery,
) -> Result<SearchResponse> {
    if query.text.trim().is_empty() {
        return Ok(SearchResponse {
            hits: Vec::new(),
            semantic_model: embedder.name().to_string(),
            semantic_available: embedder.is_semantic(),
            total_candidates: 0,
        });
    }

    let keyword = keyword_search(conn, query)?;
    let semantic = if query.meaning {
        semantic_search(conn, cache, embedder, query)?
    } else {
        Vec::new()
    };

    // Fuse.
    let mut scores: HashMap<i64, f64> = HashMap::new();
    let mut kw_rank: HashMap<i64, usize> = HashMap::new();
    let mut sem_rank: HashMap<i64, usize> = HashMap::new();
    let mut snippets: HashMap<i64, String> = HashMap::new();

    for (i, (id, snip)) in keyword.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += W_KEYWORD / (RRF_K + (i + 1) as f64);
        kw_rank.insert(*id, i + 1);
        snippets.insert(*id, snip.clone());
    }
    for (i, id) in semantic.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += W_SEMANTIC / (RRF_K + (i + 1) as f64);
        sem_rank.insert(*id, i + 1);
    }
    if scores.is_empty() {
        return Ok(SearchResponse {
            hits: Vec::new(),
            semantic_model: embedder.name().to_string(),
            semantic_available: embedder.is_semantic(),
            total_candidates: 0,
        });
    }

    let total_candidates = scores.len();
    let ids: Vec<i64> = scores.keys().copied().collect();
    let conversations = load_conversations(conn, &ids)?;

    // Recency as a third list over the candidates already retrieved.
    let mut by_recency: Vec<i64> = ids.clone();
    by_recency.sort_by_key(|id| {
        std::cmp::Reverse(conversations.get(id).map(|c| c.updated_at).unwrap_or(0))
    });
    for (i, id) in by_recency.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += W_RECENCY / (RRF_K + (i + 1) as f64);
    }

    let mut hits: Vec<SearchHit> = scores
        .into_iter()
        .filter_map(|(id, score)| {
            let conversation = conversations.get(&id)?.clone();
            let (k, s) = (kw_rank.get(&id).copied(), sem_rank.get(&id).copied());
            let matched_via = match (k.is_some(), s.is_some()) {
                (true, true) => MatchedVia::Both,
                (true, false) => MatchedVia::Keyword,
                (false, true) => MatchedVia::Meaning,
                (false, false) => return None,
            };
            Some(SearchHit {
                conversation,
                score,
                snippet: snippets.get(&id).cloned().unwrap_or_default(),
                matched_via,
                keyword_rank: k,
                semantic_rank: s,
            })
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.conversation.updated_at.cmp(&a.conversation.updated_at))
    });
    hits.truncate(query.limit);

    // A semantic-only hit has no keyword snippet; show the conversation's
    // opening instead so the row is not blank.
    for hit in hits.iter_mut() {
        if hit.snippet.is_empty() {
            hit.snippet = opening_line(conn, hit.conversation.id)?;
        }
    }

    Ok(SearchResponse {
        hits,
        semantic_model: embedder.name().to_string(),
        semantic_available: embedder.is_semantic(),
        total_candidates,
    })
}

fn load_conversations(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, Conversation>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, source, external_id, title, project_path, git_branch,
                started_at, updated_at, message_count
         FROM conversations WHERE id IN ({list})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
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
    })?;
    Ok(rows.flatten().map(|c| (c.id, c)).collect())
}

fn opening_line(conn: &Connection, conversation_id: i64) -> Result<String> {
    let text: Option<String> = conn
        .query_row(
            "SELECT text FROM messages WHERE conversation_id = ?1 ORDER BY seq LIMIT 1",
            [conversation_id],
            |r| r.get(0),
        )
        .ok();
    Ok(text
        .map(|t| {
            let mut s: String = t.chars().take(160).collect();
            if t.chars().count() > 160 {
                s.push('…');
            }
            s.replace('\n', " ")
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_expressions_quote_user_input() {
        assert_eq!(
            build_match_expression("auth middleware", true).unwrap(),
            "\"auth\"* AND \"middleware\"*"
        );
        assert_eq!(
            build_match_expression("auth middleware", false).unwrap(),
            "\"auth\"* OR \"middleware\"*"
        );
        assert!(build_match_expression("   ", true).is_none());
    }

    /// FTS5 operators typed by a user must be treated as text, not syntax.
    #[test]
    fn match_expressions_neutralise_fts_operators() {
        let expr = build_match_expression("NEAR(a b) OR \"x\" -y*", true).unwrap();
        assert!(!expr.contains("NEAR("), "{expr}");
        assert!(expr.starts_with('"'));
        // Every token ends up individually quoted and prefix-matched.
        assert!(expr.contains("\"NEAR\"*"), "{expr}");
        assert!(expr.contains("\"y\"*"), "{expr}");
    }

    #[test]
    fn source_filter_is_empty_for_all_sources() {
        assert_eq!(source_filter(&[]), "");
        assert_eq!(
            source_filter(&[SourceId::Zed, SourceId::Codex]),
            " AND c.source IN ('zed','codex')"
        );
    }
}
