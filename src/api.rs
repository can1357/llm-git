use std::{path::Path, sync::OnceLock, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
   config::{CommitConfig, ResolvedApiMode},
   error::{CommitGenError, Result},
   templates,
   tokens::TokenCounter,
   types::{CommitSummary, ConventionalAnalysis},
};

/// Whether API tracing is enabled (`LLM_GIT_TRACE=1`).
static TRACE_ENABLED: OnceLock<bool> = OnceLock::new();

/// Check if API request tracing is enabled via `LLM_GIT_TRACE` env var.
fn trace_enabled() -> bool {
   *TRACE_ENABLED.get_or_init(|| std::env::var("LLM_GIT_TRACE").is_ok())
}

/// Send an HTTP request with timing instrumentation.
///
/// Measures TTFT (time to first byte / headers received) separately from total
/// response time. Logs to stderr when `LLM_GIT_TRACE=1`.
pub async fn timed_send(
   request_builder: reqwest::RequestBuilder,
   label: &str,
   model: &str,
) -> std::result::Result<(reqwest::StatusCode, String), CommitGenError> {
   let trace = trace_enabled();
   let start = std::time::Instant::now();

   let response = request_builder
      .send()
      .await
      .map_err(CommitGenError::HttpError)?;

   let ttft = start.elapsed();
   let status = response.status();
   let content_length = response.content_length();

   let body = response.text().await.map_err(CommitGenError::HttpError)?;
   let total = start.elapsed();

   if trace {
      let size_info = content_length.map_or_else(
         || format!("{}B", body.len()),
         |cl| format!("{}B (content-length: {cl})", body.len()),
      );
      // Clear spinner line before printing (spinner writes \r to stdout)
      if !crate::style::pipe_mode() {
         print!("\r\x1b[K");
         std::io::Write::flush(&mut std::io::stdout()).ok();
      }
      eprintln!(
         "[TRACE] {label} model={model} status={status} ttft={ttft:.0?} total={total:.0?} \
          body={size_info}"
      );
   }

   Ok((status, body))
}

// Prompts now loaded from config instead of compile-time constants

/// Optional context information for commit analysis
#[derive(Default)]
pub struct AnalysisContext<'a> {
   /// User-provided context
   pub user_context:    Option<&'a str>,
   /// Recent commits for style learning
   pub recent_commits:  Option<&'a str>,
   /// Common scopes for suggestions
   pub common_scopes:   Option<&'a str>,
   /// Project context (language, framework) for terminology
   pub project_context: Option<&'a str>,
   /// Debug output directory for saving raw I/O
   pub debug_output:    Option<&'a Path>,
   /// Prefix for debug output files to avoid collisions
   pub debug_prefix:    Option<&'a str>,
}

/// Shared HTTP client, lazily initialized on first use.
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Get (or create) the shared HTTP client with timeouts from config.
///
/// The first call initializes the client with the given config's timeouts;
/// subsequent calls reuse the same client regardless of config values.
pub fn get_client(config: &CommitConfig) -> &'static reqwest::Client {
   CLIENT.get_or_init(|| {
      reqwest::Client::builder()
         .timeout(Duration::from_secs(config.request_timeout_secs))
         .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
         .build()
         .expect("Failed to build HTTP client")
   })
}

fn debug_filename(prefix: Option<&str>, name: &str) -> String {
   match prefix {
      Some(p) if !p.is_empty() => format!("{p}_{name}"),
      _ => name.to_string(),
   }
}

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

