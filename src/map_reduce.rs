//! Map-reduce pattern for large diff analysis
//!
//! When diffs exceed the token threshold, this module splits analysis across
//! files, then synthesizes results for accurate classification.

use std::path::Path;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::{
   api::{OneShotSpec, run_oneshot, strict_json_schema},
   config::CommitConfig,
   diff::{FileDiff, parse_diff, reconstruct_diff},
   error::{CommitGenError, Result},
   templates,
   tokens::TokenCounter,
   types::ConventionalAnalysis,
};

/// Observation from a single file during map phase
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileObservation {
   pub file:         String,
   pub observations: Vec<String>,
   pub additions:    usize,
   pub deletions:    usize,
}

/// Minimum files to justify map-reduce overhead (below this, unified is fine)
const MIN_FILES_FOR_MAP_REDUCE: usize = 4;

/// Maximum tokens per file in map phase (leave headroom for prompt template +
/// context)
const MAX_FILE_TOKENS: usize = 50_000;

/// Check if map-reduce should be used
/// Always use map-reduce except for:
/// 1. Explicitly disabled in config
/// 2. Very small diffs (≤3 files) where overhead isn't worth it
pub fn should_use_map_reduce(diff: &str, config: &CommitConfig, counter: &TokenCounter) -> bool {
   if !config.map_reduce_enabled {
      return false;
   }

   let files = parse_diff(diff);
   let file_count = files
      .iter()
      .filter(|f| {
         !config
            .excluded_files
            .iter()
            .any(|ex| f.filename.ends_with(ex))
      })
      .count();

   // Use map-reduce for 4+ files, or if any single file would need truncation
   file_count >= MIN_FILES_FOR_MAP_REDUCE
      || files
         .iter()
         .any(|f| f.token_estimate(counter) > MAX_FILE_TOKENS)
}

/// Maximum files to include in context header (prevent token explosion)
const MAX_CONTEXT_FILES: usize = 20;

/// Generate context header summarizing other files for cross-file awareness
fn generate_context_header(files: &[FileDiff], current_file: &str) -> String {
   // Skip context header for very large commits (diminishing returns)
   if files.len() > 100 {
      return format!("(Large commit with {} total files)", files.len());
   }

   let mut lines = vec!["OTHER FILES IN THIS CHANGE:".to_string()];

   let other_files: Vec<_> = files
      .iter()
      .filter(|f| f.filename != current_file)
      .collect();

   let total_other = other_files.len();

   // Only show top files by change size if too many
   let to_show: Vec<&FileDiff> = if total_other > MAX_CONTEXT_FILES {
      let mut sorted = other_files;
      sorted.sort_by_key(|f| std::cmp::Reverse(f.additions + f.deletions));
      sorted.truncate(MAX_CONTEXT_FILES);
      sorted
   } else {
      other_files
   };

   for file in &to_show {
      let line_count = file.additions + file.deletions;
      let description = infer_file_description(&file.filename, &file.content);
      lines.push(format!("- {} ({} lines): {}", file.filename, line_count, description));
   }

   if to_show.len() < total_other {
      lines.push(format!("... and {} more files", total_other - to_show.len()));
   }

   if lines.len() == 1 {
      return String::new(); // No other files
   }

   lines.join("\n")
}

/// Infer a brief description of what a file likely contains based on
/// name/content
fn infer_file_description(filename: &str, content: &str) -> &'static str {
   let filename_lower = filename.to_lowercase();

   // Check filename patterns
   if filename_lower.contains("test") {
      return "test file";
   }
   if Path::new(filename)
      .extension()
      .is_some_and(|e| e.eq_ignore_ascii_case("md"))
   {
      return "documentation";
   }
   let ext = Path::new(filename).extension();
   if filename_lower.contains("config")
      || ext.is_some_and(|e| e.eq_ignore_ascii_case("toml"))
      || ext.is_some_and(|e| e.eq_ignore_ascii_case("yaml"))
      || ext.is_some_and(|e| e.eq_ignore_ascii_case("yml"))
   {
      return "configuration";
   }
   if filename_lower.contains("error") {
      return "error definitions";
   }
   if filename_lower.contains("type") {
      return "type definitions";
   }
   if filename_lower.ends_with("mod.rs") || filename_lower.ends_with("lib.rs") {
      return "module exports";
   }
   if filename_lower.ends_with("main.rs")
      || filename_lower.ends_with("main.go")
      || filename_lower.ends_with("main.py")
   {
      return "entry point";
   }

   // Check content patterns
   if content.contains("impl ") || content.contains("fn ") {
      return "implementation";
   }
   if content.contains("struct ") || content.contains("enum ") {
      return "type definitions";
   }
   if content.contains("async ") || content.contains("await") {
      return "async code";
   }

   "source code"
}

