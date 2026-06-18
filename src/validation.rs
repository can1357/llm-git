use crate::{
   config::CommitConfig,
   error::{CommitGenError, Result},
   git::git_command,
   style::{self, icons},
   types::ConventionalCommit,
};

// Static lookup tables (verbs, morphology blocklists, file extensions, filler/
// meta phrases) are codegen'd from `src/validation_data.json` by `build.rs`,
// defining `PAST_TENSE_MAP`, `IRREGULAR_PAST`, `ED_BLOCKLIST`, `D_BLOCKLIST`,
// `CODE_EXTENSIONS`, `DOC_EXTENSIONS`, `FILLER_WORDS`, `META_PHRASES`, and
// `BODY_PRESENT_TENSE`. Edit the JSON, not generated code.
include!(concat!(env!("OUT_DIR"), "/validation_data.rs"));

/// Check if an extension is a code file extension
fn is_code_extension(ext: &str) -> bool {
   CODE_EXTENSIONS.iter().any(|&e| e.eq_ignore_ascii_case(ext))
}

/// Get repository name from git working directory
fn get_repository_name() -> Result<String> {
   let output = git_command()
      .args(["rev-parse", "--show-toplevel"])
      .output()
      .map_err(|e| CommitGenError::git(e.to_string()))?;

   if !output.status.success() {
      return Err(CommitGenError::git("Failed to get repository root".to_string()));
   }

   let path = String::from_utf8_lossy(&output.stdout);
   let repo_name = std::path::Path::new(path.trim())
      .file_name()
      .and_then(|n| n.to_str())
      .ok_or_else(|| CommitGenError::git("Could not extract repository name".to_string()))?;

   Ok(repo_name.to_string())
}

/// Normalize name for comparison (convert hyphens/underscores, lowercase)
fn normalize_name(name: &str) -> String {
   name.to_lowercase().replace(['-', '_'], "")
}

/// Look up the past-tense form of a lowercase present-tense verb.
pub fn present_to_past(present: &str) -> Option<&'static str> {
   PAST_TENSE_MAP
      .iter()
      .find(|(k, _)| *k == present)
      .map(|(_, v)| *v)
}

/// Extract the verb stem from a first-word token by stripping any trailing
/// non-alphabetic suffix (dash, slash, number, etc.).
///
/// Returns the lowercase stem, or `None` if the leading run is all uppercase
/// (acronym like `API`/`NFC`) or there are no leading ASCII letters \u{2014}
/// those are skipped for conversion since they aren't verbs.
pub fn verb_stem(token: &str) -> Option<String> {
   let n = token
      .bytes()
      .take_while(|&b| b.is_ascii_alphabetic())
      .count();
   if n == 0 {
      return None;
   }
   let stem = &token[..n];
   // Skip all-caps acronyms; they're not verbs we convert.
   if stem.chars().all(|c| c.is_uppercase()) {
      return None;
   }
   Some(stem.to_ascii_lowercase())
}

/// Split a first-word token into `(stem, suffix)` where `stem` is the
/// leading ASCII-alphabetic run and `suffix` is the remainder.
///
/// Returns `None` if the token has no leading letters.
pub fn split_verb_token(token: &str) -> Option<(&str, &str)> {
   let n = token
      .bytes()
      .take_while(|&b| b.is_ascii_alphabetic())
      .count();
   if n == 0 {
      None
   } else {
      Some((&token[..n], &token[n..]))
   }
}

/// Check if word is past-tense verb using morphology + common irregulars.
/// `word` should be a bare verb (no trailing suffix); use
/// [`is_past_tense_first_word`] for raw summary tokens.
pub fn is_past_tense_verb(word: &str) -> bool {
   // Values in PAST_TENSE_MAP that are genuinely past-tense (key != value).
   // Same-form entries like `("reset", "reset")` are NOT accepted here;
   // they're handled by UNCHANGED_IRREGULAR below.
   if PAST_TENSE_MAP.iter().any(|(k, v)| *v == word && *k != *v) {
      return true;
   }

   // Regular past tense: ends with -ed
   if word.ends_with("ed") {
      // Exclude common false positives (words that end in -ed but aren't verbs)
      return !ED_BLOCKLIST.contains(&word);
   }

   // Words ending in single 'd' preceded by vowel (configured, exposed, etc.)
   // Must be at least 4 chars and not end in common non-verb patterns
   if word.len() >= 4 && word.ends_with('d') {
      let before_d = &word[word.len() - 2..word.len() - 1];
      // Check if letter before 'd' is vowel (covers: configured, exposed, etc.)
      if "aeiou".contains(before_d) {
         return !D_BLOCKLIST.contains(&word);
      }
   }

   IRREGULAR_PAST.contains(&word)
}

