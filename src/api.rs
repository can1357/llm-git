use std::{
   path::Path,
   sync::{LazyLock, OnceLock},
   time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
   config::{CommitConfig, ResolvedApiMode},
   error::{CommitGenError, Result},
   templates,
   tokens::TokenCounter,
   types::{
      CommitSummary, CommitType, ConventionalAnalysis, ConventionalCommit, coerce_optional_scope,
   },
};

/// Whether API tracing is enabled (`LLM_GIT_TRACE=1`).
static TRACE_ENABLED: LazyLock<bool> =
   LazyLock::new(|| env_flag_value_enabled(std::env::var("LLM_GIT_TRACE").ok().as_deref()));

/// Whether per-request LLM progress logging is enabled.
///
/// `LLM_GIT_PROGRESS=1` prints query/response/cache lines. `LLM_GIT_TRACE=1`
/// implies this and also prints the lower-level trace line.
static LLM_PROGRESS_ENABLED: LazyLock<bool> = LazyLock::new(|| {
   env_flag_value_enabled(std::env::var("LLM_GIT_PROGRESS").ok().as_deref()) || trace_enabled()
});

fn env_flag_value_enabled(value: Option<&str>) -> bool {
   let Some(value) = value else {
      return false;
   };

   !matches!(value.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no" | "off")
}

/// Check if API request tracing is enabled via `LLM_GIT_TRACE` env var.
fn trace_enabled() -> bool {
   *TRACE_ENABLED
}

pub(crate) fn llm_progress_enabled() -> bool {
   *LLM_PROGRESS_ENABLED
}

pub(crate) fn print_llm_progress(message: impl FnOnce() -> String) {
   if llm_progress_enabled() {
      crate::style::print_info(&message());
   }
}

const fn api_mode_label(mode: ResolvedApiMode) -> &'static str {
   match mode {
      ResolvedApiMode::ChatCompletions => "chat completions",
      ResolvedApiMode::AnthropicMessages => "Anthropic messages",
   }
}

/// Send an HTTP request with timing instrumentation.
///
/// Measures TTFT (time to first byte / headers received) separately from total
/// response time. Logs to stderr when `LLM_GIT_TRACE=1`.
#[tracing::instrument(target = "lgit", name = "api.timed_send", skip_all, fields(operation = label, model))]
pub async fn timed_send(
   request_builder: reqwest::RequestBuilder,
   label: &str,
   model: &str,
) -> std::result::Result<(reqwest::StatusCode, String), CommitGenError> {
   let trace = trace_enabled();
   let profile = crate::profile::enabled();
   let start = std::time::Instant::now();

   if profile {
      tracing::info!(
         target: crate::profile::TARGET,
         event = "api_request_started",
         operation = label,
         model,
      );
   }

   let response = match request_builder.send().await {
      Ok(response) => response,
      Err(error) => {
         if profile {
            let elapsed = start.elapsed();
            tracing::warn!(
               target: crate::profile::TARGET,
               event = "api_request_failed",
               operation = label,
               model,
               elapsed_ms = elapsed.as_secs_f64() * 1000.0,
               elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
               error = %error,
            );
         }
         return Err(CommitGenError::HttpError(error));
      },
   };

   let ttft = start.elapsed();
   let status = response.status();
   let content_length = response.content_length();

   let body = match response.text().await {
      Ok(body) => body,
      Err(error) => {
         if profile {
            let elapsed = start.elapsed();
            tracing::warn!(
               target: crate::profile::TARGET,
               event = "api_response_body_failed",
               operation = label,
               model,
               status = status.as_u16(),
               elapsed_ms = elapsed.as_secs_f64() * 1000.0,
               elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
               error = %error,
            );
         }
         return Err(CommitGenError::HttpError(error));
      },
   };
   let total = start.elapsed();

   if profile {
      tracing::info!(
         target: crate::profile::TARGET,
         event = "api_request_finished",
         operation = label,
         model,
         status = status.as_u16(),
         success = status.is_success(),
         ttft_ms = ttft.as_secs_f64() * 1000.0,
         ttft_us = u64::try_from(ttft.as_micros()).unwrap_or(u64::MAX),
         total_ms = total.as_secs_f64() * 1000.0,
         total_us = u64::try_from(total.as_micros()).unwrap_or(u64::MAX),
         body_bytes = body.len(),
         content_length_known = content_length.is_some(),
         content_length_bytes = content_length.unwrap_or(0),
      );
   }

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

pub fn strict_json_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
   serde_json::json!({
      "type": "object",
      "properties": properties,
      "required": required,
      "additionalProperties": false
   })
}

pub(crate) fn extract_json_from_content(content: &str) -> String {
   let trimmed = content.trim();

   if trimmed.is_empty() {
      return String::new();
   }

   if let Some(start) = trimmed.find("```json") {
      let after_marker = &trimmed[start + 7..];
      if let Some(end) = after_marker.find("```") {
         return after_marker[..end].trim().to_string();
      }
   }

   if let Some(start) = trimmed.find("```") {
      let after_marker = &trimmed[start + 3..];
      let content_start = after_marker.find('\n').map_or(0, |i| i + 1);
      let after_newline = &after_marker[content_start..];
      if let Some(end) = after_newline.find("```") {
         return after_newline[..end].trim().to_string();
      }
   }

   if let Some(start) = trimmed.find('{')
      && let Some(end) = trimmed.rfind('}')
      && end >= start
   {
      return trimmed[start..=end].to_string();
   }

   trimmed.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneShotSource {
   ToolCall,
   OutputJsonParse,
   PlainTextContent,
   Cache,
}

#[derive(Debug, Clone, Copy)]
pub struct OneShotDebug<'a> {
   pub dir:    Option<&'a Path>,
   pub prefix: Option<&'a str>,
   pub name:   &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct OneShotSpec<'a> {
   pub operation:        &'a str,
   pub model:            &'a str,
   pub prompt_family:    &'a str,
   pub prompt_variant:   &'a str,
   pub system_prompt:    &'a str,
   pub user_prompt:      &'a str,
   pub tool_name:        &'a str,
   pub tool_description: &'a str,
   pub schema:           &'a serde_json::Value,
   pub progress_label:   Option<&'a str>,
   pub debug:            Option<OneShotDebug<'a>>,
   /// Look up / store the parsed response in the global LLM cache. Cache
   /// entries are keyed on a hash of the spec fields plus prompts/schema.
   pub cacheable:        bool,
}

#[derive(Debug)]
pub struct OneShotResponse<T> {
   pub output:       T,
   pub source:       OneShotSource,
   pub text_content: Option<String>,
   pub stop_reason:  Option<String>,
}

fn oneshot_progress_label<'a>(spec: &OneShotSpec<'a>) -> &'a str {
   spec.progress_label.unwrap_or(spec.operation)
}

const fn estimate_prompt_text_tokens(spec: &OneShotSpec<'_>) -> usize {
   spec
      .system_prompt
      .len()
      .saturating_add(spec.user_prompt.len())
      .saturating_add(3)
      / 4
}

