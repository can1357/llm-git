//! Map-reduce pattern for large diff analysis
//!
//! When diffs exceed the token threshold, this module splits analysis across
//! files, then synthesizes results for accurate classification.

use std::path::Path;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::{
   api::retry_api_call,
   config::{CommitConfig, ResolvedApiMode},
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
   retry_api_call(config, async move || {
      let client = crate::api::get_client(config);

      let tool = build_observation_tool();

      let parts = templates::render_map_prompt("default", filename, file_diff, context_header)?;
      let mode = config.resolved_api_mode(model_name);

      let response_text = match mode {
         ResolvedApiMode::ChatCompletions => {
            let request = build_api_request(
               config,
               model_name,
               config.temperature,
               vec![tool],
               &parts.system,
               &parts.user,
               "map",
               "default",
            );

            let mut request_builder = client
               .post(format!("{}/chat/completions", config.api_base_url))
               .header("content-type", "application/json");

            if let Some(api_key) = &config.api_key {
               request_builder =
                  request_builder.header("Authorization", format!("Bearer {api_key}"));
            }

            let (status, response_text) =
               crate::api::timed_send(request_builder.json(&request), "map-reduce/map", model_name).await?;

            if status.is_server_error() {
               eprintln!(
                  "{}",
                  crate::style::error(&format!("Server error {status}: {response_text}"))
               );
               return Ok((true, None)); // Retry
            }

            if !status.is_success() {
               return Err(CommitGenError::ApiError {
                  status: status.as_u16(),
                  body:   response_text,
               });
            }

            response_text
         },
         ResolvedApiMode::AnthropicMessages => {
            let prompt_caching = anthropic_prompt_caching_enabled(config);
            let mut tools = vec![AnthropicTool {
               name:         "create_file_observation".to_string(),
               description:  "Extract observations from a single file's changes".to_string(),
               input_schema: serde_json::json!({
                  "type": "object",
                  "properties": {
                     "observations": {
                        "type": "array",
                        "description": "List of factual observations about what changed in this file",
                        "items": {"type": "string"}
                     }
                  },
                  "required": ["observations"]
               }),
               cache_control: None,
            }];
            cache_last_anthropic_tool(&mut tools, prompt_caching);

            let request = AnthropicRequest {
               model:       model_name.to_string(),
               max_tokens:  1500,
               temperature: config.temperature,
               system:      anthropic_system_content(&parts.system, prompt_caching),
               tools,
               tool_choice: Some(AnthropicToolChoice {
                  choice_type: "tool".to_string(),
                  name:        "create_file_observation".to_string(),
               }),
               messages:    vec![AnthropicMessage {
                  role:    "user".to_string(),
                  content: vec![anthropic_text_content(parts.user, false)],
               }],
            };

            let mut request_builder = append_anthropic_cache_beta_header(
               client
                  .post(anthropic_messages_url(&config.api_base_url))
                  .header("content-type", "application/json")
                  .header("anthropic-version", "2023-06-01"),
               prompt_caching,
            );

            if let Some(api_key) = &config.api_key {
               request_builder = request_builder.header("x-api-key", api_key);
            }

            let (status, response_text) =
               crate::api::timed_send(request_builder.json(&request), "map-reduce/map", model_name).await?;

            if status.is_server_error() {
               eprintln!(
                  "{}",
                  crate::style::error(&format!("Server error {status}: {response_text}"))
               );
               return Ok((true, None)); // Retry
            }

            if !status.is_success() {
               return Err(CommitGenError::ApiError {
                  status: status.as_u16(),
                  body:   response_text,
               });
            }

            response_text
         },
      };

      if response_text.trim().is_empty() {
         crate::style::warn("Model returned empty response body for observation; retrying.");
         return Ok((true, None));
      }

      match mode {
         ResolvedApiMode::ChatCompletions => {
            let api_response: ApiResponse = serde_json::from_str(&response_text).map_err(|e| {
               CommitGenError::Other(format!(
                  "Failed to parse observation response JSON: {e}. Response body: {}",
                  response_snippet(&response_text, 500)
               ))
            })?;

            if api_response.choices.is_empty() {
               return Err(CommitGenError::Other(
                  "API returned empty response for file observation".to_string(),
               ));
            }

            let message = &api_response.choices[0].message;

            if !message.tool_calls.is_empty() {
               let tool_call = &message.tool_calls[0];
               if tool_call.function.name.ends_with("create_file_observation") {
                  let args = &tool_call.function.arguments;
                  if args.is_empty() {
                     return Err(CommitGenError::Other(
                        "Model returned empty function arguments for observation".to_string(),
                     ));
                  }

                  let obs: FileObservationResponse = serde_json::from_str(args).map_err(|e| {
                     CommitGenError::Other(format!("Failed to parse observation response: {e}"))
                  })?;

                  return Ok((
                     false,
                     Some(FileObservation {
                        file:         filename.to_string(),
                        observations: obs.observations,
                        additions:    0, // Will be filled from FileDiff
                        deletions:    0,
                     }),
                  ));
               }
            }

            // Fallback: try to parse content
            if let Some(content) = &message.content {
               if content.trim().is_empty() {
                  crate::style::warn("Model returned empty content for observation; retrying.");
                  return Ok((true, None));
               }
               let obs: FileObservationResponse =
                  serde_json::from_str(content.trim()).map_err(|e| {
                     CommitGenError::Other(format!(
                        "Failed to parse observation content JSON: {e}. Content: {}",
                        response_snippet(content, 500)
                     ))
                  })?;
               return Ok((
                  false,
                  Some(FileObservation {
                     file:         filename.to_string(),
                     observations: obs.observations,
                     additions:    0,
                     deletions:    0,
                  }),
               ));
            }

            Err(CommitGenError::Other("No observation found in API response".to_string()))
         },
         ResolvedApiMode::AnthropicMessages => {
            let (tool_input, text_content, stop_reason) =
               extract_anthropic_content(&response_text, "create_file_observation")?;

            if let Some(input) = tool_input {
               let mut observations = match input.get("observations") {
                  Some(serde_json::Value::Array(arr)) => arr
                     .iter()
                     .filter_map(|v| v.as_str().map(str::to_string))
                     .collect::<Vec<_>>(),
                  Some(serde_json::Value::String(s)) => parse_string_to_observations(s),
                  _ => Vec::new(),
               };

               if observations.is_empty() {
                  let text_observations = parse_observations_from_text(&text_content);
                  if !text_observations.is_empty() {
                     observations = text_observations;
                  } else if stop_reason.as_deref() == Some("max_tokens") {
                     crate::style::warn(
                        "Anthropic stopped at max_tokens with empty observations; using fallback \
                         observation.",
                     );
                     let fallback_target = Path::new(filename)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(filename);
                     observations = vec![format!("Updated {fallback_target}.")];
                  } else {
                     crate::style::warn(
                        "Model returned empty observation tool input; continuing with no \
                         observations.",
                     );
                  }
               }

               return Ok((
                  false,
                  Some(FileObservation {
                     file: filename.to_string(),
                     observations,
                     additions: 0,
                     deletions: 0,
                  }),
               ));
            }

            if text_content.trim().is_empty() {
               crate::style::warn("Model returned empty content for observation; retrying.");
               return Ok((true, None));
            }

            let obs: FileObservationResponse =
               serde_json::from_str(text_content.trim()).map_err(|e| {
                  CommitGenError::Other(format!(
                     "Failed to parse observation content JSON: {e}. Content: {}",
                     response_snippet(&text_content, 500)
                  ))
               })?;
            Ok((
               false,
               Some(FileObservation {
                  file:         filename.to_string(),
                  observations: obs.observations,
                  additions:    0,
                  deletions:    0,
               }),
            ))
         },
      }
   }).await
}