/// Map phase: analyze each file individually and extract observations
async fn map_phase(
   files: &[FileDiff],
   model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> Result<Vec<FileObservation>> {
   // Process files concurrently using futures stream
   let observations: Vec<Result<FileObservation>> = stream::iter(files.iter())
      .map(|file| async {
         if file.is_binary {
            return Ok(FileObservation {
               file:         file.filename.clone(),
               observations: vec!["Binary file changed.".to_string()],
               additions:    0,
               deletions:    0,
            });
         }

         let context_header = generate_context_header(files, &file.filename);
         // Truncate large files to fit API limits
         let mut file_clone = file.clone();
         let file_tokens = file_clone.token_estimate(counter);
         if file_tokens > MAX_FILE_TOKENS {
            let target_size = MAX_FILE_TOKENS * 4; // Convert tokens to chars
            file_clone.truncate(target_size);
            eprintln!(
               "  {} truncated {} ({} \u{2192} {} tokens)",
               crate::style::icons::WARNING,
               file.filename,
               file_tokens,
               file_clone.token_estimate(counter)
            );
         }

         let file_diff = reconstruct_diff(&[file_clone]);

         map_single_file(&file.filename, &file_diff, &context_header, model_name, config).await
      })
      .buffer_unordered(8)
      .collect()
      .await;

   // Collect results, failing fast on first error
   observations.into_iter().collect()
}

/// Analyze a single file and extract observations
async fn map_single_file(
   filename: &str,
   file_diff: &str,
   context_header: &str,
   model_name: &str,
   config: &CommitConfig,
 ) -> Result<FileObservation> {
   let parts = templates::render_map_prompt("default", filename, file_diff, context_header)?;
   let observation_schema = build_observation_schema();

   let response = run_oneshot::<FileObservationResponse>(
      config,
      &OneShotSpec {
         operation:        "map-reduce/map",
         model:            model_name,
         max_tokens:       1500,
         temperature:      config.temperature,
         prompt_family:    "map",
         prompt_variant:   "default",
         system_prompt:    &parts.system,
         user_prompt:      &parts.user,
         tool_name:        "create_file_observation",
         tool_description: "Extract observations from a single file's changes",
         schema:           &observation_schema,
         debug:            None,
      },
   )
   .await?;

   let mut observations = response.output.observations;
   if observations.is_empty() {
      let text_observations = response
         .text_content
         .as_deref()
         .map(parse_observations_from_text)
         .unwrap_or_default();

      if !text_observations.is_empty() {
         observations = text_observations;
      } else if response.stop_reason.as_deref() == Some("max_tokens") {
         crate::style::warn(
            "Anthropic stopped at max_tokens with empty observations; using fallback observation.",
         );
         let fallback_target = Path::new(filename)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(filename);
         observations = vec![format!("Updated {fallback_target}.")];
      } else {
         crate::style::warn(
            "Model returned empty observations; continuing with no observations.",
         );
      }
   }

   Ok(FileObservation {
      file: filename.to_string(),
      observations,
      additions: 0,
      deletions: 0,
   })
}

/// Reduce phase: synthesize all observations into final analysis
pub async fn reduce_phase(
   observations: &[FileObservation],
   stat: &str,
   scope_candidates: &str,
   model_name: &str,
   config: &CommitConfig,
 ) -> Result<ConventionalAnalysis> {
   let type_enum: Vec<&str> = config.types.keys().map(|s| s.as_str()).collect();
   let observations_json =
      serde_json::to_string_pretty(observations).unwrap_or_else(|_| "[]".to_string());

   let types_description = crate::api::format_types_description(config);
   let parts = templates::render_reduce_prompt(
      "default",
      &observations_json,
      stat,
      scope_candidates,
      Some(&types_description),
   )?;

   let analysis_schema = build_analysis_schema(&type_enum);
   let response = run_oneshot::<ConventionalAnalysis>(
      config,
      &OneShotSpec {
         operation:        "map-reduce/reduce",
         model:            model_name,
         max_tokens:       1500,
         temperature:      config.temperature,
         prompt_family:    "reduce",
         prompt_variant:   "default",
         system_prompt:    &parts.system,
         user_prompt:      &parts.user,
         tool_name:        "create_conventional_analysis",
         tool_description: "Analyze changes and classify as conventional commit with type, scope, details, and metadata",
         schema:           &analysis_schema,
         debug:            None,
      },
   )
   .await?;

   Ok(response.output)
}

/// Run full map-reduce pipeline for large diffs
pub async fn run_map_reduce(
   diff: &str,
   stat: &str,
   scope_candidates: &str,
   model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> Result<ConventionalAnalysis> {
   let mut files = parse_diff(diff);

   // Filter excluded files
   files.retain(|f| {
      !config
         .excluded_files
         .iter()
         .any(|excluded| f.filename.ends_with(excluded))
   });

   if files.is_empty() {
      return Err(CommitGenError::Other(
         "No relevant files to analyze after filtering".to_string(),
      ));
   }

   let file_count = files.len();
   crate::style::print_info(&format!("Running map-reduce on {file_count} files..."));

   // Map phase
   let observations = map_phase(&files, model_name, config, counter).await?;

   // Reduce phase
   reduce_phase(&observations, stat, scope_candidates, model_name, config).await
}


fn parse_observations_from_text(text: &str) -> Vec<String> {
   let trimmed = text.trim();
   if trimmed.is_empty() {
      return Vec::new();
   }

   if let Ok(obs) = serde_json::from_str::<FileObservationResponse>(trimmed) {
      return obs.observations;
   }

   trimmed
      .lines()
      .map(str::trim)
      .filter(|line| !line.is_empty())
      .map(|line| {
         line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line)
            .trim()
      })
      .filter(|line| !line.is_empty())
      .map(str::to_string)
      .collect()
}


#[derive(Debug, Deserialize)]
struct FileObservationResponse {
   #[serde(deserialize_with = "deserialize_observations")]
   observations: Vec<String>,
}

/// Deserialize observations flexibly: handles array, stringified array, or
/// bullet string
fn deserialize_observations<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
   D: serde::Deserializer<'de>,
{
   use std::fmt;

   use serde::de::{self, Visitor};

   struct ObservationsVisitor;

   impl<'de> Visitor<'de> for ObservationsVisitor {
      type Value = Vec<String>;

      fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
         formatter.write_str("an array of strings, a JSON array string, or a bullet-point string")
      }

      fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
      where
         A: de::SeqAccess<'de>,
      {
         let mut vec = Vec::new();
         while let Some(item) = seq.next_element::<String>()? {
            vec.push(item);
         }
         Ok(vec)
      }

      fn visit_str<E>(self, s: &str) -> std::result::Result<Self::Value, E>
      where
         E: de::Error,
      {
         Ok(parse_string_to_observations(s))
      }
   }

   deserializer.deserialize_any(ObservationsVisitor)
}

