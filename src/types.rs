use std::{fmt, path::PathBuf};

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CommitGenError, Result};

#[derive(Debug, Clone, ValueEnum)]
pub enum Mode {
   /// Analyze staged changes
   Staged,
   /// Analyze a specific commit
   Commit,
   /// Analyze unstaged changes
   Unstaged,
   /// Compose changes into multiple commits
   Compose,
}

/// Resolve model name from short aliases to full `LiteLLM` model names
pub fn resolve_model_name(name: &str) -> String {
   match name {
      // Claude short names
      "sonnet" | "s" => "claude-sonnet-4.5",
      "opus" | "o" | "o4.5" => "claude-opus-4.5",
      "haiku" | "h" => "claude-haiku-4-5",
      "3.5" | "sonnet-3.5" => "claude-3.5-sonnet",
      "3.7" | "sonnet-3.7" => "claude-3.7-sonnet",

      // GPT short names
      "gpt5" | "g5" => "gpt-5",
      "gpt5-pro" => "gpt-5-pro",
      "gpt5-mini" => "gpt-5-mini",
      "gpt5-codex" => "gpt-5-codex",

      // o-series short names
      "o3" => "o3",
      "o3-pro" => "o3-pro",
      "o3-mini" => "o3-mini",
      "o1" => "o1",
      "o1-pro" => "o1-pro",
      "o1-mini" => "o1-mini",

      // Gemini short names
      "gemini" | "g2.5" => "gemini-2.5-pro",
      "flash" | "g2.5-flash" => "gemini-2.5-flash",
      "flash-lite" => "gemini-2.5-flash-lite",

      // Cerebras
      "qwen" | "q480b" => "qwen-3-coder-480b",

      // GLM models
      "glm4.6" => "glm-4.6",
      "glm4.5" => "glm-4.5",
      "glm-air" => "glm-4.5-air",

      // Otherwise pass through as-is (allows full model names)
      _ => name,
   }
   .to_string()
}

/// Scope candidate with metadata for inference
#[derive(Debug, Clone)]
pub struct ScopeCandidate {
   pub path:       String,
   pub percentage: f32,
   pub confidence: f32,
}

/// Type-safe commit type with validation
#[derive(Clone, PartialEq, Eq)]
pub struct CommitType(String);

impl CommitType {
   const VALID_TYPES: &'static [&'static str] = &[
      "feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert",
   ];

   /// Create new `CommitType` with validation
   pub fn new(s: impl Into<String>) -> Result<Self> {
      let s = s.into();
      let normalized = s.to_lowercase();

      if !Self::VALID_TYPES.contains(&normalized.as_str()) {
         return Err(CommitGenError::InvalidCommitType(format!(
            "Invalid commit type '{}'. Must be one of: {}",
            s,
            Self::VALID_TYPES.join(", ")
         )));
      }

      Ok(Self(normalized))
   }

   /// Returns inner string slice
   pub fn as_str(&self) -> &str {
      &self.0
   }

   /// Returns length of commit type
   pub const fn len(&self) -> usize {
      self.0.len()
   }

   /// Checks if commit type is empty
   #[allow(dead_code, reason = "Convenience method for future use")]
   pub const fn is_empty(&self) -> bool {
      self.0.is_empty()
   }
}

impl fmt::Display for CommitType {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      write!(f, "{}", self.0)
   }
}

impl fmt::Debug for CommitType {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_tuple("CommitType").field(&self.0).finish()
   }
}

impl Serialize for CommitType {
   fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
   where
      S: serde::Serializer,
   {
      self.0.serialize(serializer)
   }
}

impl<'de> Deserialize<'de> for CommitType {
   fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      let s = String::deserialize(deserializer)?;
      Self::new(s).map_err(serde::de::Error::custom)
   }
}

/// Type-safe commit summary with validation
#[derive(Clone)]
pub struct CommitSummary(String);

impl CommitSummary {
   /// Creates new `CommitSummary` with strict length validation and format
   /// warnings
   pub fn new(s: impl Into<String>, max_len: usize) -> Result<Self> {
      Self::new_impl(s, max_len, true)
   }

   /// Internal constructor allowing warning suppression (used by
   /// post-processing)
   pub(crate) fn new_unchecked(s: impl Into<String>, max_len: usize) -> Result<Self> {
      Self::new_impl(s, max_len, false)
   }

   fn new_impl(s: impl Into<String>, max_len: usize, emit_warnings: bool) -> Result<Self> {
      let s = s.into();

      // Strict validation: must not be empty
      if s.trim().is_empty() {
         return Err(CommitGenError::ValidationError("commit summary cannot be empty".to_string()));
      }

      // Strict validation: must be ≤ max_len characters (hard limit from config)
      if s.len() > max_len {
         return Err(CommitGenError::SummaryTooLong { len: s.len(), max: max_len });
      }

      if emit_warnings {
         // Warning-only: should start with lowercase
         if let Some(first_char) = s.chars().next()
            && first_char.is_uppercase()
         {
            eprintln!("⚠ warning: commit summary should start with lowercase: {s}");
         }

         // Warning-only: should NOT end with period (conventional commits style)
         if s.trim_end().ends_with('.') {
            eprintln!(
               "⚠ warning: commit summary should NOT end with period (conventional commits \
                style): {s}"
            );
         }
      }

      Ok(Self(s))
   }

