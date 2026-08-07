//! Resolving a conversation to the repository and the files it touched.
//!
//! Every source records some notion of where a conversation was happening —
//! `Conversation::project_path`. On its own that is a string with a machine's
//! home directory baked into it, which is useless as a join key the moment a
//! clone moves. This module turns it into two durable things: a repository
//! identity that survives the move, and the set of repo-relative file paths
//! the conversation actually talked about.
//!
//! Git is read directly rather than through a library or the `git` binary.
//! `.git/config` is an INI file and the only field wanted from it is a remote
//! URL; linking libgit2 to read one line would put a C toolchain back into a
//! dependency graph that [`crate::db`] went out of its way to keep pure Rust.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// A conversation mentioning more paths than this is a transcript that pasted
/// a directory listing, not one that worked on a thousand files. The cap keeps
/// one pathological conversation from dominating the file table.
const MAX_FILES_PER_CONVERSATION: usize = 200;

/// How a conversation came to be attached to a repository. Surfaced rather
/// than hidden because the two are not equally trustworthy, and a caller
/// ranking results deserves to know which it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    /// The source recorded a working directory and it resolved to a repo.
    Recorded,
    /// No usable working directory; the repo was inferred from file paths in
    /// the transcript.
    Inferred,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Recorded => "recorded",
            Origin::Inferred => "inferred",
        }
    }

    pub fn parse(s: &str) -> Origin {
        match s {
            "inferred" => Origin::Inferred,
            _ => Origin::Recorded,
        }
    }
}

/// A repository identity.
///
/// `key` is what conversations are grouped by, and is deliberately *not* the
/// local path: the same repository cloned to `~/src/api` on one machine and
/// `~/work/api` on another — or simply moved — has to land in one bucket. A
/// remote URL is the only identifier the repository carries with it, so it is
/// preferred; a repo with no remote falls back to its own path, which at least
/// groups correctly on the machine where the work happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub key: String,
    /// Where this repo was last seen locally. May no longer exist.
    pub root: PathBuf,
    pub remote: Option<String>,
    /// Last path component of the root, or the remote's repo name.
    pub name: String,
}

/// Resolve a source-recorded working directory to a repository.
///
/// Returns `None` only for an empty path. A directory that is not in a git
/// repository still yields a `RepoRef` keyed on its own path — grouping
/// conversations by project is useful whether or not the project is versioned,
/// and pretending a non-git project does not exist would silently drop it from
/// every query in this module.
pub fn resolve(project_path: &str) -> Option<RepoRef> {
    let trimmed = project_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if let Some(found) = discover(&path) {
        return Some(found);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| trimmed.to_string());
    Some(RepoRef {
        key: format!("path:{}", normalize_path_key(&path)),
        root: path,
        remote: None,
        name,
    })
}

/// Walk up from `start` looking for a `.git`, and describe the repository if
/// one is found.
pub fn discover(start: &Path) -> Option<RepoRef> {
    let mut dir: &Path = start;
    // A file path (or a path that no longer exists) is treated as a location
    // inside a directory, not as one.
    if dir.is_file() {
        dir = dir.parent()?;
    }
    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            let remote = git_dir(&dot_git)
                .and_then(|g| std::fs::read_to_string(g.join("config")).ok())
                .and_then(|cfg| origin_url(&cfg))
                .map(|u| normalize_remote(&u));
            let name = remote
                .as_deref()
                .and_then(repo_name_from_remote)
                .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| dir.display().to_string());
            let key = match &remote {
                Some(r) => format!("git:{r}"),
                None => format!("path:{}", normalize_path_key(dir)),
            };
            return Some(RepoRef {
                key,
                root: dir.to_path_buf(),
                remote,
                name,
            });
        }
        dir = dir.parent()?;
    }
}

/// The directory holding `config` for a given `.git`.
///
/// Usually `.git` itself. For a worktree or submodule it is a file containing
/// `gitdir: <path>`, and for a worktree that gitdir is per-worktree state
/// whose `commondir` points at the real one — which is where `config` lives.
fn git_dir(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let contents = std::fs::read_to_string(dot_git).ok()?;
    let target = contents.strip_prefix("gitdir:")?.trim();
    let gitdir = {
        let p = PathBuf::from(target);
        if p.is_absolute() {
            p
        } else {
            dot_git.parent()?.join(p)
        }
    };
    if gitdir.join("config").exists() {
        return Some(gitdir);
    }
    // A linked worktree: `commondir` is relative to the per-worktree gitdir.
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = common.trim();
    let resolved = {
        let p = PathBuf::from(common);
        if p.is_absolute() {
            p
        } else {
            gitdir.join(p)
        }
    };
    Some(resolved)
}