/// Parse a string into observations: handles JSON array string or bullet-point
/// string
fn parse_string_to_observations(s: &str) -> Vec<String> {
   let trimmed = s.trim();
   if trimmed.is_empty() {
      return Vec::new();
   }

   // Try parsing as JSON array first
   if trimmed.starts_with('[')
      && let Ok(arr) = serde_json::from_str::<Vec<String>>(trimmed)
   {
      return arr;
   }

   // Fall back to bullet-point parsing
   trimmed
      .lines()
      .map(str::trim)
      .filter(|line| !line.is_empty())
      .map(|line| {
         line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("• "))
            .unwrap_or(line)
            .trim()
            .to_string()
      })
      .filter(|line| !line.is_empty())
      .collect()
}

fn build_observation_schema() -> serde_json::Value {
   strict_json_schema(
      serde_json::json!({
         "observations": {
            "type": "array",
            "description": "List of factual observations about what changed in this file",
            "items": {
               "type": "string"
            }
         }
      }),
      &["observations"],
   )
}


fn build_analysis_schema(type_enum: &[&str]) -> serde_json::Value {
   strict_json_schema(
      serde_json::json!({
         "type": {
            "type": "string",
            "enum": type_enum,
            "description": "Commit type based on combined changes"
         },
         "scope": {
            "type": "string",
            "description": "Optional scope (module/component). Omit if unclear or multi-component."
         },
         "details": {
            "type": "array",
            "description": "Array of 0-6 detail items with changelog metadata.",
            "items": {
               "type": "object",
               "properties": {
                  "text": {
                     "type": "string",
                     "description": "Detail about change, starting with past-tense verb, ending with period"
                  },
                  "changelog_category": {
                     "type": "string",
                     "enum": ["Added", "Changed", "Fixed", "Deprecated", "Removed", "Security"],
                     "description": "Changelog category if user-visible. Omit for internal changes."
                  },
                  "user_visible": {
                     "type": "boolean",
                     "description": "True if this change affects users/API and should appear in changelog"
                  }
               },
               "required": ["text", "user_visible"]
            }
         },
         "issue_refs": {
            "type": "array",
            "description": "Issue numbers from context (e.g., ['#123', '#456']). Empty if none.",
            "items": {
               "type": "string"
            }
         }
      }),
      &["type", "details", "issue_refs"],
   )
}