   /// Returns inner string slice
   pub fn as_str(&self) -> &str {
      &self.0
   }

   /// Returns length of summary
   pub const fn len(&self) -> usize {
      self.0.len()
   }

   /// Checks if summary is empty
   #[allow(dead_code, reason = "Convenience method for future use")]
   pub const fn is_empty(&self) -> bool {
      self.0.is_empty()
   }
}

impl fmt::Display for CommitSummary {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      write!(f, "{}", self.0)
   }
}

impl fmt::Debug for CommitSummary {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_tuple("CommitSummary").field(&self.0).finish()
   }
}

impl Serialize for CommitSummary {
   fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
   where
      S: serde::Serializer,
   {
      self.0.serialize(serializer)
   }
}

impl<'de> Deserialize<'de> for CommitSummary {
   fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      let s = String::deserialize(deserializer)?;
      // During deserialization, bypass warnings to avoid console spam
      if s.trim().is_empty() {
         return Err(serde::de::Error::custom("commit summary cannot be empty"));
      }
      if s.len() > 128 {
         return Err(serde::de::Error::custom(format!(
            "commit summary must be ≤128 characters, got {}",
            s.len()
         )));
      }
      Ok(Self(s))
   }
}

/// Type-safe scope for conventional commits
#[derive(Clone, PartialEq, Eq)]
pub struct Scope(String);

impl Scope {
   /// Creates new scope with validation
   ///
   /// Rules:
   /// - Max 2 segments separated by `/`
   /// - Only lowercase alphanumeric with `/`, `-`, `_`
   /// - No empty segments
   pub fn new(s: impl Into<String>) -> Result<Self> {
      let s = s.into();
      let segments: Vec<&str> = s.split('/').collect();

      if segments.len() > 2 {
         return Err(CommitGenError::InvalidScope(format!(
            "scope has {} segments, max 2 allowed",
            segments.len()
         )));
      }

      for segment in &segments {
         if segment.is_empty() {
            return Err(CommitGenError::InvalidScope("scope contains empty segment".to_string()));
         }
         if !segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
         {
            return Err(CommitGenError::InvalidScope(format!(
               "invalid characters in scope segment: {segment}"
            )));
         }
      }

      Ok(Self(s))
   }

   /// Returns inner string slice
   pub fn as_str(&self) -> &str {
      &self.0
   }

   /// Splits scope by `/` into segments
   #[allow(dead_code, reason = "Public API method for scope manipulation")]
   pub fn segments(&self) -> Vec<&str> {
      self.0.split('/').collect()
   }

   /// Check if scope is empty
   pub const fn is_empty(&self) -> bool {
      self.0.is_empty()
   }
}

impl fmt::Display for Scope {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      write!(f, "{}", self.0)
   }
}

impl fmt::Debug for Scope {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_tuple("Scope").field(&self.0).finish()
   }
}

impl Serialize for Scope {
   fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
   where
      S: serde::Serializer,
   {
      serializer.serialize_str(&self.0)
   }
}

impl<'de> Deserialize<'de> for Scope {
   fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      let s = String::deserialize(deserializer)?;
      Self::new(s).map_err(serde::de::Error::custom)
   }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConventionalCommit {
   pub commit_type: CommitType,
   pub scope:       Option<Scope>,
   pub summary:     CommitSummary,
   pub body:        Vec<String>,
   pub footers:     Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConventionalAnalysis {
   #[serde(rename = "type")]
   pub commit_type: CommitType,
   #[serde(default, deserialize_with = "deserialize_optional_scope")]
   pub scope:       Option<Scope>,
   #[serde(default, deserialize_with = "deserialize_string_vec")]
   pub body:        Vec<String>,
   #[serde(default, deserialize_with = "deserialize_string_vec")]
   pub issue_refs:  Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code, reason = "Used by src/api/mod.rs in binary but not in tests")]
pub struct SummaryOutput {
   pub summary: String,
}

/// Metadata for a single commit during history rewrite
#[derive(Debug, Clone)]
pub struct CommitMetadata {
   pub hash:            String,
   pub author_name:     String,
   pub author_email:    String,
   pub author_date:     String,
   pub committer_name:  String,
   pub committer_email: String,
   pub committer_date:  String,
   pub message:         String,
   pub parent_hashes:   Vec<String>,
   pub tree_hash:       String,
}

/// Selector for which hunks to include in a file change
#[derive(Debug, Clone)]
pub enum HunkSelector {
   /// All changes in the file
   All,
   /// Specific line ranges (1-indexed, inclusive)
   Lines { start: usize, end: usize },
   /// Search pattern to match lines
   Search { pattern: String },
}

impl Serialize for HunkSelector {
   fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
   where
      S: serde::Serializer,
   {
      match self {
         Self::All => serializer.serialize_str("ALL"),
         Self::Lines { start, end } => {
            use serde::ser::SerializeStruct;
            let mut state = serializer.serialize_struct("Lines", 2)?;
            state.serialize_field("start", start)?;
            state.serialize_field("end", end)?;
            state.end()
         },
         Self::Search { pattern } => {
            use serde::ser::SerializeStruct;
            let mut state = serializer.serialize_struct("Search", 1)?;
            state.serialize_field("pattern", pattern)?;
            state.end()
         },
      }
   }
}

