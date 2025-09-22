/// Normalization utilities for commit messages
use unicode_normalization::UnicodeNormalization;

use crate::{config::CommitConfig, types::ConventionalCommit, validation::is_past_tense_verb};

/// Normalize Unicode characters to ASCII (remove AI-style formatting)
/// Normalize Unicode characters to ASCII (remove AI-style formatting)
pub fn normalize_unicode(text: &str) -> String {
   // Pre-NFKD replacements for chars that decompose badly
   // (≠ → = + combining, ½ → 1⁄2, ² → 2)
   let pre_normalized = text
      // Math symbols that decompose badly
      .replace('≠', "!=") // not equal to (decomposes to = + \u{338})
      // Fractions (NFKD decomposes ½ to 1⁄2 with fraction slash, not regular /)
      .replace('½', "1/2")
      .replace('¼', "1/4")
      .replace('¾', "3/4")
      .replace('⅓', "1/3")
      .replace('⅔', "2/3")
      .replace('⅕', "1/5")
      .replace('⅖', "2/5")
      .replace('⅗', "3/5")
      .replace('⅘', "4/5")
      .replace('⅙', "1/6")
      .replace('⅚', "5/6")
      .replace('⅛', "1/8")
      .replace('⅜', "3/8")
      .replace('⅝', "5/8")
      .replace('⅞', "7/8")
      // Superscripts (NFKD decomposes ² to just "2", losing the superscript meaning)
      .replace('⁰', "^0")
      .replace('¹', "^1")
      .replace('²', "^2")
      .replace('³', "^3")
      .replace('⁴', "^4")
      .replace('⁵', "^5")
      .replace('⁶', "^6")
      .replace('⁷', "^7")
      .replace('⁸', "^8")
      .replace('⁹', "^9")
      // Subscripts
      .replace('₀', "_0")
      .replace('₁', "_1")
      .replace('₂', "_2")
      .replace('₃', "_3")
      .replace('₄', "_4")
      .replace('₅', "_5")
      .replace('₆', "_6")
      .replace('₇', "_7")
      .replace('₈', "_8")
      .replace('₉', "_9");

   // Apply NFKD normalization for canonical decomposition
   let normalized: String = pre_normalized.nfkd().collect();

   normalized
      // Smart quotes to straight quotes
      .replace(['\u{2018}', '\u{2019}'], "'") // ' right single quote / apostrophe
      .replace(['\u{201C}', '\u{201D}'], "\"") // " right double quote
      .replace('\u{201A}', "'") // ‚ single low-9 quote
      .replace(['\u{201E}', '\u{00AB}', '\u{00BB}'], "\"") // » right-pointing double angle quote
      .replace(['\u{2039}', '\u{203A}'], "'") // › single right-pointing angle quote
      // Dashes and hyphens
      .replace(['\u{2010}', '\u{2011}', '\u{2012}'], "-") // ‒ figure dash
      .replace(['\u{2013}', '\u{2014}', '\u{2015}'], "--") // ― horizontal bar
      .replace('\u{2212}', "-") // − minus sign
      // Arrows
      .replace('\u{2192}', "->") // rightwards arrow
      .replace('←', "<-") // leftwards arrow
      .replace('↔', "<->") // left right arrow
      .replace('⇒', "=>") // rightwards double arrow
      .replace('⇐', "<=") // leftwards double arrow
      .replace('⇔', "<=>") // left right double arrow
      .replace('↑', "^") // upwards arrow
      .replace('↓', "v") // downwards arrow
      // Math symbols
      .replace('\u{2264}', "<=") // less than or equal to
      .replace('≥', ">=") // greater than or equal to
      .replace('≈', "~=") // approximately equal to
      .replace('≡', "==") // identical to
      .replace('\u{00D7}', "x") // multiplication sign
      .replace('÷', "/") // division sign
      // Ellipsis
      .replace(['\u{2026}', '⋯', '⋮'], "...") // vertical ellipsis
      // Bullet points (convert to hyphens for consistency)
      .replace(['•', '◦', '▪', '▫', '◆', '◇'], "-") // white diamond
      // Check marks
      .replace(['✓', '✔'], "v") // heavy check mark
      .replace(['✗', '✘'], "x") // heavy ballot x
      // Greek letters (common in programming)
      .replace('λ', "lambda")
      .replace('α', "alpha")
      .replace('β', "beta")
      .replace('γ', "gamma")
      .replace('δ', "delta")
      .replace('ε', "epsilon")
      .replace('θ', "theta")
      .replace('μ', "mu")
      .replace('π', "pi")
      .replace('σ', "sigma")
      .replace('Σ', "Sigma")
      .replace('Δ', "Delta")
      .replace('Π', "Pi")
      // Special spaces to regular space
      .replace(
         [
            '\u{00A0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}',
            '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}',
            '\u{3000}',
         ],
         " ",
      ) // ideographic space
      // Zero-width characters (remove)
      .replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "") // zero-width no-break space (BOM)
}