#[cfg(test)]
mod tests {
   use super::*;
   use crate::tokens::TokenCounter;

   fn test_counter() -> TokenCounter {
      TokenCounter::new("http://localhost:4000", None, "claude-sonnet-4.5")
   }


   #[test]
   fn test_should_use_map_reduce_disabled() {
      let config = CommitConfig { map_reduce_enabled: false, ..Default::default() };
      let counter = test_counter();
      // Even with many files, disabled means no map-reduce
      let diff = r"diff --git a/a.rs b/a.rs
@@ -0,0 +1 @@
+a
diff --git a/b.rs b/b.rs
@@ -0,0 +1 @@
+b
diff --git a/c.rs b/c.rs
@@ -0,0 +1 @@
+c
diff --git a/d.rs b/d.rs
@@ -0,0 +1 @@
+d";
      assert!(!should_use_map_reduce(diff, &config, &counter));
   }

   #[test]
   fn test_should_use_map_reduce_few_files() {
      let config = CommitConfig::default();
      let counter = test_counter();
      // Only 2 files - below threshold
      let diff = r"diff --git a/a.rs b/a.rs
@@ -0,0 +1 @@
+a
diff --git a/b.rs b/b.rs
@@ -0,0 +1 @@
+b";
      assert!(!should_use_map_reduce(diff, &config, &counter));
   }

   #[test]
   fn test_should_use_map_reduce_many_files() {
      let config = CommitConfig::default();
      let counter = test_counter();
      // 5 files - above threshold
      let diff = r"diff --git a/a.rs b/a.rs
@@ -0,0 +1 @@
+a
diff --git a/b.rs b/b.rs
@@ -0,0 +1 @@
+b
diff --git a/c.rs b/c.rs
@@ -0,0 +1 @@
+c
diff --git a/d.rs d/d.rs
@@ -0,0 +1 @@
+d
diff --git a/e.rs b/e.rs
@@ -0,0 +1 @@
+e";
      assert!(should_use_map_reduce(diff, &config, &counter));
   }

