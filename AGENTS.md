# Project Overview

Git commit message generator using AI via LiteLLM (or any OpenAI-compatible API). Generates conventional commit messages with concise summaries (≤72 chars) and structured detail points from git diffs.

**Three operational modes:**
1. **Standard mode**: Single commit generation from staged/unstaged changes
2. **Compose mode**: AI-powered splitting of large changesets into multiple atomic commits
3. **Rewrite mode**: Batch rewriting of git history to conventional format

**Two-phase generation (standard mode):**
1. Analysis phase: Extract 0-6 detail points from diff using Sonnet/Opus
2. Summary phase: Generate commit summary from details using Haiku

# Commands

This is a `uv`-managed Python project (`>=3.14`). `uv sync --dev` installs everything.

**Build:**
```bash
uv build                    # sdist + wheel into dist/
uv sync --dev               # install package + dev tools into .venv
```

**Run:**
```bash
# Standard mode
uv run lgit                                 # Analyze & commit staged changes
uv run lgit --dry-run                       # Preview without committing
uv run lgit --mode=unstaged                 # Analyze unstaged (no commit)
uv run lgit --mode=commit --target=HEAD~1   # Analyze specific commit
uv run lgit --copy                          # Copy message to clipboard
uv run lgit -m opus                         # Use Opus model
uv run lgit Fixed regression from PR #123   # Add context

# Compose mode - split large changesets into atomic commits
uv run lgit --compose                       # Execute compose
uv run lgit --compose --compose-preview     # Preview splits only
uv run lgit --compose --compose-max-commits 3       # Limit to 3 commits
uv run lgit --compose --compose-test-after-each     # Run tests after each
```

After `uv tool install .` (or installing the wheel) the `lgit` command is on `PATH` directly, no `uv run` prefix needed.

**Environment:**
- Expects LiteLLM server running at `http://localhost:4000/chat/completions`

**Testing:**
```bash
uv run pytest                               # Run all tests
uv run pytest tests/test_compose.py         # A single test module
uv run pytest -k truncate                    # Match by name
```

# Architecture

## Module Structure

**Core package** (`lgit/`):
- `analysis` - Scope candidate extraction from git numstat
- `api` - LLM integration (OpenAI-compatible) with function calling, retry logic, and response caching
- `cache` - On-disk cache of LLM responses (BLAKE3-keyed)
- `changelog` - `CHANGELOG.md` maintenance against the staged tree
- `compose` - AI-powered commit splitting
- `config` - Configuration loading (TOML) and prompt-variant selection
- `diffing` - Smart diff truncation with priority scoring
- `errors` - Error/exception types
- `git` - Git command wrappers (diff, stat, commit, history operations)
- `map_reduce` - Parallel per-file analysis for large diffs
- `markdown_output` - Markdown rendering and heuristic fallback summary
- `models` - Type-safe commit types, scopes, summaries; model-name resolution; compose data types
- `normalization` - Unicode normalization, commit message formatting
- `patch` - Hunk-level staging for compose mode
- `profile` - Lightweight timing / JSONL profiling
- `repo` - Repository metadata detection
- `style` - Terminal styling helpers
- `templates` - Prompt template rendering with Jinja2
- `tokens` - Token counting helpers
- `validation` - Commit message validation (past-tense verbs, length limits)
- `rewrite` - History rewrite orchestration
- `resources/` - Bundled prompt templates + JSON data (`commit_types.json`, `validation_data.json`)

**Entry points:**
- `lgit/cli.py` - CLI parsing (argparse) + routing to standard/compose/rewrite modes (`main`)
- `lgit/__main__.py` - `python -m lgit` shim

## Core Workflows

**Standard Mode** (`lgit/cli.py`):
1. `get_git_diff()` + `get_git_stat()` - Extract changes based on mode (staged/unstaged/commit)
2. `smart_truncate_diff()` - Truncate if >100KB with priority-based selection:
   - Priority: source files > config > tests > binaries > lock files
   - Preserve ALL file headers, truncate content proportionally
   - Keep context (first 15 + last 10 lines per file)
3. `extract_scope_candidates()` - Parse git numstat to identify changed modules/components
4. Analysis call - AI call with function calling schema:
   - Tool: `create_conventional_analysis`
   - Returns: `{type, scope?, body: [details], issue_refs: [...]}`
5. `generate_summary_from_analysis()` - AI call for summary generation:
   - Tool: `create_commit_summary`
   - Input: type + scope + detail points + stat
   - Returns: `{summary}` (≤72 chars)
6. `post_process_commit_message()` - Enforce capitalization, punctuation
7. `validate_commit_message()` - Check past-tense verbs, length limits
8. Create commit (unless dry-run)

