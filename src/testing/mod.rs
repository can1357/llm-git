//! Testing infrastructure for llm-git
//!
//! Provides fixture-based golden file testing for commit message generation.
//!
//! # Directory Structure
//!
//! ```text
//! tests/fixtures/
//! ├── manifest.toml              # Fixture registry
//! ├── large-wasm-merge/
//! │   ├── meta.toml              # Fixture metadata
//! │   ├── input/
//! │   │   ├── diff.patch         # Frozen diff
//! │   │   ├── stat.txt           # Frozen stat
//! │   │   ├── scope_candidates.txt
//! │   │   └── context.toml       # Analysis context
//! │   └── golden/
//! │       ├── analysis.json      # Expected analysis
//! │       └── final.txt          # Expected commit message
//! └── ...
//! ```

mod compare;
pub mod fixture;
mod runner;

pub use compare::{CompareResult, compare_analysis};
pub use fixture::{
   Fixture, FixtureContext, FixtureEntry, FixtureInput, FixtureMeta, Golden, Manifest,
   discover_fixtures,
};
pub use runner::{RunResult, TestRunner, TestSummary};

use std::path::Path;

use crate::error::Result;

/// Default fixtures directory relative to crate root
pub const FIXTURES_DIR: &str = "tests/fixtures";

/// Get the fixtures directory path
pub fn fixtures_dir() -> std::path::PathBuf {
   // Try to find it relative to CARGO_MANIFEST_DIR or current dir
   if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
      return Path::new(&manifest_dir).join(FIXTURES_DIR);
   }

   // Fall back to current directory
   Path::new(FIXTURES_DIR).to_path_buf()
}

/// List all available fixtures
pub fn list_fixtures() -> Result<Vec<String>> {
   let manifest = Manifest::load(&fixtures_dir())?;
   Ok(manifest.fixtures.into_keys().collect())
}
