//! Map-reduce pattern for large diff analysis
//!
//! When diffs exceed the token threshold, this module splits analysis across
//! files, then synthesizes results for accurate classification.

use std::{borrow::Cow, cmp::Reverse, fmt::Write as _, path::Path};

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

/// Maximum tokens per file in map phase (leave headroom for prompt template +
/// context)
const MAX_FILE_TOKENS: usize = 50_000;
const MAP_PHASE_CONCURRENCY: usize = 16;

const fn map_phase_model(config: &CommitConfig) -> &str {
   config.summary_model.as_str()
}

fn build_file_batches(
   files: &[FileDiff],
   counter: &TokenCounter,
   budget: usize,
) -> Vec<Vec<usize>> {
   build_file_batches_for_indices(files, 0..files.len(), counter, budget)
}

fn build_llm_file_batches(
   files: &[FileDiff],
   counter: &TokenCounter,
   budget: usize,
) -> Vec<Vec<usize>> {
   if files.iter().all(|file| !file.is_binary) {
      return build_file_batches(files, counter, budget);
   }

   build_file_batches_for_indices(
      files,
      files
         .iter()
         .enumerate()
         .filter_map(|(idx, file)| (!file.is_binary).then_some(idx)),
      counter,
      budget,
   )
}

fn build_file_batches_for_indices<I>(
   files: &[FileDiff],
   indices: I,
   counter: &TokenCounter,
   budget: usize,
) -> Vec<Vec<usize>>
where
   I: IntoIterator<Item = usize>,
{
   let budget = budget.max(1);
   let mut batches = Vec::new();
   let mut current_batch = Vec::new();
   let mut current_tokens = 0usize;

   for idx in indices {
      let file_tokens = files[idx].token_estimate(counter);
      if file_tokens > budget {
         if !current_batch.is_empty() {
            batches.push(std::mem::take(&mut current_batch));
            current_tokens = 0;
         }
         batches.push(vec![idx]);
         continue;
      }

      if !current_batch.is_empty() && current_tokens.saturating_add(file_tokens) > budget {
         batches.push(std::mem::take(&mut current_batch));
         current_tokens = 0;
      }

      current_batch.push(idx);
      current_tokens = current_tokens.saturating_add(file_tokens);
   }

   if !current_batch.is_empty() {
      batches.push(current_batch);
   }

   batches
}

/// Check if map-reduce should be used.
///
/// Route only when the included diff is large enough to benefit from
/// per-file analysis, or when a single included file would be truncated in the
/// map phase.
#[tracing::instrument(target = "lgit", name = "map_reduce.should_use", skip_all, fields(diff_bytes = diff.len(), threshold = config.map_reduce_threshold))]
pub fn should_use_map_reduce(diff: &str, config: &CommitConfig, counter: &TokenCounter) -> bool {
   if !config.map_reduce_enabled {
      return false;
   }

   let files = parse_diff(diff);
   let mut has_included_file = false;
   let mut total_tokens = 0usize;

   for file in files.iter().filter(|file| {
      !config
         .excluded_files
         .iter()
         .any(|excluded| file.filename.ends_with(excluded))
   }) {
      has_included_file = true;

      let file_tokens = file.token_estimate(counter);
      if file_tokens > MAX_FILE_TOKENS {
         return true;
      }

      total_tokens = total_tokens.saturating_add(file_tokens);
      if total_tokens >= config.map_reduce_threshold {
         return true;
      }
   }

   has_included_file && total_tokens >= config.map_reduce_threshold
}

/// Maximum files to include in context header (prevent token explosion)
const MAX_CONTEXT_FILES: usize = 20;

/// Precomputed context header metadata for cross-file awareness.
struct ContextFile<'a> {
   filename:     &'a str,
   summary_line: String,
   change_size:  usize,
}

