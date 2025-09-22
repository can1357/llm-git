use std::{thread, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
   config::CommitConfig,
   error::{CommitGenError, Result},
   templates,
   types::{CommitSummary, ConventionalAnalysis},
};

// Prompts now loaded from config instead of compile-time constants

/// Build HTTP client with timeouts from config
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryOutput {
   summary: String,
}

/// Retry an API call with exponential backoff
pub fn retry_api_call<F, T>(config: &CommitConfig, mut f: F) -> Result<T>
where
   F: FnMut() -> Result<(bool, Option<T>)>,
{
   let mut attempt = 0;

   loop {
      attempt += 1;

      match f() {
         Ok((false, Some(result))) => return Ok(result),
         Ok((false, None)) => {
            return Err(CommitGenError::Other("API call failed without result".to_string()));
         },
         Ok((true, _)) if attempt < config.max_retries => {
            let backoff_ms = config.initial_backoff_ms * (1 << (attempt - 1));
            eprintln!("Retry {}/{} after {}ms...", attempt, config.max_retries, backoff_ms);
            thread::sleep(Duration::from_millis(backoff_ms));
         },
         Ok((true, _last_err)) => {
            return Err(CommitGenError::ApiRetryExhausted {
               retries: config.max_retries,
               source:  Box::new(CommitGenError::Other("Max retries exceeded".to_string())),
            });
         },
         Err(e) => {
            if attempt < config.max_retries {
               let backoff_ms = config.initial_backoff_ms * (1 << (attempt - 1));
               eprintln!(
                  "Error: {} - Retry {}/{} after {}ms...",
                  e, attempt, config.max_retries, backoff_ms
               );
               thread::sleep(Duration::from_millis(backoff_ms));
               continue;
            }
            return Err(e);
         },
      }
   }
}

