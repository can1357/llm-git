use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{CommitGenError, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CommitConfig {
   pub api_base_url: String,

   /// Optional API key for authentication (overridden by `LLM_GIT_API_KEY` env
   /// var)
   pub api_key: Option<String>,

   /// HTTP request timeout in seconds
   pub request_timeout_secs: u64,

   /// HTTP connection timeout in seconds
   pub connect_timeout_secs: u64,

   /// Maximum rounds for compose mode multi-commit generation
   pub compose_max_rounds: usize,

   pub summary_guideline:       usize,
   pub summary_soft_limit:      usize,
   pub summary_hard_limit:      usize,
   pub max_retries:             u32,
   pub initial_backoff_ms:      u64,
   pub max_diff_length:         usize,
   pub wide_change_threshold:   f32,
   pub temperature:             f32,
   pub analysis_model:          String,
   pub summary_model:           String,
   pub excluded_files:          Vec<String>,
   pub low_priority_extensions: Vec<String>,

   /// Prompt variant for analysis phase (e.g., "default")
   #[serde(default = "default_analysis_prompt_variant")]
   pub analysis_prompt_variant: String,

   /// Prompt variant for summary phase (e.g., "default")
   #[serde(default = "default_summary_prompt_variant")]
   pub summary_prompt_variant: String,

   /// Exclude old commit message from context in commit mode (rewrite mode uses
   /// this)
   #[serde(default = "default_exclude_old_message")]
   pub exclude_old_message: bool,

   /// Loaded analysis prompt (not in config file)
   #[serde(skip)]
   pub analysis_prompt: String,

   /// Loaded summary prompt (not in config file)
   #[serde(skip)]
   pub summary_prompt: String,
}

fn default_analysis_prompt_variant() -> String {
   "default".to_string()
}

fn default_summary_prompt_variant() -> String {
   "default".to_string()
}

const fn default_exclude_old_message() -> bool {
   true
}

impl Default for CommitConfig {
   fn default() -> Self {
      Self {
         api_base_url:            "http://localhost:4000".to_string(),
         api_key:                 None,
         request_timeout_secs:    120,
         connect_timeout_secs:    30,
         compose_max_rounds:      5,
         summary_guideline:       72,
         summary_soft_limit:      96,
         summary_hard_limit:      128,
         max_retries:             3,
         initial_backoff_ms:      1000,
         max_diff_length:         100000, // Increased to handle larger refactors better
         wide_change_threshold:   0.50,
         temperature:             0.2, // Low temperature for consistent structured output
         analysis_model:          "claude-sonnet-4.5".to_string(),
         summary_model:           "claude-haiku-4-5".to_string(),
         excluded_files:          vec![
            "Cargo.lock".to_string(),
            "package-lock.json".to_string(),
            "yarn.lock".to_string(),
            "pnpm-lock.yaml".to_string(),
            "composer.lock".to_string(),
            "Gemfile.lock".to_string(),
            "poetry.lock".to_string(),
            "flake.lock".to_string(),
            ".gitignore".to_string(),
         ],
         low_priority_extensions: vec![
            ".lock".to_string(),
            ".sum".to_string(),
            ".toml".to_string(),
            ".yaml".to_string(),
            ".yml".to_string(),
            ".json".to_string(),
            ".md".to_string(),
            ".txt".to_string(),
            ".log".to_string(),
            ".tmp".to_string(),
            ".bak".to_string(),
         ],
         analysis_prompt_variant: default_analysis_prompt_variant(),
         summary_prompt_variant:  default_summary_prompt_variant(),
         exclude_old_message:     default_exclude_old_message(),
         analysis_prompt:         String::new(),
         summary_prompt:          String::new(),
      }
   }
}

impl CommitConfig {
   /// Load config from default location (~/.config/llm-git/config.toml)
   /// Falls back to Default if file doesn't exist or can't determine home
   /// directory Environment variables override config file values:
   /// - `LLM_GIT_API_URL` overrides `api_base_url`
   /// - `LLM_GIT_API_KEY` overrides `api_key`
   pub fn load() -> Result<Self> {
      let config_path = if let Ok(custom_path) = std::env::var("LLM_GIT_CONFIG") {
         PathBuf::from(custom_path)
      } else {
         Self::default_config_path().unwrap_or_else(|_| PathBuf::new())
      };

      let mut config = if config_path.exists() {
         Self::from_file(&config_path)?
      } else {
         Self::default()
      };

      // Apply environment variable overrides
      Self::apply_env_overrides(&mut config);

      config.load_prompts()?;
      Ok(config)
   }

   /// Apply environment variable overrides to config
   fn apply_env_overrides(config: &mut Self) {
      if let Ok(api_url) = std::env::var("LLM_GIT_API_URL") {
         config.api_base_url = api_url;
      }

      if let Ok(api_key) = std::env::var("LLM_GIT_API_KEY") {
         config.api_key = Some(api_key);
      }
   }

   /// Load config from specific file
   pub fn from_file(path: &Path) -> Result<Self> {
      let contents = std::fs::read_to_string(path)
         .map_err(|e| CommitGenError::Other(format!("Failed to read config: {e}")))?;
      let mut config: Self = toml::from_str(&contents)
         .map_err(|e| CommitGenError::Other(format!("Failed to parse config: {e}")))?;

      // Apply environment variable overrides
      Self::apply_env_overrides(&mut config);

      config.load_prompts()?;
      Ok(config)
   }

   /// Load prompts - templates are now loaded dynamically via Tera
   /// This method ensures prompts are initialized
   fn load_prompts(&mut self) -> Result<()> {
      // Ensure prompts directory exists and embedded templates are unpacked
      crate::templates::ensure_prompts_dir()?;

      // Templates loaded dynamically at render time
      self.analysis_prompt = String::new();
      self.summary_prompt = String::new();
      Ok(())
   }

   /// Get default config path (platform-safe)
   /// Tries HOME (Unix/Linux/macOS) then USERPROFILE (Windows)
   pub fn default_config_path() -> Result<PathBuf> {
      // Try HOME first (Unix/Linux/macOS)
      if let Ok(home) = std::env::var("HOME") {
         return Ok(PathBuf::from(home).join(".config/llm-git/config.toml"));
      }

      // Try USERPROFILE on Windows
      if let Ok(home) = std::env::var("USERPROFILE") {
         return Ok(PathBuf::from(home).join(".config/llm-git/config.toml"));
      }

      Err(CommitGenError::Other("No home directory found (tried HOME and USERPROFILE)".to_string()))
   }
}

/// Valid past-tense verbs for commit messages
pub const PAST_TENSE_VERBS: &[&str] = &[
   "added",
   "fixed",
   "updated",
   "refactored",
   "removed",
   "replaced",
   "improved",
   "implemented",
   "migrated",
   "renamed",
   "moved",
   "merged",
   "split",
   "extracted",
   "restructured",
   "reorganized",
   "consolidated",
   "simplified",
   "optimized",
   "documented",
   "tested",
   "changed",
   "introduced",
   "deprecated",
   "deleted",
   "corrected",
   "enhanced",
   "reverted",
];

#[allow(dead_code, reason = "Defined in src/api/prompts.rs where it is used")]
pub const CONVENTIONAL_ANALYSIS_PROMPT: &str = r#"
Analyze git changes and classify as a conventional commit with detail points.

OVERVIEW OF CHANGES:
```
{stat}
```

COMMIT TYPE (choose one):
- feat: New public API, function, or user-facing capability (even with refactoring)
- fix: Bug fix or correction
- refactor: Code restructuring with SAME behavior (no new capability)
- docs: Documentation-only changes
- test: Test additions/modifications
- chore: Tooling, dependencies, maintenance (no production code)
- style: Formatting, whitespace (no logic change)
- perf: Performance optimization
- build: Build system, dependencies (Cargo.toml, package.json)
- ci: CI/CD configuration (.github/workflows, etc)
- revert: Reverts a previous commit

TYPE CLASSIFICATION (CRITICAL):
✓ feat: New public functions, API endpoints, features, capabilities users can invoke
  - "Added TLS support with new builder API" → feat (new capability)
  - "Implemented JSON-LD iterator traits" → feat (new API surface)
✗ refactor: ONLY when behavior unchanged
  - "Replaced polling with event model" → feat if new behavior; refactor if same output
  - "Migrated from HTTP to gRPC" → feat (protocol change affects behavior)
  - "Renamed internal functions" → refactor (no user-visible change)

RULE: Be neutral between feat and refactor. Feat requires NEW capability/behavior. Refactor requires PROOF of unchanged behavior.

CRITICAL REFACTOR vs FEAT DISTINCTION:
When deciding between 'feat' and 'refactor', ask: "Can users observe different behavior?"

- refactor: Same external behavior, different internal structure
  ✗ "Migrated HTTP client to async" → feat (behavior change: now async)
  ✓ "Reorganized HTTP client modules" → refactor (no behavior change)

- feat: New behavior users can observe/invoke
  ✓ "Added async HTTP client support" → feat (new capability)
  ✓ "Implemented TLS transport layer" → feat (new feature)
  ✓ "Migrated from polling to event-driven model" → feat (observable change)

GUIDELINE: If the diff adds new public APIs, changes protocols, or enables new capabilities → feat
If the diff just reorganizes code without changing what it does → refactor

OTHER HEURISTICS:
- Commit message starts with "Revert" → revert
- Bug keywords, test fixes → fix
- Only .md/doc comments → docs
- Only test files → test
- Lock files, configs, .gitignore → chore
- Only formatting → style
- Optimization (proven faster) → perf
- Build scripts, dependency updates → build
- CI config files → ci

SCOPE EXTRACTION (optional):
SCOPE SUGGESTIONS (derived from changed files with line-count weights): {scope_candidates}
- You may use a suggested scope above, infer a more specific two-segment scope (e.g., core/utime), or omit when changes are broad
- Scopes MUST reflect actual directories from the diff, not invented names
- Use slash-separated paths (e.g., core/utime) when changes focus on a specific submodule
- Omit scope when: multi-component changes, cross-cutting concerns, or unclear focus
- Special cases (even if not suggested): "toolchain", "deps", "config"
- Format: lowercase alphanumeric with `/`, `-`, or `_` only (max 2 segments)

ISSUE REFERENCE EXTRACTION:
- Extract issue numbers from context (e.g. #123, GH-456)
- Return as array of strings or empty array if none

DETAIL REQUIREMENTS (0-6 items, prefer 3-4):
1. Past-tense verb ONLY: added, fixed, updated, refactored, removed, replaced,
   improved, implemented, migrated, renamed, moved, merged, split, extracted,
   restructured, reorganized, consolidated, simplified, optimized
2. End with period
3. Balance WHAT changed with WHY/HOW (not just "what")
4. Abstraction levels (prefer higher):
   - Level 3 (BEST): Architectural impact, user-facing change, performance gain
     "Replaced polling with event-driven model for 10x throughput."
   - Level 2 (GOOD): Component changes, API surface
     "Consolidated three HTTP builders into unified API."
   - Level 1 (AVOID): Low-level details, renames
     "Renamed workspacePath to locate." ❌
5. Group ≥3 similar changes: "Updated 5 test files for new API." not 5 bullets
6. Prioritize: user-visible > performance/security > architecture > internal refactoring
7. Empty array if no supporting details needed

EXCLUDE FROM DETAILS:
- Import/use statements
- Whitespace/formatting/indentation
- Trivial renames (unless part of larger API change)
- Debug prints/temporary logging
- Comment changes (unless substantial docs)
- File moves without modification
- Single-line tweaks/typo fixes
- Internal implementation details invisible to users

WRITING RULES:
- Plain sentences only (bullets/numbering added during formatting)
- Short, direct (120 chars max per detail)
- Precise nouns (module/file/API names)
- Group related changes
- Include why or how validated when meaningful:
  Added retry logic to handle transient network failures.
  Migrated to async I/O to unblock event loop.
- Avoid meta phrases (This commit, Updated code, etc)

DETAILED DIFF:
```diff
{diff}
```"#;

#[allow(dead_code, reason = "Defined in src/api/prompts.rs where it is used")]
pub const SUMMARY_PROMPT_TEMPLATE: &str = r#"
Draft a conventional commit summary (WITHOUT type/scope prefix).

COMMIT TYPE: {type}
SCOPE: {scope}

DETAIL POINTS:
{details}

DIFF STAT:
```
{stat}
```

SUMMARY REQUIREMENTS:
1. Output ONLY the description part (after "type(scope): ")
2. Maximum {chars} characters
3. First word MUST be one of these past-tense verbs:
   added, fixed, updated, removed, replaced, improved, implemented,
   migrated, renamed, moved, merged, split, extracted, simplified,
   optimized, documented, tested, changed, introduced, deprecated,
   deleted, corrected, enhanced, restructured, reorganized, consolidated,
   reverted
4. Focus on primary change (single concept if scope is specific)
5. NO trailing period (conventional commits style)
6. NO leading adjectives before verb

FORBIDDEN PATTERNS:
- DO NOT repeat the commit type "{type}" in the summary
- If type is "refactor", use: restructured, reorganized, migrated, simplified,
  consolidated, extracted (NOT "refactored")
- NO filler words: "comprehensive", "improved", "enhanced", "various", "several"
- NO "and" conjunctions cramming multiple unrelated concepts

GOOD EXAMPLES (type in parens):
- (feat) "added TLS support with mutual authentication"
- (refactor) "migrated HTTP transport to unified builder API"
- (fix) "corrected race condition in connection pool"
- (perf) "optimized batch processing to reduce allocations"

BAD EXAMPLES:
- (refactor) "refactor TLS configuration" ❌ (repeats type)
- (feat) "add comprehensive support for..." ❌ (filler word)
- (chore) "update deps and improve build" ❌ (multiple concepts)

FULL FORMAT WILL BE: {type}({scope}): <your summary>

BEFORE RESPONDING:
✓ Summary ≤{chars} chars
✓ Starts lowercase
✓ First word is past-tense verb from list above
✓ Does NOT repeat type "{type}"
✓ NO trailing period
✓ NO filler words
✓ Single focused concept
✓ Aligns with detail points and diff stat
✓ Specific (names subsystem/artifact)
"#;