struct ContextHeaders<'a> {
   files:               Vec<ContextFile<'a>>,
   ranked_indices:      Vec<usize>,
   large_commit_header: Option<String>,
}

impl<'a> ContextHeaders<'a> {
   fn new(files: &'a [FileDiff]) -> Self {
      // Skip detailed context for very large commits (diminishing returns).
      if files.len() > 100 {
         return Self {
            files:               Vec::new(),
            ranked_indices:      Vec::new(),
            large_commit_header: Some(format!("(Large commit with {} total files)", files.len())),
         };
      }

      let files: Vec<_> = files
         .iter()
         .map(|file| {
            let change_size = file.additions + file.deletions;
            let description = infer_file_description(&file.filename, &file.content);
            ContextFile {
               filename: &file.filename,
               summary_line: format!(
                  "- {} ({} lines): {}",
                  file.filename, change_size, description
               ),
               change_size,
            }
         })
         .collect();

      let mut ranked_indices = Vec::new();
      if files.len() > MAX_CONTEXT_FILES {
         ranked_indices = (0..files.len()).collect();
         ranked_indices.sort_by_key(|&idx| Reverse(files[idx].change_size));
      }

      Self { files, ranked_indices, large_commit_header: None }
   }

   fn header_for_files(&self, current_files: &[&str]) -> Cow<'_, str> {
      if let Some(header) = &self.large_commit_header {
         return Cow::Borrowed(header.as_str());
      }

      let current_count = self
         .files
         .iter()
         .filter(|file| is_current_context_file(file.filename, current_files))
         .count();
      let total_other = self.files.len().saturating_sub(current_count);

      if total_other == 0 {
         return Cow::Borrowed("");
      }

      let mut header = String::with_capacity(32 + total_other.min(MAX_CONTEXT_FILES) * 80);
      header.push_str("OTHER FILES IN THIS CHANGE:");

      let mut shown = 0usize;
      if total_other > MAX_CONTEXT_FILES {
         for &idx in &self.ranked_indices {
            let file = &self.files[idx];
            if is_current_context_file(file.filename, current_files) {
               continue;
            }

            header.push('\n');
            header.push_str(&file.summary_line);
            shown += 1;

            if shown == MAX_CONTEXT_FILES {
               break;
            }
         }
      } else {
         for file in &self.files {
            if is_current_context_file(file.filename, current_files) {
               continue;
            }

            header.push('\n');
            header.push_str(&file.summary_line);
            shown += 1;
         }
      }

      if shown < total_other {
         write!(&mut header, "\n... and {} more files", total_other - shown)
            .expect("writing to a string cannot fail");
      }

      Cow::Owned(header)
   }
}

fn is_current_context_file(filename: &str, current_files: &[&str]) -> bool {
   current_files.contains(&filename)
}