   #[test]
   fn test_generate_context_header_empty() {
      let files = vec![FileDiff {
         filename:  "only.rs".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 10,
         deletions: 5,
         is_binary: false,
      }];
      let header = generate_context_header(&files, "only.rs");
      assert!(header.is_empty());
   }

   #[test]
   fn test_generate_context_header_multiple() {
      let files = vec![
         FileDiff {
            filename:  "src/main.rs".to_string(),
            header:    String::new(),
            content:   "fn main() {}".to_string(),
            additions: 10,
            deletions: 5,
            is_binary: false,
         },
         FileDiff {
            filename:  "src/lib.rs".to_string(),
            header:    String::new(),
            content:   "mod test;".to_string(),
            additions: 3,
            deletions: 1,
            is_binary: false,
         },
         FileDiff {
            filename:  "tests/test.rs".to_string(),
            header:    String::new(),
            content:   "#[test]".to_string(),
            additions: 20,
            deletions: 0,
            is_binary: false,
         },
      ];

      let header = generate_context_header(&files, "src/main.rs");
      assert!(header.contains("OTHER FILES IN THIS CHANGE:"));
      assert!(header.contains("src/lib.rs"));
      assert!(header.contains("tests/test.rs"));
      assert!(!header.contains("src/main.rs")); // Current file excluded
   }

   #[test]
   fn test_infer_file_description() {
      assert_eq!(infer_file_description("src/test_utils.rs", ""), "test file");
      assert_eq!(infer_file_description("README.md", ""), "documentation");
      assert_eq!(infer_file_description("config.toml", ""), "configuration");
      assert_eq!(infer_file_description("src/error.rs", ""), "error definitions");
      assert_eq!(infer_file_description("src/types.rs", ""), "type definitions");
      assert_eq!(infer_file_description("src/mod.rs", ""), "module exports");
      assert_eq!(infer_file_description("src/main.rs", ""), "entry point");
      assert_eq!(infer_file_description("src/api.rs", "fn call()"), "implementation");
      assert_eq!(infer_file_description("src/models.rs", "struct Foo"), "type definitions");
      assert_eq!(infer_file_description("src/unknown.xyz", ""), "source code");
   }

   #[test]
   fn test_parse_string_to_observations_json_array() {
      let input = r#"["item one", "item two", "item three"]"#;
      let result = parse_string_to_observations(input);
      assert_eq!(result, vec!["item one", "item two", "item three"]);
   }

   #[test]
   fn test_parse_string_to_observations_bullet_points() {
      let input = "- added new function\n- fixed bug in parser\n- updated tests";
      let result = parse_string_to_observations(input);
      assert_eq!(result, vec!["added new function", "fixed bug in parser", "updated tests"]);
   }

   #[test]
   fn test_parse_string_to_observations_asterisk_bullets() {
      let input = "* first change\n* second change";
      let result = parse_string_to_observations(input);
      assert_eq!(result, vec!["first change", "second change"]);
   }

   #[test]
   fn test_parse_string_to_observations_empty() {
      assert!(parse_string_to_observations("").is_empty());
      assert!(parse_string_to_observations("   ").is_empty());
   }

   #[test]
   fn test_deserialize_observations_array() {
      let json = r#"{"observations": ["a", "b", "c"]}"#;
      let result: FileObservationResponse =
         serde_json::from_str(json).expect("valid observation array JSON should deserialize");
      assert_eq!(result.observations, vec!["a", "b", "c"]);
   }

   #[test]
   fn test_deserialize_observations_stringified_array() {
      let json = r#"{"observations": "[\"a\", \"b\", \"c\"]"}"#;
      let result: FileObservationResponse = serde_json::from_str(json)
         .expect("valid stringified observation array JSON should deserialize");
      assert_eq!(result.observations, vec!["a", "b", "c"]);
   }

   #[test]
   fn test_deserialize_observations_bullet_string() {
      let json = r#"{"observations": "- updated function\n- fixed bug"}"#;
      let result: FileObservationResponse =
         serde_json::from_str(json).expect("valid bullet observation JSON should deserialize");
      assert_eq!(result.observations, vec!["updated function", "fixed bug"]);
   }
}
