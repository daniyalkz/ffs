# ffs (fast-file-suggestion)

Sub-millisecond file suggestion with typo tolerance.

## Why

You're in a large codebase. You type `@authservice` to reference a file — nothing. Try `@AuthServce` (typo) — still nothing. Traditional tools like `find` and glob patterns choke on 200k+ file repositories, taking seconds when you need milliseconds.

**ffs** fixes this:
- **Blazingly fast** — sub-millisecond responses even on massive codebases (200k+ files)
- **Typo-tolerant** — `scaner` finds `scanner`, `authservce` finds `AuthService`
- **Zero friction** — indexes automatically, respects `.gitignore`, just works

Built for AI coding assistants and autocomplete systems that can't afford to wait.

## What it does

Indexes your project files in SQLite and provides blazingly fast search with fuzzy matching. Searches 200k+ files in under 1ms (warm) with support for typos like "scaner" → "scanner".

## Install

```bash
cargo build --release
cp ./target/release/ffs ~/.claude/bin/
```

Then add to `~/.claude/settings.json`:

```json
{
  "fileSuggestion": {
    "type": "command",
    "command": "~/.claude/bin/ffs"
  }
}
```

Now `@filename` autocomplete in Claude Code uses ffs.

## Usage

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

## Safety

ffs includes guards to prevent Claude from accidentally accessing files outside your project:

- **Blocked paths** — Won't index system directories (`/`, `/Users`, `/home`, `/tmp`, `/etc`, etc.) so Claude can't suggest files from there
- **Minimum path length** — Rejects paths shorter than 10 characters to catch misconfigurations
- **Absolute paths only** — Prevents ambiguous paths that could resolve to unintended locations
- **SQL injection prevention** — Queries sanitized to alphanumeric + `._-/` only
- **Silent failure** — Returns empty results on errors, never exposes internals

## License

Apache-2.0