**Compose Mode** (`lgit/compose.py:run_compose_mode`):
1. Combine staged + unstaged diffs into single analysis
2. Intent analysis - AI identifies logical commit groups:
   - Tool: `create_compose_analysis`
   - Returns: `{groups: [{changes: [{path, hunks}], type, scope?, rationale, dependencies}]}`
   - **CRITICAL**: Each group specifies file paths + hunk headers (e.g., `@@ -10,5 +10,7 @@`) or `["ALL"]`
3. Dependency order - Topological sort (Kahn's algorithm) to ensure working state
4. Display proposed splits, optionally stop (preview mode)
5. Execute - For each group in dependency order:
   - Capture baseline diff once (against original HEAD)
   - Hunk-aware staging:
     - If all hunks = `["ALL"]`: stage whole files
     - Otherwise: extract specific hunks, `git apply --cached <patch>`
   - Generate commit message via standard flow
   - Create commit + capture new HEAD hash
   - Optionally run tests

**Rewrite Mode** (`lgit/rewrite.py`):
1. `get_commit_list()` - Extract commit hashes via `git rev-list --reverse`
2. Concurrent API calls (asyncio) to Haiku for message conversion
3. Rebuild history with `git commit-tree`:
   - Preserves trees, authors, dates, parent relationships
   - Updates messages only
   - Updates branch ref to new head

## Smart Truncation Strategy (`lgit/diffing.py`)

**Priority scoring** (higher = more important):
- Source files (`.py`, `.rs`, `.js`, `.ts`, …) — highest
- Config (`.toml`, `.yaml`, `.json`, …)
- Tests
- Docs (`.md`)
- Binaries (images, …) — lowest

**Excluded files** (never included in diff): `Cargo.lock`, `package-lock.json`, `yarn.lock`, `uv.lock`, etc.

**Truncation logic:**
1. Parse diff into per-file records
2. Calculate total length, determine how much to trim
3. Show ALL file headers (crucial for context)
4. Distribute remaining space proportionally by priority
5. For each file: keep first 15 + last 10 lines, truncate middle
6. Annotate with `[... X lines omitted ...]`

## Hunk-Level Staging (`lgit/patch.py`)

**Problem**: Staging from the live worktree (`git add <file>`) reads whatever is on disk *at staging time*. Compose spends minutes in LLM calls; any edits the user makes meanwhile would leak into the generated commits.

**Solution**: Everything staged during compose comes from the immutable snapshot captured at invocation — never from the live worktree.

**Key steps:**
- Build the compose snapshot - Parse the captured diff into `ComposeFile`/`ComposeHunk` records with stable ids
- Pin worktree state - Hash every changed file's worktree content into the odb at capture time (object `{mode, oid}`, or deleted); handles symlinks, submodule gitlinks, and binaries
- Stage a group into an isolated temp index:
  - Partial file: `git apply --cached --3way` with the snapshot-derived patch; on conflict, re-splice from the base blob
  - Whole-file / binary: `git update-index --cacheinfo` with the pinned blob (deletions via `--force-remove`); falls back to `git add` only for unpinned snapshots (tests)
- Splice hunks into base - Reconstruct file content from base blob + selected hunks without `git apply`

**Important**: The snapshot (diff + pins) is captured ONCE in `run_compose_round` before any LLM call. Commits are built as `commit-tree` objects against a temp index; the branch ref update at the end is guarded against HEAD movement. If the real index drifted mid-run, only the snapshot paths are refreshed so mid-run staging stays staged; otherwise a full `reset --mixed` runs.

**Snapshot isolation elsewhere:**
- Standard/fast staged mode captures the index tree after auto-stage/changelog (`lgit/cli.py`). If the index still matches, plain `git commit` runs (hooks included). If it drifted mid-run, the snapshot tree is committed directly (`commit-tree` + checked ref update, hooks skipped) — the index and worktree are left untouched, so mid-run staging stays staged for the next commit.
- Changelog maintenance (`lgit/changelog.py`) generates entries against the *staged* copy of `CHANGELOG.md` and stages the result as an exact blob, so unrelated unstaged changelog edits never enter the commit; the worktree copy gets the entries inserted separately.

## Prompt Engineering

**Prompt variants**: `default` vs `markdown`
- Selected per call by the `markdown_output` config flag (markdown on by default); `analysis_prompt_variant` / `summary_prompt_variant` override the non-markdown variant.
- Bundled under `lgit/resources/prompts/<family>/` (families: `analysis`, `summary`, `changelog`, `map`, `reduce`, `compose-intent`, `compose-bind`, `fast`).
- Rendered at runtime via Jinja2 (`lgit/templates.py`). User overrides may live in `~/.llm-git/prompts/`.

**Validation retry**: Summary generation retries once on validation failure with constraint injection
- Validates: past-tense verb, no type repetition, type-file consistency heuristics
- Fallback: Uses first detail or heuristic if retry exhausted
- See `validate_summary_quality()` in `lgit/validation.py`

## Type System (`lgit/models.py`)

**Type-safe wrappers** with validation:
- `CommitType` - Validates against `[feat, fix, refactor, docs, test, chore, style, perf, build, ci, revert]`
- `Scope` - Validates lowercase alphanumeric, max 2 segments (e.g., `api/client`)
- `CommitSummary` - Enforces length limits (72 guideline, 96 soft, 128 hard), warns on uppercase/period

**Compose types**:
- `FileChange` - `{path: str, hunks: list[str]}` - Hunk headers or `["ALL"]`
- `ChangeGroup` - `{changes: list[FileChange], commit_type, scope?, rationale, dependencies: list[int]}`
- `ComposeAnalysis` - `{groups: list[ChangeGroup], dependency_order: list[int]}`

**Model name resolution** (`resolve_model_name()`):
- Short names: `sonnet` → `claude-sonnet-4.5`, `opus` → `claude-opus-4.1`, `haiku` → `claude-haiku-4-5`
- GPT: `gpt5` → `gpt-5`, `gpt5-mini` → `gpt-5-mini`
- Gemini: `gemini` → `gemini-2.5-pro`, `flash` → `gemini-2.5-flash`
- Pass-through for full names

## API Integration (`lgit/api.py`)

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
           {"path": "lgit/foo.py", "hunks": ["@@ -10,5 +10,7 @@"]},
           {"path": "lgit/bar.py", "hunks": ["ALL"]}
         ],
         "type": "feat",
         "scope": "api",
         "rationale": "Added TLS support",
         "dependencies": []
       }
     ]
   }
   ```

**Retry logic**:
- Exponential backoff: 1s, 2s, 4s (default 3 retries)
- Retries on 5xx errors or transient failures
- Configurable: `max_retries`, `initial_backoff_ms` in config

**Caching**: Responses are cached on disk keyed by a BLAKE3 hash of `(model, prompt_family, prompt_variant, …)` (`lgit/cache.py`); set `LLM_GIT_CACHE_DISABLED=1` to bypass.

**Fallback**: If AI calls fail, `fallback_summary()` (`lgit/markdown_output.py`) generates a heuristic summary from stat.

## Configuration (`~/.config/llm-git/config.toml`)

```toml
api_base_url = "http://localhost:4000"
analysis_model = "claude-sonnet-4.5"
summary_model = "claude-haiku-4-5-20251001"

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

