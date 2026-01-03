//! Token counting with cascading fallback.
//!
//! Attempts token counting in order:
//! 1. API count_tokens (Anthropic-compatible endpoints)
//! 2. tiktoken (OpenAI/GPT models)
//! 3. Character estimate (4 chars ≈ 1 token)

use std::fmt;

use tiktoken_rs::{CoreBPE, get_bpe_from_model};

use crate::config::CommitConfig;

/// Create a TokenCounter from config values.
pub fn create_token_counter(config: &CommitConfig) -> TokenCounter {
   TokenCounter::new(
      &config.api_base_url,
      config.api_key.as_deref(),
      &config.analysis_model,
   )
}

/// Token counter with cascading fallback.
pub struct TokenCounter {
   client: reqwest::Client,
   api_base_url: String,
   api_key: Option<String>,
   model: String,
   tiktoken: Option<CoreBPE>,
}

impl fmt::Debug for TokenCounter {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_struct("TokenCounter")
         .field("model", &self.model)
         .field("has_tiktoken", &self.tiktoken.is_some())
         .finish_non_exhaustive()
   }
}

impl TokenCounter {
   /// Create a new token counter for the given API configuration.
   pub fn new(api_base_url: &str, api_key: Option<&str>, model: &str) -> Self {
      Self {
         client: reqwest::Client::new(),
         api_base_url: api_base_url.to_string(),
         api_key: api_key.map(String::from),
         model: model.to_string(),
         tiktoken: get_bpe_from_model(model).ok(),
      }
   }

   /// Count tokens for a text string.
   ///
   /// Tries API count_tokens first, then tiktoken, then 4-char estimate.
   pub async fn count(&self, text: &str) -> usize {
      // 1. Try API count_tokens (works with Anthropic, LiteLLM, and other proxies)
      if let Some(count) = self.try_api_count(text).await {
         return count;
      }
      // 2. Fall back to tiktoken or char estimate
      self.count_sync(text)
   }

   /// Synchronous token count (tiktoken or char estimate).
   pub fn count_sync(&self, text: &str) -> usize {
      if let Some(ref encoder) = self.tiktoken {
         encoder.encode_with_special_tokens(text).len()
      } else {
         text.len() / 4
      }
   }

   /// Try counting tokens via API (Anthropic-compatible count_tokens endpoint).
   /// Works with Anthropic directly, LiteLLM, and other proxies that implement the endpoint.
   async fn try_api_count(&self, text: &str) -> Option<usize> {
      let api_key = self.api_key.as_ref()?;

      // OpenAI doesn't have a count_tokens endpoint - use tiktoken instead
      if self.api_base_url.contains("openai.com") {
         return None;
      }

      // Try Anthropic-compatible count_tokens endpoint (works with proxies like LiteLLM)
      let resp = self
         .client
         .post(format!("{}/messages/count_tokens", self.api_base_url))
         .header("x-api-key", api_key)
         .header("anthropic-version", "2023-06-01")
         .header("content-type", "application/json")
         .json(&serde_json::json!({
             "model": self.model,
             "messages": [{"role": "user", "content": text}]
         }))
         .send()
         .await
         .ok()?;

      let body: serde_json::Value = resp.json().await.ok()?;
      body["input_tokens"].as_u64().map(|n| n as usize)
   }
}