/// Check whether a summary's first raw token is a past-tense verb, tolerating
/// trailing non-alpha suffixes (e.g. `bound-check`, `isolated-subagent`).
///
/// The full token is tried first (so `re-enabled` passes via `-ed`), then the
/// stripped stem. All-caps acronyms and numeric-led tokens are rejected.
pub fn is_past_tense_first_word(token: &str) -> bool {
   if token.is_empty() {
      return false;
   }
   // Try the full token first (covers `re-enabled`, `auto-detected`, ...).
   if is_past_tense_verb(&token.to_ascii_lowercase()) {
      return true;
   }
   // Then try the stripped stem (`bound-check` -> `bound`).
   if let Some(stem) = verb_stem(token)
      && is_past_tense_verb(&stem)
   {
      return true;
   }
   // Handle `re-` prefixed verbs: `re-ran`, `re-built`, `re-wrote`.
   // `split_verb_token` gives stem="re", suffix="-ran". Parse the inner
   // segment and check it as past tense.
   if let Some((stem, suffix)) = split_verb_token(token)
      && stem.eq_ignore_ascii_case("re")
      && let Some(rest) = suffix.strip_prefix('-')
   {
      let inner_n = rest
         .bytes()
         .take_while(|&b| b.is_ascii_alphabetic())
         .count();
      if inner_n > 0 {
         let inner = &rest[..inner_n];
         // Try the inner segment as a past-tense verb (covers `re-ran`,
         // `re-built`, `re-wrote`, `re-read`, `re-set`).
         if is_past_tense_verb(&inner.to_ascii_lowercase()) {
            return true;
         }
         // Also try converting from present tense (e.g. `re-enable` ->
         // `enabled` is past, so the present `enable` should be accepted
         // because normalization will convert it).
         if present_to_past(&inner.to_ascii_lowercase()).is_some() {
            return true;
         }
      }
   }
   false
}

