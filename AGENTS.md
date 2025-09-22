# Project Overview

Git commit message generator using AI via LiteLLM. Generates conventional commit messages with concise summaries (≤72 chars) and structured detail points from git diffs.

**Three operational modes:**
1. **Standard mode**: Single commit generation from staged/unstaged changes
2. **Compose mode**: AI-powered splitting of large changesets into multiple atomic commits
3. **Rewrite mode**: Batch rewriting of git history to conventional format

**Two-phase generation (standard mode):**
1. Analysis phase: Extract 0-6 detail points from diff using Sonnet/Opus
2. Summary phase: Generate commit summary from details using Haiku

# Commands

**Build:**
```bash
cargo build --release
cargo build --release --bin llm-git  # Main binary only
```

**Run:**
```bash
# Standard mode
cargo run --release --bin llm-git                              # Analyze & commit staged changes
cargo run --release --bin llm-git -- --dry-run                 # Preview without committing
cargo run --release --bin llm-git -- --mode=unstaged           # Analyze unstaged (no commit)
cargo run --release --bin llm-git -- --mode=commit --target=HEAD~1  # Analyze specific commit
cargo run --release --bin llm-git -- --copy                    # Copy message to clipboard
cargo run --release --bin llm-git -- -m opus                   # Use Opus model
cargo run --release --bin llm-git -- Fixed regression from PR #123  # Add context

# Compose mode - split large changesets into atomic commits
cargo run --release --bin llm-git -- --compose                 # Execute compose
cargo run --release --bin llm-git -- --compose --compose-preview  # Preview splits only
cargo run --release --bin llm-git -- --compose --compose-max-commits 3  # Limit to 3 commits
cargo run --release --bin llm-git -- --compose --compose-test-after-each  # Run tests after each
```

**Environment:**
- Expects LiteLLM server running at `http://localhost:4000/chat/completions`

**Testing:**
```bash
cargo test                                  # Run all unit tests
cargo test --lib                            # Library tests only
```

# Architecture

## Module Structure

**Core library** (`src/lib.rs`):
- `analysis` - Scope candidate extraction from git numstat
- `api` - OpenRouter/LiteLLM integration with function calling + retry logic
- `compose` - AI-powered commit splitting (NEW)
- `config` - Configuration loading, prompt template management
- `diff` - Smart diff truncation with priority scoring
- `error` - Error types with `thiserror`
- `git` - Git command wrappers (diff, stat, commit, history operations)
- `normalization` - Unicode normalization, commit message formatting
- `patch` - Hunk-level staging for compose mode (NEW)
- `templates` - Prompt template rendering with `tera`
- `types` - Type-safe commit types, scopes, summaries with validation
- `validation` - Commit message validation (past-tense verbs, length limits)
- `rewrite` - History rewrite orchestration

**Entry points:**
- `src/main.rs` - CLI routing to standard/compose/rewrite modes

## Core Workflows

**Standard Mode** (`src/main.rs:run_generation`):
1. `get_git_diff()` + `get_git_stat()` - Extract changes based on mode (staged/unstaged/commit)
2. `smart_truncate_diff()` - Truncate if >100KB with priority-based selection:
   - Priority: source files > config > tests > binaries > lock files
   - Preserve ALL file headers, truncate content proportionally
   - Keep context (first 15 + last 10 lines per file)
3. `extract_scope_candidates()` - Parse git numstat to identify changed modules/components
4. `generate_conventional_analysis()` - AI call with function calling schema:
   - Tool: `create_conventional_analysis`
   - Returns: `{type, scope?, body: [details], issue_refs: [...]}`
5. `generate_summary_from_analysis()` - AI call for summary generation:
   - Tool: `create_commit_summary`
   - Input: type + scope + detail points + stat
   - Returns: `{summary}` (≤72 chars)
6. `post_process_commit_message()` - Enforce capitalization, punctuation
7. `validate_commit_message()` - Check past-tense verbs, length limits
8. `git_commit()` - Create commit (unless dry-run)

**Compose Mode** (`src/compose.rs:run_compose_mode`):
1. Combine staged + unstaged diffs into single analysis
2. `analyze_for_compose()` - AI identifies logical commit groups:
   - Tool: `create_compose_analysis`
   - Returns: `{groups: [{changes: [{path, hunks}], type, scope?, rationale, dependencies}]}`
   - **CRITICAL**: Each group specifies file paths + hunk headers (e.g., `@@ -10,5 +10,7 @@`) or `["ALL"]`
