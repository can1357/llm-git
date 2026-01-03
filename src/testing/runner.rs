//! Test runner for fixture-based testing

use crate::{
   api::{AnalysisContext, generate_analysis_with_map_reduce},
   config::CommitConfig,
   error::Result,
   normalization::format_commit_message,
   tokens::create_token_counter,
   types::{CommitType, ConventionalAnalysis, ConventionalCommit},
};

use super::{
   compare::{CompareResult, compare_analysis},
   fixture::{Fixture, discover_fixtures},
};

/// Result of running a single fixture
#[derive(Debug)]
pub struct RunResult {
   /// Fixture name
   pub name: String,
   /// Comparison result (None if no golden exists)
   pub comparison: Option<CompareResult>,
   /// The actual analysis produced
   pub analysis: crate::types::ConventionalAnalysis,
   /// The actual commit message produced
   pub final_message: String,
   /// Error if any
   pub error: Option<String>,
}

/// Test runner configuration
pub struct TestRunner {
   /// Fixtures directory
   pub fixtures_dir: std::path::PathBuf,
   /// Config to use for analysis
   pub config: CommitConfig,
   /// Filter pattern for fixture names
   pub filter: Option<String>,
}

impl TestRunner {
   /// Create a new test runner
   pub fn new(fixtures_dir: impl Into<std::path::PathBuf>, config: CommitConfig) -> Self {
      Self {
         fixtures_dir: fixtures_dir.into(),
         config,
         filter: None,
      }
   }

   /// Set filter pattern
   pub fn with_filter(mut self, filter: Option<String>) -> Self {
      self.filter = filter;
      self
   }

   /// Run all fixtures and return results
   pub fn run_all(&self) -> Result<Vec<RunResult>> {
      let fixture_names = discover_fixtures(&self.fixtures_dir)?;
      let mut results = Vec::new();

      for name in fixture_names {
         // Apply filter if set
         if let Some(ref pattern) = self.filter {
            if !name.contains(pattern) {
               continue;
            }
         }

         let result = self.run_fixture(&name);
         results.push(result);
      }

      Ok(results)
   }

   /// Run a single fixture
   pub fn run_fixture(&self, name: &str) -> RunResult {
      match self.run_fixture_inner(name) {
         Ok(result) => result,
         Err(e) => RunResult {
            name:          name.to_string(),
            comparison:    None,
            analysis:      ConventionalAnalysis {
               commit_type: CommitType::new("chore").expect("valid type"),
               scope:       None,
               details:     vec![],
               issue_refs:  vec![],
            },
            final_message: String::new(),
            error:         Some(e.to_string()),
         },
      }
   }

   fn run_fixture_inner(&self, name: &str) -> Result<RunResult> {
      let fixture = Fixture::load(&self.fixtures_dir, name)?;
      let token_counter = create_token_counter(&self.config);

      // Build analysis context from fixture
      let ctx = AnalysisContext {
         user_context:    fixture.input.context.user_context.as_deref(),
         recent_commits:  fixture.input.context.recent_commits.as_deref(),
         common_scopes:   fixture.input.context.common_scopes.as_deref(),
         project_context: fixture.input.context.project_context.as_deref(),
      };

      // Run analysis
      let analysis = generate_analysis_with_map_reduce(
         &fixture.input.stat,
         &fixture.input.diff,
         &self.config.analysis_model,
         &fixture.input.scope_candidates,
         &ctx,
         &self.config,
         &token_counter,
      )?;

      // Get summary
      let detail_points = analysis.body_texts();
      let summary = crate::api::generate_summary_from_analysis(
         &fixture.input.stat,
         analysis.commit_type.as_str(),
         analysis.scope.as_ref().map(|s| s.as_str()),
         &detail_points,
         fixture.input.context.user_context.as_deref(),
         &self.config,
      )
      .unwrap_or_else(|_| {
         crate::api::fallback_summary(
            &fixture.input.stat,
            &detail_points,
            analysis.commit_type.as_str(),
            &self.config,
         )
      });

      let final_commit = ConventionalCommit {
         commit_type: analysis.commit_type.clone(),
         scope:       analysis.scope.clone(),
         summary,
         body:        detail_points,
         footers:     vec![],
      };
      let final_message = format_commit_message(&final_commit);

      // Compare to golden if exists
      let comparison = fixture
         .golden
         .as_ref()
         .map(|g| compare_analysis(&g.analysis, &analysis));

      Ok(RunResult {
         name: name.to_string(),
         comparison,
         analysis,
         final_message,
         error: None,
      })
   }

   /// Update golden files for all fixtures
   pub fn update_all(&self) -> Result<Vec<String>> {
      let fixture_names = discover_fixtures(&self.fixtures_dir)?;
      let mut updated = Vec::new();

      for name in fixture_names {
         if let Some(ref pattern) = self.filter {
            if !name.contains(pattern) {
               continue;
            }
         }

         self.update_fixture(&name)?;
         updated.push(name);
      }

      Ok(updated)
   }

   /// Update golden file for a single fixture
   pub fn update_fixture(&self, name: &str) -> Result<()> {
      let result = self.run_fixture(name);

      if let Some(err) = result.error {
         return Err(crate::error::CommitGenError::Other(format!(
            "Failed to run fixture '{}': {}",
            name, err
         )));
      }

      let mut fixture = Fixture::load(&self.fixtures_dir, name)?;
      fixture.update_golden(result.analysis, result.final_message);
      fixture.save(&self.fixtures_dir)?;

      Ok(())
   }
}

/// Summary of test run
#[derive(Debug, Default)]
pub struct TestSummary {
   pub total: usize,
   pub passed: usize,
   pub failed: usize,
   pub no_golden: usize,
   pub errors: usize,
}

impl TestSummary {
   /// Create summary from results
   pub fn from_results(results: &[RunResult]) -> Self {
      let mut summary = Self::default();
      summary.total = results.len();

      for result in results {
         if result.error.is_some() {
            summary.errors += 1;
         } else if let Some(ref cmp) = result.comparison {
            if cmp.passed {
               summary.passed += 1;
            } else {
               summary.failed += 1;
            }
         } else {
            summary.no_golden += 1;
         }
      }

      summary
   }

   /// Check if all tests passed
   pub fn all_passed(&self) -> bool {
      self.failed == 0 && self.errors == 0
   }
}