/// Validate conventional commit message
pub fn validate_commit_message(msg: &ConventionalCommit, config: &CommitConfig) -> Result<()> {
   // Validate commit type
   let valid_types = [
      "feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert",
      "deps", "security", "config", "ux", "release", "hotfix", "infra", "init", "merge", "hack",
      "wip",
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
   if let Some(scope) = &msg.scope
      && scope.is_empty()
   {
      return Err(CommitGenError::InvalidScope(
         "Scope cannot be empty string (omit if not applicable)".to_string(),
      ));
   }

   // Reject scope if it's just the project/repo name
   if let Some(scope) = &msg.scope
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
      style::warn(&format!(
         "Summary exceeds soft limit: {} > {} chars (retry recommended)",
         first_line_len, config.summary_soft_limit
      ));
   }

   // Guideline warning (72-96 range) - INFO
   if first_line_len > config.summary_guideline && first_line_len <= config.summary_soft_limit {
      eprintln!(
         "{} {}",
         style::info(icons::INFO),
         style::info(&format!(
            "Summary exceeds guideline: {} > {} chars (still acceptable)",
            first_line_len, config.summary_guideline
         ))
      );
   }

   // Note: lowercase check is done in CommitSummary::new() to avoid duplication

   // Check first word is past-tense verb (morphology-based)
   let first_word = msg.summary.as_str().split_whitespace().next().unwrap_or("");

   if first_word.is_empty() {
      return Err(CommitGenError::ValidationError(
         "Summary must contain at least one word".to_string(),
      ));
   }

   if !is_past_tense_first_word(first_word) {
      return Err(CommitGenError::ValidationError(format!(
         "Summary must start with a past-tense verb (ending in -ed/-d or irregular). Got \
          '{first_word}'"
      )));
   }

   // Check for type-word repetition
   let type_word = msg.commit_type.as_str();
   let first_word_lower = first_word.to_lowercase();
   if first_word_lower == type_word {
      return Err(CommitGenError::ValidationError(format!(
         "Summary repeats commit type '{type_word}': first word is '{first_word}'"
      )));
   }

   // Check for filler words (removed "improved"/"enhanced" as they're valid
   // past-tense verbs)
   for filler in FILLER_WORDS {
      if msg.summary.as_str().to_lowercase().contains(filler) {
         style::warn(&format!("Summary contains filler word '{}': {}", filler, msg.summary));
      }
   }

   // Check for meta-phrases that add no information
   for phrase in META_PHRASES {
      if msg.summary.as_str().to_lowercase().contains(phrase) {
         style::warn(&format!(
            "Summary contains meta-phrase '{phrase}' - be more specific about what changed"
         ));
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
      if BODY_PRESENT_TENSE
         .iter()
         .any(|&word| first_word.to_lowercase() == word)
      {
         style::warn(&format!("Body item uses present tense: '{item}'"));
      }
      if !item.trim_end().ends_with('.') {
         style::warn(&format!("Body item missing period: '{item}'"));
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
         let is_doc_file = std::path::Path::new(&path)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| DOC_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
         is_doc_file
            || path.to_lowercase().contains("/docs/")
            || path.to_lowercase().contains("readme")
      });
      if !has_docs {
         style::warn("Commit type 'docs' but no documentation files changed");
      }
   }

   // Check for test type
   if commit_type == "test" {
      let has_test = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim().to_lowercase();
         path.contains("/test") || path.contains("_test.") || path.contains(".test.")
      });
      if !has_test {
         style::warn("Commit type 'test' but no test files changed");
      }
   }

   // Check for style type (should be mostly whitespace/formatting)
   if commit_type == "style" {
      let has_code = stat.lines().any(|line| {
         let path = line.split('|').next().unwrap_or("").trim();
         let path_obj = std::path::Path::new(&path);
         path_obj
            .extension()
            .is_some_and(|ext| is_code_extension(ext.to_str().unwrap_or("")))
      });
      if has_code {
         style::warn("Commit type 'style' but code files changed (verify no logic changes)");
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
         style::warn("Commit type 'ci' but no CI configuration files changed");
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
         style::warn("Commit type 'build' but no build files (Cargo.toml, package.json) changed");
      }
   }

   // Check for refactor with new files (might actually be feat)
   if commit_type == "refactor" {
      let has_new_files = stat
         .lines()
         .any(|line| line.trim().starts_with("create mode") || line.contains("new file"));
      if has_new_files {
         style::warn(
            "Commit type 'refactor' but new files were created - verify no new capabilities added \
             (might be 'feat')",
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
         style::warn(
            "Commit type 'perf' but no performance-related files or optimization keywords found",
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

   #[test]
   fn test_is_past_tense_verb_map_values() {
      // Values from PAST_TENSE_MAP should be accepted as past tense
      assert!(is_past_tense_verb("hardened"));
      assert!(is_past_tense_verb("bound"));
      assert!(is_past_tense_verb("isolated"));
      assert!(is_past_tense_verb("guarded"));
      assert!(is_past_tense_verb("rebuilt"));
      assert!(is_past_tense_verb("rewrote"));
      assert!(is_past_tense_verb("reran"));
   }

   #[test]
   fn test_is_past_tense_verb_same_form_not_accepted_via_map() {
      // Same-form entries (key == value) should NOT be accepted via the map
      // check. "reset" is accepted via IRREGULAR_PAST, not the map.
      // But "setup" was removed from IRREGULAR_PAST, so it should NOT pass.
      assert!(!is_past_tense_verb("setup"));
      // "reset" is in IRREGULAR_PAST so it passes
      assert!(is_past_tense_verb("reset"));
   }

   #[test]
   fn test_is_past_tense_first_word_suffix_tolerance() {
      // Trailing non-alpha suffix should be stripped for stem check
      assert!(is_past_tense_first_word("bound-check"));
      assert!(is_past_tense_first_word("isolated-subagent"));
      assert!(is_past_tense_first_word("re-enabled"));
      assert!(is_past_tense_first_word("auto-detected"));
      // Full token that is past tense via -ed
      assert!(is_past_tense_first_word("hardened"));
   }

   #[test]
   fn test_is_past_tense_first_word_acronyms_rejected() {
      // All-caps acronyms should be rejected
      assert!(!is_past_tense_first_word("API"));
      assert!(!is_past_tense_first_word("NFC"));
      assert!(!is_past_tense_first_word("LSP"));
   }

   #[test]
   fn test_is_past_tense_first_word_numeric_rejected() {
      // Numeric-led tokens should be rejected
      assert!(!is_past_tense_first_word("403"));
      assert!(!is_past_tense_first_word("v1.0"));
      assert!(!is_past_tense_first_word("2.0.0"));
   }

   #[test]
   fn test_is_past_tense_first_word_re_prefix() {
      // re-ran: inner segment "ran" is past tense
      assert!(is_past_tense_first_word("re-ran"));
      // re-built: inner segment "built" is past tense
      assert!(is_past_tense_first_word("re-built"));
      // re-wrote: inner segment "wrote" is past tense
      assert!(is_past_tense_first_word("re-wrote"));
      // re-enabled: full token ends in -ed, passes via full token check
      assert!(is_past_tense_first_word("re-enabled"));
      // re-enable: inner "enable" is present tense but in map, so accepted
      // (normalization will convert it to re-enabled)
      assert!(is_past_tense_first_word("re-enable"));
      // re-read: inner "read" is unchanged irregular
      assert!(is_past_tense_first_word("re-read"));
      // re-reset: inner "reset" is unchanged irregular
      assert!(is_past_tense_first_word("re-reset"));
   }

   #[test]
   fn test_is_past_tense_first_word_re_prefix_rejected() {
      // re- with non-verb inner segment should be rejected
      assert!(!is_past_tense_first_word("re-foo"));
      assert!(!is_past_tense_first_word("re-123"));
   }

   #[test]
   fn test_verb_stem_extraction() {
      assert_eq!(verb_stem("bound-check"), Some("bound".to_string()));
      assert_eq!(verb_stem("isolated-subagent"), Some("isolated".to_string()));
      assert_eq!(verb_stem("harden"), Some("harden".to_string()));
      // All-caps -> None (acronym)
      assert_eq!(verb_stem("API"), None);
      assert_eq!(verb_stem("NFC"), None);
      // No leading letters -> None
      assert_eq!(verb_stem("403"), None);
      assert_eq!(verb_stem(""), None);
   }

   #[test]
   fn test_split_verb_token() {
      assert_eq!(split_verb_token("bound-check"), Some(("bound", "-check")));
      assert_eq!(split_verb_token("harden"), Some(("harden", "")));
      assert_eq!(split_verb_token("fix(tui):"), Some(("fix", "(tui):")));
      assert_eq!(split_verb_token("403"), None);
   }

   #[test]
   fn test_present_to_past_lookup() {
      assert_eq!(present_to_past("harden"), Some("hardened"));
      assert_eq!(present_to_past("bind"), Some("bound"));
      assert_eq!(present_to_past("isolate"), Some("isolated"));
      assert_eq!(present_to_past("rebuild"), Some("rebuilt"));
      assert_eq!(present_to_past("nonexistent"), None);
   }

   #[test]
   fn test_validate_bound_and_hardened() {
      let config = CommitConfig::default();
      // "bound" should pass validation (the original failing case)
      let msg =
         create_commit("fix", Some("stealth"), "bound native Reflect methods to variables", vec![]);
      assert!(
         validate_commit_message(&msg, &config).is_ok(),
         "'bound' should be accepted as past-tense verb"
      );
      // "hardened" should pass validation
      let msg = create_commit(
         "fix",
         Some("stealth"),
         "hardened stealth scripts against detection",
         vec![],
      );
      assert!(
         validate_commit_message(&msg, &config).is_ok(),
         "'hardened' should be accepted as past-tense verb"
      );
   }

   #[test]
   fn test_validate_bound_check_suffix() {
      let config = CommitConfig::default();
      // "bound-check" should pass via stem extraction
      let msg = create_commit("fix", None, "bound-checked the inputs", vec![]);
      assert!(
         validate_commit_message(&msg, &config).is_ok(),
         "'bound-checked' should be accepted as past-tense verb"
      );
   }
}
