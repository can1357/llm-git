use std::process::Command;

use crate::{
   config::CommitConfig,
   error::{CommitGenError, Result},
   types::ConventionalCommit,
};

/// Get repository name from git working directory
fn get_repository_name() -> Result<String> {
   let output = Command::new("git")
      .args(["rev-parse", "--show-toplevel"])
      .output()
      .map_err(|e| CommitGenError::GitError(e.to_string()))?;

   if !output.status.success() {
      return Err(CommitGenError::GitError("Failed to get repository root".to_string()));
   }

   let path = String::from_utf8_lossy(&output.stdout);
   let repo_name = std::path::Path::new(path.trim())
      .file_name()
      .and_then(|n| n.to_str())
      .ok_or_else(|| CommitGenError::GitError("Could not extract repository name".to_string()))?;

   Ok(repo_name.to_string())
}

/// Normalize name for comparison (convert hyphens/underscores, lowercase)
fn normalize_name(name: &str) -> String {
   name.to_lowercase().replace(['-', '_'], "")
}

/// Check if word is past-tense verb using morphology + common irregulars
pub fn is_past_tense_verb(word: &str) -> bool {
   // Regular past tense: ends with -ed
   if word.ends_with("ed") {
      // Exclude common false positives (words that end in -ed but aren't verbs)
      const BLOCKLIST: &[&str] = &["hundred", "thousand", "red", "bed", "wed", "shed"];
      return !BLOCKLIST.contains(&word);
   }

   // Words ending in single 'd' preceded by vowel (configured, exposed, etc.)
   // Must be at least 4 chars and not end in common non-verb patterns
   if word.len() >= 4 && word.ends_with('d') {
      let before_d = &word[word.len() - 2..word.len() - 1];
      // Check if letter before 'd' is vowel (covers: configured, exposed, etc.)
      if "aeiou".contains(before_d) {
         const D_BLOCKLIST: &[&str] = &[
            "and", "bad", "bid", "god", "had", "kid", "lad", "mad", "mid", "mud", "nod", "odd",
            "old", "pad", "raid", "said", "sad", "should", "would", "could",
         ];
         return !D_BLOCKLIST.contains(&word);
      }
   }

   // Common irregular past-tense verbs
   const IRREGULAR: &[&str] = &[
      "made",
      "built",
      "ran",
      "wrote",
      "took",
      "gave",
      "found",
      "kept",
      "left",
      "felt",
      "meant",
      "sent",
      "spent",
      "lost",
      "held",
      "told",
      "sold",
      "stood",
      "understood",
      "became",
      "began",
      "brought",
      "bought",
      "caught",
      "taught",
      "thought",
      "fought",
      "sought",
      "chose",
      "came",
      "did",
      "got",
      "had",
      "knew",
      "met",
      "put",
      "read",
      "saw",
      "said",
      "set",
      "sat",
      "cut",
      "let",
      "hit",
      "hurt",
      "shut",
      "split",
      "spread",
      "bet",
      "cast",
      "cost",
      "quit",
   ];

   IRREGULAR.contains(&word)
}