const fn prompt_text_chars(spec: &OneShotSpec<'_>) -> usize {
   spec
      .system_prompt
      .len()
      .saturating_add(spec.user_prompt.len())
}

fn format_count(count: usize) -> String {
   if count >= 10_000 {
      format!("{:.1}k", count as f64 / 1000.0)
   } else {
      count.to_string()
   }
}

fn format_elapsed(elapsed: Duration) -> String {
   if elapsed.as_secs() > 0 {
      format!("{:.1}s", elapsed.as_secs_f64())
   } else {
      format!("{}ms", elapsed.as_millis())
   }
}

fn format_bytes(bytes: usize) -> String {
   if bytes >= 1024 * 1024 {
      format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
   } else if bytes >= 1024 {
      format!("{:.1}KB", bytes as f64 / 1024.0)
   } else {
      format!("{bytes}B")
   }
}

fn format_llm_query_progress(
   spec: &OneShotSpec<'_>,
   mode: ResolvedApiMode,
) -> String {
   format!(
      "LLM query: {} \u{2192} {} ({}/{}, {}, {}, prompt ~{} tokens/{} chars)",
      oneshot_progress_label(spec),
      spec.model,
      spec.prompt_family,
      spec.prompt_variant,
      api_mode_label(mode),
      "tool call",
      format_count(estimate_prompt_text_tokens(spec)),
      format_count(prompt_text_chars(spec))
   )
}

fn format_llm_response_progress(
   spec: &OneShotSpec<'_>,
   status: reqwest::StatusCode,
   elapsed: Duration,
   body_bytes: usize,
) -> String {
   format!(
      "LLM response: {} \u{2190} {} (HTTP {}, {}, {})",
      oneshot_progress_label(spec),
      spec.model,
      status.as_u16(),
      format_elapsed(elapsed),
      format_bytes(body_bytes)
   )
}

fn format_llm_cache_progress(spec: &OneShotSpec<'_>) -> String {
   format!(
      "LLM cache hit: {} \u{2192} {} ({}/{})",
      oneshot_progress_label(spec),
      spec.model,
      spec.prompt_family,
      spec.prompt_variant
   )
}

enum OneShotRequestOutcome {
   Response { request_json: String, response_text: String },
   Retry,
}

enum OneShotParseOutcome<T> {
   Success(OneShotResponse<T>),
   Retry,
   Fatal(CommitGenError),
}

fn save_oneshot_debug<T: Serialize>(
   debug: Option<OneShotDebug<'_>>,
   phase: &str,
   value: &T,
) -> Result<()> {
   let Some(debug) = debug else {
      return Ok(());
   };

   let filename = debug_filename(
      debug.prefix,
      &format!("{}_{}.json", debug.name, phase),
   );
   let json = serde_json::to_string_pretty(value)?;
   save_debug_output(debug.dir, &filename, &json)
}

fn save_oneshot_debug_text(
   debug: Option<OneShotDebug<'_>>,
   phase: &str,
   text: &str,
) -> Result<()> {
   let Some(debug) = debug else {
      return Ok(());
   };

   let filename = debug_filename(
      debug.prefix,
      &format!("{}_{}.json", debug.name, phase),
   );
   save_debug_output(debug.dir, &filename, text)
}

fn schema_properties(schema: &serde_json::Value) -> Result<serde_json::Value> {
   schema
      .get("properties")
      .cloned()
      .ok_or_else(|| CommitGenError::Other("Schema must include top-level properties".to_string()))
}

fn schema_required(schema: &serde_json::Value) -> Result<Vec<String>> {
   schema
      .get("required")
      .and_then(|value| value.as_array())
      .ok_or_else(|| {
         CommitGenError::Other("Schema must include top-level required array".to_string())
      })
      .and_then(|values| {
         values
            .iter()
            .map(|value| {
               value.as_str().map(str::to_string).ok_or_else(|| {
                  CommitGenError::Other("Schema required entries must be strings".to_string())
               })
            })
            .collect()
      })
}

fn build_openai_tool(
   tool_name: &str,
   tool_description: &str,
   schema: &serde_json::Value,
) -> Result<Tool> {
   Ok(Tool {
      tool_type: "function".to_string(),
      function:  Function {
         name:        tool_name.to_string(),
         description: tool_description.to_string(),
         parameters:  FunctionParameters {
            param_type: "object".to_string(),
            properties: schema_properties(schema)?,
            required:   schema_required(schema)?,
         },
      },
   })
}

fn build_anthropic_tool(
   tool_name: &str,
   tool_description: &str,
   schema: &serde_json::Value,
   prompt_caching: bool,
) -> AnthropicTool {
   let mut tool = AnthropicTool {
      name:          tool_name.to_string(),
      description:   tool_description.to_string(),
      input_schema:  schema.clone(),
      cache_control: None,
   };

   if prompt_caching {
      tool.cache_control = Some(prompt_cache_control());
   }

   tool
}

fn is_context_length_error(body: &str) -> bool {
   let lower = body.to_lowercase();
   [
      "context_length_exceeded",
      "context window",
      "maximum context length",
      "exceeds the context",
      "input exceeds",
      "prompt is too long",
      "too many tokens",
   ]
   .iter()
   .any(|needle| lower.contains(needle))
}