/// Pull `url` out of the `[remote "origin"]` section of a git config.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let header: String = line
                .trim_matches(|c| c == '[' || c == ']')
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            in_origin = header.eq_ignore_ascii_case("remote \"origin\"");
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("url") {
                let v = value.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Reduce the several ways of writing the same remote to one string.
///
/// `git@github.com:o/r.git`, `https://github.com/o/r.git` and
/// `ssh://git@github.com/o/r` all describe one repository and must produce one
/// key. Case is folded because hosts are case-insensitive and treating
/// `Owner/Repo` as distinct from `owner/repo` would split a repo in two more
/// often than it would correctly separate one.
pub fn normalize_remote(url: &str) -> String {
    let mut s = url.trim().to_string();

    if let Some(rest) = s.split_once("://").map(|(_, r)| r.to_string()) {
        s = rest;
    }
    // `user@host:path` — the scp-like form. Only the first colon separates
    // host from path, and a port (`host:22/path`) is not that.
    if let Some((_, rest)) = s.split_once('@') {
        s = rest.to_string();
    }
    if let Some((host, path)) = s.split_once(':') {
        let path = path.trim_start_matches('/');
        s = format!("{host}/{path}");
    }

    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    s.trim_end_matches('/').to_ascii_lowercase()
}

fn repo_name_from_remote(remote: &str) -> Option<String> {
    remote
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

/// A path reduced to a stable string for use in a key. Not canonicalised —
/// that would touch the filesystem and fail for a repo that has since moved.
fn normalize_path_key(path: &Path) -> String {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    let s = out.to_string_lossy().replace('\\', "/");
    s.trim_end_matches('/').to_string()
}

/// Reconstruct a repository from absolute paths mentioned in a transcript,
/// for conversations whose source recorded no working directory.
///
/// Transcripts are full of absolute paths — tool output, error messages,
/// stack traces — and each one is a candidate location to walk up from. The
/// repository named most often wins, which keeps a passing mention of some
/// other checkout from outvoting the one the conversation was about.
///
/// This only ever finds repositories that still exist on disk. A conversation
/// about a repo that has since been deleted stays unattached, which is the
/// right outcome: the alternative is inventing a location that cannot be
/// checked.
pub fn infer(body: &str) -> Option<RepoRef> {
    let mut votes: HashMap<String, (RepoRef, usize)> = HashMap::new();
    let mut seen_dirs: HashMap<PathBuf, Option<RepoRef>> = HashMap::new();

    for raw in body.split_whitespace() {
        let Some(token) = clean_token(raw) else {
            continue;
        };
        let path = PathBuf::from(&token);
        if !path.is_absolute() || token.contains("://") {
            continue;
        }
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            continue;
        };
        // Walking up the tree hits the filesystem, and transcripts repeat the
        // same directory hundreds of times; resolve each one once.
        let found = seen_dirs
            .entry(dir.clone())
            .or_insert_with(|| discover(&dir))
            .clone();
        if let Some(repo) = found {
            let entry = votes.entry(repo.key.clone()).or_insert((repo, 0));
            entry.1 += 1;
        }
    }

    votes
        .into_values()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.key.cmp(&a.0.key)))
        .map(|(repo, _)| repo)
}

/// File extensions that make a bare token credible as a source path even when
/// the file cannot be found on disk — because the repo moved, or the file was
/// since deleted or renamed, which is exactly the history worth keeping.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs",
    "toml",
    "lock",
    "go",
    "mod",
    "py",
    "pyi",
    "rb",
    "java",
    "kt",
    "kts",
    "scala",
    "swift",
    "m",
    "mm",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "cs",
    "js",
    "jsx",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "vue",
    "svelte",
    "php",
    "pl",
    "lua",
    "ex",
    "exs",
    "erl",
    "hs",
    "ml",
    "clj",
    "cljs",
    "dart",
    "zig",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "sql",
    "graphql",
    "proto",
    "json",
    "yaml",
    "yml",
    "xml",
    "html",
    "css",
    "scss",
    "sass",
    "less",
    "md",
    "mdx",
    "rst",
    "txt",
    "cfg",
    "ini",
    "conf",
    "env",
    "gradle",
    "bzl",
    "bazel",
    "cmake",
    "mk",
    "dockerfile",
    "tf",
    "tfvars",
    "nix",
    "vim",
    "el",
];

/// Files with no extension that are still worth indexing by name.
const EXTENSIONLESS_FILES: &[&str] = &[
    "Makefile",
    "Dockerfile",
    "Rakefile",
    "Gemfile",
    "Procfile",
    "Justfile",
    "BUILD",
    "WORKSPACE",
    "CMakeLists.txt",
];