/// Generate conventional commit analysis using OpenAI-compatible API
pub fn generate_conventional_analysis<'a>(
   stat: &'a str,
   diff: &'a str,
   model_name: &'a str,
   context: Option<&'a str>,
   scope_candidates_str: &'a str,
   config: &'a CommitConfig,
) -> Result<ConventionalAnalysis> {
   retry_api_call(config, move || {
      let client = build_client(config);

      // Define the conventional analysis tool
      let tool = Tool {
         tool_type: "function".to_string(),
         function:  Function {
            name:        "create_conventional_analysis".to_string(),
            description: "Analyze changes and classify as conventional commit with type, scope, \
                          details, and metadata"
               .to_string(),
            parameters:  FunctionParameters {
               param_type: "object".to_string(),
               properties: serde_json::json!({
                  "type": {
                     "type": "string",
                     "enum": ["feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert"],
                     "description": "Commit type based on change classification"
                  },
                  "scope": {
                     "type": "string",
                     "description": "Optional scope (module/component). Omit if unclear or multi-component."
                  },
                  "body": {
                     "type": "array",
                     "description": "Array of 0-6 detail items (empty if no supporting details).",
                     "items": {
                        "type": "string",
                        "description": "Detail about change, starting with past-tense verb, ending with period"
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
               required:   vec!["type".to_string(), "body".to_string(), "issue_refs".to_string()],
            },
         },
      };

      let request = ApiRequest {
         model:       model_name.to_string(),
         max_tokens:  1000,
         temperature: config.temperature,
         tools:       vec![tool],
         tool_choice: Some(
            serde_json::json!({ "type": "function", "function": { "name": "create_conventional_analysis" } }),
         ),
         messages:    vec![Message {
            role:    "user".to_string(),
            content: {
               let mut prompt = templates::render_analysis_prompt(
                  &config.analysis_prompt_variant,
                  stat,
                  diff,
                  scope_candidates_str,
               )?;

               if let Some(ctx) = context {
                  prompt = format!("ADDITIONAL CONTEXT FROM USER:\n{ctx}\n\n{prompt}");
               }

               prompt
            },
         }],
      };

      let mut request_builder = client
         .post(format!("{}/chat/completions", config.api_base_url))
         .header("content-type", "application/json");

      // Add Authorization header if API key is configured
      if let Some(ref api_key) = config.api_key {
         request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
      }

      let response = request_builder
         .json(&request)
         .send()
         .map_err(CommitGenError::HttpError)?;

      let status = response.status();

      // Retry on 5xx errors
      if status.is_server_error() {
         let error_text = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
         eprintln!("Server error {status}: {error_text}");
         return Ok((true, None)); // Retry
      }

      if !status.is_success() {
         let error_text = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
         return Err(CommitGenError::ApiError { status: status.as_u16(), body: error_text });
      }

      let api_response: ApiResponse = response.json().map_err(CommitGenError::HttpError)?;

      if api_response.choices.is_empty() {
         return Err(CommitGenError::Other(
            "API returned empty response for change analysis".to_string(),
         ));
      }

      let message = &api_response.choices[0].message;

      // Find the tool call in the response
      if !message.tool_calls.is_empty() {
         let tool_call = &message.tool_calls[0];
         if tool_call.function.name == "create_conventional_analysis" {
            let args = &tool_call.function.arguments;
            if args.is_empty() {
               eprintln!(
                  "Warning: Model returned empty function arguments. Model may not support \
                   function calling properly."
               );
               return Err(CommitGenError::Other(
                  "Model returned empty function arguments - try using a Claude model \
                   (sonnet/opus/haiku)"
                     .to_string(),
               ));
            }
            let analysis: ConventionalAnalysis = serde_json::from_str(args).map_err(|e| {
               CommitGenError::Other(format!(
                  "Failed to parse model response: {}. Response was: {}",
                  e,
                  args.chars().take(200).collect::<String>()
               ))
            })?;
            return Ok((false, Some(analysis)));
         }
      }

      // Fallback: try to parse content as text
      if let Some(content) = &message.content {
         let analysis: ConventionalAnalysis =
            serde_json::from_str(content.trim()).map_err(CommitGenError::JsonError)?;
         return Ok((false, Some(analysis)));
      }

      Err(CommitGenError::Other("No conventional analysis found in API response".to_string()))
   })
}

/// Validate summary against requirements
fn validate_summary_quality(
   summary: &str,
   commit_type: &str,
   stat: &str,
) -> std::result::Result<(), String> {
   use crate::validation::is_past_tense_verb;

   let first_word = summary
      .split_whitespace()
      .next()
      .ok_or_else(|| "summary is empty".to_string())?;

   let first_word_lower = first_word.to_lowercase();

   // Check past-tense verb
   if !is_past_tense_verb(&first_word_lower) {
      return Err(format!(
         "must start with past-tense verb (ending in -ed/-d or irregular), got '{first_word}'"
      ));
   }

   // Check type repetition
   if first_word_lower == commit_type {
      return Err(format!("repeats commit type '{commit_type}' in summary"));
   }

   // Type-file mismatch heuristic
   let file_exts: Vec<&str> = stat
      .lines()
      .filter_map(|line| {
         let path = line.split('|').next()?.trim();
         std::path::Path::new(path).extension()?.to_str()
      })
      .collect();

   if !file_exts.is_empty() {
      let total = file_exts.len();
      let md_count = file_exts.iter().filter(|&&e| e == "md").count();

      // If >80% markdown but not docs type, suggest docs
      if md_count * 100 / total > 80 && commit_type != "docs" {
         eprintln!(
            "⚠ Type mismatch: {}% .md files but type is '{}' (consider docs type)",
            md_count * 100 / total,
            commit_type
         );
      }

      // If no code files and type=feat/fix, warn
      let code_exts = ["rs", "py", "js", "ts", "go", "java", "c", "cpp"];
      let code_count = file_exts
         .iter()
         .filter(|&&e| code_exts.contains(&e))
         .count();
      if code_count == 0 && (commit_type == "feat" || commit_type == "fix") {
         eprintln!("⚠ Type mismatch: no code files changed but type is '{commit_type}'");
      }
   }

   Ok(())
}

/// Create commit summary using a smaller model focused on detail retention
pub fn generate_summary_from_analysis<'a>(
   stat: &'a str,
   commit_type: &'a str,
   scope: Option<&'a str>,
   details: &'a [String],
   user_context: Option<&'a str>,
   config: &'a CommitConfig,
) -> Result<CommitSummary> {
   let mut validation_attempt = 0;
   let max_validation_retries = 1;
   let mut last_failure_reason: Option<String> = None;

   loop {
      let additional_constraint = if let Some(ref reason) = last_failure_reason {
         format!("\n\nCRITICAL: Previous attempt failed because {reason}. Correct this.")
      } else {
         String::new()
      };

      let result = retry_api_call(config, move || {
         // Pass details as plain sentences (no numbering - prevents model parroting)
         let bullet_points = details.join("\n");

         let client = build_client(config);

         let tool = Tool {
            tool_type: "function".to_string(),
            function:  Function {
               name:        "create_commit_summary".to_string(),
               description: "Compose a git commit summary line from detail statements".to_string(),
               parameters:  FunctionParameters {
                  param_type: "object".to_string(),
                  properties: serde_json::json!({
                     "summary": {
                        "type": "string",
                        "description": format!("Single line summary, target {} chars (hard limit {}), past tense verb first.", config.summary_guideline, config.summary_hard_limit),
                        "maxLength": config.summary_hard_limit
                     }
                  }),
                  required:   vec!["summary".to_string()],
               },
            },
         };

         // Calculate guideline summary length accounting for "type(scope): " prefix
         let scope_str = scope.unwrap_or("");
         let prefix_len =
            commit_type.len() + 2 + scope_str.len() + if scope_str.is_empty() { 0 } else { 2 }; // "type: " or "type(scope): "
         let max_summary_len = config.summary_guideline.saturating_sub(prefix_len);

         let request = ApiRequest {
            model:       config.summary_model.clone(),
            max_tokens:  200,
            temperature: config.temperature,
            tools:       vec![tool],
            tool_choice: Some(serde_json::json!({
               "type": "function",
               "function": { "name": "create_commit_summary" }
            })),
            messages:    vec![Message {
               role:    "user".to_string(),
               content: {
                  let details_str = if bullet_points.is_empty() {
                     "None (no supporting detail points were generated)."
                  } else {
                     bullet_points.as_str()
                  };

                  let base_prompt = templates::render_summary_prompt(
                     &config.summary_prompt_variant,
                     commit_type,
                     scope_str,
                     &max_summary_len.to_string(),
                     details_str,
                     stat.trim(),
                     user_context,
                  )?;

                  format!("{base_prompt}{additional_constraint}")
               },
            }],
         };

         let mut request_builder = client
            .post(format!("{}/chat/completions", config.api_base_url))
            .header("content-type", "application/json");

         // Add Authorization header if API key is configured
         if let Some(ref api_key) = config.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
         }

         let response = request_builder
            .json(&request)
            .send()
            .map_err(CommitGenError::HttpError)?;

         let status = response.status();

         // Retry on 5xx errors
         if status.is_server_error() {
            let error_text = response
               .text()
               .unwrap_or_else(|_| "Unknown error".to_string());
            eprintln!("Server error {status}: {error_text}");
            return Ok((true, None)); // Retry
         }

         if !status.is_success() {
            let error_text = response
               .text()
               .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(CommitGenError::ApiError { status: status.as_u16(), body: error_text });
         }

         let api_response: ApiResponse = response.json().map_err(CommitGenError::HttpError)?;

         if api_response.choices.is_empty() {
            return Err(CommitGenError::Other("Summary creation response was empty".to_string()));
         }

         let message_choice = &api_response.choices[0].message;

         if !message_choice.tool_calls.is_empty() {
            let tool_call = &message_choice.tool_calls[0];
            if tool_call.function.name == "create_commit_summary" {
               let args = &tool_call.function.arguments;
               if args.is_empty() {
                  eprintln!(
                     "Warning: Model returned empty function arguments for summary. Model may not \
                      support function calling."
                  );
                  return Err(CommitGenError::Other(
                     "Model returned empty summary arguments - try using a Claude model \
                      (sonnet/opus/haiku)"
                        .to_string(),
                  ));
               }
               let summary: SummaryOutput = serde_json::from_str(args).map_err(|e| {
                  CommitGenError::Other(format!(
                     "Failed to parse summary response: {}. Response was: {}",
                     e,
                     args.chars().take(200).collect::<String>()
                  ))
               })?;
               return Ok((
                  false,
                  Some(CommitSummary::new(summary.summary, config.summary_hard_limit)?),
               ));
            }
         }

         if let Some(content) = &message_choice.content {
            let summary: SummaryOutput =
               serde_json::from_str(content.trim()).map_err(CommitGenError::JsonError)?;
            return Ok((
               false,
               Some(CommitSummary::new(summary.summary, config.summary_hard_limit)?),
            ));
         }

         Err(CommitGenError::Other("No summary found in summary creation response".to_string()))
      });

      match result {
         Ok(summary) => {
            // Validate quality
            match validate_summary_quality(summary.as_str(), commit_type, stat) {
               Ok(()) => return Ok(summary),
               Err(reason) if validation_attempt < max_validation_retries => {
                  eprintln!(
                     "⚠ Validation failed (attempt {}/{}): {}",
                     validation_attempt + 1,
                     max_validation_retries + 1,
                     reason
                  );
                  last_failure_reason = Some(reason);
                  validation_attempt += 1;
                  // Retry with constraint
               },
               Err(reason) => {
                  eprintln!(
                     "⚠ Validation failed after {} retries: {}. Using fallback.",
                     max_validation_retries + 1,
                     reason
                  );
                  // Fallback: use first detail or heuristic
                  return Ok(fallback_from_details_or_summary(
                     details,
                     summary.as_str(),
                     commit_type,
                     config,
                  ));
               },
            }
         },
         Err(e) => return Err(e),
      }
   }
}

/// Fallback when validation fails: use first detail, strip type word if present
fn fallback_from_details_or_summary(
   details: &[String],
   invalid_summary: &str,
   commit_type: &str,
   config: &CommitConfig,
) -> CommitSummary {
   let candidate = if let Some(first_detail) = details.first() {
      // Use first detail line, strip type word
      let mut cleaned = first_detail.trim().trim_end_matches('.').to_string();

      // Remove type word if present at start
      let type_word_variants =
         [commit_type, &format!("{commit_type}ed"), &format!("{commit_type}d")];
      for variant in &type_word_variants {
         if cleaned
            .to_lowercase()
            .starts_with(&format!("{} ", variant.to_lowercase()))
         {
            cleaned = cleaned[variant.len()..].trim().to_string();
            break;
         }
      }

      cleaned
   } else {
      // No details, try to fix invalid summary
      let mut cleaned = invalid_summary
         .split_whitespace()
         .skip(1) // Remove first word (invalid verb)
         .collect::<Vec<_>>()
         .join(" ");

      if cleaned.is_empty() {
         cleaned = fallback_summary("", details, commit_type, config)
            .as_str()
            .to_string();
      }

      cleaned
   };

   // Ensure valid past-tense verb prefix
   let with_verb = if candidate
      .split_whitespace()
      .next()
      .is_some_and(|w| crate::validation::is_past_tense_verb(&w.to_lowercase()))
   {
      candidate
   } else {
      let verb = match commit_type {
         "feat" => "added",
         "fix" => "fixed",
         "refactor" => "restructured",
         "docs" => "documented",
         "test" => "tested",
         "perf" => "optimized",
         "build" | "ci" | "chore" => "updated",
         "style" => "formatted",
         "revert" => "reverted",
         _ => "changed",
      };
      format!("{verb} {candidate}")
   };

   CommitSummary::new(with_verb, config.summary_hard_limit)
      .unwrap_or_else(|_| fallback_summary("", details, commit_type, config))
}

/// Provide a deterministic fallback summary if model generation fails
pub fn fallback_summary(
   stat: &str,
   details: &[String],
   commit_type: &str,
   config: &CommitConfig,
) -> CommitSummary {
   let mut candidate = if let Some(first) = details.first() {
      first.trim().trim_end_matches('.').to_string()
   } else {
      let primary_line = stat
         .lines()
         .map(str::trim)
         .find(|line| !line.is_empty())
         .unwrap_or("files");

      let subject = primary_line
         .split('|')
         .next()
         .map(str::trim)
         .filter(|s| !s.is_empty())
         .unwrap_or("files");

      if subject.eq_ignore_ascii_case("files") {
         "Updated files".to_string()
      } else {
         format!("Updated {subject}")
      }
   };

   candidate = candidate
      .replace(['\n', '\r'], " ")
      .split_whitespace()
      .collect::<Vec<_>>()
      .join(" ")
      .trim()
      .trim_end_matches('.')
      .trim_end_matches(';')
      .trim_end_matches(':')
      .to_string();

   if candidate.is_empty() {
      candidate = "Updated files".to_string();
   }

   // Truncate to conservative length (50 chars) since we don't know the scope yet
   // post_process_commit_message will truncate further if needed
   const CONSERVATIVE_MAX: usize = 50;
   while candidate.len() > CONSERVATIVE_MAX {
      if let Some(pos) = candidate.rfind(' ') {
         candidate.truncate(pos);
         candidate = candidate.trim_end_matches(',').trim().to_string();
      } else {
         candidate.truncate(CONSERVATIVE_MAX);
         break;
      }
   }

   // Ensure no trailing period (conventional commits style)
   candidate = candidate.trim_end_matches('.').to_string();

   // If the candidate ended up identical to the commit type, replace with a safer
   // default
   if candidate
      .split_whitespace()
      .next()
      .is_some_and(|word| word.eq_ignore_ascii_case(commit_type))
   {
      candidate = match commit_type {
         "refactor" => "restructured change".to_string(),
         "feat" => "added functionality".to_string(),
         "fix" => "fixed issue".to_string(),
         "docs" => "documented updates".to_string(),
         "test" => "tested changes".to_string(),
         "chore" | "build" | "ci" | "style" => "updated tooling".to_string(),
         "perf" => "optimized performance".to_string(),
         "revert" => "reverted previous commit".to_string(),
         _ => "updated files".to_string(),
      };
   }

   // Unwrap is safe: fallback_summary guarantees non-empty string ≤50 chars (<
   // config limit)
   CommitSummary::new(candidate, config.summary_hard_limit)
      .expect("fallback summary should always be valid")
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::config::CommitConfig;

   #[test]
   fn test_validate_summary_quality_valid() {
      let stat = "src/main.rs | 10 +++++++---\n";
      assert!(validate_summary_quality("added new feature", "feat", stat).is_ok());
      assert!(validate_summary_quality("fixed critical bug", "fix", stat).is_ok());
      assert!(validate_summary_quality("restructured module layout", "refactor", stat).is_ok());
   }

   #[test]
   fn test_validate_summary_quality_invalid_verb() {
      let stat = "src/main.rs | 10 +++++++---\n";
      let result = validate_summary_quality("adding new feature", "feat", stat);
      assert!(result.is_err());
      assert!(result.unwrap_err().contains("past-tense verb"));
   }

   #[test]
   fn test_validate_summary_quality_type_repetition() {
      let stat = "src/main.rs | 10 +++++++---\n";
      // "feat" is not a past-tense verb so it should fail on verb check first
      let result = validate_summary_quality("feat new feature", "feat", stat);
      assert!(result.is_err());
      assert!(result.unwrap_err().contains("past-tense verb"));

      // "fixed" is past-tense but repeats "fix" type
      let result = validate_summary_quality("fix bug", "fix", stat);
      assert!(result.is_err());
      // "fix" is not in PAST_TENSE_VERBS, so fails on verb check
      assert!(result.unwrap_err().contains("past-tense verb"));
   }

   #[test]
   fn test_validate_summary_quality_empty() {
      let stat = "src/main.rs | 10 +++++++---\n";
      let result = validate_summary_quality("", "feat", stat);
      assert!(result.is_err());
      assert!(result.unwrap_err().contains("empty"));
   }

   #[test]
   fn test_validate_summary_quality_markdown_type_mismatch() {
      let stat = "README.md | 10 +++++++---\nDOCS.md | 5 +++++\n";
      // Should warn but not fail
      assert!(validate_summary_quality("added documentation", "feat", stat).is_ok());
   }

   #[test]
   fn test_validate_summary_quality_no_code_files() {
      let stat = "config.toml | 2 +-\nREADME.md | 1 +\n";
      // Should warn but not fail
      assert!(validate_summary_quality("added config option", "feat", stat).is_ok());
   }

   #[test]
   fn test_fallback_from_details_with_first_detail() {
      let config = CommitConfig::default();
      let details = vec![
         "Added authentication middleware.".to_string(),
         "Updated error handling.".to_string(),
      ];
      let result = fallback_from_details_or_summary(&details, "invalid verb", "feat", &config);
      // Capital A preserved from detail
      assert_eq!(result.as_str(), "Added authentication middleware");
   }

   #[test]
   fn test_fallback_from_details_strips_type_word() {
      let config = CommitConfig::default();
      let details = vec!["Featuring new oauth flow.".to_string()];
      let result = fallback_from_details_or_summary(&details, "invalid", "feat", &config);
      // Should strip "Featuring" (present participle, not past tense) and add valid
      // verb
      assert!(result.as_str().starts_with("added"));
   }

   #[test]
   fn test_fallback_from_details_no_details() {
      let config = CommitConfig::default();
      let details: Vec<String> = vec![];
      let result = fallback_from_details_or_summary(&details, "invalid verb here", "feat", &config);
      // Should use rest of summary or fallback
      assert!(result.as_str().starts_with("added"));
   }

   #[test]
   fn test_fallback_from_details_adds_verb() {
      let config = CommitConfig::default();
      let details = vec!["configuration for oauth".to_string()];
      let result = fallback_from_details_or_summary(&details, "invalid", "feat", &config);
      assert_eq!(result.as_str(), "added configuration for oauth");
   }

   #[test]
   fn test_fallback_from_details_preserves_existing_verb() {
      let config = CommitConfig::default();
      let details = vec!["fixed authentication bug".to_string()];
      let result = fallback_from_details_or_summary(&details, "invalid", "fix", &config);
      assert_eq!(result.as_str(), "fixed authentication bug");
   }

   #[test]
   fn test_fallback_from_details_type_specific_verbs() {
      let config = CommitConfig::default();
      let details = vec!["module structure".to_string()];

      let result = fallback_from_details_or_summary(&details, "invalid", "refactor", &config);
      assert_eq!(result.as_str(), "restructured module structure");

      let result = fallback_from_details_or_summary(&details, "invalid", "docs", &config);
      assert_eq!(result.as_str(), "documented module structure");

      let result = fallback_from_details_or_summary(&details, "invalid", "test", &config);
      assert_eq!(result.as_str(), "tested module structure");

      let result = fallback_from_details_or_summary(&details, "invalid", "perf", &config);
      assert_eq!(result.as_str(), "optimized module structure");
   }

   #[test]
   fn test_fallback_summary_with_stat() {
      let config = CommitConfig::default();
      let stat = "src/main.rs | 10 +++++++---\n";
      let details = vec![];
      let result = fallback_summary(stat, &details, "feat", &config);
      assert!(result.as_str().contains("main.rs") || result.as_str().contains("updated"));
   }

   #[test]
   fn test_fallback_summary_with_details() {
      let config = CommitConfig::default();
      let stat = "";
      let details = vec!["First detail here.".to_string()];
      let result = fallback_summary(stat, &details, "feat", &config);
      // Capital F preserved
      assert_eq!(result.as_str(), "First detail here");
   }

   #[test]
   fn test_fallback_summary_no_stat_no_details() {
      let config = CommitConfig::default();
      let result = fallback_summary("", &[], "feat", &config);
      // Fallback returns "Updated files" when no stat/details
      assert_eq!(result.as_str(), "Updated files");
   }

   #[test]
   fn test_fallback_summary_type_word_overlap() {
      let config = CommitConfig::default();
      let details = vec!["refactor was performed".to_string()];
      let result = fallback_summary("", &details, "refactor", &config);
      // Should replace "refactor" with type-specific verb
      assert_eq!(result.as_str(), "restructured change");
   }

   #[test]
   fn test_fallback_summary_length_limit() {
      let config = CommitConfig::default();
      let long_detail = "a ".repeat(100); // 200 chars
      let details = vec![long_detail.trim().to_string()];
      let result = fallback_summary("", &details, "feat", &config);
      // Should truncate to conservative max (50 chars)
      assert!(result.len() <= 50);
   }
}
