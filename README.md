# ffs (fast-file-suggestion)

Sub-millisecond file suggestion with typo tolerance.

## Why

You're in Claude Code, working on a large codebase. You type `@authservice` to reference a file — nothing. Try `@AuthServce` (typo) — still nothing. Traditional tools like `find` and glob patterns choke on 200k+ file repositories, taking seconds when you need milliseconds.

**ffs** fixes this:
- **Blazingly fast** — sub-millisecond responses even on massive codebases (200k+ files)
- **Typo-tolerant** — `scaner` finds `scanner`, `authservce` finds `AuthService`
- **Zero friction** — indexes automatically, respects `.gitignore`, just works

Built for AI coding assistants and autocomplete systems that can't afford to wait.

## What it does

Indexes your project files in SQLite and provides blazingly fast search with fuzzy matching. Searches 200k+ files in under 1ms (warm) with support for typos like "scaner" → "scanner".

## Install

To use ffs for faster file suggestion in Claude Code:

**1. Build the binary:**
```bash
cargo build --release
cp ./target/release/ffs ~/.claude/bin/
```

**2. Configure Claude Code** — add to `~/.claude/settings.json`:
```json
{
  "fileSuggestion": {
    "type": "command",
    "command": "~/.claude/bin/ffs"
  }
}
```

That's it. Open a new Claude Code session for the settings to take effect, then `@filename` autocomplete will be powered by ffs.

## Usage

After installation, just type `@` followed by a filename in Claude Code:

```
@authservice    → finds AuthenticationService.ts
@config         → finds config.json, configuration.ts, etc.
@scaner         → finds scanner.rs (typo-tolerant)
```

ffs handles indexing automatically. The first search takes a few seconds to build the index, then subsequent searches are sub-millisecond.

## How it works

**3-tier search with graceful fallback:**

1. **FTS5** - SQLite full-text search with BM25 ranking. Handles exact and prefix matches in <1ms.

2. **LIKE** - Substring matching. Catches cases the FTS5 tokenizer misses.

3. **Trigram** - Fuzzy matching via 3-character substrings. Finds files even with typos.

95% of queries resolve in tier 1. Only typo queries fall through to slower tiers.

**Index caching:**
- Stored in `~/.claude/cache/file-index/`
- Auto-rebuilds after 5 minutes
- Respects `.gitignore`

## Technical Details

For those curious about why ffs is fast:

- **Pre-computed trigram index** — Trigrams are generated at build time and stored in SQLite with an index, not computed at query time
- **Per-project isolation** — Each project gets its own index file via path hashing, no cross-contamination
- **Parallel file walking** — Uses the `ignore` crate (ripgrep's engine) with multi-threaded directory traversal
- **Proper gitignore support** — Handles `.gitignore`, global gitignore, and `.git/info/exclude` correctly
- **Atomic index rebuilds** — Builds to temp file, then renames, so queries never see partial indexes
- **Concurrent-safe locking** — mkdir-based locks prevent multiple processes from rebuilding simultaneously
- **Zero external dependencies** — SQLite is bundled, no system libraries required

## Safety

ffs includes guards to prevent Claude from accidentally accessing files outside your project:

- **Blocked paths** — Won't index system directories (`/`, `/Users`, `/home`, `/tmp`, `/etc`, etc.) so Claude can't suggest files from there
- **Minimum path length** — Rejects paths shorter than 10 characters to catch misconfigurations
- **Absolute paths only** — Prevents ambiguous paths that could resolve to unintended locations
- **SQL injection prevention** — Queries sanitized to alphanumeric + `._-/` only
- **Silent failure** — Returns empty results on errors, never exposes internals

<details>
<summary><strong>API Reference</strong> (for building tools on top of ffs)</summary>

### CLI Interface

```bash
# Set the project directory to index
export CLAUDE_PROJECT_DIR=/path/to/your/project

# Search for files
echo '{"query": "main", "limit": 10}' | ffs

# Output: newline-separated file paths
src/main.rs
tests/main_test.rs
```

### Input (stdin)

JSON object with:
- `query` - Search string (optional, empty returns shallowest files)
- `limit` - Max results (optional, default 200, max 500)

### Output (stdout)

Newline-separated file paths, relative to project root.

</details>

## License

Apache-2.0