/// Validate conventional commit message
pub fn validate_commit_message(msg: &ConventionalCommit, config: &CommitConfig) -> Result<()> {
   // Validate commit type
   let valid_types = [
      "feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert",
   ];
   if !valid_types.contains(&msg.commit_type.as_str()) {
      return Err(CommitGenError::InvalidCommitType(format!(
         "Invalid commit type: '{}'. Must be one of: {}",
         msg.commit_type,
         valid_types.join(", ")
      )));
   }

   // Validate scope (if present) - Scope type already validates format
   // This is just a double-check, Scope::new() already enforces rules
   if let Some(ref scope) = msg.scope
      && scope.is_empty()
   {
      return Err(CommitGenError::InvalidScope(
         "Scope cannot be empty string (omit if not applicable)".to_string(),
      ));
   }

   // Reject scope if it's just the project/repo name
   if let Some(ref scope) = msg.scope
      && let Ok(repo_name) = get_repository_name()
   {
      let normalized_scope = normalize_name(scope.as_str());
      let normalized_repo = normalize_name(&repo_name);

      if normalized_scope == normalized_repo {
         return Err(CommitGenError::InvalidScope(format!(
            "Scope '{scope}' is the project name - omit scope for project-wide changes"
         )));
      }
   }

   // Check summary not empty
   if msg.summary.as_str().trim().is_empty() {
      return Err(CommitGenError::ValidationError("Summary cannot be empty".to_string()));
   }

   // Check summary does NOT end with period (conventional commits don't use
   // periods)
   if msg.summary.as_str().trim_end().ends_with('.') {
      return Err(CommitGenError::ValidationError(
         "Summary must NOT end with a period (conventional commits style)".to_string(),
      ));
   }

   // Check first line length: type(scope): summary
   let scope_part = msg
      .scope
      .as_ref()
      .map(|s| format!("({s})"))
      .unwrap_or_default();
   let first_line_len = msg.commit_type.len() + scope_part.len() + 2 + msg.summary.len();

   // Hard limit check (absolute maximum) - REJECT
   if first_line_len > config.summary_hard_limit {
      return Err(CommitGenError::SummaryTooLong {
         len: first_line_len,
         max: config.summary_hard_limit,
      });
   }

   // Soft limit warning (triggers retry in main.rs) - WARN but pass
   if first_line_len > config.summary_soft_limit {
      eprintln!(
         "⚠ Summary exceeds soft limit: {} > {} chars (retry recommended)",
         first_line_len, config.summary_soft_limit
      );
   }

   // Guideline warning (72-96 range) - INFO
   if first_line_len > config.summary_guideline && first_line_len <= config.summary_soft_limit {
      eprintln!(
         "ℹ Summary exceeds guideline: {} > {} chars (still acceptable)",
         first_line_len, config.summary_guideline
      );
   }

   // Check summary starts with lowercase after type:
   if let Some(first_char) = msg.summary.as_str().trim().chars().next()
      && first_char.is_uppercase()
   {
      eprintln!("Warning: Summary '{}' should start lowercase after type:", msg.summary);
   }

   // Check first word is past-tense verb (morphology-based)
   let first_word = msg.summary.as_str().split_whitespace().next().unwrap_or("");

   if first_word.is_empty() {
      return Err(CommitGenError::ValidationError(
         "Summary must contain at least one word".to_string(),
      ));
   }

   let first_word_lower = first_word.to_lowercase();
   if !is_past_tense_verb(&first_word_lower) {
      return Err(CommitGenError::ValidationError(format!(
         "Summary must start with a past-tense verb (ending in -ed/-d or irregular). Got \
          '{first_word}'"
      )));
   }

   // Check for type-word repetition
   let type_word = msg.commit_type.as_str();
   if first_word_lower == type_word {
      return Err(CommitGenError::ValidationError(format!(
         "Summary repeats commit type '{type_word}': first word is '{first_word}'"
      )));
   }

   // Check for filler words (removed "improved"/"enhanced" as they're valid
   // past-tense verbs)
   const FILLER_WORDS: &[&str] = &["comprehensive", "better", "various", "several"];
   for filler in FILLER_WORDS {
      if msg.summary.as_str().to_lowercase().contains(filler) {
         eprintln!("Warning: Summary contains filler word '{}': {}", filler, msg.summary);
      }
   }

   // Check for meta-phrases that add no information
   const META_PHRASES: &[&str] = &[
      "this commit",
      "this change",
      "updated code",
      "updated the",
      "modified code",
      "changed code",
      "improved code",
      "modified the",
      "changed the",
   ];
   for phrase in META_PHRASES {
      if msg.summary.as_str().to_lowercase().contains(phrase) {
         eprintln!(
            "Warning: Summary contains meta-phrase '{phrase}' - be more specific about what \
             changed"
         );
      }
   }

   // Final length check after all potential mutations
   let final_scope_part = msg
      .scope
      .as_ref()
      .map(|s| format!("({s})"))
      .unwrap_or_default();
   let final_first_line_len =
      msg.commit_type.len() + final_scope_part.len() + 2 + msg.summary.len();

   if final_first_line_len > config.summary_hard_limit {
      return Err(CommitGenError::SummaryTooLong {
         len: final_first_line_len,
         max: config.summary_hard_limit,
      });
   }

   // Validate body items
   for item in &msg.body {
      let first_word = item.split_whitespace().next().unwrap_or("");
      let present_tense = [
         "adds",
         "fixes",
         "updates",
         "removes",
         "changes",
         "creates",
         "refactors",
         "implements",
         "migrates",
         "renames",
         "moves",
         "replaces",
         "improves",
         "merges",
         "splits",
         "extracts",
         "restructures",
         "reorganizes",
         "consolidates",
      ];
      if present_tense
         .iter()
         .any(|&word| first_word.to_lowercase() == word)
      {
         eprintln!("Warning: Body item uses present tense: '{item}'");
      }
      if !item.trim_end().ends_with('.') {
         eprintln!("Warning: Body item missing period: '{item}'");
      }
   }

   Ok(())
}