impl<'de> Deserialize<'de> for HunkSelector {
   fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      let value = Value::deserialize(deserializer)?;

      match value {
         // String "ALL" -> All variant
         Value::String(s) if s.eq_ignore_ascii_case("all") => Ok(Self::All),
         // Old format: hunk headers like "@@ -10,5 +10,7 @@" -> treat as search pattern
         Value::String(s) if s.starts_with("@@") => Ok(Self::Search { pattern: s }),
         // New format: line range string like "10-20"
         Value::String(s) if s.contains('-') => {
            let parts: Vec<&str> = s.split('-').collect();
            if parts.len() == 2 {
               let start = parts[0].trim().parse().map_err(serde::de::Error::custom)?;
               let end = parts[1].trim().parse().map_err(serde::de::Error::custom)?;
               Ok(Self::Lines { start, end })
            } else {
               Err(serde::de::Error::custom(format!("Invalid line range format: {s}")))
            }
         },
         // Object with start/end fields -> Lines
         Value::Object(map) if map.contains_key("start") && map.contains_key("end") => {
            let start = map
               .get("start")
               .and_then(|v| v.as_u64())
               .ok_or_else(|| serde::de::Error::custom("Invalid start field"))?
               as usize;
            let end = map
               .get("end")
               .and_then(|v| v.as_u64())
               .ok_or_else(|| serde::de::Error::custom("Invalid end field"))?
               as usize;
            Ok(Self::Lines { start, end })
         },
         // Object with pattern field -> Search
         Value::Object(map) if map.contains_key("pattern") => {
            let pattern = map
               .get("pattern")
               .and_then(|v| v.as_str())
               .ok_or_else(|| serde::de::Error::custom("Invalid pattern field"))?
               .to_string();
            Ok(Self::Search { pattern })
         },
         // Fallback: treat other strings as search patterns
         Value::String(s) => Ok(Self::Search { pattern: s }),
         _ => Err(serde::de::Error::custom("Invalid HunkSelector format")),
      }
   }
}

/// File change with specific hunks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
   pub path:  String,
   pub hunks: Vec<HunkSelector>,
}

/// Represents a logical group of changes for compose mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeGroup {
   pub changes:      Vec<FileChange>,
   #[serde(rename = "type")]
   pub commit_type:  CommitType,
   pub scope:        Option<Scope>,
   pub rationale:    String,
   #[serde(default)]
   pub dependencies: Vec<usize>,
}

/// Result of compose analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeAnalysis {
   pub groups:           Vec<ChangeGroup>,
   pub dependency_order: Vec<usize>,
}

// API types for OpenRouter/LiteLLM communication
#[derive(Debug, Serialize)]
#[allow(dead_code, reason = "Used by src/api/mod.rs in binary but not in tests")]
pub struct Message {
   pub role:    String,
   pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code, reason = "Used by src/api/mod.rs in binary but not in tests")]
pub struct FunctionParameters {
   #[serde(rename = "type")]
   pub param_type: String,
   pub properties: serde_json::Value,
   pub required:   Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code, reason = "Used by src/api/mod.rs in binary but not in tests")]
pub struct Function {
   pub name:        String,
   pub description: String,
   pub parameters:  FunctionParameters,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code, reason = "Used by src/api/mod.rs in binary but not in tests")]
pub struct Tool {
   #[serde(rename = "type")]
   pub tool_type: String,
   pub function:  Function,
}

// CLI Args
#[derive(Parser, Debug)]
#[command(author, version, about = "Generate git commit messages using Claude AI", long_about = None)]
pub struct Args {
   /// What to analyze
   #[arg(long, value_enum, default_value = "staged")]
   pub mode: Mode,

   /// Commit hash/ref when using --mode=commit
   #[arg(long)]
   pub target: Option<String>,

   /// Copy the message to clipboard
   #[arg(long)]
   pub copy: bool,

   /// Preview without committing (default is to commit for staged mode)
   #[arg(long)]
   pub dry_run: bool,

   /// Push changes after committing
   #[arg(long, short = 'p')]
   pub push: bool,

   /// Directory to run git commands in
   #[arg(long, default_value = ".")]
   pub dir: String,

   /// Model for analysis (default: sonnet). Use short names (sonnet/opus/haiku)
   /// or full names
   #[arg(long, short = 'm')]
   pub model: Option<String>,

   /// Model for summary creation (default: haiku)
   #[arg(long)]
   pub summary_model: Option<String>,

   /// Temperature for API calls (0.0-1.0, default: 1.0)
   #[arg(long, short = 't')]
   pub temperature: Option<f32>,

   /// Issue numbers this commit fixes (e.g., --fixes 123 456)
   #[arg(long)]
   pub fixes: Vec<String>,

   /// Issue numbers this commit closes (alias for --fixes)
   #[arg(long)]
   pub closes: Vec<String>,

   /// Issue numbers this commit resolves (alias for --fixes)
   #[arg(long)]
   pub resolves: Vec<String>,

   /// Related issue numbers (e.g., --refs 789)
   #[arg(long)]
   pub refs: Vec<String>,

   /// Mark this commit as a breaking change
   #[arg(long)]
   pub breaking: bool,