/// Cap detail points to top 3-4 by priority (keyword scoring + length)
pub fn cap_details(details: &mut Vec<String>, max_count: usize) {
   if details.len() <= max_count {
      return;
   }

   // Score by priority keywords and length
   let mut scored: Vec<(usize, i32, &String)> = details
      .iter()
      .enumerate()
      .map(|(idx, detail)| {
         let lower = detail.to_lowercase();
         let mut score = 0;

         // High priority keywords (security, crashes, critical bugs)
         if lower.contains("security")
            || lower.contains("vulnerability")
            || lower.contains("exploit")
            || lower.contains("critical")
            || (lower.contains("fix") && lower.contains("crash"))
         {
            score += 100;
         }
         if lower.contains("breaking") || lower.contains("incompatible") {
            score += 90;
         }
         if lower.contains("performance")
            || lower.contains("faster")
            || lower.contains("optimization")
         {
            score += 80;
         }
         if lower.contains("fix") || lower.contains("bug") {
            score += 70;
         }

         // Medium priority keywords
         if lower.contains("api") || lower.contains("interface") || lower.contains("public") {
            score += 50;
         }
         if lower.contains("user") || lower.contains("client") {
            score += 40;
         }
         if lower.contains("deprecated") || lower.contains("removed") {
            score += 35;
         }

         // Add length component (capped contribution to avoid favoring verbosity)
         score += (detail.len() / 20).min(10) as i32;

         (idx, score, detail)
      })
      .collect();

   // Sort by score descending
   scored.sort_by(|a, b| b.1.cmp(&a.1));

   // Keep top max_count indices
   let mut keep_indices: Vec<usize> = scored
      .iter()
      .take(max_count)
      .map(|(idx, ..)| *idx)
      .collect();
   keep_indices.sort_unstable(); // Preserve original order

   // Filter details
   let kept: Vec<String> = keep_indices
      .iter()
      .filter_map(|&idx| details.get(idx).cloned())
      .collect();
   *details = kept;
}

/// Convert present-tense verbs to past-tense and handle type-specific
/// replacements
pub fn normalize_summary_verb(summary: &mut String, commit_type: &str) {
   if summary.trim().is_empty() {
      return;
   }

   let mut parts_iter = summary.split_whitespace();
   let first_word = match parts_iter.next() {
      Some(word) => word.to_string(),
      None => return,
   };
   let rest = parts_iter.collect::<Vec<_>>().join(" ");
   let first_word_lower = first_word.to_lowercase();

   // Check if already past tense
   if is_past_tense_verb(&first_word_lower) {
      // Special case: refactor type shouldn't use "refactored"
      if commit_type == "refactor" && first_word_lower == "refactored" {
         *summary = if rest.is_empty() {
            "restructured".to_string()
         } else {
            format!("restructured {rest}")
         };
      }
      return;
   }

   // Convert present tense to past tense
   let converted = match first_word_lower.as_str() {
      "add" | "adds" => Some("added"),
      "fix" | "fixes" => Some("fixed"),
      "update" | "updates" => Some("updated"),
      "refactor" | "refactors" => Some(if commit_type == "refactor" {
         "restructured"
      } else {
         "refactored"
      }),
      "remove" | "removes" => Some("removed"),
      "replace" | "replaces" => Some("replaced"),
      "improve" | "improves" => Some("improved"),
      "implement" | "implements" => Some("implemented"),
      "migrate" | "migrates" => Some("migrated"),
      "rename" | "renames" => Some("renamed"),
      "move" | "moves" => Some("moved"),
      "merge" | "merges" => Some("merged"),
      "split" | "splits" => Some("split"),
      "extract" | "extracts" => Some("extracted"),
      "restructure" | "restructures" => Some("restructured"),
      "reorganize" | "reorganizes" => Some("reorganized"),
      "consolidate" | "consolidates" => Some("consolidated"),
      "simplify" | "simplifies" => Some("simplified"),
      "optimize" | "optimizes" => Some("optimized"),
      "document" | "documents" => Some("documented"),
      "test" | "tests" => Some("tested"),
      "change" | "changes" => Some("changed"),
      "introduce" | "introduces" => Some("introduced"),
      "deprecate" | "deprecates" => Some("deprecated"),
      "delete" | "deletes" => Some("deleted"),
      "correct" | "corrects" => Some("corrected"),
      "enhance" | "enhances" => Some("enhanced"),
      "revert" | "reverts" => Some("reverted"),
      _ => None,
   };

   if let Some(past) = converted {
      *summary = if rest.is_empty() {
         past.to_string()
      } else {
         format!("{past} {rest}")
      };
   }
}

