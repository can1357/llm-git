//! Map-reduce pattern for large diff analysis
//!
//! When diffs exceed the token threshold, this module splits analysis across files,
//! then synthesizes results for accurate classification.

use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
   api::retry_api_call,
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

/// Maximum tokens per file in map phase (leave headroom for prompt template + context)
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
      || files.iter().any(|f| f.token_estimate(counter) > MAX_FILE_TOKENS)
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

/// Infer a brief description of what a file likely contains based on name/content
fn infer_file_description(filename: &str, content: &str) -> &'static str {
   let filename_lower = filename.to_lowercase();

   // Check filename patterns
   if filename_lower.contains("test") {
      return "test file";
   }
   if Path::new(filename).extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
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
   if filename_lower.ends_with("main.rs") || filename_lower.ends_with("main.go")
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
fn map_phase(
   files: &[FileDiff],
   model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> Result<Vec<FileObservation>> {

   // Process files in parallel using rayon
   let observations: Vec<Result<FileObservation>> = files
      .par_iter()
      .map(|file| {
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
               "  {} truncated {} ({} → {} tokens)",
               crate::style::icons::WARNING,
               file.filename,
               file_tokens,
               file_clone.token_estimate(counter)
            );
         }

         let file_diff = reconstruct_diff(&[file_clone]);

         map_single_file(&file.filename, &file_diff, &context_header, model_name, config)
      })
      .collect();

   // Collect results, failing fast on first error
   observations.into_iter().collect()
}

/// Analyze a single file and extract observations
fn map_single_file(
   filename: &str,
   file_diff: &str,
   context_header: &str,
   model_name: &str,
   config: &CommitConfig,
) -> Result<FileObservation> {
   retry_api_call(config, || {
      let client = build_client(config);

      let tool = build_observation_tool();

      let prompt = templates::render_map_prompt("default", filename, file_diff, context_header)?;

      let request = build_api_request(model_name, config.temperature, vec![tool], &prompt);

      let mut request_builder = client
         .post(format!("{}/chat/completions", config.api_base_url))
         .header("content-type", "application/json");

      if let Some(ref api_key) = config.api_key {
         request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
      }

      let response = request_builder
         .json(&request)
         .send()
         .map_err(CommitGenError::HttpError)?;

      let status = response.status();

      if status.is_server_error() {
         let error_text = response.text().unwrap_or_else(|_| "Unknown error".to_string());
         eprintln!("{}", crate::style::error(&format!("Server error {status}: {error_text}")));
         return Ok((true, None)); // Retry
      }

      if !status.is_success() {
         let error_text = response.text().unwrap_or_else(|_| "Unknown error".to_string());
         return Err(CommitGenError::ApiError { status: status.as_u16(), body: error_text });
      }

      let api_response: ApiResponse = response.json().map_err(CommitGenError::HttpError)?;

      if api_response.choices.is_empty() {
         return Err(CommitGenError::Other(
            "API returned empty response for file observation".to_string(),
         ));
      }

      let message = &api_response.choices[0].message;

      if !message.tool_calls.is_empty() {
         let tool_call = &message.tool_calls[0];
         if tool_call.function.name == "create_file_observation" {
            let args = &tool_call.function.arguments;
            if args.is_empty() {
               return Err(CommitGenError::Other(
                  "Model returned empty function arguments for observation".to_string(),
               ));
            }

            let obs: FileObservationResponse = serde_json::from_str(args).map_err(|e| {
               CommitGenError::Other(format!("Failed to parse observation response: {e}"))
            })?;

            return Ok((false, Some(FileObservation {
               file:         filename.to_string(),
               observations: obs.observations,
               additions:    0, // Will be filled from FileDiff
               deletions:    0,
            })));
         }
      }

      // Fallback: try to parse content
      if let Some(content) = &message.content {
         let obs: FileObservationResponse =
            serde_json::from_str(content.trim()).map_err(CommitGenError::JsonError)?;
         return Ok((false, Some(FileObservation {
            file:         filename.to_string(),
            observations: obs.observations,
            additions:    0,
            deletions:    0,
         })));
      }

      Err(CommitGenError::Other("No observation found in API response".to_string()))
   })
}

/// Reduce phase: synthesize all observations into final analysis
pub fn reduce_phase(
   observations: &[FileObservation],
   stat: &str,
   scope_candidates: &str,
   model_name: &str,
   config: &CommitConfig,
) -> Result<ConventionalAnalysis> {
   retry_api_call(config, || {
      let client = build_client(config);

      // Build type enum from config
      let type_enum: Vec<&str> = config.types.keys().map(|s| s.as_str()).collect();

      let tool = build_analysis_tool(&type_enum);

      let observations_json =
         serde_json::to_string_pretty(observations).unwrap_or_else(|_| "[]".to_string());

      let types_description = crate::api::format_types_description(config);
      let prompt = templates::render_reduce_prompt(
         "default",
         &observations_json,
         stat,
         scope_candidates,
         Some(&types_description),
      )?;

      let request = build_api_request(model_name, config.temperature, vec![tool], &prompt);

      let mut request_builder = client
         .post(format!("{}/chat/completions", config.api_base_url))
         .header("content-type", "application/json");

      if let Some(ref api_key) = config.api_key {
         request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
      }

      let response = request_builder
         .json(&request)
         .send()
         .map_err(CommitGenError::HttpError)?;

      let status = response.status();

      if status.is_server_error() {
         let error_text = response.text().unwrap_or_else(|_| "Unknown error".to_string());
         eprintln!("{}", crate::style::error(&format!("Server error {status}: {error_text}")));
         return Ok((true, None)); // Retry
      }

      if !status.is_success() {
         let error_text = response.text().unwrap_or_else(|_| "Unknown error".to_string());
         return Err(CommitGenError::ApiError { status: status.as_u16(), body: error_text });
      }

      let api_response: ApiResponse = response.json().map_err(CommitGenError::HttpError)?;

      if api_response.choices.is_empty() {
         return Err(CommitGenError::Other(
            "API returned empty response for synthesis".to_string(),
         ));
      }

      let message = &api_response.choices[0].message;

      if !message.tool_calls.is_empty() {
         let tool_call = &message.tool_calls[0];
         if tool_call.function.name == "create_conventional_analysis" {
            let args = &tool_call.function.arguments;
            if args.is_empty() {
               return Err(CommitGenError::Other(
                  "Model returned empty function arguments for synthesis".to_string(),
               ));
            }

            let analysis: ConventionalAnalysis = serde_json::from_str(args).map_err(|e| {
               CommitGenError::Other(format!("Failed to parse synthesis response: {e}"))
            })?;

            return Ok((false, Some(analysis)));
         }
      }

      // Fallback
      if let Some(content) = &message.content {
         let analysis: ConventionalAnalysis =
            serde_json::from_str(content.trim()).map_err(CommitGenError::JsonError)?;
         return Ok((false, Some(analysis)));
      }

      Err(CommitGenError::Other("No analysis found in synthesis response".to_string()))
   })
}