fn save_debug_output(debug_dir: Option<&Path>, filename: &str, content: &str) -> Result<()> {
   let Some(dir) = debug_dir else {
      return Ok(());
   };

   std::fs::create_dir_all(dir)?;
   let path = dir.join(filename);
   std::fs::write(&path, content)?;
   Ok(())
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

fn supports_openai_prompt_cache_key(config: &CommitConfig) -> bool {
   config
      .api_base_url
      .to_lowercase()
      .contains("api.openai.com")
}

/// Generate a deterministic cache key for `OpenAI` prompt-prefix routing.
pub fn openai_prompt_cache_key(
   config: &CommitConfig,
   model_name: &str,
   prompt_family: &str,
   prompt_variant: &str,
   system_prompt: &str,
) -> Option<String> {
   if system_prompt.trim().is_empty() || !supports_openai_prompt_cache_key(config) {
      return None;
   }

   Some(format!("llm-git:v1:{model_name}:{prompt_family}:{prompt_variant}"))
}

fn extract_anthropic_content(
   response_text: &str,
   tool_name: &str,
) -> Result<(Option<serde_json::Value>, String)> {
   let value: serde_json::Value = serde_json::from_str(response_text).map_err(|e| {
      CommitGenError::Other(format!(
         "Failed to parse Anthropic response JSON: {e}. Response body: {}",
         response_snippet(response_text, 500)
      ))
   })?;

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

   Ok((tool_input, text_parts.join("\n")))
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryOutput {
   summary: String,
}

/// Retry an API call with exponential backoff
pub async fn retry_api_call<T>(
   config: &CommitConfig,
   mut f: impl AsyncFnMut() -> Result<(bool, Option<T>)>,
) -> Result<T> {
   let mut attempt = 0;

   loop {
      attempt += 1;

      match f().await {
         Ok((false, Some(result))) => return Ok(result),
         Ok((false, None)) => {
            return Err(CommitGenError::Other("API call failed without result".to_string()));
         },
         Ok((true, _)) if attempt < config.max_retries => {
            let backoff_ms = config.initial_backoff_ms * (1 << (attempt - 1));
            eprintln!(
               "{}",
               crate::style::warning(&format!(
                  "Retry {}/{} after {}ms...",
                  attempt, config.max_retries, backoff_ms
               ))
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
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
                  "{}",
                  crate::style::warning(&format!(
                     "Error: {} - Retry {}/{} after {}ms...",
                     e, attempt, config.max_retries, backoff_ms
                  ))
               );
               tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
               continue;
            }
            return Err(e);
         },
      }
   }
}

/// Format commit types from config into a rich description for the prompt
/// Order is preserved from config (first = highest priority)
pub fn format_types_description(config: &CommitConfig) -> String {
   use std::fmt::Write;
   let mut out = String::from("Check types in order (first match wins):\n\n");

   for (name, tc) in &config.types {
      let _ = writeln!(out, "**{name}**: {}", tc.description);
      if !tc.diff_indicators.is_empty() {
         let _ = writeln!(out, "  Diff indicators: `{}`", tc.diff_indicators.join("`, `"));
      }
      if !tc.file_patterns.is_empty() {
         let _ = writeln!(out, "  File patterns: {}", tc.file_patterns.join(", "));
      }
      for ex in &tc.examples {
         let _ = writeln!(out, "  - {ex}");
      }
      if !tc.hint.is_empty() {
         let _ = writeln!(out, "  Note: {}", tc.hint);
      }
      out.push('\n');
   }

   if !config.classifier_hint.is_empty() {
      let _ = writeln!(out, "\n{}", config.classifier_hint);
   }

   out
}

/// Generate conventional commit analysis using OpenAI-compatible API
pub async fn generate_conventional_analysis<'a>(
   stat: &'a str,
   diff: &'a str,
   model_name: &'a str,
   scope_candidates_str: &'a str,
   ctx: &AnalysisContext<'a>,
   config: &'a CommitConfig,
) -> Result<ConventionalAnalysis> {
   retry_api_call(config, async move || {
      let client = get_client(config);

      // Build type enum from config
      let type_enum: Vec<&str> = config.types.keys().map(|s| s.as_str()).collect();

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
               }),
               required:   vec![
                  "type".to_string(),
                  "details".to_string(),
                  "issue_refs".to_string(),
               ],
            },
         },
      };

      let debug_dir = ctx.debug_output;
      let debug_prefix = ctx.debug_prefix;
      let mode = config.resolved_api_mode(model_name);

      let response_text = match mode {
         ResolvedApiMode::ChatCompletions => {
            let types_desc = format_types_description(config);
            let parts = templates::render_analysis_prompt(&templates::AnalysisParams {
               variant: &config.analysis_prompt_variant,
               stat,
               diff,
               scope_candidates: scope_candidates_str,
               recent_commits: ctx.recent_commits,
               common_scopes: ctx.common_scopes,
               types_description: Some(&types_desc),
               project_context: ctx.project_context,
            })?;

            let user_content = if let Some(user_ctx) = ctx.user_context {
               format!("ADDITIONAL CONTEXT FROM USER:\n{user_ctx}\n\n{}", parts.user)
            } else {
               parts.user
            };

            let prompt_cache_key = openai_prompt_cache_key(
               config,
               model_name,
               "analysis",
               &config.analysis_prompt_variant,
               &parts.system,
            );
            let request = ApiRequest {
               model:            model_name.to_string(),
               max_tokens:       1000,
               temperature:      config.temperature,
               tools:            vec![tool],
               tool_choice:      Some(
                  serde_json::json!({ "type": "function", "function": { "name": "create_conventional_analysis" } }),
               ),
               prompt_cache_key,
               messages:         vec![
                  Message { role: "system".to_string(), content: parts.system },
                  Message { role: "user".to_string(), content: user_content },
               ],
            };

            if debug_dir.is_some() {
               let request_json = serde_json::to_string_pretty(&request)?;
               save_debug_output(
                  debug_dir,
                  &debug_filename(debug_prefix, "analysis_request.json"),
                  &request_json,
               )?;
            }

            let mut request_builder = client
               .post(format!("{}/chat/completions", config.api_base_url))
               .header("content-type", "application/json");

            // Add Authorization header if API key is configured
            if let Some(api_key) = &config.api_key {
               request_builder =
                  request_builder.header("Authorization", format!("Bearer {api_key}"));
            }

            let (status, response_text) =
               timed_send(request_builder.json(&request), "analysis", model_name).await?;
            if debug_dir.is_some() {
               save_debug_output(
                  debug_dir,
                  &debug_filename(debug_prefix, "analysis_response.json"),
                  &response_text,
               )?;
            }

            // Retry on 5xx errors
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
            let types_desc = format_types_description(config);
            let parts = templates::render_analysis_prompt(&templates::AnalysisParams {
               variant: &config.analysis_prompt_variant,
               stat,
               diff,
               scope_candidates: scope_candidates_str,
               recent_commits: ctx.recent_commits,
               common_scopes: ctx.common_scopes,
               types_description: Some(&types_desc),
               project_context: ctx.project_context,
            })?;

            let user_content = if let Some(user_ctx) = ctx.user_context {
               format!("ADDITIONAL CONTEXT FROM USER:\n{user_ctx}\n\n{}", parts.user)
            } else {
               parts.user
            };

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
               max_tokens:  1000,
               temperature: config.temperature,
               system:      anthropic_system_content(&parts.system, prompt_caching),
               tools,
               tool_choice: Some(AnthropicToolChoice {
                  choice_type: "tool".to_string(),
                  name:        "create_conventional_analysis".to_string(),
               }),
               messages:    vec![AnthropicMessage {
                  role:    "user".to_string(),
                  content: vec![anthropic_text_content(user_content, false)],
               }],
            };

            if debug_dir.is_some() {
               let request_json = serde_json::to_string_pretty(&request)?;
               save_debug_output(
                  debug_dir,
                  &debug_filename(debug_prefix, "analysis_request.json"),
                  &request_json,
               )?;
            }

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
               timed_send(request_builder.json(&request), "analysis", model_name).await?;
            if debug_dir.is_some() {
               save_debug_output(
                  debug_dir,
                  &debug_filename(debug_prefix, "analysis_response.json"),
                  &response_text,
               )?;
            }

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
         crate::style::warn("Model returned empty response body for analysis; retrying.");
         return Ok((true, None));
      }

      match mode {
         ResolvedApiMode::ChatCompletions => {
            let api_response: ApiResponse = serde_json::from_str(&response_text).map_err(|e| {
               CommitGenError::Other(format!(
                  "Failed to parse analysis response JSON: {e}. Response body: {}",
                  response_snippet(&response_text, 500)
               ))
            })?;

            if api_response.choices.is_empty() {
               return Err(CommitGenError::Other(
                  "API returned empty response for change analysis".to_string(),
               ));
            }

            let message = &api_response.choices[0].message;

            // Find the tool call in the response
            if !message.tool_calls.is_empty() {
               let tool_call = &message.tool_calls[0];
               if tool_call
                  .function
                  .name
                  .ends_with("create_conventional_analysis")
               {
                  let args = &tool_call.function.arguments;
                  if args.is_empty() {
                     crate::style::warn(
                        "Model returned empty function arguments. Model may not support function \
                         calling properly.",
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
               if content.trim().is_empty() {
                  crate::style::warn("Model returned empty content for analysis; retrying.");
                  return Ok((true, None));
               }
               let analysis: ConventionalAnalysis =
                  serde_json::from_str(content.trim()).map_err(|e| {
                     CommitGenError::Other(format!(
                        "Failed to parse analysis content JSON: {e}. Content: {}",
                        response_snippet(content, 500)
                     ))
                  })?;
               return Ok((false, Some(analysis)));
            }

            Err(CommitGenError::Other("No conventional analysis found in API response".to_string()))
         },
         ResolvedApiMode::AnthropicMessages => {
            let (tool_input, text_content) =
               extract_anthropic_content(&response_text, "create_conventional_analysis")?;

            if let Some(input) = tool_input {
               let analysis: ConventionalAnalysis = serde_json::from_value(input).map_err(|e| {
                  CommitGenError::Other(format!(
                     "Failed to parse analysis tool input: {e}. Response body: {}",
                     response_snippet(&response_text, 500)
                  ))
               })?;
               return Ok((false, Some(analysis)));
            }

            if text_content.trim().is_empty() {
               crate::style::warn("Model returned empty content for analysis; retrying.");
               return Ok((true, None));
            }

            let analysis: ConventionalAnalysis = serde_json::from_str(text_content.trim())
               .map_err(|e| {
                  CommitGenError::Other(format!(
                     "Failed to parse analysis content JSON: {e}. Content: {}",
                     response_snippet(&text_content, 500)
                  ))
               })?;
            Ok((false, Some(analysis)))
         },
      }
   }).await
}