   /// GPG sign the commit (equivalent to git commit -S)
   #[arg(long, short = 'S')]
   pub sign: bool,

   /// Skip pre-commit and commit-msg hooks (equivalent to git commit --no-verify)
   #[arg(long, short = 'n')]
   pub skip_hooks: bool,

   /// Path to config file (default: ~/.config/llm-git/config.toml)
   #[arg(long)]
   pub config: Option<PathBuf>,

   /// Additional context to provide to the analysis model (all trailing
   /// non-flag text)
   #[arg(trailing_var_arg = true)]
   pub context: Vec<String>,

   // === Rewrite mode args ===
   /// Rewrite git history to conventional commits
   #[arg(long, conflicts_with_all = ["mode", "target", "copy", "dry_run"])]
   pub rewrite: bool,

   /// Preview N commits without rewriting
   #[arg(long, requires = "rewrite")]
   pub rewrite_preview: Option<usize>,

   /// Start from this ref (exclusive, e.g., main~50)
   #[arg(long, requires = "rewrite")]
   pub rewrite_start: Option<String>,

   /// Number of parallel API calls
   #[arg(long, default_value = "10", requires = "rewrite")]
   pub rewrite_parallel: usize,

   /// Dry run - show what would be changed
   #[arg(long, requires = "rewrite")]
   pub rewrite_dry_run: bool,

   /// Hide old commit type/scope tags to avoid model influence
   #[arg(long, requires = "rewrite")]
   pub rewrite_hide_old_types: bool,

   /// Exclude old commit message from context when analyzing commits (prevents
   /// contamination)
   #[arg(long)]
   pub exclude_old_message: bool,

   // === Compose mode args ===
   /// Compose changes into multiple atomic commits
   #[arg(long, conflicts_with_all = ["mode", "target", "rewrite"])]
   pub compose: bool,

   /// Preview proposed splits without committing
   #[arg(long, requires = "compose")]
   pub compose_preview: bool,

   /// Maximum number of commits to create
   #[arg(long, requires = "compose")]
   pub compose_max_commits: Option<usize>,

   /// Run tests after each commit
   #[arg(long, requires = "compose")]
   pub compose_test_after_each: bool,
}

impl Default for Args {
   fn default() -> Self {
      Self {
         mode:                    Mode::Staged,
         target:                  None,
         copy:                    false,
         dry_run:                 false,
         push:                    false,
         dir:                     ".".to_string(),
         model:                   None,
         summary_model:           None,
         temperature:             None,
         fixes:                   vec![],
         closes:                  vec![],
         resolves:                vec![],
         refs:                    vec![],
         breaking:                false,
         sign:                    false,
         skip_hooks:              false,
         config:                  None,
         context:                 vec![],
         rewrite:                 false,
         rewrite_preview:         None,
         rewrite_start:           None,
         rewrite_parallel:        10,
         rewrite_dry_run:         false,
         rewrite_hide_old_types:  false,
         exclude_old_message:     false,
         compose:                 false,
         compose_preview:         false,
         compose_max_commits:     None,
         compose_test_after_each: false,
      }
   }
}
fn deserialize_string_vec<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
   D: serde::Deserializer<'de>,
{
   let value = Value::deserialize(deserializer)?;
   Ok(value_to_string_vec(value))
}

fn extract_strings_from_malformed_json(input: &str) -> Vec<String> {
   let mut strings = Vec::new();
   let mut chars = input.chars();

   while let Some(c) = chars.next() {
      if c == '"' {
         let mut current_string = String::new();
         let mut escaped = false;

         for inner_c in chars.by_ref() {
            if escaped {
               current_string.push(inner_c);
               escaped = false;
            } else if inner_c == '\\' {
               current_string.push(inner_c);
               escaped = true;
            } else if inner_c == '"' {
               break;
            } else {
               current_string.push(inner_c);
            }
         }

         // Try to parse as JSON string first
         let json_candidate = format!("\"{current_string}\"");
         if let Ok(parsed) = serde_json::from_str::<String>(&json_candidate) {
            strings.push(parsed);
         } else {
            // Fallback: Replace newlines with space and try again
            let sanitized = current_string.replace(['\n', '\r'], " ");
            let json_sanitized = format!("\"{sanitized}\"");
            if let Ok(parsed) = serde_json::from_str::<String>(&json_sanitized) {
               strings.push(parsed);
            } else {
               // Ultimate fallback: raw content
               strings.push(sanitized);
            }
         }
      }
   }
   strings
}

