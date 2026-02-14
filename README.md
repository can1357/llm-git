<p align="center">
  <img src="https://raw.githubusercontent.com/can1357/llm-git/main/assets/banner.png" alt="lgit">
</p>

<p align="center">
  <strong>LLM-powered git commit message generator</strong>
</p>

<p align="center">
  <a href="https://github.com/can1357/llm-git/actions"><img src="https://img.shields.io/github/actions/workflow/status/can1357/llm-git/ci.yml?style=flat&colorA=222222&colorB=3FB950" alt="CI"></a>
  <a href="https://crates.io/crates/llm-git"><img src="https://img.shields.io/crates/v/llm-git?style=flat&colorA=222222&colorB=dea584" alt="Crates.io"></a>
  <a href="https://github.com/can1357/llm-git/blob/main/LICENSE"><img src="https://img.shields.io/github/license/can1357/llm-git?style=flat&colorA=222222&colorB=58A6FF" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-nightly-dea584?style=flat&colorA=222222&logo=rust&logoColor=white" alt="Rust"></a>
</p>

<p align="center">
  Generates <a href="https://www.conventionalcommits.org">conventional commits</a> from git diffs using Claude AI or any OpenAI-compatible API.<br>
  Automatic changelog maintenance, multi-commit composition, and full history rewriting.
</p>

---

## Features

- **Conventional commits** — Generates properly formatted commit messages with type, scope, and past-tense summary (≤72 chars)
- **Automatic changelogs** — Maintains `CHANGELOG.md` following [Keep a Changelog](https://keepachangelog.com) format with monorepo support
- **Compose mode** — Splits large staged changes into multiple logical atomic commits
- **Rewrite mode** — Converts entire git history to conventional commits (with automatic backup)
- **Map-reduce analysis** — Parallel per-file analysis for large commits without truncation
- **Any LLM provider** — Works with Anthropic, OpenAI, OpenRouter, or any OpenAI-compatible API

## Quick Start

```bash
# Install
cargo install llm-git

# Configure (pick one)
export LLM_GIT_API_KEY=your_anthropic_key                    # Direct Anthropic
export LLM_GIT_API_URL=https://openrouter.ai/api/v1          # OpenRouter
litellm --port 4000                                           # Local proxy (default)

# Use
git add .
lgit                    # Analyze, update changelog, commit
lgit --dry-run          # Preview without committing
lgit --compose          # Split into multiple commits
```

## Usage

### Basic Commands

```bash
lgit                                # Analyze staged changes and commit
lgit --dry-run                      # Preview message without committing
lgit --copy                         # Copy message to clipboard
lgit -p                             # Commit and push
lgit -S                             # GPG sign the commit
lgit -s                             # Add Signed-off-by trailer
lgit --amend                        # Amend the previous commit
lgit > msg.txt                      # Save raw message to file (auto-detected pipe mode)
lgit --dry-run | git commit -F -    # Generate message and commit with custom git flags

# Modes
lgit --mode=unstaged                # Preview unstaged changes (no commit)
lgit --mode=commit --target=HEAD~1  # Analyze a specific commit

# Models
lgit -m opus                        # Use Opus for analysis (more capable)
lgit -m sonnet                      # Use Sonnet (default)

# Context
lgit Fixed regression from PR #123  # Add context via trailing text
lgit --fixes 123 456                # Add "Fixes #123, #456" to body
lgit --breaking                     # Mark as breaking change
```

### Compose Mode

Split staged changes into multiple logical commits:

```bash
lgit --compose                      # Propose and create atomic commits
lgit --compose --compose-preview    # Preview splits without committing
lgit --compose --compose-max-commits 5
lgit --compose --compose-test-after-each
```

### Rewrite Mode

Convert repository history to conventional commits:

```bash
lgit --rewrite                      # Rewrite full history (creates backup)
lgit --rewrite --rewrite-preview 10 # Preview first 10 commits
lgit --rewrite --rewrite-dry-run    # Show all changes without applying
lgit --rewrite --rewrite-start main~50  # Rewrite last 50 commits only
lgit --rewrite --rewrite-parallel 20    # 20 concurrent API calls
```

## Automatic Changelog

lgit automatically maintains `CHANGELOG.md` files when committing:

- **Auto-detection** — Finds all `CHANGELOG.md` files in your repository
- **Monorepo support** — Routes changes to the correct changelog based on file paths
- **Deduplication** — Skips entries semantically similar to existing ones
- **Category mapping** — Maps commit types to sections (Added, Fixed, Changed, etc.)

```
project/
├── CHANGELOG.md              ← covers: src/, docs/
├── packages/
│   ├── core/
│   │   └── CHANGELOG.md      ← covers: packages/core/**
│   └── cli/
│       └── CHANGELOG.md      ← covers: packages/cli/**
```

Disable with `--no-changelog` or `changelog_enabled = false` in config.

## Configuration

Create `~/.config/llm-git/config.toml`:

```toml
# API
api_base_url = "http://localhost:4000"    # Default: LiteLLM proxy
api_key = "sk-..."                        # Or use LLM_GIT_API_KEY env var

# Model
model = "claude-sonnet-4-5"               # Default model for all API calls

# Commit message limits
summary_guideline = 72                    # Target length
summary_soft_limit = 96                   # Triggers retry
summary_hard_limit = 128                  # Absolute max

# Features
changelog_enabled = true
map_reduce_enabled = true                 # Parallel analysis for large commits
temperature = 0.2

# Commit signing
gpg_sign = false                          # GPG sign commits by default (-S)
signoff = false                           # Add Signed-off-by trailer by default (-s)
```

### Provider Examples

**Anthropic Direct:**
```toml
api_base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-..."
```

**OpenRouter:**
```toml
api_base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-..."
model = "anthropic/claude-sonnet-4.5"
```

**OpenAI:**
```toml
api_base_url = "https://api.openai.com/v1"
api_key = "sk-..."
model = "gpt-4o"
```

### Commit Types

Customize commit type classification:

```toml
[types.feat]
description = "New public API or user-observable behavior change"
diff_indicators = ["pub fn", "pub struct", "export function"]

[types.fix]
description = "Fixes incorrect behavior"
diff_indicators = ["unwrap() → ?", "bounds check", "error handling"]

[types.refactor]
description = "Internal restructuring with unchanged behavior"
hint = "If behavior changes, use feat instead."
```

### Changelog Categories

```toml
[[categories]]
name = "Breaking"
header = "Breaking Changes"
match.body_contains = ["breaking", "incompatible"]

[[categories]]
name = "Added"
match.types = ["feat"]

[[categories]]
name = "Fixed"
match.types = ["fix"]

[[categories]]
name = "Changed"
default = true
```

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `LLM_GIT_API_URL` | API endpoint | `http://localhost:4000` |
| `LLM_GIT_API_KEY` | API key | none |
| `LLM_GIT_CONFIG` | Config file path | `~/.config/llm-git/config.toml` |
| `LLM_GIT_VERBOSE` | Debug output | `false` |

## Installation

### From crates.io

```bash
cargo install llm-git
```

### From source

```bash
git clone https://github.com/can1357/llm-git.git
cd llm-git
cargo install --path .
```

### Prerequisites

- Rust nightly toolchain
- Git
- API access (Anthropic, OpenAI, OpenRouter, or local LiteLLM proxy)

## License

MIT