async fn send_oneshot_request(
   config: &CommitConfig,
   spec: &OneShotSpec<'_>,
   mode: ResolvedApiMode,
   capture_request: bool,
) -> Result<OneShotRequestOutcome> {
   print_llm_progress(|| format_llm_query_progress(spec, mode));
   match mode {
      ResolvedApiMode::ChatCompletions => {
         let prompt_cache_key = openai_prompt_cache_key(
            config,
            spec.model,
            spec.prompt_family,
            spec.prompt_variant,
            spec.system_prompt,
         );
         let mut messages = Vec::new();
         if !spec.system_prompt.trim().is_empty() {
            messages.push(Message {
               role:    "system".to_string(),
               content: spec.system_prompt.to_string(),
            });
         }
         messages
            .push(Message { role: "user".to_string(), content: spec.user_prompt.to_string() });

         // In markdown mode, omit the tool entirely so the model emits plain
         // markdown text (parsed by the markdown fallback) instead of a tool call.
         let (tools, tool_choice) = if config.markdown_output {
            (vec![], None)
         } else {
            let tool = build_openai_tool(spec.tool_name, spec.tool_description, spec.schema)?;
            (vec![tool], Some(serde_json::json!("required")))
         };

         let request = ApiRequest {
            model: spec.model.to_string(),
            tools,
            tool_choice,
            prompt_cache_key,
            messages,
         };

         save_oneshot_debug(spec.debug, "request", &request)?;

         let client = get_client(config);
         let mut request_builder = client
            .post(format!("{}/chat/completions", config.api_base_url))
            .header("content-type", "application/json");

         if let Some(api_key) = &config.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
         }

         let request_json = if capture_request {
            serde_json::to_string(&request).unwrap_or_default()
         } else {
            String::new()
         };
         let request_start = std::time::Instant::now();
         let (status, response_text) =
            timed_send(request_builder.json(&request), spec.operation, spec.model).await?;
         print_llm_progress(|| {
            format_llm_response_progress(spec, status, request_start.elapsed(), response_text.len())
         });
         save_oneshot_debug_text(spec.debug, "response", &response_text)?;
         if !status.is_success() && is_context_length_error(&response_text) {
            return Err(CommitGenError::ApiContextLengthExceeded {
               operation: spec.operation.to_string(),
               model:     spec.model.to_string(),
               status:    status.as_u16(),
               body:      response_text,
            });
         }

         if status.is_server_error() {
            eprintln!(
               "{}",
               crate::style::error(&format!("Server error {status}: {response_text}"))
            );
            return Ok(OneShotRequestOutcome::Retry);
         }

         if !status.is_success() {
            return Err(CommitGenError::ApiError {
               status: status.as_u16(),
               body:   response_text,
            });
         }

         if response_text.trim().is_empty() {
            crate::style::warn(&format!(
               "Model returned empty response body for {}; retrying.",
               spec.operation
            ));
            return Ok(OneShotRequestOutcome::Retry);
         }

         Ok(OneShotRequestOutcome::Response { request_json, response_text })
      },
      ResolvedApiMode::AnthropicMessages => {
         let prompt_caching = anthropic_prompt_caching_enabled(config);
         // In markdown mode, omit the tool so the model emits plain markdown text.
         let (tools, tool_choice) = if config.markdown_output {
            (vec![], None)
         } else {
            (
               vec![build_anthropic_tool(
                  spec.tool_name,
                  spec.tool_description,
                  spec.schema,
                  prompt_caching,
               )],
               Some(AnthropicToolChoice {
                  choice_type: "tool".to_string(),
                  name:        spec.tool_name.to_string(),
               }),
            )
         };
         // The Anthropic Messages API requires max_tokens to be sent in the request.
         // The user requested sending 16384 for Anthropic calls.
         const ANTHROPIC_REQUIRED_MAX_TOKENS: u32 = 16384;
         let request = AnthropicRequest {
            model: spec.model.to_string(),
            max_tokens: ANTHROPIC_REQUIRED_MAX_TOKENS,
            system: anthropic_system_content(spec.system_prompt, prompt_caching),
            tools,
            tool_choice,
            messages: vec![AnthropicMessage {
               role:    "user".to_string(),
               content: vec![anthropic_text_content(spec.user_prompt.to_string(), false)],
            }],
         };

         save_oneshot_debug(spec.debug, "request", &request)?;

         let client = get_client(config);
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

         let request_json = if capture_request {
            serde_json::to_string(&request).unwrap_or_default()
         } else {
            String::new()
         };
         let request_start = std::time::Instant::now();
         let (status, response_text) =
            timed_send(request_builder.json(&request), spec.operation, spec.model).await?;
         print_llm_progress(|| {
            format_llm_response_progress(spec, status, request_start.elapsed(), response_text.len())
         });
         save_oneshot_debug_text(spec.debug, "response", &response_text)?;
         if !status.is_success() && is_context_length_error(&response_text) {
            return Err(CommitGenError::ApiContextLengthExceeded {
               operation: spec.operation.to_string(),
               model:     spec.model.to_string(),
               status:    status.as_u16(),
               body:      response_text,
            });
         }

         if status.is_server_error() {
            eprintln!(
               "{}",
               crate::style::error(&format!("Server error {status}: {response_text}"))
            );
            return Ok(OneShotRequestOutcome::Retry);
         }

         if !status.is_success() {
            return Err(CommitGenError::ApiError {
               status: status.as_u16(),
               body:   response_text,
            });
         }

         if response_text.trim().is_empty() {
            crate::style::warn(&format!(
               "Model returned empty response body for {}; retrying.",
               spec.operation
            ));
            return Ok(OneShotRequestOutcome::Retry);
         }

         Ok(OneShotRequestOutcome::Response { request_json, response_text })
      },
   }
}

fn parse_json_output<T: DeserializeOwned>(json_text: &str, error_label: &str) -> Result<T> {
   let candidate = extract_json_from_content(json_text);
   serde_json::from_str(&candidate).map_err(|e| {
      CommitGenError::Other(format!(
         "Failed to parse {error_label}: {e}. Content: {}",
         response_snippet(&candidate, 500)
      ))
   })
}

fn normalize_plain_text_content(content: &str) -> String {
   let trimmed = content.trim();

   if let Some(start) = trimmed.find("```") {
      let after_marker = &trimmed[start + 3..];
      let content_start = after_marker.find('\n').map_or(0, |i| i + 1);
      let after_newline = &after_marker[content_start..];
      if let Some(end) = after_newline.find("```") {
         return after_newline[..end].trim().to_string();
      }
   }

   trimmed.to_string()
}

