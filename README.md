# llm-git

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
llm-git --opus                             # Use Opus model (more powerful)
llm-git --sonnet                           # Use Sonnet model (default)
llm-git Fixed regression from PR #123      # Add context (trailing text)
```

**History Rewrite (Python):**
```bash
pip install -r requirements.txt
./rewrite_history.py --preview 10          # Preview first 10 commits
./rewrite_history.py --dry-run             # Preview all without applying
./rewrite_history.py                       # Rewrite full history (creates backup)
./rewrite_history.py --start main~50       # Rewrite last 50 commits only
./rewrite_history.py --parallel 20         # Use 20 parallel API calls
```

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
- `src/git.rs` - Git command wrappers
- `src/normalization.rs` - Unicode normalization, formatting
- `src/types.rs` - Type-safe commit types, scopes, summaries
- `src/validation.rs` - Commit message validation

**Core workflow:**
1. `get_git_diff()` + `get_git_stat()` - Extract staged/unstaged/commit changes
2. `smart_truncate_diff()` - Priority-based diff truncation when >100KB:
   - Parse into `FileDiff` structs with priority scoring
   - Source files (rs/py/js) > config > tests > binaries > lock files
   - Excluded files: `Cargo.lock`, `package-lock.json`, etc. (see `EXCLUDED_FILES`)
   - Preserve headers for all files, truncate content proportionally
3. `generate_conventional_analysis()` - Call Sonnet/Opus with `CONVENTIONAL_ANALYSIS_PROMPT` using function calling
4. `generate_summary_from_analysis()` - Call Haiku with `SUMMARY_PROMPT_TEMPLATE` + detail points
5. `post_process_commit_message()` - Enforce length limits, punctuation, capitalization
6. `validate_commit_message()` - Check past-tense verbs, length, punctuation

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

## History Rewrite Tool

**Python script** (`rewrite_history.py`):

**Workflow:**
1. `get_commit_list()` - Extract commit hashes via `git rev-list --reverse`
2. `get_commit_info()` - For each commit: author, date, message, file list (no diff)
3. `call_haiku_api()` - Parallel Haiku calls to convert to conventional format
4. `rewrite_commit_messages()` - Rebuild history with `git commit-tree`:
   - Preserves trees, authors, dates
   - Updates messages only
   - Maintains parent relationships
   - Updates branch ref to new head

**Safety:**
- Auto-creates timestamped backup branch
- `--dry-run` / `--preview N` modes
- Checks working tree is clean
- Preserves all commit metadata except message

**Cost:** ~$0.001/commit with Haiku (~$1-5 for 1000-5000 commits)

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

## License

MIT
