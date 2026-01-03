//! Git commit message generator library
//!
//! This library provides functionality for analyzing git diffs and generating
//! conventional commit messages using Claude AI via `LiteLLM`.
pub mod analysis;
pub mod api;
pub mod changelog;
pub mod compose;
pub mod config;
pub mod diff;
pub mod error;
pub mod git;
pub mod normalization;
pub mod patch;
pub mod style;
pub mod templates;
pub mod types;
pub mod validation;

// Re-export commonly used types
pub use config::CommitConfig;
pub use error::{CommitGenError, Result};
pub use types::{ConventionalCommit, Mode, resolve_model_name};

// Re-export rewrite module for main.rs
pub mod rewrite;