/// Post-process conventional commit message to fix common issues
pub fn post_process_commit_message(msg: &mut ConventionalCommit, _config: &CommitConfig) {
   // CommitType and Scope are already normalized to lowercase in their
   // constructors No need to re-normalize them here

   // Extract summary string for mutations, will reconstruct at end
   let mut summary_str = normalize_unicode(msg.summary.as_str());

   // Normalize body and footers
   msg.body = msg.body.iter().map(|s| normalize_unicode(s)).collect();
   msg.footers = msg.footers.iter().map(|s| normalize_unicode(s)).collect();

   // Normalize summary formatting: single line, trimmed, enforce trailing period
   summary_str = summary_str
      .replace(['\r', '\n'], " ")
      .split_whitespace()
      .collect::<Vec<_>>()
      .join(" ")
      .trim()
      .trim_end_matches('.')
      .trim_end_matches(';')
      .trim_end_matches(':')
      .to_string();

   // Helper: check if first token is all caps (acronym/initialism)
   let is_first_token_all_caps = |s: &str| -> bool {
      s.split_whitespace().next().is_some_and(|token| {
         token
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase())
      })
   };

   // Ensure summary starts with lowercase (unless first token is all caps)
   if !is_first_token_all_caps(&summary_str)
      && let Some(first_char) = summary_str.chars().next()
      && first_char.is_uppercase()
   {
      let rest = &summary_str[first_char.len_utf8()..];
      summary_str = format!("{}{}", first_char.to_lowercase(), rest);
   }

   // Normalize verb tense (present \u{2192} past, handle type-specific
   // replacements)
   normalize_summary_verb(&mut summary_str, msg.commit_type.as_str());
   summary_str = summary_str.trim().to_string();

   // Ensure lowercase after normalization (unless first token is all caps)
   if !is_first_token_all_caps(&summary_str)
      && let Some(first_char) = summary_str.chars().next()
      && first_char.is_uppercase()
   {
      let rest = &summary_str[first_char.len_utf8()..];
      summary_str = format!("{}{}", first_char.to_lowercase(), rest);
   }

   // No truncation - validation handles length checks
   // Remove any trailing period (conventional commits don't use periods)
   summary_str = summary_str.trim_end_matches('.').to_string();

   // Reconstruct CommitSummary (bypassing warnings since post-processing
   // normalizes)
   msg.summary = crate::types::CommitSummary::new_unchecked(summary_str, 128)
      .expect("post-processed summary should be valid");

   // Clean and enforce punctuation for body items
   for item in &mut msg.body {
      let mut cleaned = item
         .replace(['\r', '\n'], " ")
         .trim()
         .trim_start_matches('\u{2022}')
         .trim_start_matches('-')
         .trim_start_matches('*')
         .trim_start_matches('+')
         .trim()
         .to_string();

      cleaned = cleaned
         .split_whitespace()
         .collect::<Vec<_>>()
         .join(" ")
         .trim()
         .trim_end_matches('.')
         .trim_end_matches(';')
         .trim_end_matches(',')
         .to_string();

      if cleaned.is_empty() {
         *item = cleaned;
         continue;
      }

      // Capitalize first letter
      if let Some(first_char) = cleaned.chars().next()
         && first_char.is_lowercase()
      {
         let rest = &cleaned[first_char.len_utf8()..];
         cleaned = format!("{}{}", first_char.to_uppercase(), rest);
      }

      if !cleaned.ends_with('.') {
         cleaned.push('.');
      }

      *item = cleaned;
   }

   // Remove empty body items
   msg.body.retain(|item| !item.trim().is_empty());

   // Cap details to top 4 by priority
   cap_details(&mut msg.body, 4);
}