**Dependencies** (`uv` for env + lockfile):
- `httpx` - HTTP client for the LLM API
- `jinja2` - prompt template rendering
- `blake3` - hashing for the LLM response cache
- Standard library: `argparse` (CLI), `asyncio` (concurrent API calls), `tomllib` (config + test fixtures), `importlib.resources` (bundled prompts/JSON), `dataclasses`, `subprocess` (git + clipboard)
- Clipboard (`--copy`): shells out to `pbcopy` (macOS), `clip` (Windows), or `wl-copy`/`xclip`/`xsel` (Linux) — no third-party clipboard library
- Dev: `pytest`, `ruff`, `mypy`

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

# Linting & Formatting

Tooling is `ruff` (lint + format) and `mypy` (local type checking), all run through `uv`:
```bash
uv run ruff format          # apply formatting
uv run ruff format --check  # verify formatting (CI gate)
uv run ruff check           # lint (CI gate)
uv run mypy                 # optional local type check
```
- `ruff format` owns line length (`line-length = 120`); `E501` is therefore disabled in lint.
- Lint rule set: `E`, `F`, `W`, `I`, `UP`, `B` (see `[tool.ruff.lint]` in `pyproject.toml`).
- CI gates on `ruff format --check`, `ruff check`, and `pytest`; `mypy` is local-only (not a CI gate).

# Common Issues

**Compose mode empty commits**: Ensure AI returns hunk headers from diff, not fabricated. If model struggles, file may need `hunks: ["ALL"]` for entire file.

**Hunk extraction fails**: Check the diff parser correctly handles `diff --git a/... b/...` headers. File path matching is sensitive to `a/` and `b/` prefixes.

**Validation retry loops**: If summary validation fails repeatedly, check `validate_summary_quality()` constraints aren't overly strict for edge cases.

**API timeouts**: Increase the httpx client `timeout` (currently 120s) if large diffs take longer to process.

**Prompt changes not applied**: Prompts load at runtime from `lgit/resources/prompts/` via `importlib.resources` — no rebuild step. For an installed wheel, reinstall (`uv sync`) to pick up edits, or drop overrides in `~/.llm-git/prompts/`.