3. `compute_dependency_order()` - Topological sort (Kahn's algorithm) to ensure working state
4. Display proposed splits, optionally stop (preview mode)
5. `execute_compose()` - For each group in dependency order:
   - Capture baseline diff once (against original HEAD)
   - `stage_group_changes()` - Hunk-aware staging:
     - If all hunks = `["ALL"]`: use `git add <files>`
     - Otherwise: extract specific hunks, `git apply --cached <patch>`
   - Generate commit message via standard flow
   - `git_commit()` + capture new HEAD hash
   - Optionally run tests

**Rewrite Mode** (`rewrite_history.py` + `src/rewrite.rs`):
1. `get_commit_list()` - Extract commit hashes via `git rev-list --reverse`
2. Parallel API calls to Haiku for message conversion
3. `rewrite_history()` - Rebuild history with `git commit-tree`:
   - Preserves trees, authors, dates, parent relationships
   - Updates messages only
   - Updates branch ref to new head

## Smart Truncation Strategy (`src/diff.rs`)

**Priority scoring** (higher = more important):
```rust
pub const PRIORITY_SOURCE: i32 = 100;    // .rs, .py, .js, .ts, etc.
pub const PRIORITY_CONFIG: i32 = 80;     // .toml, .yaml, .json, etc.
pub const PRIORITY_TEST: i32 = 60;       // test files
pub const PRIORITY_DOC: i32 = 40;        // .md files
pub const PRIORITY_BINARY: i32 = 20;     // images, etc.
```

**Excluded files** (never included in diff): `Cargo.lock`, `package-lock.json`, `yarn.lock`, etc.

**Truncation logic:**
1. Parse diff into `FileDiff` structs
2. Calculate total length, determine how much to trim
3. Show ALL file headers (crucial for context)
4. Distribute remaining space proportionally by priority
5. For each file: keep first 15 + last 10 lines, truncate middle
6. Annotate with `[... X lines omitted ...]`

## Hunk-Level Staging (`src/patch.rs`)

**Problem**: When multiple commit groups reference the same file, staging by whole file (`git add <file>`) commits all changes at once, leaving nothing for subsequent groups.

**Solution**: Extract specific hunks per group, apply with `git apply --cached`.

**Key functions:**
- `extract_file_diff()` - Isolate single file's diff from full diff
- `extract_hunks_for_file()` - Filter specific hunks by header matching
- `normalize_hunk_header()` - Compare hunks by line numbers (e.g., `-10,5 +10,7`)
- `create_patch_for_changes()` - Assemble multi-file patch from hunk selections
- `stage_group_changes()` - Route to `git add` (if all `["ALL"]`) or `git apply --cached` (partial)

**Important**: Baseline diff must be captured ONCE before any compose commits, so hunk headers remain stable across groups (lines 408-421 in `src/compose.rs`).

## Prompt Engineering

**Prompt versions**: V1 (default) vs V2 (behind `new_prompts` feature flag)
- Located in `prompts/analysis/` and `prompts/summary/`
- Rendered at runtime via `tera` templates
- Config setting: `analysis_prompt_variant` / `summary_prompt_variant`

**Validation retry**: Summary generation retries once on validation failure with constraint injection
- Validates: past-tense verb, no type repetition, type-file consistency heuristics
- Fallback: Uses first detail or heuristic if retry exhausted
- See `validate_summary_quality()` in `src/api/mod.rs:288-358`


## Type System (`src/types.rs`)

**Type-safe wrappers** with validation:
- `CommitType` - Validates against `[feat, fix, refactor, docs, test, chore, style, perf, build, ci, revert]`
- `Scope` - Validates lowercase alphanumeric, max 2 segments (e.g., `api/client`)
- `CommitSummary` - Enforces length limits (72 guideline, 96 soft, 128 hard), warns on uppercase/period

**Compose types**:
- `FileChange` - `{path: String, hunks: Vec<String>}` - Hunk headers or `["ALL"]`
- `ChangeGroup` - `{changes: Vec<FileChange>, commit_type, scope?, rationale, dependencies: Vec<usize>}`
- `ComposeAnalysis` - `{groups: Vec<ChangeGroup>, dependency_order: Vec<usize>}`

**Model name resolution** (`resolve_model_name()`):
- Short names: `sonnet` → `claude-sonnet-4.5`, `opus` → `claude-opus-4.1`, `haiku` → `claude-haiku-4-5`
- GPT: `gpt5` → `gpt-5`, `gpt5-mini` → `gpt-5-mini`
- Gemini: `gemini` → `gemini-2.5-pro`, `flash` → `gemini-2.5-flash`
- Pass-through for full names

## API Integration (`src/api/mod.rs`)

**Function calling schema**:
1. `create_conventional_analysis` - Detail extraction:
   ```json
   {
     "type": "feat|fix|refactor|...",
     "scope": "optional_scope",
     "body": ["detail 1.", "detail 2."],
     "issue_refs": ["#123", "#456"]
   }
   ```

2. `create_commit_summary` - Summary generation:
   ```json
   {
     "summary": "concise past-tense summary without period"
   }
   ```

3. `create_compose_analysis` - Compose grouping:
   ```json
   {
     "groups": [
       {
         "changes": [
           {"path": "src/foo.rs", "hunks": ["@@ -10,5 +10,7 @@"]},
           {"path": "src/bar.rs", "hunks": ["ALL"]}
         ],
         "type": "feat",
         "scope": "api",
         "rationale": "Added TLS support",
         "dependencies": []
       }
     ]
   }
   ```

**Retry logic** (`retry_api_call()`):
- Exponential backoff: 1s, 2s, 4s (default 3 retries)
- Retries on 5xx errors or transient failures
- Configurable: `max_retries`, `initial_backoff_ms` in config

**Fallback**: If AI calls fail, `fallback_summary()` generates heuristic summary from stat.

## Configuration (`~/.config/llm-git/config.toml`)

```toml
api_base_url = "http://localhost:4000"
analysis_model = "claude-sonnet-4.5"
summary_model = "claude-haiku-4-5-20251001"
temperature = 1.0

summary_guideline = 72        # Target length
summary_soft_limit = 96       # Triggers retry
summary_hard_limit = 128      # Absolute max

max_retries = 3
initial_backoff_ms = 1000
max_diff_length = 100000

wide_change_threshold = 0.50  # Omit scope if >50% of files changed

analysis_prompt_variant = "default"
summary_prompt_variant = "default"

exclude_old_message = false   # When true, git show omits original message
```

# Implementation Notes

**Dependencies:**
- `clap` - CLI parsing with derive macros
- `reqwest` (blocking) - HTTP client for OpenRouter API
- `serde` + `serde_json` - Serialization for function calling
- `arboard` - Clipboard support for `--copy`
- `tera` - Prompt template rendering
- `rust-embed` - Embed prompt files in binary
- `anyhow` + `thiserror` - Error handling
- `rayon` - Parallel processing in rewrite mode
- `chrono` - Timestamps for backup branches
- `parking_lot` - High-performance Mutex/RwLock (ALWAYS use over `std::sync`)

**Models:**
- Default: Sonnet 4.5 for analysis, Haiku 4.5 for summary
- Optional: Opus 4.1 via `-m opus` (more powerful, slower, expensive)
- Compose mode uses analysis model for both grouping + per-commit generation

**Validation rules:**
- Summary: ≤72 chars (guideline), ≤96 (soft limit), ≤128 (hard limit), past-tense verb, no trailing period
- Body: Past-tense verbs preferred, ends with periods
- Warns on present-tense usage but doesn't block
- Type-file consistency checks (e.g., >80% .md files but type != docs)

**Cost estimates:**
- Standard commit: ~$0.02-0.05 (Sonnet analysis + Haiku summary)
- Compose mode: ~$0.05-0.15 per group (multiple analysis + summary calls)
- Rewrite mode: ~$0.001/commit with Haiku (~$1-5 for 1000-5000 commits)

# Linting

Project uses comprehensive Clippy linting (see `Cargo.toml`):
- `all`, `pedantic`, `style`, `perf`, `correctness`, `suspicious`, `nursery` enabled
- `allow_attributes_without_reason = "warn"` - MUST provide reason for `#[allow(...)]`
- Many pragmatic allows for builder patterns, numeric casts, code organization
- **CRITICAL**: Always use `parking_lot::{Mutex, RwLock}` instead of `std::sync`

# Common Issues

**Compose mode empty commits**: Ensure AI returns hunk headers from diff, not fabricated. If model struggles, file may need `hunks: ["ALL"]` for entire file.

**Hunk extraction fails**: Check `extract_file_diff()` correctly parses `diff --git a/... b/...` headers. File path matching is sensitive to `a/` and `b/` prefixes.

**Validation retry loops**: If summary validation fails repeatedly, check `validate_summary_quality()` constraints aren't overly strict for edge cases.

**API timeouts**: Increase `timeout` in HTTP client (currently 120s) if large diffs take longer to process.

**Prompt changes not applied**: After editing `prompts/*.md`, rebuild to re-embed templates.
