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
- `api` - LLM integration (OpenAI-compatible) with markdown response parsing, retry logic, and response caching
- `cache` - On-disk cache of LLM responses (BLAKE3-keyed)
- `changelog` - `CHANGELOG.md` maintenance against the staged tree
- `compose` - AI-powered commit splitting
- `config` - Configuration loading (TOML)
- `diffing` - Smart diff truncation with priority scoring
- `errors` - Error/exception types
- `git` - Git command wrappers (diff, stat, commit, history operations)
- `judgment` - Typed judgments (yes/no probability, pick-one choice) over small states via TypeSafe's System One API (jev); cached and cost-accounted like model calls, optional (no key = no judge)
- `map_reduce` - Parallel per-file analysis for large diffs
- `markdown_output` - Markdown rendering and heuristic fallback summary
- `mechanical` - Hunk-level judgments: every hunk of a prompt-bound diff gets a mechanical probability and an intent kind; follow-through hunks (imports, call sites, formatting, generated output) become one-line markers, the rest carry a `[judged: <type>]` hint on their `@@` header
- `models` - Type-safe commit types, scopes, summaries; model-name resolution; compose data types
- `normalization` - Unicode normalization, commit message formatting
- `patch` - Hunk-level staging for compose mode
- `pricing` - Token-usage parsing, per-model cost estimation, session spend accounting
- `profile` - Lightweight timing / JSONL profiling
- `repo` - Repository metadata detection
- `style` - Terminal styling helpers
- `synthesis` - Shared `CommitEvidence`: one diff/observation reading produces conventional analysis plus typed per-boundary changelog updates, using existing API accounting and prompt overrides
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
2. `prune_mechanical_hunks()` (`lgit/mechanical.py`) - With a judge configured and the diff ≥ `mechanical_prune_min_tokens`, every hunk is judged (one request per file, two questions per hunk — mechanical `noul` + kind `choice` over `HUNK_KINDS` using the configured type descriptions — chunked under jev's ~32k-token ceiling, all files concurrent ≈ 1s wall). Hunks at ≥ `mechanical_hunk_threshold` become `[mechanical after-effect omitted: +a/-d lines]` markers (a file whose hunks are all mechanical collapses to one marker); the remaining hunks' `@@` headers gain `[judged: feat|fix|test|...]` when the kind choice is ≥ `KIND_CONFIDENCE_FLOOR` (0.6) — a prior for the small analysis/fast models, which the prompts say the diff overrides. Guards: a diff that is entirely mechanical, or has fewer than two hunks, is never pruned (hints still render). The `stat` is untouched, so scope/wide-change logic still sees every file. Applies to both the standard and fast workflows (before map-reduce sizing and truncation) and to compose's per-group analysis diffs
3. `smart_truncate_diff()` - Truncate if >100KB with priority-based selection:
   - Priority: source files > config > tests > binaries > lock files
   - Preserve ALL file headers, truncate content proportionally
   - Keep context (first 15 + last 10 lines per file)
4. `extract_scope_candidates()` - Parse git numstat to identify changed modules/components
5. Shared synthesis (staged mode, committing, with applicable changelogs): `_generate_shared_message` calls `synthesis.generate_commit_evidence` once for conventional analysis and all boundary updates. `ChangelogContext` snapshots carry paths, governed files, existing entries, authored entries and revision permissions; the response must cover every path exactly once. Fast mode uses `effective_fast_model`; standard mode uses `analysis_model`. Large standard inputs reuse map observations before synthesis, not a separate reduce/changelog reread. There is no docs-by-extension override or arbitrary changelog-entry cap.
6. `_ChangelogRunner.start_with_evidence` prepares revision-aware deduplication while summary validation runs; mutation still waits for final validation. Invalid/incomplete synthesis or relevant legacy prompt overrides use the existing separate analysis/changelog flow. Dry-run, no-changelog, no-boundary, commit-mode and unstaged generation stay on their existing paths.
7. `summary_from_holistic_analysis` reuses a compliant heading, mechanically repairs tense, and checks the full type/scope-prefixed soft/hard length. Otherwise `generate_summary_from_analysis()` retains the summary/rewrite fallback:
   - Input: type + scope + detail points + stat
   - Returns plain text in `<summary>...</summary>` tags (≤72 chars)
8. `post_process_commit_message()` - Enforce capitalization, punctuation
9. `validate_commit_message()` - Check past-tense verbs, length limits
10. Await changelog generation, apply + stage entries, then create commit (unless dry-run)

**Compose Mode** (`lgit/compose.py:run_compose_mode`):
1. Scope to the staged tree, exactly like the regular commit path: callers `git add -A` first only when nothing is staged (`_run_compose` → `_auto_stage_if_needed`). `run_compose_mode` then captures that staged tree ONCE (`git write-tree`) as the fixed target; every round diffs `HEAD` against it (`git diff HEAD <target>`, with rename detection) and pins blobs from it — so anything staged or edited mid-run is excluded and stays staged, exactly as a normal commit would leave it. Unstaged worktree edits and untracked files are never in scope. The loop is unconstrained — each round commits what its plan covers and re-plans the rest until `HEAD` reaches the target — but errors out if a round produces no commits while changes remain (a no-op plan), instead of spinning forever.
2. Hunk judgments (`classify_hunks` → `HunkJudgments`, once per round, same judge/thresholds as standard mode): mechanical hunk ids (a) prune the observe-phase diff (`render_snapshot_diff`), (b) are hidden from the planner's sampled hunk list and replaced with a per-file/area note (`... N mechanical follow-through hunks not listed` / `all N hunks are mechanical follow-through; group with their cause`), and (c) prune each group's analysis diff (`ComposeGroupPatch.analysis_diff`; the stageable `diff` is never pruned). Kind hints tag the planner's listed hunks (`- F001-H002 [fix] old:...`) so it can group by intent, and ride into each group's analysis diff
3. Intent analysis - AI identifies logical commit groups from markdown:
   - Returns group headings with file IDs/hunk IDs that are mapped to compose groups
   - **CRITICAL**: Each group specifies file paths + hunk headers (e.g., `@@ -10,5 +10,7 @@`) or `["ALL"]`
   - Planning targets are per-file (`F...`) for small snapshots and per-path-area (`A...`) for large ones (`_build_planning_index`). Area subdivision stops once `COMPOSE_AREA_TARGET_MIN_COUNT` (24) targets exist and never exceeds `COMPOSE_AREA_TARGET_MAX_COUNT` (48), so a monorepo plans over ~30 workstream-sized areas instead of hundreds of single files — the signal a planner needs to split by subsystem rather than bucket alphabetically
   - Model output is accepted as area IDs, file IDs, or paths: `PlanningIndex.aliases` maps every file id and path back to its area, so a planner that answers with paths is normalized instead of silently falling back
   - `_split_grab_bag_groups` is a structural backstop: any returned group spanning `COMPOSE_GRAB_BAG_WORKSTREAM_THRESHOLD` (5) or more workstreams is partitioned per workstream while `max_commits` slots remain, keeping the model's own rationale on the partition that matches it
   - `--compose-max-commits` defaults to 32; each group still holds whole workstreams, so the ceiling bounds commits, not areas
4. Hunk binding for files claimed by several groups (`_bind_compose_plan`): with a judge, `_bind_with_judge` asks one `choice` question per hunk over the file's candidate groups (criteria = `type(scope): rationale`, state = the file's hunks; all files concurrent, ~1s wall), accepts choices at ≥ `COMPOSE_BIND_CONFIDENCE_FLOOR` (0.5), and hands the rest to the deterministic fallback (earliest candidate in dependency order) — judgments are deterministic, so there is no repair round. Without a judge, `_bind_with_model` runs the batched `compose-bind` calls on the analysis model plus one repair round, as before
5. Dependency order - Topological sort (Kahn's algorithm) to ensure working state
6. Display proposed splits, optionally stop (preview mode)
7. Execute - For each group in dependency order:
   - Capture baseline diff once (against original HEAD)
   - Hunk-aware staging:
     - If all hunks = `["ALL"]`: stage whole files
     - Otherwise: extract specific hunks, `git apply --cached <patch>`
   - `_prepare_shared_groups` generates each fixed group's message and applicable changelog updates together, in the existing bounded parallel wave. It does not change group ownership, dependencies, or staging. The plan retains type/scope authority; a valid synthesized summary is reused, otherwise the existing summary repair/rewrite path runs. Relevant prompt overrides or invalid synthesis retain separate generation.
   - `ChangelogWeaver.contexts_for_group` captures the round's seeded chain context; `updates_for_group` routes validated updates to existing changelog paths. `weave_group` reconciles each group's entries against the accumulating chain before writing its commit tree. Cross-group repeats are filtered, unmatched revisions are skipped, and authored entries stay protected. As with the existing concurrent generation, sibling groups do not see each other's newly generated entries. Claimed changelogs stay out of planning; authored staged edits ride the first commit. `--no-changelog` or `changelog_enabled = false` disables the weave.
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

**Prompt scrub** (`scrub_diff_for_prompt`): every LLM-bound diff (standard/fast analysis, compose planning + per-group messages, changelog, rewrite) is scrubbed first: lines >512 chars — hex/base64 blobs, minified bundles, long string literals — collapse to `head[..omitted 14KB..]tail` (`collapse_blob_lines`), then any file section still >100KB is capped to its `diff --git`/`index`/`+++`/`---` metadata plus edge lines. `parse_diff` keeps `@@` hunk headers in file *content*, not in `header`: a file with thousands of hunks is otherwise uncappable — 21.5k surviving `@@` lines alone became a 143k-token request and blew the context window of any 131k model. Line-count budgets alone can't catch these (a single 5MB line survives "first 15 lines"). Binary files (images) are safe by construction — diffs are generated without `--binary`, so they appear only as one-line `Binary files … differ` headers. Prompt-side only; never applied to diffs used for staging.

## Hunk-Level Staging (`lgit/patch.py`)

**Problem**: Compose splits the *staged* tree but spends minutes in LLM calls. Reading the live worktree (`git add <file>`) at staging time would let unstaged edits made to a staged file in the meantime leak into the generated commits.

**Solution**: Everything staged during a compose round comes from the immutable staged-index snapshot captured at the start of that round — never from the live worktree.

**Key steps:**
- Build the compose snapshot - Parse the round's `git diff HEAD <target>` into `ComposeFile`/`ComposeHunk` records with stable ids
- Pin blobs from the target tree - Capture each changed path's blob (object `{mode, oid}`, or deleted) from the fixed target tree via `git ls-tree` (live index for the no-target test path); covers regular blobs, symlinks, and submodule gitlinks uniformly
- Stage a group into an isolated temp index:
  - Partial file: `git apply --cached --3way` with the snapshot-derived patch; on conflict, re-splice from the base blob
  - Whole-file / binary: `git update-index --cacheinfo` with the pinned blob (deletions via `--force-remove`); falls back to `git add` only for unpinned snapshots (tests)
- Splice hunks into base - Reconstruct file content from base blob + selected hunks without `git apply`

**Important**: The target staged tree is captured ONCE in `run_compose_mode`; each round's snapshot (diff + pins) is derived from it before any LLM call. Commits are built as `commit-tree` objects against a temp index; the branch ref update at the end is guarded against HEAD movement (`update_ref_checked`). The real index is left untouched for planned source paths, so committed paths match the new HEAD (clean) while anything the user stages or edits mid-run stays exactly as they left it; claimed changelogs are synchronized separately under compare-before-write guards. `run_compose_mode` re-diffs `HEAD` against the fixed target and loops until they match, erroring if a round commits nothing while changes remain.

**Snapshot isolation elsewhere:**
- Standard/fast staged mode captures the index tree before generation (`lgit/cli.py`). After changelog application, `git.tree_with_index_paths` overlays only the generated changelog paths as exact index objects onto that captured tree; it never re-captures unrelated mid-run staging. If the live index still matches, plain `git commit` runs (hooks included). Otherwise the snapshot is committed directly (`commit-tree` + checked ref update, hooks skipped), leaving unrelated staging/worktree edits for the next commit. The overlay does not use text patches, so display settings, CRLF and whitespace-fixing configuration cannot alter bytes.
- Changelog maintenance (`lgit/changelog.py`) generates against the staged copy and stages exact blobs; the worktree receives entries separately, so unstaged changelog text does not enter the commit. `prepare_changelog_flow` captures boundary snapshots. Shared synthesis exports them through `prepared_changelog_contexts` and prepares results with `prepare_synthesized_updates`; standalone `generate_changelog_updates` uses the same revision-aware duplicate filtering. `apply_changelog_updates` writes only after validation. Edits splice into the parsed `UnreleasedSection.lines` (`apply_revisions`, `write_entries`): new bullets go to the top of their `###` category (above any `####` subsection), revisions reword/drop bullets in place, and every other line — subsections, prose, unknown categories — stays verbatim; a drop removes a heading only when it leaves it empty. Existing Unreleased entries may be revised, authored pending entries remain protected, and unmatched operations are skipped. `ChangelogRevision`, `ChangelogContext`, `ChangelogUpdate`, and `CommitEvidence` live in `models.py`. Discovery uses `git ls-files`, never a worktree walk.
- Hand-written changelog edits are respected: `[Unreleased]` entries the author added in this change (the staged copy — compose: the target tree — vs `HEAD:`, `_entries_added_since`; never the worktree, whose unstaged entries may document other pending work and only feed the duplicate filter) are passed to the model as `authored_entries` — it documents only changes they don't cover, returning nothing when they cover everything. As a backstop, generated entries that restate an existing bullet are dropped before insertion (`drop_duplicate_entries`, run inside the concurrent generation task so it never lands on the critical path): verbatim repeats always; content-stem overlap with one existing entry decides the clear cases locally (≥70% duplicate, <15% distinct); only the band between goes to the judge (one request per boundary, one `noul` per candidate, dropped at ≥ `changelog_duplicate_threshold`) — the judge catches paraphrases the overlap heuristic misses (e.g. "models now fall back to the next model in the chain" vs an existing fallback-chain entry) and is kept for those without a judge.
- The changelog-worthiness judge remains in standalone `generate_changelog_entries` (for separate generation or fallback): below `changelog_gate_threshold` it skips the changelog model, with `changelog_gate_min_tokens` limiting the extra round trip to large inputs. Shared synthesis already determines visibility while reading the evidence for the message, so it needs no separate worthiness call.
- Compose changelog weaving updates the real index and worktree only while each copy still matches what the run last staged or wrote; mid-run user changes win and remain untouched.

## Prompt Engineering

**Prompt files**:
- One markdown prompt per family under `lgit/resources/prompts/<family>.md` (families: `analysis`, `summary`, `summary-rewrite`, `changelog`, `map`, `reduce`, `compose-intent`, `compose-bind`, `fast`, `synthesis`).
- Rendered at runtime via Jinja2 (`lgit/templates.py`). Overrides are opt-in: set `prompts_dir` in config to a directory containing `<family>.md` files; nothing is ever auto-written to disk.
- `synthesis.md` returns validated JSON containing conventional analysis and one update per changelog path. A relevant legacy override keeps separate generation unless an explicit `synthesis.md` override opts into shared generation; custom summary/rewrite prompts remain in the existing repair path.

**Validation retry**: Summary generation salvages rejected model output before falling back
- Validates: past-tense verb, no type repetition, type-file consistency heuristics
- Mechanical repair first: a leading present-tense verb is rewritten via `present_to_past()` (`repair_summary_tense()`), no extra API call; the reduce/analysis heading is tried as a second candidate
- Then one `summary-rewrite` call: the summary model minimally edits the rejected draft into compliance (`summary-rewrite.md`) — a pure text task with no diff/details context
- Fallback: Uses first detail or heuristic if repair and rewrite are exhausted
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

**Markdown response parsing**:
1. `create_conventional_analysis` - Detail extraction:
   - Parses a conventional-commit heading such as `feat(api): added login`
   - Converts markdown bullets into detail points
   - Extracts issue refs from the parsed response

2. `create_commit_summary` - Summary generation:
   - Parses `<summary>concise past-tense summary</summary>`
   - Strips accidental type/scope prefixes before validation

3. `create_compose_intent_plan` / `bind_compose_hunks` - Compose grouping:
   - Parses markdown group plans into file IDs, hunk IDs, commit types, scopes, and dependencies
   - Falls back to deterministic grouping when parsing fails

**Retry logic**:
- Exponential backoff: 1s, 2s, 4s (default 3 retries)
- Retries on 5xx errors or transient failures
- Configurable: `max_retries`, `initial_backoff_ms` in config

**Caching**: Responses are cached on disk keyed by a BLAKE3 hash of `(operation, model, api_mode, tool_name, system_prompt, user_prompt)` (`lgit/cache.py`); set `LLM_GIT_CACHE_DISABLED=1` to bypass.

**Cost tracking**: Every real API response is recorded (`lgit/pricing.py`): the LiteLLM `x-litellm-response-cost` header is used when present, otherwise cost is estimated from response `usage` tokens against the bundled rate table (`lgit/resources/model_pricing.json`, substring-matched on model name). The cost is stored on the cached response row (`responses.cost_usd`) and in a per-request `usage` table in the cache db; cache hits credit the stored cost as savings. Session totals (spend + savings) print after compose completes.

**Fallback**: If AI calls fail, `fallback_summary()` (`lgit/markdown_output.py`) generates a heuristic summary from stat.

## Configuration (`~/.config/llm-git/config.toml`)

```toml
api_base_url = "http://localhost:4000"
analysis_model = "claude-sonnet-4.5"
summary_model = "claude-haiku-4-5-20251001"
map_model = ""                # Map-phase (per-file observation) model; empty = summary_model
fast_model = ""               # Fast-mode (single-call) model; empty = summary_model
# Any model role accepts a `;`-separated fallback chain, tried left to right:
# analysis_model = "flash-lite;haiku"

summary_guideline = 72        # Target length
summary_soft_limit = 96       # Triggers retry
summary_hard_limit = 128      # Absolute max

max_retries = 3
initial_backoff_ms = 1000
max_diff_length = 100000

wide_change_threshold = 0.50  # Omit scope if >50% of files changed
changelog_revise = true      # Reconcile earlier Unreleased entries
changelog_reasoning_effort = "low"  # Reasoning effort for changelog calls (chat-completions mode); "" = provider default


exclude_old_message = true    # git show omits the original message from commit-mode prompts (default)   # When true, git show omits original message
prompts_dir = ""              # Optional dir of <family>.md prompt overrides; empty = packaged prompts

# Typed judgments (TypeSafe jev). No key (config or TYPESAFE_API_KEY) = no judge = previous behaviour.
judgment_enabled = true       # LLM_GIT_JUDGMENT_DISABLED=1 overrides; tests set it via conftest
judgment_api_key = ""         # `!cmd` supported like api_key
judgment_base_url = "https://api.typesafe.ai"   # TYPESAFE_BASE_URL overrides the default
judgment_model = "jev-latest"
mechanical_hunk_threshold = 0.7      # imports/re-exports/pass-through score 0.75-0.9, behaviour <0.3, fixture edits 0.5-0.7
mechanical_prune_min_tokens = 5000   # the round trip is a flat ~1s; below this it does not pay
mechanical_prune_min_model_rate = 1.0  # prune only when the analysis model bills >= this $/M input (unknown = yes)
mechanical_kind_hints = true         # per-hunk `[judged: <type>]` hints; ~40% of the judgment bill
changelog_gate_threshold = 0.3       # skip the changelog model below this judged user-visibility; 0 disables
changelog_gate_min_tokens = 8000     # gate only standard-path diffs; on the fast path the round trip is pure latency
changelog_duplicate_threshold = 0.6  # drop generated entries judged to restate an existing one at/above this
```

**Cost and verification:** Jev uses the bundled input-only rate and shares the same session accounting/cache as generation; `LLM cost:` includes both. Pruning has token-size and model-price gates because the extra read can cost more than it saves on cheap models. Shared synthesis removes redundant message/changelog reads and reuses compliant summaries, but provider latency, retries and output length remain variable. Compare actual CLI output, source snapshots, quality and total billed usage—not component timings or judge confidence. Existing compose grouping and dependency limitations are unchanged; exact final-tree equality alone does not establish that every intermediate commit builds.

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
- Map phase (per-file observations, runs in parallel) uses `map_model`, falling back to `summary_model`; it reads raw diffs, so it benefits from a stronger model than the summary role while parallelism hides the extra per-call latency
- Fast mode (`--fast`, and the auto-fast path) uses `fast_model`, falling back to `summary_model` — one call for the whole message, so it stays on the cheap/low-latency tier. `-m` overrides every role, including this one
- **Fallback chains**: every model role (and `-m`) accepts `"primary;fallback;..."` (`resolve_model_chain`). Each candidate gets the full `max_retries` budget inside `_run_oneshot_response`; if it still fails — 5xx stream stalls such as Gemini `Thinking loop detected`, refusals, or a prompt that cannot fit its context window — the next candidate runs and a warning names both models. Only the last candidate's failure propagates. Cache keys, cost accounting, and API-mode resolution all use the concrete candidate, so a fallback response is cached under the model that actually served it
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

**PEP 758 — unparenthesized `except` (do not flag)**: This project targets Python ≥3.14, where `except`/`except*` may list multiple exception types *without* parentheses as long as no name is bound — e.g. `except OSError, json.JSONDecodeError:` catches **both**. This is valid 3.14+ syntax ([PEP 758](https://peps.python.org/pep-0758/)), not the removed Python-2 `except E, name:` form and not a bug. Do not mark, "fix", or rewrite these to parenthesized tuples; both styles are accepted (`ruff` passes them) and appear in `lgit/api.py` and `lgit/compose.py`.

# Common Issues

**Compose mode empty commits**: Ensure AI returns hunk headers from diff, not fabricated. If model struggles, file may need `hunks: ["ALL"]` for entire file.

**A real change was pruned as mechanical**: raise `mechanical_hunk_threshold` (0.8 keeps everything but imports/re-exports) or set `mechanical_prune_min_tokens` high enough to skip that diff; judgments are cached, so re-running with `LLM_GIT_CACHE_DISABLED=1` re-judges. Judgment failures never abort a run — unjudged hunks stay in the diff and one `RuntimeWarning` names the failure.

**Hunk extraction fails**: Check the diff parser correctly handles `diff --git a/... b/...` headers. File path matching is sensitive to `a/` and `b/` prefixes.

**Validation retry loops**: If summary validation fails repeatedly, check `validate_summary_quality()` constraints aren't overly strict for edge cases.

**API timeouts**: Increase the httpx client `timeout` (currently 120s) if large diffs take longer to process.

**Prompt changes not applied**: Prompts load at runtime from `lgit/resources/prompts/` via `importlib.resources` — no rebuild step. For an installed wheel, reinstall (`uv sync`) to pick up edits. If `prompts_dir` is set in config, files there shadow the packaged prompts — clear it (or delete the stale file) when packaged prompt updates seem ignored.