fn parse_plain_text_output<T: DeserializeOwned>(
   tool_name: &str,
   content: &str,
   markdown_mode: bool,
) -> Result<Option<T>> {
   let trimmed = normalize_plain_text_content(content);
   if trimmed.is_empty() {
      return Ok(None);
   }

   let value = if markdown_mode {
      // Try markdown parsing for all tool names in markdown mode
      match tool_name {
         "create_conventional_analysis" => {
            crate::markdown_output::parse_conventional_analysis(&trimmed)
         },
         "create_commit_summary" => crate::markdown_output::parse_summary_output(&trimmed),
         "create_changelog_entries" => crate::markdown_output::parse_changelog_response(&trimmed),
         "create_compose_intent_plan" => crate::markdown_output::parse_compose_intent(&trimmed),
         "bind_compose_hunks" => crate::markdown_output::parse_compose_binding(&trimmed),
         "create_fast_commit" => crate::markdown_output::parse_fast_commit(&trimmed),
         "create_file_observations" => crate::markdown_output::parse_batch_observations(&trimmed),
         _ => return Ok(None),
      }?
   } else {
      // Original JSON-wrapping behavior for backward compat
      match tool_name {
         "create_commit_summary" => serde_json::json!({ "summary": trimmed }),
         _ => return Ok(None),
      }
   };

   serde_json::from_value(value).map(Some).map_err(|e| {
      CommitGenError::Other(format!(
         "Failed to parse {tool_name} plain-text fallback: {e}. Content: {}",
         response_snippet(&trimmed, 500)
      ))
   })
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

fn parse_oneshot_response<T: DeserializeOwned>(
   mode: ResolvedApiMode,
   tool_name: &str,
   operation: &str,
   response_text: &str,
   markdown_mode: bool,
) -> OneShotParseOutcome<T> {
   match mode {
      ResolvedApiMode::ChatCompletions => {
         let api_response: ApiResponse = match serde_json::from_str(response_text) {
            Ok(response) => response,
            Err(e) => {
               return OneShotParseOutcome::Fatal(CommitGenError::Other(format!(
                  "Failed to parse {operation} response JSON: {e}. Response body: {}",
                  response_snippet(response_text, 500)
               )));
            },
         };

         if api_response.choices.is_empty() {
            return OneShotParseOutcome::Fatal(CommitGenError::Other(format!(
               "API returned empty response for {operation}"
            )));
         }

         let message = &api_response.choices[0].message;
         if let Some(refusal) = &message.refusal {
            return OneShotParseOutcome::Fatal(CommitGenError::Other(format!(
               "Model refused {operation}: {refusal}"
            )));
         }

         let mut last_error: Option<CommitGenError> = None;

         if let Some(tool_call) = message.tool_calls.first()
            && tool_call.function.name.ends_with(tool_name)
         {
            let args = tool_call.function.arguments.trim();
            if args.is_empty() {
               last_error = Some(CommitGenError::Other(format!(
                  "Model returned empty function arguments for {operation}"
               )));
            } else {
               match serde_json::from_str::<T>(args) {
                  Ok(output) => {
                     return OneShotParseOutcome::Success(OneShotResponse {
                        output,
                        source: OneShotSource::ToolCall,
                        text_content: message.content.clone(),
                        stop_reason: None,
                     });
                  },
                  Err(e) => {
                     last_error = Some(CommitGenError::Other(format!(
                        "Failed to parse {operation} tool arguments: {e}. Args: {}",
                        response_snippet(args, 500)
                     )));
                  },
               }
            }
         }

         if let Some(content) = &message.content {
            if content.trim().is_empty() {
               return OneShotParseOutcome::Retry;
            }

            match parse_json_output::<T>(content, &format!("{operation} content JSON")) {
               Ok(output) => {
                  return OneShotParseOutcome::Success(OneShotResponse {
                     output,
                     source: OneShotSource::OutputJsonParse,
                     text_content: Some(content.clone()),
                     stop_reason: None,
                  });
               },
               Err(err) => match parse_plain_text_output::<T>(tool_name, content, markdown_mode) {
                  Ok(Some(output)) => {
                     return OneShotParseOutcome::Success(OneShotResponse {
                        output,
                        source: OneShotSource::PlainTextContent,
                        text_content: Some(content.clone()),
                        stop_reason: None,
                     });
                  },
                  Ok(None) => last_error = Some(err),
                  Err(fallback_err) => last_error = Some(fallback_err),
               },
            }
         }

         OneShotParseOutcome::Fatal(last_error.unwrap_or_else(|| {
            CommitGenError::Other(format!("No {operation} found in API response"))
         }))
      },
      ResolvedApiMode::AnthropicMessages => {
         let (tool_input, text_content, stop_reason) =
            match extract_anthropic_content(response_text, tool_name) {
               Ok(content) => content,
               Err(err) => return OneShotParseOutcome::Fatal(err),
            };

         let mut last_error: Option<CommitGenError> = None;

         if let Some(input) = tool_input {
            match serde_json::from_value::<T>(input) {
               Ok(output) => {
                  return OneShotParseOutcome::Success(OneShotResponse {
                     output,
                     source: OneShotSource::ToolCall,
                     text_content: (!text_content.is_empty()).then_some(text_content),
                     stop_reason,
                  });
               },
               Err(e) => {
                  last_error = Some(CommitGenError::Other(format!(
                     "Failed to parse {operation} tool input: {e}. Response body: {}",
                     response_snippet(response_text, 500)
                  )));
               },
            }
         }

         if text_content.trim().is_empty() {
            return OneShotParseOutcome::Retry;
         }

         match parse_json_output::<T>(&text_content, &format!("{operation} content JSON")) {
            Ok(output) => OneShotParseOutcome::Success(OneShotResponse {
               output,
               source: OneShotSource::OutputJsonParse,
               text_content: Some(text_content),
               stop_reason,
            }),
            Err(err) => match parse_plain_text_output::<T>(tool_name, &text_content, markdown_mode) {
               Ok(Some(output)) => OneShotParseOutcome::Success(OneShotResponse {
                  output,
                  source: OneShotSource::PlainTextContent,
                  text_content: Some(text_content),
                  stop_reason,
               }),
               Ok(None) => OneShotParseOutcome::Fatal(last_error.unwrap_or(err)),
               Err(fallback_err) => OneShotParseOutcome::Fatal(last_error.unwrap_or(fallback_err)),
            },
         }
      },
   }
}

#[tracing::instrument(target = "lgit", name = "api.run_oneshot", skip_all, fields(operation = spec.operation, model = spec.model, prompt_family = spec.prompt_family, prompt_variant = spec.prompt_variant))]
pub async fn run_oneshot<T>(
   config: &CommitConfig,
   spec: &OneShotSpec<'_>,
) -> Result<OneShotResponse<T>>
where
   T: DeserializeOwned + Serialize,
{
   let cache_entry = build_cache_entry(config, spec);
   if let Some((cache, key)) = cache_entry.as_ref()
      && let Some(stored) = cache.get(key)
      && let Ok(output) = serde_json::from_str::<T>(&stored)
   {
      print_llm_progress(|| format_llm_cache_progress(spec));
      return Ok(OneShotResponse {
         output,
         source: OneShotSource::Cache,
         text_content: None,
         stop_reason: None,
      });
   }
   // On parse failure (stale schema / wrong T) we silently fall through and
   // re-fetch — the next successful response will overwrite the stale entry.

   let capture_request = cache_entry.is_some();
   let (response, request_json): (OneShotResponse<T>, Option<String>) =
      retry_api_call(config, async move || {
         let mode = config.resolved_api_mode(spec.model);

         let (request_json, response_text) = match send_oneshot_request(
            config,
            spec,
            mode,
            capture_request,
         )
         .await?
         {
            OneShotRequestOutcome::Response { request_json, response_text } => {
               (request_json, response_text)
            },
            OneShotRequestOutcome::Retry => return Ok((true, None)),
         };

         match parse_oneshot_response::<T>(
            mode,
            spec.tool_name,
            spec.operation,
            &response_text,
            config.markdown_output,
         ) {
            OneShotParseOutcome::Success(output) => Ok((false, Some((output, Some(request_json))))),
            OneShotParseOutcome::Retry => Ok((true, None)),
            OneShotParseOutcome::Fatal(err) => Err(err),
         }
      })
      .await?;

   if let Some((cache, key)) = cache_entry.as_ref()
      && let Ok(payload) = serde_json::to_string(&response.output)
   {
      cache.put(key, spec.model, spec.operation, request_json.as_deref().unwrap_or(""), &payload);
   }

   Ok(response)
}

fn build_cache_entry(
   config: &CommitConfig,
   spec: &OneShotSpec<'_>,
) -> Option<(std::sync::Arc<crate::llm_cache::LlmCache>, String)> {
   if !spec.cacheable {
      return None;
   }
   let cache = crate::llm_cache::global()?;
   let mode = config.resolved_api_mode(spec.model);
   let api_mode = match mode {
      ResolvedApiMode::ChatCompletions => "chat-completions",
      ResolvedApiMode::AnthropicMessages => "anthropic-messages",
   };
   let key = crate::llm_cache::compute_key(&crate::llm_cache::CacheMaterial {
      operation: spec.operation,
      model: spec.model,
      tool_name: spec.tool_name,
      tool_description: spec.tool_description,
      system_prompt: spec.system_prompt,
      user_prompt: spec.user_prompt,
      schema: spec.schema,
      api_mode,
   });
   Some((cache, key))
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
   #[serde(skip_serializing_if = "Vec::is_empty")]
   tools:            Vec<Tool>,
   #[serde(skip_serializing_if = "Option::is_none")]
   tool_choice:      Option<serde_json::Value>,
   #[serde(skip_serializing_if = "Option::is_none")]
   prompt_cache_key: Option<String>,
   messages:         Vec<Message>,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
   model:         String,
   max_tokens:    u32,
   #[serde(skip_serializing_if = "Option::is_none")]
   system:        Option<Vec<AnthropicContent>>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   tools:         Vec<AnthropicTool>,
   #[serde(skip_serializing_if = "Option::is_none")]
   tool_choice:   Option<AnthropicToolChoice>,
   messages:      Vec<AnthropicMessage>,
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
   #[serde(default)]
   refusal:    Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
   choices: Vec<Choice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryOutput {
   summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FastCommitOutput {
   #[serde(rename = "type")]
   commit_type: String,
   #[serde(default)]
   scope:       Option<String>,
   summary:     String,
   #[serde(default)]
   details:     Vec<String>,
}

const fn should_retry_error(error: &CommitGenError) -> bool {
   !matches!(error, CommitGenError::ApiContextLengthExceeded { .. })
}
/// Retry an API call with exponential backoff
#[tracing::instrument(target = "lgit", name = "api.retry", skip_all, fields(max_retries = config.max_retries))]
pub async fn retry_api_call<T>(
   config: &CommitConfig,
   mut f: impl AsyncFnMut() -> Result<(bool, Option<T>)>,
) -> Result<T> {
   let mut attempt = 0;

   loop {
      attempt += 1;
      if crate::profile::enabled() {
         tracing::info!(
            target: crate::profile::TARGET,
            event = "api_retry_attempt_started",
            attempt,
            max_retries = config.max_retries,
         );
      }

      match f().await {
         Ok((false, Some(result))) => return Ok(result),
         Ok((false, None)) => {
            return Err(CommitGenError::Other("API call failed without result".to_string()));
         },
         Ok((true, _)) if attempt < config.max_retries => {
            let backoff_ms = config.initial_backoff_ms * (1 << (attempt - 1));
            if crate::profile::enabled() {
               tracing::warn!(
                  target: crate::profile::TARGET,
                  event = "api_retry_scheduled",
                  attempt,
                  max_retries = config.max_retries,
                  backoff_ms,
                  reason = "retryable_response",
               );
            }
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
            if !should_retry_error(&e) {
               return Err(e);
            }

            if attempt < config.max_retries {
               let backoff_ms = config.initial_backoff_ms * (1 << (attempt - 1));
               if crate::profile::enabled() {
                  tracing::warn!(
                     target: crate::profile::TARGET,
                     event = "api_retry_scheduled",
                     attempt,
                     max_retries = config.max_retries,
                     backoff_ms,
                     reason = "error",
                     error = %e,
                  );
               }
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
#[tracing::instrument(target = "lgit", name = "api.generate_conventional_analysis", skip_all, fields(model = model_name, diff_bytes = diff.len(), stat_bytes = stat.len()))]
pub async fn generate_conventional_analysis<'a>(
   stat: &'a str,
   diff: &'a str,
   model_name: &'a str,
   scope_candidates_str: &'a str,
   ctx: &AnalysisContext<'a>,
   config: &'a CommitConfig,
) -> Result<ConventionalAnalysis> {
   let type_enum: Vec<&str> = config.types.keys().map(|s| s.as_str()).collect();

   let analysis_schema = strict_json_schema(
      serde_json::json!({
         "type": {
            "type": "string",
            "enum": type_enum,
            "description": "Commit type based on change classification"
         },
         "scope": {
            "type": "string",
            "description": "Optional scope (module/component). Omit if unclear or multi-component."
         },
         "summary": {
            "type": "string",
            "description": format!(
               "Umbrella commit summary without type/scope prefix or trailing period; target {} chars, hard limit {}.",
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
            "items": { "type": "string" }
         }
      }),
      &["type", "summary", "details", "issue_refs"],
   );

   let prompt_variant = if config.markdown_output {
      "markdown"
   } else {
      &config.analysis_prompt_variant
   };

   let types_desc = format_types_description(config);
   let parts = templates::render_analysis_prompt(&templates::AnalysisParams {
      variant: prompt_variant,
      stat,
      diff,
      scope_candidates: scope_candidates_str,
      recent_commits: ctx.recent_commits,
      common_scopes: ctx.common_scopes,
      types_description: Some(&types_desc),
      project_context: ctx.project_context,
   })?;

   let user_prompt = if let Some(user_ctx) = ctx.user_context {
      format!("ADDITIONAL CONTEXT FROM USER:\n{user_ctx}\n\n{}", parts.user)
   } else {
      parts.user
   };

   let response = run_oneshot::<ConventionalAnalysis>(config, &OneShotSpec {
      operation:        "analysis",
      model:            model_name,
      prompt_family:    "analysis",
      prompt_variant,
      system_prompt:    &parts.system,
      user_prompt:      &user_prompt,
      tool_name:        "create_conventional_analysis",
      tool_description: "Analyze changes and classify as conventional commit with type, scope, \
                         summary, details, and metadata",
      schema:           &analysis_schema,
      progress_label:   Some("analysis"),
      debug:            Some(OneShotDebug {
         dir:    ctx.debug_output,
         prefix: ctx.debug_prefix,
         name:   "analysis",
      }),
      cacheable:        true,
   })
   .await?;

   Ok(response.output)
}

/// Strip conventional commit type prefix if LLM included it in summary.
///
/// Some models return the full format `feat(scope): summary` instead of just
/// `summary`. This function removes the prefix to normalize the response.
///
/// Tries the exact `type(scope): ` prefix first, then `type: ` (no scope),
/// then a generic `type(any-scope): ` pattern. All comparisons are
/// case-insensitive on the type token so `Fix(tui):` / `Feat: ` are stripped
/// too.
pub fn strip_type_prefix(summary: &str, commit_type: &str, scope: Option<&str>) -> String {
   let scope_part = scope.map(|s| format!("({s})")).unwrap_or_default();
   let prefix = format!("{commit_type}{scope_part}: ");

   if let Some(stripped) = summary.strip_prefix(&prefix) {
      return stripped.to_string();
   }

   // Try without scope in case model omitted it
   let prefix_no_scope = format!("{commit_type}: ");
   if let Some(stripped) = summary.strip_prefix(&prefix_no_scope) {
      return stripped.to_string();
   }

   // Case-insensitive fallbacks: models sometimes emit `Fix(tui):` or
   // `Feat: `. Check if the summary starts with the type token (ignoring
   // case) followed by `(` or `:`.
   let summary_lower = summary.to_ascii_lowercase();
   let commit_lower = commit_type.to_ascii_lowercase();

   // Generic `type(any-scope): ` — model emits a different scope than parsed.
   let generic_prefix = format!("{commit_lower}(");
   if let Some(after_type) = summary_lower.strip_prefix(&generic_prefix) {
      if let Some(close) = after_type.find("): ") {
         return summary[commit_type.len() + 1 + close + 3..].to_string();
      }
      if let Some(close) = after_type.find("):") {
         return summary[commit_type.len() + 1 + close + 2..].trim_start().to_string();
      }
   }

   // Case-insensitive `type: ` (no scope)
   let prefix_no_scope_lower = format!("{commit_lower}: ");
   if summary_lower.starts_with(&prefix_no_scope_lower) {
      return summary[commit_type.len() + 2..].to_string();
   }

   summary.to_string()
}

/// Build a commit summary from the holistic analysis response when present.
///
/// Returns `None` for map-reduce or legacy responses that do not include the
/// optional `summary` field.
pub fn summary_from_holistic_analysis(
   analysis: &ConventionalAnalysis,
   config: &CommitConfig,
) -> Result<Option<CommitSummary>> {
   let Some(raw_summary) = analysis
      .summary
      .as_deref()
      .map(str::trim)
      .filter(|summary| !summary.is_empty())
   else {
      return Ok(None);
   };

   let cleaned = strip_type_prefix(
      raw_summary,
      analysis.commit_type.as_str(),
      analysis.scope.as_ref().map(|scope| scope.as_str()),
   );

   CommitSummary::new(cleaned, config.summary_hard_limit).map(Some)
}

/// Validate summary against requirements
fn validate_summary_quality(
   summary: &str,
   commit_type: &str,
   stat: &str,
) -> std::result::Result<(), String> {
   use crate::validation::is_past_tense_first_word;

   let first_word = summary
      .split_whitespace()
      .next()
      .ok_or_else(|| "summary is empty".to_string())?;

   let first_word_lower = first_word.to_lowercase();

   // Check past-tense verb (tolerates trailing non-alpha suffixes like
   // `bound-check`, and all-caps acronyms are rejected).
   if !is_past_tense_first_word(first_word) {
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
#[tracing::instrument(target = "lgit", name = "api.generate_summary_from_analysis", skip_all, fields(commit_type, scope = ?scope, detail_count = details.len(), model = %config.summary_model))]
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

      let bullet_points = details.join("\n");
      let details_str = if bullet_points.is_empty() {
         "None (no supporting detail points were generated)."
      } else {
         bullet_points.as_str()
      };

      let scope_str = scope.unwrap_or("");
      let prefix_len =
         commit_type.len() + 2 + scope_str.len() + if scope_str.is_empty() { 0 } else { 2 };
      let max_summary_len = config.summary_guideline.saturating_sub(prefix_len);

      let summary_variant = if config.markdown_output { "markdown" } else { &config.summary_prompt_variant };

      let parts = templates::render_summary_prompt(
         summary_variant,
         commit_type,
         scope_str,
         &max_summary_len.to_string(),
         details_str,
         stat.trim(),
         user_context,
      )?;

      let user_prompt = format!("{}{additional_constraint}", parts.user);
      let summary_schema = strict_json_schema(
         serde_json::json!({
            "summary": {
               "type": "string",
               "description": format!(
                  "Single line summary, target {} chars (hard limit {}), past tense verb first.",
                  config.summary_guideline,
                  config.summary_hard_limit
               ),
               "maxLength": config.summary_hard_limit
            }
         }),
         &["summary"],
      );

      let response = run_oneshot::<SummaryOutput>(config, &OneShotSpec {
         operation:        "summary",
         model:            &config.summary_model,
         prompt_family:    "summary",
         prompt_variant:   summary_variant,
         system_prompt:    &parts.system,
         user_prompt:      &user_prompt,
         tool_name:        "create_commit_summary",
         tool_description: "Compose a git commit summary line from detail statements",
         schema:           &summary_schema,
         progress_label:   Some("summary"),
         debug:            Some(OneShotDebug {
            dir:    debug_dir,
            prefix: debug_prefix,
            name:   "summary",
         }),
         cacheable:        true,
      })
      .await;

      match response {
         Ok(response) => {
            let cleaned = strip_type_prefix(&response.output.summary, commit_type, scope);
            // Normalize present->past tense before validation so Gemini
            // summaries like "harden ..." get converted to "hardened ...".
            let mut normalized = cleaned;
            crate::normalization::normalize_summary_verb(&mut normalized, commit_type);
            let summary = CommitSummary::new(&normalized, config.summary_hard_limit)?;

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
               },
               Err(reason) => {
                  crate::style::warn(&format!(
                     "Validation failed after {} retries: {}. Using fallback.",
                     max_validation_retries + 1,
                     reason
                  ));
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
      .is_some_and(crate::validation::is_past_tense_first_word)
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
#[tracing::instrument(target = "lgit", name = "api.generate_analysis_with_map_reduce", skip_all, fields(model = model_name, diff_bytes = diff.len(), stat_bytes = stat.len()))]
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

/// Generate a complete commit in a single API call (fast mode).
///
/// Returns a `ConventionalCommit` directly — no separate summary phase.
#[tracing::instrument(target = "lgit", name = "api.generate_fast_commit", skip_all, fields(model = model_name, diff_bytes = diff.len(), stat_bytes = stat.len()))]
pub async fn generate_fast_commit(
   stat: &str,
   diff: &str,
   model_name: &str,
   scope_candidates_str: &str,
   user_context: Option<&str>,
   config: &CommitConfig,
   debug_dir: Option<&Path>,
) -> Result<ConventionalCommit> {
   let type_enum: Vec<&str> = config.types.keys().map(|s| s.as_str()).collect();
   let types_desc = format_types_description(config);

   let fast_variant = if config.markdown_output { "markdown" } else { "default" };
   let parts = templates::render_fast_prompt(&templates::FastPromptParams {
      variant: fast_variant,
      stat,
      diff,
      scope_candidates: scope_candidates_str,
      user_context,
      types_description: Some(&types_desc),
   })?;

   let fast_schema = strict_json_schema(
      serde_json::json!({
         "type": {
            "type": "string",
            "enum": type_enum,
            "description": "Conventional commit type"
         },
         "scope": {
            "type": "string",
            "description": "Optional scope. Omit if unclear or cross-cutting."
         },
         "summary": {
            "type": "string",
            "description": "≤72 char past-tense summary, no type prefix, no trailing period"
         },
         "details": {
            "type": "array",
            "items": { "type": "string" },
            "description": "0-3 past-tense detail sentences ending with period"
         }
      }),
      &["type", "summary", "details"],
   );

   let response = run_oneshot::<FastCommitOutput>(config, &OneShotSpec {
      operation:        "fast",
      model:            model_name,
      prompt_family:    "fast",
      prompt_variant:   fast_variant,
      system_prompt:    &parts.system,
      user_prompt:      &parts.user,
      tool_name:        "create_fast_commit",
      tool_description: "Generate a conventional commit from the given diff",
      schema:           &fast_schema,
      progress_label:   Some("fast commit"),
      debug:            Some(OneShotDebug { dir: debug_dir, prefix: None, name: "fast" }),
      cacheable:        true,
   })
   .await?;

   build_fast_commit(response.output, config)
}

/// Convert a `FastCommitOutput` into a validated `ConventionalCommit`.
fn build_fast_commit(
   output: FastCommitOutput,
   config: &CommitConfig,
) -> Result<ConventionalCommit> {
   let commit_type = CommitType::new(&output.commit_type)?;
   let scope = coerce_optional_scope(output.scope.as_deref());
   let cleaned_summary = strip_type_prefix(
      &output.summary,
      commit_type.as_str(),
      scope.as_ref().map(|s| s.as_str()),
   );
   let summary = CommitSummary::new(&cleaned_summary, config.summary_hard_limit)?;
   Ok(ConventionalCommit { commit_type, scope, summary, body: output.details, footers: vec![] })
}
#[cfg(test)]
mod tests {
   use super::*;
   use crate::config::CommitConfig;

   #[test]
   fn test_strip_type_prefix_exact_scope() {
      assert_eq!(strip_type_prefix("fix(api): fixed bug", "fix", Some("api")), "fixed bug");
   }

   #[test]
   fn test_strip_type_prefix_no_scope() {
      assert_eq!(strip_type_prefix("fix: fixed bug", "fix", None), "fixed bug");
   }

   #[test]
   fn test_strip_type_prefix_different_scope() {
      // Model emits scope we didn't parse (e.g. scope is None but model wrote fix(tui):)
      assert_eq!(strip_type_prefix("fix(tui): fixed bug", "fix", None), "fixed bug");
      // Model emits different scope than parsed
      assert_eq!(strip_type_prefix("fix(tui): fixed bug", "fix", Some("api")), "fixed bug");
   }

   #[test]
   fn test_strip_type_prefix_no_prefix() {
      // No prefix present — should return unchanged
      assert_eq!(strip_type_prefix("fixed bug", "fix", None), "fixed bug");
   }

   #[test]
   fn test_strip_type_prefix_wrong_type_not_stripped() {
      // Should not strip a prefix with a different type
      assert_eq!(strip_type_prefix("feat(api): added feature", "fix", None), "feat(api): added feature");
   }

   #[test]
   fn test_strip_type_prefix_capitalized_type_with_scope() {
      // Model emits `Fix(tui):` with capital F
      assert_eq!(strip_type_prefix("Fix(tui): fixed bug", "fix", None), "fixed bug");
      assert_eq!(strip_type_prefix("Fix(tui): fixed bug", "fix", Some("api")), "fixed bug");
   }

   #[test]
   fn test_strip_type_prefix_capitalized_type_no_scope() {
      // Model emits `Feat: ` with capital F
      assert_eq!(strip_type_prefix("Feat: added feature", "feat", None), "added feature");
   }

   #[test]
   fn test_strip_type_prefix_uppercase_type() {
      // Model emits `FIX(api):` all caps
      assert_eq!(strip_type_prefix("FIX(api): fixed bug", "fix", Some("api")), "fixed bug");
   }

   #[test]
   fn test_strict_json_schema_disallows_extra_properties() {
      let schema =
         strict_json_schema(serde_json::json!({ "summary": { "type": "string" } }), &["summary"]);
      assert_eq!(schema["type"], "object");
      assert_eq!(schema["required"], serde_json::json!(["summary"]));
      assert_eq!(schema["additionalProperties"], serde_json::json!(false));
   }

   #[test]
   fn test_env_flag_value_enabled_uses_boolean_semantics() {
      assert!(!env_flag_value_enabled(None));
      assert!(!env_flag_value_enabled(Some("")));
      assert!(!env_flag_value_enabled(Some("0")));
      assert!(!env_flag_value_enabled(Some("false")));
      assert!(!env_flag_value_enabled(Some("NO")));
      assert!(!env_flag_value_enabled(Some("off")));
      assert!(env_flag_value_enabled(Some("1")));
      assert!(env_flag_value_enabled(Some("true")));
      assert!(env_flag_value_enabled(Some("yes")));
      assert!(env_flag_value_enabled(Some("anything")));
   }
   #[test]
   fn test_request_serialization() {
      let api_req = ApiRequest {
         model: "test-model".to_string(),
         tools: vec![],
         tool_choice: None,
         prompt_cache_key: None,
         messages: vec![],
      };
      let api_json = serde_json::to_string(&api_req).unwrap();
      assert!(!api_json.contains("max_tokens"));
      assert!(!api_json.contains("temperature"));

      let anthropic_req = AnthropicRequest {
         model: "test-model".to_string(),
         max_tokens: 16384,
         system: None,
         tools: vec![],
         tool_choice: None,
         messages: vec![],
      };
      let anthropic_json = serde_json::to_string(&anthropic_req).unwrap();
      assert!(anthropic_json.contains("\"max_tokens\":16384"));
      assert!(!anthropic_json.contains("temperature"));
   }

   #[test]
   fn test_format_llm_progress_uses_operation_label_and_request_shape() {
      let schema =
         strict_json_schema(serde_json::json!({ "summary": { "type": "string" } }), &["summary"]);
      let spec = OneShotSpec {
         operation:        "map-reduce/map",
         model:            "claude-sonnet-4.5",
         prompt_family:    "map",
         prompt_variant:   "default",
         system_prompt:    "system",
         user_prompt:      "user",
         tool_name:        "create_file_observation",
         tool_description: "Extract observations",
         schema:           &schema,
         progress_label:   Some("map file 2/5 src/lib.rs"),
         debug:            None,
         cacheable:        false,
      };

      assert_eq!(
         format_llm_query_progress(&spec, ResolvedApiMode::ChatCompletions),
         "LLM query: map file 2/5 src/lib.rs \u{2192} claude-sonnet-4.5 (map/default, chat \
          completions, tool call, prompt ~3 tokens/10 chars)"
      );
      assert_eq!(
         format_llm_response_progress(
            &spec,
            reqwest::StatusCode::OK,
            std::time::Duration::from_millis(1234),
            2048,
         ),
         "LLM response: map file 2/5 src/lib.rs \u{2190} claude-sonnet-4.5 (HTTP 200, 1.2s, 2.0KB)"
      );
      assert_eq!(
         format_llm_cache_progress(&spec),
         "LLM cache hit: map file 2/5 src/lib.rs \u{2192} claude-sonnet-4.5 (map/default)"
      );
   }

   #[test]
   fn test_context_length_error_detection() {
      assert!(is_context_length_error(
         r#"{"error":{"message":"Your input exceeds the context window of this model. (code=context_length_exceeded)"}}"#,
      ));
      assert!(is_context_length_error("This model's maximum context length is 128000 tokens.",));
      assert!(!is_context_length_error("upstream temporarily overloaded"));
   }

   #[tokio::test]
   async fn test_retry_api_call_does_not_retry_context_length_errors() {
      use std::sync::atomic::{AtomicUsize, Ordering};

      let config = CommitConfig { max_retries: 3, initial_backoff_ms: 1, ..Default::default() };
      let attempts = AtomicUsize::new(0);

      let result = retry_api_call::<()>(&config, async || {
         attempts.fetch_add(1, Ordering::SeqCst);
         Err::<(bool, Option<()>), CommitGenError>(CommitGenError::ApiContextLengthExceeded {
            operation: "analysis".to_string(),
            model:     "codex".to_string(),
            status:    502,
            body:      "context_length_exceeded".to_string(),
         })
      })
      .await;

      assert!(matches!(result, Err(CommitGenError::ApiContextLengthExceeded { .. })));
      assert_eq!(attempts.load(Ordering::SeqCst), 1);
   }

   #[tokio::test]
   async fn test_run_oneshot_returns_context_length_error() {
      let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
      let addr = listener.local_addr().unwrap();
      let server = std::thread::spawn(move || {
         use std::io::{Read, Write};

         let (mut stream, _) = listener.accept().unwrap();
         let mut request = [0_u8; 4096];
         let _ = stream.read(&mut request);
         let body = r#"{"error":{"message":"context_length_exceeded"}}"#;
         let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: \
             {}\r\n\r\n{}",
            body.len(),
            body
         );
         stream.write_all(response.as_bytes()).unwrap();
      });

      let model = "gpt-4o-mini-probe-clear-test";
      let config = CommitConfig {
         api_base_url: format!("http://{addr}"),
         max_retries: 3,
         initial_backoff_ms: 1,
         ..Default::default()
      };
      let schema =
         strict_json_schema(serde_json::json!({ "summary": { "type": "string" } }), &["summary"]);

      let result = run_oneshot::<SummaryOutput>(&config, &OneShotSpec {
         operation: "summary",
         model,
         prompt_family: "summary",
         prompt_variant: "default",
         system_prompt: "Summarize.",
         user_prompt: "A large diff.",
         tool_name: "create_commit_summary",
         tool_description: "Create a commit summary",
         schema: &schema,
         progress_label: Some("summary"),
         debug: None,
         cacheable: false,
      })
      .await;
      assert!(result.is_err());

      server.join().unwrap();
   }

   #[test]
   fn test_extract_json_from_content_code_block() {
      let content = r#"Here is the payload:

```json
{"summary":"added support"}
```
"#;
      assert_eq!(extract_json_from_content(content), r#"{"summary":"added support"}"#);
   }

   #[test]
   fn test_build_fast_commit_coerces_invalid_scope_output() {
      let commit = build_fast_commit(
         FastCommitOutput {
            commit_type: "chore".to_string(),
            scope:       Some(".".to_string()),
            summary:     "updated tooling".to_string(),
            details:     vec![],
         },
         &CommitConfig::default(),
      )
      .unwrap();

      assert!(commit.scope.is_none());
   }

   #[test]
   fn test_build_fast_commit_sanitizes_path_like_scope_output() {
      let commit = build_fast_commit(
         FastCommitOutput {
            commit_type: "chore".to_string(),
            scope:       Some(".github/Release Notes".to_string()),
            summary:     "updated tooling".to_string(),
            details:     vec![],
         },
         &CommitConfig::default(),
      )
      .unwrap();

      assert_eq!(
         commit.scope.as_ref().map(crate::types::Scope::as_str),
         Some("github/release-notes")
      );
   }

   #[test]
   fn test_parse_oneshot_response_prefers_tool_payload() {
      let response_text = serde_json::json!({
         "choices": [{
            "message": {
               "tool_calls": [{
                  "function": {
                     "name": "create_commit_summary",
                     "arguments": "{\"summary\":\"added feature\"}"
                  }
               }],
               "content": "{\"summary\":\"ignored\"}"
            }
         }]
      })
      .to_string();

      let result = parse_oneshot_response::<SummaryOutput>(
         ResolvedApiMode::ChatCompletions,
         "create_commit_summary",
         "summary",
         &response_text,
         false,
      );

      match result {
         OneShotParseOutcome::Success(response) => {
            assert_eq!(response.source, OneShotSource::ToolCall);
            assert_eq!(response.output.summary, "added feature");
         },
         OneShotParseOutcome::Retry => panic!("expected parsed tool payload"),
         OneShotParseOutcome::Fatal(err) => panic!("unexpected parse failure: {err}"),
      }
   }

   #[test]
   fn test_parse_oneshot_response_falls_back_to_content_json() {
      let response_text = serde_json::json!({
         "choices": [{
            "message": {
               "tool_calls": [{
                  "function": {
                     "name": "create_commit_summary",
                     "arguments": "{invalid json}"
                  }
               }],
               "content": "{\"summary\":\"added fallback\"}"
            }
         }]
      })
      .to_string();

      let result = parse_oneshot_response::<SummaryOutput>(
         ResolvedApiMode::ChatCompletions,
         "create_commit_summary",
         "summary",
         &response_text,
         false,
      );

      match result {
         OneShotParseOutcome::Success(response) => {
            assert_eq!(response.source, OneShotSource::OutputJsonParse);
            assert_eq!(response.output.summary, "added fallback");
         },
         OneShotParseOutcome::Retry => panic!("expected parsed content JSON"),
         OneShotParseOutcome::Fatal(err) => panic!("unexpected parse failure: {err}"),
      }
   }

   #[test]
   fn test_parse_oneshot_response_accepts_plain_text_summary_content() {
      let response_text = serde_json::json!({
         "choices": [{
            "message": {
               "content": "updated gemini-image tests for CustomToolContext and array headers"
            }
         }]
      })
      .to_string();

      let result = parse_oneshot_response::<SummaryOutput>(
         ResolvedApiMode::ChatCompletions,
         "create_commit_summary",
         "summary",
         &response_text,
         false,
      );

      match result {
         OneShotParseOutcome::Success(response) => {
            assert_eq!(response.source, OneShotSource::PlainTextContent);
            assert_eq!(
               response.output.summary,
               "updated gemini-image tests for CustomToolContext and array headers"
            );
         },
         OneShotParseOutcome::Retry => panic!("expected plain-text summary fallback"),
         OneShotParseOutcome::Fatal(err) => panic!("unexpected parse failure: {err}"),
      }
   }

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
      // "fix" is not past-tense, so fails on verb check
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
