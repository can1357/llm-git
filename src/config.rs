use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::{
   error::{CommitGenError, Result},
   types::{
      CategoryConfig, TypeConfig, default_categories, default_classifier_hint, default_types,
   },
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiMode {
   Auto,
   ChatCompletions,
   AnthropicMessages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedApiMode {
   ChatCompletions,
   AnthropicMessages,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CommitConfig {
   pub api_base_url: String,

   /// API mode for model endpoints (auto/chat-completions/anthropic-messages)
   #[serde(default = "default_api_mode")]
   pub api_mode: ApiMode,

   /// Optional API key for authentication (overridden by `LLM_GIT_API_KEY` env
   /// var)
   pub api_key: Option<String>,

   /// HTTP request timeout in seconds
   pub request_timeout_secs: u64,

   /// HTTP connection timeout in seconds
   pub connect_timeout_secs: u64,

   /// Disable git background/index features that are slow for short-lived CLI
   /// subprocesses.
   #[serde(default = "default_disable_git_background_features")]
   pub disable_git_background_features: bool,

   /// Maximum rounds for compose mode multi-commit generation
   pub compose_max_rounds: usize,

   pub summary_guideline:         usize,
   pub summary_soft_limit:        usize,
   pub summary_hard_limit:        usize,
   pub max_retries:               u32,
   pub initial_backoff_ms:        u64,
   #[serde(default = "default_auto_fast_threshold_lines")]
   pub auto_fast_threshold_lines: usize,
   pub max_diff_length:           usize,
   pub max_diff_tokens:           usize,
   pub wide_change_threshold:     f32,
   pub temperature:               f32,
   #[serde(default = "default_analysis_model")]
   pub analysis_model:            String,
   #[serde(default = "default_summary_model")]
   pub summary_model:             String,
   /// Legacy single-model config key. Parsed for backward compatibility and
   /// normalized into `analysis_model`, and into `summary_model` when the
   /// summary model was not set explicitly.
   #[serde(default, rename = "model")]
   pub legacy_model:              Option<String>,
   pub excluded_files:            Vec<String>,
   pub low_priority_extensions:   Vec<String>,

   /// Maximum token budget for commit message detail points (approx 4
   /// chars/token)
   pub max_detail_tokens: usize,

   /// Prompt variant for analysis phase (e.g., "default")
   #[serde(default = "default_analysis_prompt_variant")]
   pub analysis_prompt_variant: String,

   /// Prompt variant for summary phase (e.g., "default")
   #[serde(default = "default_summary_prompt_variant")]
   pub summary_prompt_variant: String,

   /// Enable abstract summaries for wide changes (cross-cutting refactors)
   #[serde(default = "default_wide_change_abstract")]
   pub wide_change_abstract: bool,

   /// Exclude old commit message from context in commit mode (rewrite mode uses
   /// this)
   #[serde(default = "default_exclude_old_message")]
   pub exclude_old_message: bool,

   /// GPG sign commits by default (can be overridden by --sign CLI flag)
   #[serde(default = "default_gpg_sign")]
   pub gpg_sign: bool,

   /// Add Signed-off-by trailer by default (can be overridden by --signoff CLI
   /// flag)
   #[serde(default = "default_signoff")]
   pub signoff: bool,

   /// Commit types with descriptions for AI prompts (order = priority)
   #[serde(default = "default_types")]
   pub types: IndexMap<String, TypeConfig>,

   /// Global hint for cross-type disambiguation
   #[serde(default = "default_classifier_hint")]
   pub classifier_hint: String,

   /// Changelog categories with matching rules (order = render order)
   #[serde(default = "default_categories")]
   pub categories: Vec<CategoryConfig>,

   /// Enable automatic changelog updates (default: true)
   #[serde(default = "default_changelog_enabled")]
   pub changelog_enabled: bool,

   /// Enable map-reduce for large diffs (default: true)
   #[serde(default = "default_map_reduce_enabled")]
   pub map_reduce_enabled: bool,

   /// Token threshold for triggering map-reduce (default: 30000 tokens)
   #[serde(default = "default_map_reduce_threshold")]
   pub map_reduce_threshold: usize,

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

const fn default_api_mode() -> ApiMode {
   ApiMode::Auto
}

const fn default_disable_git_background_features() -> bool {
   true
}

fn default_summary_prompt_variant() -> String {
   "default".to_string()
}

fn default_analysis_model() -> String {
   "claude-opus-4.5".to_string()
}

fn default_summary_model() -> String {
   "claude-haiku-4-5".to_string()
}

const fn default_wide_change_abstract() -> bool {
   true
}

const fn default_exclude_old_message() -> bool {
   true
}

const fn default_gpg_sign() -> bool {
   false
}

const fn default_signoff() -> bool {
   false
}

const fn default_changelog_enabled() -> bool {
   true
}

const fn default_map_reduce_enabled() -> bool {
   true
}

const fn default_map_reduce_threshold() -> usize {
   30000 // ~30k tokens, roughly 120k characters
}

const fn default_auto_fast_threshold_lines() -> usize {
   200
}

fn parse_api_mode(value: &str) -> ApiMode {
   match value.trim().to_lowercase().as_str() {
      "auto" => ApiMode::Auto,
      "chat" | "chat-completions" | "chat_completions" => ApiMode::ChatCompletions,
      "anthropic" | "messages" | "anthropic-messages" | "anthropic_messages" => {
         ApiMode::AnthropicMessages
      },
      _ => ApiMode::Auto,
   }
}

impl Default for CommitConfig {
   fn default() -> Self {
      Self {
         api_base_url: "http://localhost:4000".to_string(),
         api_mode: default_api_mode(),
         api_key: None,
         request_timeout_secs: 120,
         connect_timeout_secs: 30,
         disable_git_background_features: default_disable_git_background_features(),
         compose_max_rounds: 5,
         summary_guideline: 72,
         summary_soft_limit: 96,
         summary_hard_limit: 128,
         max_retries: 3,
         initial_backoff_ms: 1000,
         auto_fast_threshold_lines: default_auto_fast_threshold_lines(),
         max_diff_length: 100000, // Increased to handle larger refactors better
         max_diff_tokens: 25000,  // ~100K chars = 25K tokens (4 chars/token estimate)
         wide_change_threshold: 0.50,
         temperature: 0.2, // Low temperature for consistent structured output
         analysis_model: default_analysis_model(),
         summary_model: default_summary_model(),
         legacy_model: None,
         excluded_files: vec![
            // Rust
            "Cargo.lock".to_string(),
            // JavaScript/Node
            "package-lock.json".to_string(),
            "npm-shrinkwrap.json".to_string(),
            "yarn.lock".to_string(),
            "pnpm-lock.yaml".to_string(),
            "shrinkwrap.yaml".to_string(),
            "bun.lock".to_string(),
            "bun.lockb".to_string(),
            "deno.lock".to_string(),
            // PHP
            "composer.lock".to_string(),
            // Ruby
            "Gemfile.lock".to_string(),
            // Python
            "poetry.lock".to_string(),
            "Pipfile.lock".to_string(),
            "pdm.lock".to_string(),
            "uv.lock".to_string(),
            // Go
            "go.sum".to_string(),
            // Nix
            "flake.lock".to_string(),
            // Dart/Flutter
            "pubspec.lock".to_string(),
            // iOS/macOS
            "Podfile.lock".to_string(),
            "Packages.resolved".to_string(),
            // Elixir
            "mix.lock".to_string(),
            // .NET
            "packages.lock.json".to_string(),
            // Gradle
            "gradle.lockfile".to_string(),
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
         max_detail_tokens: 200,
         analysis_prompt_variant: default_analysis_prompt_variant(),
         summary_prompt_variant: default_summary_prompt_variant(),
         wide_change_abstract: default_wide_change_abstract(),
         exclude_old_message: default_exclude_old_message(),
         gpg_sign: default_gpg_sign(),
         signoff: default_signoff(),
         types: default_types(),
         classifier_hint: default_classifier_hint(),
         categories: default_categories(),
         changelog_enabled: default_changelog_enabled(),
         map_reduce_enabled: default_map_reduce_enabled(),
         map_reduce_threshold: default_map_reduce_threshold(),
         analysis_prompt: String::new(),
         summary_prompt: String::new(),
      }
   }
}

impl CommitConfig {
   pub fn resolved_api_mode(&self, _model_name: &str) -> ResolvedApiMode {
      match self.api_mode {
         ApiMode::ChatCompletions => ResolvedApiMode::ChatCompletions,
         ApiMode::AnthropicMessages => ResolvedApiMode::AnthropicMessages,
         ApiMode::Auto => {
            let base = self.api_base_url.to_lowercase();
            if base.contains("anthropic") {
               ResolvedApiMode::AnthropicMessages
            } else {
               ResolvedApiMode::ChatCompletions
            }
         },
      }
   }

   /// Load config from default location (~/.config/llm-git/config.toml)
   /// Falls back to Default if file doesn't exist or can't determine home
   /// directory Environment variables override config file values:
   /// - `LLM_GIT_API_URL` overrides `api_base_url`
   /// - `LLM_GIT_API_KEY` overrides `api_key`
   /// - `LLM_GIT_API_MODE` overrides `api_mode`
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
      config.normalize_models();

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

      if let Ok(api_mode) = std::env::var("LLM_GIT_API_MODE") {
         config.api_mode = parse_api_mode(&api_mode);
      }

      if let Ok(value) = std::env::var("LLM_GIT_DISABLE_GIT_BACKGROUND_FEATURES") {
         match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => config.disable_git_background_features = true,
            "0" | "false" | "no" | "off" => config.disable_git_background_features = false,
            _ => {},
         }
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
      config.normalize_models();

      config.load_prompts()?;
      Ok(config)
   }

   fn normalize_models(&mut self) {
      if let Some(model) = self.legacy_model.as_ref() {
         self.analysis_model = model.clone();
         if self.summary_model == default_summary_model() {
            self.summary_model = model.clone();
         }
      }
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

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_normalize_models_legacy_model_sets_summary_when_default() {
      let mut config = CommitConfig {
         legacy_model: Some("gpt-5.3-codex-spark".to_string()),
         ..CommitConfig::default()
      };

      config.normalize_models();

      assert_eq!(config.analysis_model, "gpt-5.3-codex-spark");
      assert_eq!(config.summary_model, "gpt-5.3-codex-spark");
      assert_eq!(config.legacy_model.as_deref(), Some("gpt-5.3-codex-spark"));
   }

   #[test]
   fn test_normalize_models_preserves_explicit_summary_model() {
      let mut config = CommitConfig {
         summary_model: "gpt-5-mini".to_string(),
         legacy_model: Some("gpt-5.3-codex-spark".to_string()),
         ..CommitConfig::default()
      };

      config.normalize_models();

      assert_eq!(config.analysis_model, "gpt-5.3-codex-spark");
      assert_eq!(config.summary_model, "gpt-5-mini");
   }
}