/// Check type-scope consistency (warn if mismatched)
pub fn check_type_scope_consistency(msg: &ConventionalCommit, stat: &str) {
   let commit_type = msg.commit_type.as_str();

   // Check for docs type
   if commit_type == "docs" {
      let has_docs = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim();
         std::path::Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            || path.to_lowercase().contains("/docs/")
            || path.to_lowercase().contains("readme")
      });
      if !has_docs {
         eprintln!("Warning: Commit type 'docs' but no documentation files (.md) changed");
      }
   }

   // Check for test type
   if commit_type == "test" {
      let has_test = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim().to_lowercase();
         path.contains("/test") || path.contains("_test.") || path.contains(".test.")
      });
      if !has_test {
         eprintln!("Warning: Commit type 'test' but no test files changed");
      }
   }

   // Check for style type (should be mostly whitespace/formatting)
   if commit_type == "style" {
      let has_code = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim();
         let path_obj = std::path::Path::new(&path);
         path_obj.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("rs")
               || ext.eq_ignore_ascii_case("js")
               || ext.eq_ignore_ascii_case("py")
               || ext.eq_ignore_ascii_case("go")
         })
      });
      if has_code {
         eprintln!("Warning: Commit type 'style' but code files changed (verify no logic changes)");
      }
   }

   // Check for ci type
   if commit_type == "ci" {
      let has_ci = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim().to_lowercase();
         path.contains(".github/workflows")
            || path.contains(".gitlab-ci")
            || path.contains("jenkinsfile")
      });
      if !has_ci {
         eprintln!("Warning: Commit type 'ci' but no CI configuration files changed");
      }
   }

   // Check for build type
   if commit_type == "build" {
      let has_build = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim().to_lowercase();
         path.contains("cargo.toml")
            || path.contains("package.json")
            || path.contains("makefile")
            || path.contains("build.")
      });
      if !has_build {
         eprintln!(
            "Warning: Commit type 'build' but no build files (Cargo.toml, package.json) changed"
         );
      }
   }

   // Check for refactor with new files (might actually be feat)
   if commit_type == "refactor" {
      let has_new_files = stat
         .lines()
         .any(|line| line.trim().starts_with("create mode") || line.contains("new file"));
      if has_new_files {
         eprintln!(
            "Warning: Commit type 'refactor' but new files were created - verify no new \
             capabilities added (might be 'feat')"
         );
      }
   }

   // Check for perf type without performance evidence
   if commit_type == "perf" {
      let has_perf_files = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim().to_lowercase();
         path.contains("bench") || path.contains("perf") || path.contains("profile")
      });

      // Check if details mention performance
      let details_text = msg.body.join(" ").to_lowercase();
      let has_perf_details = details_text.contains("faster")
         || details_text.contains("optimization")
         || details_text.contains("performance")
         || details_text.contains("optimized");

      if !has_perf_files && !has_perf_details {
         eprintln!(
            "Warning: Commit type 'perf' but no performance-related files or optimization \
             keywords found"
         );
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::types::{CommitSummary, CommitType, ConventionalCommit, Scope};

   fn create_commit(
      type_str: &str,
      scope: Option<&str>,
      summary: &str,
      body: Vec<&str>,
   ) -> ConventionalCommit {
      ConventionalCommit {
         commit_type: CommitType::new(type_str).unwrap(),
         scope:       scope.map(|s| Scope::new(s).unwrap()),
         summary:     CommitSummary::new_unchecked(summary, 128).unwrap(),
         body:        body.into_iter().map(|s| s.to_string()).collect(),
         footers:     vec![],
      }
   }

   #[test]
   fn test_validate_valid_commit() {
      let config = CommitConfig::default();
      let msg = create_commit("feat", Some("api"), "added new endpoint", vec![]);
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_valid_commit_no_scope() {
      let config = CommitConfig::default();
      let msg = create_commit("fix", None, "corrected race condition", vec![]);
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_invalid_type() {
      let _config = CommitConfig::default();
      let result = CommitType::new("invalid");
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::InvalidCommitType(_)));
   }

   #[test]
   fn test_validate_summary_ends_with_period() {
      let config = CommitConfig::default();
      let msg = create_commit("feat", Some("api"), "added endpoint.", vec![]);
      let result = validate_commit_message(&msg, &config);
      assert!(result.is_err());
      assert!(
         result
            .unwrap_err()
            .to_string()
            .contains("must NOT end with a period")
      );
   }

   #[test]
   fn test_validate_summary_too_long() {
      // CommitSummary::new() enforces 128 char hard limit on summary alone
      let long_summary = "a".repeat(129);
      let result = CommitSummary::new(&long_summary, 128);
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::SummaryTooLong { .. }));
   }

   #[test]
   fn test_validate_summary_empty() {
      let result = CommitSummary::new("", 128);
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::ValidationError(_)));
   }

   #[test]
   fn test_validate_summary_empty_whitespace() {
      let result = CommitSummary::new("   ", 128);
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::ValidationError(_)));
   }

   #[test]
   fn test_validate_wrong_verb() {
      let config = CommitConfig::default();
      let result = CommitSummary::new_unchecked("adding new feature", 128);
      assert!(result.is_ok());
      let msg = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         summary:     result.unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      let result = validate_commit_message(&msg, &config);
      assert!(result.is_err());
      assert!(
         result
            .unwrap_err()
            .to_string()
            .contains("must start with a past-tense verb")
      );
   }

   #[test]
   fn test_validate_present_tense_verb() {
      let config = CommitConfig::default();
      let result = CommitSummary::new_unchecked("adds new feature", 128);
      assert!(result.is_ok());
      let msg = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         summary:     result.unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      let result = validate_commit_message(&msg, &config);
      assert!(result.is_err());
      assert!(
         result
            .unwrap_err()
            .to_string()
            .contains("must start with a past-tense verb")
      );
   }

   #[test]
   fn test_validate_no_type_verb_overlap() {
      // This test verifies that using a related verb doesn't trigger false positives
      // "documented" is valid for "docs" type since they're not exact matches
      let config = CommitConfig::default();
      let msg = create_commit("docs", Some("api"), "documented new api", vec![]);
      assert!(validate_commit_message(&msg, &config).is_ok());

      // "tested" is valid for "test" type
      let msg = create_commit("test", Some("api"), "added unit tests", vec![]);
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_morphology_based_past_tense() {
      let config = CommitConfig::default();
      // Test regular -ed endings
      let regular_verbs = ["added", "configured", "exposed", "formatted", "clarified"];
      for verb in regular_verbs {
         let summary = format!("{verb} something");
         let msg = create_commit("feat", None, &summary, vec![]);
         assert!(
            validate_commit_message(&msg, &config).is_ok(),
            "Regular verb '{verb}' should be accepted"
         );
      }

      // Test irregular verbs
      let irregular_verbs = ["made", "built", "ran", "wrote", "split"];
      for verb in irregular_verbs {
         let summary = format!("{verb} something");
         let msg = create_commit("feat", None, &summary, vec![]);
         assert!(
            validate_commit_message(&msg, &config).is_ok(),
            "Irregular verb '{verb}' should be accepted"
         );
      }

      // Test false positives (should be rejected)
      let non_verbs = ["hundred", "red", "bed"];
      for word in non_verbs {
         let summary = format!("{word} something");
         let msg = ConventionalCommit {
            commit_type: CommitType::new("feat").unwrap(),
            scope:       None,
            summary:     CommitSummary::new_unchecked(&summary, 128).unwrap(),
            body:        vec![],
            footers:     vec![],
         };
         assert!(
            validate_commit_message(&msg, &config).is_err(),
            "Non-verb '{word}' should be rejected"
         );
      }
   }

   #[test]
   fn test_validate_scope_empty_string() {
      let result = Scope::new("");
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::InvalidScope(_)));
   }

   #[test]
   fn test_validate_scope_invalid_chars() {
      let result = Scope::new("API/New");
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::InvalidScope(_)));
   }

   #[test]
   fn test_validate_scope_too_many_segments() {
      let result = Scope::new("core/api/http");
      assert!(result.is_err());
      assert!(result.unwrap_err().to_string().contains("max 2 allowed"));
   }

   #[test]
   fn test_validate_scope_valid_single() {
      let result = Scope::new("api");
      assert!(result.is_ok());
   }

   #[test]
   fn test_validate_scope_valid_two_segments() {
      let result = Scope::new("core/api");
      assert!(result.is_ok());
   }

   #[test]
   fn test_validate_scope_with_dash_underscore() {
      let result = Scope::new("core_api/http-client");
      assert!(result.is_ok());
   }

   #[test]
   fn test_validate_total_length_at_guideline() {
      let config = CommitConfig::default();
      // type(scope): summary = exactly 72 chars (guideline)
      // "feat(scope): " = 13 chars, summary = 59 chars, starts with valid verb
      let summary = format!("added {}", "x".repeat(53));
      let msg = create_commit("feat", Some("scope"), &summary, vec![]);
      // Should pass (with info message about being at guideline)
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_total_length_at_soft_limit() {
      let config = CommitConfig::default();
      // type(scope): summary = exactly 96 chars (soft limit)
      // "feat(scope): " = 13 chars, summary = 83 chars
      let summary = format!("added {}", "x".repeat(77));
      let msg = create_commit("feat", Some("scope"), &summary, vec![]);
      // Should pass (with warning about soft limit)
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_total_length_at_hard_limit() {
      let config = CommitConfig::default();
      // type(scope): summary = exactly 128 chars (hard limit)
      // "feat(scope): " = 13 chars, summary = 115 chars
      let summary = format!("added {}", "x".repeat(109));
      let msg = create_commit("feat", Some("scope"), &summary, vec![]);
      // Should pass (at hard limit)
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_total_length_over_hard_limit() {
      let config = CommitConfig::default();
      // type(scope): summary > 128 chars (exceeds hard limit)
      // "feat(scope): " = 13 chars, summary = 116 chars (total 129)
      let summary = "a".repeat(116);
      let msg = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("scope").unwrap()),
         summary:     CommitSummary::new_unchecked(&summary, 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      let result = validate_commit_message(&msg, &config);
      assert!(result.is_err());
      assert!(matches!(result.unwrap_err(), CommitGenError::SummaryTooLong { .. }));
   }

   #[test]
   fn test_check_type_scope_docs_with_md() {
      let msg = create_commit("docs", Some("readme"), "updated installation guide", vec![]);
      let stat = " README.md | 10 +++++++---\n 1 file changed, 7 insertions(+), 3 deletions(-)";
      // Should not print warning
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_docs_without_md() {
      let msg = create_commit("docs", None, "updated documentation", vec![]);
      let stat = " src/main.rs | 10 +++++++---\n 1 file changed, 7 insertions(+), 3 deletions(-)";
      // Should print warning (but we can't test stderr easily)
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_test_with_test_files() {
      let msg = create_commit("test", Some("api"), "added integration tests", vec![]);
      let stat = " tests/integration_test.rs | 50 ++++++++++++++++++++++++++++++++\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_test_without_test_files() {
      let msg = create_commit("test", None, "added tests", vec![]);
      let stat = " src/lib.rs | 10 +++++++---\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_refactor_new_files() {
      let msg = create_commit("refactor", Some("core"), "restructured modules", vec![]);
      let stat = " create mode 100644 src/new_module.rs\n src/lib.rs | 10 +++++++---\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_ci_with_workflow() {
      let msg = create_commit("ci", None, "updated github actions", vec![]);
      let stat = " .github/workflows/ci.yml | 20 ++++++++++++++++++++\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_build_with_cargo() {
      let msg = create_commit("build", Some("deps"), "updated dependencies", vec![]);
      let stat = " Cargo.toml | 5 +++--\n Cargo.lock | 150 +++++++++++++++++++\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_perf_with_details() {
      let msg = create_commit("perf", Some("core"), "optimized batch processing", vec![
         "reduced allocations by 50% for faster throughput.",
      ]);
      let stat = " src/core.rs | 30 +++++++++++++-----------------\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_check_type_scope_perf_without_evidence() {
      let msg = create_commit("perf", None, "changed algorithm", vec![]);
      let stat = " src/lib.rs | 10 +++++++---\n";
      check_type_scope_consistency(&msg, stat);
   }

   #[test]
   fn test_validate_body_present_tense_warning() {
      let config = CommitConfig::default();
      let msg = create_commit("feat", None, "added new feature", vec![
         "adds support for TLS.",
         "updates configuration.",
      ]);
      // Should succeed but print warnings (we can't easily test stderr)
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_validate_body_missing_period_warning() {
      let config = CommitConfig::default();
      let msg = create_commit("feat", None, "added new feature", vec![
         "added support for TLS",
         "updated configuration",
      ]);
      // Should succeed but print warnings
      assert!(validate_commit_message(&msg, &config).is_ok());
   }

   #[test]
   fn test_commit_type_case_normalization() {
      assert!(CommitType::new("FEAT").is_ok());
      assert!(CommitType::new("Feat").is_ok());
      assert!(CommitType::new("feat").is_ok());
      assert_eq!(CommitType::new("FEAT").unwrap().as_str(), "feat");
   }

   #[test]
   fn test_commit_type_all_valid() {
      let valid_types = [
         "feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci",
         "revert",
      ];
      for t in &valid_types {
         assert!(CommitType::new(*t).is_ok(), "Type '{t}' should be valid");
      }
   }

   #[test]
   fn test_summary_length_boundaries() {
      // Guideline (72) - should pass
      let summary_72 = "a".repeat(72);
      assert!(CommitSummary::new(&summary_72, 128).is_ok());

      // Soft limit (96) - should pass
      let summary_96 = "a".repeat(96);
      assert!(CommitSummary::new(&summary_96, 128).is_ok());

      // Hard limit (128) - should pass
      let summary_128 = "a".repeat(128);
      assert!(CommitSummary::new(&summary_128, 128).is_ok());

      // Over hard limit (129) - should fail
      let summary_129 = "a".repeat(129);
      let result = CommitSummary::new(&summary_129, 128);
      assert!(result.is_err());
      match result.unwrap_err() {
         CommitGenError::SummaryTooLong { len, max } => {
            assert_eq!(len, 129);
            assert_eq!(max, 128);
         },
         _ => panic!("Expected SummaryTooLong error"),
      }
   }
}