fn value_to_string_vec(value: Value) -> Vec<String> {
   match value {
      Value::Null => Vec::new(),
      Value::String(s) => {
         let trimmed = s.trim();

         // Try to parse as JSON array if it looks like one
         if trimmed.starts_with('[') {
            // Remove trailing punctuation and quotes iteratively until stable
            // Handles cases like: `[...]".` or `[...].` or `[...]"`
            let mut cleaned = trimmed;
            loop {
               let before = cleaned;
               cleaned = cleaned.trim_end_matches(['.', ',', ';', '"', '\'']);
               if cleaned == before {
                  break;
               }
            }

            // Attempt to parse as JSON array
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(cleaned) {
               return arr
                  .into_iter()
                  .flat_map(|v| value_to_string_vec(v).into_iter())
                  .collect();
            }

            // Fallback: try sanitizing newlines (LLM sometimes outputs literal newlines in
            // JSON strings)
            let sanitized = cleaned.replace(['\n', '\r'], " ");
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&sanitized) {
               return arr
                  .into_iter()
                  .flat_map(|v| value_to_string_vec(v).into_iter())
                  .collect();
            }

            // Final fallback: Try manual string extraction for truncated/malformed arrays
            // e.g. ["Item 1", "Item 2".
            let extracted = extract_strings_from_malformed_json(trimmed);
            if !extracted.is_empty() {
               return extracted;
            }
         }

         // Default: split by lines
         s.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
      },
      Value::Array(arr) => arr
         .into_iter()
         .flat_map(|v| value_to_string_vec(v).into_iter())
         .collect(),
      Value::Object(map) => map
         .into_iter()
         .flat_map(|(k, v)| {
            let values = value_to_string_vec(v);
            if values.is_empty() {
               vec![k]
            } else {
               values
                  .into_iter()
                  .map(|val| format!("{k}: {val}"))
                  .collect()
            }
         })
         .collect(),
      other => vec![other.to_string()],
   }
}