/// Format `ConventionalCommit` as a single string for display and commit
pub fn format_commit_message(msg: &ConventionalCommit) -> String {
   // Build first line: type(scope): summary
   let scope_part = msg
      .scope
      .as_ref()
      .map(|s| format!("({s})"))
      .unwrap_or_default();
   let first_line = format!("{}{}: {}", msg.commit_type, scope_part, msg.summary);

   // Build body with - bullets
   let body_formatted = if msg.body.is_empty() {
      String::new()
   } else {
      msg.body
         .iter()
         .map(|item| format!("- {item}"))
         .collect::<Vec<_>>()
         .join("\n")
   };

   // Build footers
   let footers_formatted = if msg.footers.is_empty() {
      String::new()
   } else {
      msg.footers.join("\n")
   };

   // Combine parts
   let mut result = first_line;
   if !body_formatted.is_empty() {
      result.push_str("\n\n");
      result.push_str(&body_formatted);
   }
   if !footers_formatted.is_empty() {
      result.push_str("\n\n");
      result.push_str(&footers_formatted);
   }
   result
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::types::{CommitSummary, CommitType, ConventionalCommit, Scope};

   // normalize_unicode tests
   #[test]
   fn test_normalize_unicode_smart_quotes() {
      assert_eq!(normalize_unicode("\u{2018}smart quotes\u{2019}"), "'smart quotes'");
      assert_eq!(normalize_unicode("\u{201C}double quotes\u{201D}"), "\"double quotes\"");
      assert_eq!(normalize_unicode("\u{201A}low quote\u{2019}"), "'low quote'");
      assert_eq!(normalize_unicode("\u{201E}low double\u{201D}"), "\"low double\"");
   }

   #[test]
   fn test_normalize_unicode_dashes() {
      assert_eq!(normalize_unicode("en\u{2013}dash"), "en--dash");
      assert_eq!(normalize_unicode("em\u{2014}dash"), "em--dash");
      assert_eq!(normalize_unicode("fig\u{2012}dash"), "fig-dash");
      assert_eq!(normalize_unicode("minus\u{2212}sign"), "minus-sign");
   }

   #[test]
   fn test_normalize_unicode_arrows() {
      assert_eq!(normalize_unicode("arrow\u{2192}right"), "arrow->right");
      assert_eq!(normalize_unicode("arrow\u{2190}left"), "arrow<-left");
      assert_eq!(normalize_unicode("arrow\u{2194}both"), "arrow<->both");
      assert_eq!(normalize_unicode("double\u{21D2}arrow"), "double=>arrow");
      assert_eq!(normalize_unicode("up\u{2191}arrow"), "up^arrow");
   }

   #[test]
   fn test_normalize_unicode_math() {
      assert_eq!(normalize_unicode("a\u{00D7}b"), "axb");
      assert_eq!(normalize_unicode("a\u{00F7}b"), "a/b");
      assert_eq!(normalize_unicode("x\u{2264}y"), "x<=y");
      assert_eq!(normalize_unicode("x\u{2265}y"), "x>=y");
      assert_eq!(normalize_unicode("x\u{2260}y"), "x!=y");
      assert_eq!(normalize_unicode("x\u{2248}y"), "x~=y");
   }

   #[test]
   fn test_normalize_unicode_greek() {
      assert_eq!(normalize_unicode("\u{03BB} function"), "lambda function");
      assert_eq!(normalize_unicode("\u{03B1} beta \u{03B3}"), "alpha beta gamma");
      assert_eq!(normalize_unicode("\u{03BC} service"), "mu service");
      assert_eq!(normalize_unicode("\u{03A3} total"), "Sigma total");
   }

   #[test]
   fn test_normalize_unicode_fractions() {
      assert_eq!(normalize_unicode("\u{00BD} cup"), "1/2 cup");
      assert_eq!(normalize_unicode("\u{00BE} done"), "3/4 done");
      assert_eq!(normalize_unicode("\u{2153} left"), "1/3 left");
   }

   #[test]
   fn test_normalize_unicode_superscripts() {
      assert_eq!(normalize_unicode("x\u{00B2}"), "x^2");
      assert_eq!(normalize_unicode("10\u{00B3}"), "10^3");
   }

   #[test]
   fn test_normalize_unicode_multiple_replacements() {
      let input =
         "\u{2018}smart\u{2019}\u{2192}straight \u{201C}quotes\u{201D}\u{00D7}math\u{2264}ops";
      let expected = "'smart'->straight \"quotes\"xmath<=ops";
      assert_eq!(normalize_unicode(input), expected);
   }

   #[test]
   fn test_normalize_unicode_ellipsis() {
      assert_eq!(normalize_unicode("wait\u{2026}"), "wait...");
      assert_eq!(normalize_unicode("more\u{22EF}dots"), "more...dots");
   }

   #[test]
   fn test_normalize_unicode_bullets() {
      assert_eq!(normalize_unicode("\u{2022}item"), "-item");
      assert_eq!(normalize_unicode("\u{25E6}item"), "-item");
   }

   #[test]
   fn test_normalize_unicode_check_marks() {
      assert_eq!(normalize_unicode("\u{2713}done"), "vdone");
      assert_eq!(normalize_unicode("\u{2717}failed"), "xfailed");
   }

   // normalize_summary_verb tests
   #[test]
   fn test_normalize_summary_verb_present_to_past() {
      let mut s = "add new feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added new feature");

      let mut s = "fix bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");

      let mut s = "update docs".to_string();
      normalize_summary_verb(&mut s, "docs");
      assert_eq!(s, "updated docs");
   }

   #[test]
   fn test_normalize_summary_verb_already_past() {
      let mut s = "added feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added feature");

      let mut s = "fixed bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");
   }

   #[test]
   fn test_normalize_summary_verb_third_person() {
      let mut s = "adds feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added feature");

      let mut s = "fixes bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");
   }

   #[test]
   fn test_normalize_summary_verb_non_verb_start() {
      let mut s = "123 files changed".to_string();
      normalize_summary_verb(&mut s, "chore");
      assert_eq!(s, "123 files changed");
   }

   #[test]
   fn test_normalize_summary_verb_refactor_special_case() {
      let mut s = "refactored code".to_string();
      normalize_summary_verb(&mut s, "refactor");
      assert_eq!(s, "restructured code");
   }

   #[test]
   fn test_normalize_summary_verb_refactor_present() {
      let mut s = "refactor code".to_string();
      normalize_summary_verb(&mut s, "refactor");
      assert_eq!(s, "restructured code");

      let mut s = "refactor logic".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "refactored logic");
   }

   #[test]
   fn test_normalize_summary_verb_empty() {
      let mut s = String::new();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "");
   }

   #[test]
   fn test_normalize_summary_verb_single_word() {
      let mut s = "add".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added");
   }

   // cap_details tests
   #[test]
   fn test_cap_details_below_max() {
      let mut details = vec!["first".to_string(), "second".to_string(), "third".to_string()];
      cap_details(&mut details, 6);
      assert_eq!(details.len(), 3);
   }

   #[test]
   fn test_cap_details_at_max() {
      let mut details = vec![
         "one".to_string(),
         "two".to_string(),
         "three".to_string(),
         "four".to_string(),
         "five".to_string(),
         "six".to_string(),
      ];
      cap_details(&mut details, 6);
      assert_eq!(details.len(), 6);
   }

   #[test]
   fn test_cap_details_security_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "security vulnerability fixed".to_string(),
         "another change".to_string(),
         "third change".to_string(),
         "fourth change".to_string(),
         "fifth change".to_string(),
         "sixth change".to_string(),
      ];
      cap_details(&mut details, 4);
      assert!(details.iter().any(|d| d.contains("security")));
   }

   #[test]
   fn test_cap_details_performance_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "performance optimization added".to_string(),
         "another change".to_string(),
         "third change".to_string(),
         "fourth change".to_string(),
         "fifth change".to_string(),
      ];
      cap_details(&mut details, 3);
      assert!(details.iter().any(|d| d.contains("performance")));
   }

   #[test]
   fn test_cap_details_api_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "API interface updated".to_string(),
         "internal change".to_string(),
         "another internal change".to_string(),
         "yet another change".to_string(),
      ];
      cap_details(&mut details, 3);
      assert!(details.iter().any(|d| d.contains("API")));
   }

   #[test]
   fn test_cap_details_preserves_order() {
      let mut details = vec![
         "first".to_string(),
         "critical security fix".to_string(),
         "third".to_string(),
         "performance improvement".to_string(),
         "fifth".to_string(),
      ];
      cap_details(&mut details, 3);
      // Should preserve relative order of kept items
      let security_idx = details.iter().position(|d| d.contains("security"));
      let perf_idx = details.iter().position(|d| d.contains("performance"));
      assert!(security_idx.unwrap() < perf_idx.unwrap());
   }

   #[test]
   fn test_cap_details_empty_list() {
      let mut details: Vec<String> = vec![];
      cap_details(&mut details, 4);
      assert_eq!(details.len(), 0);
   }

   #[test]
   fn test_cap_details_breaking_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "breaking change introduced".to_string(),
         "another change".to_string(),
         "third change".to_string(),
         "fourth change".to_string(),
      ];
      cap_details(&mut details, 3);
      assert!(details.iter().any(|d| d.contains("breaking")));
   }

   // format_commit_message tests
   #[test]
   fn test_format_commit_message_type_summary_only() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         summary:     CommitSummary::new_unchecked("added new feature", 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      assert_eq!(format_commit_message(&commit), "feat: added new feature");
   }

   #[test]
   fn test_format_commit_message_with_scope() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       Some(Scope::new("api").unwrap()),
         summary:     CommitSummary::new_unchecked("fixed bug", 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      assert_eq!(format_commit_message(&commit), "fix(api): fixed bug");
   }

   #[test]
   fn test_format_commit_message_with_body() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         summary:     CommitSummary::new_unchecked("added feature", 128).unwrap(),
         body:        vec!["First detail.".to_string(), "Second detail.".to_string()],
         footers:     vec![],
      };
      let expected = "feat: added feature\n\n- First detail.\n- Second detail.";
      assert_eq!(format_commit_message(&commit), expected);
   }

   #[test]
   fn test_format_commit_message_with_footers() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       None,
         summary:     CommitSummary::new_unchecked("fixed bug", 128).unwrap(),
         body:        vec![],
         footers:     vec!["Closes: #123".to_string(), "Fixes: #456".to_string()],
      };
      let expected = "fix: fixed bug\n\nCloses: #123\nFixes: #456";
      assert_eq!(format_commit_message(&commit), expected);
   }

   #[test]
   fn test_format_commit_message_full() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("auth").unwrap()),
         summary:     CommitSummary::new_unchecked("added oauth support", 128).unwrap(),
         body:        vec![
            "Implemented OAuth2 flow.".to_string(),
            "Added token refresh.".to_string(),
         ],
         footers:     vec!["Closes: #789".to_string()],
      };
      let expected = "feat(auth): added oauth support\n\n- Implemented OAuth2 flow.\n- Added \
                      token refresh.\n\nCloses: #789";
      assert_eq!(format_commit_message(&commit), expected);
   }

   #[test]
   fn test_format_commit_message_nested_scope() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("refactor").unwrap(),
         scope:       Some(Scope::new("api/client").unwrap()),
         summary:     CommitSummary::new_unchecked("restructured code", 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      assert_eq!(format_commit_message(&commit), "refactor(api/client): restructured code");
   }
}
