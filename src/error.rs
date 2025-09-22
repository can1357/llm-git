use thiserror::Error;

#[derive(Debug, Error)]
pub enum CommitGenError {
   #[error("Git command failed: {0}")]
   GitError(String),

   #[error("API request failed (HTTP {status}): {body}")]
   ApiError { status: u16, body: String },

   #[error("API call failed after {retries} retries: {source}")]
   ApiRetryExhausted {
      retries: u32,
      #[source]
      source:  Box<Self>,
   },

   #[error("Validation failed: {0}")]
   ValidationError(String),

   #[error("No changes found in {mode} mode")]
   NoChanges { mode: String },

   #[error("Diff parsing failed: {0}")]
   #[allow(dead_code, reason = "Reserved for future diff parsing error handling")]
   DiffParseError(String),

   #[error("Invalid commit type: {0}")]
   InvalidCommitType(String),

   #[error("Invalid scope format: {0}")]
   InvalidScope(String),

   #[error("Summary too long: {len} chars (max {max})")]
   SummaryTooLong { len: usize, max: usize },

   #[error("IO error: {0}")]
   IoError(#[from] std::io::Error),

   #[error("JSON error: {0}")]
   JsonError(#[from] serde_json::Error),

   #[error("HTTP error: {0}")]
   HttpError(#[from] reqwest::Error),

   #[error("Clipboard error: {0}")]
   ClipboardError(#[from] arboard::Error),

   #[error("{0}")]
   Other(String),
}

pub type Result<T> = std::result::Result<T, CommitGenError>;