/// Extract the repo-relative file paths a transcript mentions, with how often
/// each was mentioned.
///
/// This is a heuristic over prose, and it is worth being plain about why.
/// The tool-call arguments in these transcripts carry exact paths, but the
/// source parsers flatten every message to text before the indexer sees it, so
/// that structure is already gone by this point. What is left is scanning for
/// tokens that look like paths and keeping the credible ones: anything that
/// resolves under `root` on disk is taken, and anything else needs a
/// recognisable source extension to qualify.
///
/// Ordered by mention count, so a caller truncating the list keeps the files
/// the conversation was actually about rather than ones it mentioned in
/// passing.
pub fn extract_paths(body: &str, root: Option<&Path>) -> Vec<(String, i64)> {
    let mut counts: HashMap<String, i64> = HashMap::new();

    for raw in body.split_whitespace() {
        let Some(candidate) = clean_token(raw) else {
            continue;
        };
        let Some(path) = qualify(&candidate, root) else {
            continue;
        };
        *counts.entry(path).or_insert(0) += 1;
    }

    let mut out: Vec<(String, i64)> = counts.into_iter().collect();
    // Count first, then path, so the result is stable across runs — a HashMap
    // iteration order leaking into the index would make re-indexing produce
    // gratuitously different rows.
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(MAX_FILES_PER_CONVERSATION);
    out
}

/// Strip the punctuation a path picks up from being written in prose or
/// markdown, plus any trailing `:line:col`.
fn clean_token(raw: &str) -> Option<String> {
    // Wrappers and sentence punctuation interleave — `(src/lib.rs).` ends
    // with a period *inside* a paren — so each pass can expose more for the
    // other, and stripping stops only when a pass changes nothing.
    let mut cut = raw;
    loop {
        let before = cut;
        cut = cut.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | '*'
            ) || c.is_whitespace()
        });
        cut = cut.trim_end_matches(['.', ':', '!', '?']);
        if cut == before {
            break;
        }
    }
    let cut = strip_line_suffix(cut);
    if cut.is_empty() {
        return None;
    }
    Some(cut.to_string())
}

/// `src/main.rs:42` and `src/main.rs:42:9` both name `src/main.rs`.
fn strip_line_suffix(token: &str) -> &str {
    let mut cut = token;
    for _ in 0..2 {
        let Some((head, tail)) = cut.rsplit_once(':') else {
            break;
        };
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        cut = head;
    }
    cut
}

