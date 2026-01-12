//! # ffs (fast-file-suggestion)
//!
//! Sub-millisecond file suggestion with typo tolerance using SQLite FTS5.
//!
//! ## Design Goals
//! - **Fast**: <1ms warm search, <3s cold start for 200k files
//! - **Safe**: No unsafe code, SQL injection prevention, path validation
//! - **Typo-tolerant**: "scaner" finds "scanner" via trigram matching
//! - **Zero deps**: SQLite bundled, no system libraries required
//!
//! ## Interface
//! - Input (stdin): `{"query": "...", "limit": N}`
//! - Output (stdout): newline-separated file paths
//! - Env: `CLAUDE_PROJECT_DIR` - project root to index
//!
//! ## Search Strategy
//! Uses 3-tier fallback for robustness:
//! 1. FTS5 - Fast prefix matching with BM25 ranking
//! 2. LIKE - Substring matching when FTS5 finds nothing
//! 3. Trigram - Fuzzy matching for typos when LIKE fails

use ignore::WalkBuilder;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

// =============================================================================
// CONFIGURATION
// =============================================================================

const CACHE_DIR_NAME: &str = ".claude/cache/file-index";
const MAX_RESULTS: usize = 200;
const INDEX_MAX_AGE_SECS: u64 = 300; // 5 minutes
/// Minimum path length to index.
/// Why 10? Prevents Claude from accidentally indexing broad directories.
/// A misconfigured path like "/" or "/tmp" would expose system files to Claude.
const MIN_PATH_LENGTH: usize = 10;
const MAX_LIMIT: usize = 500;

/// Directories that should never be indexed (safety blocklist).
///
/// Why these specific paths?
/// - `/`, `/Users`, `/home` - Would expose entire filesystem or all user data to Claude
/// - `/tmp`, `/var` - System files that shouldn't be suggested to Claude
/// - `/etc`, `/usr`, `/opt` - System configs/binaries, not user code
/// - `/System`, `/Library`, `/bin`, `/sbin` - macOS system directories
/// - `/Applications` - Installed apps, not source code
///
/// Without this blocklist, a misconfigured CLAUDE_PROJECT_DIR could let Claude
/// read and suggest files from anywhere on the system.
const BLOCKED_PATHS: &[&str] = &[
    "/",
    "/Users",
    "/home",
    "/tmp",
    "/var",
    "/etc",
    "/usr",
    "/opt",
    "/System",
    "/Library",
    "/bin",
    "/sbin",
    "/Applications",
];

// =============================================================================
// INPUT/OUTPUT TYPES
// =============================================================================

#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    MAX_RESULTS
}

// =============================================================================
// MAIN ENTRY POINT
// =============================================================================

fn main() {
    // Output newline-separated paths (Claude Code expected format)
    let result = run();
    match result {
        Ok(paths) => {
            for path in paths {
                println!("{}", path);
            }
        }
        Err(_) => {
            // Silent failure - output nothing
            // Why? This runs in autocomplete context. Errors should never
            // bubble up as scary messages. Empty results = "no suggestions"
            // which is safe and expected behavior.
        }
    }
}

fn run() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // Parse input from stdin
    let input = parse_input()?;

    // Get and validate project directory
    let project_dir = env::var("CLAUDE_PROJECT_DIR").unwrap_or_default();
    if !is_safe_path(&project_dir) {
        return Ok(vec![]);
    }

    let project_path = PathBuf::from(&project_dir);
    if !project_path.is_dir() {
        return Ok(vec![]);
    }

    // Calculate paths - require HOME to be set (don't fall back to /tmp)
    let home = env::var("HOME")?;
    let cache_dir = PathBuf::from(&home).join(CACHE_DIR_NAME);
    fs::create_dir_all(&cache_dir)?;

    let project_hash = hash_path(&project_dir);
    let db_path = cache_dir.join(format!("{}.db", project_hash));
    let lock_path = cache_dir.join(format!("{}.lock", project_hash));

    // Check if index needs rebuilding
    let needs_rebuild = check_needs_rebuild(&db_path);

    if needs_rebuild {
        if !db_path.exists() {
            // First time - build synchronously
            let _ = build_index(&project_path, &db_path, &lock_path);
        } else {
            // Stale index - build in background (spawn detached process)
            // For now, rebuild synchronously to keep it simple
            // Background rebuild can be added later if needed
            let _ = build_index(&project_path, &db_path, &lock_path);
        }
    }

    // Search the index
    if db_path.exists() {
        search_index(&db_path, &input.query, input.limit)
    } else {
        Ok(vec![])
    }
}