/// Strip conventional commit type prefix if LLM included it in summary.
///
/// Some models return the full format `feat(scope): summary` instead of just
/// `summary`. This function removes the prefix to normalize the response.
fn strip_type_prefix(summary: &str, commit_type: &str, scope: Option<&str>) -> String {
   let scope_part = scope.map(|s| format!("({s})")).unwrap_or_default();
   let prefix = format!("{commit_type}{scope_part}: ");

   summary
      .strip_prefix(&prefix)
      .or_else(|| {
         // Also try without scope in case model omitted it
         let prefix_no_scope = format!("{commit_type}: ");
         summary.strip_prefix(&prefix_no_scope)
      })
      .unwrap_or(summary)
      .to_string()
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
         crate::style::warn(&format!(
            "Type mismatch: {}% .md files but type is '{}' (consider docs type)",
            md_count * 100 / total,
            commit_type
         ));
      }

      // If no code files and type=feat/fix, warn
      let code_exts = [
         // Systems programming
         "rs", "c", "cpp", "cc", "cxx", "h", "hpp", "hxx", "zig", "nim", "v",
         // JVM languages
         "java", "kt", "kts", "scala", "groovy", "clj", "cljs", // .NET languages
         "cs", "fs", "vb", // Web/scripting
         "js", "ts", "jsx", "tsx", "mjs", "cjs", "vue", "svelte", // Python ecosystem
         "py", "pyx", "pxd", "pyi", // Ruby
         "rb", "rake", "gemspec", // PHP
         "php",     // Go
         "go",      // Swift/Objective-C
         "swift", "m", "mm",  // Lua
         "lua", // Shell
         "sh", "bash", "zsh", "fish", // Perl
         "pl", "pm", // Haskell/ML family
         "hs", "lhs", "ml", "mli", "fs", "fsi", "elm", "ex", "exs", "erl", "hrl",
         // Lisp family
         "lisp", "cl", "el", "scm", "rkt", // Julia
         "jl",  // R
         "r", "R",    // Dart/Flutter
         "dart", // Crystal
         "cr",   // D
         "d",    // Fortran
         "f", "f90", "f95", "f03", "f08", // Ada
         "ada", "adb", "ads", // Cobol
         "cob", "cbl", // Assembly
         "asm", "s", "S", // SQL (stored procs)
         "sql", "plsql", // Prolog
         "pl", "pro", // OCaml/ReasonML
         "re", "rei", // Nix
         "nix", // Terraform/HCL
         "tf", "hcl",  // Solidity
         "sol",  // Move
         "move", // Cairo
         "cairo",
      ];
      let code_count = file_exts
         .iter()
         .filter(|&&e| code_exts.contains(&e))
         .count();
      if code_count == 0 && (commit_type == "feat" || commit_type == "fix") {
         crate::style::warn(&format!(
            "Type mismatch: no code files changed but type is '{commit_type}'"
         ));
      }
   }

   Ok(())
}