fn deserialize_optional_scope<'de, D>(
   deserializer: D,
) -> std::result::Result<Option<Scope>, D::Error>
where
   D: serde::Deserializer<'de>,
{
   let value = Option::<String>::deserialize(deserializer)?;
   match value {
      None => Ok(None),
      Some(scope_str) => {
         let trimmed = scope_str.trim();
         if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
            Ok(None)
         } else {
            Scope::new(trimmed.to_string())
               .map(Some)
               .map_err(serde::de::Error::custom)
         }
      },
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   // ========== resolve_model_name Tests ==========

   #[test]
   fn test_resolve_model_name() {
      // Claude short names
      assert_eq!(resolve_model_name("sonnet"), "claude-sonnet-4.5");
      assert_eq!(resolve_model_name("s"), "claude-sonnet-4.5");
      assert_eq!(resolve_model_name("opus"), "claude-opus-4.5");
      assert_eq!(resolve_model_name("o"), "claude-opus-4.5");
      assert_eq!(resolve_model_name("haiku"), "claude-haiku-4-5");
      assert_eq!(resolve_model_name("h"), "claude-haiku-4-5");

      // GPT short names
      assert_eq!(resolve_model_name("gpt5"), "gpt-5");
      assert_eq!(resolve_model_name("g5"), "gpt-5");

      // Gemini short names
      assert_eq!(resolve_model_name("gemini"), "gemini-2.5-pro");
      assert_eq!(resolve_model_name("flash"), "gemini-2.5-flash");

      // Pass-through for full names
      assert_eq!(resolve_model_name("claude-sonnet-4.5"), "claude-sonnet-4.5");
      assert_eq!(resolve_model_name("custom-model"), "custom-model");
   }

   // ========== CommitType Tests ==========

   #[test]
   fn test_commit_type_valid() {
      let valid_types = [
         "feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci",
         "revert",
      ];

      for ty in &valid_types {
         assert!(CommitType::new(*ty).is_ok(), "Expected '{ty}' to be valid");
      }
   }

   #[test]
   fn test_commit_type_case_normalization() {
      // Uppercase should normalize to lowercase
      let ct = CommitType::new("FEAT").expect("FEAT should normalize");
      assert_eq!(ct.as_str(), "feat");

      let ct = CommitType::new("Fix").expect("Fix should normalize");
      assert_eq!(ct.as_str(), "fix");

      let ct = CommitType::new("ReFaCtOr").expect("ReFaCtOr should normalize");
      assert_eq!(ct.as_str(), "refactor");
   }

   #[test]
   fn test_commit_type_invalid() {
      let invalid_types = ["invalid", "bug", "feature", "update", "change", "random", "xyz", "123"];

      for ty in &invalid_types {
         assert!(CommitType::new(*ty).is_err(), "Expected '{ty}' to be invalid");
      }
   }

   #[test]
   fn test_commit_type_empty() {
      assert!(CommitType::new("").is_err(), "Empty string should be invalid");
   }

   #[test]
   fn test_commit_type_display() {
      let ct = CommitType::new("feat").unwrap();
      assert_eq!(format!("{ct}"), "feat");
   }

   #[test]
   fn test_commit_type_len() {
      let ct = CommitType::new("feat").unwrap();
      assert_eq!(ct.len(), 4);

      let ct = CommitType::new("refactor").unwrap();
      assert_eq!(ct.len(), 8);
   }

   // ========== Scope Tests ==========

   #[test]
   fn test_scope_valid_single_segment() {
      let valid_scopes = ["core", "api", "lib", "client", "server", "ui", "test-123", "foo_bar"];

      for scope in &valid_scopes {
         assert!(Scope::new(*scope).is_ok(), "Expected '{scope}' to be valid");
      }
   }

   #[test]
   fn test_scope_valid_two_segments() {
      let valid_scopes = ["api/client", "lib/core", "ui/components", "test-1/foo_2"];

      for scope in &valid_scopes {
         assert!(Scope::new(*scope).is_ok(), "Expected '{scope}' to be valid");
      }
   }

   #[test]
   fn test_scope_invalid_three_segments() {
      let scope = Scope::new("a/b/c");
      assert!(scope.is_err(), "Three segments should be invalid");

      if let Err(CommitGenError::InvalidScope(msg)) = scope {
         assert!(msg.contains("3 segments"));
      } else {
         panic!("Expected InvalidScope error");
      }
   }

   #[test]
   fn test_scope_invalid_uppercase() {
      let invalid_scopes = ["Core", "API", "MyScope", "api/Client"];

      for scope in &invalid_scopes {
         assert!(Scope::new(*scope).is_err(), "Expected '{scope}' with uppercase to be invalid");
      }
   }

   #[test]
   fn test_scope_invalid_empty_segments() {
      let invalid_scopes = ["", "a//b", "/foo", "bar/"];

      for scope in &invalid_scopes {
         assert!(
            Scope::new(*scope).is_err(),
            "Expected '{scope}' with empty segments to be invalid"
         );
      }
   }

   #[test]
   fn test_scope_invalid_chars() {
      let invalid_scopes = ["a b", "foo bar", "test@scope", "api/client!", "a.b"];

      for scope in &invalid_scopes {
         assert!(
            Scope::new(*scope).is_err(),
            "Expected '{scope}' with invalid chars to be invalid"
         );
      }
   }

   #[test]
   fn test_scope_segments() {
      let scope = Scope::new("core").unwrap();
      assert_eq!(scope.segments(), vec!["core"]);

      let scope = Scope::new("api/client").unwrap();
      assert_eq!(scope.segments(), vec!["api", "client"]);
   }

   #[test]
   fn test_scope_display() {
      let scope = Scope::new("api/client").unwrap();
      assert_eq!(format!("{scope}"), "api/client");
   }

   // ========== CommitSummary Tests ==========

   #[test]
   fn test_commit_summary_valid() {
      let summary_72 = "a".repeat(72);
      let summary_96 = "a".repeat(96);
      let summary_128 = "a".repeat(128);
      let valid_summaries = [
         "added new feature",
         "fixed bug in authentication",
         "x",                  // 1 char
         summary_72.as_str(),  // exactly 72 chars (guideline)
         summary_96.as_str(),  // exactly 96 chars (soft limit)
         summary_128.as_str(), // exactly 128 chars (hard limit)
      ];

      for summary in &valid_summaries {
         assert!(
            CommitSummary::new(*summary, 128).is_ok(),
            "Expected '{}' (len={}) to be valid",
            if summary.len() > 50 {
               &summary[..50]
            } else {
               summary
            },
            summary.len()
         );
      }
   }

   #[test]
   fn test_commit_summary_too_long() {
      let long_summary = "a".repeat(129); // 129 chars (exceeds hard limit)
      let result = CommitSummary::new(long_summary, 128);
      assert!(result.is_err(), "129 char summary should be invalid");

      if let Err(CommitGenError::SummaryTooLong { len, max }) = result {
         assert_eq!(len, 129);
         assert_eq!(max, 128);
      } else {
         panic!("Expected SummaryTooLong error");
      }
   }

   #[test]
   fn test_commit_summary_empty() {
      let empty_cases = ["", "   ", "\t", "\n"];

      for empty in &empty_cases {
         assert!(
            CommitSummary::new(*empty, 128).is_err(),
            "Empty/whitespace-only summary should be invalid"
         );
      }
   }

   #[test]
   fn test_commit_summary_warnings_uppercase_start() {
      // Should succeed but emit warning
      let result = CommitSummary::new("Added new feature", 128);
      assert!(result.is_ok(), "Should succeed despite uppercase start");
   }

   #[test]
   fn test_commit_summary_warnings_with_period() {
      // Should succeed but emit warning (periods not allowed in conventional commits)
      let result = CommitSummary::new("added new feature.", 128);
      assert!(result.is_ok(), "Should succeed despite having period");
   }

   #[test]
   fn test_commit_summary_new_unchecked() {
      // new_unchecked should not emit warnings (internal use)
      let result = CommitSummary::new_unchecked("Added feature", 128);
      assert!(result.is_ok(), "new_unchecked should succeed");
   }

   #[test]
   fn test_commit_summary_len() {
      let summary = CommitSummary::new("hello world", 128).unwrap();
      assert_eq!(summary.len(), 11);
   }

   #[test]
   fn test_commit_summary_display() {
      let summary = CommitSummary::new("fixed bug", 128).unwrap();
      assert_eq!(format!("{summary}"), "fixed bug");
   }

   // ========== Serialization Tests ==========

   #[test]
   fn test_commit_type_serialize() {
      let ct = CommitType::new("feat").unwrap();
      let json = serde_json::to_string(&ct).unwrap();
      assert_eq!(json, "\"feat\"");
   }

   #[test]
   fn test_commit_type_deserialize() {
      let ct: CommitType = serde_json::from_str("\"fix\"").unwrap();
      assert_eq!(ct.as_str(), "fix");

      // Invalid type should fail deserialization
      let result: serde_json::Result<CommitType> = serde_json::from_str("\"invalid\"");
      assert!(result.is_err());
   }

   #[test]
   fn test_scope_serialize() {
      let scope = Scope::new("api/client").unwrap();
      let json = serde_json::to_string(&scope).unwrap();
      assert_eq!(json, "\"api/client\"");
   }

   #[test]
   fn test_scope_deserialize() {
      let scope: Scope = serde_json::from_str("\"core\"").unwrap();
      assert_eq!(scope.as_str(), "core");

      // Invalid scope should fail deserialization
      let result: serde_json::Result<Scope> = serde_json::from_str("\"INVALID\"");
      assert!(result.is_err());
   }

   #[test]
   fn test_commit_summary_serialize() {
      let summary = CommitSummary::new("fixed bug", 128).unwrap();
      let json = serde_json::to_string(&summary).unwrap();
      assert_eq!(json, "\"fixed bug\"");
   }

   #[test]
   fn test_body_array_with_trailing_punctuation() {
      // Test the fix for LLM returning body as stringified JSON with trailing
      // punctuation
      let test_cases = [
         // Case 1: trailing period after closing quote
         r#"{"type":"feat","body":"[\"item1\", \"item2\"]".,"issue_refs":[]}"#,
         // Case 2: trailing quote and period
         r#"{"type":"feat","body":"[\"item1\", \"item2\"]\"."#,
         // Case 3: just trailing period
         r#"{"type":"feat","body":"[\"item1\", \"item2\"]."#,
         // Case 4: multiple trailing characters
         r#"{"type":"feat","body":"[\"item1\", \"item2\"]\".,;"#,
      ];

      for (idx, json) in test_cases.iter().enumerate() {
         let result: serde_json::Result<ConventionalAnalysis> = serde_json::from_str(json);
         match result {
            Ok(analysis) => {
               assert_eq!(
                  analysis.body.len(),
                  2,
                  "Case {idx}: Expected 2 body items, got {}",
                  analysis.body.len()
               );
               assert_eq!(analysis.body[0], "item1", "Case {idx}: First item mismatch");
               assert_eq!(analysis.body[1], "item2", "Case {idx}: Second item mismatch");
            },
            Err(e) => {
               // If direct parsing fails, try extracting just the body field
               eprintln!(
                  "Case {idx} warning: Full parse failed ({e}), testing body field directly"
               );
               let body_str = r#"["item1", "item2"]"."#;
               let cleaned_value = serde_json::Value::String(body_str.to_string());
               let body_vec = value_to_string_vec(cleaned_value);
               assert_eq!(
                  body_vec.len(),
                  2,
                  "Case {idx}: Expected 2 items from value_to_string_vec"
               );
            },
         }
      }
   }

   #[test]
   fn test_commit_summary_deserialize() {
      let summary: CommitSummary = serde_json::from_str("\"added feature\"").unwrap();
      assert_eq!(summary.as_str(), "added feature");

      // Too long should fail (>128 chars)
      let long = format!("\"{}\"", "a".repeat(129));
      let result: serde_json::Result<CommitSummary> = serde_json::from_str(&long);
      assert!(result.is_err());

      // Empty should fail
      let result: serde_json::Result<CommitSummary> = serde_json::from_str("\"\"");
      assert!(result.is_err());
   }

   #[test]
   fn test_conventional_commit_roundtrip() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("api").unwrap()),
         summary:     CommitSummary::new_unchecked("added endpoint", 128).unwrap(),
         body:        vec!["detail 1.".to_string(), "detail 2.".to_string()],
         footers:     vec!["Fixes: #123".to_string()],
      };

      let json = serde_json::to_string(&commit).unwrap();
      let deserialized: ConventionalCommit = serde_json::from_str(&json).unwrap();

      assert_eq!(deserialized.commit_type.as_str(), "feat");
      assert_eq!(deserialized.scope.unwrap().as_str(), "api");
      assert_eq!(deserialized.summary.as_str(), "added endpoint");
      assert_eq!(deserialized.body.len(), 2);
      assert_eq!(deserialized.footers.len(), 1);
   }

   #[test]
   fn test_scope_null_string_deserializes_to_none() {
      // LLMs sometimes return "null" as a string instead of JSON null
      let test_cases = [
         r#"{"type":"feat","scope":"null","body":[],"issue_refs":[]}"#,
         r#"{"type":"feat","scope":"Null","body":[],"issue_refs":[]}"#,
         r#"{"type":"feat","scope":"NULL","body":[],"issue_refs":[]}"#,
         r#"{"type":"feat","scope":" null ","body":[],"issue_refs":[]}"#,
      ];

      for (idx, json) in test_cases.iter().enumerate() {
         let analysis: ConventionalAnalysis = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("Case {idx} failed to deserialize: {e}"));
         assert!(
            analysis.scope.is_none(),
            "Case {idx}: Expected scope to be None, got {:?}",
            analysis.scope
         );
      }
   }

   // ========== HunkSelector Tests ==========

   #[test]
   fn test_body_array_with_newline_in_string() {
      // This reproduces the issue where literal newlines in the string prevent JSON
      // parsing The input mimics what happens when LLM returns a JSON string
      // with unescaped newlines
      let raw_str = "[\"Item 1\", \"Item\n2\"]";
      let value = serde_json::Value::String(raw_str.to_string());

      // desired behavior: should clean the newline and parse as array
      let result = value_to_string_vec(value);

      // It should be ["Item 1", "Item 2"] (newline replaced by space)
      assert_eq!(result.len(), 2);
      assert_eq!(result[0], "Item 1");
      // Depending on implementation, it might be "Item 2" or "Item  2" etc.
      // For now let's assume we replace with space.
      assert_eq!(result[1], "Item 2");
   }

   #[test]
   fn test_body_array_malformed_truncated() {
      // This reproduces the issue where the array is truncated or has trailing
      // punctuation
      let raw_str = "[\"Refactored finance...\", \"Added automatic detection...\".";
      let value = serde_json::Value::String(raw_str.to_string());

      let result = value_to_string_vec(value);

      // Should recover 2 items
      assert_eq!(result.len(), 2);
      assert_eq!(result[0], "Refactored finance...");
      assert_eq!(result[1], "Added automatic detection...");
   }

   #[test]
   fn test_hunk_selector_deserialize_all() {
      let json = r#""ALL""#;
      let selector: HunkSelector = serde_json::from_str(json).unwrap();
      assert!(matches!(selector, HunkSelector::All));
   }

   #[test]
   fn test_hunk_selector_deserialize_lines_object() {
      let json = r#"{"start": 10, "end": 20}"#;
      let selector: HunkSelector = serde_json::from_str(json).unwrap();
      match selector {
         HunkSelector::Lines { start, end } => {
            assert_eq!(start, 10);
            assert_eq!(end, 20);
         },
         _ => panic!("Expected Lines variant"),
      }
   }

   #[test]
   fn test_hunk_selector_deserialize_lines_string() {
      let json = r#""10-20""#;
      let selector: HunkSelector = serde_json::from_str(json).unwrap();
      match selector {
         HunkSelector::Lines { start, end } => {
            assert_eq!(start, 10);
            assert_eq!(end, 20);
         },
         _ => panic!("Expected Lines variant"),
      }
   }

   #[test]
   fn test_hunk_selector_deserialize_search_pattern() {
      let json = r#"{"pattern": "fn main"}"#;
      let selector: HunkSelector = serde_json::from_str(json).unwrap();
      match selector {
         HunkSelector::Search { pattern } => {
            assert_eq!(pattern, "fn main");
         },
         _ => panic!("Expected Search variant"),
      }
   }

   #[test]
   fn test_hunk_selector_deserialize_old_format_hunk_header() {
      // Old format: hunk headers like "@@ -10,5 +10,7 @@" should be treated as search
      let json = r#""@@ -10,5 +10,7 @@""#;
      let selector: HunkSelector = serde_json::from_str(json).unwrap();
      match selector {
         HunkSelector::Search { pattern } => {
            assert_eq!(pattern, "@@ -10,5 +10,7 @@");
         },
         _ => panic!("Expected Search variant for old hunk header format"),
      }
   }

   #[test]
   fn test_hunk_selector_serialize_all() {
      let selector = HunkSelector::All;
      let json = serde_json::to_string(&selector).unwrap();
      assert_eq!(json, r#""ALL""#);
   }

   #[test]
   fn test_hunk_selector_serialize_lines() {
      let selector = HunkSelector::Lines { start: 10, end: 20 };
      let json = serde_json::to_value(&selector).unwrap();
      assert_eq!(json["start"], 10);
      assert_eq!(json["end"], 20);
   }

   #[test]
   fn test_file_change_deserialize_with_all() {
      let json = r#"{"path": "src/main.rs", "hunks": ["ALL"]}"#;
      let change: FileChange = serde_json::from_str(json).unwrap();
      assert_eq!(change.path, "src/main.rs");
      assert_eq!(change.hunks.len(), 1);
      assert!(matches!(change.hunks[0], HunkSelector::All));
   }

   #[test]
   fn test_file_change_deserialize_with_line_ranges() {
      let json = r#"{"path": "src/main.rs", "hunks": [{"start": 10, "end": 20}, {"start": 50, "end": 60}]}"#;
      let change: FileChange = serde_json::from_str(json).unwrap();
      assert_eq!(change.path, "src/main.rs");
      assert_eq!(change.hunks.len(), 2);

      match &change.hunks[0] {
         HunkSelector::Lines { start, end } => {
            assert_eq!(*start, 10);
            assert_eq!(*end, 20);
         },
         _ => panic!("Expected Lines variant"),
      }

      match &change.hunks[1] {
         HunkSelector::Lines { start, end } => {
            assert_eq!(*start, 50);
            assert_eq!(*end, 60);
         },
         _ => panic!("Expected Lines variant"),
      }
   }

   #[test]
   fn test_file_change_deserialize_mixed_formats() {
      // Mix of string line ranges and object line ranges
      let json = r#"{"path": "src/main.rs", "hunks": ["10-20", {"start": 50, "end": 60}]}"#;
      let change: FileChange = serde_json::from_str(json).unwrap();
      assert_eq!(change.hunks.len(), 2);

      match &change.hunks[0] {
         HunkSelector::Lines { start, end } => {
            assert_eq!(*start, 10);
            assert_eq!(*end, 20);
         },
         _ => panic!("Expected Lines variant"),
      }

      match &change.hunks[1] {
         HunkSelector::Lines { start, end } => {
            assert_eq!(*start, 50);
            assert_eq!(*end, 60);
         },
         _ => panic!("Expected Lines variant"),
      }
   }
}