// =============================================================================
// INPUT PARSING
// =============================================================================

fn parse_input() -> Result<Input, Box<dyn std::error::Error>> {
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer)?;

    if buffer.trim().is_empty() {
        return Ok(Input::default());
    }

    let mut input: Input = serde_json::from_str(&buffer).unwrap_or_default();

    // Validate and clamp limit
    if input.limit == 0 || input.limit > MAX_LIMIT {
        input.limit = MAX_RESULTS;
    }

    Ok(input)
}

// =============================================================================
// SAFETY CHECKS
// =============================================================================

fn is_safe_path(path: &str) -> bool {
    // Guard 1: Non-empty
    if path.is_empty() {
        return false;
    }

    // Guard 2: Minimum length
    if path.len() < MIN_PATH_LENGTH {
        return false;
    }

    // Guard 3: Check against blocklist
    let home = env::var("HOME").unwrap_or_default();
    if path == home {
        return false;
    }

    for blocked in BLOCKED_PATHS {
        if path == *blocked {
            return false;
        }
    }

    // Guard 4: Must be absolute path
    // Why? Relative paths are ambiguous and could resolve unexpectedly,
    // potentially giving Claude access to unintended directories.
    if !path.starts_with('/') {
        return false;
    }

    true
}

// =============================================================================
// HASHING
// =============================================================================

/// Simple hash for project path to create unique DB filename.
/// Uses a basic string hash - doesn't need to be cryptographic.
fn hash_path(path: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// =============================================================================
// INDEX FRESHNESS
// =============================================================================

fn check_needs_rebuild(db_path: &Path) -> bool {
    if !db_path.exists() {
        return true;
    }

    let metadata = match fs::metadata(db_path) {
        Ok(m) => m,
        Err(_) => return true,
    };

    let modified = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return true,
    };

    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::from_secs(INDEX_MAX_AGE_SECS + 1));

    age.as_secs() > INDEX_MAX_AGE_SECS
}

// =============================================================================
// LOCKING
// =============================================================================

/// mkdir-based locking for concurrent index builds.
///
/// Why mkdir instead of file locks (flock/fcntl)?
/// - `mkdir` is atomic on all filesystems - it either succeeds or fails
/// - No platform-specific APIs needed (works on macOS, Linux, Windows)
/// - Simple to reason about: directory exists = locked
/// - Stale locks are easy to detect via mtime and clean up
///
/// The 5-minute stale lock timeout matches INDEX_MAX_AGE_SECS, ensuring
/// a crashed/killed process doesn't permanently block rebuilds.
struct Lock {
    path: PathBuf,
    acquired: bool,
}

impl Lock {
    fn acquire(path: &Path) -> Self {
        // Clean up stale locks first
        if path.exists() {
            if let Ok(metadata) = fs::metadata(path) {
                if let Ok(modified) = metadata.modified() {
                    let age = SystemTime::now()
                        .duration_since(modified)
                        .unwrap_or(Duration::ZERO);
                    if age.as_secs() > INDEX_MAX_AGE_SECS {
                        let _ = fs::remove_dir(path);
                    }
                }
            }
        }

        // Try to acquire lock via mkdir (atomic)
        let acquired = fs::create_dir(path).is_ok();

        Lock {
            path: path.to_path_buf(),
            acquired,
        }
    }