/// Create commit summary using a smaller model focused on detail retention
#[allow(clippy::too_many_arguments, reason = "summary generation needs debug hooks and context")]
pub async fn generate_summary_from_analysis<'a>(
   stat: &'a str,
   commit_type: &'a str,
   scope: Option<&'a str>,
   details: &'a [String],
   user_context: Option<&'a str>,
   config: &'a CommitConfig,
   debug_dir: Option<&'a Path>,
   debug_prefix: Option<&'a str>,
) -> Result<CommitSummary> {
   let mut validation_attempt = 0;
   let max_validation_retries = 1;
   let mut last_failure_reason: Option<String> = None;

   loop {
      let additional_constraint = if let Some(reason) = &last_failure_reason {
         format!("\n\nCRITICAL: Previous attempt failed because {reason}. Correct this.")
      } else {
         String::new()
      };

      let result = retry_api_call(config, async move || {
         // Pass details as plain sentences (no numbering - prevents model parroting)
         let bullet_points = details.join("\n");

         let client = get_client(config);

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

         let mode = config.resolved_api_mode(&config.model);

         let response_text = match mode {
            ResolvedApiMode::ChatCompletions => {
               let details_str = if bullet_points.is_empty() {
                  "None (no supporting detail points were generated)."
               } else {
                  bullet_points.as_str()
               };

               let parts = templates::render_summary_prompt(
                  &config.summary_prompt_variant,
                  commit_type,
                  scope_str,
                  &max_summary_len.to_string(),
                  details_str,
                  stat.trim(),
                  user_context,
               )?;

               let user_content = format!("{}{additional_constraint}", parts.user);

               let prompt_cache_key = openai_prompt_cache_key(
                  config,
                  &config.model,
                  "summary",
                  &config.summary_prompt_variant,
                  &parts.system,
               );
               let request = ApiRequest {
                  model:            config.model.clone(),
                  max_tokens:       200,
                  temperature:      config.temperature,
                  tools:            vec![tool],
                  tool_choice:      Some(serde_json::json!({
                     "type": "function",
                     "function": { "name": "create_commit_summary" }
                  })),
                  prompt_cache_key,
                  messages:         vec![
                     Message { role: "system".to_string(), content: parts.system },
                     Message { role: "user".to_string(), content: user_content },
                  ],
               };

               if debug_dir.is_some() {
                  let request_json = serde_json::to_string_pretty(&request)?;
                  save_debug_output(
                     debug_dir,
                     &debug_filename(debug_prefix, "summary_request.json"),
                     &request_json,
                  )?;
               }

               let mut request_builder = client
                  .post(format!("{}/chat/completions", config.api_base_url))
                  .header("content-type", "application/json");

               // Add Authorization header if API key is configured
               if let Some(api_key) = &config.api_key {
                  request_builder =
                     request_builder.header("Authorization", format!("Bearer {api_key}"));
               }

               let (status, response_text) =
                  timed_send(request_builder.json(&request), "summary", &config.model).await?;
               if debug_dir.is_some() {
                  save_debug_output(
                     debug_dir,
                     &debug_filename(debug_prefix, "summary_response.json"),
                     &response_text,
                  )?;
               }

               // Retry on 5xx errors
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
               let details_str = if bullet_points.is_empty() {
                  "None (no supporting detail points were generated)."
               } else {
                  bullet_points.as_str()
               };

               let parts = templates::render_summary_prompt(
                  &config.summary_prompt_variant,
                  commit_type,
                  scope_str,
                  &max_summary_len.to_string(),
                  details_str,
                  stat.trim(),
                  user_context,
               )?;

               let user_content = format!("{}{additional_constraint}", parts.user);

               let prompt_caching = anthropic_prompt_caching_enabled(config);
               let mut tools = vec![AnthropicTool {
                  name:         "create_commit_summary".to_string(),
                  description:  "Compose a git commit summary line from detail statements"
                     .to_string(),
                  input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                           "summary": {
                              "type": "string",
                              "description": format!("Single line summary, target {} chars (hard limit {}), past tense verb first.", config.summary_guideline, config.summary_hard_limit),
                              "maxLength": config.summary_hard_limit
                           }
                        },
                        "required": ["summary"]
                     }),
                  cache_control: None,
               }];
               cache_last_anthropic_tool(&mut tools, prompt_caching);

               let request = AnthropicRequest {
                  model:       config.model.clone(),
                  max_tokens:  200,
                  temperature: config.temperature,
                  system:      anthropic_system_content(&parts.system, prompt_caching),
                  tools,
                  tool_choice: Some(AnthropicToolChoice {
                     choice_type: "tool".to_string(),
                     name:        "create_commit_summary".to_string(),
                  }),
                  messages:    vec![AnthropicMessage {
                     role:    "user".to_string(),
                     content: vec![anthropic_text_content(user_content, false)],
                  }],
               };

               if debug_dir.is_some() {
                  let request_json = serde_json::to_string_pretty(&request)?;
                  save_debug_output(
                     debug_dir,
                     &debug_filename(debug_prefix, "summary_request.json"),
                     &request_json,
                  )?;
               }

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
                  timed_send(request_builder.json(&request), "summary", &config.model).await?;
               if debug_dir.is_some() {
                  save_debug_output(
                     debug_dir,
                     &debug_filename(debug_prefix, "summary_response.json"),
                     &response_text,
                  )?;
               }

               // Retry on 5xx errors
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
            crate::style::warn("Model returned empty response body for summary; retrying.");
            return Ok((true, None));
         }

         match mode {
            ResolvedApiMode::ChatCompletions => {
               let api_response: ApiResponse =
                  serde_json::from_str(&response_text).map_err(|e| {
                     CommitGenError::Other(format!(
                        "Failed to parse summary response JSON: {e}. Response body: {}",
                        response_snippet(&response_text, 500)
                     ))
                  })?;

               if api_response.choices.is_empty() {
                  return Err(CommitGenError::Other(
                     "Summary creation response was empty".to_string(),
                  ));
               }

               let message_choice = &api_response.choices[0].message;

               if !message_choice.tool_calls.is_empty() {
                  let tool_call = &message_choice.tool_calls[0];
                  if tool_call.function.name.ends_with("create_commit_summary") {
                     let args = &tool_call.function.arguments;
                     if args.is_empty() {
                        crate::style::warn(
                           "Model returned empty function arguments for summary. Model may not \
                            support function calling.",
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
                     // Strip type prefix if LLM included it (e.g., "feat(scope): summary" ->
                     // "summary")
                     let cleaned = strip_type_prefix(&summary.summary, commit_type, scope);
                     return Ok((
                        false,
                        Some(CommitSummary::new(cleaned, config.summary_hard_limit)?),
                     ));
                  }
               }

               if let Some(content) = &message_choice.content {
                  if content.trim().is_empty() {
                     crate::style::warn("Model returned empty content for summary; retrying.");
                     return Ok((true, None));
                  }
                  // Try JSON first, fall back to plain text (for models without function calling)
                  let trimmed = content.trim();
                  let summary_text = match serde_json::from_str::<SummaryOutput>(trimmed) {
                     Ok(summary) => summary.summary,
                     Err(e) => {
                        // Only use plain text if it doesn't look like JSON
                        if trimmed.starts_with('{') {
                           return Err(CommitGenError::Other(format!(
                              "Failed to parse summary JSON: {e}. Content: {}",
                              response_snippet(trimmed, 500)
                           )));
                        }
                        // Model returned plain text instead of JSON - use it directly
                        trimmed.to_string()
                     },
                  };
                  // Strip type prefix if LLM included it
                  let cleaned = strip_type_prefix(&summary_text, commit_type, scope);
                  return Ok((
                     false,
                     Some(CommitSummary::new(cleaned, config.summary_hard_limit)?),
                  ));
               }

               Err(CommitGenError::Other(
                  "No summary found in summary creation response".to_string(),
               ))
            },
            ResolvedApiMode::AnthropicMessages => {
               let (tool_input, text_content) =
                  extract_anthropic_content(&response_text, "create_commit_summary")?;

               if let Some(input) = tool_input {
                  let summary: SummaryOutput = serde_json::from_value(input).map_err(|e| {
                     CommitGenError::Other(format!(
                        "Failed to parse summary tool input: {e}. Response body: {}",
                        response_snippet(&response_text, 500)
                     ))
                  })?;
                  let cleaned = strip_type_prefix(&summary.summary, commit_type, scope);
                  return Ok((
                     false,
                     Some(CommitSummary::new(cleaned, config.summary_hard_limit)?),
                  ));
               }

               if text_content.trim().is_empty() {
                  crate::style::warn("Model returned empty content for summary; retrying.");
                  return Ok((true, None));
               }

               // Try JSON first, fall back to plain text (for models without function calling)
               let trimmed = text_content.trim();
               let summary_text = match serde_json::from_str::<SummaryOutput>(trimmed) {
                  Ok(summary) => summary.summary,
                  Err(e) => {
                     // Only use plain text if it doesn't look like JSON
                     if trimmed.starts_with('{') {
                        return Err(CommitGenError::Other(format!(
                           "Failed to parse summary JSON: {e}. Content: {}",
                           response_snippet(trimmed, 500)
                        )));
                     }
                     // Model returned plain text instead of JSON - use it directly
                     trimmed.to_string()
                  },
               };
               let cleaned = strip_type_prefix(&summary_text, commit_type, scope);
               Ok((false, Some(CommitSummary::new(cleaned, config.summary_hard_limit)?)))
            },
         }
      }).await;

      match result {
         Ok(summary) => {
            // Validate quality
            match validate_summary_quality(summary.as_str(), commit_type, stat) {
               Ok(()) => return Ok(summary),
               Err(reason) if validation_attempt < max_validation_retries => {
                  crate::style::warn(&format!(
                     "Validation failed (attempt {}/{}): {}",
                     validation_attempt + 1,
                     max_validation_retries + 1,
                     reason
                  ));
                  last_failure_reason = Some(reason);
                  validation_attempt += 1;
                  // Retry with constraint
               },
               Err(reason) => {
                  crate::style::warn(&format!(
                     "Validation failed after {} retries: {}. Using fallback.",
                     max_validation_retries + 1,
                     reason
                  ));
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

/// Generate conventional commit analysis, using map-reduce for large diffs
///
/// This is the main entry point for analysis. It automatically routes to
/// map-reduce when the diff exceeds the configured token threshold.
pub async fn generate_analysis_with_map_reduce<'a>(
   stat: &'a str,
   diff: &'a str,
   model_name: &'a str,
   scope_candidates_str: &'a str,
   ctx: &AnalysisContext<'a>,
   config: &'a CommitConfig,
   counter: &TokenCounter,
) -> Result<ConventionalAnalysis> {
   use crate::map_reduce::{run_map_reduce, should_use_map_reduce};

   if should_use_map_reduce(diff, config, counter) {
      crate::style::print_info(&format!(
         "Large diff detected ({} tokens), using map-reduce...",
         counter.count_sync(diff)
      ));
      run_map_reduce(diff, stat, scope_candidates_str, model_name, config, counter).await
   } else {
      generate_conventional_analysis(stat, diff, model_name, scope_candidates_str, ctx, config)
         .await
   }
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