/// Decide whether a cleaned token is a repo file, and return it repo-relative.
fn qualify(token: &str, root: Option<&Path>) -> Option<String> {
    // URLs contain slashes and dots and are otherwise extremely path-shaped.
    if token.contains("://") || token.starts_with('-') || token.starts_with('@') {
        return None;
    }

    let path = Path::new(token);
    let relative: String = if path.is_absolute() {
        // An absolute path is only meaningful if it is inside the repository
        // being indexed. Without a root there is nothing to measure it
        // against, and keeping it would store another machine's directory
        // layout as though it were a repo path.
        let root = root?;
        let stripped = path.strip_prefix(root).ok()?;
        normalize_path_key(stripped)
    } else {
        normalize_path_key(path)
    };

    if relative.is_empty() || relative.starts_with("..") {
        return None;
    }

    let name = relative.rsplit('/').next().unwrap_or(&relative);
    let recognised = EXTENSIONLESS_FILES.iter().any(|f| f == &name)
        || name
            .rsplit_once('.')
            .map(|(stem, ext)| {
                !stem.is_empty() && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
            })
            .unwrap_or(false);

    let on_disk = root.map(|r| r.join(&relative).exists()).unwrap_or(false);

    if !recognised && !on_disk {
        return None;
    }
    // A bare filename with no directory is only credible when it can be seen
    // on disk; otherwise every mention of `index.js` in any conversation would
    // collapse into one file that may not exist in this repository at all.
    if !relative.contains('/') && !on_disk && !EXTENSIONLESS_FILES.iter().any(|f| f == &name) {
        return None;
    }

    Some(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_in_every_form_normalise_to_one_key() {
        let expected = "github.com/owner/repo";
        for url in [
            "git@github.com:owner/repo.git",
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "ssh://git@github.com/owner/repo.git",
            "git@github.com:Owner/Repo.git",
            "https://github.com/owner/repo/",
        ] {
            assert_eq!(normalize_remote(url), expected, "failed on {url}");
        }
    }

    #[test]
    fn origin_is_read_out_of_a_real_config_shape() {
        let cfg = r#"
[core]
	repositoryformatversion = 0
[remote "upstream"]
	url = https://example.com/not-this.git
[remote "origin"]
	url = git@github.com:owner/repo.git
	fetch = +refs/heads/*:refs/remotes/origin/*
[branch "main"]
	remote = origin
"#;
        assert_eq!(
            origin_url(cfg).as_deref(),
            Some("git@github.com:owner/repo.git")
        );
    }

    #[test]
    fn a_config_without_origin_yields_nothing() {
        assert!(origin_url("[core]\n\tbare = false\n").is_none());
    }

    #[test]
    fn line_and_column_suffixes_are_stripped() {
        assert_eq!(strip_line_suffix("src/main.rs:42"), "src/main.rs");
        assert_eq!(strip_line_suffix("src/main.rs:42:9"), "src/main.rs");
        assert_eq!(strip_line_suffix("src/main.rs"), "src/main.rs");
        // Not a line number, so not stripped.
        assert_eq!(strip_line_suffix("C:/tmp/x.rs"), "C:/tmp/x.rs");
    }

    #[test]
    fn paths_are_pulled_out_of_prose_and_markdown() {
        let body = "I edited `src/main.rs` and then src/db.rs:14, plus (crates/core/lib.rs).";
        let found: Vec<String> = extract_paths(body, None)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(found.contains(&"src/main.rs".to_string()), "{found:?}");
        assert!(found.contains(&"src/db.rs".to_string()), "{found:?}");
        assert!(
            found.contains(&"crates/core/lib.rs".to_string()),
            "{found:?}"
        );
    }

    #[test]
    fn urls_and_bare_filenames_are_not_mistaken_for_repo_files() {
        let body = "see https://example.com/docs/index.html and also index.html generally";
        assert!(
            extract_paths(body, None).is_empty(),
            "{:?}",
            extract_paths(body, None)
        );
    }

    #[test]
    fn mention_counts_order_the_result() {
        let body = "src/a.rs src/b.rs src/a.rs src/a.rs src/b.rs src/c.rs";
        let found = extract_paths(body, None);
        assert_eq!(found[0], ("src/a.rs".to_string(), 3));
        assert_eq!(found[1], ("src/b.rs".to_string(), 2));
        assert_eq!(found[2], ("src/c.rs".to_string(), 1));
    }

    #[test]
    fn absolute_paths_are_relativised_against_the_repo_and_otherwise_dropped() {
        let root = Path::new("/home/dev/api");
        assert_eq!(
            qualify("/home/dev/api/src/main.rs", Some(root)).as_deref(),
            Some("src/main.rs")
        );
        assert!(qualify("/etc/passwd", Some(root)).is_none());
        // No root to measure against.
        assert!(qualify("/home/dev/api/src/main.rs", None).is_none());
    }

    #[test]
    fn a_repo_is_discovered_by_walking_up_and_keyed_on_its_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n",
        )
        .unwrap();
        let nested = root.join("crates/core/src");
        std::fs::create_dir_all(&nested).unwrap();

        let found = discover(&nested).expect("should find the repo from a nested directory");
        assert_eq!(found.key, "git:github.com/owner/repo");
        assert_eq!(found.name, "repo");
        assert_eq!(found.root, root);
    }

    #[test]
    fn a_repo_without_a_remote_falls_back_to_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\n\tbare = false\n").unwrap();

        let found = discover(dir.path()).unwrap();
        assert!(found.key.starts_with("path:"), "{}", found.key);
        assert!(found.remote.is_none());
    }

    /// A worktree's `.git` is a file pointing at per-worktree state, and the
    /// config it needs lives in the common dir that state points back to.
    #[test]
    fn a_worktree_resolves_to_the_main_repos_config() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main/.git");
        std::fs::create_dir_all(main.join("worktrees/feature")).unwrap();
        std::fs::write(
            main.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/owner/repo.git\n",
        )
        .unwrap();
        std::fs::write(main.join("worktrees/feature/commondir"), "../..\n").unwrap();

        let wt = dir.path().join("feature");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join("worktrees/feature").display()),
        )
        .unwrap();

        let found = discover(&wt).expect("worktree should resolve");
        assert_eq!(found.key, "git:github.com/owner/repo");
    }

    /// A project that is not a git repository still has to group, or every
    /// query in this module silently ignores it.
    #[test]
    fn a_non_git_project_still_resolves_by_path() {
        let found = resolve("/home/dev/scratch-project").unwrap();
        assert_eq!(found.key, "path:/home/dev/scratch-project");
        assert_eq!(found.name, "scratch-project");
        assert!(resolve("   ").is_none());
    }
}