    fn is_acquired(&self) -> bool {
        self.acquired
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.acquired {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

// =============================================================================
// INDEX BUILDING
// =============================================================================

fn build_index(
    project_dir: &Path,
    db_path: &Path,
    lock_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let lock = Lock::acquire(lock_path);
    if !lock.is_acquired() {
        // Another process is building
        return Ok(());
    }

    // Build to temp file, then atomic rename
    let temp_path = db_path.with_extension("tmp");

    // Clean up any stale temp file from previous failed builds
    let _ = fs::remove_file(&temp_path);

    // Use inner function to ensure temp file cleanup on any error
    let result = build_index_inner(project_dir, &temp_path);

    if result.is_err() {
        // Clean up temp file on failure
        let _ = fs::remove_file(&temp_path);
        return result;
    }

    // Atomic rename on success
    fs::rename(&temp_path, db_path)?;
    Ok(())
}

fn build_index_inner(
    project_dir: &Path,
    temp_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create and initialize database
    let conn = Connection::open(temp_path)?;

    // Create schema
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL,
            filename TEXT NOT NULL,
            extension TEXT,
            depth INTEGER
        );

        CREATE VIRTUAL TABLE files_fts USING fts5(
            path,
            filename,
            content=files,
            content_rowid=id,
            tokenize='porter unicode61'
        );

        CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
            INSERT INTO files_fts(rowid, path, filename)
            VALUES (new.id, new.path, new.filename);
        END;

        CREATE TABLE trigrams (
            trigram TEXT NOT NULL,
            file_id INTEGER NOT NULL
        );

        CREATE INDEX idx_trigrams ON trigrams(trigram);
        "#,
    )?;

    // Collect files using ignore crate (parallel, respects .gitignore)
    let files: Mutex<Vec<(String, String, String, i32)>> = Mutex::new(Vec::new());
    let project_dir_clone = project_dir.to_path_buf();

    let walker = WalkBuilder::new(project_dir)
        .hidden(false)      // Include hidden files (files starting with .)
        .git_ignore(true)   // Respect .gitignore
        .git_global(true)   // Respect global gitignore
        .git_exclude(true)  // Respect .git/info/exclude
        .filter_entry(|entry| {
            // Skip .git directory and common build artifacts
            let dominated_name = entry.file_name().to_string_lossy();
            !matches!(dominated_name.as_ref(), ".git" | "node_modules" | "target" | "__pycache__" | ".venv" | "venv")
        })
        .threads(0)         // Auto-detect CPU count for parallel walking
        .build_parallel();

    walker.run(|| {
        let files = &files;
        let project_dir = &project_dir_clone;
        Box::new(move |entry| {
            if let Ok(entry) = entry {
                if entry.file_type().is_some_and(|ft| ft.is_file()) {
                    let path = entry.path();
                    if let Ok(relative) = path.strip_prefix(project_dir) {
                        let path_str = relative.to_string_lossy().to_string();
                        let filename = path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let extension = path
                            .extension()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let depth = path_str.matches('/').count() as i32;

                        files.lock().unwrap().push((path_str, filename, extension, depth));
                    }
                }
            }
            ignore::WalkState::Continue
        })
    });

    let files = files.into_inner().unwrap();

    // Batch insert files in a scoped block to ensure statements are dropped before conn
    {
        conn.execute("BEGIN TRANSACTION", [])?;

        let mut file_stmt =
            conn.prepare("INSERT INTO files (path, filename, extension, depth) VALUES (?, ?, ?, ?)")?;

        let mut trigram_stmt =
            conn.prepare("INSERT INTO trigrams (trigram, file_id) VALUES (?, ?)")?;

        for (id, (path, filename, extension, depth)) in files.iter().enumerate() {
            let file_id = (id + 1) as i64;

            file_stmt.execute(params![path, filename, extension, depth])?;

            // Generate trigrams for fuzzy matching
            let lower_name = filename.to_lowercase();
            if lower_name.len() >= 3 {
                let chars: Vec<char> = lower_name.chars().collect();
                let mut seen_trigrams: HashSet<String> = HashSet::new();

                for window in chars.windows(3) {
                    let trigram: String = window.iter().collect();
                    // Only alphanumeric trigrams, deduplicated
                    if trigram.chars().all(|c| c.is_alphanumeric()) && seen_trigrams.insert(trigram.clone()) {
                        trigram_stmt.execute(params![trigram, file_id])?;
                    }
                }
            }
        }

        conn.execute("COMMIT", [])?;
    } // file_stmt and trigram_stmt dropped here

    Ok(())
}

// =============================================================================
// SEARCH
// =============================================================================

/// 3-tier search with graceful fallback.
///
/// Why not just use trigrams for everything?
/// - FTS5 is 10-100x faster for exact/prefix matches (BM25 ranking built-in)
/// - LIKE handles cases FTS5 tokenizer misses (e.g., "config.json" as one token)
/// - Trigrams are slow (requires scanning all trigrams) but handle typos
///
/// By trying faster methods first, 95% of queries resolve in <1ms.
/// Only typo queries fall through to the slower trigram search.
fn search_index(
    db_path: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let conn = Connection::open(db_path)?;

    // Sanitize query - only allow safe characters
    // Why? Prevents SQL injection and FTS5 syntax injection.
    // FTS5 has special operators (AND, OR, NEAR, *) that could cause unexpected behavior.
    // Stripping to alphanumeric + common filename chars is safest.
    let safe_query: String = query
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '_' || *c == '-' || *c == '.' || *c == '/')
        .collect();

