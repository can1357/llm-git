#![allow(unused_assignments, reason = "miette::Diagnostic derive generates field assignments")]

use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

/// Top-level error type for the commit message generator.
///
/// Each variant carries context appropriate to the failure mode and, where
/// applicable, [`miette::Diagnostic`] metadata (help text, error codes) so
/// that the CLI renders human-friendly reports.
#[derive(Debug, Error, Diagnostic)]
pub enum CommitGenError {
   #[error("git: {message}")]
   #[diagnostic(code(lgit::git))]
   GitError { message: String },

   #[error("git index is locked")]
   #[diagnostic(
      code(lgit::git::index_locked),
      help("Another git process may be running in this repository.\nRemove the lock file to continue:\n  rm {}", lock_path.display()),
   )]
   GitIndexLocked { lock_path: PathBuf },

   #[error("API request failed (HTTP {status}): {body}")]
   #[diagnostic(code(lgit::api))]
   ApiError { status: u16, body: String },

   #[error("API call failed after {retries} retries")]
   #[diagnostic(
      code(lgit::api::retry_exhausted),
      help(
         "Check that your LiteLLM server is running and reachable.\nYou can increase max_retries \
          in ~/.config/llm-git/config.toml"
      )
   )]
   ApiRetryExhausted {
      retries: u32,
      #[source]
      source:  Box<Self>,
   },

   #[error("Validation failed: {0}")]
   #[diagnostic(code(lgit::validation))]
   ValidationError(String),

   #[error("No changes found in {mode} mode")]
   #[diagnostic(
      code(lgit::git::no_changes),
      help("Stage changes with `git add` or use --mode=unstaged to analyze working tree changes")
   )]
   NoChanges { mode: String },

   #[error("Diff parsing failed: {0}")]
   #[diagnostic(code(lgit::diff))]
   #[allow(dead_code, reason = "Reserved for future diff parsing error handling")]
   DiffParseError(String),

   #[error("Invalid commit type: {0}")]
   #[diagnostic(
      code(lgit::types::commit_type),
      help("Valid types: feat, fix, refactor, docs, test, chore, style, perf, build, ci, revert")
   )]
   InvalidCommitType(String),

   #[error("Invalid scope format: {0}")]
   #[diagnostic(
      code(lgit::types::scope),
      help("Scopes must be lowercase alphanumeric with at most 2 segments (e.g. api/client)")
   )]
   InvalidScope(String),

   #[error("Summary too long: {len} chars (max {max})")]
   #[diagnostic(code(lgit::validation::length))]
   SummaryTooLong { len: usize, max: usize },

   #[error("IO error: {0}")]
   #[diagnostic(code(lgit::io))]
   IoError(#[from] std::io::Error),

   #[error("JSON error: {0}")]
   #[diagnostic(code(lgit::json))]
   JsonError(#[from] serde_json::Error),

   #[error("HTTP error: {0}")]
   #[diagnostic(code(lgit::http))]
   HttpError(#[from] reqwest::Error),

   #[error("Clipboard error: {0}")]
   #[diagnostic(code(lgit::clipboard))]
   ClipboardError(#[from] arboard::Error),

   #[error("{0}")]
   Other(String),

   #[error("Failed to parse changelog {path}: {reason}")]
   #[diagnostic(code(lgit::changelog::parse))]
   ChangelogParseError { path: String, reason: String },

   #[error("No [Unreleased] section found in {path}")]
   #[diagnostic(
      code(lgit::changelog::no_unreleased),
      help("Add an ## [Unreleased] section to the changelog file")
   )]
   NoUnreleasedSection { path: String },
}

impl CommitGenError {
   /// Construct a [`GitError`](CommitGenError::GitError) from any displayable
   /// message.
   pub fn git(msg: impl Into<String>) -> Self {
      Self::GitError { message: msg.into() }
   }
}

pub type Result<T, E = CommitGenError> = std::result::Result<T, E>;