/// Run full map-reduce pipeline for large diffs
pub fn run_map_reduce(
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
   let observations = map_phase(&files, model_name, config, counter)?;

   // Reduce phase
   reduce_phase(&observations, stat, scope_candidates, model_name, config)
}

// ============================================================================
// API types (duplicated from api.rs to avoid circular deps)
// ============================================================================

use std::time::Duration;

fn build_client(config: &CommitConfig) -> reqwest::blocking::Client {
   reqwest::blocking::Client::builder()
      .timeout(Duration::from_secs(config.request_timeout_secs))
      .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
      .build()
      .expect("Failed to build HTTP client")
}

#[derive(Debug, Serialize)]
struct Message {
   role:    String,
   content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FunctionParameters {
   #[serde(rename = "type")]
   param_type: String,
   properties: serde_json::Value,
   required:   Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Function {
   name:        String,
   description: String,
   parameters:  FunctionParameters,
}

#[derive(Debug, Serialize, Deserialize)]
struct Tool {
   #[serde(rename = "type")]
   tool_type: String,
   function:  Function,
}

#[derive(Debug, Serialize)]
struct ApiRequest {
   model:       String,
   max_tokens:  u32,
   temperature: f32,
   tools:       Vec<Tool>,
   #[serde(skip_serializing_if = "Option::is_none")]
   tool_choice: Option<serde_json::Value>,
   messages:    Vec<Message>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
   function: FunctionCall,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
   name:      String,
   arguments: String,
}

#[derive(Debug, Deserialize)]
struct Choice {
   message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
   #[serde(default)]
   tool_calls: Vec<ToolCall>,
   #[serde(default)]
   content:    Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
   choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct FileObservationResponse {
   observations: Vec<String>,
}

fn build_observation_tool() -> Tool {
   Tool {
      tool_type: "function".to_string(),
      function:  Function {
         name:        "create_file_observation".to_string(),
         description: "Extract observations from a single file's changes".to_string(),
         parameters:  FunctionParameters {
            param_type: "object".to_string(),
            properties: serde_json::json!({
               "observations": {
                  "type": "array",
                  "description": "List of factual observations about what changed in this file",
                  "items": {
                     "type": "string"
                  }
               }
            }),
            required:   vec!["observations".to_string()],
         },
      },
   }
}

fn build_analysis_tool(type_enum: &[&str]) -> Tool {
   Tool {
      tool_type: "function".to_string(),
      function:  Function {
         name:        "create_conventional_analysis".to_string(),
         description: "Synthesize observations into conventional commit analysis".to_string(),
         parameters:  FunctionParameters {
            param_type: "object".to_string(),
            properties: serde_json::json!({
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
            required:   vec!["type".to_string(), "details".to_string(), "issue_refs".to_string()],
         },
      },
   }
}

fn build_api_request(
   model: &str,
   temperature: f32,
   tools: Vec<Tool>,
   prompt: &str,
) -> ApiRequest {
   let tool_name = tools.first().map(|t| t.function.name.clone());

   ApiRequest {
      model:       model.to_string(),
      max_tokens:  1000,
      temperature,
      tool_choice: tool_name.map(|name| {
         serde_json::json!({ "type": "function", "function": { "name": name } })
      }),
      tools,
      messages:    vec![Message { role: "user".to_string(), content: prompt.to_string() }],
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_should_use_map_reduce_disabled() {
      let config = CommitConfig { map_reduce_enabled: false, ..Default::default() };
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
      assert!(!should_use_map_reduce(diff, &config));
   }

   #[test]
   fn test_should_use_map_reduce_few_files() {
      let config = CommitConfig::default();
      // Only 2 files - below threshold
      let diff = r"diff --git a/a.rs b/a.rs
@@ -0,0 +1 @@
+a
diff --git a/b.rs b/b.rs
@@ -0,0 +1 @@
+b";
      assert!(!should_use_map_reduce(diff, &config));
   }

   #[test]
   fn test_should_use_map_reduce_many_files() {
      let config = CommitConfig::default();
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
diff --git a/d.rs b/d.rs
@@ -0,0 +1 @@
+d
diff --git a/e.rs b/e.rs
@@ -0,0 +1 @@
+e";
      assert!(should_use_map_reduce(diff, &config));
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
}