/// Reduce phase: synthesize all observations into final analysis
pub async fn reduce_phase(
   observations: &[FileObservation],
   stat: &str,
   scope_candidates: &str,
   model_name: &str,
   config: &CommitConfig,
) -> Result<ConventionalAnalysis> {
   retry_api_call(config, async move || {
      let client = crate::api::get_client(config);

      // Build type enum from config
      let type_enum: Vec<&str> = config.types.keys().map(|s| s.as_str()).collect();

      let tool = build_analysis_tool(&type_enum);

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
      let mode = config.resolved_api_mode(model_name);

      let response_text = match mode {
         ResolvedApiMode::ChatCompletions => {
            let request = build_api_request(
               config,
               model_name,
               config.temperature,
               vec![tool],
               &parts.system,
               &parts.user,
               "reduce",
               "default",
            );

            let mut request_builder = client
               .post(format!("{}/chat/completions", config.api_base_url))
               .header("content-type", "application/json");

            if let Some(api_key) = &config.api_key {
               request_builder =
                  request_builder.header("Authorization", format!("Bearer {api_key}"));
            }

            let (status, response_text) =
               crate::api::timed_send(request_builder.json(&request), "map-reduce/reduce", model_name).await?;

            if status.is_server_error() {
               eprintln!(
                  "{}",
                  crate::style::error(&format!("Server error {status}: {response_text}"))
               );
               return Ok((true, None)); // Retry
            }

            if !status.is_success() {
               return Err(CommitGenError::ApiError {
                  status: status.as_u16(),
                  body:   response_text,
               });
            }

            response_text
         },
         ResolvedApiMode::AnthropicMessages => {
            let prompt_caching = anthropic_prompt_caching_enabled(config);
            let mut tools = vec![AnthropicTool {
               name:         "create_conventional_analysis".to_string(),
               description:  "Analyze changes and classify as conventional commit with type, \
                              scope, details, and metadata"
                  .to_string(),
               input_schema: serde_json::json!({
                     "type": "object",
                     "properties": {
                        "type": {
                           "type": "string",
                           "enum": type_enum,
                           "description": "Commit type based on change classification"
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
                     },
                     "required": ["type", "details", "issue_refs"]
                  }),
               cache_control: None,
            }];
            cache_last_anthropic_tool(&mut tools, prompt_caching);

            let request = AnthropicRequest {
               model:       model_name.to_string(),
               max_tokens:  1500,
               temperature: config.temperature,
               system:      anthropic_system_content(&parts.system, prompt_caching),
               tools,
               tool_choice: Some(AnthropicToolChoice {
                  choice_type: "tool".to_string(),
                  name:        "create_conventional_analysis".to_string(),
               }),
               messages:    vec![AnthropicMessage {
                  role:    "user".to_string(),
                  content: vec![anthropic_text_content(parts.user, false)],
               }],
            };

            let mut request_builder = append_anthropic_cache_beta_header(
               client
                  .post(anthropic_messages_url(&config.api_base_url))
                  .header("content-type", "application/json")
                  .header("anthropic-version", "2023-06-01"),
               prompt_caching,
            );

            if let Some(api_key) = &config.api_key {
               request_builder = request_builder.header("x-api-key", api_key);
            }

            let (status, response_text) =
               crate::api::timed_send(request_builder.json(&request), "map-reduce/reduce", model_name).await?;

            if status.is_server_error() {
               eprintln!(
                  "{}",
                  crate::style::error(&format!("Server error {status}: {response_text}"))
               );
               return Ok((true, None));
            }

            if !status.is_success() {
               return Err(CommitGenError::ApiError {
                  status: status.as_u16(),
                  body:   response_text,
               });
            }

            response_text
         },
      };

      if response_text.trim().is_empty() {
         crate::style::warn("Model returned empty response body for synthesis; retrying.");
         return Ok((true, None));
      }

      match mode {
         ResolvedApiMode::ChatCompletions => {
            let api_response: ApiResponse = serde_json::from_str(&response_text).map_err(|e| {
               CommitGenError::Other(format!(
                  "Failed to parse synthesis response JSON: {e}. Response body: {}",
                  response_snippet(&response_text, 500)
               ))
            })?;

            if api_response.choices.is_empty() {
               return Err(CommitGenError::Other(
                  "API returned empty response for synthesis".to_string(),
               ));
            }

            let message = &api_response.choices[0].message;

            if !message.tool_calls.is_empty() {
               let tool_call = &message.tool_calls[0];
               if tool_call
                  .function
                  .name
                  .ends_with("create_conventional_analysis")
               {
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
               if content.trim().is_empty() {
                  crate::style::warn("Model returned empty content for synthesis; retrying.");
                  return Ok((true, None));
               }
               let analysis: ConventionalAnalysis =
                  serde_json::from_str(content.trim()).map_err(|e| {
                     CommitGenError::Other(format!(
                        "Failed to parse synthesis content JSON: {e}. Content: {}",
                        response_snippet(content, 500)
                     ))
                  })?;
               return Ok((false, Some(analysis)));
            }

            Err(CommitGenError::Other("No analysis found in synthesis response".to_string()))
         },
         ResolvedApiMode::AnthropicMessages => {
            let (tool_input, text_content, stop_reason) =
               extract_anthropic_content(&response_text, "create_conventional_analysis")?;

            if let Some(input) = tool_input {
               let analysis: ConventionalAnalysis = serde_json::from_value(input).map_err(|e| {
                  CommitGenError::Other(format!(
                     "Failed to parse synthesis tool input: {e}. Response body: {}",
                     response_snippet(&response_text, 500)
                  ))
               })?;
               return Ok((false, Some(analysis)));
            }

            if text_content.trim().is_empty() {
               if stop_reason.as_deref() == Some("max_tokens") {
                  crate::style::warn(
                     "Anthropic stopped at max_tokens with empty synthesis; retrying.",
                  );
                  return Ok((true, None));
               }
               crate::style::warn("Model returned empty content for synthesis; retrying.");
               return Ok((true, None));
            }

            let analysis: ConventionalAnalysis = serde_json::from_str(text_content.trim())
               .map_err(|e| {
                  CommitGenError::Other(format!(
                     "Failed to parse synthesis content JSON: {e}. Content: {}",
                     response_snippet(&text_content, 500)
                  ))
               })?;
            Ok((false, Some(analysis)))
         },
      }
   }).await
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

// ============================================================================
// API types (duplicated from api.rs to avoid circular deps)
// ============================================================================

fn response_snippet(body: &str, limit: usize) -> String {
   if body.is_empty() {
      return "<empty response body>".to_string();
   }
   let mut snippet = body.trim().to_string();
   if snippet.len() > limit {
      snippet.truncate(limit);
      snippet.push_str("...");
   }
   snippet
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

fn anthropic_messages_url(base_url: &str) -> String {
   let trimmed = base_url.trim_end_matches('/');
   if trimmed.ends_with("/v1") {
      format!("{trimmed}/messages")
   } else {
      format!("{trimmed}/v1/messages")
   }
}

fn prompt_cache_control() -> PromptCacheControl {
   PromptCacheControl { control_type: "ephemeral".to_string() }
}

fn anthropic_prompt_caching_enabled(config: &CommitConfig) -> bool {
   config.api_base_url.to_lowercase().contains("anthropic.com")
}

fn append_anthropic_cache_beta_header(
   request_builder: reqwest::RequestBuilder,
   enable_cache: bool,
) -> reqwest::RequestBuilder {
   if enable_cache {
      request_builder.header("anthropic-beta", "prompt-caching-2024-07-31")
   } else {
      request_builder
   }
}

fn anthropic_text_content(text: String, cache: bool) -> AnthropicContent {
   AnthropicContent {
      content_type: "text".to_string(),
      text,
      cache_control: cache.then(prompt_cache_control),
   }
}

fn anthropic_system_content(system_prompt: &str, cache: bool) -> Option<Vec<AnthropicContent>> {
   if system_prompt.trim().is_empty() {
      None
   } else {
      Some(vec![anthropic_text_content(system_prompt.to_string(), cache)])
   }
}

fn cache_last_anthropic_tool(tools: &mut [AnthropicTool], cache: bool) {
   if cache && let Some(last) = tools.last_mut() {
      last.cache_control = Some(prompt_cache_control());
   }
}

fn extract_anthropic_content(
   response_text: &str,
   tool_name: &str,
) -> Result<(Option<serde_json::Value>, String, Option<String>)> {
   let value: serde_json::Value = serde_json::from_str(response_text).map_err(|e| {
      CommitGenError::Other(format!(
         "Failed to parse Anthropic response JSON: {e}. Response body: {}",
         response_snippet(response_text, 500)
      ))
   })?;

   let stop_reason = value
      .get("stop_reason")
      .and_then(|v| v.as_str())
      .map(str::to_string);

   let mut tool_input: Option<serde_json::Value> = None;
   let mut text_parts = Vec::new();

   if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
      for item in content {
         let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
         match item_type {
            "tool_use" => {
               let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
               if name == tool_name
                  && let Some(input) = item.get("input")
               {
                  tool_input = Some(input.clone());
               }
            },
            "text" => {
               if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                  text_parts.push(text.to_string());
               }
            },
            _ => {},
         }
      }
   }

   Ok((tool_input, text_parts.join("\n"), stop_reason))
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
   model:            String,
   max_tokens:       u32,
   temperature:      f32,
   tools:            Vec<Tool>,
   #[serde(skip_serializing_if = "Option::is_none")]
   tool_choice:      Option<serde_json::Value>,
   #[serde(skip_serializing_if = "Option::is_none")]
   prompt_cache_key: Option<String>,
   messages:         Vec<Message>,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
   model:       String,
   max_tokens:  u32,
   temperature: f32,
   #[serde(skip_serializing_if = "Option::is_none")]
   system:      Option<Vec<AnthropicContent>>,
   tools:       Vec<AnthropicTool>,
   #[serde(skip_serializing_if = "Option::is_none")]
   tool_choice: Option<AnthropicToolChoice>,
   messages:    Vec<AnthropicMessage>,
}

#[derive(Debug, Clone, Serialize)]
struct PromptCacheControl {
   #[serde(rename = "type")]
   control_type: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
   name:          String,
   description:   String,
   input_schema:  serde_json::Value,
   #[serde(skip_serializing_if = "Option::is_none")]
   cache_control: Option<PromptCacheControl>,
}

#[derive(Debug, Serialize)]
struct AnthropicToolChoice {
   #[serde(rename = "type")]
   choice_type: String,
   name:        String,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
   role:    String,
   content: Vec<AnthropicContent>,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicContent {
   #[serde(rename = "type")]
   content_type:  String,
   text:          String,
   #[serde(skip_serializing_if = "Option::is_none")]
   cache_control: Option<PromptCacheControl>,
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

#[allow(
   clippy::too_many_arguments,
   reason = "needs model/tool/prompt metadata to build deterministic cacheable request"
)]
fn build_api_request(
   config: &CommitConfig,
   model: &str,
   temperature: f32,
   tools: Vec<Tool>,
   system: &str,
   user: &str,
   prompt_family: &str,
   prompt_variant: &str,
) -> ApiRequest {
   let tool_name = tools.first().map(|t| t.function.name.clone());

   let mut messages = Vec::new();
   if !system.is_empty() {
      messages.push(Message { role: "system".to_string(), content: system.to_string() });
   }
   messages.push(Message { role: "user".to_string(), content: user.to_string() });

   let prompt_cache_key =
      crate::api::openai_prompt_cache_key(config, model, prompt_family, prompt_variant, system);

   ApiRequest {
      model: model.to_string(),
      max_tokens: 1500,
      temperature,
      tool_choice: tool_name
         .map(|name| serde_json::json!({ "type": "function", "function": { "name": name } })),
      prompt_cache_key,
      tools,
      messages,
   }
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
      let result: FileObservationResponse = serde_json::from_str(json).unwrap();
      assert_eq!(result.observations, vec!["a", "b", "c"]);
   }

   #[test]
   fn test_deserialize_observations_stringified_array() {
      let json = r#"{"observations": "[\"a\", \"b\", \"c\"]"}"#;
      let result: FileObservationResponse = serde_json::from_str(json).unwrap();
      assert_eq!(result.observations, vec!["a", "b", "c"]);
   }

   #[test]
   fn test_deserialize_observations_bullet_string() {
      let json = r#"{"observations": "- updated function\n- fixed bug"}"#;
      let result: FileObservationResponse = serde_json::from_str(json).unwrap();
      assert_eq!(result.observations, vec!["updated function", "fixed bug"]);
   }
}