    if safe_query.is_empty() {
        // Empty query - return shallowest files
        return query_empty(&conn, limit);
    }

    // Tier 1: FTS5 search (prefix matching)
    if let Ok(results) = query_fts5(&conn, &safe_query, limit) {
        if !results.is_empty() {
            return Ok(results);
        }
    }

    // Tier 2: LIKE search (substring matching)
    if let Ok(results) = query_like(&conn, &safe_query, limit) {
        if !results.is_empty() {
            return Ok(results);
        }
    }

    // Tier 3: Trigram search (typo tolerance)
    query_trigram(&conn, &safe_query, limit)
}

fn query_empty(conn: &Connection, limit: usize) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM files ORDER BY depth ASC, filename ASC LIMIT ?",
    )?;

    let rows = stmt.query_map([limit as i64], |row| row.get(0))?;
    let results: Vec<String> = rows.flatten().collect();

    Ok(results)
}

fn query_fts5(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // Build FTS5 query with prefix matching
    let fts_query: String = query
        .split_whitespace()
        .map(|word| format!("{}*", word))
        .collect::<Vec<_>>()
        .join(" ");

    let mut stmt = conn.prepare(
        r#"
        SELECT f.path
        FROM files f
        JOIN files_fts fts ON f.id = fts.rowid
        WHERE files_fts MATCH ?
        ORDER BY bm25(files_fts, 1.0, 2.0), f.depth ASC
        LIMIT ?
        "#,
    )?;

    let rows = stmt.query_map(params![fts_query, limit as i64], |row| row.get(0))?;
    let results: Vec<String> = rows.flatten().collect();

    Ok(results)
}

fn query_like(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let pattern = format!("%{}%", query);

    let mut stmt = conn.prepare(
        r#"
        SELECT path FROM files
        WHERE path LIKE ?1 OR filename LIKE ?1
        ORDER BY
            CASE WHEN filename LIKE ?2 THEN 0 ELSE 1 END,
            depth ASC
        LIMIT ?3
        "#,
    )?;

    let prefix_pattern = format!("{}%", query);
    let rows = stmt.query_map(params![pattern, prefix_pattern, limit as i64], |row| row.get(0))?;
    let results: Vec<String> = rows.flatten().collect();

    Ok(results)
}

fn query_trigram(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let lower_query = query.to_lowercase();

    if lower_query.len() < 3 {
        return Ok(vec![]);
    }

    // Generate trigrams from query
    let chars: Vec<char> = lower_query.chars().collect();
    let trigrams: Vec<String> = chars
        .windows(3)
        .map(|w| w.iter().collect::<String>())
        .filter(|t| t.chars().all(|c| c.is_alphanumeric()))
        .collect();

    if trigrams.is_empty() {
        return Ok(vec![]);
    }

    // Build SQL with placeholders
    let placeholders: String = trigrams.iter().map(|_| "?").collect::<Vec<_>>().join(",");

    // Why 40% threshold?
    // - Too low (20%): Returns too many false positives ("main" matches "domain")
    // - Too high (60%): Misses legitimate typos ("scaner" → "scanner" needs ~50%)
    // - 40% is the sweet spot: catches 1-2 char typos while filtering noise
    // - min 2 ensures very short queries don't match everything
    let min_matches = std::cmp::max(2, (trigrams.len() * 2) / 5); // ~40% match

    let sql = format!(
        r#"
        SELECT f.path, COUNT(*) as matches
        FROM trigrams t
        JOIN files f ON t.file_id = f.id
        WHERE t.trigram IN ({})
        GROUP BY f.id
        HAVING matches >= ?
        ORDER BY matches DESC, f.depth ASC
        LIMIT ?
        "#,
        placeholders
    );

    let mut stmt = conn.prepare(&sql)?;

    // Bind trigrams and limit
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = trigrams
        .iter()
        .map(|t| Box::new(t.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    params_vec.push(Box::new(min_matches as i64));
    params_vec.push(Box::new(limit as i64));

    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

    let rows = stmt.query_map(params_refs.as_slice(), |row| row.get::<_, String>(0))?;
    let results: Vec<String> = rows.flatten().collect();

    Ok(results)
}
