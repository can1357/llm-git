//! Comparison logic for fixture testing

use crate::types::ConventionalAnalysis;

/// Result of comparing actual output to golden
#[derive(Debug, Clone)]
pub struct CompareResult {
   /// Whether the type matches
   pub type_match: bool,
   /// Whether the scope matches (or both are None)
   pub scope_match: bool,
   /// Scope difference description if any
   pub scope_diff: Option<String>,
   /// Number of details in golden
   pub golden_detail_count: usize,
   /// Number of details in actual
   pub actual_detail_count: usize,
   /// Overall pass/fail
   pub passed: bool,
   /// Human-readable summary
   pub summary: String,
}

/// Compare actual analysis to golden
pub fn compare_analysis(golden: &ConventionalAnalysis, actual: &ConventionalAnalysis) -> CompareResult {
   let type_match = golden.commit_type == actual.commit_type;

   let scope_match = golden.scope == actual.scope;
   let scope_diff = if !scope_match {
      Some(format!(
         "{} → {}",
         golden.scope.as_ref().map(|s| s.as_str()).unwrap_or("null"),
         actual.scope.as_ref().map(|s| s.as_str()).unwrap_or("null")
      ))
   } else {
      None
   };

   let golden_detail_count = golden.details.len();
   let actual_detail_count = actual.details.len();

   // Type mismatch is a hard failure
   // Scope mismatch is a warning (might be an improvement)
   let passed = type_match;

   let summary = if passed && scope_match {
      format!(
         "✓ {} | {} | {} details",
         actual.commit_type.as_str(),
         actual.scope.as_ref().map(|s| s.as_str()).unwrap_or("(no scope)"),
         actual_detail_count
      )
   } else if passed {
      format!(
         "≈ {} | scope: {} | {} details",
         actual.commit_type.as_str(),
         scope_diff.as_ref().unwrap(),
         actual_detail_count
      )
   } else {
      format!(
         "✗ type: {} → {} | {} details",
         golden.commit_type.as_str(),
         actual.commit_type.as_str(),
         actual_detail_count
      )
   };

   CompareResult {
      type_match,
      scope_match,
      scope_diff,
      golden_detail_count,
      actual_detail_count,
      passed,
      summary,
   }
}

#[cfg(test)]
mod tests {
   use std::collections::HashSet;

   use super::*;
   use crate::types::{CommitType, Scope};

   /// Compute Jaccard similarity between two strings (word-level)
   fn jaccard_similarity(a: &str, b: &str) -> f64 {
      let words_a: HashSet<&str> = a.split_whitespace().collect();
      let words_b: HashSet<&str> = b.split_whitespace().collect();

      if words_a.is_empty() && words_b.is_empty() {
         return 1.0;
      }

      let intersection = words_a.intersection(&words_b).count();
      let union = words_a.union(&words_b).count();

      if union == 0 {
         return 0.0;
      }

      intersection as f64 / union as f64
   }

   #[test]
   fn test_compare_exact_match() {
      let golden = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("api").unwrap()),
         details:     vec![],
         issue_refs:  vec![],
      };
      let actual = golden.clone();

      let result = compare_analysis(&golden, &actual);
      assert!(result.passed);
      assert!(result.type_match);
      assert!(result.scope_match);
   }

   #[test]
   fn test_compare_type_mismatch() {
      let golden = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         details:     vec![],
         issue_refs:  vec![],
      };
      let actual = ConventionalAnalysis {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       None,
         details:     vec![],
         issue_refs:  vec![],
      };

      let result = compare_analysis(&golden, &actual);
      assert!(!result.passed);
      assert!(!result.type_match);
   }

   #[test]
   fn test_compare_scope_mismatch() {
      let golden = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("api").unwrap()),
         details:     vec![],
         issue_refs:  vec![],
      };
      let actual = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("api/client").unwrap()),
         details:     vec![],
         issue_refs:  vec![],
      };

      let result = compare_analysis(&golden, &actual);
      assert!(result.passed); // Scope mismatch is warning, not failure
      assert!(!result.scope_match);
      assert!(result.scope_diff.is_some());
   }

   #[test]
   fn test_jaccard_similarity() {
      assert!((jaccard_similarity("hello world", "hello world") - 1.0).abs() < 0.001);
      assert!((jaccard_similarity("hello world", "hello there") - 0.333).abs() < 0.1);
      assert!((jaccard_similarity("", "") - 1.0).abs() < 0.001);
   }
}
