# llm-git

[![CI](https://github.com/can1357/llm-git/workflows/CI/badge.svg)](https://github.com/can1357/llm-git/actions)
[![Crates.io](https://img.shields.io/crates/v/llm-git.svg)](https://crates.io/crates/llm-git)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-nightly--2025--11--01-orange.svg)](https://www.rust-lang.org)

Git commit message generator using Claude AI (or other LLMs) via OpenAI-compatible API. Generates conventional commit messages with concise summaries (≤72 chars) and structured detail points from git diffs.

**Two-phase generation:**
1. Analysis phase: Extract 0-6 detail points from diff using Sonnet/Opus
2. Summary phase: Generate commit summary from details using Haiku

## Installation

### Prerequisites

- **Rust** (latest stable): Install from [rustup.rs](https://rustup.rs/)
- **Git**: Required for repository operations
- **API Access**: One of the following:
  - [Anthropic API key](https://console.anthropic.com/) (recommended)
  - [OpenAI API key](https://platform.openai.com/api-keys)
  - [OpenRouter API key](https://openrouter.ai/keys)
  - Local LiteLLM proxy (for development)

### From Source

```bash
git clone https://github.com/can1357/llm-git.git
cd llm-git
cargo install --path .
```

### From crates.io

Once published:

```bash
cargo install llm-git
```

## Quick Start

### Configure API Access

**Option A: LiteLLM (Recommended for local development)**
```bash
# Start LiteLLM proxy (handles API keys and routing)
pip install litellm
export ANTHROPIC_API_KEY=your_key_here
litellm --port 4000 --model claude-sonnet-4.5

# llm-git uses localhost:4000 by default
llm-git  # Ready to use!
```

**Option B: Direct API Access**
```bash
# Set API URL and key via environment variables
export LLM_GIT_API_URL=https://api.anthropic.com/v1
export LLM_GIT_API_KEY=your_api_key_here
llm-git
```

**Option C: OpenRouter**
```bash
export LLM_GIT_API_URL=https://openrouter.ai/api/v1
export LLM_GIT_API_KEY=your_openrouter_key
llm-git
```

**Option D: Configuration File**
```bash
# Create ~/.config/llm-git/config.toml (see Configuration section)
mkdir -p ~/.config/llm-git
# Edit config.toml with your preferences
llm-git
```

### Usage
```bash
# Stage your changes
git add .

# Generate and commit
llm-git                                     # Analyze & commit staged changes
llm-git --dry-run                          # Preview without committing
llm-git --mode=unstaged                    # Analyze unstaged (no commit)
llm-git --copy                             # Copy message to clipboard
llm-git Fixed regression from PR #123      # Add context (trailing text)
```

## Commands

**Basic Usage:**
```bash
llm-git                                     # Analyze & commit staged changes (default)
llm-git --dry-run                          # Preview without committing
llm-git --mode=unstaged                    # Analyze unstaged (no commit)
llm-git --mode=commit --target=HEAD~1      # Analyze specific commit
llm-git --copy                             # Copy message to clipboard
llm-git -m opus                            # Use Opus model (more powerful)
llm-git -m sonnet                          # Use Sonnet model (default)
llm-git -S                                 # GPG sign the commit
llm-git --no-changelog                     # Skip automatic changelog updates
llm-git Fixed regression from PR #123      # Add context (trailing text)
```

**Compose Mode (Multi-commit generation):**
```bash
llm-git --compose                          # Compose changes into multiple atomic commits
llm-git --compose --compose-preview        # Preview proposed splits without committing
llm-git --compose --compose-max-commits 5  # Limit number of commits
llm-git --compose --compose-test-after-each # Run tests after each commit
```

**Rewrite Mode (History rewrite to conventional commits):**
```bash
llm-git --rewrite                          # Rewrite full history (creates backup)
llm-git --rewrite --rewrite-preview 10     # Preview first 10 commits
llm-git --rewrite --rewrite-dry-run        # Preview all without applying
llm-git --rewrite --rewrite-start main~50  # Rewrite last 50 commits only
llm-git --rewrite --rewrite-parallel 20    # Use 20 parallel API calls
llm-git --rewrite --rewrite-hide-old-types # Hide old type/scope tags
```

## Automatic Changelog Maintenance

llm-git automatically maintains CHANGELOG.md files following the [Keep a Changelog](https://keepachangelog.com) format. When you commit, it analyzes your staged changes and appends entries to the `[Unreleased]` section.

**Features:**
- **Auto-detection**: Finds all CHANGELOG.md files in your repository
- **Monorepo support**: Routes changes to the correct changelog based on file paths
- **Deduplication**: Skips entries that semantically match existing ones (Jaccard similarity ≥0.7)
- **Category mapping**: Maps commit types to changelog sections (Added, Fixed, Changed, etc.)
- **Breaking change detection**: Scans commit body for "breaking" or "incompatible" keywords

**How it works:**
1. Detects CHANGELOG.md files and their "boundaries" (which directories they cover)
2. For each boundary with staged changes, analyzes the diff
3. Generates changelog entries from the analysis detail points
4. Parses the existing `[Unreleased]` section
5. Deduplicates against existing entries
6. Writes new entries and stages the modified changelog

**Monorepo boundary detection:**
```
project/
├── CHANGELOG.md              ← covers: src/, docs/, scripts/
├── packages/
│   ├── core/
│   │   ├── CHANGELOG.md      ← covers: packages/core/**
│   │   └── src/
│   └── cli/
│       ├── CHANGELOG.md      ← covers: packages/cli/**
│       └── src/
```

Changes to `packages/core/src/lib.rs` update `packages/core/CHANGELOG.md`.
Changes to `src/main.rs` update the root `CHANGELOG.md`.

**Disabling changelog updates:**
```bash
llm-git --no-changelog                     # Skip for this commit
```

Or permanently in config:
```toml
changelog_enabled = false
```

**Expected CHANGELOG.md format:**
```markdown
# Changelog

## [Unreleased]

### Added
- Existing entry one

### Fixed
- Existing entry two

## [1.0.0] - 2024-01-01
...
```

The `## [Unreleased]` header is required. Entries are inserted at the top of each category section.

## Environment Variables

All configuration options can be overridden via environment variables:

- `LLM_GIT_API_URL` - API endpoint URL (default: `http://localhost:4000`)
- `LLM_GIT_API_KEY` - API authentication key (default: none)
- `LLM_GIT_CONFIG` - Custom config file path (default: `~/.config/llm-git/config.toml`)
- `LLM_GIT_VERBOSE` - Enable verbose output with JSON structure

**Examples:**
```bash
# Use OpenAI instead of Claude
export LLM_GIT_API_URL=https://api.openai.com/v1
export LLM_GIT_API_KEY=sk-...
llm-git --analysis-model=gpt-4o --summary-model=gpt-4o-mini

# Use custom config location
export LLM_GIT_CONFIG=~/my-project/.llm-git-config.toml
llm-git

# Enable verbose debugging
export LLM_GIT_VERBOSE=1
llm-git
```

## Testing

```bash
cargo test                                  # Run all unit tests
cargo test --lib                            # Library tests only
```

## Architecture

**Modular library structure:**
- `src/lib.rs` - Public API exports
- `src/main.rs` - CLI entry point
- `src/analysis.rs` - Scope candidate extraction
- `src/api/` - OpenRouter/LiteLLM integration with retry logic
- `src/config.rs` - Configuration and prompt templates
- `src/diff.rs` - Smart diff truncation with priority scoring
- `src/error.rs` - Error types
- `src/map_reduce.rs` - Map-reduce analysis for large diffs
- `src/git.rs` - Git command wrappers
- `src/normalization.rs` - Unicode normalization, formatting
- `src/types.rs` - Type-safe commit types, scopes, summaries, config types
- `src/validation.rs` - Commit message validation
- `src/changelog.rs` - Automatic CHANGELOG.md maintenance

**Core workflow:**
1. `get_git_diff()` + `get_git_stat()` - Extract staged/unstaged/commit changes
2. Route to unified or map-reduce analysis based on file count
3. `generate_summary_from_analysis()` - Call Haiku with detail points
4. `post_process_commit_message()` - Enforce length limits, punctuation, capitalization
5. `validate_commit_message()` - Check past-tense verbs, length, punctuation

## LLM Call Flows

llm-git uses different strategies depending on the number of files changed:

### Unified Analysis (≤3 files)

For small commits, a single analysis call processes the entire diff:

```
┌─────────────────────────────────────────────────────────────────┐
│                 UNIFIED (≤3 files, no changelog)                │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  diff ──► [1] Analysis (Sonnet) ──► [2] Summary (Haiku) ──► git │
│                                                                 │
│  Total: 2 LLM calls                                             │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                 UNIFIED + CHANGELOG (≤3 files)                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  diff ──► [1] Changelog ──► [2] Analysis ──► [3] Summary ──► git│
│              (Sonnet)         (Sonnet)         (Haiku)          │
│                                                                 │
│  Total: 3 LLM calls                                             │
└─────────────────────────────────────────────────────────────────┘
```

### Map-Reduce Analysis (≥4 files)

For larger commits, each file is analyzed independently, then results are synthesized:

```
┌─────────────────────────────────────────────────────────────────┐
│               MAP-REDUCE (≥4 files, no changelog)               │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│                 ┌──► [1] Map file_1 ──┐                         │
│                 ├──► [2] Map file_2 ──┤                         │
│  diff ──► parse ┼──► [3] Map file_3 ──┼──► [N+1] Reduce ──┐     │
│           files ├──► ...              │      (Sonnet)     │     │
│                 └──► [N] Map file_N ──┘                   │     │
│                      (parallel, Sonnet)                   │     │
│                                                           ▼     │
│                                              [N+2] Summary ──► git
│                                                   (Haiku)       │
│                                                                 │
│  Total: N + 2 LLM calls                                         │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│               MAP-REDUCE + CHANGELOG (≥4 files)                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  [1] Changelog ──► parse ──┬──► [2..N+1] Map ──┐                │
│      (Sonnet)      files   │    (parallel)     │                │
│                            └───────────────────┤                │
│                                                ▼                │
│                               [N+2] Reduce ──► [N+3] Summary    │
│                                                                 │
│  Total: N + 3 LLM calls                                         │
└─────────────────────────────────────────────────────────────────┘
```

**Example costs:**
| Commit Size | Mode | LLM Calls | Approximate Cost |
|-------------|------|-----------|------------------|
| 2 files | Unified | 2-3 | ~$0.002 |
| 10 files | Map-Reduce | 12-13 | ~$0.015 |
| 50 files | Map-Reduce | 52-53 | ~$0.06 |

### Why Map-Reduce?

The map-reduce approach provides several benefits for larger commits:

1. **No truncation** - Each file gets full attention without information loss
2. **Parallel processing** - Map phase runs concurrently for faster analysis
3. **Better detail extraction** - Per-file focus captures more specific observations
4. **Accurate synthesis** - Reduce phase sees complete picture for classification

### Configuration

```toml
# Enable/disable map-reduce (default: true)
map_reduce_enabled = true
```

When disabled, all commits use unified analysis with smart truncation for large diffs

**Prompts:**
- `CONVENTIONAL_ANALYSIS_PROMPT` - Extracts 0-6 past-tense detail statements from diff
- `SUMMARY_PROMPT_TEMPLATE` - Creates ≤72 char summary from details + stat
- Both enforce past-tense verbs: `added`, `fixed`, `updated`, `refactored`, etc.

**Smart truncation strategy** (`src/diff.rs`):
- Shows ALL file headers even under length pressure
- Distributes remaining space proportionally by priority
- Keeps diff context (first 15 + last 10 lines per file)
- Annotates omitted files

## Implementation Notes

**Dependencies:**
- `clap` - CLI parsing with derive macros
- `reqwest` (blocking) - OpenRouter API via LiteLLM localhost:4000
- `serde` + `serde_json` - Function calling schema + response parsing
- `arboard` - Clipboard support for `--copy`
- `anyhow` + `thiserror` - Error handling

**API integration:**
- Uses OpenRouter's function calling API with structured output
- Two tools: `create_conventional_analysis` (detail extraction), `create_commit_summary` (summary creation)
- Supports trailing text arguments as user context to analysis phase
- Fallback to `fallback_summary()` if model calls fail
- Retry logic with exponential backoff for transient failures

**Validation rules:**
- Summary: ≤72 chars (guideline), ≤96 (soft limit), ≤128 (hard limit), past-tense verb, no trailing period
- Body: Past-tense verbs preferred, ends with periods
- Warns on present-tense usage but doesn't block

**Models:**
- Default: Sonnet 4.5 (`claude-sonnet-4.5`)
- Optional: Opus 4.1 (`claude-opus-4.1`) via `--opus`
- Summary creation: Haiku 4.5 (`claude-haiku-4-5-20251001`) hardcoded

## Rewrite Mode Details

Rewrite mode converts entire git histories to conventional commits format:

**Workflow:**
1. Extracts commit list via `git rev-list --reverse`
2. For each commit: analyzes diff and generates conventional message
3. Parallel API calls for faster processing
4. Rebuilds history with `git commit-tree`:
   - Preserves trees, authors, dates
   - Updates messages only
   - Maintains parent relationships
   - Updates branch ref to new head

**Safety:**
- Auto-creates timestamped backup branch
- `--rewrite-preview N` / `--rewrite-dry-run` modes
- Checks working tree is clean
- Preserves all commit metadata except message

**Cost:** ~$0.001-0.005/commit depending on model and diff size

## Configuration

Create `~/.config/llm-git/config.toml` to customize behavior:

```toml
# API Configuration
api_base_url = "http://localhost:4000"           # Override with LLM_GIT_API_URL
api_key = "your-api-key"                         # Optional, override with LLM_GIT_API_KEY

# HTTP Timeouts
request_timeout_secs = 120                       # Request timeout (default: 120s)
connect_timeout_secs = 30                        # Connection timeout (default: 30s)

# Models (supports any OpenAI-compatible API)
analysis_model = "claude-sonnet-4.5"             # Model for analysis phase
summary_model = "claude-haiku-4-5"               # Model for summary phase

# Commit Message Limits
summary_guideline = 72                           # Target length (conventional commits)
summary_soft_limit = 96                          # Triggers retry if exceeded
summary_hard_limit = 128                         # Absolute maximum

# Retry Configuration
max_retries = 3                                  # API retry attempts
initial_backoff_ms = 1000                        # Initial backoff delay

# Diff Processing
max_diff_length = 100000                         # Max diff size before truncation
wide_change_threshold = 0.50                     # Threshold for omitting scope (50%)

# Compose Mode
compose_max_rounds = 5                           # Max rounds for multi-commit generation

# Model Temperature
temperature = 0.2                                # Low for consistency (0.0-1.0)

# GPG Signing
gpg_sign = false                                 # Sign commits by default (or use --sign/-S)

# Changelog Maintenance
changelog_enabled = true                         # Auto-update CHANGELOG.md (default: true)

# File Exclusions
excluded_files = [                               # Files to exclude from diff
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    # ... add more
]

low_priority_extensions = [                      # Low-priority file extensions
    ".lock", ".toml", ".yaml", ".json", ".md",
    # ... add more
]

# Prompt Variants (advanced)
analysis_prompt_variant = "default"             # Prompt template variant
summary_prompt_variant = "default"              # Prompt template variant
exclude_old_message = true                       # Exclude old message in rewrite mode
```

### Configuration Examples

**LiteLLM (localhost):**
```toml
api_base_url = "http://localhost:4000"
# No api_key needed - LiteLLM handles authentication
analysis_model = "claude-sonnet-4.5"
summary_model = "claude-haiku-4-5"
```

**Anthropic Direct:**
```toml
api_base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-..."  # Or use LLM_GIT_API_KEY env var
analysis_model = "claude-sonnet-4.5-20250514"
summary_model = "claude-haiku-4-5-20250514"
```

**OpenRouter:**
```toml
api_base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-..."
analysis_model = "anthropic/claude-sonnet-4.5"
summary_model = "anthropic/claude-haiku-4-5"
```

**OpenAI:**
```toml
api_base_url = "https://api.openai.com/v1"
api_key = "sk-..."
analysis_model = "gpt-4o"
summary_model = "gpt-4o-mini"
temperature = 0.3  # OpenAI models may benefit from slightly higher temp
```

### Commit Types and Changelog Categories

Customize commit types (used in AI prompts) and changelog category mappings.

**Type order matters** — first type checked first in the decision tree:

```toml
# Global hint for cross-type disambiguation
classifier_hint = """
CRITICAL - feat vs refactor:
- feat: ANY observable behavior change OR new public API
- refactor: ONLY when provably unchanged (same tests, same API)
When in doubt, prefer feat over refactor.
"""

# Commit types with rich guidance for AI prompts
# Order defines priority: first type checked first in decision tree
[types.feat]
description = "New public API surface OR user-observable capability/behavior change"
diff_indicators = ["pub fn", "pub struct", "pub enum", "export function", "#[arg]"]
examples = [
    "Added pub fn process_batch() → feat (new API)",
    "Migrated HTTP client to async → feat (behavior change)",
]

[types.fix]
description = "Fixes incorrect behavior (bugs, crashes, wrong outputs, race conditions)"
diff_indicators = ["unwrap() → ?", "bounds check", "off-by-one", "error handling"]

[types.refactor]
description = "Internal restructuring with provably unchanged behavior"
diff_indicators = ["rename", "extract", "consolidate", "reorganize"]
examples = ["Renamed internal module structure → refactor (no API change)"]
hint = "Requires proof: same tests pass, same API. If behavior changes, use feat."

[types.docs]
description = "Documentation only changes"
file_patterns = ["*.md", "doc comments"]

[types.test]
description = "Adding or modifying tests"
file_patterns = ["*_test.rs", "tests/", "*.test.ts"]

[types.chore]
description = "Maintenance tasks, dependencies, tooling"
file_patterns = [".gitignore", "*.lock", "config files"]

[types.style]
description = "Formatting, whitespace changes (no logic change)"
diff_indicators = ["whitespace", "formatting"]
hint = "Variable/function renames are refactor, not style."

[types.perf]
description = "Performance improvements (proven faster)"
diff_indicators = ["optimization", "cache", "batch"]

[types.build]
description = "Build system, dependency changes"
file_patterns = ["Cargo.toml", "package.json", "Makefile"]

[types.ci]
description = "CI/CD configuration"
file_patterns = [".github/workflows/", ".gitlab-ci.yml"]

[types.revert]
description = "Reverts a previous commit"
diff_indicators = ["Revert"]
```

**Type configuration fields** (all optional except `description`):
- `description` — When to use this type
- `diff_indicators` — Code patterns that suggest this type
- `file_patterns` — File patterns that suggest this type
- `examples` — Example scenarios with rationale
- `hint` — Classification guidance for this type

**Changelog categories** control how commits are grouped in CHANGELOG.md:

```toml
# Categories listed in render order (first = appears first in changelog)
# Matching rules: body_contains is checked before types

[[categories]]
name = "Breaking"
header = "Breaking Changes"                      # Display header (defaults to name)
match.body_contains = ["breaking", "incompatible"]  # Match if body contains these

[[categories]]
name = "Added"
match.types = ["feat"]                           # Map commit types to this category

[[categories]]
name = "Changed"
default = true                                   # Fallback for unmatched types

[[categories]]
name = "Deprecated"
# No match rules - only used if explicitly set

[[categories]]
name = "Removed"
match.types = ["revert"]

[[categories]]
name = "Fixed"
match.types = ["fix"]

[[categories]]
name = "Security"
match.types = ["security"]
```

**Category matching logic:**
1. **Body matching first**: If commit body contains any `match.body_contains` pattern (case-insensitive), use that category
2. **Type matching**: Otherwise, match commit type against `match.types`
3. **Default fallback**: If no rules match, use the category with `default = true`

**Example: Adding a Performance category**
```toml
[[categories]]
name = "Performance"
match.types = ["perf"]
# Insert before "Changed" to render performance improvements prominently
```

**Example: Custom breaking change detection**
```toml
[[categories]]
name = "Breaking"
header = "⚠️ Breaking Changes"
match.body_contains = ["breaking", "incompatible", "BREAKING CHANGE", "migration required"]
```

## License

MIT