/// Infer a brief description of what a file likely contains based on
/// name/content
fn infer_file_description(filename: &str, content: &str) -> &'static str {
   let filename_lower = filename.to_lowercase();

   // Check filename patterns
   if filename_lower.contains("test") {
      return "test file";
   }
   if filename_lower.contains("prompt") || filename_lower.contains("system") {
      return "prompt template";
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

/// Map phase: analyze token-budgeted file batches and extract observations
#[tracing::instrument(target = "lgit", name = "map_reduce.map_phase", skip_all, fields(file_count = files.len(), model = map_model_name))]
async fn map_phase(
   files: &[FileDiff],
   map_model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> Result<Vec<FileObservation>> {
   let context_headers = ContextHeaders::new(files);
   let llm_batches = build_llm_file_batches(files, counter, config.map_batch_token_budget);
   let total_batches = llm_batches.len();

   let mut observations_by_index = vec![None; files.len()];
   for (idx, file) in files.iter().enumerate().filter(|(_, file)| file.is_binary) {
      observations_by_index[idx] = Some(FileObservation {
         file:         file.filename.clone(),
         observations: vec!["Binary file changed.".to_string()],
         additions:    0,
         deletions:    0,
      });
   }

   let batch_results: Vec<Result<Vec<(usize, FileObservation)>>> =
      stream::iter(llm_batches.into_iter().enumerate())
         .map(|(batch_idx, batch_indices)| {
            let context_headers = &context_headers;
            async move {
               let batch_files: Vec<&FileDiff> =
                  batch_indices.iter().map(|&idx| &files[idx]).collect();
               let current_paths: Vec<&str> = batch_files
                  .iter()
                  .map(|file| file.filename.as_str())
                  .collect();
               let context_header = context_headers.header_for_files(&current_paths);
               let progress_label = format!(
                  "map batch {}/{} ({} files)",
                  batch_idx + 1,
                  total_batches,
                  batch_files.len()
               );
               let observations = map_file_batch(
                  &batch_files,
                  &context_header,
                  map_model_name,
                  config,
                  counter,
                  &progress_label,
               )
               .await?;

               Ok(batch_indices.into_iter().zip(observations).collect())
            }
         })
         .buffer_unordered(MAP_PHASE_CONCURRENCY)
         .collect()
         .await;

   for result in batch_results {
      for (idx, observation) in result? {
         observations_by_index[idx] = Some(observation);
      }
   }

   let mut observations = Vec::with_capacity(files.len());
   for (idx, observation) in observations_by_index.into_iter().enumerate() {
      let observation = observation.ok_or_else(|| {
         CommitGenError::Other(format!("Missing map observation for {}", files[idx].filename))
      })?;
      observations.push(observation);
   }

   Ok(observations)
}

#[tracing::instrument(target = "lgit", name = "map_reduce.observe_diff_files", skip_all, fields(diff_bytes = diff.len(), model = map_model_name))]
pub async fn observe_diff_files(
   diff: &str,
   map_model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> Result<Vec<FileObservation>> {
   let mut files = parse_diff(diff);

   files.retain(|file| {
      !config
         .excluded_files
         .iter()
         .any(|excluded| file.filename.ends_with(excluded))
   });

   if files.is_empty() {
      return Err(CommitGenError::Other(
         "No relevant files to summarize after filtering".to_string(),
      ));
   }

   let llm_file_count = files.iter().filter(|file| !file.is_binary).count();
   let batch_count = build_llm_file_batches(&files, counter, config.map_batch_token_budget).len();
   crate::api::print_llm_progress(|| {
      format!(
         "Map-reduce map phase: {batch_count} batch LLM queries for {llm_file_count} files queued \
          on {map_model_name} (max {MAP_PHASE_CONCURRENCY} parallel)"
      )
   });

   map_phase(&files, map_model_name, config, counter).await
}

/// Analyze a token-budgeted file batch and extract per-file observations
#[tracing::instrument(target = "lgit", name = "map_reduce.map_file_batch", skip_all, fields(file_count = files.len(), model = model_name))]
async fn map_file_batch(
   files: &[&FileDiff],
   context_header: &str,
   model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
   progress_label: &str,
) -> Result<Vec<FileObservation>> {
   let rendered_diffs: Vec<String> = files
      .iter()
      .map(|file| render_file_diff_for_batch(file, counter))
      .collect();
   let prompt_files: Vec<templates::MapFile<'_>> = files
      .iter()
      .zip(&rendered_diffs)
      .map(|(file, diff)| templates::MapFile { path: file.filename.as_str(), diff })
      .collect();
   let variant = if config.markdown_output { "markdown" } else { "default" };
   let parts = templates::render_map_prompt(variant, &prompt_files, context_header)?;
   let observation_schema = build_batch_observation_schema();
   let response = run_oneshot::<BatchObservationResponse>(config, &OneShotSpec {
      operation: "map-reduce/map",
      model: model_name,
      prompt_family: "map",
      prompt_variant: variant,
      system_prompt: &parts.system,
      user_prompt: &parts.user,
      tool_name: "create_file_observations",
      tool_description: "Extract observations from a batch of file changes",
      schema: &observation_schema,
      progress_label: Some(progress_label),
      debug: None,
      cacheable: true,
   })
   .await?;

   Ok(map_batch_response_to_observations(
      files,
      &response.output,
      response.text_content.as_deref(),
      response.stop_reason.as_deref(),
   ))
}

fn render_file_diff_for_batch(file: &FileDiff, counter: &TokenCounter) -> String {
   let file_tokens = file.token_estimate(counter);
   if file_tokens > MAX_FILE_TOKENS {
      let mut file_clone = file.clone();
      let target_size = MAX_FILE_TOKENS * 4; // Convert tokens to chars
      file_clone.truncate(target_size);
      eprintln!(
         "  {} truncated {} ({} \u{2192} {} tokens)",
         crate::style::icons::WARNING,
         file.filename,
         file_tokens,
         file_clone.token_estimate(counter)
      );
      return reconstruct_diff(&[file_clone]);
   }

   reconstruct_single_file_diff(file)
}

fn reconstruct_single_file_diff(file: &FileDiff) -> String {
   let mut diff = String::with_capacity(file.size());
   diff.push_str(&file.header);
   if !file.content.is_empty() {
      diff.push('\n');
      diff.push_str(&file.content);
   }
   diff
}

fn map_batch_response_to_observations(
   files: &[&FileDiff],
   response: &BatchObservationResponse,
   text_content: Option<&str>,
   stop_reason: Option<&str>,
) -> Vec<FileObservation> {
   if response.files.is_empty() && text_content.is_some_and(|text| !text.trim().is_empty()) {
      crate::style::warn(
         "Model returned batch observations as text; using fallback observations for every file.",
      );
      return files
         .iter()
         .map(|file| fallback_file_observation(file))
         .collect();
   }

   let stopped_at_max_tokens = stop_reason == Some("max_tokens");
   let mut used_entries = vec![false; response.files.len()];
   files
      .iter()
      .map(|file| {
         let Some(entry_idx) =
            find_observation_entry(file.filename.as_str(), &response.files, &used_entries, files)
         else {
            return fallback_file_observation(file);
         };

         used_entries[entry_idx] = true;
         let entry = &response.files[entry_idx];
         let observations = if entry.observations.is_empty() && stopped_at_max_tokens {
            vec![fallback_observation_text(&file.filename)]
         } else {
            entry.observations.clone()
         };

         FileObservation { file: file.filename.clone(), observations, additions: 0, deletions: 0 }
      })
      .collect()
}

fn find_observation_entry(
   filename: &str,
   entries: &[FileObservationEntry],
   used_entries: &[bool],
   batch_files: &[&FileDiff],
) -> Option<usize> {
   find_entry_by(entries, used_entries, |entry| entry.path == filename)
      .or_else(|| {
         let filename_basename = path_basename(filename);
         let basename_is_unique = batch_files
            .iter()
            .filter(|file| path_basename(file.filename.as_str()) == filename_basename)
            .count()
            == 1;
         basename_is_unique
            .then(|| {
               find_entry_by(entries, used_entries, |entry| {
                  path_basename(&entry.path) == filename_basename
               })
            })
            .flatten()
      })
      .or_else(|| {
         find_entry_by(entries, used_entries, |entry| path_suffix_matches(&entry.path, filename))
      })
}

fn find_entry_by<F>(
   entries: &[FileObservationEntry],
   used_entries: &[bool],
   mut matches: F,
) -> Option<usize>
where
   F: FnMut(&FileObservationEntry) -> bool,
{
   entries
      .iter()
      .enumerate()
      .find_map(|(idx, entry)| (!used_entries[idx] && matches(entry)).then_some(idx))
}

fn path_basename(path: &str) -> &str {
   Path::new(path)
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or(path)
}

fn path_suffix_matches(left: &str, right: &str) -> bool {
   path_has_suffix(left, right) || path_has_suffix(right, left)
}

fn path_has_suffix(path: &str, suffix: &str) -> bool {
   if path == suffix {
      return true;
   }

   path
      .strip_suffix(suffix)
      .is_some_and(|prefix| prefix.ends_with('/') || prefix.ends_with('\\'))
}

fn fallback_file_observation(file: &FileDiff) -> FileObservation {
   FileObservation {
      file:         file.filename.clone(),
      observations: vec![fallback_observation_text(&file.filename)],
      additions:    0,
      deletions:    0,
   }
}

fn fallback_observation_text(filename: &str) -> String {
   let fallback_target = path_basename(filename);
   format!("Updated {fallback_target}.")
}

/// Reduce phase: synthesize all observations into final analysis
#[tracing::instrument(target = "lgit", name = "map_reduce.reduce_phase", skip_all, fields(observation_count = observations.len(), model = model_name))]
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
   let variant = if config.markdown_output { "markdown" } else { "default" };
   let parts = templates::render_reduce_prompt(
      variant,
      &observations_json,
      stat,
      scope_candidates,
      Some(&types_description),
   )?;

   let analysis_schema = build_analysis_schema(&type_enum, config);
   let response = run_oneshot::<ConventionalAnalysis>(config, &OneShotSpec {
      operation:        "map-reduce/reduce",
      model:            model_name,
      prompt_family:    "reduce",
      prompt_variant:   variant,
      system_prompt:    &parts.system,
      user_prompt:      &parts.user,
      tool_name:        "create_conventional_analysis",
      tool_description: "Analyze changes and classify as conventional commit with type, scope, \
                         summary, details, and metadata",
      schema:           &analysis_schema,
      progress_label:   Some("reduce file observations"),
      debug:            None,
      cacheable:        true,
   })
   .await?;

   Ok(response.output)
}

/// Run full map-reduce pipeline for large diffs
#[tracing::instrument(target = "lgit", name = "map_reduce.run", skip_all, fields(diff_bytes = diff.len(), model = model_name))]
pub async fn run_map_reduce(
   diff: &str,
   stat: &str,
   scope_candidates: &str,
   model_name: &str,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> Result<ConventionalAnalysis> {
   let map_model_name = map_phase_model(config);
   let observations = observe_diff_files(diff, map_model_name, config, counter).await?;
   let file_count = observations.len();
   crate::api::print_llm_progress(|| {
      format!("Map-reduce reduce phase: synthesizing {file_count} file observations")
   });

   reduce_phase(&observations, stat, scope_candidates, model_name, config).await
}

#[derive(Debug, Deserialize, Serialize)]
struct BatchObservationResponse {
   #[serde(default)]
   files: Vec<FileObservationEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FileObservationEntry {
   path:         String,
   #[serde(default, deserialize_with = "deserialize_observations")]
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

fn build_batch_observation_schema() -> serde_json::Value {
   strict_json_schema(
      serde_json::json!({
         "files": {
            "type": "array",
            "description": "Per-file observations for every file in the map batch.",
            "items": {
               "type": "object",
               "properties": {
                  "path": {
                     "type": "string",
                     "description": "The exact input file path this observation set describes."
                  },
                  "observations": {
                     "type": "array",
                     "description": "Factual observations about what changed in this file.",
                     "items": {
                        "type": "string"
                     }
                  }
               },
               "required": ["path", "observations"],
               "additionalProperties": false
            }
         }
      }),
      &["files"],
   )
}

fn build_analysis_schema(type_enum: &[&str], config: &CommitConfig) -> serde_json::Value {
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
         "summary": {
            "type": "string",
            "description": format!(
               "Concise past-tense commit summary without type/scope prefix or trailing period; target {} chars, hard limit {}.",
               config.summary_guideline,
               config.summary_hard_limit
            ),
            "maxLength": config.summary_hard_limit
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

   fn file_with_tokens(filename: &str, token_estimate: usize) -> FileDiff {
      FileDiff {
         filename:  filename.to_string(),
         header:    String::new(),
         content:   "x".repeat(token_estimate * 4),
         additions: 0,
         deletions: 0,
         is_binary: false,
      }
   }

   #[test]
   fn test_map_phase_model_uses_summary_model() {
      let config = CommitConfig {
         summary_model: "claude-haiku-4-5".to_string(),
         analysis_model: "claude-opus-4.1".to_string(),
         ..Default::default()
      };

      assert_eq!(map_phase_model(&config), "claude-haiku-4-5");
      assert_eq!(MAP_PHASE_CONCURRENCY, 16);
   }

   #[test]
   fn test_build_file_batches_single_batch_when_under_budget() {
      let counter = test_counter();
      let files = vec![
         file_with_tokens("a.rs", 4),
         file_with_tokens("b.rs", 4),
         file_with_tokens("c.rs", 1),
      ];

      assert_eq!(build_file_batches(&files, &counter, 10), vec![vec![0, 1, 2]]);
   }

   #[test]
   fn test_build_file_batches_splits_when_budget_exceeded() {
      let counter = test_counter();
      let files = vec![
         file_with_tokens("a.rs", 4),
         file_with_tokens("b.rs", 4),
         file_with_tokens("c.rs", 4),
      ];

      assert_eq!(build_file_batches(&files, &counter, 10), vec![vec![0, 1], vec![2]]);
   }

   #[test]
   fn test_build_file_batches_preserves_order_and_every_file_once() {
      let counter = test_counter();
      let files = vec![
         file_with_tokens("a.rs", 3),
         file_with_tokens("b.rs", 8),
         file_with_tokens("c.rs", 2),
         file_with_tokens("d.rs", 9),
         file_with_tokens("e.rs", 1),
      ];

      let batches = build_file_batches(&files, &counter, 10);
      let flattened: Vec<usize> = batches.into_iter().flatten().collect();
      assert_eq!(flattened, vec![0, 1, 2, 3, 4]);
   }

   #[test]
   fn test_build_file_batches_isolates_oversized_files() {
      let counter = test_counter();
      let files = vec![
         file_with_tokens("a.rs", 2),
         file_with_tokens("b.rs", 2),
         file_with_tokens("huge.rs", 12),
         file_with_tokens("c.rs", 2),
      ];

      assert_eq!(build_file_batches(&files, &counter, 10), vec![vec![0, 1], vec![2], vec![3]]);
   }

   #[test]
   fn test_batch_response_mapping_matches_paths_and_falls_back_for_omissions() {
      let exact = file_with_tokens("src/lib.rs", 1);
      let basename = file_with_tokens("src/main.rs", 1);
      let omitted = file_with_tokens("crates/core/mod.rs", 1);
      let files = [&exact, &basename, &omitted];
      let response = BatchObservationResponse {
         files: vec![
            FileObservationEntry {
               path:         "src/lib.rs".to_string(),
               observations: vec!["updated library entrypoint".to_string()],
            },
            FileObservationEntry {
               path:         "main.rs".to_string(),
               observations: vec!["changed CLI wiring".to_string()],
            },
         ],
      };

      let result = map_batch_response_to_observations(&files, &response, None, None);

      assert_eq!(result[0].file, "src/lib.rs");
      assert_eq!(result[0].observations, vec!["updated library entrypoint".to_string()]);
      assert_eq!(result[1].file, "src/main.rs");
      assert_eq!(result[1].observations, vec!["changed CLI wiring".to_string()]);
      assert_eq!(result[2].file, "crates/core/mod.rs");
      assert_eq!(result[2].observations, vec!["Updated mod.rs.".to_string()]);
   }

   #[test]
   fn test_batch_response_mapping_falls_back_for_text_only_response() {
      let first = file_with_tokens("src/lib.rs", 1);
      let second = file_with_tokens("src/main.rs", 1);
      let files = [&first, &second];
      let response = BatchObservationResponse { files: Vec::new() };

      let result = map_batch_response_to_observations(
         &files,
         &response,
         Some("- unstructured observation"),
         None,
      );

      assert_eq!(result[0].observations, vec!["Updated lib.rs.".to_string()]);
      assert_eq!(result[1].observations, vec!["Updated main.rs.".to_string()]);
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
   fn test_should_use_map_reduce_many_tiny_files_below_threshold() {
      let config = CommitConfig { map_reduce_threshold: 1_000, ..Default::default() };
      let counter = test_counter();
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
      assert!(!should_use_map_reduce(diff, &config, &counter));
   }

   #[test]
   fn test_should_use_map_reduce_large_total_diff() {
      let config = CommitConfig { map_reduce_threshold: 20, ..Default::default() };
      let counter = test_counter();
      let payload = "a".repeat(200);
      let diff = format!("diff --git a/a.rs b/a.rs\n@@ -0,0 +1 @@\n+{payload}");

      assert!(should_use_map_reduce(&diff, &config, &counter));
   }

   #[test]
   fn test_should_use_map_reduce_single_oversized_file() {
      let config = CommitConfig { map_reduce_threshold: usize::MAX, ..Default::default() };
      let counter = test_counter();
      let payload = "a".repeat((MAX_FILE_TOKENS + 1) * 4);
      let diff = format!("diff --git a/a.rs b/a.rs\n@@ -0,0 +1 @@\n+{payload}");

      assert!(should_use_map_reduce(&diff, &config, &counter));
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
      let context_headers = ContextHeaders::new(&files);
      let header = context_headers.header_for_files(&["only.rs"]);
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

      let context_headers = ContextHeaders::new(&files);
      let header = context_headers.header_for_files(&["src/main.rs"]);
      assert!(header.contains("OTHER FILES IN THIS CHANGE:"));
      assert!(header.contains("src/lib.rs"));
      assert!(header.contains("tests/test.rs"));
      assert!(!header.contains("src/main.rs")); // Current file excluded
   }

   #[test]
   fn test_infer_file_description() {
      assert_eq!(infer_file_description("src/test_utils.rs", ""), "test file");
      assert_eq!(infer_file_description("README.md", ""), "documentation");
      assert_eq!(infer_file_description("prompts/analysis/default.md", ""), "prompt template");
      assert_eq!(infer_file_description("system/analysis/default.md", ""), "prompt template");
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
      let json = r#"{"path": "src/lib.rs", "observations": ["a", "b", "c"]}"#;
      let result: FileObservationEntry =
         serde_json::from_str(json).expect("valid observation array JSON should deserialize");
      assert_eq!(result.path, "src/lib.rs");
      assert_eq!(result.observations, vec!["a", "b", "c"]);
   }

   #[test]
   fn test_deserialize_observations_stringified_array() {
      let json = r#"{"path": "src/lib.rs", "observations": "[\"a\", \"b\", \"c\"]"}"#;
      let result: FileObservationEntry = serde_json::from_str(json)
         .expect("valid stringified observation array JSON should deserialize");
      assert_eq!(result.path, "src/lib.rs");
      assert_eq!(result.observations, vec!["a", "b", "c"]);
   }

   #[test]
   fn test_deserialize_observations_bullet_string() {
      let json = r#"{"path": "src/lib.rs", "observations": "- updated function\n- fixed bug"}"#;
      let result: FileObservationEntry =
         serde_json::from_str(json).expect("valid bullet observation JSON should deserialize");
      assert_eq!(result.path, "src/lib.rs");
      assert_eq!(result.observations, vec!["updated function", "fixed bug"]);
   }
}
